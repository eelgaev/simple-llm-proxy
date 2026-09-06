use axum::extract::{Request, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

use crate::error::ProxyError;
use crate::proxy::{acquire_gpu_set_for_model, forward_get, forward_request, pick_server};
use crate::state::AppState;

const MAX_BODY_SIZE: usize = 10 * 1024 * 1024; // 10 MB

pub async fn proxy_model_request(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ProxyError> {
    let path = request.uri().path().to_string();
    let body = axum::body::to_bytes(request.into_body(), MAX_BODY_SIZE).await?;

    let parsed: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| ProxyError::BadRequest(format!("invalid JSON: {e}")))?;

    let model = parsed
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or_else(|| ProxyError::BadRequest("missing \"model\" field".into()))?;

    let (gpu_set, permit) = acquire_gpu_set_for_model(&state, model).await?;
    tracing::info!(gpu_set = %gpu_set.name, model = model, "acquired gpu set");

    let server = pick_server(&gpu_set);
    forward_request(&state.http_client, server, &path, body, permit).await
}

pub async fn list_models(
    State(state): State<Arc<AppState>>,
) -> axum::Json<serde_json::Value> {
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        state.discover_models(),
    )
    .await;

    let model_map = state.model_map.load();
    let model_ids: Vec<serde_json::Value> = model_map
        .keys()
        .map(|id| {
            serde_json::json!({
                "id": id,
                "object": "model",
                "owned_by": "system",
            })
        })
        .collect();

    axum::Json(serde_json::json!({
        "object": "list",
        "data": model_ids,
    }))
}

// The two handlers below exist solely for llama.cpp-flavored clients (e.g.
// pi-llama-cpp), which probe /health and /props at the server root instead of
// speaking the OpenAI-style /v1 surface. vLLM/sglang clients never call these.

/// Liveness check. Clients like pi-llama-cpp use this (at the server root, not
/// under /v1) to decide whether the whole endpoint is worth talking to at all.
pub async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "status": "ok" }))
}

/// Mirrors llama.cpp's /props endpoint. Clients like pi-llama-cpp call this
/// both without a `model` query param (to detect single/router/legacy mode)
/// and with one (to read per-model status/capabilities), always at the
/// server root rather than under /v1.
pub async fn get_props(
    State(state): State<Arc<AppState>>,
    uri: Uri,
) -> Result<Response, ProxyError> {
    let query = uri.query().unwrap_or("");
    let path = if query.is_empty() {
        "/props".to_string()
    } else {
        format!("/props?{query}")
    };

    let model = query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == "model").then_some(v)
    });

    let gpu_set = match model {
        Some(model) => {
            let map = state.model_map.load();
            match map.get(model) {
                Some(gpu_sets) => Arc::clone(&gpu_sets[0]),
                None => {
                    // Matches llama.cpp's own shape for an unknown/unloaded model,
                    // which pi-llama-cpp maps to its "unloaded" status.
                    let body = serde_json::json!({
                        "error": {
                            "code": 400,
                            "message": "model is not loaded",
                            "type": "not_found_error",
                        }
                    });
                    return Ok((StatusCode::OK, axum::Json(body)).into_response());
                }
            }
        }
        None => Arc::clone(&state.gpu_sets[0]),
    };

    let server = pick_server(&gpu_set).clone();
    forward_get(&state.http_client, &server, &path).await
}
