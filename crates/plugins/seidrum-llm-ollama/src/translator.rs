// Conversion between unified LLM format and Ollama-specific format.

use seidrum_common::events::{ToolCallRequest, ToolSchema, UnifiedMessage, UnifiedToolCall};

use crate::ollama_types::{
    OllamaFunctionCall, OllamaFunctionSchema, OllamaMessage, OllamaTool, OllamaToolCall,
};

/// Convert unified messages to Ollama message format.
///
/// Mapping:
/// - role "user" -> "user", role "assistant" -> "assistant"
/// - UnifiedMessage.content -> OllamaMessage with content
/// - UnifiedMessage.tool_calls -> OllamaMessage with tool_calls
/// - UnifiedMessage.tool_results -> Separate message with role "tool"
pub fn unified_to_ollama_messages(messages: &[UnifiedMessage]) -> Vec<OllamaMessage> {
    let mut result: Vec<OllamaMessage> = Vec::new();

    for msg in messages {
        let mut ollama_msg = OllamaMessage {
            role: msg.role.clone(),
            content: msg.content.clone(),
            thinking: None,
            tool_calls: None,
            tool_name: None,
        };

        // Add tool calls
        if let Some(tool_calls) = &msg.tool_calls {
            ollama_msg.tool_calls = Some(
                tool_calls
                    .iter()
                    .map(|tc| OllamaToolCall {
                        id: Some(tc.id.clone()),
                        call_type: Some("function".to_string()),
                        function: OllamaFunctionCall {
                            index: None,
                            name: tc.name.clone(),
                            arguments: tc.arguments.clone(),
                            description: None,
                        },
                    })
                    .collect(),
            );
        }

        result.push(ollama_msg);

        // Tool results become separate messages
        if let Some(tool_results) = &msg.tool_results {
            for tr in tool_results {
                result.push(OllamaMessage {
                    role: "tool".to_string(),
                    content: Some(tr.content.clone()),
                    thinking: None,
                    tool_calls: None,
                    tool_name: None,
                });
            }
        }
    }

    result
}

/// Convert unified tool schemas to Ollama tool format.
pub fn unified_to_ollama_tools(tools: &[ToolSchema]) -> Vec<OllamaTool> {
    tools
        .iter()
        .map(|t| OllamaTool {
            tool_type: "function".to_string(),
            function: OllamaFunctionSchema {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            },
        })
        .collect()
}

/// Extract function call parts from Ollama response and convert to unified format.
pub fn ollama_tool_calls_to_unified(tool_calls: &[OllamaToolCall]) -> Vec<UnifiedToolCall> {
    tool_calls
        .iter()
        .enumerate()
        .map(|(idx, tc)| {
            let args = match &tc.function.arguments {
                serde_json::Value::String(raw) => serde_json::from_str(raw)
                    .unwrap_or_else(|_| serde_json::Value::String(raw.clone())),
                value => value.clone(),
            };

            UnifiedToolCall {
                id: tc
                    .id
                    .clone()
                    .or_else(|| tc.function.index.map(|i| format!("call_{i}")))
                    .unwrap_or_else(|| format!("call_{idx}")),
                name: tc.function.name.clone(),
                arguments: args,
            }
        })
        .collect()
}

/// Build a ToolCallRequest for dispatching via NATS request/reply.
pub fn tool_call_to_dispatch_request(
    tool_call: &UnifiedToolCall,
    correlation_id: Option<&str>,
) -> ToolCallRequest {
    ToolCallRequest {
        tool_id: tool_call.name.clone(),
        plugin_id: String::new(),
        arguments: tool_call.arguments.clone(),
        correlation_id: correlation_id.map(|s| s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidrum_common::events::{UnifiedToolCall as UTC, UnifiedToolResult};

    #[test]
    fn test_unified_to_ollama_messages_simple() {
        let messages = vec![
            UnifiedMessage {
                role: "user".to_string(),
                content: Some("Hello".to_string()),
                tool_calls: None,
                tool_results: None,
            },
            UnifiedMessage {
                role: "assistant".to_string(),
                content: Some("Hi there!".to_string()),
                tool_calls: None,
                tool_results: None,
            },
        ];

        let ollama_msgs = unified_to_ollama_messages(&messages);
        assert_eq!(ollama_msgs.len(), 2);
        assert_eq!(ollama_msgs[0].role, "user");
        assert_eq!(ollama_msgs[0].content.as_deref(), Some("Hello"));
        assert_eq!(ollama_msgs[1].role, "assistant");
        assert_eq!(ollama_msgs[1].content.as_deref(), Some("Hi there!"));
    }

    #[test]
    fn test_unified_to_ollama_messages_with_tool_calls() {
        let messages = vec![UnifiedMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![UTC {
                id: "call_123".to_string(),
                name: "search".to_string(),
                arguments: serde_json::json!({"query": "test"}),
            }]),
            tool_results: None,
        }];

        let ollama_msgs = unified_to_ollama_messages(&messages);
        assert_eq!(ollama_msgs.len(), 1);
        assert!(ollama_msgs[0].tool_calls.is_some());
        let tool_calls = ollama_msgs[0].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls[0].function.name, "search");
        assert_eq!(tool_calls[0].id.as_deref(), Some("call_123"));
    }

    #[test]
    fn test_unified_to_ollama_messages_with_tool_results() {
        let messages = vec![UnifiedMessage {
            role: "user".to_string(),
            content: None,
            tool_calls: None,
            tool_results: Some(vec![UnifiedToolResult {
                tool_call_id: "call_123".to_string(),
                content: "search result".to_string(),
                is_error: false,
            }]),
        }];

        let ollama_msgs = unified_to_ollama_messages(&messages);
        assert_eq!(ollama_msgs.len(), 2);
        assert_eq!(ollama_msgs[1].role, "tool");
        assert_eq!(ollama_msgs[1].content.as_deref(), Some("search result"));
    }

    #[test]
    fn test_unified_to_ollama_tools() {
        let tools = vec![ToolSchema {
            name: "search".to_string(),
            description: "Search the web".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let ollama_tools = unified_to_ollama_tools(&tools);
        assert_eq!(ollama_tools.len(), 1);
        assert_eq!(ollama_tools[0].tool_type, "function");
        assert_eq!(ollama_tools[0].function.name, "search");
    }

    #[test]
    fn test_ollama_tool_calls_to_unified() {
        let tool_calls = vec![OllamaToolCall {
            id: Some("call_123".to_string()),
            call_type: Some("function".to_string()),
            function: OllamaFunctionCall {
                index: None,
                name: "search".to_string(),
                arguments: serde_json::json!({"query": "test"}),
                description: None,
            },
        }];

        let unified = ollama_tool_calls_to_unified(&tool_calls);
        assert_eq!(unified.len(), 1);
        assert_eq!(unified[0].id, "call_123");
        assert_eq!(unified[0].name, "search");
        assert_eq!(unified[0].arguments, serde_json::json!({"query": "test"}));
    }

    #[test]
    fn test_ollama_tool_calls_to_unified_with_string_args() {
        let tool_calls = vec![OllamaToolCall {
            id: None,
            call_type: None,
            function: OllamaFunctionCall {
                index: Some(7),
                name: "search".to_string(),
                arguments: serde_json::Value::String(r#"{"query":"test"}"#.to_string()),
                description: None,
            },
        }];

        let unified = ollama_tool_calls_to_unified(&tool_calls);
        assert_eq!(unified.len(), 1);
        assert_eq!(unified[0].id, "call_7");
        assert_eq!(unified[0].arguments, serde_json::json!({"query": "test"}));
    }
}
