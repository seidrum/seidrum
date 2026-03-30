// Conversion between unified LLM format and OpenAI-specific format.

use seidrum_common::events::{
    ToolCallRequest, ToolSchema, UnifiedMessage, UnifiedToolCall,
};

use crate::openai_types::{OpenAiFunctionSchema, OpenAiMessage, OpenAiTool, OpenAiToolCall};

/// Convert unified messages to OpenAI message format.
///
/// Mapping:
/// - role "user" -> "user", role "assistant" -> "assistant"
/// - UnifiedMessage.content -> OpenAiMessage with content
/// - UnifiedMessage.tool_calls -> OpenAiMessage with tool_calls
/// - UnifiedMessage.tool_results -> OpenAiMessage with role "tool", tool_call_id, content
pub fn unified_to_openai_messages(messages: &[UnifiedMessage]) -> Vec<OpenAiMessage> {
    let mut result: Vec<OpenAiMessage> = Vec::new();

    for msg in messages {
        let mut openai_msg = OpenAiMessage {
            role: msg.role.clone(),
            content: msg.content.clone(),
            tool_call_id: None,
            tool_calls: None,
        };

        // Add tool calls
        if let Some(tool_calls) = &msg.tool_calls {
            openai_msg.tool_calls = Some(
                tool_calls
                    .iter()
                    .map(|tc| OpenAiToolCall {
                        id: tc.id.clone(),
                        call_type: "function".to_string(),
                        function: crate::openai_types::OpenAiFunctionCall {
                            name: tc.name.clone(),
                            arguments: serde_json::to_string(&tc.arguments)
                                .unwrap_or_else(|_| "{}".to_string()),
                        },
                    })
                    .collect(),
            );
        }

        result.push(openai_msg);

        // Tool results become separate messages
        if let Some(tool_results) = &msg.tool_results {
            for tr in tool_results {
                result.push(OpenAiMessage::tool_result(
                    &tr.tool_call_id,
                    &tr.content,
                ));
            }
        }
    }

    result
}

/// Convert unified tool schemas to OpenAI tool format.
pub fn unified_to_openai_tools(tools: &[ToolSchema]) -> Vec<OpenAiTool> {
    tools
        .iter()
        .map(|t| OpenAiTool {
            tool_type: "function".to_string(),
            function: OpenAiFunctionSchema {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            },
        })
        .collect()
}

/// Extract function call parts from OpenAI response and convert to unified format.
pub fn openai_tool_calls_to_unified(tool_calls: &[OpenAiToolCall]) -> Vec<UnifiedToolCall> {
    tool_calls
        .iter()
        .filter_map(|tc| {
            // Parse arguments from JSON string
            let args = serde_json::from_str(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::json!({}));

            Some(UnifiedToolCall {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                arguments: args,
            })
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
    fn test_unified_to_openai_messages_simple() {
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

        let openai_msgs = unified_to_openai_messages(&messages);
        assert_eq!(openai_msgs.len(), 2);
        assert_eq!(openai_msgs[0].role, "user");
        assert_eq!(openai_msgs[0].content.as_deref(), Some("Hello"));
        assert_eq!(openai_msgs[1].role, "assistant");
        assert_eq!(openai_msgs[1].content.as_deref(), Some("Hi there!"));
    }

    #[test]
    fn test_unified_to_openai_messages_with_tool_calls() {
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

        let openai_msgs = unified_to_openai_messages(&messages);
        assert_eq!(openai_msgs.len(), 1);
        assert!(openai_msgs[0].tool_calls.is_some());
        let tool_calls = openai_msgs[0].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls[0].function.name, "search");
        assert_eq!(tool_calls[0].id, "call_123");
    }

    #[test]
    fn test_unified_to_openai_messages_with_tool_results() {
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

        let openai_msgs = unified_to_openai_messages(&messages);
        assert_eq!(openai_msgs.len(), 2); // One for user message, one for tool result
        assert_eq!(openai_msgs[1].role, "tool");
        assert_eq!(openai_msgs[1].tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(openai_msgs[1].content.as_deref(), Some("search result"));
    }

    #[test]
    fn test_unified_to_openai_tools() {
        let tools = vec![
            ToolSchema {
                name: "search".to_string(),
                description: "Search the web".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolSchema {
                name: "calculate".to_string(),
                description: "Calculate math".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ];

        let openai_tools = unified_to_openai_tools(&tools);
        assert_eq!(openai_tools.len(), 2);
        assert_eq!(openai_tools[0].tool_type, "function");
        assert_eq!(openai_tools[0].function.name, "search");
        assert_eq!(openai_tools[1].function.name, "calculate");
    }

    #[test]
    fn test_openai_tool_calls_to_unified() {
        let tool_calls = vec![OpenAiToolCall {
            id: "call_123".to_string(),
            call_type: "function".to_string(),
            function: crate::openai_types::OpenAiFunctionCall {
                name: "search".to_string(),
                arguments: r#"{"query": "test"}"#.to_string(),
            },
        }];

        let unified = openai_tool_calls_to_unified(&tool_calls);
        assert_eq!(unified.len(), 1);
        assert_eq!(unified[0].id, "call_123");
        assert_eq!(unified[0].name, "search");
        assert_eq!(
            unified[0].arguments,
            serde_json::json!({"query": "test"})
        );
    }
}
