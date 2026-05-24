use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub enum ProxyError {
    BackendUnavailable(String),
    BadRequest(String),
    Internal(String),
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            ProxyError::BackendUnavailable(msg) => {
                (StatusCode::BAD_GATEWAY, "backend_unavailable", msg)
            }
            ProxyError::BadRequest(msg) => {
                (StatusCode::BAD_REQUEST, "bad_request", msg)
            }
            ProxyError::Internal(msg) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", msg)
            }
        };

        let body = serde_json::json!({
            "error": {
                "message": message,
                "type": "proxy_error",
                "code": code,
            }
        });

        (status, axum::Json(body)).into_response()
    }
}

impl From<reqwest::Error> for ProxyError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_connect() {
            ProxyError::BackendUnavailable(format!("backend connection failed: {err}"))
        } else if err.is_timeout() {
            ProxyError::BackendUnavailable(format!("backend timed out: {err}"))
        } else {
            ProxyError::Internal(format!("request error: {err}"))
        }
    }
}

impl From<axum::Error> for ProxyError {
    fn from(err: axum::Error) -> Self {
        ProxyError::BadRequest(format!("failed to read request body: {err}"))
    }
}
