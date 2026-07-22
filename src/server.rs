//! The network front door: HTTP probes, CDP discovery, and the WS endpoint.

pub mod auth;
pub mod http;
pub mod ws;

use crate::config::{Loaded, PressureConfig};
use crate::factory::ChromeFactory;
use crate::pool::{Pool, PoolOptions};
use crate::pressure::PressureGauge;
use crate::{chrome, pressure, session_dirs};
use axum::Router;
use axum::routing::{any, get};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower_http::timeout::TimeoutLayer;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const FORCE_ABORT_BUDGET: Duration = Duration::from_secs(5);

/// Shared server state.
pub struct AppState {
    /// The warm pool serving sessions.
    pub pool: Pool<ChromeFactory>,
    /// Factory handle for the cached browser version.
    pub factory: ChromeFactory,
    /// Auth token; `None` disables auth.
    pub token: Option<String>,
    /// Host load gauge.
    pub gauge: Arc<PressureGauge>,
    /// Admission thresholds.
    pub pressure: PressureConfig,
    /// Advertised address for `webSocketDebuggerUrl`, when configured.
    pub external_address: Option<String>,
    /// Tracks live WS session tasks for drain.
    pub tracker: TaskTracker,
    /// Cancelled when the drain deadline expires.
    pub cancel: CancellationToken,
    /// WS message size cap, mirroring the CDP frame cap.
    pub max_message_bytes: usize,
    /// Resolved isolation tiers for this host.
    pub tiers: crate::linux::tiers::Tiers,
    /// Where the session ceiling came from: `config`, or the auto-capacity
    /// constraint that bound (`memory`, `pids`, `cpu`).
    pub capacity_source: &'static str,
}

/// Builds the router: probe routes carry an HTTP timeout; the WS route does
/// not (claims may legitimately queue longer).
pub fn router(state: Arc<AppState>) -> Router {
    let probes = Router::new()
        .route("/live", get(http::live))
        .route("/ready", get(http::ready))
        .route("/pressure", get(http::pressure))
        .route("/json/version", get(http::json_version))
        .route("/json/version/", get(http::json_version))
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            HTTP_TIMEOUT,
        ));
    Router::new()
        .route("/", any(ws::ws_handler))
        .merge(probes)
        .with_state(state)
}

/// Runs the server until SIGTERM/ctrl-c, then drains: intake stops first,
/// live sessions get `drainTimeoutMs`, stragglers are cancelled.
///
/// # Errors
///
/// Returns a human-readable message when startup preconditions fail (no
/// browser, unbindable address).
pub async fn serve(loaded: Loaded) -> Result<(), String> {
    let config = loaded.config;
    let executable =
        chrome::find_chrome(config.chrome.executable_path.as_deref()).map_err(|e| e.to_string())?;
    let swept = session_dirs::clean_stale(&config.data_dir)
        .await
        .map_err(|e| format!("stale session sweep failed: {e}"))?;
    if swept > 0 {
        tracing::info!(
            count = swept,
            "removed stale session dirs from a previous run"
        );
    }

    let tiers = crate::linux::probe::detect(&config.data_dir);
    tracing::info!(isolation = %tiers.summary(), "resolved isolation tiers");
    for note in &tiers.notes {
        tracing::info!("{note}");
    }

    let factory = ChromeFactory::new(&config, executable.clone(), tiers.clone());
    factory.prepare_template().await;
    let (max_sessions, capacity_source) = if let Some(explicit) = config.pool.max_sessions {
        (explicit, "config")
    } else {
        let limits = crate::capacity::probe_host();
        let cap = crate::capacity::compute(limits, factory.template_footprint());
        tracing::info!(
            max_sessions = cap.max_sessions,
            bound_by = cap.bound_by,
            mem_ceiling_bytes = limits.mem_ceiling_bytes,
            pids_max = limits.pids_max,
            cpus = limits.cpus,
            measured = factory.template_footprint().is_some(),
            "auto capacity resolved"
        );
        (cap.max_sessions, cap.bound_by)
    };
    let pool = Pool::new(
        factory.clone(),
        PoolOptions {
            min_ready: config.pool.min_ready as usize,
            max_sessions: max_sessions as usize,
            max_queue: config.pool.max_queue as usize,
            queue_timeout: Duration::from_millis(config.pool.queue_timeout_ms),
            warm_idle: Duration::from_millis(config.pool.warm_idle_ms),
            warm_max_age: Duration::from_millis(config.pool.warm_max_age_ms),
        },
    );

    let state = Arc::new(AppState {
        pool: pool.clone(),
        factory,
        token: loaded.serve.token,
        gauge: pressure::spawn_sampler(),
        pressure: config.pressure,
        external_address: config.external_address,
        tracker: TaskTracker::new(),
        cancel: CancellationToken::new(),
        max_message_bytes: config.chrome.max_frame_bytes,
        tiers,
        capacity_source,
    });

    let bind = format!("{}:{}", loaded.serve.host, loaded.serve.port);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| format!("cannot bind {bind}: {e}"))?;
    tracing::info!(address = %bind, chrome = %executable.display(), "browserserve listening");

    let shutdown_pool = pool.clone();
    axum::serve(listener, router(Arc::clone(&state)))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutdown signal received; refusing new sessions");
            shutdown_pool.close();
        })
        .await
        .map_err(|e| format!("server error: {e}"))?;

    state.tracker.close();
    let drain = Duration::from_millis(config.drain_timeout_ms);
    tokio::select! {
        () = state.tracker.wait() => {
            tracing::info!("all sessions drained cleanly");
        }
        () = tokio::time::sleep(drain) => {
            tracing::warn!(timeout_ms = config.drain_timeout_ms, "drain deadline hit; cancelling remaining sessions");
            state.cancel.cancel();
            let _ = tokio::time::timeout(FORCE_ABORT_BUDGET, state.tracker.wait()).await;
        }
    }
    if let Ok(swept) = session_dirs::clean_stale(&config.data_dir).await
        && swept > 0
    {
        tracing::info!(count = swept, "swept session dirs on shutdown");
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(signal) => signal,
        Err(e) => {
            tracing::error!(error = %e, "cannot install SIGTERM handler; ctrl-c only");
            let _ = ctrl_c.await;
            return;
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = sigterm.recv() => {}
    }
}
