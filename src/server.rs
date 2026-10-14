//! The HTTP surface: the notify endpoint, a health check, and the spec's
//! `M_UNRECOGNIZED` for everything else.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::dispatch::Gateway;
use crate::matrix::{MatrixError, NotifyRequest};

pub const NOTIFY_PATH: &str = "/_matrix/push/v1/notify";

pub fn router(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route(NOTIFY_PATH, post(notify))
        .route("/health", get(health))
        .fallback(|| async { MatrixError::unrecognized(StatusCode::NOT_FOUND) })
        .method_not_allowed_fallback(|| async {
            MatrixError::unrecognized(StatusCode::METHOD_NOT_ALLOWED)
        })
        .with_state(gateway)
}

/// Takes the raw body so a malformed one gets the Matrix error shape rather
/// than axum's own rejection, and regardless of `Content-Type`.
async fn notify(State(gateway): State<Arc<Gateway>>, body: Bytes) -> Response {
    match NotifyRequest::try_from(&body[..]) {
        Ok(request) => gateway.notify(request.notification).await.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn health() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], "{}")
}
