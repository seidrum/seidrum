//! Tool registry and tool call execution for the LLM router.
//!
//! - Registers built-in tools (brain-query).
//! - Queries the brain for dynamically available tools via vector search.
//! - Builds tool schemas in Anthropic API format.
//! - Executes tool calls by dispatching to the appropriate handler.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Anthropic tool format types
// ---------------------------------------------------------------------------

/// Tool definition in the Anthropic API format.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A tool_use content block returned by the Anthropic API.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AnthropicToolUse {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Result of executing a single tool call.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// Built-in tool definitions
// ---------------------------------------------------------------------------

/// Return the built-in brain-query tool schema in Anthropic format.
pub fn brain_query_tool() -> AnthropicTool {
    AnthropicTool {
        name: "brain-query".to_string(),
        description: "Query your knowledge graph. Use AQL syntax to search entities, facts, content, and relationships stored in the brain.".to_string(),
        input_schema: serde_json::json!({
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
    }
}

/// Return the list of pinned (always-included) tools.
pub fn pinned_tools() -> Vec<AnthropicTool> {
    vec![brain_query_tool()]
}

// ---------------------------------------------------------------------------
// Dynamic tool discovery via brain vector search
// ---------------------------------------------------------------------------

/// Query the brain for tools relevant to the user's message.
///
/// Sends a `brain.query.request` with `query_type: "vector_search"` over the
/// `tools` collection. Returns any tool schemas found, converted to Anthropic
/// format.
pub async fn query_dynamic_tools(
    nats: &async_nats::Client,
    message_text: &str,
    max_dynamic_tools: u32,
) -> Vec<AnthropicTool> {
    // We use a get_context query type since we don't have a pre-computed
    // embedding vector here. The kernel can handle text-based tool lookup.
    let query_request = serde_json::json!({
        "query_type": "vector_search",
        "collection": "tools",
        "query_text": message_text,
        "limit": max_dynamic_tools,
    });

    let payload = match serde_json::to_vec(&query_request) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "Failed to serialize tool query request");
            return Vec::new();
        }
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        nats.request("brain.query.request", payload.into()),
    )
    .await
    {
        Ok(Ok(response)) => {
            match serde_json::from_slice::<BrainToolQueryResponse>(&response.payload) {
                Ok(resp) => {
                    let tools = resp
                        .results
                        .into_iter()
                        .filter_map(|t| {
                            Some(AnthropicTool {
                                name: t.get("name")?.as_str()?.to_string(),
                                description: t
                                    .get("description")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                input_schema: t
                                    .get("parameters")
                                    .cloned()
                                    .unwrap_or(serde_json::json!({"type": "object"})),
                            })
                        })
                        .collect::<Vec<_>>();
                    debug!(count = tools.len(), "Dynamic tools loaded from brain");
                    tools
                }
                Err(e) => {
                    warn!(error = %e, "Failed to parse brain tool query response");
                    Vec::new()
                }
            }
        }
        Ok(Err(e)) => {
            warn!(error = %e, "Brain tool query NATS request failed");
            Vec::new()
        }
        Err(_) => {
            warn!("Brain tool query timed out");
            Vec::new()
        }
    }
}

#[derive(Deserialize, Debug)]
struct BrainToolQueryResponse {
    #[serde(default)]
    results: Vec<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Collect all tools (pinned + dynamic), deduplicated
// ---------------------------------------------------------------------------

/// Build the complete tool list for an LLM call.
///
/// Pinned tools are always present. Dynamic tools from the brain are appended
/// if they don't collide by name with a pinned tool.
pub async fn collect_tools(
    nats: &async_nats::Client,
    message_text: &str,
    max_dynamic_tools: u32,
) -> Vec<AnthropicTool> {
    let mut tools = pinned_tools();
    let pinned_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();

    let dynamic = query_dynamic_tools(nats, message_text, max_dynamic_tools).await;
    for tool in dynamic {
        if !pinned_names.contains(&tool.name) {
            tools.push(tool);
        }
    }

    info!(tool_count = tools.len(), "Tools collected for LLM call");
    tools
}

// ---------------------------------------------------------------------------
// Tool call execution
// ---------------------------------------------------------------------------

/// Execute a single tool call and return the result.
///
/// Currently supported:
/// - `brain-query`: sends an AQL query to the kernel via NATS request/reply
pub async fn execute_tool_call(
    tool_use: &AnthropicToolUse,
    nats: &async_nats::Client,
) -> ToolResult {
    info!(tool = %tool_use.name, id = %tool_use.id, "Executing tool call");

    match tool_use.name.as_str() {
        "brain-query" => execute_brain_query(tool_use, nats).await,
        other => {
            warn!(tool = %other, "Unknown tool requested");
            ToolResult {
                tool_use_id: tool_use.id.clone(),
                content: format!("Error: unknown tool '{}'", other),
                is_error: true,
            }
        }
    }
}

/// Execute the brain-query built-in tool.
async fn execute_brain_query(
    tool_use: &AnthropicToolUse,
    nats: &async_nats::Client,
) -> ToolResult {
    let aql = tool_use
        .input
        .get("aql")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let description = tool_use
        .input
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if aql.is_empty() {
        return ToolResult {
            tool_use_id: tool_use.id.clone(),
            content: "Error: 'aql' parameter is required for brain-query".to_string(),
            is_error: true,
        };
    }

    info!(
        description = %description,
        aql_len = aql.len(),
        "Executing brain-query tool"
    );

    // Build a BrainQueryRequest for AQL execution
    let query_request = serde_json::json!({
        "query_type": "aql",
        "aql": aql,
        "bind_vars": null,
        "embedding": null,
        "collection": null,
        "limit": null,
        "start_vertex": null,
        "direction": null,
        "depth": null,
        "query_text": null,
        "max_facts": null,
        "graph_depth": null,
        "min_confidence": null,
    });

    let payload = match serde_json::to_vec(&query_request) {
        Ok(p) => p,
        Err(e) => {
            error!(error = %e, "Failed to serialize brain query request");
            return ToolResult {
                tool_use_id: tool_use.id.clone(),
                content: format!("Error: failed to build query request: {}", e),
                is_error: true,
            };
        }
    };

    match tokio::time::timeout(
        Duration::from_secs(10),
        nats.request("brain.query.request", payload.into()),
    )
    .await
    {
        Ok(Ok(response)) => {
            let result_str = String::from_utf8_lossy(&response.payload).to_string();
            // Try to pretty-format the JSON for the LLM
            let content = match serde_json::from_str::<serde_json::Value>(&result_str) {
                Ok(val) => serde_json::to_string_pretty(&val).unwrap_or(result_str),
                Err(_) => result_str,
            };
            info!(result_len = content.len(), "brain-query completed");
            ToolResult {
                tool_use_id: tool_use.id.clone(),
                content,
                is_error: false,
            }
        }
        Ok(Err(e)) => {
            error!(error = %e, "brain-query NATS request failed");
            ToolResult {
                tool_use_id: tool_use.id.clone(),
                content: format!("Error: brain query failed: {}", e),
                is_error: true,
            }
        }
        Err(_) => {
            warn!("brain-query timed out after 10s");
            ToolResult {
                tool_use_id: tool_use.id.clone(),
                content: "Error: brain query timed out".to_string(),
                is_error: true,
            }
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
    fn test_brain_query_tool_schema() {
        let tool = brain_query_tool();
        assert_eq!(tool.name, "brain-query");
        assert!(!tool.description.is_empty());

        // input_schema must be a valid JSON object with properties
        let schema = &tool.input_schema;
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["aql"].is_object());
        assert!(schema["properties"]["description"].is_object());

        let required = schema["required"].as_array().expect("required is array");
        let req_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(req_names.contains(&"description"));
        assert!(req_names.contains(&"aql"));
    }

    #[test]
    fn test_pinned_tools_includes_brain_query() {
        let tools = pinned_tools();
        assert!(!tools.is_empty());
        assert!(tools.iter().any(|t| t.name == "brain-query"));
    }

    #[test]
    fn test_anthropic_tool_serialization() {
        let tool = brain_query_tool();
        let json = serde_json::to_value(&tool).unwrap();

        // Anthropic format requires name, description, input_schema
        assert!(json.get("name").is_some());
        assert!(json.get("description").is_some());
        assert!(json.get("input_schema").is_some());
        // Must NOT have "parameters" key (that's OpenAI format)
        assert!(json.get("parameters").is_none());
    }

    #[test]
    fn test_tool_result_structure() {
        let result = ToolResult {
            tool_use_id: "toolu_123".to_string(),
            content: "query returned 5 results".to_string(),
            is_error: false,
        };
        assert_eq!(result.tool_use_id, "toolu_123");
        assert!(!result.is_error);
    }
}
