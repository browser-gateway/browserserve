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

/// Maps a claim failure to its HTTP status and short reason. Every variant is a
/// transient capacity/lifecycle condition the client should retry, so all map to
/// 503 (never 500) — a browser that fails to launch (e.g. the container's
/// `pids.max` is exhausted) is the server being at capacity, not a server bug.
fn claim_status(e: &ClaimError) -> (StatusCode, &'static str) {
    let reason = match e {
        ClaimError::QueueFull { .. } => "queue_full",
        ClaimError::QueueTimeout { .. } => "queue_timeout",
        ClaimError::Closed => "draining",
        ClaimError::Launch { .. } => "launch_failed",
    };
    (StatusCode::SERVICE_UNAVAILABLE, reason)
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
        Err(e) => {
            let (status, reason) = claim_status(&e);
            return reject(status, reason, &e.to_string());
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

#[cfg(test)]
mod tests {
    use super::{ClaimError, StatusCode, claim_status};

    #[test]
    fn every_claim_failure_is_503_never_500() {
        let cases = [
            ClaimError::QueueFull { max_queue: 1 },
            ClaimError::QueueTimeout { waited_ms: 1 },
            ClaimError::Closed,
            ClaimError::Launch {
                message: "pids.max exhausted".into(),
            },
        ];
        for e in &cases {
            assert_eq!(claim_status(e).0, StatusCode::SERVICE_UNAVAILABLE, "{e}");
        }
        assert_eq!(
            claim_status(&ClaimError::Launch {
                message: String::new()
            })
            .1,
            "launch_failed"
        );
    }
}
