//! The pool's session factory: real Chrome launches with tier-aware isolation.

use crate::capacity::SessionFootprint;
use crate::chrome::{Browser, BrowserVersion, LaunchSpec, force_kill_group, launch, teardown};
use crate::config::RuntimeConfig;
use crate::linux::tiers::{MemCapTier, Tiers};
use crate::pool::SessionFactory;
use crate::rss::{tree_rss_bytes, tree_thread_count};
use crate::session_dirs::SessionDirs;
use crate::template;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::task::AbortHandle;
use uuid::Uuid;

const RSS_POLL_INTERVAL: Duration = Duration::from_secs(3);

#[cfg(target_os = "linux")]
type SessionCgroup = crate::linux::cgroup::Cgroup;
#[cfg(not(target_os = "linux"))]
type SessionCgroup = ();

/// One pooled browser: the process, its private disk state, and any
/// per-session isolation resources (cgroup, soft-cap monitor).
pub struct ChromeSession {
    /// The launched, CDP-ready browser.
    pub browser: Browser,
    /// The session's private directories, removed on destroy.
    pub dirs: SessionDirs,
    cgroup: Option<SessionCgroup>,
    monitor: Option<AbortHandle>,
}

struct FactoryInner {
    executable: PathBuf,
    no_sandbox: bool,
    extra_flags: Vec<String>,
    launch_timeout: Duration,
    max_frame_bytes: usize,
    kill_grace: Duration,
    memory_max_bytes: u64,
    data_dir: PathBuf,
    tiers: Tiers,
    version: Mutex<Option<BrowserVersion>>,
    template_dir: Mutex<Option<PathBuf>>,
    footprint: Mutex<Option<SessionFootprint>>,
    #[cfg(target_os = "linux")]
    cgroup_base: Option<PathBuf>,
}

/// Launches and destroys [`ChromeSession`]s with the host's best isolation
/// tier. Cheap to clone; clones share the cached version and template.
#[derive(Clone)]
pub struct ChromeFactory {
    inner: Arc<FactoryInner>,
}

impl ChromeFactory {
    /// Builds a factory from resolved config, a validated executable, and the
    /// detected isolation tiers.
    #[must_use]
    pub fn new(config: &RuntimeConfig, executable: PathBuf, tiers: Tiers) -> Self {
        #[cfg(target_os = "linux")]
        let cgroup_base = if tiers.memcap == MemCapTier::Cgroup {
            prepare_cgroup_base()
        } else {
            None
        };
        Self {
            inner: Arc::new(FactoryInner {
                executable,
                no_sandbox: config.chrome.no_sandbox,
                extra_flags: config.chrome.extra_flags.clone(),
                launch_timeout: Duration::from_millis(config.chrome.launch_timeout_ms),
                max_frame_bytes: config.chrome.max_frame_bytes,
                kill_grace: Duration::from_millis(config.session.kill_grace_ms),
                memory_max_bytes: config.session.memory_max_mb.saturating_mul(1024 * 1024),
                data_dir: config.data_dir.clone(),
                tiers,
                version: Mutex::new(None),
                template_dir: Mutex::new(None),
                footprint: Mutex::new(None),
                #[cfg(target_os = "linux")]
                cgroup_base,
            }),
        }
    }

    /// The browser version reported by the most recent launch, if any yet.
    #[must_use]
    pub fn cached_version(&self) -> Option<BrowserVersion> {
        self.inner
            .version
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Records a browser version, deriving the template key from it.
    fn note_version(&self, version: &BrowserVersion) {
        *self
            .inner
            .version
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(version.clone());
    }

    /// The measured process-tree footprint of the warmed template browser, if
    /// this host could measure one (Linux with a live template launch).
    #[must_use]
    pub fn template_footprint(&self) -> Option<SessionFootprint> {
        *self
            .inner
            .footprint
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn template_dir(&self) -> Option<PathBuf> {
        self.inner
            .template_dir
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Builds the warmed, sealed profile template once, so later sessions clone
    /// it instead of paying Chrome's first-run cost. Best-effort: on any
    /// failure the factory silently keeps launching on empty dirs.
    pub async fn prepare_template(&self) {
        if self.template_dir().is_some() {
            return;
        }
        let inner = &self.inner;
        let scratch = match SessionDirs::provision_plain(&inner.data_dir, Uuid::new_v4()).await {
            Ok(dirs) => dirs,
            Err(e) => {
                tracing::warn!(error = %e, "template: scratch dir failed; sessions use empty dirs");
                return;
            }
        };
        let spec = self.launch_spec(&scratch.user_data_dir);
        let browser = match launch(&spec).await {
            Ok(browser) => browser,
            Err(e) => {
                tracing::warn!(error = %e, "template: warm launch failed; sessions use empty dirs");
                let _ = scratch.teardown().await;
                return;
            }
        };
        let key = browser_key(&browser.version);
        self.note_version(&browser.version);
        let measured = SessionFootprint {
            bytes: tree_rss_bytes(browser.pid).unwrap_or(0),
            threads: tree_thread_count(browser.pid).unwrap_or(0),
        };
        if measured.bytes > 0 || measured.threads > 0 {
            *inner
                .footprint
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(measured);
        }
        teardown(browser, inner.kill_grace).await;

        let template = template::template_path(&inner.data_dir, &key);
        if let Some(parent) = template.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::remove_dir_all(&template).await;
        let strip_target = scratch.user_data_dir.clone();
        let move_ok = tokio::task::spawn_blocking(move || {
            template::strip_for_seal(&strip_target)?;
            std::io::Result::Ok(())
        })
        .await
        .is_ok();
        if move_ok
            && tokio::fs::rename(&scratch.user_data_dir, &template)
                .await
                .is_ok()
        {
            *inner
                .template_dir
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(template);
            tracing::info!(browser = %key, "warmed profile template sealed");
        } else {
            tracing::warn!("template: seal failed; sessions use empty dirs");
        }
        let _ = scratch.teardown().await;
    }

    fn launch_spec<'a>(&'a self, user_data_dir: &'a std::path::Path) -> LaunchSpec<'a> {
        LaunchSpec {
            executable: &self.inner.executable,
            user_data_dir,
            no_sandbox: self.inner.no_sandbox,
            extra_flags: &self.inner.extra_flags,
            launch_timeout: self.inner.launch_timeout,
            max_frame_bytes: self.inner.max_frame_bytes,
        }
    }

    async fn provision_dirs(&self) -> Result<SessionDirs, String> {
        let inner = &self.inner;
        let id = Uuid::new_v4();
        if let Some(template) = self.template_dir() {
            let root = inner.data_dir.join("sessions").join(id.to_string());
            let user_data_dir = root.join("user-data-dir");
            let profile_tier = inner.tiers.profile;
            let template2 = template.clone();
            let udd = user_data_dir.clone();
            let cloned = tokio::task::spawn_blocking(move || {
                template::clone_template(&template2, &udd, profile_tier)
            })
            .await;
            match cloned {
                Ok(Ok(())) => {
                    return Ok(SessionDirs {
                        root,
                        user_data_dir,
                    });
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "template clone failed; falling back to empty dir");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "template clone task failed; falling back to empty dir");
                }
            }
        }
        SessionDirs::provision_plain(&inner.data_dir, id)
            .await
            .map_err(|e| format!("session dir provisioning failed: {e}"))
    }
}

impl SessionFactory for ChromeFactory {
    type Session = ChromeSession;

    async fn create(&self) -> Result<ChromeSession, String> {
        let dirs = self.provision_dirs().await?;
        let spec = self.launch_spec(&dirs.user_data_dir);
        let browser = match launch(&spec).await {
            Ok(browser) => browser,
            Err(e) => {
                let _ = dirs.teardown().await;
                let mut message = e.to_string();
                if let Some(hint) = e.remediation() {
                    message.push_str("\nhint: ");
                    message.push_str(hint);
                }
                return Err(message);
            }
        };
        self.note_version(&browser.version);

        let cgroup = self.apply_cgroup(browser.pid);
        let monitor = self.spawn_soft_cap(cgroup.is_some(), browser.pid);

        Ok(ChromeSession {
            browser,
            dirs,
            cgroup,
            monitor,
        })
    }

    async fn destroy(&self, session: ChromeSession) {
        if let Some(monitor) = session.monitor {
            monitor.abort();
        }
        // cgroup.kill (when present) SIGKILLs the whole subtree atomically;
        // teardown then reaps the direct child and is the universal fallback.
        self.destroy_cgroup(session.cgroup).await;
        let report = teardown(session.browser, self.inner.kill_grace).await;
        if !report.reaped {
            tracing::warn!(exit = ?report.exit_status, "browser was not reaped during teardown");
        }
        if let Err(e) = session.dirs.teardown().await {
            tracing::warn!(error = %e, "session dir teardown failed");
        }
    }

    fn is_alive(&self, session: &mut ChromeSession) -> bool {
        session.browser.is_running()
    }
}

impl ChromeFactory {
    #[cfg(target_os = "linux")]
    fn apply_cgroup(&self, pid: i32) -> Option<SessionCgroup> {
        let base = self.inner.cgroup_base.as_ref()?;
        let name = format!("session-{pid}");
        let leaf = crate::linux::cgroup::Cgroup::create(base, &name).ok()?;
        if self.inner.memory_max_bytes > 0
            && let Err(e) = leaf.set_memory_max(self.inner.memory_max_bytes)
        {
            tracing::warn!(error = %e, "cgroup: memory.max write failed");
        }
        if let Err(e) = leaf.attach(pid) {
            tracing::warn!(error = %e, "cgroup: attach failed; leaf inert");
        }
        Some(leaf)
    }

    #[cfg(not(target_os = "linux"))]
    #[allow(clippy::unused_self)]
    fn apply_cgroup(&self, _pid: i32) -> Option<SessionCgroup> {
        None
    }

    #[cfg(target_os = "linux")]
    async fn destroy_cgroup(&self, cgroup: Option<SessionCgroup>) {
        if let Some(cg) = cgroup {
            cg.kill_and_remove().await;
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[allow(clippy::unused_self)]
    fn destroy_cgroup(&self, _cgroup: Option<SessionCgroup>) -> std::future::Ready<()> {
        std::future::ready(())
    }

    fn spawn_soft_cap(&self, has_cgroup: bool, pid: i32) -> Option<AbortHandle> {
        let cap = self.inner.memory_max_bytes;
        if cap == 0 || has_cgroup || self.inner.tiers.memcap != MemCapTier::RssPoll {
            return None;
        }
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(RSS_POLL_INTERVAL).await;
                match tree_rss_bytes(pid) {
                    Some(rss) if rss > cap => {
                        tracing::warn!(pid, rss, cap, "session exceeded memory soft-cap; killing");
                        force_kill_group(pid);
                        return;
                    }
                    Some(_) => {}
                    None => return,
                }
            }
        });
        Some(handle.abort_handle())
    }
}

#[cfg(target_os = "linux")]
fn prepare_cgroup_base() -> Option<PathBuf> {
    // The entrypoint sets this to a pre-delegated, uid-999-owned subtree when it
    // ran as root and self-delegated; prefer it over guessing our own cgroup.
    if let Ok(explicit) = std::env::var("BROWSERSERVE_CGROUP_BASE") {
        let dir = PathBuf::from(explicit);
        if dir.is_dir() {
            return Some(dir);
        }
    }
    let base = crate::linux::cgroup::own_cgroup_dir()?;
    if crate::linux::cgroup::Cgroup::create(&base, "browserserve").is_ok() {
        Some(base.join("browserserve"))
    } else {
        None
    }
}

fn browser_key(version: &BrowserVersion) -> String {
    let product = version.product.replace('/', "-").replace(
        |c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-',
        "",
    );
    if product.is_empty() {
        String::from("chrome-unknown")
    } else {
        product.to_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load;
    use crate::linux::probe;
    use std::collections::HashMap;

    fn factory_with_bad_executable() -> (ChromeFactory, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = load(None, &HashMap::new()).unwrap().config;
        config.data_dir = tmp.path().to_path_buf();
        let tiers = probe::detect(&config.data_dir);
        let factory = ChromeFactory::new(&config, PathBuf::from("/nonexistent/browser"), tiers);
        (factory, tmp)
    }

    #[tokio::test]
    async fn create_failure_is_described_and_leaves_no_dirs() {
        let (factory, tmp) = factory_with_bad_executable();
        let Err(err) = factory.create().await else {
            panic!("create must fail for a nonexistent executable");
        };
        assert!(err.contains("failed to spawn"), "unexpected error: {err}");
        let sessions = tmp.path().join("sessions");
        let leftover = std::fs::read_dir(&sessions).map_or(0, Iterator::count);
        assert_eq!(leftover, 0, "failed create must clean its session dir");
    }

    #[tokio::test]
    async fn version_cache_starts_empty() {
        let (factory, _tmp) = factory_with_bad_executable();
        assert!(factory.cached_version().is_none());
    }

    #[test]
    fn browser_key_sanitizes_product() {
        let v = BrowserVersion {
            product: String::from("Chrome/149.0.7827.0"),
            ..BrowserVersion::default()
        };
        assert_eq!(browser_key(&v), "chrome-149.0.7827.0");
        assert_eq!(browser_key(&BrowserVersion::default()), "chrome-unknown");
    }
}

#[cfg(test)]
mod chrome_tests {
    use super::*;
    use crate::chrome::find_chrome;
    use crate::config::load;
    use crate::linux::probe;
    use crate::pool::{Pool, PoolOptions};
    use std::collections::HashMap;

    fn real_factory(tmp: &tempfile::TempDir) -> ChromeFactory {
        let executable = find_chrome(None).expect("chrome required for this test");
        let mut config = load(None, &HashMap::new()).unwrap().config;
        config.data_dir = tmp.path().to_path_buf();
        let tiers = probe::detect(&config.data_dir);
        ChromeFactory::new(&config, executable, tiers)
    }

    #[tokio::test]
    #[ignore = "requires a local Chrome installation"]
    async fn create_is_alive_destroy_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let factory = real_factory(&tmp);
        let mut session = factory.create().await.expect("launch");
        assert!(factory.is_alive(&mut session));
        assert!(factory.cached_version().is_some());
        let root = session.dirs.root.clone();
        factory.destroy(session).await;
        assert!(!root.exists());
    }

    #[tokio::test]
    #[ignore = "requires a local Chrome installation"]
    async fn template_prepared_then_sessions_clone_it() {
        let tmp = tempfile::tempdir().unwrap();
        let factory = real_factory(&tmp);
        factory.prepare_template().await;
        assert!(factory.template_dir().is_some(), "template should seal");
        let session = factory.create().await.expect("clone + launch");
        assert!(
            session.dirs.user_data_dir.join("Default").exists()
                || session.dirs.user_data_dir.join("Local State").exists(),
            "cloned session should carry warmed profile files"
        );
        factory.destroy(session).await;
    }

    #[tokio::test]
    #[ignore = "requires a local Chrome installation"]
    async fn pool_serves_real_chrome_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let factory = real_factory(&tmp);
        let pool = Pool::new(
            factory,
            PoolOptions {
                min_ready: 1,
                max_sessions: 2,
                max_queue: 2,
                queue_timeout: Duration::from_secs(30),
                warm_idle: Duration::from_mins(5),
                warm_max_age: Duration::from_hours(1),
            },
        );
        let claimed = pool.claim().await.expect("claim");
        assert!(claimed.session().browser.pid > 0);
        pool.destroy(claimed).await;
        pool.close();
    }
}
