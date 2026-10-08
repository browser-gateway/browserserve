//! Keeps a session's browser alive for a short window after its client goes
//! away, so a reconnecting client gets the same browser back.

use crate::bridge::{BridgeEnd, bridge_session};
use crate::chrome::{CdpClient, CdpPipe};
use axum::extract::ws::WebSocket;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Response header carrying the token a client resumes with.
pub const RESUME_TOKEN_HEADER: &str = "browserserve-resume-token";

const CLEANUP_CALL_TIMEOUT: Duration = Duration::from_secs(2);
// Far above any id a CDP client library allocates, so a late reply to the
// previous client's command cannot be mistaken for a cleanup reply, yet inside
// the 32-bit range Chrome accepts for message ids (crdtp/dispatch.cc rejects
// larger ids without echoing them, so the call would never be answered).
const CLEANUP_FIRST_ID: u64 = 2_000_000_000;

/// Parked sessions, keyed by resume token. A session is listed only while it
/// is waiting for a client; a claimed token disappears until the session is
/// parked again.
pub struct ResumeRegistry {
    window: Duration,
    parked: Mutex<HashMap<String, mpsc::Sender<WebSocket>>>,
}

impl ResumeRegistry {
    /// A registry that keeps parked browsers for `window`.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            parked: Mutex::new(HashMap::new()),
        }
    }

    /// A fresh, unguessable resume token (32 random bytes, hex).
    ///
    /// # Errors
    ///
    /// When the operating system's random source is unavailable.
    pub fn new_token() -> Result<String, getrandom::Error> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)?;
        Ok(bytes.iter().fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        }))
    }

    /// Takes the hand-off for a parked session. `None` when the token is
    /// unknown, expired, or already claimed.
    #[must_use]
    pub fn claim(&self, token: &str) -> Option<mpsc::Sender<WebSocket>> {
        self.parked.lock().ok()?.remove(token)
    }

    /// How many sessions are waiting for their client.
    #[must_use]
    pub fn parked(&self) -> usize {
        self.parked.lock().map_or(0, |map| map.len())
    }

    fn park(&self, token: &str, handoff: mpsc::Sender<WebSocket>) {
        if let Ok(mut map) = self.parked.lock() {
            map.insert(token.to_owned(), handoff);
        }
    }

    fn unpark(&self, token: &str) {
        if let Ok(mut map) = self.parked.lock() {
            map.remove(token);
        }
    }
}

/// Serves a session that can be resumed: bridges the client; when the client's
/// connection breaks, parks the browser for the registry's window, discarding
/// everything the browser sends meanwhile; a client presenting the token gets
/// the same browser. Returns when the session must be destroyed (browser gone,
/// a deliberate close, idle timeout, window expired, or shutdown).
pub async fn serve(
    registry: &ResumeRegistry,
    token: &str,
    first_socket: WebSocket,
    first_pipe: CdpPipe,
    idle_timeout: Option<Duration>,
    cancel: &CancellationToken,
) {
    let mut socket = first_socket;
    let mut pipe = first_pipe;
    loop {
        let end = tokio::select! {
            end = bridge_session(socket, pipe, idle_timeout) => end,
            () = cancel.cancelled() => return,
        };
        let BridgeEnd::ClientGone(returned) = end else {
            return;
        };

        let (handoff, mut incoming) = mpsc::channel(1);
        registry.park(token, handoff);
        tracing::info!(
            window_ms = registry.window.as_millis(),
            "client gone; session parked for resume"
        );
        let (mut reader, writer) = returned.split();
        let deadline = tokio::time::sleep(registry.window);
        tokio::pin!(deadline);
        let resumed = loop {
            tokio::select! {
                next = incoming.recv() => break next,
                () = &mut deadline => break None,
                () = cancel.cancelled() => break None,
                frame = reader.recv_raw() => {
                    if frame.is_err() {
                        break None;
                    }
                }
            }
        };
        registry.unpark(token);
        let Some(next_socket) = resumed else {
            tracing::info!("resume window ended without a client; destroying session");
            return;
        };
        pipe = CdpPipe::from_halves(reader, writer);
        clean_slate(&mut pipe).await;
        tracing::info!("session resumed");
        socket = next_socket;
    }
}

/// Best effort: detach the previous client's page sessions and switch off the
/// connection-level target modes it enabled, so the next client starts like a
/// fresh connection while pages, cookies and storage stay.
async fn clean_slate(pipe: &mut CdpPipe) {
    let mut client = CdpClient::with_first_id(pipe, CLEANUP_FIRST_ID);
    if let Ok(Ok(targets)) = tokio::time::timeout(
        CLEANUP_CALL_TIMEOUT,
        client.call("Target.getTargets", json!({})),
    )
    .await
    {
        let attached = targets
            .get("targetInfos")
            .and_then(Value::as_array)
            .map(|infos| {
                infos
                    .iter()
                    .filter(|t| t.get("attached").and_then(Value::as_bool) == Some(true))
                    .filter_map(|t| t.get("targetId").and_then(Value::as_str).map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for target_id in attached {
            let _ = tokio::time::timeout(
                CLEANUP_CALL_TIMEOUT,
                client.call("Target.detachFromTarget", json!({ "targetId": target_id })),
            )
            .await;
        }
    }
    for (method, params) in [
        (
            "Target.setAutoAttach",
            json!({ "autoAttach": false, "waitForDebuggerOnStart": false }),
        ),
        ("Target.setDiscoverTargets", json!({ "discover": false })),
    ] {
        if let Ok(Err(e)) =
            tokio::time::timeout(CLEANUP_CALL_TIMEOUT, client.call(method, params)).await
        {
            tracing::debug!(error = %e, method, "resume cleanup call failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_ids_fit_chromes_message_id_range() {
        assert!(CLEANUP_FIRST_ID + 1_000 <= u64::try_from(i32::MAX).unwrap());
    }

    #[test]
    fn tokens_are_long_and_distinct() {
        let a = ResumeRegistry::new_token().unwrap();
        let b = ResumeRegistry::new_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn a_parked_session_is_claimed_once() {
        let registry = ResumeRegistry::new(Duration::from_mins(1));
        let (tx, _rx) = mpsc::channel(1);
        registry.park("t1", tx);
        assert_eq!(registry.parked(), 1);
        assert!(registry.claim("t1").is_some());
        assert!(registry.claim("t1").is_none());
        assert_eq!(registry.parked(), 0);
    }

    #[test]
    fn unknown_and_unparked_tokens_are_refused() {
        let registry = ResumeRegistry::new(Duration::from_mins(1));
        assert!(registry.claim("nope").is_none());
        let (tx, _rx) = mpsc::channel(1);
        registry.park("t2", tx);
        registry.unpark("t2");
        assert!(registry.claim("t2").is_none());
    }
}
