mod auth;
mod connections;
mod protocol;
mod rest;
mod ws;

use std::time::Instant;

use anyhow::{Context, Result};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::Router;
use clap::Parser;
use seidrum_common::events::PluginRegister;
use seidrum_common::nats_utils::NatsClient;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

use connections::ConnectionManager;

const PLUGIN_ID: &str = "api-gateway";
const PLUGIN_NAME: &str = "API Gateway";
const PLUGIN_VERSION: &str = "0.1.0";

#[derive(Parser, Debug)]
#[command(name = "seidrum-api-gateway")]
#[command(about = "HTTP/WebSocket API gateway for external Seidrum plugins")]
struct Cli {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://127.0.0.1:4222")]
    nats_url: String,

    /// Listen address for the HTTP/WebSocket server
    #[arg(long, env = "GATEWAY_LISTEN_ADDR", default_value = "0.0.0.0:8080")]
    listen_addr: String,

    /// API key for authentication (required)
    #[arg(long, env = "GATEWAY_API_KEY")]
    api_key: String,
}

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub nats: NatsClient,
    pub connections: ConnectionManager,
    pub api_key: String,
    pub start_time: Instant,
}

/// Query parameters for WebSocket upgrade.
#[derive(serde::Deserialize)]
struct WsQuery {
    api_key: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    info!(plugin = PLUGIN_ID, "Starting API gateway");

    // Connect to NATS
    let nats = NatsClient::connect(&cli.nats_url, PLUGIN_ID).await?;
    info!(url = %cli.nats_url, "Connected to NATS");

    // Register self as a plugin
    let register = PluginRegister {
        id: PLUGIN_ID.to_string(),
        name: PLUGIN_NAME.to_string(),
        version: PLUGIN_VERSION.to_string(),
        description: "HTTP/WebSocket gateway for external plugins".to_string(),
        consumes: vec![],
        produces: vec![],
        health_subject: format!("plugin.{}.health", PLUGIN_ID),
        consumed_event_types: vec![],
        produced_event_types: vec![],
    };

    nats.publish_envelope("plugin.register", None, None, &register)
        .await?;
    info!("Plugin registered");

    // Build shared state
    let state = AppState {
        nats: nats.clone(),
        connections: ConnectionManager::new(),
        api_key: cli.api_key,
        start_time: Instant::now(),
    };

    // Spawn pending request reaper (every 5 seconds)
    let reaper_connections = state.connections.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            reaper_connections.reap_expired().await;
        }
    });

    // Build axum router
    // Authenticated REST routes
    let api_routes = Router::new()
        .route("/plugins", get(rest::list_plugins))
        .route("/plugins/{id}", delete(rest::deregister_plugin))
        .route("/capabilities/{id}/call", post(rest::call_capability))
        .route("/capabilities", get(rest::search_capabilities))
        .route("/storage/get", post(rest::storage_get))
        .route("/storage/set", post(rest::storage_set))
        .route("/storage/delete", post(rest::storage_delete))
        .route("/storage/list", post(rest::storage_list))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            bearer_auth_middleware,
        ));

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/v1/health", get(rest::health))
        .nest("/api/v1", api_routes)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cli.listen_addr)
        .await
        .with_context(|| format!("Failed to bind to {}", cli.listen_addr))?;

    info!(addr = %cli.listen_addr, "API gateway listening");

    axum::serve(listener, app).await?;

    Ok(())
}

/// WebSocket upgrade handler with API key authentication.
async fn ws_handler(
    State(state): State<AppState>,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let api_key = query.api_key.unwrap_or_default();

    if !auth::validate_api_key(&api_key, &state.api_key) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let nats = state.nats.clone();
    let connections = state.connections.clone();

    ws.on_upgrade(move |socket| ws::handle_ws(socket, nats, connections))
        .into_response()
}

/// Middleware that checks the Authorization: Bearer header against the API key.
async fn bearer_auth_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> impl IntoResponse {
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let token = auth_header.strip_prefix("Bearer ").unwrap_or("");

    if !auth::validate_api_key(token, &state.api_key) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    next.run(req).await.into_response()
}
