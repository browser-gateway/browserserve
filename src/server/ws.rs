//! The WS session endpoint: auth, admission, claim, bridge, destroy.

use crate::bridge::{bridge, bridge_reclaimable};
use crate::chrome::CdpClient;
use crate::factory::ChromeSession;
use crate::pool::{ClaimError, SessionFactory};
use crate::profile::cdp::{
    apply_cookies, apply_local_storage, attach_page_target, capture_cookies,
};
use crate::profile::payload::ProfilePayload;
use crate::server::http::QueryMap;
use crate::server::{AppState, http};
use axum::extract::ws::WebSocket;
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

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

    // Profile session: claim the dropped-off profile, launch a fresh browser
    // with its native layer already on disk, inject the portable core, serve,
    // then capture back. Bypasses the warm pool (a warm browser is already
    // launched, so its user-data-dir cannot be seeded before start).
    if let Some(token) = query.get("profileToken").cloned() {
        let Some(payload) = state.profiles.claim(&token) else {
            return reject(
                StatusCode::NOT_FOUND,
                "unknown_profile_token",
                "no pending profile for this token",
            );
        };
        let session = match state.factory.create_seeded(&payload.indexeddb).await {
            Ok(session) => session,
            Err(e) => return reject(StatusCode::SERVICE_UNAVAILABLE, "launch_failed", &e),
        };
        // Read-only: seed the profile but skip capture on close (faster
        // teardown; the gateway also skips the lock, so many sessions can share
        // one read-only profile at once).
        let read_only = query
            .get("readOnly")
            .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
        let cancel = state.cancel.clone();
        let tracker = state.tracker.clone();
        let max = state.max_message_bytes;
        let state = Arc::clone(&state);
        return ws.max_message_size(max).on_upgrade(move |socket| {
            tracker.track_future(profile_session(
                state, socket, session, payload, token, cancel, read_only,
            ))
        });
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

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Seeds, serves, and captures one profile session on its own fresh browser.
async fn profile_session(
    state: Arc<AppState>,
    socket: WebSocket,
    mut session: ChromeSession,
    payload: ProfilePayload,
    token: String,
    cancel: CancellationToken,
    read_only: bool,
) {
    let Some(mut pipe) = session.browser.take_pipe() else {
        state.factory.destroy(session).await;
        return;
    };

    // Inject the portable core over our own pipe, before the client is bridged.
    {
        let mut client = CdpClient::new(&mut pipe);
        match attach_page_target(&mut client).await {
            Ok(session_id) => {
                if let Err(e) = apply_cookies(&mut client, &payload.cookies, now_secs()).await {
                    tracing::warn!(error = %e, "profile cookie inject failed");
                }
                if let Err(e) =
                    apply_local_storage(&mut client, &session_id, &payload.local_storage).await
                {
                    tracing::warn!(error = %e, "profile localStorage inject failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "profile attach failed; serving unseeded"),
        }
    }
    drop(payload);

    // Read-only: serve, then tear down fast with NO capture — nothing is saved
    // back, which is what lets many sessions share one read-only profile.
    if read_only {
        tokio::select! {
            () = bridge(socket, pipe) => {}
            () = cancel.cancelled() => {}
        }
        state.factory.destroy(session).await;
        return;
    }

    // Serve; reclaim the pipe if the CLIENT closed (browser still alive).
    let reclaimed = tokio::select! {
        r = bridge_reclaimable(socket, pipe) => r,
        () = cancel.cancelled() => None,
    };

    // Capture cookies via CDP while the browser is alive (browser-level, no
    // attach). localStorage + IndexedDB are read from disk after the kill, which
    // enumerates EVERY origin — including cookieless ones a CDP capture misses.
    let mut captured = ProfilePayload::default();
    let did_capture = reclaimed.is_some();
    if let Some(mut pipe) = reclaimed {
        {
            let mut client = CdpClient::new(&mut pipe);
            captured.cookies = capture_cookies(&mut client).await.unwrap_or_default();
            // Graceful close: Chrome batches localStorage commits (~5s), so a
            // signal-kill would lose the newest writes. Browser.close flushes
            // storage to disk before exit; then we read the LevelDB.
            let _ = client.send("Browser.close", json!({}), None).await;
        }
        // Wait for the browser to close the pipe (it has flushed + exited).
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            while pipe.recv().await.is_ok() {}
        })
        .await;
    }

    let native = state.factory.destroy_capturing_native(session).await;

    // Only deposit when we actually captured; a browser that died mid-session
    // must not overwrite the gateway's stored profile with nothing.
    if did_capture {
        captured.indexeddb = native.indexeddb;
        captured.local_storage = native.local_storage;
        state.profiles.deposit_result(&token, captured);
    }
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
