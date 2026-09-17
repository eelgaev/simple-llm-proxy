mod auth;
mod config;
mod error;
mod model_scan;
mod proxy;
mod routes;
mod state;

use axum::Router;
use axum::routing::{get, post};
use std::sync::Arc;
use tower_http::trace::TraceLayer;

fn build_app(state: Arc<state::AppState>) -> Router {
    let protected = Router::new()
        .route("/v1/chat/completions", post(routes::proxy_model_request))
        .route("/v1/completions", post(routes::proxy_model_request))
        .route("/v1/embeddings", post(routes::proxy_model_request))
        .route("/v1/rerank", post(routes::proxy_model_request))
        .route("/v1/messages", post(routes::proxy_model_request))
        .route(
            "/v1/messages/count_tokens",
            post(routes::proxy_model_request),
        )
        .route("/v1/models", get(routes::list_models))
        .route("/health", get(routes::health))
        .route("/props", get(routes::get_props))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ));

    Router::new()
        .route("/register", post(routes::register))
        .merge(protected)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".into());

    let config = config::Config::load(&config_path).unwrap_or_else(|e| {
        eprintln!("failed to load config from '{config_path}': {e}");
        std::process::exit(1);
    });

    let listen_addr = config.listen.clone();

    let gpu_set_count = config.servers.len();
    let server_count: usize = config.servers.values().map(|v| v.len()).sum();

    let discovered_hosts_path = std::path::Path::new(&config_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("discovered_hosts.json");
    let state = Arc::new(state::AppState::from_config(config, discovered_hosts_path));

    routes::restore_discovered_hosts(&state).await;

    let discovery_state = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            discovery_state.discover_models().await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    let app = build_app(state);

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use std::collections::HashMap;

    async fn models(headers: HeaderMap) -> axum::response::Response {
        if headers.get("authorization").and_then(|v| v.to_str().ok())
            != Some("Bearer backend-secret")
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        axum::Json(serde_json::json!({
            "data": [{"id": "registered-model", "object": "model"}],
            "models": [{"model": "registered-model"}]
        }))
        .into_response()
    }

    async fn chat(
        headers: HeaderMap,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> axum::response::Response {
        if headers.get("authorization").and_then(|v| v.to_str().ok())
            != Some("Bearer backend-secret")
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if body["model"] != "registered-model" {
            return StatusCode::BAD_REQUEST.into_response();
        }
        axum::Json(serde_json::json!({"model": body["model"]})).into_response()
    }

    #[tokio::test]
    async fn register_falls_back_to_http_persists_and_adds_provider() {
        let backend = Router::new()
            .route(
                "/health",
                get(|headers: HeaderMap| async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok())
                        == Some("Bearer backend-secret")
                    {
                        axum::Json(serde_json::json!({"status": "ok"})).into_response()
                    } else {
                        StatusCode::UNAUTHORIZED.into_response()
                    }
                }),
            )
            .route("/models", get(models))
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(chat));
        let backend_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_address = backend_listener.local_addr().unwrap();
        let backend_task = tokio::spawn(async move {
            axum::serve(backend_listener, backend).await.unwrap();
        });

        let mut servers = HashMap::new();
        servers.insert(
            "configured".to_string(),
            vec![config::ServerEntry::WithToken {
                url: format!("http://{backend_address}"),
                token: "backend-secret".into(),
            }],
        );
        let config = config::Config {
            listen: "127.0.0.1:0".into(),
            api_tokens: vec!["proxy-secret".into()],
            servers,
        };
        let file = std::env::temp_dir().join(format!(
            "simple-llm-proxy-register-{}-{}.json",
            std::process::id(),
            backend_address.port()
        ));
        let state = Arc::new(state::AppState::from_config(config, file.clone()));
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy_listener.local_addr().unwrap();
        let proxy_task = tokio::spawn(async move {
            axum::serve(proxy_listener, build_app(state)).await.unwrap();
        });

        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{proxy_address}/register"))
            .json(&serde_json::json!({
                "host": "127.0.0.1",
                "port": backend_address.port(),
                "api_key": "backend-secret"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let response: serde_json::Value = response.json().await.unwrap();
        assert_eq!(response["url"], format!("http://{backend_address}"));

        let saved: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(&file).await.unwrap()).unwrap();
        assert_eq!(saved[0]["host"], "127.0.0.1");
        assert_eq!(saved[0]["api_key"], "backend-secret");

        let models = client
            .get(format!("http://{proxy_address}/v1/models"))
            .bearer_auth("proxy-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(models.status(), StatusCode::OK);
        let models: serde_json::Value = models.json().await.unwrap();
        assert!(
            models["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "127.0.0.1/registered-model")
        );
        assert!(
            models["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["model"] == "127.0.0.1/registered-model")
        );

        let completion = client
            .post(format!("http://{proxy_address}/v1/chat/completions"))
            .bearer_auth("proxy-secret")
            .json(&serde_json::json!({
                "model": "127.0.0.1/registered-model",
                "messages": []
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(completion.status(), StatusCode::OK);
        let completion: serde_json::Value = completion.json().await.unwrap();
        assert_eq!(completion["model"], "registered-model");

        let unauthenticated = client
            .get(format!("http://{proxy_address}/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        proxy_task.abort();
        backend_task.abort();
        let _ = tokio::fs::remove_file(file).await;
    }
}
