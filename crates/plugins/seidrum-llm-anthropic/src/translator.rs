// Conversion between unified LLM format and Anthropic-specific format.

use seidrum_common::events::{ToolCallRequest, ToolSchema, UnifiedMessage, UnifiedToolCall};

use crate::anthropic_types::{AnthropicContent, AnthropicMessage, AnthropicTool};

/// Convert unified messages to Anthropic message format.
///
/// Mapping:
/// - role "user" -> "user", role "assistant" -> "assistant"
/// - UnifiedMessage.content -> AnthropicContent::Text
/// - UnifiedMessage.tool_calls -> AnthropicContent::ToolUse
/// - UnifiedMessage.tool_results -> AnthropicContent::ToolResult
///
/// Note: Anthropic requires separate messages (one per role), and tool_calls/results
/// are content blocks within those messages.
pub fn unified_to_anthropic_messages(messages: &[UnifiedMessage]) -> Vec<AnthropicMessage> {
    let mut result: Vec<AnthropicMessage> = Vec::new();

    for msg in messages {
        let mut content: Vec<AnthropicContent> = Vec::new();

        // Add text content
        if let Some(text) = &msg.content {
            if !text.is_empty() {
                content.push(AnthropicContent::Text { text: text.clone() });
            }
        }

        // Add tool calls
        if let Some(tool_calls) = &msg.tool_calls {
            for tc in tool_calls {
                content.push(AnthropicContent::ToolUse {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    input: tc.arguments.clone(),
                });
            }
        }

        // Add tool results
        if let Some(tool_results) = &msg.tool_results {
            for tr in tool_results {
                content.push(AnthropicContent::ToolResult {
                    tool_use_id: tr.tool_call_id.clone(),
                    content: tr.content.clone(),
                    is_error: if tr.is_error { Some(true) } else { None },
                });
            }
        }

        if !content.is_empty() {
            result.push(AnthropicMessage {
                role: msg.role.clone(),
                content,
            });
        }
    }

    result
}

/// Convert unified tool schemas to Anthropic tool format.
pub fn unified_to_anthropic_tools(tools: &[ToolSchema]) -> Vec<AnthropicTool> {
    tools
        .iter()
        .map(|t| AnthropicTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.parameters.clone(),
        })
        .collect()
}

/// Extract tool use parts from Anthropic response content and convert to unified format.
pub fn anthropic_tool_uses_to_unified(content: &[AnthropicContent]) -> Vec<UnifiedToolCall> {
    content
        .iter()
        .filter_map(|c| {
            if let AnthropicContent::ToolUse { id, name, input } = c {
                Some(UnifiedToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: input.clone(),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Extract text from Anthropic response content.
pub fn anthropic_content_to_text(content: &[AnthropicContent]) -> String {
    content
        .iter()
        .filter_map(|c| {
            if let AnthropicContent::Text { text } = c {
                Some(text.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    fn test_unified_to_anthropic_messages_simple() {
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

        let anthropic_msgs = unified_to_anthropic_messages(&messages);
        assert_eq!(anthropic_msgs.len(), 2);
        assert_eq!(anthropic_msgs[0].role, "user");
        assert_eq!(anthropic_msgs[1].role, "assistant");
    }

    #[test]
    fn test_unified_to_anthropic_messages_with_tool_calls() {
        let messages = vec![UnifiedMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![UTC {
                id: "tooluse_123".to_string(),
                name: "search".to_string(),
                arguments: serde_json::json!({"query": "test"}),
            }]),
            tool_results: None,
        }];

        let anthropic_msgs = unified_to_anthropic_messages(&messages);
        assert_eq!(anthropic_msgs.len(), 1);
        assert_eq!(anthropic_msgs[0].role, "assistant");
        assert_eq!(anthropic_msgs[0].content.len(), 1);

        match &anthropic_msgs[0].content[0] {
            AnthropicContent::ToolUse { id, name, input } => {
                assert_eq!(id, "tooluse_123");
                assert_eq!(name, "search");
                assert_eq!(input, &serde_json::json!({"query": "test"}));
            }
            _ => panic!("Expected ToolUse content"),
        }
    }

    #[test]
    fn test_unified_to_anthropic_messages_with_tool_results() {
        let messages = vec![UnifiedMessage {
            role: "user".to_string(),
            content: None,
            tool_calls: None,
            tool_results: Some(vec![UnifiedToolResult {
                tool_call_id: "tooluse_123".to_string(),
                content: "search result".to_string(),
                is_error: false,
            }]),
        }];

        let anthropic_msgs = unified_to_anthropic_messages(&messages);
        assert_eq!(anthropic_msgs.len(), 1);
        assert_eq!(anthropic_msgs[0].role, "user");
        assert_eq!(anthropic_msgs[0].content.len(), 1);

        match &anthropic_msgs[0].content[0] {
            AnthropicContent::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "tooluse_123");
                assert_eq!(content, "search result");
            }
            _ => panic!("Expected ToolResult content"),
        }
    }

    #[test]
    fn test_unified_to_anthropic_tools() {
        let tools = vec![ToolSchema {
            name: "search".to_string(),
            description: "Search the web".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let anthropic_tools = unified_to_anthropic_tools(&tools);
        assert_eq!(anthropic_tools.len(), 1);
        assert_eq!(anthropic_tools[0].name, "search");
        assert_eq!(anthropic_tools[0].description, "Search the web");
    }

    #[test]
    fn test_anthropic_content_to_text() {
        let content = vec![
            AnthropicContent::Text {
                text: "Hello".to_string(),
            },
            AnthropicContent::Text {
                text: "World".to_string(),
            },
        ];

        let text = anthropic_content_to_text(&content);
        assert_eq!(text, "Hello\nWorld");
    }

    #[test]
    fn test_anthropic_tool_uses_to_unified() {
        let content = vec![AnthropicContent::ToolUse {
            id: "tooluse_123".to_string(),
            name: "search".to_string(),
            input: serde_json::json!({"query": "test"}),
        }];

        let unified = anthropic_tool_uses_to_unified(&content);
        assert_eq!(unified.len(), 1);
        assert_eq!(unified[0].id, "tooluse_123");
        assert_eq!(unified[0].name, "search");
    }
}
