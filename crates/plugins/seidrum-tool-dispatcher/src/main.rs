use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use futures::StreamExt as _;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use seidrum_common::events::{
    EventEnvelope, PluginRegister, ToolCallRequest, ToolCallResponse, ToolDescribeRequest,
    ToolDescribeResponse, ToolRegistered,
};
use seidrum_common::nats_utils::NatsClient;

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "seidrum-tool-dispatcher",
    about = "Seidrum capability dispatcher plugin — routes capability.call requests to owning plugins"
)]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4222")]
    nats_url: String,

    /// Timeout for tool calls in seconds
    #[arg(long, env = "TOOL_CALL_TIMEOUT", default_value = "30")]
    tool_call_timeout: u64,
}

// ---------------------------------------------------------------------------
// Tool cache
// ---------------------------------------------------------------------------

/// Cached info for a registered tool: (plugin_id, call_subject).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolEntry {
    plugin_id: String,
    call_subject: String,
}

/// Thread-safe cache mapping tool_id -> ToolEntry.
type ToolCache = Arc<RwLock<HashMap<String, ToolEntry>>>;

fn new_tool_cache() -> ToolCache {
    Arc::new(RwLock::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let timeout = Duration::from_secs(cli.tool_call_timeout);

    info!(
        nats_url = %cli.nats_url,
        timeout_secs = cli.tool_call_timeout,
        "Starting seidrum-tool-dispatcher plugin..."
    );

    // Connect to NATS
    let nats = NatsClient::connect(&cli.nats_url, "tool-dispatcher").await?;

    // Publish plugin registration
    let register = PluginRegister {
        id: "tool-dispatcher".to_string(),
        name: "Tool Dispatcher".to_string(),
        version: "0.1.0".to_string(),
        description:
            "Routes unified capability.call requests to the plugin that owns the capability"
                .to_string(),
        consumes: vec!["capability.call".to_string()],
        produces: vec![],
        health_subject: "plugin.tool-dispatcher.health".to_string(),
        consumed_event_types: vec![],
        produced_event_types: vec![],
    };
    nats.publish_envelope("plugin.register", None, None, &register)
        .await?;
    info!("Published plugin.register");

    // Build tool cache
    let cache = new_tool_cache();

    // Subscribe to tool.registered events (background listener)
    let cache_bg = cache.clone();
    let nats_bg = nats.clone();
    tokio::spawn(async move {
        if let Err(e) = listen_tool_registered(&nats_bg, cache_bg).await {
            error!(error = %e, "capability.registered listener failed");
        }
    });

    // Subscribe to capability.call (request/reply service)
    let mut sub = nats
        .inner()
        .subscribe("capability.call".to_string())
        .await?;
    info!("Subscribed to capability.call (request/reply)");

    while let Some(msg) = sub.next().await {
        let nats_clone = nats.clone();
        let cache_clone = cache.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tool_call(msg, &nats_clone, &cache_clone, timeout).await {
                error!(error = %e, "Failed to handle capability.call");
            }
        });
    }

    warn!("capability.call subscription ended, shutting down");
    Ok(())
}

// ---------------------------------------------------------------------------
// tool.registered listener — keeps cache fresh
// ---------------------------------------------------------------------------

async fn listen_tool_registered(nats: &NatsClient, cache: ToolCache) -> Result<()> {
    let mut sub = nats.subscribe("capability.registered").await?;
    info!("Subscribed to capability.registered (cache updater)");

    while let Some(msg) = sub.next().await {
        match serde_json::from_slice::<EventEnvelope>(&msg.payload) {
            Ok(envelope) => {
                match serde_json::from_value::<ToolRegistered>(envelope.payload) {
                    Ok(registered) => {
                        info!(
                            tool_id = %registered.tool_id,
                            plugin_id = %registered.plugin_id,
                            "Cached tool from capability.registered event"
                        );
                        // We only have tool_id and plugin_id from ToolRegistered.
                        // We need the call_subject — fetch it via capability.describe.
                        match describe_tool(nats, &registered.tool_id).await {
                            Ok(desc) => {
                                let mut w = cache.write().await;
                                w.insert(
                                    registered.tool_id.clone(),
                                    ToolEntry {
                                        plugin_id: desc.plugin_id,
                                        call_subject: desc.call_subject,
                                    },
                                );
                            }
                            Err(e) => {
                                warn!(
                                    tool_id = %registered.tool_id,
                                    error = %e,
                                    "Failed to describe capability after registration, skipping cache"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to parse ToolRegistered payload");
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to parse capability.registered envelope");
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// tool.describe helper
// ---------------------------------------------------------------------------

async fn describe_tool(nats: &NatsClient, tool_id: &str) -> Result<ToolDescribeResponse> {
    let req = ToolDescribeRequest {
        tool_id: tool_id.to_string(),
    };
    let resp: ToolDescribeResponse = nats.request("capability.describe", &req).await?;
    Ok(resp)
}

// ---------------------------------------------------------------------------
// tool.call handler (request/reply)
// ---------------------------------------------------------------------------

async fn handle_tool_call(
    msg: async_nats::Message,
    nats: &NatsClient,
    cache: &ToolCache,
    timeout: Duration,
) -> Result<()> {
    let reply = match &msg.reply {
        Some(r) => r.clone(),
        None => {
            warn!("Received capability.call without reply subject, ignoring");
            return Ok(());
        }
    };

    // Parse the envelope
    let envelope: EventEnvelope = serde_json::from_slice(&msg.payload)?;

    // Parse the ToolCallRequest from the envelope payload
    let call_req: ToolCallRequest = serde_json::from_value(envelope.payload)?;

    let tool_id = &call_req.tool_id;
    info!(
        tool_id = %tool_id,
        correlation_id = ?call_req.correlation_id,
        "Received capability.call request"
    );

    // 1. Look up in cache
    let entry = {
        let r = cache.read().await;
        r.get(tool_id).cloned()
    };

    let entry = match entry {
        Some(e) => e,
        None => {
            // 2. Cache miss — query kernel via capability.describe
            info!(tool_id = %tool_id, "Cache miss, querying capability.describe");
            match describe_tool(nats, tool_id).await {
                Ok(desc) => {
                    let entry = ToolEntry {
                        plugin_id: desc.plugin_id,
                        call_subject: desc.call_subject,
                    };
                    // Cache it
                    {
                        let mut w = cache.write().await;
                        w.insert(tool_id.clone(), entry.clone());
                    }
                    entry
                }
                Err(e) => {
                    // 3. Still not found — reply with error
                    warn!(tool_id = %tool_id, error = %e, "Tool not found");
                    let err_resp = ToolCallResponse {
                        tool_id: tool_id.clone(),
                        result: serde_json::json!({
                            "error": format!("Unknown tool: {tool_id}"),
                            "details": e.to_string()
                        }),
                        is_error: true,
                    };
                    let bytes = serde_json::to_vec(&err_resp)?;
                    nats.inner()
                        .publish(reply.to_string(), bytes.into())
                        .await?;
                    return Ok(());
                }
            }
        }
    };

    info!(
        tool_id = %tool_id,
        plugin_id = %entry.plugin_id,
        call_subject = %entry.call_subject,
        "Dispatching tool call"
    );

    // 4. Forward the call to the owning plugin via NATS request/reply with timeout
    let start = tokio::time::Instant::now();

    let forward_payload = serde_json::to_vec(&call_req)?;
    let forward_result = tokio::time::timeout(
        timeout,
        nats.inner()
            .request(entry.call_subject.clone(), forward_payload.into()),
    )
    .await;

    let duration = start.elapsed();

    let response = match forward_result {
        Ok(Ok(resp_msg)) => {
            // 5. Parse the owning plugin's response
            match serde_json::from_slice::<ToolCallResponse>(&resp_msg.payload) {
                Ok(tool_resp) => {
                    info!(
                        tool_id = %tool_id,
                        plugin_id = %entry.plugin_id,
                        duration_ms = duration.as_millis() as u64,
                        is_error = tool_resp.is_error,
                        "Tool call completed"
                    );
                    tool_resp
                }
                Err(e) => {
                    error!(
                        tool_id = %tool_id,
                        error = %e,
                        "Failed to parse tool response from plugin"
                    );
                    ToolCallResponse {
                        tool_id: tool_id.clone(),
                        result: serde_json::json!({
                            "error": "Failed to parse tool response",
                            "details": e.to_string()
                        }),
                        is_error: true,
                    }
                }
            }
        }
        Ok(Err(e)) => {
            error!(
                tool_id = %tool_id,
                plugin_id = %entry.plugin_id,
                duration_ms = duration.as_millis() as u64,
                error = %e,
                "NATS request to tool plugin failed"
            );
            ToolCallResponse {
                tool_id: tool_id.clone(),
                result: serde_json::json!({
                    "error": "NATS request failed",
                    "details": e.to_string()
                }),
                is_error: true,
            }
        }
        Err(_) => {
            error!(
                tool_id = %tool_id,
                plugin_id = %entry.plugin_id,
                timeout_secs = timeout.as_secs(),
                "Tool call timed out"
            );
            ToolCallResponse {
                tool_id: tool_id.clone(),
                result: serde_json::json!({
                    "error": "Tool call timed out",
                    "timeout_secs": timeout.as_secs()
                }),
                is_error: true,
            }
        }
    };

    // 6. Reply to the original caller
    let resp_bytes = serde_json::to_vec(&response)?;
    nats.inner()
        .publish(reply.to_string(), resp_bytes.into())
        .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_entry_equality() {
        let a = ToolEntry {
            plugin_id: "plugin-a".to_string(),
            call_subject: "plugin.a.tool.foo".to_string(),
        };
        let b = ToolEntry {
            plugin_id: "plugin-a".to_string(),
            call_subject: "plugin.a.tool.foo".to_string(),
        };
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn test_cache_insert_and_lookup() {
        let cache = new_tool_cache();

        // Initially empty
        {
            let r = cache.read().await;
            assert!(r.get("tool-1").is_none());
        }

        // Insert
        {
            let mut w = cache.write().await;
            w.insert(
                "tool-1".to_string(),
                ToolEntry {
                    plugin_id: "plugin-code-exec".to_string(),
                    call_subject: "plugin.code-exec.tool.run_python".to_string(),
                },
            );
        }

        // Lookup
        {
            let r = cache.read().await;
            let entry = r.get("tool-1").expect("tool-1 should exist in cache");
            assert_eq!(entry.plugin_id, "plugin-code-exec");
            assert_eq!(entry.call_subject, "plugin.code-exec.tool.run_python");
        }
    }

    #[tokio::test]
    async fn test_cache_overwrite() {
        let cache = new_tool_cache();

        {
            let mut w = cache.write().await;
            w.insert(
                "tool-1".to_string(),
                ToolEntry {
                    plugin_id: "old-plugin".to_string(),
                    call_subject: "old.subject".to_string(),
                },
            );
        }

        // Overwrite
        {
            let mut w = cache.write().await;
            w.insert(
                "tool-1".to_string(),
                ToolEntry {
                    plugin_id: "new-plugin".to_string(),
                    call_subject: "new.subject".to_string(),
                },
            );
        }

        {
            let r = cache.read().await;
            let entry = r.get("tool-1").expect("tool-1 should exist");
            assert_eq!(entry.plugin_id, "new-plugin");
            assert_eq!(entry.call_subject, "new.subject");
        }
    }

    #[tokio::test]
    async fn test_cache_multiple_tools() {
        let cache = new_tool_cache();

        {
            let mut w = cache.write().await;
            w.insert(
                "tool-a".to_string(),
                ToolEntry {
                    plugin_id: "plugin-1".to_string(),
                    call_subject: "p1.tool.a".to_string(),
                },
            );
            w.insert(
                "tool-b".to_string(),
                ToolEntry {
                    plugin_id: "plugin-2".to_string(),
                    call_subject: "p2.tool.b".to_string(),
                },
            );
        }

        {
            let r = cache.read().await;
            assert_eq!(r.len(), 2);
            assert_eq!(r.get("tool-a").unwrap().plugin_id, "plugin-1");
            assert_eq!(r.get("tool-b").unwrap().plugin_id, "plugin-2");
        }
    }

    #[test]
    fn test_tool_call_response_serialization() {
        let resp = ToolCallResponse {
            tool_id: "tool-001".to_string(),
            result: serde_json::json!({"stdout": "hello\n", "exit_code": 0}),
            is_error: false,
        };

        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: ToolCallResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(resp.tool_id, deserialized.tool_id);
        assert_eq!(resp.result, deserialized.result);
        assert_eq!(resp.is_error, deserialized.is_error);
    }

    #[test]
    fn test_tool_call_response_error_serialization() {
        let resp = ToolCallResponse {
            tool_id: "tool-missing".to_string(),
            result: serde_json::json!({
                "error": "Unknown tool: tool-missing",
                "details": "not found"
            }),
            is_error: true,
        };

        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: ToolCallResponse = serde_json::from_str(&json).unwrap();

        assert!(deserialized.is_error);
        assert_eq!(deserialized.tool_id, "tool-missing");
        assert_eq!(deserialized.result["error"], "Unknown tool: tool-missing");
    }

    #[test]
    fn test_tool_call_request_roundtrip() {
        let req = ToolCallRequest {
            tool_id: "tool-001".to_string(),
            plugin_id: "plugin-code-exec".to_string(),
            arguments: serde_json::json!({"code": "print('hi')"}),
            correlation_id: Some("corr-123".to_string()),
        };

        let json = serde_json::to_string(&req).unwrap();
        let deserialized: ToolCallRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(req.tool_id, deserialized.tool_id);
        assert_eq!(req.plugin_id, deserialized.plugin_id);
        assert_eq!(req.arguments, deserialized.arguments);
        assert_eq!(req.correlation_id, deserialized.correlation_id);
    }
}
