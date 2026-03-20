//! Context window assembly for the LLM router.
//!
//! When an `agent.context.loaded` event arrives, this module:
//! 1. Renders the Tera prompt template with injected variables.
//! 2. Counts tokens using tiktoken-rs (cl100k_base).
//! 3. Applies the budget algorithm from LLM_INTEGRATION.md.
//! 4. Assembles the final messages array as simple role/content pairs.

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use tiktoken_rs::cl100k_base;
use tracing::{debug, info, warn};

use crate::{AgentContextLoaded, ChannelInbound};

// ---------------------------------------------------------------------------
// Simple message type (provider-agnostic)
// ---------------------------------------------------------------------------

/// A simple role/content message pair for assembled context.
#[derive(Debug, Clone)]
pub struct SimpleMessage {
    pub role: String,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Token counting
// ---------------------------------------------------------------------------

/// Count tokens for a string using cl100k_base (GPT-4 / Claude compatible).
pub fn count_tokens(text: &str) -> usize {
    let bpe = cl100k_base().expect("failed to load cl100k_base tokenizer");
    bpe.encode_with_special_tokens(text).len()
}

/// Truncate text to fit within a token budget, splitting on newlines.
/// Keeps the most recent items (trims from the beginning).
fn truncate_to_budget(text: &str, budget: usize) -> String {
    let total = count_tokens(text);
    if total <= budget {
        return text.to_string();
    }

    // Split into lines and keep from the end (most recent = most relevant)
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut running_tokens = 0;

    for line in lines.iter().rev() {
        let line_tokens = count_tokens(line) + 1; // +1 for newline
        if running_tokens + line_tokens > budget {
            break;
        }
        kept.push(line);
        running_tokens += line_tokens;
    }

    kept.reverse();
    kept.join("\n")
}

// ---------------------------------------------------------------------------
// Formatting context sections from AgentContextLoaded
// ---------------------------------------------------------------------------

/// Format facts from the graph context into a readable string.
fn format_facts(facts: &[Value]) -> String {
    if facts.is_empty() {
        return "No relevant facts available.".to_string();
    }

    facts
        .iter()
        .map(|f| {
            let subject = f.get("subject_name")
                .or_else(|| f.get("subject"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let predicate = f.get("predicate").and_then(|v| v.as_str()).unwrap_or("?");
            let object = f.get("object_name")
                .or_else(|| f.get("object"))
                .and_then(|v| v.as_str());
            let value = f.get("value").and_then(|v| v.as_str());
            let confidence = f.get("confidence").and_then(|v| v.as_f64()).unwrap_or(1.0);

            let target = object.or(value).unwrap_or("?");
            format!("- {} {} {} (confidence: {:.0}%)", subject, predicate, target, confidence * 100.0)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format active tasks into a readable string.
fn format_tasks(tasks: &[Value]) -> String {
    if tasks.is_empty() {
        return "No active tasks.".to_string();
    }

    tasks
        .iter()
        .map(|t| {
            let title = t.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
            let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("open");
            let priority = t.get("priority").and_then(|v| v.as_str()).unwrap_or("normal");
            let due = t.get("due_date").and_then(|v| v.as_str()).unwrap_or("no due date");
            format!("- [{}] {} (priority: {}, due: {})", status, title, priority, due)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format conversation history into a readable string.
fn format_history(history: &[Value]) -> String {
    if history.is_empty() {
        return "No recent conversation.".to_string();
    }

    history
        .iter()
        .filter_map(|msg| {
            let role = msg.get("role").and_then(|v| v.as_str())
                .or_else(|| msg.get("source").and_then(|v| v.as_str()))
                .unwrap_or("unknown");
            let content = msg.get("content").and_then(|v| v.as_str())
                .or_else(|| msg.get("text").and_then(|v| v.as_str()))
                .or_else(|| msg.get("raw_text").and_then(|v| v.as_str()))
                .unwrap_or("");
            if content.is_empty() {
                return None;
            }
            Some(format!("{}: {}", role, content))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format similar content (RAG results) into a readable string.
fn format_similar_content(content: &[Value]) -> String {
    if content.is_empty() {
        return String::new();
    }

    content
        .iter()
        .filter_map(|c| {
            let text = c.get("raw_text").and_then(|v| v.as_str())
                .or_else(|| c.get("text").and_then(|v| v.as_str()))
                .unwrap_or("");
            let score = c.get("score").and_then(|v| v.as_f64());
            if text.is_empty() {
                return None;
            }
            match score {
                Some(s) => Some(format!("[relevance: {:.2}] {}", s, text)),
                None => Some(text.to_string()),
            }
        })
        .collect::<Vec<_>>()
        .join("\n---\n")
}

// ---------------------------------------------------------------------------
// Prompt template rendering
// ---------------------------------------------------------------------------

/// Render the Tera prompt template with variables from the agent context.
fn render_prompt(template_content: &str, context: &AgentContextLoaded) -> Result<String> {
    let mut tera = tera::Tera::default();
    tera.add_raw_template("prompt", template_content)
        .context("Failed to parse prompt template")?;

    let mut tera_ctx = tera::Context::new();

    // user_name: extract from original event metadata or use a default
    let user_name = extract_user_name(context);
    tera_ctx.insert("user_name", &user_name);

    // current_time: ISO 8601
    tera_ctx.insert("current_time", &Utc::now().to_rfc3339());

    // scope_name
    let scope_name = context
        .original_event
        .scope
        .as_deref()
        .unwrap_or("default");
    tera_ctx.insert("scope_name", scope_name);

    // current_facts
    tera_ctx.insert("current_facts", &format_facts(&context.facts));

    // active_tasks
    tera_ctx.insert("active_tasks", &format_tasks(&context.active_tasks));

    // conversation_history
    tera_ctx.insert(
        "conversation_history",
        &format_history(&context.conversation_history),
    );

    // available_tools (placeholder for now, tools come from registry)
    tera_ctx.insert("available_tools", "brain-query, web-search");

    tera.render("prompt", &tera_ctx)
        .context("Failed to render prompt template")
}

/// Try to extract user name from the original event's metadata or user_id.
fn extract_user_name(context: &AgentContextLoaded) -> String {
    // Try metadata.user_name from the original ChannelInbound payload
    if let Ok(inbound) = serde_json::from_value::<ChannelInbound>(context.original_event.payload.clone()) {
        if let Some(name) = inbound.metadata.get("user_name") {
            return name.clone();
        }
        // Fall back to user_id
        if !inbound.user_id.is_empty() {
            return inbound.user_id;
        }
    }

    // Check entities for identity type
    for entity in &context.entities {
        if entity.get("entity_type").and_then(|v| v.as_str()) == Some("person") {
            if let Some(name) = entity.get("name").and_then(|v| v.as_str()) {
                return name.to_string();
            }
        }
    }

    "User".to_string()
}

// ---------------------------------------------------------------------------
// Budget algorithm: assemble context window
// ---------------------------------------------------------------------------

/// Configuration for context assembly.
pub struct ContextConfig {
    /// Maximum context window size in tokens.
    pub max_context_tokens: usize,
    /// Max tokens reserved for the response.
    pub max_response_tokens: usize,
    /// Prompt template content loaded from disk.
    pub prompt_template: String,
}

/// Assembled messages ready for the LLM provider (provider-agnostic).
pub struct AssembledContext {
    /// The rendered system prompt.
    pub system_prompt: String,
    /// The messages array as simple role/content pairs.
    pub messages: Vec<SimpleMessage>,
    /// Total tokens estimated for the assembled context.
    pub estimated_tokens: usize,
}

/// Assemble the full context window from an AgentContextLoaded event.
///
/// Follows the budget algorithm from LLM_INTEGRATION.md:
/// - System prompt: rendered Tera template
/// - Facts: ~20% of remaining budget
/// - Tasks: ~5%
/// - RAG (similar_content): ~25%
/// - Tools: ~15%
/// - History: ~35%
pub fn assemble_context(
    config: &ContextConfig,
    context: &AgentContextLoaded,
) -> Result<AssembledContext> {
    // 1. Render the system prompt
    let system_prompt = render_prompt(&config.prompt_template, context)?;
    let system_tokens = count_tokens(&system_prompt);

    // 2. Extract user text from the original event
    let user_text = extract_user_text(context)?;
    let user_tokens = count_tokens(&user_text);

    // 3. Calculate available budget
    let available = config
        .max_context_tokens
        .saturating_sub(config.max_response_tokens);
    let remaining = available
        .saturating_sub(system_tokens)
        .saturating_sub(user_tokens);

    info!(
        total = config.max_context_tokens,
        reserved = config.max_response_tokens,
        system_tokens,
        user_tokens,
        remaining,
        "Context budget calculation"
    );

    // 4. Proportional allocation
    let facts_budget = (remaining as f64 * 0.20) as usize;
    let tasks_budget = (remaining as f64 * 0.05) as usize;
    let rag_budget = (remaining as f64 * 0.25) as usize;
    let _tools_budget = (remaining as f64 * 0.15) as usize;
    let history_budget = (remaining as f64 * 0.35) as usize;

    debug!(
        facts_budget,
        tasks_budget,
        rag_budget,
        _tools_budget,
        history_budget,
        "Token budget allocation"
    );

    // 5. Format and truncate each section
    let facts_text = truncate_to_budget(&format_facts(&context.facts), facts_budget);
    let tasks_text = truncate_to_budget(&format_tasks(&context.active_tasks), tasks_budget);
    let rag_text = truncate_to_budget(
        &format_similar_content(&context.similar_content),
        rag_budget,
    );
    let history_text = truncate_to_budget(
        &format_history(&context.conversation_history),
        history_budget,
    );

    // 6. Build messages array as simple role/content pairs
    let mut messages: Vec<SimpleMessage> = Vec::new();

    // Add RAG context as a system-injected user context message if non-empty
    if !rag_text.is_empty() && rag_text != format_similar_content(&[]) {
        messages.push(SimpleMessage {
            role: "user".to_string(),
            content: format!(
                "[System context - relevant knowledge]\n{}\n[End system context]",
                rag_text
            ),
        });
        messages.push(SimpleMessage {
            role: "assistant".to_string(),
            content: "I've noted the relevant context. How can I help you?".to_string(),
        });
    }

    // Add conversation history as alternating user/assistant messages
    if !context.conversation_history.is_empty() {
        for msg in &context.conversation_history {
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user");
            let content = msg
                .get("content")
                .and_then(|v| v.as_str())
                .or_else(|| msg.get("text").and_then(|v| v.as_str()))
                .or_else(|| msg.get("raw_text").and_then(|v| v.as_str()))
                .unwrap_or("");
            if content.is_empty() {
                continue;
            }

            // Normalize role to user/assistant (provider-agnostic)
            let normalized_role = match role {
                "user" | "human" => "user",
                "assistant" | "bot" | "agent" | "model" => "assistant",
                _ => "user",
            };

            messages.push(SimpleMessage {
                role: normalized_role.to_string(),
                content: content.to_string(),
            });
        }
    }

    // Check history token usage and truncate from the front if needed
    let history_tokens: usize = messages
        .iter()
        .map(|m| count_tokens(&m.content) + 4)
        .sum();
    if history_tokens > history_budget + rag_budget {
        warn!(
            history_tokens,
            budget = history_budget + rag_budget,
            "History exceeds budget, truncating from oldest"
        );
        while messages.len() > 2 {
            let total: usize = messages
                .iter()
                .map(|m| count_tokens(&m.content) + 4)
                .sum();
            if total <= history_budget + rag_budget {
                break;
            }
            messages.remove(0);
        }
    }

    // Add the current user message at the end
    messages.push(SimpleMessage {
        role: "user".to_string(),
        content: user_text,
    });

    // Re-render system prompt with budgeted sections
    let budgeted_system = render_budgeted_system_prompt(
        &config.prompt_template,
        context,
        &facts_text,
        &tasks_text,
        &history_text,
    )?;

    let estimated_tokens = count_tokens(&budgeted_system)
        + messages
            .iter()
            .map(|m| count_tokens(&m.content) + 4)
            .sum::<usize>();

    info!(
        estimated_tokens,
        message_count = messages.len(),
        "Context assembly complete"
    );

    Ok(AssembledContext {
        system_prompt: budgeted_system,
        messages,
        estimated_tokens,
    })
}

/// Re-render the system prompt with budget-truncated sections.
fn render_budgeted_system_prompt(
    template_content: &str,
    context: &AgentContextLoaded,
    facts_text: &str,
    tasks_text: &str,
    history_text: &str,
) -> Result<String> {
    let mut tera = tera::Tera::default();
    tera.add_raw_template("prompt", template_content)
        .context("Failed to parse prompt template")?;

    let mut tera_ctx = tera::Context::new();

    let user_name = extract_user_name(context);
    tera_ctx.insert("user_name", &user_name);
    tera_ctx.insert("current_time", &Utc::now().to_rfc3339());

    let scope_name = context
        .original_event
        .scope
        .as_deref()
        .unwrap_or("default");
    tera_ctx.insert("scope_name", scope_name);
    tera_ctx.insert("current_facts", facts_text);
    tera_ctx.insert("active_tasks", tasks_text);
    tera_ctx.insert("conversation_history", history_text);
    tera_ctx.insert("available_tools", "brain-query, web-search");

    tera.render("prompt", &tera_ctx)
        .context("Failed to render budgeted prompt template")
}

/// Extract the user's message text from the original event inside AgentContextLoaded.
fn extract_user_text(context: &AgentContextLoaded) -> Result<String> {
    let inbound: ChannelInbound = serde_json::from_value(context.original_event.payload.clone())
        .context("Failed to parse original_event payload as ChannelInbound")?;
    Ok(inbound.text)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventEnvelope;

    fn make_test_context() -> AgentContextLoaded {
        let inbound_payload = serde_json::json!({
            "platform": "cli",
            "user_id": "luis",
            "chat_id": "chat-1",
            "text": "What tasks do I have?",
            "reply_to": null,
            "attachments": [],
            "metadata": {"user_name": "Luis"}
        });

        AgentContextLoaded {
            original_event: EventEnvelope {
                id: "evt-1".to_string(),
                event_type: "channel.cli.inbound".to_string(),
                timestamp: Utc::now(),
                source: "cli".to_string(),
                correlation_id: Some("corr-1".to_string()),
                scope: Some("scope_root".to_string()),
                payload: inbound_payload,
            },
            entities: vec![],
            facts: vec![
                serde_json::json!({
                    "subject_name": "Luis",
                    "predicate": "works_at",
                    "object_name": "Acme Corp",
                    "confidence": 0.95
                }),
                serde_json::json!({
                    "subject_name": "Luis",
                    "predicate": "lives_in",
                    "value": "San Francisco",
                    "confidence": 0.8
                }),
            ],
            similar_content: vec![],
            active_tasks: vec![serde_json::json!({
                "title": "Review PR #42",
                "status": "open",
                "priority": "high",
                "due_date": "2026-03-20"
            })],
            conversation_history: vec![
                serde_json::json!({"role": "user", "content": "Hello"}),
                serde_json::json!({"role": "assistant", "content": "Hi there!"}),
            ],
        }
    }

    #[test]
    fn test_count_tokens() {
        let tokens = count_tokens("Hello, world!");
        assert!(tokens > 0);
        assert!(tokens < 10);
    }

    #[test]
    fn test_format_facts() {
        let facts = vec![serde_json::json!({
            "subject_name": "Luis",
            "predicate": "works_at",
            "object_name": "Acme",
            "confidence": 0.95
        })];
        let result = format_facts(&facts);
        assert!(result.contains("Luis"));
        assert!(result.contains("works_at"));
        assert!(result.contains("Acme"));
        assert!(result.contains("95%"));
    }

    #[test]
    fn test_format_tasks() {
        let tasks = vec![serde_json::json!({
            "title": "Fix bug",
            "status": "open",
            "priority": "high",
            "due_date": "2026-03-20"
        })];
        let result = format_tasks(&tasks);
        assert!(result.contains("Fix bug"));
        assert!(result.contains("open"));
    }

    #[test]
    fn test_format_empty() {
        assert_eq!(format_facts(&[]), "No relevant facts available.");
        assert_eq!(format_tasks(&[]), "No active tasks.");
        assert_eq!(format_history(&[]), "No recent conversation.");
    }

    #[test]
    fn test_truncate_to_budget() {
        let text = "Line 1\nLine 2\nLine 3\nLine 4\nLine 5";
        // With a very small budget, we should get fewer lines
        let truncated = truncate_to_budget(text, 5);
        let truncated_tokens = count_tokens(&truncated);
        assert!(truncated_tokens <= 5);
    }

    #[test]
    fn test_render_prompt() {
        let template = "Hello {{ user_name }}, time is {{ current_time }}.";
        let ctx = make_test_context();
        let result = render_prompt(template, &ctx).unwrap();
        assert!(result.contains("Hello Luis"));
    }

    #[test]
    fn test_assemble_context() {
        let ctx = make_test_context();
        let config = ContextConfig {
            max_context_tokens: 10_000,
            max_response_tokens: 1_000,
            prompt_template: "You are {{ user_name }}'s assistant.\n## Facts\n{{ current_facts }}\n## Tasks\n{{ active_tasks }}\n## History\n{{ conversation_history }}".to_string(),
        };

        let assembled = assemble_context(&config, &ctx).unwrap();
        assert!(!assembled.system_prompt.is_empty());
        assert!(assembled.system_prompt.contains("Luis"));
        // Should have at least the user message
        assert!(!assembled.messages.is_empty());
        // Last message should be the user's current message
        let last = assembled.messages.last().unwrap();
        assert_eq!(last.role, "user");
        assert_eq!(last.content, "What tasks do I have?");
        assert!(assembled.estimated_tokens > 0);
    }

    #[test]
    fn test_extract_user_name_from_metadata() {
        let ctx = make_test_context();
        let name = extract_user_name(&ctx);
        assert_eq!(name, "Luis");
    }
}
