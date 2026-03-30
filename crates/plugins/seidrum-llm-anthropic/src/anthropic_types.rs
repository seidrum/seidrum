// Anthropic API types (reqwest, no SDK).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct AnthropicRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Vec<AnthropicContent>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum AnthropicContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Serialize, Debug, Clone)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub struct AnthropicResponse {
    pub content: Vec<AnthropicContent>,
    pub stop_reason: String,
    pub usage: AnthropicUsage,
    #[serde(default)]
    pub error: Option<AnthropicErrorDetail>,
}

#[derive(Deserialize, Debug)]
pub struct AnthropicUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub struct AnthropicErrorDetail {
    pub message: String,
    #[serde(default)]
    pub error_type: Option<String>,
}

impl AnthropicMessage {
    /// Create a user message with text content.
    #[allow(dead_code)]
    pub fn user(content: &str) -> Self {
        Self {
            role: "user".to_string(),
            content: vec![AnthropicContent::Text {
                text: content.to_string(),
            }],
        }
    }

    /// Create an assistant message with text content.
    #[allow(dead_code)]
    pub fn assistant(content: &str) -> Self {
        Self {
            role: "assistant".to_string(),
            content: vec![AnthropicContent::Text {
                text: content.to_string(),
            }],
        }
    }

    /// Create a user message with mixed content (for tool results).
    #[allow(dead_code)]
    pub fn with_content(role: &str, content: Vec<AnthropicContent>) -> Self {
        Self {
            role: role.to_string(),
            content,
        }
    }
}
