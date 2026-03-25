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

    let doc = serde_json::json!({
        "id": &skill.id,
        "description": &skill.description,
        "snippet": &skill.snippet,
        "embedding": emb,
        "source": "system",
        "scope": skill.scope,
        "tags": skill.tags,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "learned_from": null,
    });

    arango
        .upsert_document("skills", &skill.id, &doc)
        .await
        .with_context(|| format!("Failed to upsert skill '{}'", skill.id))?;

    Ok(skill.id)
}
