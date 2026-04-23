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
    pub tools: Option<Vec<OllamaTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<OllamaOptions>,
    pub stream: bool,
}

#[derive(Serialize, Debug, Clone, Default)]
pub struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OllamaMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OllamaToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OllamaToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub call_type: Option<String>,
    pub function: OllamaFunctionCall,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OllamaFunctionCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
    #[serde(default)]
    pub done_reason: Option<String>,
    #[serde(default)]
    pub total_duration: Option<u64>,
    pub eval_count: Option<u32>,
    pub prompt_eval_count: Option<u32>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct OllamaTagsResponse {
    #[serde(default)]
    pub models: Vec<OllamaModelTag>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct OllamaModelTag {
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub details: OllamaModelDetails,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct OllamaModelDetails {
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub families: Vec<String>,
}

impl OllamaOptions {
    pub fn is_empty(&self) -> bool {
        self.temperature.is_none() && self.num_predict.is_none() && self.top_p.is_none()
    }
}

impl OllamaMessage {
    /// Create a simple text message.
    pub fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content.to_string()),
            thinking: None,
            tool_calls: None,
            tool_name: None,
        }
    }

    /// Create a tool result message.
    pub fn tool_result(tool_name: &str, content: &str) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.to_string()),
            thinking: None,
            tool_calls: None,
            tool_name: Some(tool_name.to_string()),
        }
    }
}
