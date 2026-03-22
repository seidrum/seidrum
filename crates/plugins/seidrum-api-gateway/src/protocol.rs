//! WebSocket protocol message types.
//!
//! All messages are JSON with a `"type"` discriminator field.

use seidrum_common::events::{PluginHealthResponse, ToolCallRequest, ToolCallResponse};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Client → Gateway
// ---------------------------------------------------------------------------

/// Messages sent by external plugins over WebSocket.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Register this connection as a plugin.
    Register { plugin: PluginInfo },
    /// Register a capability (tool/command).
    RegisterCapability { capability: serde_json::Value },
    /// Deregister and disconnect cleanly.
    Deregister {},
    /// Respond to a capability call from the kernel.
    CapabilityResponse {
        request_id: String,
        response: ToolCallResponse,
    },
    /// Respond to a health check from the kernel.
    HealthResponse {
        request_id: String,
        response: PluginHealthResponse,
    },
    /// Subscribe to NATS event subjects.
    Subscribe { subjects: Vec<String> },
    /// Unsubscribe from NATS event subjects.
    Unsubscribe { subjects: Vec<String> },
    /// Publish an event to a NATS subject.
    Publish {
        subject: String,
        payload: serde_json::Value,
    },
    /// Storage get.
    StorageGet {
        request_id: String,
        namespace: Option<String>,
        key: String,
    },
    /// Storage set.
    StorageSet {
        request_id: String,
        namespace: Option<String>,
        key: String,
        value: serde_json::Value,
    },
    /// Storage delete.
    StorageDelete {
        request_id: String,
        namespace: Option<String>,
        key: String,
    },
    /// Storage list keys.
    StorageList {
        request_id: String,
        namespace: Option<String>,
    },
}

/// Plugin identity provided during registration.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub consumes: Vec<String>,
    #[serde(default)]
    pub produces: Vec<String>,
}

// ---------------------------------------------------------------------------
// Gateway → Client
// ---------------------------------------------------------------------------

/// Messages sent by the gateway to external plugins over WebSocket.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Registration confirmed.
    Registered { plugin_id: String },
    /// Incoming capability call — the client must respond with `capability_response`.
    CapabilityCall {
        request_id: String,
        request: ToolCallRequest,
    },
    /// Health check from the kernel — the client must respond with `health_response`.
    HealthCheck { request_id: String },
    /// Event received on a subscribed subject.
    Event {
        subject: String,
        payload: serde_json::Value,
    },
    /// Result of a storage operation.
    StorageResult {
        request_id: String,
        result: serde_json::Value,
    },
    /// Error message.
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_register_roundtrip() {
        let msg = ClientMessage::Register {
            plugin: PluginInfo {
                id: "my-plugin".into(),
                name: "My Plugin".into(),
                version: "0.1.0".into(),
                description: "Does things".into(),
                consumes: vec![],
                produces: vec![],
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"register""#));
        let _: ClientMessage = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn server_capability_call_roundtrip() {
        let msg = ServerMessage::CapabilityCall {
            request_id: "req-1".into(),
            request: ToolCallRequest {
                tool_id: "my-tool".into(),
                plugin_id: "my-plugin".into(),
                arguments: serde_json::json!({"key": "value"}),
                correlation_id: None,
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"capability_call""#));
        let _: ServerMessage = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn server_error_roundtrip() {
        let msg = ServerMessage::Error {
            message: "something went wrong".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let de: ServerMessage = serde_json::from_str(&json).unwrap();
        match de {
            ServerMessage::Error { message } => assert_eq!(message, "something went wrong"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn client_storage_get_roundtrip() {
        let msg = ClientMessage::StorageGet {
            request_id: "r1".into(),
            namespace: Some("settings".into()),
            key: "theme".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"storage_get""#));
        let _: ClientMessage = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn client_deregister_shape() {
        let json = r#"{"type":"deregister"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        matches!(msg, ClientMessage::Deregister {});
    }
}
