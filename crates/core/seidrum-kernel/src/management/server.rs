use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use axum::Router;
use tower_http::cors::{CorsLayer, AllowOrigin, AllowMethods, AllowHeaders};
use http::header;
use tracing::info;

use super::routes;
use super::state::ManagementState;

pub struct ManagementServer {
    state: ManagementState,
}

impl ManagementServer {
    pub fn new(
        nats: async_nats::Client,
        config_dir: PathBuf,
        agents_dir: PathBuf,
        workflows_dir: PathBuf,
        env_file: PathBuf,
    ) -> Self {
        Self {
            state: ManagementState::new(nats, config_dir, agents_dir, workflows_dir, env_file),
        }
    }

    pub async fn spawn(self, listen_addr: &str) -> Result<tokio::task::JoinHandle<()>> {
        let addr: SocketAddr = listen_addr
            .parse()
            .with_context(|| format!("Invalid management listen address: {}", listen_addr))?;

        let cors = CorsLayer::new()
            .allow_origin(
                [
                    "http://localhost:3030".parse().unwrap(),
                    "http://127.0.0.1:3030".parse().unwrap(),
                    "http://localhost:5173".parse().unwrap(),
                    "http://127.0.0.1:5173".parse().unwrap(),
                ]
                .into_iter()
                .collect::<AllowOrigin>()
            )
            .allow_methods(AllowMethods::list([
                http::Method::GET,
                http::Method::POST,
                http::Method::PUT,
                http::Method::DELETE,
                http::Method::OPTIONS,
            ]))
            .allow_headers(AllowHeaders::list([
                header::CONTENT_TYPE,
                header::AUTHORIZATION,
            ]));

        let app = routes::build_router(self.state).layer(cors);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("Failed to bind management server to {}", addr))?;

        info!(%addr, "Management API server listening");

        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "Management server error");
            }
        });

        Ok(handle)
    }
}
