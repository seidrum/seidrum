//! Load system skills from YAML files, embed descriptions, and store in ArangoDB.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use super::service::EmbeddingService;
use crate::brain::client::ArangoClient;

/// YAML structure for a skill file.
#[derive(Deserialize)]
struct SkillFile {
    skill: SkillDefinition,
}

#[derive(Deserialize)]
struct SkillDefinition {
    id: String,
    description: String,
    snippet: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    scope: Option<String>,
}

/// Load all skill YAML files from a directory, embed them, and store in ArangoDB.
pub async fn load_system_skills(
    arango: &ArangoClient,
    embedding: &EmbeddingService,
    skills_dir: &str,
) -> Result<()> {
    let dir = Path::new(skills_dir);
    if !dir.is_dir() {
        info!(
            "No skills directory at {}, skipping skill loading",
            skills_dir
        );
        return Ok(());
    }

    let mut loaded = 0;

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "yaml" && ext != "yml" {
            continue;
        }

        match load_and_store_skill(arango, embedding, &path).await {
            Ok(id) => {
                info!(skill_id = %id, "System skill loaded");
                loaded += 1;
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Failed to load skill");
            }
        }
    }

    info!(count = loaded, "System skills loaded");
    Ok(())
}

async fn load_and_store_skill(
    arango: &ArangoClient,
    embedding: &EmbeddingService,
    path: &Path,
) -> Result<String> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let file: SkillFile = serde_yaml::from_str(&contents)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    let skill = file.skill;

    // Validate skill ID
    if skill.id.is_empty()
        || skill.id.len() > 254
        || !skill
            .id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!(
            "Invalid skill ID '{}': must be 1-254 alphanumeric, dash, or underscore characters",
            skill.id
        );
    }

    // Generate embedding from description + snippet for better semantic matching
    let embed_text = format!("{}\n{}", skill.description, skill.snippet);
    let emb = match embedding.embed(&embed_text).await {
        Ok(v) => v,
        Err(e) => {
            warn!(skill_id = %skill.id, error = %e, "Failed to generate embedding, storing without");
            vec![]
        }
    };

    let now = chrono::Utc::now().to_rfc3339();
    let doc = serde_json::json!({
        "id": &skill.id,
        "description": &skill.description,
        "snippet": &skill.snippet,
        "embedding": emb,
        "source": "system",
        "scope": skill.scope,
        "tags": skill.tags,
        "updated_at": &now,
        "learned_from": null,
    });

    // Use AQL UPSERT to preserve created_at on re-runs
    let query = r#"
        UPSERT { _key: @key }
        INSERT MERGE(@doc, { _key: @key, created_at: @now })
        UPDATE MERGE(OLD, @doc)
        IN skills
        RETURN NEW
    "#;
    arango
        .execute_aql(
            query,
            &serde_json::json!({ "key": &skill.id, "doc": &doc, "now": &now }),
        )
        .await
        .with_context(|| format!("Failed to upsert skill '{}'", skill.id))?;

    Ok(skill.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_skill_yaml() {
        let yaml = r#"
skill:
  id: test-skill
  description: "A test skill"
  snippet: "Do the thing"
  tags: [test, example]
"#;
        let file: SkillFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(file.skill.id, "test-skill");
        assert_eq!(file.skill.description, "A test skill");
        assert_eq!(file.skill.snippet, "Do the thing");
        assert_eq!(file.skill.tags, vec!["test", "example"]);
        assert!(file.skill.scope.is_none());
    }

    #[test]
    fn parse_minimal_skill_yaml() {
        let yaml = r#"
skill:
  id: minimal
  description: "desc"
  snippet: "snip"
"#;
        let file: SkillFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(file.skill.id, "minimal");
        assert!(file.skill.tags.is_empty());
    }

    #[test]
    fn parse_skill_with_scope() {
        let yaml = r#"
skill:
  id: scoped-skill
  description: "desc"
  snippet: "snip"
  scope: scope_projects
"#;
        let file: SkillFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(file.skill.scope.unwrap(), "scope_projects");
    }

    #[test]
    fn reject_invalid_skill_id() {
        // Empty ID
        assert!("".is_empty());

        // ID with invalid chars
        let id = "../../bad";
        let valid = !id.is_empty()
            && id.len() <= 254
            && id
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
        assert!(!valid);

        // Valid ID
        let id = "code-review_v2";
        let valid = !id.is_empty()
            && id.len() <= 254
            && id
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
        assert!(valid);
    }
}
