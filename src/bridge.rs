//! Bridges one client WebSocket to one browser's CDP pipe.
//!
//! Hot path is raw frames: one WS message equals one `\0`-delimited CDP
//! message, no JSON parsing in either direction.

use crate::chrome::{CdpPipe, CdpReader, CdpWriter};
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use std::time::Duration;

const CLOSE_NORMAL: u16 = 1000;
const CLOSE_BROWSER_GONE: u16 = 1011;
const CLOSE_IDLE_TIMEOUT: u16 = 1013;

/// Pumps messages both ways until either side closes, then stops the other
/// direction. The caller owns session teardown afterwards. When `idle_timeout`
/// is `Some`, a session whose client stops sending CDP messages for that long
/// is killed and the client receives a `1013` close.
pub async fn bridge(socket: WebSocket, pipe: CdpPipe, idle_timeout: Option<Duration>) {
    let _ = bridge_reclaimable(socket, pipe, idle_timeout).await;
}

/// How a bridged session ended.
pub enum BridgeEnd {
    /// The client's connection broke (no close frame, an error, a failed send,
    /// or a close code other than 1000). The browser is still alive and its
    /// pipe is handed back.
    ClientGone(CdpPipe),
    /// The client closed the connection on purpose (close frame with code 1000
    /// or no code). The browser is still alive and its pipe is handed back.
    ClientFinished(CdpPipe),
    /// The client sent nothing for the idle timeout and was sent a `1013` close.
    /// The browser is still alive and its pipe is handed back.
    IdleTimeout(CdpPipe),
    /// The browser side closed first; nothing is left to reuse.
    BrowserGone,
}

/// Like [`bridge`], but returns the CDP pipe when the CLIENT closed (the browser
/// is still alive, so the caller can run post-session capture over it), or
/// `None` when the browser closed first (nothing left to capture).
pub async fn bridge_reclaimable(
    socket: WebSocket,
    pipe: CdpPipe,
    idle_timeout: Option<Duration>,
) -> Option<CdpPipe> {
    match bridge_session(socket, pipe, idle_timeout).await {
        BridgeEnd::ClientGone(pipe)
        | BridgeEnd::ClientFinished(pipe)
        | BridgeEnd::IdleTimeout(pipe) => Some(pipe),
        BridgeEnd::BrowserGone => None,
    }
}

/// Pumps messages both ways until either side ends, and reports which side
/// ended. The pipe comes back whenever the browser is still alive.
pub async fn bridge_session(
    socket: WebSocket,
    pipe: CdpPipe,
    idle_timeout: Option<Duration>,
) -> BridgeEnd {
    let (mut reader, mut writer) = pipe.split();
    let (mut ws_sink, mut ws_stream) = socket.split();

    // Borrowed pumps: whichever side ends, the other future is cancelled but
    // its half stays owned here, so the pipe stays reassemblable.
    let ended = tokio::select! {
        outcome = pump_client_to_browser(&mut ws_stream, &mut writer, idle_timeout) => {
            if let PumpExit::IdleTimeout(dur) = outcome {
                let _ = ws_sink
                    .send(Message::Close(Some(CloseFrame {
                        code: CLOSE_IDLE_TIMEOUT,
                        reason: axum::extract::ws::Utf8Bytes::from(format!(
                            "idle-timeout after {}ms",
                            dur.as_millis()
                        )),
                    })))
                    .await;
                Ended::IdleTimeout
            } else if matches!(outcome, PumpExit::ClientFinished) {
                Ended::ClientFinished
            } else {
                Ended::ClientGone
            }
        }
        exit = pump_browser_to_client(&mut reader, &mut ws_sink) => match exit {
            BrowserPumpExit::BrowserGone => Ended::BrowserGone,
            BrowserPumpExit::ClientGone => Ended::ClientGone,
        },
    };
    match ended {
        Ended::ClientGone => BridgeEnd::ClientGone(CdpPipe::from_halves(reader, writer)),
        Ended::ClientFinished => BridgeEnd::ClientFinished(CdpPipe::from_halves(reader, writer)),
        Ended::IdleTimeout => BridgeEnd::IdleTimeout(CdpPipe::from_halves(reader, writer)),
        Ended::BrowserGone => BridgeEnd::BrowserGone,
    }
}

enum Ended {
    ClientGone,
    ClientFinished,
    IdleTimeout,
    BrowserGone,
}

/// Why the browser pump exited.
enum BrowserPumpExit {
    /// The browser closed its pipe (the client was sent a `1011` close).
    BrowserGone,
    /// Sending to the client failed: the client is gone, the browser is not.
    ClientGone,
}

/// Why the client pump exited.
enum PumpExit {
    /// The connection broke: no close frame, an error, or an abnormal close code.
    ClientClosed,
    /// The client sent a close frame with code 1000 or no code.
    ClientFinished,
    /// No client→browser message arrived within `idle_timeout`.
    IdleTimeout(Duration),
}

async fn pump_client_to_browser(
    ws: &mut SplitStream<WebSocket>,
    cdp: &mut CdpWriter,
    idle_timeout: Option<Duration>,
) -> PumpExit {
    loop {
        let next = match idle_timeout {
            Some(dur) => match tokio::time::timeout(dur, ws.next()).await {
                Ok(next) => next,
                Err(_) => return PumpExit::IdleTimeout(dur),
            },
            None => ws.next().await,
        };
        let Some(Ok(message)) = next else {
            return PumpExit::ClientClosed;
        };
        let sent = match message {
            Message::Text(text) => cdp.send_raw(text.as_bytes()).await,
            Message::Binary(bytes) => cdp.send_raw(&bytes).await,
            Message::Close(frame) => return close_exit(frame.as_ref()),
            Message::Ping(_) | Message::Pong(_) => continue,
        };
        if sent.is_err() {
            return PumpExit::ClientClosed;
        }
    }
}

/// A close frame with code 1000, or with no code, is a deliberate end; any
/// other code (going away, a proxy restart, an application code) is a break.
fn close_exit(frame: Option<&CloseFrame>) -> PumpExit {
    match frame {
        None => PumpExit::ClientFinished,
        Some(frame) if frame.code == CLOSE_NORMAL => PumpExit::ClientFinished,
        Some(_) => PumpExit::ClientClosed,
    }
}

async fn pump_browser_to_client(
    cdp: &mut CdpReader,
    ws: &mut SplitSink<WebSocket, Message>,
) -> BrowserPumpExit {
    loop {
        let Ok(frame) = cdp.recv_raw().await else {
            let _ = ws
                .send(Message::Close(Some(CloseFrame {
                    code: CLOSE_BROWSER_GONE,
                    reason: axum::extract::ws::Utf8Bytes::from_static("browser closed"),
                })))
                .await;
            return BrowserPumpExit::BrowserGone;
        };
        let message = match axum::extract::ws::Utf8Bytes::try_from(frame.clone()) {
            Ok(text) => Message::Text(text),
            Err(_) => Message::Binary(frame),
        };
        if ws.send(message).await.is_err() {
            return BrowserPumpExit::ClientGone;
        }
    }
}

#[cfg(test)]
mod close_tests {
    use super::*;

    fn frame(code: u16) -> CloseFrame {
        CloseFrame {
            code,
            reason: axum::extract::ws::Utf8Bytes::from_static(""),
        }
    }

    #[test]
    fn a_normal_or_codeless_close_is_a_deliberate_end() {
        assert!(matches!(close_exit(None), PumpExit::ClientFinished));
        assert!(matches!(
            close_exit(Some(&frame(1000))),
            PumpExit::ClientFinished
        ));
    }

    #[test]
    fn going_away_and_application_codes_are_breaks() {
        assert!(matches!(
            close_exit(Some(&frame(1001))),
            PumpExit::ClientClosed
        ));
        assert!(matches!(
            close_exit(Some(&frame(4100))),
            PumpExit::ClientClosed
        ));
    }
}
