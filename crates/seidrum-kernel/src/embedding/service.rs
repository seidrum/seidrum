//! Embedding service: generates vector embeddings from text.
//!
//! Currently supports OpenAI's text-embedding-3-small model.
//! The service is shared across the kernel (used by skills, content, entities).

use anyhow::{Context, Result};
use reqwest::Client;
use tracing::{debug, info, warn};

/// Configurable embedding generator.
#[derive(Clone)]
pub struct EmbeddingService {
    provider: String,
    api_key: String,
    http: Client,
    model: String,
    pub dimension: u32,
}

impl EmbeddingService {
    /// Create a new embedding service from environment variables.
    pub fn from_env() -> Result<Self> {
        let provider = std::env::var("EMBEDDING_PROVIDER").unwrap_or_else(|_| "openai".to_string());
        let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();

        if api_key.is_empty() {
            warn!("OPENAI_API_KEY not set — embedding service will be unavailable. Skills will be stored without embeddings.");
        } else {
            info!(provider = %provider, "Embedding service configured");
        }

        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client for embeddings")?;

        Ok(Self {
            provider,
            api_key,
            http,
            model: "text-embedding-3-small".to_string(),
            dimension: 1536,
        })
    }

    /// Check if the embedding service is configured and ready.
    pub fn is_available(&self) -> bool {
        !self.api_key.is_empty() && self.provider == "openai"
    }

    /// Generate an embedding vector from text.
    /// Returns an error if the service is not configured or the text is empty.
    pub async fn embed(&self, text: &str) -> Result<Vec<f64>> {
        if !self.is_available() {
            anyhow::bail!("Embedding service not configured (missing OPENAI_API_KEY)");
        }

        let trimmed = text.trim();
        if trimmed.is_empty() {
            anyhow::bail!("Cannot embed empty text");
        }

        let body = serde_json::json!({
            "model": self.model,
            "input": trimmed,
        });

        let resp = self
            .http
            .post("https://api.openai.com/v1/embeddings")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .context("embedding API request failed")?;

        let status = resp.status();
        let text_resp = resp
            .text()
            .await
            .context("failed to read embedding response")?;

        if !status.is_success() {
            anyhow::bail!("embedding API returned {}: {}", status, text_resp);
        }

        let json: serde_json::Value =
            serde_json::from_str(&text_resp).context("invalid JSON from embedding API")?;

        let embedding = json
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("embedding"))
            .and_then(|e| e.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_f64()).collect::<Vec<f64>>())
            .context("unexpected embedding response format")?;

        debug!(dimension = embedding.len(), "Generated embedding");

        Ok(embedding)
    }
}
