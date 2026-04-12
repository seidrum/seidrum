use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use lru::LruCache;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use seidrum_common::events::{
    AgentFeedback, ChannelInbound, EventEnvelope, FeedbackType, LlmResponse, PluginHealthResponse,
    PluginRegister,
};

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "seidrum-feedback-extractor",
    about = "Seidrum feedback extraction and analysis plugin"
)]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

    /// Optional LLM provider for ambiguous feedback classification
    /// E.g., "google", "anthropic"
    #[arg(long, env = "LLM_PROVIDER")]
    llm_provider: Option<String>,

    /// Maximum number of recent agent responses to keep in buffer
    #[arg(long, env = "RESPONSE_BUFFER_SIZE", default_value = "100")]
    response_buffer_size: usize,

    /// Heuristic confidence threshold for feedback detection (0.0-1.0)
    #[arg(long, env = "HEURISTIC_THRESHOLD", default_value = "0.7")]
    heuristic_threshold: f64,
}

// ---------------------------------------------------------------------------
// Response Context Buffer
// ---------------------------------------------------------------------------

/// Stores recent agent responses by correlation_id for feedback attribution
#[derive(Clone, Debug)]
struct ResponseContext {
    content: String,
    #[allow(dead_code)]
    timestamp: i64,
}

// ---------------------------------------------------------------------------
// Feedback Heuristics
// ---------------------------------------------------------------------------

/// Simple heuristic patterns that indicate feedback
const CORRECTION_PATTERNS: &[&str] = &[
    "no, i",
    "no that's",
    "not that",
    "i meant",
    "i think you",
    "wrong",
    "incorrect",
];

const CONFIRMATION_PATTERNS: &[&str] = &[
    "yes, exactly",
    "yes exactly",
    "that's right",
    "perfect",
    "spot on",
    "precisely",
    "correct",
];

const PREFERENCE_PATTERNS: &[&str] = &[
    "i prefer",
    "i'd prefer",
    "i like",
    "i dislike",
    "shorter",
    "longer",
    "more detailed",
    "less detailed",
    "always do",
    "never do",
    "don't",
    "avoid",
];

/// Run heuristic analysis on user message to detect feedback
fn analyze_heuristics(text: &str) -> (f64, Option<FeedbackType>) {
    let lower = text.to_lowercase();

    // Check correction patterns
    for pattern in CORRECTION_PATTERNS {
        if lower.contains(pattern) {
            return (0.85, Some(FeedbackType::Correction));
        }
    }

    // Check confirmation patterns
    for pattern in CONFIRMATION_PATTERNS {
        if lower.contains(pattern) {
            return (0.88, Some(FeedbackType::Confirmation));
        }
    }

    // Check preference patterns
    for pattern in PREFERENCE_PATTERNS {
        if lower.contains(pattern) {
            // Higher confidence if we see preference-related words
            return (0.75, Some(FeedbackType::Preference));
        }
    }

    // No strong patterns detected
    (0.0, None)
}

/// Extract key-value preference from text (very basic)
fn extract_preference(text: &str) -> (Option<String>, Option<String>) {
    let lower = text.to_lowercase();

    if lower.contains("short") && lower.contains("response") {
        return (
            Some("response_length".to_string()),
            Some("short".to_string()),
        );
    }
    if lower.contains("long") && lower.contains("response") {
        return (
            Some("response_length".to_string()),
            Some("long".to_string()),
        );
    }
    if lower.contains("concise") {
        return (Some("style".to_string()), Some("concise".to_string()));
    }
    if lower.contains("detailed") {
        return (Some("style".to_string()), Some("detailed".to_string()));
    }
    if lower.contains("formal") {
        return (Some("tone".to_string()), Some("formal".to_string()));
    }
    if lower.contains("casual") || lower.contains("friendly") {
        return (Some("tone".to_string()), Some("casual".to_string()));
    }

    (None, None)
}

// ---------------------------------------------------------------------------
// Main Plugin Logic
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(
        nats_url = %cli.nats_url,
        llm_provider = ?cli.llm_provider,
        response_buffer_size = cli.response_buffer_size,
        heuristic_threshold = cli.heuristic_threshold,
        "Starting seidrum-feedback-extractor plugin..."
    );

    // Connect to NATS
    let nats =
        seidrum_common::bus_client::BusClient::connect(&cli.nats_url, "feedback-extractor").await?;
    info!("Connected to NATS");

    // Initialize response buffer (LRU cache)
    let response_buffer: Arc<Mutex<LruCache<String, ResponseContext>>> = Arc::new(Mutex::new(
        LruCache::new(std::num::NonZeroUsize::new(cli.response_buffer_size).unwrap()),
    ));

    // Track plugin health (shared state for spawned tasks)
    let events_processed = Arc::new(AtomicU64::new(0));
    let last_error: Arc<tokio::sync::Mutex<Option<String>>> =
        Arc::new(tokio::sync::Mutex::new(None));
    let start_time = Instant::now();

    // Publish plugin registration
    let register = PluginRegister {
        id: "feedback-extractor".to_string(),
        name: "Feedback Extractor".to_string(),
        version: "0.1.0".to_string(),
        description:
            "Analyzes user messages for feedback signals and extracts user preferences from interaction patterns"
                .to_string(),
        consumes: vec![
            "channel.*.inbound".to_string(),
            "llm.response".to_string(),
            "plugin.feedback-extractor.health".to_string(),
        ],
        produces: vec!["agent.feedback".to_string()],
        health_subject: "plugin.feedback-extractor.health".to_string(),
        consumed_event_types: vec!["ChannelInbound".to_string(), "LlmResponse".to_string()],
        produced_event_types: vec!["AgentFeedback".to_string()],
        config_schema: None,
    };
    let register_envelope = EventEnvelope::new(
        "plugin.register",
        "feedback-extractor",
        None,
        None,
        &register,
    )?;
    nats.publish_bytes("plugin.register", serde_json::to_vec(&register_envelope)?)
        .await?;
    info!("Published plugin.register");

    // Subscribe to channel inbound messages (from all platforms)
    let mut channel_sub = nats.subscribe("channel.*.inbound").await?;
    info!("Subscribed to channel.*.inbound");

    // Subscribe to LLM responses for context tracking
    let llm_sub = nats.subscribe("llm.response").await?;
    info!("Subscribed to llm.response");

    // Subscribe to health checks
    let mut health_sub = nats.subscribe("plugin.feedback-extractor.health").await?;
    info!("Subscribed to plugin.feedback-extractor.health");

    let nats_clone = nats.clone();
    let response_buffer_clone = response_buffer.clone();
    let _llm_provider_clone = cli.llm_provider.clone();
    let heuristic_threshold = cli.heuristic_threshold;
    let events_processed_clone = events_processed.clone();

    // Spawn task for handling channel inbound messages
    tokio::spawn(async move {
        while let Some(msg) = channel_sub.next().await {
            match serde_json::from_slice::<EventEnvelope>(&msg.payload) {
                Ok(envelope) => {
                    match serde_json::from_value::<ChannelInbound>(envelope.payload.clone()) {
                        Ok(inbound) => {
                            // Analyze for feedback
                            let (heuristic_score, feedback_type) =
                                analyze_heuristics(&inbound.text);

                            if heuristic_score >= heuristic_threshold {
                                // Strong heuristic signal - emit feedback event
                                let (pref_key, pref_value) = extract_preference(&inbound.text);

                                let feedback = AgentFeedback {
                                    agent_id: envelope.scope.clone().unwrap_or_default(),
                                    conversation_id: None,
                                    message_id: Some(inbound.chat_id.clone()),
                                    feedback_type: feedback_type
                                        .clone()
                                        .unwrap_or(FeedbackType::Preference),
                                    content: inbound.text.clone(),
                                    prior_context: {
                                        // Try to get prior response context
                                        let buf = response_buffer_clone.lock().await;
                                        envelope
                                            .correlation_id
                                            .as_ref()
                                            .and_then(|cid| buf.peek(cid))
                                            .map(|ctx| ctx.content.clone())
                                    },
                                    preference_key: pref_key,
                                    preference_value: pref_value,
                                    confidence: heuristic_score,
                                    timestamp: Utc::now(),
                                };

                                let feedback_envelope = match EventEnvelope::new(
                                    "agent.feedback",
                                    "feedback-extractor",
                                    envelope.correlation_id.clone(),
                                    envelope.scope,
                                    &feedback,
                                ) {
                                    Ok(e) => e,
                                    Err(e) => {
                                        error!(error = %e, "Failed to create feedback envelope");
                                        continue;
                                    }
                                };

                                let payload_bytes = match serde_json::to_vec(&feedback_envelope) {
                                    Ok(b) => b,
                                    Err(e) => {
                                        error!(error = %e, "Failed to serialize feedback envelope");
                                        continue;
                                    }
                                };

                                if let Err(e) = nats_clone
                                    .publish_bytes("agent.feedback", payload_bytes)
                                    .await
                                {
                                    error!(error = %e, "Failed to publish agent.feedback event");
                                }

                                info!(
                                    confidence = heuristic_score,
                                    feedback_type = ?feedback_type,
                                    "Detected feedback signal"
                                );
                            } else if heuristic_score > 0.5 {
                                // Ambiguous signal - optionally use LLM to classify
                                info!(
                                    confidence = heuristic_score,
                                    "Feedback signal below threshold; skipping LLM classification"
                                );
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to deserialize ChannelInbound payload");
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to deserialize EventEnvelope");
                }
            }

            events_processed_clone.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Spawn task for handling LLM responses (context tracking)
    let mut llm_sub_next = llm_sub;
    let response_buffer_clone2 = response_buffer.clone();
    tokio::spawn(async move {
        while let Some(msg) = llm_sub_next.next().await {
            match serde_json::from_slice::<EventEnvelope>(&msg.payload) {
                Ok(envelope) => {
                    match serde_json::from_value::<LlmResponse>(envelope.payload.clone()) {
                        Ok(response) => {
                            // Store in buffer for feedback attribution
                            if let Some(cid) = envelope.correlation_id.as_ref() {
                                let context = ResponseContext {
                                    content: response.content.unwrap_or_default(),
                                    timestamp: Utc::now().timestamp(),
                                };
                                let mut buf = response_buffer_clone2.lock().await;
                                buf.put(cid.clone(), context);
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to deserialize LlmResponse payload");
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to deserialize EventEnvelope from LLM response");
                }
            }
        }
    });

    // Handle health checks
    while let Some(msg) = health_sub.next().await {
        match msg.reply {
            Some(reply_subject) => {
                let current_error = last_error.lock().await.clone();
                let health_response = PluginHealthResponse {
                    plugin_id: "feedback-extractor".to_string(),
                    status: if current_error.is_none() {
                        "healthy".to_string()
                    } else {
                        "degraded".to_string()
                    },
                    uptime_seconds: start_time.elapsed().as_secs(),
                    events_processed: events_processed.load(Ordering::Relaxed),
                    last_error: current_error,
                };

                if let Ok(response_bytes) = serde_json::to_vec(&health_response) {
                    if let Err(e) = nats.publish_bytes(reply_subject, response_bytes).await {
                        error!(error = %e, "Failed to publish health response");
                    }
                }
            }
            None => {
                warn!("Received health check without reply subject");
            }
        }
    }

    info!("Feedback extractor shutting down");
    Ok(())
}
