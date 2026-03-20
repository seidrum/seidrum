// Gemini API types (reqwest, no SDK).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GeminiRequest {
    pub contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GeminiGenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GeminiToolDeclaration>>,
}

#[derive(Serialize, Debug, Clone)]
pub struct GeminiSystemInstruction {
    pub parts: Vec<GeminiPart>,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// Gemini tool declaration wrapper.
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GeminiToolDeclaration {
    pub function_declarations: Vec<GeminiFunctionDeclaration>,
}

/// A single function declaration in Gemini format.
#[derive(Serialize, Debug, Clone)]
pub struct GeminiFunctionDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Content / Parts (shared between request and response)
// ---------------------------------------------------------------------------

/// Gemini message content (used for both request and response).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GeminiContent {
    pub role: String,
    pub parts: Vec<GeminiPart>,
}

/// A part in a Gemini message. Can be text, functionCall, or functionResponse.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<GeminiFunctionResponse>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GeminiFunctionCall {
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GeminiFunctionResponse {
    pub name: String,
    pub response: serde_json::Value,
}

impl GeminiContent {
    /// Create a simple text message.
    pub fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            parts: vec![GeminiPart {
                text: Some(content.to_string()),
                function_call: None,
                function_response: None,
            }],
        }
    }
}

impl GeminiPart {
    /// Create a text-only part.
    pub fn text_part(text: &str) -> Self {
        Self {
            text: Some(text.to_string()),
            function_call: None,
            function_response: None,
        }
    }

    /// Create a function_call part.
    pub fn function_call_part(name: &str, args: serde_json::Value) -> Self {
        Self {
            text: None,
            function_call: Some(GeminiFunctionCall {
                name: name.to_string(),
                args,
            }),
            function_response: None,
        }
    }

    /// Create a function_response part.
    pub fn function_response_part(name: &str, response: serde_json::Value) -> Self {
        Self {
            text: None,
            function_call: None,
            function_response: Some(GeminiFunctionResponse {
                name: name.to_string(),
                response,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
pub struct GeminiResponse {
    #[serde(default)]
    pub candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata")]
    pub usage_metadata: Option<GeminiUsageMetadata>,
    #[serde(default)]
    pub error: Option<GeminiErrorDetail>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GeminiCandidate {
    pub content: GeminiContent,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GeminiUsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u32,
    #[serde(default)]
    pub candidates_token_count: u32,
    #[serde(default)]
    pub total_token_count: u32,
}

#[derive(Deserialize, Debug)]
pub struct GeminiErrorDetail {
    pub message: String,
    #[serde(default)]
    pub code: Option<i32>,
}
