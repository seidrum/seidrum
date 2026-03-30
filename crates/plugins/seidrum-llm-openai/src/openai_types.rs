// OpenAI API types (reqwest, no SDK).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct OpenAiRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OpenAiMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OpenAiToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: OpenAiFunctionCall,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OpenAiFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct OpenAiTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: OpenAiFunctionSchema,
}

#[derive(Serialize, Debug, Clone)]
pub struct OpenAiFunctionSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub struct OpenAiResponse {
    pub choices: Vec<OpenAiChoice>,
    pub usage: OpenAiUsage,
    #[serde(default)]
    pub error: Option<OpenAiErrorDetail>,
}

#[derive(Deserialize, Debug)]
pub struct OpenAiChoice {
    pub message: OpenAiMessage,
    pub finish_reason: String,
}

#[derive(Deserialize, Debug)]
pub struct OpenAiUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub struct OpenAiErrorDetail {
    pub message: String,
    pub code: Option<String>,
}

impl OpenAiMessage {
    /// Create a simple text message.
    #[allow(dead_code)]
    pub fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content.to_string()),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// Create a tool result message.
    pub fn tool_result(tool_call_id: &str, content: &str) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.to_string()),
            tool_call_id: Some(tool_call_id.to_string()),
            tool_calls: None,
        }
    }
}
