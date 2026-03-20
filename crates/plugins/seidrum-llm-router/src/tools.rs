//! Tool registry queries for the LLM router.
//!
//! - Provides meta_tools() for the 3 always-available tools.
//! - Queries the tool registry via NATS request/reply to discover dynamic tools.
//! - No tool execution code (that's handled by the provider + tool-dispatcher).

use std::time::Duration;

use tracing::{debug, warn};

use seidrum_common::events::{ToolSchema, ToolSearchRequest, ToolSearchResponse};

// ---------------------------------------------------------------------------
// Meta tools (always available)
// ---------------------------------------------------------------------------

/// Return the 3 always-available meta tools as ToolSchema.
///
/// These are provider-agnostic and always included in every LLM request:
/// 1. brain-query: Query the knowledge graph
/// 2. task-create: Create a new task
/// 3. web-search: Search the web
pub fn meta_tools() -> Vec<ToolSchema> {
    vec![
        ToolSchema {
            name: "brain-query".to_string(),
            description: "Query your knowledge graph. Use AQL syntax to search entities, facts, content, and relationships stored in the brain.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "What you want to find"
                    },
                    "aql": {
                        "type": "string",
                        "description": "AQL query to execute against the brain"
                    }
                },
                "required": ["description", "aql"]
            }),
        },
        ToolSchema {
            name: "task-create".to_string(),
            description: "Create a new task to track an actionable item. The task will be stored in the brain and can be assigned to an agent.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Short title for the task"
                    },
                    "description": {
                        "type": "string",
                        "description": "Detailed description of what needs to be done"
                    },
                    "priority": {
                        "type": "string",
                        "enum": ["low", "normal", "high", "critical"],
                        "description": "Task priority level"
                    },
                    "due_date": {
                        "type": "string",
                        "description": "Due date in ISO 8601 format (optional)"
                    }
                },
                "required": ["title"]
            }),
        },
        ToolSchema {
            name: "web-search".to_string(),
            description: "Search the web for current information. Use this when you need up-to-date data that may not be in the knowledge graph.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 5)"
                    }
                },
                "required": ["query"]
            }),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tool registry query via NATS
// ---------------------------------------------------------------------------

/// Query the tool registry for tools relevant to the user's message.
///
/// Sends a ToolSearchRequest via NATS request/reply to "capability.search".
/// Returns discovered tools as Vec<ToolSchema>.
/// Only searches for capabilities with kind "tool" (LLM-invocable).
pub async fn query_tool_registry(
    nats: &async_nats::Client,
    message_text: &str,
    max_tools: u32,
) -> Vec<ToolSchema> {
    let search_request = ToolSearchRequest {
        query_text: message_text.to_string(),
        limit: Some(max_tools),
        kind_filter: Some("tool".to_string()),
    };

    let payload = match serde_json::to_vec(&search_request) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Failed to serialize ToolSearchRequest");
            return Vec::new();
        }
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        nats.request("capability.search", payload.into()),
    )
    .await
    {
        Ok(Ok(response)) => {
            match serde_json::from_slice::<ToolSearchResponse>(&response.payload) {
                Ok(resp) => {
                    let tools: Vec<ToolSchema> = resp
                        .tools
                        .into_iter()
                        .map(|t| ToolSchema {
                            name: t.tool_id,
                            description: t.summary_md,
                            parameters: t.parameters,
                        })
                        .collect();
                    debug!(count = tools.len(), "Tools loaded from registry");
                    tools
                }
                Err(e) => {
                    warn!(error = %e, "Failed to parse ToolSearchResponse");
                    Vec::new()
                }
            }
        }
        Ok(Err(e)) => {
            warn!(error = %e, "Tool registry NATS request failed");
            Vec::new()
        }
        Err(_) => {
            warn!("Tool registry query timed out");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_meta_tools_count() {
        let tools = meta_tools();
        assert_eq!(tools.len(), 3);
    }

    #[test]
    fn test_meta_tools_names() {
        let tools = meta_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"brain-query"));
        assert!(names.contains(&"task-create"));
        assert!(names.contains(&"web-search"));
    }

    #[test]
    fn test_meta_tools_have_valid_schemas() {
        for tool in meta_tools() {
            assert!(!tool.name.is_empty());
            assert!(!tool.description.is_empty());

            // Parameters must be a valid JSON object
            let schema = &tool.parameters;
            assert_eq!(schema["type"], "object");
            assert!(schema["properties"].is_object());

            // Must have required array
            let required = schema["required"].as_array();
            assert!(required.is_some(), "Tool {} missing required array", tool.name);
        }
    }

    #[test]
    fn test_brain_query_tool_schema() {
        let tools = meta_tools();
        let brain = tools.iter().find(|t| t.name == "brain-query").unwrap();

        let schema = &brain.parameters;
        assert!(schema["properties"]["aql"].is_object());
        assert!(schema["properties"]["description"].is_object());

        let required = schema["required"].as_array().expect("required is array");
        let req_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(req_names.contains(&"description"));
        assert!(req_names.contains(&"aql"));
    }

    #[test]
    fn test_tool_schema_serialization() {
        for tool in meta_tools() {
            let json = serde_json::to_value(&tool).unwrap();

            // Must have name, description, parameters
            assert!(json.get("name").is_some());
            assert!(json.get("description").is_some());
            assert!(json.get("parameters").is_some());
            // Must NOT have input_schema (that's Anthropic format)
            assert!(json.get("input_schema").is_none());
        }
    }
}
