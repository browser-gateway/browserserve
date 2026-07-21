//! Bridges one client WebSocket to one browser's CDP pipe.
//!
//! Hot path is raw frames: one WS message equals one `\0`-delimited CDP
//! message, no JSON parsing in either direction.

use crate::chrome::{CdpPipe, CdpReader, CdpWriter};
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::sink::SinkExt;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};

const CLOSE_BROWSER_GONE: u16 = 1011;

/// Pumps messages both ways until either side closes, then stops the other
/// direction. The caller owns session teardown afterwards.
pub async fn bridge(socket: WebSocket, pipe: CdpPipe) {
    let (cdp_reader, cdp_writer) = pipe.split();
    let (ws_sink, ws_stream) = socket.split();

    let mut to_browser = tokio::spawn(pump_client_to_browser(ws_stream, cdp_writer));
    let mut to_client = tokio::spawn(pump_browser_to_client(cdp_reader, ws_sink));

    tokio::select! {
        _ = &mut to_browser => to_client.abort(),
        _ = &mut to_client => to_browser.abort(),
    }
}

async fn pump_client_to_browser(mut ws: SplitStream<WebSocket>, mut cdp: CdpWriter) {
    while let Some(Ok(message)) = ws.next().await {
        let sent = match message {
            Message::Text(text) => cdp.send_raw(text.as_bytes()).await,
            Message::Binary(bytes) => cdp.send_raw(&bytes).await,
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => continue,
        };
        if sent.is_err() {
            break;
        }
    }
}

async fn pump_browser_to_client(mut cdp: CdpReader, mut ws: SplitSink<WebSocket, Message>) {
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
