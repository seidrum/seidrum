//! WebSocket handler: upgrades connections, routes messages between external
//! plugins and NATS.

use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use seidrum_common::events::{
    PluginDeregister, PluginRegister, StorageDeleteRequest, StorageDeleteResponse,
    StorageGetRequest, StorageGetResponse, StorageListRequest, StorageListResponse,
    StorageSetRequest, StorageSetResponse, ToolCallRequest,
};
use seidrum_common::nats_utils::NatsClient;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::connections::{ConnectionManager, PendingRequest};
use crate::protocol::{ClientMessage, PluginInfo, ServerMessage};

/// Default timeout for capability call responses from external plugins.
const CAPABILITY_TIMEOUT_SECS: u64 = 30;
/// Default timeout for health check responses from external plugins.
const HEALTH_TIMEOUT_SECS: u64 = 5;

/// Handle a single WebSocket connection.
pub async fn handle_ws(socket: WebSocket, nats: NatsClient, connections: ConnectionManager) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

    // Spawn writer task: forwards ServerMessages to the WebSocket
    let write_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Ok(text) = serde_json::to_string(&msg) {
                if ws_sender.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        }
    });

    let mut plugin_id: Option<String> = None;

    // Read messages from the WebSocket
    while let Some(Ok(msg)) = ws_receiver.next().await {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => continue,
            _ => continue,
        };

        let client_msg: ClientMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(err) => {
                let _ = tx.send(ServerMessage::Error {
                    message: format!("Invalid message: {}", err),
                });
                continue;
            }
        };

        match client_msg {
            ClientMessage::Register { plugin } => {
                if plugin_id.is_some() {
                    let _ = tx.send(ServerMessage::Error {
                        message: "Already registered. Send deregister first.".into(),
                    });
                    continue;
                }

                match handle_register(&plugin, &nats, &connections, tx.clone()).await {
                    Ok(()) => {
                        let pid = plugin.id.clone();
                        plugin_id = Some(pid.clone());

                        // Subscribe to capability calls for this plugin
                        spawn_capability_listener(&pid, &nats, &connections).await;
                        // Subscribe to health checks for this plugin
                        spawn_health_listener(&pid, &nats, &connections).await;

                        let _ = tx.send(ServerMessage::Registered { plugin_id: pid });
                    }
                    Err(err) => {
                        let _ = tx.send(ServerMessage::Error { message: err });
                    }
                }
            }

            ClientMessage::RegisterCapability { capability } => {
                if plugin_id.is_none() {
                    let _ = tx.send(ServerMessage::Error {
                        message: "Must register before registering capabilities.".into(),
                    });
                    continue;
                }
                if let Err(e) = handle_register_capability(&capability, &nats).await {
                    let _ = tx.send(ServerMessage::Error { message: e });
                }
            }

            ClientMessage::Deregister {} => {
                if let Some(ref pid) = plugin_id {
                    handle_deregister(pid, &nats, &connections).await;
                    plugin_id = None;
                }
                break;
            }

            ClientMessage::CapabilityResponse {
                request_id,
                response,
            } => {
                handle_pending_response(&request_id, &response, &connections).await;
            }

            ClientMessage::HealthResponse {
                request_id,
                response,
            } => {
                handle_pending_health_response(&request_id, &response, &connections).await;
            }

            ClientMessage::Subscribe { subjects } => {
                if let Some(ref pid) = plugin_id {
                    handle_subscribe(pid, &subjects, &nats, &connections).await;
                }
            }

            ClientMessage::Unsubscribe { .. } => {
                // For v1, unsubscribe is a no-op — subscriptions are cleaned up on disconnect.
                // A full implementation would track subscription handles by subject.
            }

            ClientMessage::Publish { subject, payload } => {
                if let Some(ref pid) = plugin_id {
                    handle_publish(pid, &subject, &payload, &nats).await;
                }
            }

            ClientMessage::StorageGet {
                request_id,
                namespace,
                key,
            } => {
                if let Some(ref pid) = plugin_id {
                    handle_storage_get(pid, &request_id, &namespace, &key, &nats, &tx).await;
                }
            }

            ClientMessage::StorageSet {
                request_id,
                namespace,
                key,
                value,
            } => {
                if let Some(ref pid) = plugin_id {
                    handle_storage_set(pid, &request_id, &namespace, &key, &value, &nats, &tx)
                        .await;
                }
            }

            ClientMessage::StorageDelete {
                request_id,
                namespace,
                key,
            } => {
                if let Some(ref pid) = plugin_id {
                    handle_storage_delete(pid, &request_id, &namespace, &key, &nats, &tx).await;
                }
            }

            ClientMessage::StorageList {
                request_id,
                namespace,
            } => {
                if let Some(ref pid) = plugin_id {
                    handle_storage_list(pid, &request_id, &namespace, &nats, &tx).await;
                }
            }
        }
    }

    // Connection closed — clean up
    if let Some(ref pid) = plugin_id {
        handle_deregister(pid, &nats, &connections).await;
    }
    write_task.abort();
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

async fn handle_register(
    plugin: &PluginInfo,
    nats: &NatsClient,
    connections: &ConnectionManager,
    sender: mpsc::UnboundedSender<ServerMessage>,
) -> Result<(), String> {
    connections
        .register(plugin.id.clone(), sender)
        .map_err(|e| e.to_string())?;

    // Publish plugin.register to NATS
    let register = PluginRegister {
        id: plugin.id.clone(),
        name: plugin.name.clone(),
        version: plugin.version.clone(),
        description: plugin.description.clone(),
        consumes: plugin.consumes.clone(),
        produces: plugin.produces.clone(),
        health_subject: format!("plugin.{}.health", plugin.id),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };

    nats.publish_envelope("plugin.register", None, None, &register)
        .await
        .map_err(|e| format!("Failed to publish plugin.register: {}", e))?;

    info!(plugin_id = %plugin.id, "External plugin registered via gateway");
    Ok(())
}

async fn handle_register_capability(
    capability: &serde_json::Value,
    nats: &NatsClient,
) -> Result<(), String> {
    let bytes =
        serde_json::to_vec(capability).map_err(|e| format!("Failed to serialize: {}", e))?;
    nats.inner()
        .publish("capability.register".to_string(), bytes.into())
        .await
        .map_err(|e| format!("Failed to publish capability.register: {}", e))?;
    Ok(())
}

async fn handle_deregister(plugin_id: &str, nats: &NatsClient, connections: &ConnectionManager) {
    connections.deregister(plugin_id).await;

    let dereg = PluginDeregister {
        id: plugin_id.to_string(),
    };
    if let Ok(bytes) = serde_json::to_vec(&dereg) {
        let _ = nats
            .inner()
            .publish("plugin.deregister".to_string(), bytes.into())
            .await;
    }

    info!(%plugin_id, "External plugin deregistered via gateway");
}

// ---------------------------------------------------------------------------
// Capability call + health check forwarding
// ---------------------------------------------------------------------------

async fn spawn_capability_listener(
    plugin_id: &str,
    nats: &NatsClient,
    connections: &ConnectionManager,
) {
    let subject = format!("capability.call.{}", plugin_id);
    let nats_client = nats.inner().clone();
    let conn_inner = connections.clone();
    let pid = plugin_id.to_string();

    let handle = tokio::spawn(async move {
        let mut sub = match nats_client.subscribe(subject.clone()).await {
            Ok(s) => s,
            Err(err) => {
                error!(%err, %subject, "Failed to subscribe to capability calls");
                return;
            }
        };

        while let Some(msg) = sub.next().await {
            let reply = match msg.reply {
                Some(ref r) => r.clone(),
                None => continue,
            };

            let request: ToolCallRequest = match serde_json::from_slice(&msg.payload) {
                Ok(r) => r,
                Err(err) => {
                    warn!(%err, "Failed to parse capability call");
                    continue;
                }
            };

            let request_id = ulid::Ulid::new().to_string();

            conn_inner.add_pending(
                request_id.clone(),
                PendingRequest {
                    reply_subject: reply,
                    deadline: Instant::now() + Duration::from_secs(CAPABILITY_TIMEOUT_SECS),
                    nats_client: nats_client.clone(),
                    is_health_check: false,
                },
            );

            if let Some(sender) = conn_inner.get_sender(&pid) {
                let _ = sender.send(ServerMessage::CapabilityCall {
                    request_id,
                    request,
                });
                conn_inner.increment_events(&pid);
            }
        }
    });

    connections.add_subscription(plugin_id, handle).await;
}

async fn spawn_health_listener(
    plugin_id: &str,
    nats: &NatsClient,
    connections: &ConnectionManager,
) {
    let subject = format!("plugin.{}.health", plugin_id);
    let nats_client = nats.inner().clone();
    let conn_inner = connections.clone();
    let pid = plugin_id.to_string();

    let handle = tokio::spawn(async move {
        let mut sub = match nats_client.subscribe(subject.clone()).await {
            Ok(s) => s,
            Err(err) => {
                error!(%err, %subject, "Failed to subscribe to health checks");
                return;
            }
        };

        while let Some(msg) = sub.next().await {
            let reply = match msg.reply {
                Some(ref r) => r.clone(),
                None => continue,
            };

            let request_id = ulid::Ulid::new().to_string();

            conn_inner.add_pending(
                request_id.clone(),
                PendingRequest {
                    reply_subject: reply,
                    deadline: Instant::now() + Duration::from_secs(HEALTH_TIMEOUT_SECS),
                    nats_client: nats_client.clone(),
                    is_health_check: true,
                },
            );

            if let Some(sender) = conn_inner.get_sender(&pid) {
                let _ = sender.send(ServerMessage::HealthCheck { request_id });
            }
        }
    });

    connections.add_subscription(plugin_id, handle).await;
}

// ---------------------------------------------------------------------------
// Pending response handling
// ---------------------------------------------------------------------------

async fn handle_pending_response(
    request_id: &str,
    response: &seidrum_common::events::ToolCallResponse,
    connections: &ConnectionManager,
) {
    if let Some(pending) = connections.take_pending(request_id) {
        if let Ok(bytes) = serde_json::to_vec(response) {
            let _ = pending
                .nats_client
                .publish(pending.reply_subject, bytes.into())
                .await;
        }
    } else {
        warn!(%request_id, "Received response for unknown/expired request");
    }
}

async fn handle_pending_health_response(
    request_id: &str,
    response: &seidrum_common::events::PluginHealthResponse,
    connections: &ConnectionManager,
) {
    if let Some(pending) = connections.take_pending(request_id) {
        if let Ok(bytes) = serde_json::to_vec(response) {
            let _ = pending
                .nats_client
                .publish(pending.reply_subject, bytes.into())
                .await;
        }
    } else {
        warn!(%request_id, "Received health response for unknown/expired request");
    }
}

// ---------------------------------------------------------------------------
// Event subscriptions + publish
// ---------------------------------------------------------------------------

async fn handle_subscribe(
    plugin_id: &str,
    subjects: &[String],
    nats: &NatsClient,
    connections: &ConnectionManager,
) {
    for subject in subjects {
        let nats_client = nats.inner().clone();
        let conn_inner = connections.clone();
        let pid = plugin_id.to_string();
        let subj = subject.clone();

        let handle = tokio::spawn(async move {
            let mut sub = match nats_client.subscribe(subj.clone()).await {
                Ok(s) => s,
                Err(err) => {
                    error!(%err, %subj, "Failed to subscribe");
                    return;
                }
            };

            while let Some(msg) = sub.next().await {
                let payload: serde_json::Value =
                    serde_json::from_slice(&msg.payload).unwrap_or(serde_json::json!(null));

                if let Some(sender) = conn_inner.get_sender(&pid) {
                    let _ = sender.send(ServerMessage::Event {
                        subject: msg.subject.to_string(),
                        payload,
                    });
                    conn_inner.increment_events(&pid);
                } else {
                    break;
                }
            }
        });

        connections.add_subscription(plugin_id, handle).await;
    }
}

async fn handle_publish(
    plugin_id: &str,
    subject: &str,
    payload: &serde_json::Value,
    nats: &NatsClient,
) {
    if let Err(e) = nats.publish_envelope(subject, None, None, payload).await {
        warn!(error = %e, %plugin_id, %subject, "Failed to publish event");
    }
}

// ---------------------------------------------------------------------------
// Storage operations
// ---------------------------------------------------------------------------

async fn handle_storage_get(
    plugin_id: &str,
    request_id: &str,
    namespace: &Option<String>,
    key: &str,
    nats: &NatsClient,
    tx: &mpsc::UnboundedSender<ServerMessage>,
) {
    let req = StorageGetRequest {
        plugin_id: plugin_id.to_string(),
        namespace: namespace.as_deref().unwrap_or("default").to_string(),
        key: key.to_string(),
    };

    let result = match tokio::time::timeout(
        Duration::from_secs(5),
        nats.request::<_, StorageGetResponse>("storage.get", &req),
    )
    .await
    {
        Ok(Ok(resp)) => serde_json::to_value(resp).unwrap_or(serde_json::json!(null)),
        Ok(Err(e)) => serde_json::json!({"error": e.to_string()}),
        Err(_) => serde_json::json!({"error": "Storage request timed out"}),
    };

    let _ = tx.send(ServerMessage::StorageResult {
        request_id: request_id.to_string(),
        result,
    });
}

async fn handle_storage_set(
    plugin_id: &str,
    request_id: &str,
    namespace: &Option<String>,
    key: &str,
    value: &serde_json::Value,
    nats: &NatsClient,
    tx: &mpsc::UnboundedSender<ServerMessage>,
) {
    let req = StorageSetRequest {
        plugin_id: plugin_id.to_string(),
        namespace: namespace.as_deref().unwrap_or("default").to_string(),
        key: key.to_string(),
        value: value.clone(),
    };

    let result = match tokio::time::timeout(
        Duration::from_secs(5),
        nats.request::<_, StorageSetResponse>("storage.set", &req),
    )
    .await
    {
        Ok(Ok(resp)) => serde_json::to_value(resp).unwrap_or(serde_json::json!(null)),
        Ok(Err(e)) => serde_json::json!({"error": e.to_string()}),
        Err(_) => serde_json::json!({"error": "Storage request timed out"}),
    };

    let _ = tx.send(ServerMessage::StorageResult {
        request_id: request_id.to_string(),
        result,
    });
}

async fn handle_storage_delete(
    plugin_id: &str,
    request_id: &str,
    namespace: &Option<String>,
    key: &str,
    nats: &NatsClient,
    tx: &mpsc::UnboundedSender<ServerMessage>,
) {
    let req = StorageDeleteRequest {
        plugin_id: plugin_id.to_string(),
        namespace: namespace.as_deref().unwrap_or("default").to_string(),
        key: key.to_string(),
    };

    let result = match tokio::time::timeout(
        Duration::from_secs(5),
        nats.request::<_, StorageDeleteResponse>("storage.delete", &req),
    )
    .await
    {
        Ok(Ok(resp)) => serde_json::to_value(resp).unwrap_or(serde_json::json!(null)),
        Ok(Err(e)) => serde_json::json!({"error": e.to_string()}),
        Err(_) => serde_json::json!({"error": "Storage request timed out"}),
    };

    let _ = tx.send(ServerMessage::StorageResult {
        request_id: request_id.to_string(),
        result,
    });
}

async fn handle_storage_list(
    plugin_id: &str,
    request_id: &str,
    namespace: &Option<String>,
    nats: &NatsClient,
    tx: &mpsc::UnboundedSender<ServerMessage>,
) {
    let req = StorageListRequest {
        plugin_id: plugin_id.to_string(),
        namespace: namespace.as_deref().unwrap_or("default").to_string(),
    };

    let result = match tokio::time::timeout(
        Duration::from_secs(5),
        nats.request::<_, StorageListResponse>("storage.list", &req),
    )
    .await
    {
        Ok(Ok(resp)) => serde_json::to_value(resp).unwrap_or(serde_json::json!(null)),
        Ok(Err(e)) => serde_json::json!({"error": e.to_string()}),
        Err(_) => serde_json::json!({"error": "Storage request timed out"}),
    };

    let _ = tx.send(ServerMessage::StorageResult {
        request_id: request_id.to_string(),
        result,
    });
}
