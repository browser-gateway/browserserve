//! CDP-level profile operations over the session pipe.
//!
//! Runs in the exclusive window after Chrome is ready and before the pipe is
//! bridged to the client. Cookies use browser-level `Storage.*` (no target
//! attach). localStorage (which needs a live frame per origin) lands here later.

use crate::chrome::{CdpClient, CdpError};
use crate::profile::cookie::{Cookie, DropCounts, sanitize};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

/// Error from a CDP-level profile operation.
#[derive(Debug, Error)]
pub enum ProfileCdpError {
    /// A CDP command failed.
    #[error(transparent)]
    Cdp(#[from] CdpError),
    /// A CDP reply could not be decoded into the profile model.
    #[error("decoding CDP response: {0}")]
    Decode(#[from] serde_json::Error),
    /// A CDP reply was missing an expected field.
    #[error("unexpected CDP response: {0}")]
    Unexpected(&'static str),
}

/// Outcome of injecting cookies: how many were applied and why any were dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CookieApplyReport {
    /// Cookies sent to the browser.
    pub applied: usize,
    /// Cookies dropped as unsafe or un-restorable, by reason.
    pub dropped: DropCounts,
}

/// Reads every cookie from the browser's default context.
///
/// # Errors
///
/// [`ProfileCdpError::Cdp`] if the command fails; [`ProfileCdpError::Decode`]
/// if the reply is not the expected cookie shape.
pub async fn capture_cookies(client: &mut CdpClient<'_>) -> Result<Vec<Cookie>, ProfileCdpError> {
    let result = client.call("Storage.getCookies", json!({})).await?;
    let cookies = result.get("cookies").cloned().unwrap_or_else(|| json!([]));
    Ok(serde_json::from_value(cookies)?)
}

/// Sanitizes `cookies` (dropping any that cannot be safely reproduced) and
/// injects the survivors into the browser's default context. `now_secs` is the
/// current time in seconds since epoch, used for the expiry check.
///
/// # Errors
///
/// [`ProfileCdpError::Cdp`] if the set command fails.
pub async fn apply_cookies(
    client: &mut CdpClient<'_>,
    cookies: &[Cookie],
    now_secs: f64,
) -> Result<CookieApplyReport, ProfileCdpError> {
    let (params, dropped) = sanitize(cookies, now_secs);
    let applied = params.len();
    if !params.is_empty() {
        client
            .call("Storage.setCookies", json!({ "cookies": params }))
            .await?;
    }
    Ok(CookieApplyReport { applied, dropped })
}

/// One localStorage key/value pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageEntry {
    /// The localStorage key.
    pub name: String,
    /// The localStorage value.
    pub value: String,
}

/// A single origin's localStorage, keyed by its first-party origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginState {
    /// The origin (e.g. `https://example.com`) whose store this is.
    pub origin: String,
    /// The origin's localStorage entries.
    pub local_storage: Vec<StorageEntry>,
}

/// Attaches to the browser's page target in flat mode and returns its session
/// id, which the localStorage inject/capture calls route commands to.
///
/// # Errors
///
/// [`ProfileCdpError::Cdp`] on command failure; [`ProfileCdpError::Unexpected`]
/// if there is no page target or the attach returns no session id.
pub async fn attach_page_target(client: &mut CdpClient<'_>) -> Result<String, ProfileCdpError> {
    let targets = client.call("Target.getTargets", json!({})).await?;
    let target_id = targets
        .get("targetInfos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .and_then(|t| t.get("targetId").and_then(Value::as_str))
        .ok_or(ProfileCdpError::Unexpected("no page target to attach"))?
        .to_owned();
    let attached = client
        .call(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await?;
    let session_id = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or(ProfileCdpError::Unexpected("attach returned no sessionId"))?
        .to_owned();
    Ok(session_id)
}

/// Restores localStorage for each origin by loading a synthetic blank document
/// at that origin (top-level frame, no network) and writing the entries in.
///
/// `session_id` must be an attached page target (flat mode). Restoring via the
/// top-level frame lands the writes in the origin's first-party partition — the
/// same bucket the site reads. Returns how many origins were applied.
///
/// # Errors
///
/// [`ProfileCdpError::Cdp`] if any CDP command fails.
pub async fn apply_local_storage(
    client: &mut CdpClient<'_>,
    session_id: &str,
    origins: &[OriginState],
) -> Result<usize, ProfileCdpError> {
    if origins.is_empty() {
        return Ok(0);
    }
    enable_interception(client, session_id).await?;
    for origin in origins {
        navigate_stub(client, session_id, &format!("{}/", origin.origin)).await?;
        client
            .call_on(
                Some(session_id),
                "Runtime.evaluate",
                json!({
                    "expression": restore_script(&origin.local_storage),
                    "returnByValue": true,
                }),
            )
            .await?;
    }
    disable_interception(client, session_id).await?;
    Ok(origins.len())
}

/// Captures the current localStorage for each of `origins` by loading a
/// synthetic blank document at each (top-level frame, no network) and reading it
/// back. Origins with no entries are omitted. Non-destructive.
///
/// The caller supplies the candidate origins (the ones injected this session
/// plus any derived from captured cookies); localStorage on an origin that was
/// neither injected nor cookie-bearing is not discovered here (exhaustive
/// on-disk enumeration is a separate, tier-3-validated path).
///
/// # Errors
///
/// [`ProfileCdpError::Cdp`] on command failure; [`ProfileCdpError::Decode`] if
/// a read result is not the expected shape.
pub async fn capture_local_storage(
    client: &mut CdpClient<'_>,
    session_id: &str,
    origins: &[String],
) -> Result<Vec<OriginState>, ProfileCdpError> {
    if origins.is_empty() {
        return Ok(Vec::new());
    }
    enable_interception(client, session_id).await?;
    let mut captured = Vec::new();
    for origin in origins {
        navigate_stub(client, session_id, &format!("{origin}/")).await?;
        let result = client
            .call_on(
                Some(session_id),
                "Runtime.evaluate",
                json!({ "expression": CAPTURE_SCRIPT, "returnByValue": true }),
            )
            .await?;
        let entries: Vec<StorageEntry> = result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        if !entries.is_empty() {
            captured.push(OriginState {
                origin: origin.clone(),
                local_storage: entries,
            });
        }
    }
    disable_interception(client, session_id).await?;
    Ok(captured)
}

async fn enable_interception(
    client: &mut CdpClient<'_>,
    session_id: &str,
) -> Result<(), ProfileCdpError> {
    let sid = Some(session_id);
    client.call_on(sid, "Page.enable", json!({})).await?;
    client
        .call_on(
            sid,
            "Fetch.enable",
            json!({ "patterns": [{ "urlPattern": "*" }] }),
        )
        .await?;
    Ok(())
}

async fn disable_interception(
    client: &mut CdpClient<'_>,
    session_id: &str,
) -> Result<(), ProfileCdpError> {
    // Remove interception and clear the synthetic page before the client sees it.
    client
        .call_on(Some(session_id), "Fetch.disable", json!({}))
        .await?;
    navigate_stub(client, session_id, "about:blank").await?;
    Ok(())
}

const CAPTURE_SCRIPT: &str = "(()=>{try{return Object.keys(localStorage).map(k=>({name:k,value:localStorage.getItem(k)}));}catch(e){return [];}})()";

// Navigates the top-level frame to `url`, fulfilling the document request with
// an empty in-origin document so no real network is touched, and returns once
// the page has loaded. Any intercepted request is fulfilled the same way.
async fn navigate_stub(
    client: &mut CdpClient<'_>,
    session_id: &str,
    url: &str,
) -> Result<(), ProfileCdpError> {
    let sid = Some(session_id);
    client
        .send("Page.navigate", json!({ "url": url }), sid)
        .await?;
    loop {
        let msg = client.recv().await?;
        match msg.get("method").and_then(Value::as_str) {
            Some("Fetch.requestPaused") => {
                let request_id = msg["params"]["requestId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                client
                    .send(
                        "Fetch.fulfillRequest",
                        json!({
                            "requestId": request_id,
                            "responseCode": 200,
                            "responseHeaders": [{ "name": "Content-Type", "value": "text/html" }],
                        }),
                        sid,
                    )
                    .await?;
            }
            Some("Page.loadEventFired") => return Ok(()),
            _ => {}
        }
    }
}

fn restore_script(entries: &[StorageEntry]) -> String {
    let data = serde_json::to_string(entries).unwrap_or_else(|_| "[]".to_owned());
    format!(
        "(()=>{{try{{localStorage.clear();for(const e of {data}){{localStorage.setItem(e.name,e.value);}}return true;}}catch(err){{return String(err);}}}})()"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::cookie::SameSite;
    use crate::test_support::mock_pipe;
    use std::time::Duration;

    fn cookie(name: &str, secure: bool, same_site: SameSite) -> Cookie {
        Cookie {
            name: name.into(),
            value: "v".into(),
            domain: "example.com".into(),
            path: "/".into(),
            expires: -1.0,
            secure,
            http_only: true,
            same_site: Some(same_site),
            priority: None,
            source_scheme: None,
            source_port: None,
            partition_key: None,
            partition_key_opaque: false,
        }
    }

    #[tokio::test]
    async fn apply_sends_only_sanitized_cookies() {
        let (mut cdp, mut browser) = mock_pipe();
        let chrome = tokio::spawn(async move {
            let cmd = browser.expect("Storage.setCookies").await;
            let names: Vec<String> = cmd["params"]["cookies"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["name"].as_str().unwrap().to_owned())
                .collect();
            browser
                .write(&json!({ "id": cmd["id"], "result": {} }))
                .await;
            names
        });

        let jar = [
            cookie("good", true, SameSite::Lax),
            cookie("bad", false, SameSite::None), // None without secure -> dropped
        ];
        let mut client = CdpClient::new(&mut cdp);
        let report = apply_cookies(&mut client, &jar, 1000.0).await.unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.dropped.same_site_none_insecure, 1);
        assert_eq!(chrome.await.unwrap(), vec!["good".to_owned()]);
    }

    #[tokio::test]
    async fn apply_skips_the_command_entirely_when_nothing_survives() {
        let (mut cdp, mut browser) = mock_pipe();
        let jar = [cookie("bad", false, SameSite::None)];
        let mut client = CdpClient::new(&mut cdp);
        let report = apply_cookies(&mut client, &jar, 1000.0).await.unwrap();
        assert_eq!(report.applied, 0);
        assert!(
            !browser.received_within(Duration::from_millis(80)).await,
            "no setCookies command should be sent"
        );
    }

    #[tokio::test]
    async fn capture_parses_the_cookie_jar() {
        let (mut cdp, mut browser) = mock_pipe();
        let chrome = tokio::spawn(async move {
            let cmd = browser.expect("Storage.getCookies").await;
            browser
                .write(&json!({
                    "id": cmd["id"],
                    "result": { "cookies": [
                        { "name": "sid", "value": "x", "domain": "example.com", "path": "/",
                          "expires": -1, "secure": true, "httpOnly": true, "sameSite": "Strict" }
                    ] }
                }))
                .await;
        });

        let mut client = CdpClient::new(&mut cdp);
        let cookies = capture_cookies(&mut client).await.unwrap();
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].name, "sid");
        assert!(cookies[0].secure);
        assert_eq!(cookies[0].same_site, Some(SameSite::Strict));
        chrome.await.unwrap();
    }

    #[test]
    fn restore_script_json_escapes_values() {
        let script = restore_script(&[StorageEntry {
            name: "k".into(),
            value: "v\"x".into(),
        }]);
        assert!(script.contains("localStorage.clear()"));
        assert!(script.contains("localStorage.setItem"));
        assert!(script.contains(r#""name":"k""#));
        assert!(
            script.contains(r#"v\"x"#),
            "value with a quote is JSON-escaped"
        );
    }

    #[tokio::test]
    async fn apply_local_storage_empty_is_a_noop() {
        let (mut cdp, _browser) = mock_pipe();
        let mut client = CdpClient::new(&mut cdp);
        assert_eq!(
            apply_local_storage(&mut client, "SID1", &[]).await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn apply_local_storage_navigates_stub_and_writes_each_origin() {
        let (mut cdp, mut browser) = mock_pipe();
        let chrome = tokio::spawn(async move {
            browser.expect_ok("Page.enable", json!({})).await;
            browser.expect_ok("Fetch.enable", json!({})).await;
            browser.serve_stub_nav(Some("https://site.example/")).await;
            let cmd = browser.expect("Runtime.evaluate").await;
            assert_eq!(
                cmd["sessionId"], "SID1",
                "routed to the attached page target"
            );
            let expr = cmd["params"]["expression"].as_str().unwrap();
            assert!(expr.contains("localStorage.setItem"));
            assert!(expr.contains("token"), "the entry made it into the script");
            browser
                .write(&json!({ "id": cmd["id"], "result": { "result": { "value": true } } }))
                .await;
            browser.expect_ok("Fetch.disable", json!({})).await;
            browser.serve_blank_nav().await;
        });

        let origins = [OriginState {
            origin: "https://site.example".into(),
            local_storage: vec![StorageEntry {
                name: "token".into(),
                value: "abc".into(),
            }],
        }];
        let mut client = CdpClient::new(&mut cdp);
        let applied = apply_local_storage(&mut client, "SID1", &origins)
            .await
            .unwrap();
        assert_eq!(applied, 1);
        chrome.await.unwrap();
    }

    #[tokio::test]
    async fn attach_page_target_picks_the_page_and_returns_its_session() {
        let (mut cdp, mut browser) = mock_pipe();
        let chrome = tokio::spawn(async move {
            let cmd = browser.expect("Target.getTargets").await;
            browser
                .write(&json!({ "id": cmd["id"], "result": { "targetInfos": [
                    { "type": "browser", "targetId": "B" },
                    { "type": "page", "targetId": "P1" }
                ] } }))
                .await;
            let cmd = browser.expect("Target.attachToTarget").await;
            assert_eq!(cmd["params"]["targetId"], "P1");
            assert_eq!(cmd["params"]["flatten"], true);
            browser
                .write(&json!({ "id": cmd["id"], "result": { "sessionId": "S1" } }))
                .await;
        });

        let mut client = CdpClient::new(&mut cdp);
        assert_eq!(attach_page_target(&mut client).await.unwrap(), "S1");
        chrome.await.unwrap();
    }

    #[tokio::test]
    async fn capture_local_storage_reads_back_each_origin() {
        let (mut cdp, mut browser) = mock_pipe();
        let chrome = tokio::spawn(async move {
            browser.expect_ok("Page.enable", json!({})).await;
            browser.expect_ok("Fetch.enable", json!({})).await;
            browser.serve_stub_nav(Some("https://site.example/")).await;
            let cmd = browser.expect("Runtime.evaluate").await;
            browser
                .write(&json!({ "id": cmd["id"], "result": { "result": { "value": [
                    { "name": "token", "value": "abc" }
                ] } } }))
                .await;
            browser.expect_ok("Fetch.disable", json!({})).await;
            browser.serve_blank_nav().await;
        });

        let origins = vec!["https://site.example".to_owned()];
        let mut client = CdpClient::new(&mut cdp);
        let captured = capture_local_storage(&mut client, "SID1", &origins)
            .await
            .unwrap();
        assert_eq!(
            captured,
            vec![OriginState {
                origin: "https://site.example".into(),
                local_storage: vec![StorageEntry {
                    name: "token".into(),
                    value: "abc".into()
                }],
            }]
        );
        chrome.await.unwrap();
    }
}
