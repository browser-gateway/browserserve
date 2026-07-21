//! Browser launch: spawn with CDP pipe fds, wait for protocol readiness.

use crate::chrome::flags::{FlagOptions, build_flags};
use crate::chrome::kill::force_kill_group;
use crate::chrome::pipe::{CdpPipe, PipeError};
use command_fds::{CommandFdExt, FdMapping};
use serde_json::json;
use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, LinesCodec};

const READY_PROBE_ID: u64 = 1;
const STDERR_MAX_LINE_BYTES: usize = 8 * 1024;
const STDERR_MAX_LINES: usize = 200;
const ABORT_REAP_BUDGET: Duration = Duration::from_secs(5);

/// Errors from launching a browser.
#[derive(Debug, Error)]
pub enum LaunchError {
    /// Creating the CDP pipe pair failed.
    #[error("failed to create CDP pipes: {0}")]
    Pipes(#[source] nix::Error),
    /// Mapping the pipe fds onto the child failed.
    #[error("failed to map CDP pipe fds onto the child")]
    FdMapping,
    /// The browser process could not be spawned.
    #[error("failed to spawn the browser: {0}")]
    Spawn(#[source] std::io::Error),
    /// The spawned child reported no pid.
    #[error("browser pid unavailable after spawn")]
    PidUnavailable,
    /// Wrapping the runtime-side pipe ends for async I/O failed.
    #[error("failed to wire the CDP pipe: {0}")]
    Wire(#[source] std::io::Error),
    /// The browser exited before becoming CDP-ready.
    #[error("browser exited during startup ({status}); stderr tail:\n{stderr_tail}")]
    ChromeExited {
        /// Exit status description.
        status: String,
        /// Most recent stderr lines at the time of failure.
        stderr_tail: String,
    },
    /// The CDP pipe failed before readiness.
    #[error("CDP pipe failed during startup: {source}; stderr tail:\n{stderr_tail}")]
    StartupPipe {
        /// The pipe failure.
        #[source]
        source: PipeError,
        /// Most recent stderr lines at the time of failure.
        stderr_tail: String,
    },
    /// The browser did not answer CDP within the launch timeout.
    #[error("browser not CDP-ready within {timeout_ms} ms; stderr tail:\n{stderr_tail}")]
    ReadyTimeout {
        /// The configured launch timeout in milliseconds.
        timeout_ms: u64,
        /// Most recent stderr lines at the time of failure.
        stderr_tail: String,
    },
}

impl LaunchError {
    /// A remediation hint for known failure signatures, when one applies.
    ///
    /// Derived from the browser's stderr tail; returns `None` when the failure
    /// has no known one-line fix.
    #[must_use]
    pub fn remediation(&self) -> Option<&'static str> {
        let (Self::ChromeExited { stderr_tail, .. }
        | Self::StartupPipe { stderr_tail, .. }
        | Self::ReadyTimeout { stderr_tail, .. }) = self
        else {
            return None;
        };
        if stderr_tail.contains("No usable sandbox") {
            return Some(SANDBOX_REMEDIATION);
        }
        None
    }
}

const SANDBOX_REMEDIATION: &str = "\
the container blocks the system calls Chromium's sandbox needs. Two ways forward:
  recommended: run the container with our seccomp profile, which keeps the sandbox ON:
    docker run --security-opt seccomp=seccomp.json ...
  fallback: explicitly disable the sandbox in config (chrome.noSandbox: true)
  details: https://github.com/browser-gateway/browserserve#sandbox";

/// Version facts the browser reported when it became ready.
#[derive(Debug, Clone, Default)]
pub struct BrowserVersion {
    /// Product string, e.g. `Chrome/149.0.7827.155`.
    pub product: String,
    /// CDP protocol version, e.g. `1.3`.
    pub protocol_version: String,
    /// Default user agent.
    pub user_agent: String,
    /// JavaScript engine version.
    pub js_version: String,
}

/// Bounded ring of the browser's most recent stderr lines.
#[derive(Debug, Clone, Default)]
pub struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    fn push(&self, line: String) {
        let mut lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        lines.push_back(line);
        while lines.len() > STDERR_MAX_LINES {
            lines.pop_front();
        }
    }

    /// Returns the retained lines, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    fn joined(&self) -> String {
        self.snapshot().join("\n")
    }
}

/// Inputs for one browser launch.
#[derive(Debug)]
pub struct LaunchSpec<'a> {
    /// Resolved browser executable.
    pub executable: &'a Path,
    /// Fresh, session-owned profile directory.
    pub user_data_dir: &'a Path,
    /// Launch with `--no-sandbox`.
    pub no_sandbox: bool,
    /// Extra flags appended after the built-in set.
    pub extra_flags: &'a [String],
    /// Budget for the browser to become CDP-ready.
    pub launch_timeout: Duration,
    /// Cap on a single CDP message.
    pub max_frame_bytes: usize,
}

/// A launched, CDP-ready browser process.
pub struct Browser {
    pub(crate) child: Child,
    pub(crate) pipe: Option<CdpPipe>,
    /// Process id; also the process group id (spawned as group leader).
    pub pid: i32,
    /// Stderr tail collector, live for the browser's lifetime.
    pub stderr: StderrTail,
    /// Version facts reported at readiness.
    pub version: BrowserVersion,
    /// When the process was spawned.
    pub launched_at: Instant,
    /// Time from spawn to a CDP response.
    pub ready_in: Duration,
}

impl Browser {
    /// Takes the CDP pipe for bridging. After this, teardown skips the
    /// protocol-close step and goes straight to signals.
    pub fn take_pipe(&mut self) -> Option<CdpPipe> {
        self.pipe.take()
    }

    /// Cheap liveness check; reaps the child if it already exited.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// Launches a browser and waits until it answers CDP over the pipe.
///
/// On any failure the spawned process group is killed and the direct child
/// reaped before the error is returned. The caller owns the profile directory
/// and its cleanup.
///
/// # Errors
///
/// Any [`LaunchError`] variant; readiness failures
/// ([`LaunchError::ChromeExited`], [`LaunchError::StartupPipe`],
/// [`LaunchError::ReadyTimeout`]) carry the browser's stderr tail.
pub async fn launch(spec: &LaunchSpec<'_>) -> Result<Browser, LaunchError> {
    let (browser_cmd_read, cmd_write) = cloexec_pipe().map_err(LaunchError::Pipes)?;
    let (out_read, browser_out_write) = cloexec_pipe().map_err(LaunchError::Pipes)?;

    let args = build_flags(&FlagOptions {
        user_data_dir: spec.user_data_dir,
        no_sandbox: spec.no_sandbox,
        extra: spec.extra_flags,
    });
    let mut command = Command::new(spec.executable);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    command
        .fd_mappings(vec![
            FdMapping {
                parent_fd: browser_cmd_read,
                child_fd: 3,
            },
            FdMapping {
                parent_fd: browser_out_write,
                child_fd: 4,
            },
        ])
        .map_err(|_| LaunchError::FdMapping)?;

    let launched_at = Instant::now();
    let mut child = command.spawn().map_err(LaunchError::Spawn)?;
    let pid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .ok_or(LaunchError::PidUnavailable)?;

    let stderr_tail = StderrTail::default();
    if let Some(stderr) = child.stderr.take() {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = FramedRead::new(
                stderr,
                LinesCodec::new_with_max_length(STDERR_MAX_LINE_BYTES),
            );
            while let Some(Ok(line)) = lines.next().await {
                tail.push(line);
            }
        });
    }

    let mut cdp = match wire_pipe(cmd_write, out_read, spec.max_frame_bytes) {
        Ok(cdp) => cdp,
        Err(e) => {
            abort(pid, &mut child).await;
            return Err(e);
        }
    };

    match timeout(spec.launch_timeout, probe_ready(&mut cdp, &mut child)).await {
        Ok(Ok(version)) => Ok(Browser {
            child,
            pipe: Some(cdp),
            pid,
            stderr: stderr_tail,
            version,
            launched_at,
            ready_in: launched_at.elapsed(),
        }),
        Ok(Err(ProbeFailure::Exited(status))) => {
            abort(pid, &mut child).await;
            Err(LaunchError::ChromeExited {
                status,
                stderr_tail: stderr_tail.joined(),
            })
        }
        Ok(Err(ProbeFailure::Pipe(source))) => {
            abort(pid, &mut child).await;
            Err(LaunchError::StartupPipe {
                source,
                stderr_tail: stderr_tail.joined(),
            })
        }
        Err(_elapsed) => {
            abort(pid, &mut child).await;
            Err(LaunchError::ReadyTimeout {
                timeout_ms: u64::try_from(spec.launch_timeout.as_millis()).unwrap_or(u64::MAX),
                stderr_tail: stderr_tail.joined(),
            })
        }
    }
}

#[cfg(target_os = "linux")]
fn cloexec_pipe() -> nix::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)
}

// macOS lacks the atomic pipe2 syscall; the two-step fallback is dev-only.
#[cfg(target_os = "macos")]
fn cloexec_pipe() -> nix::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    let (read_end, write_end) = nix::unistd::pipe()?;
    fcntl(&read_end, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    fcntl(&write_end, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    Ok((read_end, write_end))
}

fn wire_pipe(
    cmd_write: std::os::fd::OwnedFd,
    out_read: std::os::fd::OwnedFd,
    max_frame: usize,
) -> Result<CdpPipe, LaunchError> {
    let writer = pipe::Sender::from_owned_fd(cmd_write).map_err(LaunchError::Wire)?;
    let reader = pipe::Receiver::from_owned_fd(out_read).map_err(LaunchError::Wire)?;
    Ok(CdpPipe::new(writer, reader, max_frame))
}

enum ProbeFailure {
    Exited(String),
    Pipe(PipeError),
}

async fn probe_ready(cdp: &mut CdpPipe, child: &mut Child) -> Result<BrowserVersion, ProbeFailure> {
    let probe = json!({ "id": READY_PROBE_ID, "method": "Browser.getVersion" });
    if let Err(e) = cdp.send(&probe).await {
        return Err(ProbeFailure::Pipe(e));
    }
    loop {
        tokio::select! {
            status = child.wait() => {
                let text = status.map_or_else(|e| format!("wait failed: {e}"), |s| s.to_string());
                return Err(ProbeFailure::Exited(text));
            }
            message = cdp.recv() => match message {
                Ok(value) => {
                    if value.get("id").and_then(serde_json::Value::as_u64) == Some(READY_PROBE_ID) {
                        return Ok(parse_version(&value));
                    }
                }
                Err(e) => return Err(ProbeFailure::Pipe(e)),
            },
        }
    }
}

fn parse_version(value: &serde_json::Value) -> BrowserVersion {
    let field = |name: &str| {
        value
            .pointer(&format!("/result/{name}"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    BrowserVersion {
        product: field("product"),
        protocol_version: field("protocolVersion"),
        user_agent: field("userAgent"),
        js_version: field("jsVersion"),
    }
}

async fn abort(pid: i32, child: &mut Child) {
    force_kill_group(pid);
    let _ = timeout(ABORT_REAP_BUDGET, child.wait()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_extracts_known_fields() {
        let value = json!({
            "id": 1,
            "result": {
                "product": "Chrome/149.0.0.0",
                "protocolVersion": "1.3",
                "userAgent": "Mozilla/5.0",
                "jsVersion": "14.9.1"
            }
        });
        let version = parse_version(&value);
        assert_eq!(version.product, "Chrome/149.0.0.0");
        assert_eq!(version.protocol_version, "1.3");
        assert_eq!(version.user_agent, "Mozilla/5.0");
        assert_eq!(version.js_version, "14.9.1");
    }

    #[test]
    fn parse_version_tolerates_missing_fields() {
        let version = parse_version(&json!({ "id": 1, "result": {} }));
        assert_eq!(version.product, "");
    }

    #[test]
    fn sandbox_failure_gets_remediation() {
        let err = LaunchError::ChromeExited {
            status: String::from("signal: 6 (SIGABRT)"),
            stderr_tail: String::from("[23:23] FATAL:zygote_host_impl_linux.cc No usable sandbox!"),
        };
        let hint = err.remediation().unwrap();
        assert!(hint.contains("seccomp"));
        assert!(hint.contains("chrome.noSandbox"));
        assert!(hint.contains("#sandbox"));
    }

    #[test]
    fn unrelated_failures_get_no_remediation() {
        let exited = LaunchError::ChromeExited {
            status: String::from("exit status: 1"),
            stderr_tail: String::from("some other failure"),
        };
        assert!(exited.remediation().is_none());
        assert!(LaunchError::PidUnavailable.remediation().is_none());
    }

    #[test]
    fn stderr_tail_is_bounded() {
        let tail = StderrTail::default();
        for i in 0..(STDERR_MAX_LINES + 50) {
            tail.push(format!("line {i}"));
        }
        let lines = tail.snapshot();
        assert_eq!(lines.len(), STDERR_MAX_LINES);
        assert_eq!(lines.first().map(String::as_str), Some("line 50"));
    }
}
