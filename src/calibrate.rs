//! Boot-time capacity calibration: measure the host's real concurrent-session
//! ceiling by ramping actual recording sessions to the edge, rather than
//! trusting a constant.
//!
//! Runs once in the background after the port is bound, while the server reports
//! not-ready so no client sessions compete for the host. Sessions are brought up
//! one at a time (each loads a synthetic heavy page and runs a live screencast —
//! the heaviest real workload) until the first launch failure, a container-memory
//! safety line, or a hard step cap. Then every launched session is held together
//! under full concurrent load for a window matching a real client session, and the
//! ceiling is how many are STILL recording at the end — because a host can bring up
//! more sessions gradually than it can sustain once they all run at once. If the
//! step cap is reached with every session sustaining, the ceiling is extrapolated
//! from the measured per-session footprint, which is valid because browserserve
//! failure is resource-linear. The ramp only ever adds one session at a time and
//! backs off before pressure, and the hold launches nothing new, so calibration
//! cannot drive the host into an OOM.

use crate::capacity::{Capacity, HostLimits, SessionFootprint, compute};
use crate::chrome::{CdpClient, CdpPipe};
use crate::factory::{ChromeFactory, ChromeSession};
use crate::pool::SessionFactory;
use crate::rss::{tree_rss_bytes, tree_thread_count};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

/// Hard cap on how many sessions the ramp will actually launch. Bounds boot cost
/// on large hosts; above it the ceiling is extrapolated from the measured
/// footprint instead of launched.
const RAMP_CAP: u32 = 12;
/// Stop ramping when container memory crosses this fraction of the limit — the
/// kubelet-eviction analogue, so we detect the onset of pressure instead of
/// driving into an OOM.
const MEM_SAFETY_NUM: u64 = 85;
const MEM_SAFETY_DEN: u64 = 100;
/// Settle time after a session's page + screencast are up, before measuring.
const SETTLE: Duration = Duration::from_millis(1500);
/// How long to wait for the first screencast frame before treating the session
/// as failed to record.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
/// How long all launched sessions are held under full concurrent load before the
/// sustained-recording count is taken. Matches a real client's session window so
/// calibration measures the same sustained pressure a client would, catching the
/// starvation that a one-at-a-time ramp does not.
const SUSTAIN_HOLD: Duration = Duration::from_secs(12);
/// Window over which a held session must advance its frame counter to count as
/// still recording. Long enough that a healthy screencast produces several frames
/// and a stalled one produces none.
const RECORD_PROBE: Duration = Duration::from_millis(1500);
/// Per-session viewport for the calibration workload.
const VIEW_W: u32 = 1280;
const VIEW_H: u32 = 720;

/// Builds a heavy page (large DOM, several large canvases with retained backing
/// stores, and a retained buffer) and drives continuous canvas repaints so the
/// compositor keeps producing screencast frames. A synthetic stand-in for a real
/// recording workload, sized to a realistic heavy session so the measured
/// footprint is not artificially light, with no network dependency.
const WORKLOAD_SCRIPT: &str = "(()=>{const f=document.createDocumentFragment();for(let i=0;i<4000;i++){const d=document.createElement('div');d.textContent='row '+i+' lorem ipsum dolor sit amet consectetur adipiscing '.repeat(6);d.style.padding='3px';d.style.background='rgb('+(i%255)+',238,238)';f.appendChild(d);}document.body.appendChild(f);window.__cvs=[];for(let c=0;c<4;c++){const cv=document.createElement('canvas');cv.width=2048;cv.height=2048;const x=cv.getContext('2d');x.fillRect(0,0,2048,2048);document.body.appendChild(cv);window.__cvs.push(x);}window.__buf=new Uint8Array(32*1024*1024).fill(1);let n=0;setInterval(()=>{n++;for(const x of window.__cvs){x.fillStyle='rgb('+(n%255)+',0,'+((n*3)%255)+')';x.fillRect((n*11)%1600,(n*7)%1600,420,420);}document.body.style.background='rgb('+(n%255)+',0,0)';},60);return 'ok';})()";

/// Why the ramp stopped adding sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RampStop {
    /// A session failed to launch or record at this step.
    LaunchFailed,
    /// Container memory crossed the safety line at this step.
    MemoryLine,
    /// The hard step cap was reached with every session launching.
    Cap,
}

/// Derives the final ceiling from the sustained-recording verification.
/// Pure so it is unit-testable without launching browsers.
///
/// `sustained` is how many sessions were still recording after being held under
/// full concurrent load — the honest ceiling. `clean_cap` is true only when the
/// ramp reached its step cap AND every launched session sustained recording: the
/// host clearly holds at least that many and probably more, so the ceiling is the
/// resource-model number from the measured footprint (never below what sustained).
/// Otherwise the ceiling is exactly what sustained.
#[must_use]
pub fn derive_ceiling(
    sustained: u32,
    clean_cap: bool,
    footprint: Option<SessionFootprint>,
    limits: HostLimits,
) -> Capacity {
    if clean_cap {
        let modeled = compute(limits, footprint);
        Capacity {
            max_sessions: modeled.max_sessions.max(sustained.max(1)),
            bound_by: modeled.bound_by,
        }
    } else {
        Capacity {
            max_sessions: sustained.max(1),
            bound_by: "measured",
        }
    }
}

/// A live recording session held open during calibration. Its background task
/// acks screencast frames so the browser keeps producing them (an unacked stream
/// stalls) and counts them, so the sustained-load hold can tell which sessions are
/// still recording. Dropping cancels the task; the session is torn down via the
/// factory.
struct HeldSession {
    session: ChromeSession,
    cancel: CancellationToken,
    frames: Arc<AtomicU64>,
}

/// Ramps real recording sessions to the edge and returns the measured ceiling.
/// Launches through the factory directly (not the pool), so it measures what the
/// host can hold, not what the pool currently allows.
pub async fn calibrate(factory: &ChromeFactory, limits: HostLimits) -> Capacity {
    let mut held: Vec<HeldSession> = Vec::new();
    let mut footprint: Option<SessionFootprint> = None;
    let mut stop = RampStop::Cap;

    for n in 1..=RAMP_CAP {
        match spawn_recording_session(factory).await {
            Ok(h) => held.push(h),
            Err(e) => {
                tracing::warn!(at = n, error = %e, "calibration: session failed; ramp edge reached");
                stop = RampStop::LaunchFailed;
                break;
            }
        }
        sleep(SETTLE).await;

        if let Some(last) = held.last()
            && let Some(fp) = measure_footprint(last.session.browser.pid)
        {
            footprint = Some(footprint.map_or(fp, |prev| max_footprint(prev, fp)));
        }

        if crossed_memory_line(limits.mem_ceiling_bytes) {
            tracing::info!(at = n, "calibration: memory safety line reached");
            stop = RampStop::MemoryLine;
            break;
        }
    }

    // Gradual fit is not sustained fit: a host can bring up N one at a time yet
    // starve some of them once all N run together under load. Hold everything
    // under full concurrent load for a window matching a real client session,
    // then count how many are STILL recording — that is the honest ceiling. No
    // new sessions launch here, so this can never drive the host into an OOM.
    let launched = u32::try_from(held.len()).unwrap_or(u32::MAX);
    sleep(SUSTAIN_HOLD).await;
    let sustained = count_recording(&held).await;

    for h in held {
        h.cancel.cancel();
        factory.destroy(h.session).await;
    }

    let clean_cap = stop == RampStop::Cap && sustained == launched;
    let cap = derive_ceiling(sustained, clean_cap, footprint, limits);
    tracing::info!(
        max_sessions = cap.max_sessions,
        bound_by = cap.bound_by,
        launched,
        sustained,
        stop = ?stop,
        "calibration complete"
    );
    cap
}

/// Holds for [`RECORD_PROBE`] and counts how many of the held sessions advanced
/// their frame counter over that window — i.e. are still actively recording under
/// the current concurrent load. Sessions whose renderer stalled or was killed
/// under memory pressure produce no frames and are not counted.
async fn count_recording(held: &[HeldSession]) -> u32 {
    let before: Vec<u64> = held
        .iter()
        .map(|h| h.frames.load(Ordering::Relaxed))
        .collect();
    sleep(RECORD_PROBE).await;
    let still = held
        .iter()
        .zip(&before)
        .filter(|(h, b)| h.frames.load(Ordering::Relaxed) > **b)
        .count();
    u32::try_from(still).unwrap_or(u32::MAX)
}

async fn spawn_recording_session(factory: &ChromeFactory) -> Result<HeldSession, String> {
    let mut session = factory.create().await?;
    let Some(mut pipe) = session.browser.take_pipe() else {
        factory.destroy(session).await;
        return Err("session had no CDP transport".to_owned());
    };
    if let Err(e) = setup_recording(&mut pipe).await {
        factory.destroy(session).await;
        return Err(e);
    }
    let cancel = CancellationToken::new();
    let frames = Arc::new(AtomicU64::new(0));
    tokio::spawn(drain_frames(pipe, cancel.clone(), frames.clone()));
    Ok(HeldSession {
        session,
        cancel,
        frames,
    })
}

/// Opens a page, loads the workload, and starts a screencast, returning once the
/// first frame confirms the session is really recording.
async fn setup_recording(pipe: &mut CdpPipe) -> Result<(), String> {
    let mut client = CdpClient::new(pipe);
    let created = client
        .call("Target.createTarget", json!({ "url": "about:blank" }))
        .await
        .map_err(|e| e.to_string())?;
    let target_id = created
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or("createTarget returned no targetId")?
        .to_owned();
    let attached = client
        .call(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await
        .map_err(|e| e.to_string())?;
    let sid = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or("attach returned no sessionId")?
        .to_owned();
    let s = Some(sid.as_str());
    for (method, params) in [
        ("Page.enable", json!({})),
        ("Runtime.enable", json!({})),
        (
            "Page.setDeviceMetricsOverride",
            json!({ "width": VIEW_W, "height": VIEW_H, "deviceScaleFactor": 1, "mobile": false }),
        ),
        (
            "Runtime.evaluate",
            json!({ "expression": WORKLOAD_SCRIPT, "returnByValue": true }),
        ),
        (
            "Page.startScreencast",
            json!({ "format": "jpeg", "quality": 60, "maxWidth": VIEW_W, "maxHeight": VIEW_H, "everyNthFrame": 4 }),
        ),
    ] {
        client
            .call_on(s, method, params)
            .await
            .map_err(|e| format!("{method}: {e}"))?;
    }
    wait_first_frame(&mut client).await
}

/// Acks screencast frames until cancelled or the browser closes the pipe, so the
/// held session keeps producing frames (an unacked stream stalls), and counts each
/// frame so the ramp can tell whether the session is still actively recording.
async fn drain_frames(mut pipe: CdpPipe, cancel: CancellationToken, frames: Arc<AtomicU64>) {
    let mut client = CdpClient::new(&mut pipe);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            msg = client.recv() => match msg {
                Ok(m) if is_screencast_frame(&m) => {
                    frames.fetch_add(1, Ordering::Relaxed);
                    if let Some(sid) = frame_session_id(&m) {
                        let _ = client
                            .call_on(Some(&sid), "Page.screencastFrameAck", json!({ "sessionId": frame_seq(&m) }))
                            .await;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
}

async fn wait_first_frame(client: &mut CdpClient<'_>) -> Result<(), String> {
    let wait = async {
        loop {
            match client.recv().await {
                Ok(m) if is_screencast_frame(&m) => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    };
    match timeout(FIRST_FRAME_TIMEOUT, wait).await {
        Ok(r) => r,
        Err(_) => Err("no screencast frame within timeout".to_owned()),
    }
}

fn is_screencast_frame(msg: &Value) -> bool {
    msg.get("method").and_then(Value::as_str) == Some("Page.screencastFrame")
}

fn frame_session_id(msg: &Value) -> Option<String> {
    msg.get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn frame_seq(msg: &Value) -> i64 {
    msg.get("params")
        .and_then(|p| p.get("sessionId"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

fn measure_footprint(pid: i32) -> Option<SessionFootprint> {
    let bytes = tree_rss_bytes(pid)?;
    let threads = tree_thread_count(pid)?;
    (bytes > 0 && threads > 0).then_some(SessionFootprint { bytes, threads })
}

fn max_footprint(a: SessionFootprint, b: SessionFootprint) -> SessionFootprint {
    SessionFootprint {
        bytes: a.bytes.max(b.bytes),
        threads: a.threads.max(b.threads),
    }
}

fn crossed_memory_line(ceiling: Option<u64>) -> bool {
    let (Some(ceiling), Some(used)) = (ceiling, current_memory_used_bytes()) else {
        return false;
    };
    used >= ceiling / MEM_SAFETY_DEN * MEM_SAFETY_NUM
}

#[cfg(target_os = "linux")]
fn current_memory_used_bytes() -> Option<u64> {
    if let Ok(raw) = std::fs::read_to_string("/sys/fs/cgroup/memory.current")
        && let Ok(v) = raw.trim().parse::<u64>()
    {
        return Some(v);
    }
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb = |key: &str| -> Option<u64> {
        raw.lines()
            .find(|l| l.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
    };
    let total = kb("MemTotal:")?;
    let available = kb("MemAvailable:")?;
    Some(total.saturating_sub(available).saturating_mul(1024))
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)] // signature parity with the fallible Linux variant
fn current_memory_used_bytes() -> Option<u64> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    Some(system.used_memory())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(mem_gb: Option<u64>, pids: Option<u64>, cpus: u32) -> HostLimits {
        HostLimits {
            mem_ceiling_bytes: mem_gb.map(|g| g * 1024 * 1024 * 1024),
            pids_max: pids,
            cpus,
        }
    }

    const FP: SessionFootprint = SessionFootprint {
        bytes: 700 * 1024 * 1024,
        threads: 160,
    };

    #[test]
    fn not_clean_cap_uses_exactly_sustained() {
        // Ramp stopped at an edge (not a clean cap): the ceiling is exactly what
        // sustained, even on a big-memory box the model would rate higher.
        let cap = derive_ceiling(5, false, Some(FP), limits(Some(64), None, 32));
        assert_eq!(cap.max_sessions, 5);
        assert_eq!(cap.bound_by, "measured");
    }

    #[test]
    fn starvation_drops_below_gradual_fit() {
        // The VPS case: 5 launched gradually but only 3 sustained recording under
        // load. The ceiling is 3, never the modeled or launched number.
        let cap = derive_ceiling(3, false, Some(FP), limits(Some(64), None, 32));
        assert_eq!(cap.max_sessions, 3);
        assert_eq!(cap.bound_by, "measured");
    }

    #[test]
    fn clean_cap_extrapolates_from_footprint() {
        // Big host, all 12 sustained at the cap: extrapolate from footprint.
        let cap = derive_ceiling(12, true, Some(FP), limits(Some(64), None, 32));
        assert!(cap.max_sessions > 12, "extrapolated above the tested cap");
    }

    #[test]
    fn clean_cap_never_reports_below_what_sustained() {
        // Model would say less, but 12 sustained cleanly — never report under that.
        let tiny = SessionFootprint {
            bytes: 2 * 1024 * 1024 * 1024,
            threads: 500,
        };
        let cap = derive_ceiling(12, true, Some(tiny), limits(Some(8), Some(1000), 8));
        assert_eq!(cap.max_sessions, 12);
    }

    #[test]
    fn floors_at_one() {
        let cap = derive_ceiling(0, false, None, limits(Some(1), None, 1));
        assert_eq!(cap.max_sessions, 1);
    }
}
