//! The WS session endpoint: auth, admission, claim, bridge, destroy.

use crate::bridge::bridge;
use crate::pool::ClaimError;
use crate::server::http::QueryMap;
use crate::server::{AppState, http};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;

fn reject(status: StatusCode, reason: &str, detail: &str) -> Response {
    (
        status,
        axum::Json(json!({ "error": reason, "detail": detail })),
    )
        .into_response()
}

/// `WS /`: one connection, one isolated browser.
pub async fn ws_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<QueryMap>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !http::check_auth(&state, &query, &headers) {
        return http::unauthorized();
    }

    let (cpu, memory) = state.gauge.snapshot();
    if cpu > state.pressure.max_cpu_percent {
        return reject(
            StatusCode::SERVICE_UNAVAILABLE,
            "pressure",
            &format!("cpu at {cpu:.0}%"),
        );
    }
    if memory > state.pressure.max_memory_percent {
        return reject(
            StatusCode::SERVICE_UNAVAILABLE,
            "pressure",
            &format!("memory at {memory:.0}%"),
        );
    }

    let mut claimed = match state.pool.claim().await {
        Ok(claimed) => claimed,
        Err(e @ ClaimError::QueueFull { .. }) => {
            return reject(
                StatusCode::SERVICE_UNAVAILABLE,
                "queue_full",
                &e.to_string(),
            );
        }
        Err(e @ ClaimError::QueueTimeout { .. }) => {
            return reject(
                StatusCode::SERVICE_UNAVAILABLE,
                "queue_timeout",
                &e.to_string(),
            );
        }
        Err(e @ ClaimError::Closed) => {
            return reject(StatusCode::SERVICE_UNAVAILABLE, "draining", &e.to_string());
        }
        Err(e @ ClaimError::Launch { .. }) => {
            return reject(
                StatusCode::INTERNAL_SERVER_ERROR,
                "launch_failed",
                &e.to_string(),
            );
        }
    };

    let Some(pipe) = claimed.session_mut().browser.take_pipe() else {
        let pool = state.pool.clone();
        tokio::spawn(async move { pool.destroy(claimed).await });
        return reject(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "claimed browser had no transport",
        );
    };

    let pool = state.pool.clone();
    let cancel = state.cancel.clone();
    let tracker = state.tracker.clone();
    ws.max_message_size(state.max_message_bytes)
        .on_upgrade(move |socket| {
            tracker.track_future(async move {
                tokio::select! {
                    () = bridge(socket, pipe) => {}
                    () = cancel.cancelled() => {}
                }
                pool.destroy(claimed).await;
            })
        })
}
