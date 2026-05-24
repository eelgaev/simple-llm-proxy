use axum::extract::{Request, State};
use axum::response::Response;
use std::sync::Arc;

use crate::error::ProxyError;
use crate::proxy::{acquire_gpu_set_for_model, forward_request, pick_server};
use crate::state::AppState;

const MAX_BODY_SIZE: usize = 10 * 1024 * 1024; // 10 MB

pub async fn proxy_completions(
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
