// Ollama API types (reqwest, no SDK).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct OllamaRequest {
    pub model: String,
    pub messages: Vec<OllamaMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OllamaTool>>,
    pub stream: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OllamaMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OllamaToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: OllamaFunctionCall,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OllamaFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct OllamaTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: OllamaFunctionSchema,
}

#[derive(Serialize, Debug, Clone)]
pub struct OllamaFunctionSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub struct OllamaResponse {
    pub message: OllamaMessage,
    pub eval_count: Option<u32>,
    pub prompt_eval_count: Option<u32>,
    #[serde(default)]
    pub error: Option<String>,
}

impl OllamaMessage {
    /// Create a simple text message.
    pub fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content.to_string()),
            tool_calls: None,
        }
    }
}
