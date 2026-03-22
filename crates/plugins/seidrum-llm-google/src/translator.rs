// Conversion between unified LLM format and Gemini-specific format.

use seidrum_common::events::{
    LlmResponse, TokenUsage, ToolCallRequest, ToolSchema, UnifiedMessage, UnifiedToolCall,
};

use crate::gemini_types::{
    GeminiContent, GeminiFunctionDeclaration, GeminiPart, GeminiResponse, GeminiToolDeclaration,
};

/// Sanitize a tool name for Gemini (replace hyphens with underscores).
/// Gemini requires: alphanumeric, underscores, dots, colons, dashes — but
/// the name must start with a letter or underscore. Hyphens in practice
/// cause issues with some Gemini models, so we normalize to underscores.
fn sanitize_tool_name(name: &str) -> String {
    name.replace('-', "_")
}

/// Reverse the sanitization: convert underscores back to hyphens.
/// Used when mapping Gemini function calls back to Seidrum tool IDs.
fn unsanitize_tool_name(name: &str) -> String {
    name.replace('_', "-")
}

/// Convert unified messages to Gemini content format.
///
/// Mapping:
/// - role "user" -> "user", role "assistant" -> "model"
/// - UnifiedMessage.content -> GeminiPart with text
/// - UnifiedMessage.tool_calls -> GeminiPart with function_call
/// - UnifiedMessage.tool_results -> GeminiPart with function_response
pub fn unified_to_gemini_contents(messages: &[UnifiedMessage]) -> Vec<GeminiContent> {
    let mut contents: Vec<GeminiContent> = Vec::new();

    for msg in messages {
        let gemini_role = match msg.role.as_str() {
            "assistant" => "model",
            other => other,
        };

        let mut parts: Vec<GeminiPart> = Vec::new();

        // Add text content
        if let Some(text) = &msg.content {
            if !text.is_empty() {
                parts.push(GeminiPart::text_part(text));
            }
        }

        // Add tool calls as function_call parts
        if let Some(tool_calls) = &msg.tool_calls {
            for tc in tool_calls {
                parts.push(GeminiPart::function_call_part(
                    &sanitize_tool_name(&tc.name),
                    tc.arguments.clone(),
                ));
            }
        }

        // Add tool results as function_response parts
        if let Some(tool_results) = &msg.tool_results {
            for tr in tool_results {
                parts.push(GeminiPart::function_response_part(
                    &sanitize_tool_name(&tr.tool_call_id),
                    serde_json::json!({
                        "result": tr.content,
                        "is_error": tr.is_error,
                    }),
                ));
            }
        }

        if !parts.is_empty() {
            contents.push(GeminiContent {
                role: gemini_role.to_string(),
                parts,
            });
        }
    }

    contents
}

/// Convert unified tool schemas to Gemini function declarations.
pub fn unified_to_gemini_tools(tools: &[ToolSchema]) -> Vec<GeminiToolDeclaration> {
    if tools.is_empty() {
        return Vec::new();
    }

    let function_declarations: Vec<GeminiFunctionDeclaration> = tools
        .iter()
        .map(|t| GeminiFunctionDeclaration {
            name: sanitize_tool_name(&t.name),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
        })
        .collect();

    vec![GeminiToolDeclaration {
        function_declarations,
    }]
}

/// Convert a Gemini response to unified LlmResponse format.
pub fn gemini_to_unified_response(
    response: &GeminiResponse,
    model: &str,
    duration_ms: u64,
    agent_id: &str,
    total_input_tokens: u32,
    total_output_tokens: u32,
) -> LlmResponse {
    let total_tokens = total_input_tokens + total_output_tokens;

    let (content, finish_reason) = response
        .candidates
        .first()
        .map(|c| {
            let text: String = c
                .content
                .parts
                .iter()
                .filter_map(|p| p.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n");
            let content = if text.is_empty() { None } else { Some(text) };
            let reason = c
                .finish_reason
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            (content, reason)
        })
        .unwrap_or((None, "unknown".to_string()));

    LlmResponse {
        agent_id: agent_id.to_string(),
        content,
        tool_calls: None, // Tool calls are resolved internally
        model_used: model.to_string(),
        provider: "google".to_string(),
        tokens: TokenUsage {
            prompt_tokens: total_input_tokens,
            completion_tokens: total_output_tokens,
            total_tokens,
            estimated_cost_usd: 0.0,
        },
        duration_ms,
        finish_reason,
    }
}

/// Extract function call parts from a Gemini response candidate and convert
/// them to unified ToolCallRequest format for dispatch via NATS.
pub fn gemini_function_calls_to_tool_calls(parts: &[GeminiPart]) -> Vec<UnifiedToolCall> {
    parts
        .iter()
        .filter_map(|p| {
            p.function_call.as_ref().map(|fc| UnifiedToolCall {
                id: format!("call_{}", generate_call_id()),
                name: unsanitize_tool_name(&fc.name),
                arguments: fc.args.clone(),
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
        plugin_id: String::new(), // Tool dispatcher resolves the actual plugin
        arguments: tool_call.arguments.clone(),
        correlation_id: correlation_id.map(|s| s.to_string()),
    }
}

/// Generate a simple time-based unique ID for tool call tracking.
fn generate_call_id() -> String {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};

    thread_local! {
        static STATE: Cell<u64> = Cell::new(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        );
    }

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let rand_part: u64 = STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    });

    format!("{:012x}-{:016x}", ts, rand_part)
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidrum_common::events::UnifiedToolResult;

    #[test]
    fn test_unified_to_gemini_contents_simple() {
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

        let contents = unified_to_gemini_contents(&messages);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[1].role, "model");
        assert_eq!(contents[0].parts[0].text.as_deref(), Some("Hello"));
        assert_eq!(contents[1].parts[0].text.as_deref(), Some("Hi there!"));
    }

    #[test]
    fn test_unified_to_gemini_contents_with_tool_calls() {
        let messages = vec![UnifiedMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(vec![UnifiedToolCall {
                id: "tc-1".to_string(),
                name: "search".to_string(),
                arguments: serde_json::json!({"query": "test"}),
            }]),
            tool_results: None,
        }];

        let contents = unified_to_gemini_contents(&messages);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "model");
        assert!(contents[0].parts[0].function_call.is_some());
        let fc = contents[0].parts[0].function_call.as_ref().unwrap();
        assert_eq!(fc.name, "search");
    }

    #[test]
    fn test_unified_to_gemini_contents_with_tool_results() {
        let messages = vec![UnifiedMessage {
            role: "user".to_string(),
            content: None,
            tool_calls: None,
            tool_results: Some(vec![UnifiedToolResult {
                tool_call_id: "search".to_string(),
                content: "result data".to_string(),
                is_error: false,
            }]),
        }];

        let contents = unified_to_gemini_contents(&messages);
        assert_eq!(contents.len(), 1);
        assert!(contents[0].parts[0].function_response.is_some());
        let fr = contents[0].parts[0].function_response.as_ref().unwrap();
        assert_eq!(fr.name, "search");
    }

    #[test]
    fn test_unified_to_gemini_tools() {
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

        let gemini_tools = unified_to_gemini_tools(&tools);
        assert_eq!(gemini_tools.len(), 1);
        assert_eq!(gemini_tools[0].function_declarations.len(), 2);
        assert_eq!(gemini_tools[0].function_declarations[0].name, "search");
    }

    #[test]
    fn test_unified_to_gemini_tools_empty() {
        let tools: Vec<ToolSchema> = vec![];
        let gemini_tools = unified_to_gemini_tools(&tools);
        assert!(gemini_tools.is_empty());
    }
}
