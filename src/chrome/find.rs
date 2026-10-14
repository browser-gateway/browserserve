//! Chrome/Chromium executable discovery.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use thiserror::Error;

/// Errors from executable discovery.
#[derive(Debug, Error)]
pub enum FindError {
    /// No known installation location contained an executable browser.
    #[error(
        "no Chrome or Chromium executable found in {searched} known locations; \
         set chrome.executablePath in the config or the BROWSERSERVE_CHROME_PATH environment variable"
    )]
    NotFound {
        /// Number of candidate locations probed.
        searched: usize,
    },
    /// An explicitly configured path is missing or not executable.
    #[error("configured chrome executable is missing or not executable: {path}")]
    BadExplicit {
        /// The rejected path.
        path: PathBuf,
    },
}

/// Resolves the browser executable to launch.
///
/// An explicit path is validated and used as-is; otherwise well-known
/// per-platform locations are probed in order.
///
/// # Errors
///
/// [`FindError::BadExplicit`] when a configured path is missing or not
/// executable; [`FindError::NotFound`] when no known location has a browser.
pub fn find_chrome(explicit: Option<&Path>) -> Result<PathBuf, FindError> {
    if let Some(path) = explicit {
        if is_executable(path) {
            return Ok(path.to_path_buf());
        }
        return Err(FindError::BadExplicit {
            path: path.to_path_buf(),
        });
    }
    let candidates = candidates();
    for candidate in &candidates {
        if is_executable(candidate) {
            return Ok(candidate.clone());
        }
    }
    Err(FindError::NotFound {
        searched: candidates.len(),
    })
}

/// How long `--version` may take before the probe gives up.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The browser's major version, read from `<executable> --version`
/// (e.g. `Google Chrome for Testing 154.0.8037.92` gives 154). `None` when the
/// browser cannot be run, does not answer in time, or prints no version.
#[must_use]
pub fn browser_major_version(executable: &Path) -> Option<u32> {
    let mut child = Command::new(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
    while child.try_wait().ok()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    parse_major_version(&out)
}

/// The major version in a `--version` line: the first whitespace-separated
/// token shaped like `154.0.8037.92`.
fn parse_major_version(output: &str) -> Option<u32> {
    output.split_whitespace().find_map(|token| {
        let mut parts = token.split('.');
        let major = parts.next()?.parse().ok()?;
        let rest: Vec<&str> = parts.collect();
        (rest.len() == 3 && rest.iter().all(|p| p.parse::<u32>().is_ok())).then_some(major)
    })
}

// macOS is a development convenience only; the product runs in the Linux image.
#[cfg(target_os = "macos")]
fn candidates() -> Vec<PathBuf> {
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(target_os = "linux")]
fn candidates() -> Vec<PathBuf> {
    [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/google-chrome",
        "/usr/local/bin/chromium",
        "/usr/local/bin/google-chrome",
        "/snap/bin/chromium",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_version_parses_known_version_lines() {
        for (line, major) in [
            ("Google Chrome for Testing 154.0.8037.92 \n", Some(154)),
            ("Chromium 141.0.7390.37 ", Some(141)),
            ("Google Chrome 155.0.8059.79 unknown\n", Some(155)),
            ("Chromium 1.2.3 snap", None),
            ("", None),
        ] {
            assert_eq!(parse_major_version(line), major, "{line:?}");
        }
    }

    #[test]
    fn major_version_is_none_for_a_missing_browser() {
        assert_eq!(
            browser_major_version(Path::new("/nonexistent/chrome")),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn major_version_reads_the_browser_output() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fake-chrome");
        std::fs::write(&path, b"#!/bin/sh\necho 'Chromium 150.0.1.2'\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(browser_major_version(&path), Some(150));
    }

    #[test]
    fn explicit_missing_path_is_rejected() {
        let err = find_chrome(Some(Path::new("/nonexistent/chrome"))).unwrap_err();
        assert!(matches!(err, FindError::BadExplicit { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn explicit_executable_is_accepted() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fake-chrome");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_chrome(Some(&path)).unwrap(), path);
    }

    #[cfg(unix)]
    #[test]
    fn explicit_non_executable_file_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not-exec");
        std::fs::write(&path, b"data").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            find_chrome(Some(&path)).unwrap_err(),
            FindError::BadExplicit { .. }
        ));
    }
}
