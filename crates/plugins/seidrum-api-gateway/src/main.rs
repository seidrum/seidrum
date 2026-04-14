mod audit;
mod auth;
mod connections;
mod dashboard;
mod event_stream;
mod jwt;
mod protocol;
mod rate_limiter;
mod rest;
mod ws;

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, Request, State};
use axum::http::HeaderValue;
use axum::http::{header, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::Router;
use clap::Parser;
use rust_embed::Embed;
use seidrum_common::bus_client::BusClient;
use seidrum_common::events::PluginRegister;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use audit::AuditLog;
use auth::AuthHandler;
use connections::ConnectionManager;
use rate_limiter::{RateLimitConfig, RateLimiter};

const PLUGIN_ID: &str = "api-gateway";
const PLUGIN_NAME: &str = "API Gateway";
const PLUGIN_VERSION: &str = "0.1.0";

#[derive(Parser, Debug)]
#[command(name = "seidrum-api-gateway")]
#[command(about = "HTTP/WebSocket API gateway for external Seidrum plugins")]
struct Cli {
    /// Bus server URL
    #[arg(long, env = "BUS_URL", default_value = "ws://127.0.0.1:9000")]
    bus_url: String,

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
    pub nats: BusClient,
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
    let nats = BusClient::connect(&cli.bus_url, PLUGIN_ID).await?;
    info!(url = %cli.bus_url, "Connected to NATS");

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

    let rate_limiter = RateLimiter::new(rate_limit_config);

    // Restore rate limiter state from plugin storage (persistence across restarts)
    rate_limiter.restore_state(&nats).await;

    // Restore JWT revocation list from plugin storage (persistence across restarts)
    if let Some(jwt_service) = &auth_handler.jwt_service {
        jwt_service.restore_revocations(&nats).await;
    }

    let state = AppState {
        nats: nats.clone(),
        connections: ConnectionManager::new(),
        api_key: cli.api_key,
        auth_handler,
        rate_limiter,
        audit_log: AuditLog::with_nats(1000, nats.clone()),
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

    // Spawn rate limiter cleanup and state persistence (every 5 minutes)
    let cleanup_limiter = state.rate_limiter.clone();
    let cleanup_nats = nats.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            cleanup_limiter.cleanup_stale_entries(600).await;
            cleanup_limiter.save_state(&cleanup_nats).await;
        }
    });

    // Spawn JWT revocation list cleanup and persistence (every 15 minutes)
    if let Some(jwt_service) = state.auth_handler.jwt_service.clone() {
        let jwt_nats = nats.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(900));
            loop {
                interval.tick().await;
                jwt_service.cleanup_revocations(10_000).await;
                jwt_service.save_revocations(&jwt_nats).await;
            }
        });
    }

    // Build axum router
    // Authenticated REST + dashboard API routes
    let api_routes = Router::new()
        .route("/auth/token", post(auth_token_handler))
        .route("/auth/register", post(register_handler))
        .route("/auth/revoke", post(revoke_token_handler))
        .route("/audit", get(get_audit_log))
        // User management
        .route("/users/me", get(get_my_profile).put(update_my_profile))
        .route("/users", get(list_users_handler))
        .route(
            "/users/{id}",
            get(get_user_handler).delete(delete_user_handler),
        )
        .route("/users/{id}/role", put(update_user_role_handler))
        // User API keys
        .route(
            "/apikeys",
            post(create_apikey_handler).get(list_apikeys_handler),
        )
        .route("/apikeys/{id}", delete(revoke_apikey_handler))
        // Plugins
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
        .layer(build_cors_layer())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cli.listen_addr)
        .await
        .with_context(|| format!("Failed to bind to {}", cli.listen_addr))?;

    info!(addr = %cli.listen_addr, "API gateway listening");

    axum::serve(listener, app).await?;

    Ok(())
}

/// Build a CORS layer from the `CORS_ALLOWED_ORIGINS` env var (comma-separated).
/// Defaults to `http://localhost:3000` if not set.
fn build_cors_layer() -> CorsLayer {
    let origins_str = std::env::var("CORS_ALLOWED_ORIGINS")
        .unwrap_or_else(|_| "http://localhost:3000".to_string());

    let parsed: Vec<HeaderValue> = origins_str
        .split(',')
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                s.parse::<HeaderValue>().ok()
            }
        })
        .collect();

    if parsed.is_empty() {
        // Fallback — misconfigured env var
        warn!("CORS_ALLOWED_ORIGINS produced no valid origins; defaulting to localhost:3000");
        CorsLayer::new()
            .allow_origin(
                "http://localhost:3000"
                    .parse::<HeaderValue>()
                    .expect("valid origin"),
            )
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::PUT,
                axum::http::Method::DELETE,
                axum::http::Method::OPTIONS,
            ])
            .allow_headers([
                axum::http::header::AUTHORIZATION,
                axum::http::header::CONTENT_TYPE,
            ])
    } else {
        CorsLayer::new()
            .allow_origin(parsed)
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::PUT,
                axum::http::Method::DELETE,
                axum::http::Method::OPTIONS,
            ])
            .allow_headers([
                axum::http::header::AUTHORIZATION,
                axum::http::header::CONTENT_TYPE,
            ])
    }
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
    let auth_result = state.auth_handler.authenticate(&auth_header, None).await;

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

    // Extract API key from query parameters if present (deprecated — prefer Authorization header)
    let api_key_param = req.uri().query().and_then(|q| {
        q.split('&')
            .find(|p| p.starts_with("api_key="))
            .and_then(|p| p.strip_prefix("api_key="))
            .map(|k| k.to_string())
    });

    if api_key_param.is_some() {
        warn!(
            path = %path,
            "API key passed via query parameter — this is deprecated and will be removed. Use the Authorization header instead."
        );
    }

    // Authenticate
    let auth_result = state
        .auth_handler
        .authenticate(&auth_header, api_key_param)
        .await;

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
        remaining
            .to_string()
            .parse()
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("0")),
    );

    response
}

/// WebSocket handler for real-time event streaming with optional filtering.
/// Requires authentication via api_key query parameter.
async fn ws_events_handler(
    State(state): State<AppState>,
    Query(query): Query<event_stream::EventStreamQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Authenticate the request via api_key query parameter or Authorization header
    let api_key = query.api_key.clone();
    let auth_header = format!("ApiKey {}", api_key.unwrap_or_default());
    let auth_result = state.auth_handler.authenticate(&auth_header, None).await;

    if auth_result.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let nats = state.nats.clone();
    ws.on_upgrade(move |socket| {
        event_stream::handle_event_stream(socket, nats, query.filter, query.correlation_id)
    })
    .into_response()
}

/// Handle JWT token generation from API key or username/password.
/// POST /api/v1/auth/token
async fn auth_token_handler(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<auth::TokenRequest>,
) -> impl IntoResponse {
    let jwt_service = match &state.auth_handler.jwt_service {
        Some(svc) => svc,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "JWT service not configured"})),
            )
                .into_response()
        }
    };

    // Determine auth method: username/password or API key
    if let (Some(username), Some(password)) = (&req.username, &req.password) {
        // Username/password authentication — look up user in brain
        let user_get_req = seidrum_common::events::UserGetRequest {
            user_id: None,
            username: Some(username.clone()),
        };

        let user = match tokio::time::timeout(
            Duration::from_secs(5),
            state
                .nats
                .request::<_, seidrum_common::events::UserGetResponse>(
                    "brain.user.get",
                    &user_get_req,
                ),
        )
        .await
        {
            Ok(Ok(resp)) if resp.found => resp.user.unwrap(),
            _ => {
                state
                    .audit_log
                    .log(
                        audit::AuditEntryBuilder::new("auth.login_failed", username, "auth_token")
                            .method("POST")
                            .path("/api/v1/auth/token")
                            .status(401)
                            .details("user not found")
                            .build(),
                    )
                    .await;
                return (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({"error": "invalid credentials"})),
                )
                    .into_response();
            }
        };

        if user.status != "active" {
            return (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({"error": "account is not active"})),
            )
                .into_response();
        }

        if !auth::verify_password(password, &user.password_hash) {
            state
                .audit_log
                .log(
                    audit::AuditEntryBuilder::new("auth.login_failed", username, "auth_token")
                        .method("POST")
                        .path("/api/v1/auth/token")
                        .status(401)
                        .details("invalid password")
                        .user_id(Some(user.user_id.clone()))
                        .build(),
                )
                .await;
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({"error": "invalid credentials"})),
            )
                .into_response();
        }

        // Generate token with user's role and scopes
        match jwt_service.generate_token(
            username,
            &user.role,
            user.scopes.clone(),
            Some(user.user_id.clone()),
        ) {
            Ok(token) => {
                state
                    .audit_log
                    .log(
                        audit::AuditEntryBuilder::new("auth.login", username, "auth_token")
                            .method("POST")
                            .path("/api/v1/auth/token")
                            .status(200)
                            .user_id(Some(user.user_id.clone()))
                            .build(),
                    )
                    .await;

                (
                    StatusCode::OK,
                    axum::Json(auth::TokenResponse {
                        token,
                        expires_in: jwt_service.token_ttl_secs,
                        user_id: Some(user.user_id),
                    }),
                )
                    .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": format!("token generation failed: {}", e)})),
            )
                .into_response(),
        }
    } else if let Some(api_key) = &req.api_key {
        // API key authentication
        if !auth::validate_api_key(api_key, &state.api_key) {
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
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({"error": "invalid api_key"})),
            )
                .into_response();
        }

        let subject = req.subject.unwrap_or_else(|| "api-user".to_string());
        let role = req.role.unwrap_or_else(|| "admin".to_string());
        let scopes = req.scopes.unwrap_or_else(|| vec!["scope_root".to_string()]);

        match jwt_service.generate_token(&subject, &role, scopes, None) {
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

                (
                    StatusCode::OK,
                    axum::Json(auth::TokenResponse {
                        token,
                        expires_in: jwt_service.token_ttl_secs,
                        user_id: None,
                    }),
                )
                    .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": format!("token generation failed: {}", e)})),
            )
                .into_response(),
        }
    } else {
        (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "provide either api_key or username+password"})),
        )
            .into_response()
    }
}

/// Handle user registration.
/// POST /api/v1/auth/register
async fn register_handler(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<auth::RegisterRequest>,
) -> impl IntoResponse {
    // Validate password strength
    if req.password.len() < 12 {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "password must be at least 12 characters"})),
        )
            .into_response();
    }

    // Hash the password
    let password_hash = match auth::hash_password(&req.password) {
        Ok(h) => h,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": format!("password hashing failed: {}", e)})),
            )
                .into_response()
        }
    };

    let create_req = seidrum_common::events::UserCreateRequest {
        username: req.username.clone(),
        password_hash,
        email: req.email.clone(),
        display_name: req.display_name.clone(),
        role: "user".to_string(), // New registrations are always regular users
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, seidrum_common::events::UserCreateResponse>(
                "brain.user.create",
                &create_req,
            ),
    )
    .await
    {
        Ok(Ok(resp)) if resp.created => {
            state
                .audit_log
                .log(
                    audit::AuditEntryBuilder::new(
                        "user.registered",
                        &req.username,
                        "auth_register",
                    )
                    .method("POST")
                    .path("/api/v1/auth/register")
                    .status(201)
                    .user_id(Some(resp.user_id.clone()))
                    .build(),
                )
                .await;

            (
                StatusCode::CREATED,
                axum::Json(auth::RegisterResponse {
                    user_id: resp.user_id,
                    username: resp.username,
                    role: resp.role,
                }),
            )
                .into_response()
        }
        Ok(Ok(resp)) => {
            let error = resp
                .error
                .unwrap_or_else(|| "registration failed".to_string());
            (
                StatusCode::CONFLICT,
                axum::Json(serde_json::json!({"error": error})),
            )
                .into_response()
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": format!("brain request failed: {}", e)})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            axum::Json(serde_json::json!({"error": "request timed out"})),
        )
            .into_response(),
    }
}

/// Handle JWT token revocation.
/// POST /api/v1/auth/revoke
async fn revoke_token_handler(
    State(state): State<AppState>,
    request: Request,
) -> impl IntoResponse {
    let auth_result = request.extensions().get::<auth::AuthResult>().cloned();

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 16).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "invalid body"})),
            )
                .into_response()
        }
    };

    let req: auth::RevokeTokenRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "invalid request body"})),
            )
                .into_response()
        }
    };

    if let Some(jwt_service) = &state.auth_handler.jwt_service {
        // Validate the token to get the JTI
        match jwt_service.validate_token(&req.token).await {
            Ok(claims) => {
                // Users can only revoke their own tokens, admins can revoke any
                if let Some(auth) = &auth_result {
                    if !auth.is_admin() && auth.user_id != claims.user_id {
                        return (
                            StatusCode::FORBIDDEN,
                            axum::Json(
                                serde_json::json!({"error": "cannot revoke another user's token"}),
                            ),
                        )
                            .into_response();
                    }
                }

                jwt_service.revoke_token(&claims.jti).await;
                state
                    .audit_log
                    .log(
                        audit::AuditEntryBuilder::new(
                            "auth.token_revoked",
                            &claims.sub,
                            "auth_revoke",
                        )
                        .method("POST")
                        .path("/api/v1/auth/revoke")
                        .status(200)
                        .user_id(claims.user_id)
                        .build(),
                    )
                    .await;

                (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({"revoked": true})),
                )
                    .into_response()
            }
            Err(e) => (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": format!("invalid token: {}", e)})),
            )
                .into_response(),
        }
    } else {
        (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "JWT service not configured"})),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------
// User management handlers
// ---------------------------------------------------------------------------

/// Get current user's profile.
/// GET /api/v1/users/me
async fn get_my_profile(State(state): State<AppState>, request: Request) -> impl IntoResponse {
    let auth = match request.extensions().get::<auth::AuthResult>() {
        Some(a) => a.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let user_id = match &auth.user_id {
        Some(id) => id.clone(),
        None => {
            // API key auth — return basic info
            return (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "subject": auth.subject,
                    "role": auth.role,
                    "scopes": auth.scopes,
                    "auth_type": "api_key"
                })),
            )
                .into_response();
        }
    };

    let get_req = seidrum_common::events::UserGetRequest {
        user_id: Some(user_id),
        username: None,
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, seidrum_common::events::UserGetResponse>("brain.user.get", &get_req),
    )
    .await
    {
        Ok(Ok(resp)) if resp.found => {
            let user = resp.user.unwrap();
            (
                StatusCode::OK,
                axum::Json(auth::UserProfileResponse {
                    user_id: user.user_id,
                    username: user.username,
                    email: user.email,
                    display_name: user.display_name,
                    role: user.role,
                    created_at: user.created_at.to_rfc3339(),
                }),
            )
                .into_response()
        }
        _ => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "user not found"})),
        )
            .into_response(),
    }
}

/// Update current user's profile.
/// PUT /api/v1/users/me
async fn update_my_profile(State(state): State<AppState>, request: Request) -> impl IntoResponse {
    let auth = match request.extensions().get::<auth::AuthResult>() {
        Some(a) => a.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let user_id = match &auth.user_id {
        Some(id) => id.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "API key users cannot update profile"})),
            )
                .into_response()
        }
    };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 16).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "invalid body"})),
            )
                .into_response()
        }
    };

    #[derive(serde::Deserialize)]
    struct ProfileUpdate {
        email: Option<String>,
        display_name: Option<String>,
        password: Option<String>,
    }

    let update: ProfileUpdate = match serde_json::from_slice(&body) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "invalid request body"})),
            )
                .into_response()
        }
    };

    let password_hash = if let Some(password) = &update.password {
        if password.len() < 12 {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "password must be at least 12 characters"})),
            )
                .into_response();
        }
        Some(auth::hash_password(password).unwrap_or_default())
    } else {
        None
    };

    let update_req = seidrum_common::events::UserUpdateRequest {
        user_id,
        role: None, // Users can't change their own role
        email: update.email,
        display_name: update.display_name,
        status: None,
        password_hash,
        scopes: None,
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, seidrum_common::events::UserUpdateResponse>(
                "brain.user.update",
                &update_req,
            ),
    )
    .await
    {
        Ok(Ok(resp)) if resp.success => (
            StatusCode::OK,
            axum::Json(serde_json::json!({"updated": true})),
        )
            .into_response(),
        Ok(Ok(resp)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(
                serde_json::json!({"error": resp.error.unwrap_or_else(|| "update failed".into())}),
            ),
        )
            .into_response(),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": "brain request failed"})),
        )
            .into_response(),
    }
}

/// List all users (admin only).
/// GET /api/v1/users
async fn list_users_handler(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    request: Request,
) -> impl IntoResponse {
    let auth = match request.extensions().get::<auth::AuthResult>() {
        Some(a) => a.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if !auth.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "admin access required"})),
        )
            .into_response();
    }

    let list_req = seidrum_common::events::UserListRequest {
        limit: params.get("limit").and_then(|l| l.parse().ok()),
        offset: params.get("offset").and_then(|o| o.parse().ok()),
        role_filter: params.get("role").cloned(),
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, seidrum_common::events::UserListResponse>("brain.user.list", &list_req),
    )
    .await
    {
        Ok(Ok(resp)) => {
            let users: Vec<auth::UserProfileResponse> = resp
                .users
                .into_iter()
                .map(|u| auth::UserProfileResponse {
                    user_id: u.user_id,
                    username: u.username,
                    email: u.email,
                    display_name: u.display_name,
                    role: u.role,
                    created_at: u.created_at.to_rfc3339(),
                })
                .collect();
            (StatusCode::OK, axum::Json(auth::UserListResponse { users })).into_response()
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": "failed to list users"})),
        )
            .into_response(),
    }
}

/// Get a specific user (admin only).
/// GET /api/v1/users/{id}
async fn get_user_handler(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    request: Request,
) -> impl IntoResponse {
    let auth = match request.extensions().get::<auth::AuthResult>() {
        Some(a) => a.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if !auth.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "admin access required"})),
        )
            .into_response();
    }

    let get_req = seidrum_common::events::UserGetRequest {
        user_id: Some(user_id),
        username: None,
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, seidrum_common::events::UserGetResponse>("brain.user.get", &get_req),
    )
    .await
    {
        Ok(Ok(resp)) if resp.found => {
            let user = resp.user.unwrap();
            (
                StatusCode::OK,
                axum::Json(auth::UserProfileResponse {
                    user_id: user.user_id,
                    username: user.username,
                    email: user.email,
                    display_name: user.display_name,
                    role: user.role,
                    created_at: user.created_at.to_rfc3339(),
                }),
            )
                .into_response()
        }
        _ => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "user not found"})),
        )
            .into_response(),
    }
}

/// Update a user's role (admin only).
/// PUT /api/v1/users/{id}/role
async fn update_user_role_handler(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    request: Request,
) -> impl IntoResponse {
    let auth = match request.extensions().get::<auth::AuthResult>() {
        Some(a) => a.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if !auth.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "admin access required"})),
        )
            .into_response();
    }

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 16).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "invalid body"})),
            )
                .into_response()
        }
    };

    let role_req: auth::UpdateUserRoleRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "invalid request body"})),
            )
                .into_response()
        }
    };

    if role_req.role != "admin" && role_req.role != "user" && role_req.role != "readonly" {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "role must be admin, user, or readonly"})),
        )
            .into_response();
    }

    let update_req = seidrum_common::events::UserUpdateRequest {
        user_id: user_id.clone(),
        role: Some(role_req.role),
        email: None,
        display_name: None,
        status: None,
        password_hash: None,
        scopes: None,
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, seidrum_common::events::UserUpdateResponse>(
                "brain.user.update",
                &update_req,
            ),
    )
    .await
    {
        Ok(Ok(resp)) if resp.success => {
            state
                .audit_log
                .log(
                    audit::AuditEntryBuilder::new("user.role_updated", &auth.subject, &user_id)
                        .method("PUT")
                        .path(&format!("/api/v1/users/{}/role", user_id))
                        .status(200)
                        .user_id(auth.user_id)
                        .build(),
                )
                .await;

            (
                StatusCode::OK,
                axum::Json(serde_json::json!({"updated": true})),
            )
                .into_response()
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": "update failed"})),
        )
            .into_response(),
    }
}

/// Delete a user (admin only, soft delete).
/// DELETE /api/v1/users/{id}
async fn delete_user_handler(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    request: Request,
) -> impl IntoResponse {
    let auth = match request.extensions().get::<auth::AuthResult>() {
        Some(a) => a.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if !auth.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "admin access required"})),
        )
            .into_response();
    }

    let delete_req = seidrum_common::events::UserDeleteRequest {
        user_id: user_id.clone(),
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, seidrum_common::events::UserDeleteResponse>(
                "brain.user.delete",
                &delete_req,
            ),
    )
    .await
    {
        Ok(Ok(resp)) if resp.success => {
            state
                .audit_log
                .log(
                    audit::AuditEntryBuilder::new("user.deleted", &auth.subject, &user_id)
                        .method("DELETE")
                        .path(&format!("/api/v1/users/{}", user_id))
                        .status(200)
                        .user_id(auth.user_id)
                        .build(),
                )
                .await;

            (
                StatusCode::OK,
                axum::Json(serde_json::json!({"deleted": true})),
            )
                .into_response()
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": "delete failed"})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// API key management handlers
// ---------------------------------------------------------------------------

/// Create a new user-scoped API key.
/// POST /api/v1/apikeys
async fn create_apikey_handler(
    State(state): State<AppState>,
    request: Request,
) -> impl IntoResponse {
    let auth = match request.extensions().get::<auth::AuthResult>() {
        Some(a) => a.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let user_id = match &auth.user_id {
        Some(id) => id.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "only user accounts can create API keys"})),
            )
                .into_response()
        }
    };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 16).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "invalid body"})),
            )
                .into_response()
        }
    };

    #[derive(serde::Deserialize)]
    struct CreateApiKeyBody {
        name: String,
        scopes: Option<Vec<String>>,
        expires_in_days: Option<u32>,
    }

    let req: CreateApiKeyBody = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "invalid request body, expected: {name, scopes?, expires_in_days?}"})),
            )
                .into_response()
        }
    };

    // Generate a random API key
    let raw_key = format!(
        "sk_{}_{}",
        &user_id[5..], // strip "user_" prefix
        ulid::Ulid::new().to_string().to_lowercase()
    );

    // Hash the key for storage
    use sha2::{Digest, Sha256};
    let key_hash = format!("{:x}", Sha256::digest(raw_key.as_bytes()));

    let expires_at = req
        .expires_in_days
        .map(|days| chrono::Utc::now() + chrono::Duration::days(days as i64));

    let scopes = req.scopes.unwrap_or_else(|| auth.scopes.clone());

    let create_req = seidrum_common::events::ApiKeyCreateRequest {
        user_id: user_id.clone(),
        name: req.name.clone(),
        key_hash,
        scopes,
        expires_at,
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, seidrum_common::events::ApiKeyCreateResponse>(
                "brain.apikey.create",
                &create_req,
            ),
    )
    .await
    {
        Ok(Ok(resp)) if resp.created => {
            state
                .audit_log
                .log(
                    audit::AuditEntryBuilder::new("apikey.created", &auth.subject, &resp.key_id)
                        .method("POST")
                        .path("/api/v1/apikeys")
                        .status(201)
                        .user_id(Some(user_id))
                        .build(),
                )
                .await;

            // Return the raw key only once — it cannot be retrieved later
            (
                StatusCode::CREATED,
                axum::Json(serde_json::json!({
                    "key_id": resp.key_id,
                    "key": raw_key,
                    "name": req.name,
                    "expires_at": expires_at,
                    "warning": "Save this key now — it cannot be retrieved later"
                })),
            )
                .into_response()
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": "failed to create API key"})),
        )
            .into_response(),
    }
}

/// List current user's API keys.
/// GET /api/v1/apikeys
async fn list_apikeys_handler(
    State(state): State<AppState>,
    request: Request,
) -> impl IntoResponse {
    let auth = match request.extensions().get::<auth::AuthResult>() {
        Some(a) => a.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let user_id = match &auth.user_id {
        Some(id) => id.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "only user accounts have API keys"})),
            )
                .into_response()
        }
    };

    let list_req = seidrum_common::events::ApiKeyListRequest { user_id };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, seidrum_common::events::ApiKeyListResponse>(
                "brain.apikey.list",
                &list_req,
            ),
    )
    .await
    {
        Ok(Ok(resp)) => {
            // Strip key_hash from response
            let keys: Vec<serde_json::Value> = resp
                .keys
                .into_iter()
                .map(|k| {
                    serde_json::json!({
                        "key_id": k.key_id,
                        "name": k.name,
                        "scopes": k.scopes,
                        "created_at": k.created_at,
                        "expires_at": k.expires_at,
                        "last_used": k.last_used,
                        "revoked": k.revoked,
                    })
                })
                .collect();
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({"keys": keys})),
            )
                .into_response()
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": "failed to list API keys"})),
        )
            .into_response(),
    }
}

/// Revoke a user API key.
/// DELETE /api/v1/apikeys/{id}
async fn revoke_apikey_handler(
    State(state): State<AppState>,
    Path(key_id): Path<String>,
    request: Request,
) -> impl IntoResponse {
    let auth = match request.extensions().get::<auth::AuthResult>() {
        Some(a) => a.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    let user_id = match &auth.user_id {
        Some(id) => id.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "only user accounts have API keys"})),
            )
                .into_response()
        }
    };

    let revoke_req = seidrum_common::events::ApiKeyRevokeRequest {
        key_id: key_id.clone(),
        user_id: user_id.clone(),
    };

    match tokio::time::timeout(
        Duration::from_secs(5),
        state
            .nats
            .request::<_, seidrum_common::events::ApiKeyRevokeResponse>(
                "brain.apikey.revoke",
                &revoke_req,
            ),
    )
    .await
    {
        Ok(Ok(resp)) if resp.success => {
            state
                .audit_log
                .log(
                    audit::AuditEntryBuilder::new("apikey.revoked", &auth.subject, &key_id)
                        .method("DELETE")
                        .path(&format!("/api/v1/apikeys/{}", key_id))
                        .status(200)
                        .user_id(Some(user_id))
                        .build(),
                )
                .await;

            (
                StatusCode::OK,
                axum::Json(serde_json::json!({"revoked": true})),
            )
                .into_response()
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": "failed to revoke API key"})),
        )
            .into_response(),
    }
}

/// Get recent audit log entries (admin only).
/// GET /api/v1/audit?limit=50&since=<iso8601>
async fn get_audit_log(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    request: Request,
) -> impl IntoResponse {
    let auth = match request.extensions().get::<auth::AuthResult>() {
        Some(a) => a.clone(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if !auth.is_admin() {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "admin access required"})),
        )
            .into_response();
    }

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
    let req = event_stream::trace_collector::TraceGetRequest {
        correlation_id,
        auth_source: Some("api-gateway".to_string()),
    };
    match serde_json::to_vec(&req) {
        Ok(req_bytes) => match state.nats.request_bytes("trace.get", req_bytes).await {
            Ok(payload) => {
                match serde_json::from_slice::<Option<event_stream::trace_collector::Trace>>(
                    &payload,
                ) {
                    Ok(Some(trace)) => (StatusCode::OK, axum::Json(trace)).into_response(),
                    Ok(None) => (
                        StatusCode::NOT_FOUND,
                        axum::Json(serde_json::json!({"error": "trace not found"})),
                    )
                        .into_response(),
                    Err(e) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(
                            serde_json::json!({"error": format!("failed to parse trace: {}", e)}),
                        ),
                    )
                        .into_response(),
                }
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": format!("failed to query trace: {}", e)})),
            )
                .into_response(),
        },
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

    let req = event_stream::trace_collector::TraceListRequest {
        limit,
        since,
        auth_source: Some("api-gateway".to_string()),
    };
    match serde_json::to_vec(&req) {
        Ok(req_bytes) => match state.nats.request_bytes("trace.list", req_bytes).await {
            Ok(payload) => {
                match serde_json::from_slice::<event_stream::trace_collector::TraceListResponse>(
                    &payload,
                ) {
                    Ok(response) => (StatusCode::OK, axum::Json(response)).into_response(),
                    Err(e) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(
                            serde_json::json!({"error": format!("failed to parse traces: {}", e)}),
                        ),
                    )
                        .into_response(),
                }
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": format!("failed to query traces: {}", e)})),
            )
                .into_response(),
        },
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
