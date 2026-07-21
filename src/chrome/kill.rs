//! Teardown: protocol close, then signal escalation on the process group.

use crate::chrome::launch::Browser;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::json;
use std::time::Duration;
use tokio::time::timeout;

const PROTOCOL_CLOSE_BUDGET: Duration = Duration::from_secs(2);
const REAP_BUDGET: Duration = Duration::from_secs(5);

/// What happened during one browser teardown.
#[derive(Debug, Clone)]
pub struct TeardownReport {
    /// Final exit status, when the child was reaped in time.
    pub exit_status: Option<String>,
    /// The browser exited from the protocol close alone.
    pub graceful: bool,
    /// SIGKILL was required after the SIGTERM grace period.
    pub escalated_sigkill: bool,
    /// The direct child was reaped (`wait` returned).
    pub reaped: bool,
}

/// Tears down a browser: `Browser.close` over CDP first, then SIGTERM to the
/// process group, then SIGKILL. Always attempts to reap the direct child. The
/// caller owns the profile directory and removes it afterwards.
pub async fn teardown(browser: Browser, grace: Duration) -> TeardownReport {
    let Browser {
        mut child,
        pipe,
        pid,
        ..
    } = browser;
    let mut report = TeardownReport {
        exit_status: None,
        graceful: false,
        escalated_sigkill: false,
        reaped: false,
    };

    if let Some(mut pipe) = pipe {
        let _ = pipe
            .send(&json!({ "id": 0, "method": "Browser.close" }))
            .await;
        if let Ok(status) = timeout(PROTOCOL_CLOSE_BUDGET, child.wait()).await {
            report.graceful = true;
            report.reaped = true;
            report.exit_status = status.ok().map(|s| s.to_string());
            term_group(pid);
            return report;
        }
    }

    term_group(pid);
    if let Ok(status) = timeout(grace, child.wait()).await {
        report.reaped = true;
        report.exit_status = status.ok().map(|s| s.to_string());
        return report;
    }

    force_kill_group(pid);
    report.escalated_sigkill = true;
    if let Ok(status) = timeout(REAP_BUDGET, child.wait()).await {
        report.reaped = true;
        report.exit_status = status.ok().map(|s| s.to_string());
    }
    debug_assert_group_dead(pid);
    report
}

pub(crate) fn term_group(pid: i32) {
    if pid > 1 {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGTERM);
    }
}

/// SIGKILLs the whole process group led by `pid`. No-op for pid ≤ 1.
pub fn force_kill_group(pid: i32) {
    if pid > 1 {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
}

fn debug_assert_group_dead(pid: i32) {
    if cfg!(debug_assertions)
        && pid > 1
        && nix::sys::signal::kill(Pid::from_raw(-pid), None).is_ok()
    {
        tracing::warn!(pid, "process group still alive immediately after teardown");
    }
}
