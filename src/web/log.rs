//! Request logging.
//!
//! Every control interaction (a `POST` to `/zone/*`, `/zones/*`, `/ac/*`, or
//! `/refresh`) is logged at `info` level with the client IP, the action
//! (method + path + the raw form body, e.g. `power=on`), and the response
//! status plus elapsed time. All other requests (pages, partials, the SSE
//! stream, vendor assets) are logged at `debug` level with the IP, method,
//! path, and status.
//!
//! This exists because the AirTouch console can silently hang a single API
//! call, which blocks the manager's one-task command loop and deadlocks the
//! UI. With every interaction logged, the last line before a stall shows
//! exactly which action hung (and the manager's timeout now reconnects
//! instead of wedging forever).
//!
//! The client IP comes from axum's `ConnectInfo<SocketAddr>` extension, which
//! is only populated when the server is run via
//! `into_make_service_with_connect_info`. When it is absent (e.g. the test
//! harness, which serves a plain `Router`), the IP is logged as `-`.

use std::net::SocketAddr;
use std::time::Instant;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::ConnectInfo;
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;

/// Maximum form body we will buffer for logging. Control forms are tiny
/// (a handful of fields); anything larger is truncated to an empty action
/// rather than buffered in full.
const LOG_BODY_LIMIT: usize = 1 << 20; // 1 MiB

/// `axum::middleware::from_fn` target: log each request as it passes through.
pub async fn request_log(req: axum::extract::Request, next: Next) -> Response {
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "-".to_string());
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let is_control = method == Method::POST
        && (path == "/refresh"
            || path.starts_with("/zone/")
            || path.starts_with("/zones/")
            || path.starts_with("/ac/"));

    let started = Instant::now();

    // For control actions, buffer the form body so we can log it as the
    // "action" and still hand the handler a re-readable request body.
    let (action, req): (Option<String>, axum::extract::Request) = if is_control {
        let (parts, body) = req.into_parts();
        let bytes: Bytes = to_bytes(body, LOG_BODY_LIMIT)
            .await
            .unwrap_or_default();
        let action = String::from_utf8_lossy(&bytes).to_string();
        let req = axum::extract::Request::from_parts(parts, Body::from(bytes));
        (Some(action), req)
    } else {
        (None, req)
    };

    let resp = next.run(req).await;
    let status = resp.status();
    let dur = started.elapsed();

    if is_control {
        tracing::info!(
            "interaction ip={ip} {method} {path} action={action:?} -> {status} ({dur:?})"
        );
    } else {
        tracing::debug!("request ip={ip} {method} {path} -> {status} ({dur:?})");
    }

    resp
}

/// Read the IP from a request's `ConnectInfo` extension, if present.
#[allow(dead_code)]
pub fn request_ip(req: &axum::extract::Request) -> String {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "-".to_string())
}
