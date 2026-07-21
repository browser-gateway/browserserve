//! Runtime configuration: YAML file plus environment overrides.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors produced while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The YAML in the config file could not be parsed.
    #[error("invalid config YAML: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    /// The config file could not be read.
    #[error("cannot read config file {path}: {source}")]
    Read {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The `PORT` environment variable is not a valid TCP port.
    #[error("invalid PORT value {value:?}")]
    BadPort {
        /// The rejected value.
        value: String,
    },
    /// A field failed semantic validation.
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// Browser pool sizing and queueing.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct PoolConfig {
    /// Browsers kept launched and ready ahead of demand.
    pub min_ready: u32,
    /// Hard ceiling of concurrent claimed sessions.
    pub max_sessions: u32,
    /// Maximum clients waiting for a session slot before rejection.
    pub max_queue: u32,
    /// How long a queued client waits before rejection, in milliseconds.
    pub queue_timeout_ms: u64,
    /// Idle time after which warm browsers above `min_ready` are culled.
    pub warm_idle_ms: u64,
    /// Age at which any warm browser is recycled regardless of use.
    pub warm_max_age_ms: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min_ready: 1,
            max_sessions: 10,
            max_queue: 10,
            queue_timeout_ms: 30_000,
            warm_idle_ms: 300_000,
            warm_max_age_ms: 3_600_000,
        }
    }
}

/// Per-session limits and teardown behavior.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionConfig {
    /// Maximum session lifetime in milliseconds. `0` means unlimited.
    pub max_session_ms: u64,
    /// Per-session memory cap in MiB, enforced via cgroups on Linux. `0` disables.
    pub memory_max_mb: u64,
    /// Size cap for the RAM-backed session dir tier on Linux, in MiB.
    pub tmpfs_size_mb: u64,
    /// Grace period between SIGTERM and SIGKILL during teardown, in milliseconds.
    pub kill_grace_ms: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_session_ms: 0,
            memory_max_mb: 2048,
            tmpfs_size_mb: 512,
            kill_grace_ms: 5_000,
        }
    }
}

/// Admission thresholds: new sessions are rejected above these, running ones untouched.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct PressureConfig {
    /// CPU usage percentage above which new sessions are rejected.
    pub max_cpu_percent: f64,
    /// Memory usage percentage above which new sessions are rejected.
    pub max_memory_percent: f64,
}

impl Default for PressureConfig {
    fn default() -> Self {
        Self {
            max_cpu_percent: 95.0,
            max_memory_percent: 95.0,
        }
    }
}

/// How the runtime talks CDP to each Chrome it launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// CDP over inherited pipe file descriptors. No TCP port exists.
    Pipe,
    /// CDP over a localhost TCP port. Fallback mode.
    Port,
}

/// Chrome discovery and launch settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ChromeConfig {
    /// Explicit Chrome/Chromium executable. Auto-detected when unset.
    pub executable_path: Option<PathBuf>,
    /// CDP transport between the runtime and each Chrome.
    pub transport: Transport,
    /// Launch Chrome with `--no-sandbox`. Off by default; prefer a sandboxed setup.
    pub no_sandbox: bool,
    /// Extra flags appended after the built-in set.
    pub extra_flags: Vec<String>,
    /// Time budget for a launched Chrome to become CDP-ready, in milliseconds.
    pub launch_timeout_ms: u64,
    /// Maximum size of a single CDP message before the session is failed, in bytes.
    pub max_frame_bytes: usize,
}

impl Default for ChromeConfig {
    fn default() -> Self {
        Self {
            executable_path: None,
            transport: Transport::Pipe,
            no_sandbox: false,
            extra_flags: Vec::new(),
            launch_timeout_ms: 30_000,
            max_frame_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Root configuration. Every field has a working default; an empty file is valid.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeConfig {
    /// Browser pool sizing and queueing.
    pub pool: PoolConfig,
    /// Per-session limits and teardown behavior.
    pub session: SessionConfig,
    /// Admission thresholds.
    pub pressure: PressureConfig,
    /// Chrome discovery and launch settings.
    pub chrome: ChromeConfig,
    /// Directory holding per-session state. Created if missing.
    pub data_dir: PathBuf,
    /// Time budget for draining active sessions on shutdown, in milliseconds.
    pub drain_timeout_ms: u64,
    /// Public address advertised in `webSocketDebuggerUrl`, e.g.
    /// `ws://browsers.internal:9222`. Derived from the request Host when unset.
    pub external_address: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            pool: PoolConfig::default(),
            session: SessionConfig::default(),
            pressure: PressureConfig::default(),
            chrome: ChromeConfig::default(),
            data_dir: PathBuf::from(".browserserve"),
            drain_timeout_ms: 30_000,
            external_address: None,
        }
    }
}

/// Network-facing settings, sourced from the environment only.
#[derive(Debug, Clone)]
pub struct ServeSettings {
    /// TCP port for the front door. `PORT` env var, default 9222.
    pub port: u16,
    /// Bind address. `HOST` env var, default `0.0.0.0`.
    pub host: String,
    /// Auth token for WebSocket sessions. `BROWSERSERVE_TOKEN` env var, unset means no auth.
    pub token: Option<String>,
}

/// A fully resolved configuration.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// File-plus-env runtime configuration.
    pub config: RuntimeConfig,
    /// Environment-only network settings.
    pub serve: ServeSettings,
}

/// Parses YAML (may be `None` or empty) and applies environment overrides.
///
/// Recognized environment keys: `PORT`, `HOST`, `BROWSERSERVE_TOKEN`, `BROWSERSERVE_CHROME_PATH`,
/// `BROWSERSERVE_DATA_DIR`. Returns a validated [`Loaded`].
///
/// # Errors
///
/// [`ConfigError::Yaml`] on unparsable YAML or unknown keys,
/// [`ConfigError::BadPort`] on a non-numeric `PORT`, and
/// [`ConfigError::Invalid`] on semantic violations.
pub fn load<S: std::hash::BuildHasher>(
    yaml: Option<&str>,
    env: &HashMap<String, String, S>,
) -> Result<Loaded, ConfigError> {
    let mut config: RuntimeConfig = match yaml {
        Some(text) if !text.trim().is_empty() => serde_yaml_ng::from_str(text)?,
        _ => RuntimeConfig::default(),
    };

    if let Some(path) = env.get("BROWSERSERVE_CHROME_PATH") {
        config.chrome.executable_path = Some(PathBuf::from(path));
    }
    if let Some(dir) = env.get("BROWSERSERVE_DATA_DIR") {
        config.data_dir = PathBuf::from(dir);
    }

    let port = match env.get("PORT") {
        Some(value) => value.parse::<u16>().map_err(|_| ConfigError::BadPort {
            value: value.clone(),
        })?,
        None => 9222,
    };
    let serve = ServeSettings {
        port,
        host: env
            .get("HOST")
            .cloned()
            .unwrap_or_else(|| String::from("0.0.0.0")),
        token: env.get("BROWSERSERVE_TOKEN").cloned(),
    };

    validate(&config)?;
    Ok(Loaded { config, serve })
}

/// Resolves the config file path, reads it if present, and delegates to [`load`].
///
/// Resolution order: `explicit_path`, then `BROWSERSERVE_CONFIG`, then `./browserserve.yml`
/// when it exists, then no file at all.
///
/// # Errors
///
/// [`ConfigError::Read`] when a resolved file cannot be read, plus everything
/// [`load`] returns.
pub fn load_from_env<S: std::hash::BuildHasher>(
    env: &HashMap<String, String, S>,
    explicit_path: Option<&Path>,
) -> Result<Loaded, ConfigError> {
    let candidate = explicit_path
        .map(Path::to_path_buf)
        .or_else(|| env.get("BROWSERSERVE_CONFIG").map(PathBuf::from))
        .or_else(|| {
            let default = PathBuf::from("browserserve.yml");
            default.exists().then_some(default)
        });

    let yaml = match candidate {
        Some(path) => Some(
            std::fs::read_to_string(&path).map_err(|source| ConfigError::Read { path, source })?,
        ),
        None => None,
    };
    load(yaml.as_deref(), env)
}

fn validate(config: &RuntimeConfig) -> Result<(), ConfigError> {
    if config.pool.max_sessions == 0 {
        return Err(ConfigError::Invalid(String::from(
            "pool.maxSessions must be at least 1",
        )));
    }
    for (name, value) in [
        ("pressure.maxCpuPercent", config.pressure.max_cpu_percent),
        (
            "pressure.maxMemoryPercent",
            config.pressure.max_memory_percent,
        ),
    ] {
        if !(1.0..=100.0).contains(&value) {
            return Err(ConfigError::Invalid(format!(
                "{name} must be between 1 and 100"
            )));
        }
    }
    if config.session.kill_grace_ms == 0 {
        return Err(ConfigError::Invalid(String::from(
            "session.killGraceMs must be greater than 0",
        )));
    }
    if config.chrome.max_frame_bytes == 0 {
        return Err(ConfigError::Invalid(String::from(
            "chrome.maxFrameBytes must be greater than 0",
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn defaults_when_no_yaml() {
        let loaded = load(None, &HashMap::new()).unwrap();
        assert_eq!(loaded.config.pool.min_ready, 1);
        assert_eq!(loaded.config.pool.max_sessions, 10);
        assert_eq!(loaded.config.chrome.transport, Transport::Pipe);
        assert_eq!(loaded.serve.port, 9222);
        assert_eq!(loaded.serve.host, "0.0.0.0");
        assert!(loaded.serve.token.is_none());
    }

    #[test]
    fn empty_yaml_is_defaults() {
        let loaded = load(Some("  \n"), &HashMap::new()).unwrap();
        assert_eq!(loaded.config.pool.max_sessions, 10);
    }

    #[test]
    fn yaml_overrides_defaults() {
        let yaml = "
pool:
  maxSessions: 3
  minReady: 0
chrome:
  noSandbox: true
  transport: port
session:
  killGraceMs: 2000
dataDir: /var/lib/bgr
";
        let loaded = load(Some(yaml), &HashMap::new()).unwrap();
        assert_eq!(loaded.config.pool.max_sessions, 3);
        assert_eq!(loaded.config.pool.min_ready, 0);
        assert!(loaded.config.chrome.no_sandbox);
        assert_eq!(loaded.config.chrome.transport, Transport::Port);
        assert_eq!(loaded.config.session.kill_grace_ms, 2000);
        assert_eq!(loaded.config.data_dir, PathBuf::from("/var/lib/bgr"));
    }

    #[test]
    fn unknown_fields_rejected() {
        assert!(load(Some("pool:\n  maxSesions: 3\n"), &HashMap::new()).is_err());
    }

    #[test]
    fn env_overrides_win() {
        let env = env(&[
            ("PORT", "9333"),
            ("HOST", "127.0.0.1"),
            ("BROWSERSERVE_TOKEN", "secret"),
            ("BROWSERSERVE_CHROME_PATH", "/opt/chrome"),
            ("BROWSERSERVE_DATA_DIR", "/tmp/bgr-data"),
        ]);
        let loaded = load(Some("dataDir: ignored"), &env).unwrap();
        assert_eq!(loaded.serve.port, 9333);
        assert_eq!(loaded.serve.host, "127.0.0.1");
        assert_eq!(loaded.serve.token.as_deref(), Some("secret"));
        assert_eq!(
            loaded.config.chrome.executable_path,
            Some(PathBuf::from("/opt/chrome"))
        );
        assert_eq!(loaded.config.data_dir, PathBuf::from("/tmp/bgr-data"));
    }

    #[test]
    fn bad_port_rejected() {
        let err = load(None, &env(&[("PORT", "eighty")])).unwrap_err();
        assert!(matches!(err, ConfigError::BadPort { .. }));
    }

    #[test]
    fn semantic_validation() {
        assert!(load(Some("pool:\n  maxSessions: 0\n"), &HashMap::new()).is_err());
        assert!(load(Some("pressure:\n  maxCpuPercent: 0\n"), &HashMap::new()).is_err());
        assert!(load(Some("session:\n  killGraceMs: 0\n"), &HashMap::new()).is_err());
    }
}
