mod auth;
mod config;
mod error;
mod proxy;
mod routes;
mod state;

use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path = std::env::args().nth(1).unwrap_or_else(|| "config.toml".into());

    let config = config::Config::load(&config_path).unwrap_or_else(|e| {
        eprintln!("failed to load config from '{config_path}': {e}");
        std::process::exit(1);
    });

    let listen_addr = config.listen.clone();

    let gpu_set_count = config.servers.len();
    let server_count: usize = config.servers.values().map(|v| v.len()).sum();

    let state = Arc::new(state::AppState::from_config(config));

    let discovery_state = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            discovery_state.discover_models().await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    let app = Router::new()
        .route("/v1/chat/completions", post(routes::proxy_model_request))
        .route("/v1/completions", post(routes::proxy_model_request))
        .route("/v1/embeddings", post(routes::proxy_model_request))
        .route("/v1/rerank", post(routes::proxy_model_request))
        .route("/v1/messages", post(routes::proxy_model_request))
        .route("/v1/messages/count_tokens", post(routes::proxy_model_request))
        .route("/v1/models", get(routes::list_models))
        // llama.cpp-only compatibility routes (e.g. for pi-llama-cpp); not part
        // of the OpenAI-style surface and unused by vLLM/sglang clients.
        .route("/health", get(routes::health))
        .route("/props", get(routes::get_props))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("failed to bind to {listen_addr}: {e}");
            std::process::exit(1);
        });

    tracing::info!(
        "listening on {listen_addr} with {gpu_set_count} GPU set(s), {server_count} backend(s)"
    );

    axum::serve(listener, app).await.unwrap();
}
