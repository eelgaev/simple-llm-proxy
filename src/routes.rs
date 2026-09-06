use axum::extract::{Request, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use std::sync::Arc;

use crate::error::ProxyError;
use crate::model_scan::{ModelScanner, ScanError};
use crate::proxy::{acquire_gpu_set_for_model, forward_get, forward_request, pick_server};
use crate::state::AppState;

/// How much of the body we will hold while looking for the `"model"` key.
/// Routing needs that key, so this much is unavoidably buffered; everything
/// past it is relayed to the backend as it arrives. Matches the whole-body
/// limit this handler used to enforce, so no request that routed before is
/// rejected now -- and bodies larger than this stream through fine as long as
/// `"model"` appears within it, which is where clients put it.
const MAX_MODEL_SCAN_BYTES: usize = 10 * 1024 * 1024; // 10 MiB

pub async fn proxy_model_request(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ProxyError> {
    let path = request.uri().path().to_string();
    // The body is relayed byte-for-byte, so the client's own framing still
    // describes it; without this the request goes out chunked.
    let content_length = request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .cloned();

    let mut body = request.into_body().into_data_stream();

    // Read only as far as the top-level "model" key, which is what routing
    // needs. The rest of the body never lands in proxy memory.
    let mut scanner = ModelScanner::new();
    let mut head = BytesMut::new();
    let mut model = None;

    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        head.extend_from_slice(&chunk);

        match scanner.feed(&chunk) {
            Ok(Some(found)) => {
                model = Some(found);
                break;
            }
            Ok(None) => {}
            Err(ScanError::Absent) => {
                return Err(ProxyError::BadRequest("missing \"model\" field".into()))
            }
            Err(ScanError::ModelNotString) => {
                return Err(ProxyError::BadRequest("\"model\" must be a string".into()))
            }
            Err(ScanError::Malformed) => {
                return Err(ProxyError::BadRequest("invalid JSON request body".into()))
            }
        }

        if head.len() > MAX_MODEL_SCAN_BYTES {
            return Err(ProxyError::BadRequest(format!(
                "no \"model\" field in the first {MAX_MODEL_SCAN_BYTES} bytes of the body"
            )));
        }
    }

    let model = model.ok_or_else(|| ProxyError::BadRequest("missing \"model\" field".into()))?;

    let (gpu_set, permit) = acquire_gpu_set_for_model(&state, &model).await?;
    tracing::info!(gpu_set = %gpu_set.name, model = %model, "acquired gpu set");

    // What we buffered, then the client's stream picked up where it left off.
    let head = head.freeze();
    let outgoing = futures::stream::once(async move { Ok::<Bytes, axum::Error>(head) }).chain(body);

    let server = pick_server(&gpu_set);
    forward_request(
        &state.http_client,
        server,
        &path,
        reqwest::Body::wrap_stream(outgoing),
        content_length,
        permit,
    )
    .await
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
    // Each model object is served exactly as its backend reported it, so
    // backend-specific metadata (llama.cpp's `meta.n_ctx`, vLLM/sglang's
    // `max_model_len`, ...) reaches clients unchanged.
    let models: Vec<serde_json::Value> = model_map
        .values()
        .map(|entry| entry.info.clone())
        .collect();

    let mut body = serde_json::json!({
        "object": "list",
        "data": models,
    });

    // llama.cpp also lists models under a top-level `models` key; mirror it when
    // any backend provided one, and omit it entirely when none did.
    let listings: Vec<serde_json::Value> = model_map
        .values()
        .filter_map(|entry| entry.listing.clone())
        .collect();
    if !listings.is_empty() {
        body["models"] = serde_json::Value::Array(listings);
    }

    axum::Json(body)
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
                Some(entry) => Arc::clone(&entry.gpu_sets[0]),
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
