//! Test-only CDP mock browser over a real pipe pair. Shared by the `chrome`
//! and `profile` unit tests so the mock plumbing lives in one place.

use crate::chrome::CdpPipe;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;

/// The browser end of a mock CDP pipe: reads the runtime's commands and writes
/// replies and events back.
pub(crate) struct MockBrowser {
    rx: pipe::Receiver,
    tx: pipe::Sender,
}

/// Builds a runtime-side [`CdpPipe`] wired to a [`MockBrowser`] over real pipes.
#[must_use]
pub(crate) fn mock_pipe() -> (CdpPipe, MockBrowser) {
    let (chrome_tx, our_rx) = pipe::pipe().unwrap();
    let (our_tx, chrome_rx) = pipe::pipe().unwrap();
    (
        CdpPipe::new(our_tx, our_rx, 1 << 20),
        MockBrowser {
            rx: chrome_rx,
            tx: chrome_tx,
        },
    )
}

impl MockBrowser {
    /// Reads the next `\0`-framed message from the runtime.
    pub(crate) async fn read(&mut self) -> Value {
        let mut bytes = Vec::new();
        let mut b = [0u8; 1];
        loop {
            self.rx.read_exact(&mut b).await.unwrap();
            if b[0] == 0 {
                break;
            }
            bytes.push(b[0]);
        }
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Writes one `\0`-framed message to the runtime.
    pub(crate) async fn write(&mut self, v: &Value) {
        let mut frame = serde_json::to_vec(v).unwrap();
        frame.push(0);
        self.tx.write_all(&frame).await.unwrap();
    }

    /// Reads one command and asserts its method, returning the whole command.
    pub(crate) async fn expect(&mut self, method: &str) -> Value {
        let cmd = self.read().await;
        assert_eq!(cmd["method"], method, "unexpected command method");
        cmd
    }

    /// Reads a command, asserts its method, and replies with `{ result }`.
    pub(crate) async fn expect_ok(&mut self, method: &str, result: Value) -> Value {
        let cmd = self.expect(method).await;
        self.write(&json!({ "id": cmd["id"], "result": result }))
            .await;
        cmd
    }

    /// Emits a CDP event (no id).
    pub(crate) async fn emit(&mut self, method: &str, params: Value) {
        self.write(&json!({ "method": method, "params": params }))
            .await;
    }

    /// Serves one `navigate_stub`: reads `Page.navigate` (optionally asserting the
    /// url), pauses and fulfills one request, then fires the load event.
    pub(crate) async fn serve_stub_nav(&mut self, expect_url: Option<&str>) {
        let cmd = self.expect("Page.navigate").await;
        if let Some(url) = expect_url {
            assert_eq!(cmd["params"]["url"], url);
        }
        self.emit("Fetch.requestPaused", json!({ "requestId": "R1" }))
            .await;
        self.expect("Fetch.fulfillRequest").await;
        self.emit("Page.loadEventFired", json!({})).await;
    }

    /// Serves a bare navigation with no interception (e.g. the `about:blank`
    /// cleanup after `Fetch.disable`).
    pub(crate) async fn serve_blank_nav(&mut self) {
        self.expect("Page.navigate").await;
        self.emit("Page.loadEventFired", json!({})).await;
    }

    /// Returns true if the runtime sent any byte within `grace`. Use to assert a
    /// command was skipped entirely (expect `false`).
    pub(crate) async fn received_within(&mut self, grace: std::time::Duration) -> bool {
        let mut b = [0u8; 1];
        matches!(tokio::time::timeout(grace, self.rx.read(&mut b)).await, Ok(Ok(n)) if n > 0)
    }
}
