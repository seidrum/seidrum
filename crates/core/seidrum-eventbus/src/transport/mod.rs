//! Transport servers for remote event bus access.
//!
//! Provides WebSocket and HTTP transports that allow remote clients
//! to interact with the event bus.

pub mod http;
pub mod ws;

pub use http::{create_router, HttpServer};
pub use ws::WebSocketServer;
