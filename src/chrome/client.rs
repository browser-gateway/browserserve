//! Request/response CDP client over a browser's `CdpPipe`.
//!
//! Correlates each command to its reply by `id`, skipping protocol events and
//! unrelated replies. Used by profile inject/capture in the exclusive window
//! after Chrome is ready and before the session pipe is handed to the bridge,
//! so no client traffic is interleaved.

use crate::chrome::pipe::{CdpPipe, PipeError};
use serde_json::{Value, json};
use thiserror::Error;

/// Error from a single CDP command.
#[derive(Debug, Error)]
pub enum CdpError {
    /// Transport failure, or the browser closed the pipe.
    #[error(transparent)]
    Pipe(#[from] PipeError),
    /// The browser returned a CDP error response for the command.
    #[error("CDP command {method} failed: {message} (code {code})")]
    Protocol {
        /// The command that failed.
        method: String,
        /// CDP error code.
        code: i64,
        /// CDP error message.
        message: String,
    },
}

/// A request/response CDP client borrowing a ready browser's pipe.
///
/// The borrow is for the command window only; the caller retains the pipe for
/// later bridging.
pub struct CdpClient<'a> {
    pipe: &'a mut CdpPipe,
    next_id: u64,
}

impl<'a> CdpClient<'a> {
    /// Wraps a ready browser's pipe.
    #[must_use]
    pub fn new(pipe: &'a mut CdpPipe) -> Self {
        Self { pipe, next_id: 1 }
    }

    /// Sends one CDP command WITHOUT waiting for the reply and returns its `id`.
    ///
    /// For event-driven flows (e.g. navigating behind a `Fetch` stub) where the
    /// caller pumps [`CdpClient::recv`] itself. `session_id` routes the command
    /// to an attached target in flat mode; `None` is a browser-level command.
    ///
    /// # Errors
    ///
    /// [`CdpError::Pipe`] on transport failure or a closed pipe.
    pub async fn send(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<u64, CdpError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut envelope = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            envelope["sessionId"] = Value::String(sid.to_owned());
        }
        self.pipe.send(&envelope).await?;
        Ok(id)
    }

    /// Receives the next raw CDP message (a command reply or an event).
    ///
    /// # Errors
    ///
    /// [`CdpError::Pipe`] on transport failure or a closed pipe.
    pub async fn recv(&mut self) -> Result<Value, CdpError> {
        Ok(self.pipe.recv().await?)
    }

    /// Sends a browser-level command and returns its `result`.
    ///
    /// # Errors
    ///
    /// As [`CdpClient::call_on`].
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, CdpError> {
        self.call_on(None, method, params).await
    }

    /// Sends a command (optionally routed to an attached target via `session_id`)
    /// and returns its `result`, correlating the reply by `id` and skipping any
    /// events or unrelated replies in between.
    ///
    /// # Errors
    ///
    /// [`CdpError::Pipe`] on transport failure or a closed pipe;
    /// [`CdpError::Protocol`] when the browser returns a CDP error response.
    pub async fn call_on(
        &mut self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, CdpError> {
        let id = self.send(method, params, session_id).await?;
        loop {
            let msg = self.recv().await?;
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                return Err(CdpError::Protocol {
                    method: method.to_owned(),
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned(),
                });
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::mock_pipe;

    #[tokio::test]
    async fn call_correlates_reply_and_skips_interleaved_events() {
        let (mut cdp, mut browser) = mock_pipe();
        let chrome = tokio::spawn(async move {
            let cmd = browser.expect("Storage.getCookies").await;
            browser.emit("Network.requestWillBeSent", json!({})).await;
            browser
                .write(&json!({ "id": cmd["id"], "result": { "cookies": [] } }))
                .await;
        });

        let mut client = CdpClient::new(&mut cdp);
        let result = client.call("Storage.getCookies", json!({})).await.unwrap();
        assert_eq!(result, json!({ "cookies": [] }));
        chrome.await.unwrap();
    }

    #[tokio::test]
    async fn call_maps_cdp_error_response() {
        let (mut cdp, mut browser) = mock_pipe();
        let chrome = tokio::spawn(async move {
            let cmd = browser.expect("Storage.setCookies").await;
            browser
                .write(&json!({ "id": cmd["id"], "error": { "code": -32000, "message": "boom" } }))
                .await;
        });

        let mut client = CdpClient::new(&mut cdp);
        let err = client
            .call("Storage.setCookies", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            CdpError::Protocol { code: -32000, ref message, .. } if message == "boom"
        ));
        chrome.await.unwrap();
    }

    #[tokio::test]
    async fn ids_increment_across_calls() {
        let (mut cdp, mut browser) = mock_pipe();
        let chrome = tokio::spawn(async move {
            let first = browser.expect_ok("A.b", json!({})).await;
            let second = browser.expect_ok("C.d", json!({})).await;
            assert_eq!(
                second["id"].as_u64().unwrap(),
                first["id"].as_u64().unwrap() + 1
            );
        });

        let mut client = CdpClient::new(&mut cdp);
        client.call("A.b", json!({})).await.unwrap();
        client.call("C.d", json!({})).await.unwrap();
        chrome.await.unwrap();
    }
}
