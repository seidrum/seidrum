mod auth;
mod audit;
mod connections;
mod dashboard;
mod event_stream;
mod jwt;
mod protocol;
mod rate_limiter;
mod rest;
mod ws;

use std::time::Instant;

use anyhow::{Context, Result};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::Router;
use clap::Parser;
use rust_embed::Embed;
use seidrum_common::events::PluginRegister;
use seidrum_common::nats_utils::NatsClient;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

use auth::AuthHandler;
use audit::AuditLog;
use connections::ConnectionManager;
use rate_limiter::{RateLimiter, RateLimitConfig};

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

    /// JWT secret for token signing
    #[arg(long, env = "GATEWAY_JWT_SECRET")]
    jwt_secret: Option<String>,

    /// JWT token TTL in seconds
    #[arg(long, env = "GATEWAY_JWT_TTL", default_value = "86400")]
    jwt_ttl: u64,

    /// Rate limit: requests per minute for regular users
    #[arg(long, env = "GATEWAY_RATE_LIMIT", default_value = "60")]
    rate_limit: u32,

    /// Rate limit: requests per minute for admin users
    #[arg(long, env = "GATEWAY_RATE_LIMIT_ADMIN", default_value = "300")]
    rate_limit_admin: u32,
}

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub nats: NatsClient,
    pub connections: ConnectionManager,
    pub api_key: String,
    pub auth_handler: AuthHandler,
    pub rate_limiter: RateLimiter,
    pub audit_log: AuditLog,
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
        config_schema: None,
    };

    nats.publish_envelope("plugin.register", None, None, &register)
        .await?;
    info!("Plugin registered");

    // Build shared state
    let auth_handler = AuthHandler::new(cli.api_key.clone(), cli.jwt_secret.clone(), cli.jwt_ttl);
    let rate_limit_config = RateLimitConfig {
        regular_rpm: cli.rate_limit,
        admin_rpm: cli.rate_limit_admin,
        cleanup_interval_secs: 300,
    };

    let state = AppState {
        nats: nats.clone(),
        connections: ConnectionManager::new(),
        api_key: cli.api_key,
        auth_handler,
        rate_limiter: RateLimiter::new(rate_limit_config),
        audit_log: AuditLog::new(1000),
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

    // Spawn rate limiter cleanup (every 5 minutes)
    let cleanup_limiter = state.rate_limiter.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            cleanup_limiter.cleanup_stale_entries(600).await;
        }
    });

    // Build axum router
    // Authenticated REST + dashboard API routes
    let api_routes = Router::new()
        .route("/auth/token", post(auth_token_handler))
        .route("/audit", get(get_audit_log))
        .route("/plugins", get(rest::list_plugins))
        .route("/plugins/{id}", delete(rest::deregister_plugin))
        .route("/capabilities/{id}/call", post(rest::call_capability))
        .route("/capabilities", get(rest::search_capabilities))
        .route("/storage/get", post(rest::storage_get))
        .route("/storage/set", post(rest::storage_set))
        .route("/storage/delete", post(rest::storage_delete))
        .route("/storage/list", post(rest::storage_list))
        // Trace endpoints
        .route("/traces", get(list_traces))
        .route("/traces/:correlation_id", get(get_trace))
        // Dashboard endpoints
        .route("/dashboard/overview", get(dashboard::overview))
        .route(
            "/dashboard/plugins/{id}/health",
            get(dashboard::plugin_health),
        )
        .route(
            "/dashboard/plugins/{id}/config",
            get(dashboard::get_plugin_config).put(dashboard::update_plugin_config),
        )
        .route(
            "/dashboard/plugins/{id}/config/schema",
            get(dashboard::get_config_schema),
        )
        .route("/dashboard/skills", get(dashboard::list_skills))
        .route("/dashboard/skills/{id}", get(dashboard::get_skill))
        .route(
            "/dashboard/conversations",
            get(dashboard::list_conversations_dashboard),
        )
        .route(
            "/dashboard/conversations/{id}",
            get(dashboard::get_conversation_dashboard),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_rate_limit_middleware,
        ));

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/ws/events", get(ws_events_handler))
        .route("/api/v1/health", get(rest::health))
        .nest("/api/v1", api_routes)
        // Dashboard static files (no auth — HTML/CSS/JS)
        .route("/dashboard/{*path}", get(serve_static))
        .route(
            "/dashboard",
            get(|| async { axum::response::Redirect::permanent("/dashboard/") }),
        )
        .route(
            "/",
            get(|| async { axum::response::Redirect::temporary("/dashboard/") }),
        )
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
    let api_key = query.api_key.clone();

    // Authenticate the request
    let auth_header = format!("ApiKey {}", api_key.unwrap_or_default());
    let auth_result = state.auth_handler.authenticate(&auth_header, None);

    if auth_result.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let nats = state.nats.clone();
    let connections = state.connections.clone();

    ws.on_upgrade(move |socket| ws::handle_ws(socket, nats, connections))
        .into_response()
}

/// Middleware that enforces authentication, rate limiting, and audit logging.
async fn auth_rate_limit_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> impl IntoResponse {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();

    // Extract authorization header
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Extract API key from query parameters if present
    let api_key_param = req
        .uri()
        .query()
        .and_then(|q| {
            q.split('&')
                .find(|p| p.starts_with("api_key="))
                .and_then(|p| p.strip_prefix("api_key="))
                .map(|k| k.to_string())
        });

    // Authenticate
    let auth_result = state.auth_handler.authenticate(&auth_header, api_key_param);

    let auth_result = match auth_result {
        Some(result) => result,
        None => {
            state
                .audit_log
                .log(
                    audit::AuditEntryBuilder::new("auth.failed", "unknown", &path)
                        .method(&method)
                        .path(&path)
                        .status(401)
                        .build(),
                )
                .await;
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    // Check rate limit
    let (allowed, remaining, retry_after) = state
        .rate_limiter
        .check_rate_limit(&auth_result.subject, auth_result.is_admin())
        .await;

    if !allowed {
        let retry_after_secs = retry_after.unwrap_or(1);
        state
            .audit_log
            .log(
                audit::AuditEntryBuilder::new("rate_limit.exceeded", &auth_result.subject, &path)
                    .method(&method)
                    .path(&path)
                    .status(429)
                    .details(&format!("retry_after: {}s", retry_after_secs))
                    .build(),
            )
            .await;

        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_str(&retry_after_secs.to_string()).unwrap(),
            )],
        )
            .into_response();
    }

    // Store auth result in extensions for handlers to use
    req.extensions_mut().insert(auth_result);

    // Add rate limit info to response headers (if applicable)
    let mut response = next.run(req).await;
    response.headers_mut().insert(
        "X-Rate-Limit-Remaining",
        remaining.to_string().parse().unwrap_or_else(|_| axum::http::HeaderValue::from_static("0")),
    );

    response
}

/// WebSocket handler for real-time event streaming with optional filtering.
async fn ws_events_handler(
    State(state): State<AppState>,
    Query(query): Query<event_stream::EventStreamQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let nats = state.nats.clone();
    ws.on_upgrade(move |socket| {
        event_stream::handle_event_stream(socket, nats, query.filter, query.correlation_id)
    })
}

/// Handle JWT token generation from API key.
/// POST /api/v1/auth/token
async fn auth_token_handler(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<auth::TokenRequest>,
) -> impl IntoResponse {
    // Validate the API key
    if !auth::validate_api_key(&req.api_key, &state.api_key) {
        state
            .audit_log
            .log(
                audit::AuditEntryBuilder::new("auth.token_failed", "unknown", "auth_token")
                    .method("POST")
                    .path("/api/v1/auth/token")
                    .status(401)
                    .build(),
            )
            .await;
        return (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({"error": "invalid api_key"})))
            .into_response();
    }

    // Generate JWT token
    if let Some(jwt_service) = &state.auth_handler.jwt_service {
        let subject = req.subject.unwrap_or_else(|| "api-user".to_string());
        let role = req.role.unwrap_or_else(|| "admin".to_string());
        let scopes = req.scopes.unwrap_or_else(|| vec!["scope_root".to_string()]);

        match jwt_service.generate_token(&subject, &role, scopes) {
            Ok(token) => {
                state
                    .audit_log
                    .log(
                        audit::AuditEntryBuilder::new("auth.token_issued", &subject, "auth_token")
                            .method("POST")
                            .path("/api/v1/auth/token")
                            .status(200)
                            .build(),
                    )
                    .await;

                let ttl = state.auth_handler.jwt_service.as_ref().unwrap().token_ttl_secs;
                return (
                    StatusCode::OK,
                    axum::Json(auth::TokenResponse {
                        token,
                        expires_in: ttl,
                    }),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"error": format!("token generation failed: {}", e)})),
                )
                    .into_response();
            }
        }
    } else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "JWT service not configured"})),
        )
            .into_response();
    }
}

/// Get recent audit log entries.
/// GET /api/v1/audit?limit=50&since=<iso8601>
async fn get_audit_log(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let limit = params
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(50);

    let since = params
        .get("since")
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));

    let entries = state.audit_log.query(limit, since).await;
    (StatusCode::OK, axum::Json(entries)).into_response()
}

/// Get a specific trace by correlation_id.
async fn get_trace(
    State(state): State<AppState>,
    Path(correlation_id): Path<String>,
) -> impl IntoResponse {
    let req = event_stream::trace_collector::TraceGetRequest { correlation_id };
    match serde_json::to_vec(&req) {
        Ok(req_bytes) => {
            match state
                .nats
                .inner()
                .request("trace.get".to_string(), req_bytes.into())
                .await
            {
                Ok(msg) => {
                    match serde_json::from_slice::<Option<event_stream::trace_collector::Trace>>(
                        &msg.payload,
                    ) {
                        Ok(Some(trace)) => (StatusCode::OK, axum::Json(trace)).into_response(),
                        Ok(None) => (
                            StatusCode::NOT_FOUND,
                            axum::Json(serde_json::json!({"error": "trace not found"})),
                        )
                            .into_response(),
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(serde_json::json!({"error": format!("failed to parse trace: {}", e)})),
                        )
                            .into_response(),
                    }
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"error": format!("failed to query trace: {}", e)})),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": format!("failed to serialize request: {}", e)})),
        )
            .into_response(),
    }
}

/// List recent traces with optional filtering.
async fn list_traces(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let limit = params
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .or(Some(50));
    let since = params
        .get("since")
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));

    let req = event_stream::trace_collector::TraceListRequest { limit, since };
    match serde_json::to_vec(&req) {
        Ok(req_bytes) => {
            match state
                .nats
                .inner()
                .request("trace.list".to_string(), req_bytes.into())
                .await
            {
                Ok(msg) => {
                    match serde_json::from_slice::<event_stream::trace_collector::TraceListResponse>(
                        &msg.payload,
                    ) {
                        Ok(response) => (StatusCode::OK, axum::Json(response)).into_response(),
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            axum::Json(serde_json::json!({"error": format!("failed to parse traces: {}", e)})),
                        )
                            .into_response(),
                    }
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"error": format!("failed to query traces: {}", e)})),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": format!("failed to serialize request: {}", e)})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Static file serving (dashboard frontend)
// ---------------------------------------------------------------------------

#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

/// Serve embedded static files for the dashboard.
async fn serve_static(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches("/dashboard/");
    let path = if path.is_empty() { "index.html" } else { path };

    match StaticAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref())],
                content.data.into_owned(),
            )
                .into_response()
        }
        None => {
            // SPA fallback: serve index.html for unknown paths
            match StaticAssets::get("index.html") {
                Some(content) => (
                    [(header::CONTENT_TYPE, "text/html")],
                    content.data.into_owned(),
                )
                    .into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
    }
}
