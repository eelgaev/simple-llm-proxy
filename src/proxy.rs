use bytes::Bytes;
use futures::future::select_all;
use futures::Stream;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::OwnedSemaphorePermit;

use crate::config::ServerEntry;
use crate::error::ProxyError;
use crate::state::{AppState, GpuSet};

struct PermitGuardStream<S> {
    inner: S,
    _permit: OwnedSemaphorePermit,
}

impl<S, E> Stream for PermitGuardStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

pub async fn acquire_gpu_set_for_model(
    state: &AppState,
    model: &str,
) -> Result<(Arc<GpuSet>, OwnedSemaphorePermit), ProxyError> {
    let map = state.model_map.load();
    let entry = map.get(model).ok_or_else(|| {
        ProxyError::BadRequest(format!("model not found: {model}"))
    })?;

    let futures: Vec<_> = entry
        .gpu_sets
        .iter()
        .map(|gs| {
            let gs = Arc::clone(gs);
            let sem = Arc::clone(&gs.semaphore);
            Box::pin(async move {
                let permit = sem.acquire_owned().await.unwrap();
                (gs, permit)
            })
        })
        .collect();

    drop(map);

    let ((gpu_set, permit), _, _) = select_all(futures).await;
    Ok((gpu_set, permit))
}

pub fn pick_server(gpu_set: &GpuSet) -> &ServerEntry {
    let idx = gpu_set.next_server.fetch_add(1, Ordering::Relaxed);
    &gpu_set.servers[idx % gpu_set.servers.len()]
}

pub async fn forward_get(
    client: &reqwest::Client,
    server: &ServerEntry,
    path_and_query: &str,
) -> Result<axum::response::Response, ProxyError> {
    let url = format!("{}{}", server.url().trim_end_matches('/'), path_and_query);

    tracing::info!("forwarding to {url}");

    let mut req = client.get(&url);
    if let Some(token) = server.token() {
        req = req.bearer_auth(token);
    }

    let backend_resp = req.send().await?;

    let status = backend_resp.status();
    let mut builder = axum::http::Response::builder().status(status.as_u16());

    for (key, value) in backend_resp.headers() {
        if key != "transfer-encoding" {
            builder = builder.header(key, value);
        }
    }

    let bytes = backend_resp.bytes().await?;

    builder
        .body(axum::body::Body::from(bytes))
        .map_err(|e| ProxyError::Internal(format!("failed to build response: {e}")))
}

pub async fn forward_request(
    client: &reqwest::Client,
    server: &ServerEntry,
    path: &str,
    body: Bytes,
    permit: OwnedSemaphorePermit,
) -> Result<axum::response::Response, ProxyError> {
    let url = format!("{}{}", server.url().trim_end_matches('/'), path);

    tracing::info!("forwarding to {url}");

    let mut req = client
        .post(&url)
        .header("content-type", "application/json")
        .body(body);

    if let Some(token) = server.token() {
        req = req.bearer_auth(token);
    }

    let backend_resp = req.send().await?;

    let status = backend_resp.status();
    let mut builder = axum::http::Response::builder().status(status.as_u16());

    for (key, value) in backend_resp.headers() {
        if key != "transfer-encoding" {
            builder = builder.header(key, value);
        }
    }

    let stream = PermitGuardStream {
        inner: backend_resp.bytes_stream(),
        _permit: permit,
    };

    let body = axum::body::Body::from_stream(stream);

    builder
        .body(body)
        .map_err(|e| ProxyError::Internal(format!("failed to build response: {e}")))
}
