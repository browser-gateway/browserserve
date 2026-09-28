//! Probe and discovery handlers.

use crate::profile::payload::ProfilePayload;
use crate::server::profiles::PROFILE_TTL;
use crate::server::{AppState, auth};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
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
    let calibrating = state.calibrating.load(std::sync::atomic::Ordering::Relaxed);
    let spent = state
        .single_use
        .as_ref()
        .is_some_and(crate::server::single_use::SingleUse::is_spent);
    let ready = !calibrating
        && !spent
        && !state.factory.sandbox_required_but_unavailable()
        && pool_stats.accepting
        && (pool_stats.warm > 0 || pool_stats.running < pool_stats.max_sessions);
    let body = axum::Json(json!({
        "ready": ready,
        "warm": pool_stats.warm,
        "calibrating": calibrating,
        "spent": spent,
        "sandbox": state.factory.sandbox_state(),
    }));
    if ready {
        (StatusCode::OK, body).into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
    }
}

/// Instance-level states that refuse every new session, in precedence order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Gate {
    #[default]
    Open,
    SandboxBlocked,
    Spent,
    Draining,
    Calibrating,
}

/// Everything that decides whether `/pressure` reports the instance available.
#[derive(Debug, Clone, Copy, Default)]
struct PressureInputs {
    gate: Gate,
    running: usize,
    max_sessions: usize,
    queued: usize,
    max_queue: usize,
    cpu: f64,
    memory: f64,
    max_cpu: f64,
    max_memory: f64,
}

/// The reason a new session would be refused right now, or `""` when it would
/// be accepted.
fn unavailable_reason(inputs: PressureInputs) -> &'static str {
    match inputs.gate {
        Gate::SandboxBlocked => "sandbox",
        Gate::Spent => "spent",
        Gate::Draining => "draining",
        Gate::Calibrating => "calibrating",
        Gate::Open
            if inputs.running >= inputs.max_sessions && inputs.queued >= inputs.max_queue =>
        {
            "full"
        }
        Gate::Open if inputs.cpu > inputs.max_cpu => "cpu",
        Gate::Open if inputs.memory > inputs.max_memory => "memory",
        Gate::Open => "",
    }
}

fn gate(state: &AppState, accepting: bool) -> Gate {
    if state.factory.sandbox_required_but_unavailable() {
        Gate::SandboxBlocked
    } else if state
        .single_use
        .as_ref()
        .is_some_and(crate::server::single_use::SingleUse::is_spent)
    {
        Gate::Spent
    } else if !accepting {
        Gate::Draining
    } else if state.calibrating.load(std::sync::atomic::Ordering::Relaxed) {
        Gate::Calibrating
    } else {
        Gate::Open
    }
}

fn pressure_reason(state: &AppState) -> (&'static str, f64, f64) {
    let (cpu, memory) = state.gauge.snapshot();
    let pool_stats = state.pool.stats();
    let reason = unavailable_reason(PressureInputs {
        gate: gate(state, pool_stats.accepting),
        running: pool_stats.running,
        max_sessions: pool_stats.max_sessions,
        queued: pool_stats.queued,
        max_queue: pool_stats.max_queue,
        cpu,
        memory,
        max_cpu: state.pressure.max_cpu_percent,
        max_memory: state.pressure.max_memory_percent,
    });
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
        "sandbox": state.factory.sandbox_state(),
        "idleTimeoutMs": state.idle_timeout.map_or(0, |d| d.as_millis()),
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
    // Best-effort: the Chrome version fields fill in after the first launch, but
    // discovery must answer 200 the moment the server is bound so a client can
    // connect (the WS connect drives an on-demand launch) and so "starting up" is
    // never confused with "hung". The webSocketDebuggerUrl and Browserserve-*
    // headers are known without launching a browser.
    let version = state.factory.cached_version().unwrap_or_default();

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
        "Browserserve-Calibrating": state.calibrating.load(std::sync::atomic::Ordering::Relaxed),
    }))
    .into_response()
}

/// `POST /v1/profile`: hand off a profile for a coming session and receive a
/// one-shot token. Bearer-authed; the body size is capped by a route layer.
pub async fn profile_drop_off(
    State(state): State<Arc<AppState>>,
    Query(query): Query<QueryMap>,
    headers: HeaderMap,
    payload: Result<axum::Json<ProfilePayload>, JsonRejection>,
) -> Response {
    if !check_auth(&state, &query, &headers) {
        return unauthorized();
    }
    let Ok(axum::Json(payload)) = payload else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "invalid profile body" })),
        )
            .into_response();
    };
    match state.profiles.drop_off(payload) {
        Some(token) => axum::Json(json!({
            "profileToken": token,
            "expiresInSec": PROFILE_TTL.as_secs(),
        }))
        .into_response(),
        None => (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(json!({ "error": "profile store at capacity" })),
        )
            .into_response(),
    }
}

/// `GET /v1/profile/{token}`: pick up the captured profile once the session has
/// finished. Bearer-authed; single-use, returns 404 until the profile is ready.
pub async fn profile_pick_up(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
    Query(query): Query<QueryMap>,
    headers: HeaderMap,
) -> Response {
    if !check_auth(&state, &query, &headers) {
        return unauthorized();
    }
    match state.profiles.pick_up(&token) {
        Some(payload) => axum::Json(payload).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "no captured profile for token" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Gate, PressureInputs, unavailable_reason};

    fn healthy() -> PressureInputs {
        PressureInputs {
            max_sessions: 4,
            max_queue: 10,
            max_cpu: 95.0,
            max_memory: 95.0,
            ..PressureInputs::default()
        }
    }

    fn with_gate(gate: Gate) -> PressureInputs {
        PressureInputs { gate, ..healthy() }
    }

    #[test]
    fn healthy_instance_is_available() {
        assert_eq!(unavailable_reason(healthy()), "");
    }

    #[test]
    fn calibration_makes_it_unavailable() {
        assert_eq!(
            unavailable_reason(with_gate(Gate::Calibrating)),
            "calibrating"
        );
    }

    #[test]
    fn a_spent_single_use_instance_is_unavailable() {
        assert_eq!(unavailable_reason(with_gate(Gate::Spent)), "spent");
    }

    #[test]
    fn each_other_reason_is_reported() {
        assert_eq!(
            unavailable_reason(with_gate(Gate::SandboxBlocked)),
            "sandbox"
        );
        assert_eq!(unavailable_reason(with_gate(Gate::Draining)), "draining");
        assert_eq!(
            unavailable_reason(PressureInputs {
                running: 4,
                queued: 10,
                ..healthy()
            }),
            "full"
        );
        assert_eq!(
            unavailable_reason(PressureInputs {
                cpu: 99.0,
                ..healthy()
            }),
            "cpu"
        );
        assert_eq!(
            unavailable_reason(PressureInputs {
                memory: 99.0,
                ..healthy()
            }),
            "memory"
        );
    }

    #[test]
    fn a_full_pool_with_queue_room_is_still_available() {
        let inputs = PressureInputs {
            running: 4,
            queued: 3,
            ..healthy()
        };
        assert_eq!(unavailable_reason(inputs), "");
    }

    #[test]
    fn a_gate_outranks_load() {
        let inputs = PressureInputs {
            gate: Gate::Calibrating,
            cpu: 99.0,
            running: 4,
            queued: 10,
            ..healthy()
        };
        assert_eq!(unavailable_reason(inputs), "calibrating");
    }
}
