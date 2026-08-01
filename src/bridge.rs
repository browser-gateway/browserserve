//! Bridges one client WebSocket to one browser's CDP pipe.
//!
//! Hot path is raw frames: one WS message equals one `\0`-delimited CDP
//! message, no JSON parsing in either direction.

use crate::chrome::{CdpPipe, CdpReader, CdpWriter};
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use std::time::Duration;

const CLOSE_BROWSER_GONE: u16 = 1011;
const CLOSE_IDLE_TIMEOUT: u16 = 1013;

/// Pumps messages both ways until either side closes, then stops the other
/// direction. The caller owns session teardown afterwards. When `idle_timeout`
/// is `Some`, a session whose client stops sending CDP messages for that long
/// is killed and the client receives a `1013` close.
pub async fn bridge(socket: WebSocket, pipe: CdpPipe, idle_timeout: Option<Duration>) {
    let _ = bridge_reclaimable(socket, pipe, idle_timeout).await;
}

/// Like [`bridge`], but returns the CDP pipe when the CLIENT closed (the browser
/// is still alive, so the caller can run post-session capture over it), or
/// `None` when the browser closed first (nothing left to capture).
pub async fn bridge_reclaimable(
    socket: WebSocket,
    pipe: CdpPipe,
    idle_timeout: Option<Duration>,
) -> Option<CdpPipe> {
    let (mut reader, mut writer) = pipe.split();
    let (mut ws_sink, mut ws_stream) = socket.split();

    // Borrowed pumps: whichever side closes, the other future is cancelled but
    // its half stays owned here, so a client-close leaves the pipe reassemblable.
    let client_closed = tokio::select! {
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
            }
            true
        }
        () = pump_browser_to_client(&mut reader, &mut ws_sink) => false,
    };
    client_closed.then(|| CdpPipe::from_halves(reader, writer))
}

/// Why the client pump exited.
enum PumpExit {
    /// Client closed the WebSocket, or a send/upstream error broke the pump.
    ClientClosed,
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
            Message::Close(_) => return PumpExit::ClientClosed,
            Message::Ping(_) | Message::Pong(_) => continue,
        };
        if sent.is_err() {
            return PumpExit::ClientClosed;
        }
    }
}

async fn pump_browser_to_client(cdp: &mut CdpReader, ws: &mut SplitSink<WebSocket, Message>) {
    loop {
        let Ok(frame) = cdp.recv_raw().await else {
            let _ = ws
                .send(Message::Close(Some(CloseFrame {
                    code: CLOSE_BROWSER_GONE,
                    reason: axum::extract::ws::Utf8Bytes::from_static("browser closed"),
                })))
                .await;
            return;
        };
        let message = match axum::extract::ws::Utf8Bytes::try_from(frame.clone()) {
            Ok(text) => Message::Text(text),
            Err(_) => Message::Binary(frame),
        };
        if ws.send(message).await.is_err() {
            return;
        }
    }
}
