//! Probe and discovery handlers.

use crate::server::{AppState, auth};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::collections::HashMap;

/// Concrete query-map type shared by handlers (axum extractors need a
/// concrete hasher; generalizing over `BuildHasher` buys nothing here).
pub type QueryMap = HashMap<String, String>;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({ "error": "unauthorized" })),
    )
        .into_response()
}

pub(crate) fn check_auth(state: &AppState, query: &QueryMap, headers: &HeaderMap) -> bool {
    auth::authorized(
        state.token.as_deref(),
        query.get("token").map(String::as_str),
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    )
}

/// `GET /live`: the process is up.
pub async fn live() -> &'static str {
    "ok"
}

/// `GET /ready`: a session could be served right now.
pub async fn ready(State(state): State<Arc<AppState>>) -> Response {
    let pool_stats = state.pool.stats();
    let ready = pool_stats.accepting
        && (pool_stats.warm > 0 || pool_stats.running < pool_stats.max_sessions);
    let body = axum::Json(json!({ "ready": ready, "warm": pool_stats.warm }));
    if ready {
        (StatusCode::OK, body).into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
    }
}

fn pressure_reason(state: &AppState) -> (&'static str, f64, f64) {
    let (cpu, memory) = state.gauge.snapshot();
    let pool_stats = state.pool.stats();
    let reason = if !pool_stats.accepting {
        "draining"
    } else if pool_stats.running >= pool_stats.max_sessions
        && pool_stats.queued >= pool_stats.max_queue
    {
        "full"
    } else if cpu > state.pressure.max_cpu_percent {
        "cpu"
    } else if memory > state.pressure.max_memory_percent {
        "memory"
    } else {
        ""
    };
    (reason, cpu, memory)
}

/// `GET /pressure`: load and capacity in the industry-standard shape.
pub async fn pressure(State(state): State<Arc<AppState>>) -> Response {
    let pool_stats = state.pool.stats();
    let (reason, cpu, memory) = pressure_reason(&state);
    let date = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    axum::Json(json!({
        "running": pool_stats.running,
        "queued": pool_stats.queued,
        "warm": pool_stats.warm,
        "isAvailable": reason.is_empty(),
        "maxConcurrent": pool_stats.max_sessions,
        "maxQueued": pool_stats.max_queue,
        "cpu": cpu,
        "memory": memory,
        "reason": reason,
        "capacitySource": state.capacity_source,
        "isolation": state.tiers,
        "date": date,
    }))
    .into_response()
}

/// `GET /json/version` (and `/json/version/`): CDP discovery for Puppeteer's
/// `browserURL` and Playwright's `connectOverCDP`.
pub async fn json_version(
    State(state): State<Arc<AppState>>,
    Query(query): Query<QueryMap>,
    headers: HeaderMap,
) -> Response {
    if !check_auth(&state, &query, &headers) {
        return unauthorized();
    }
    let Some(version) = state.factory.cached_version() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({ "error": "warming up; no browser launched yet" })),
        )
            .into_response();
    };

    let base = state.external_address.clone().unwrap_or_else(|| {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("localhost:9222");
        format!("ws://{host}")
    });
    let mut ws_url = format!("{}/", base.trim_end_matches('/'));
    if let Some(token) = &state.token {
        ws_url.push_str("?token=");
        ws_url.push_str(token);
    }

    axum::Json(json!({
        "Browser": version.product,
        "Protocol-Version": version.protocol_version,
        "User-Agent": version.user_agent,
        "V8-Version": version.js_version,
        "webSocketDebuggerUrl": ws_url,
        "Browserserve-Version": env!("CARGO_PKG_VERSION"),
        "Browserserve-MaxConcurrent": state.pool.stats().max_sessions,
    }))
    .into_response()
}
