//! Per-session cgroup v2: create a leaf, cap memory, kill the whole subtree.
//!
//! Writes the four cgroup v2 files directly (`std::fs`); no cgroup crate. All
//! operations require a delegated, writable cgroup v2 subtree (see the M4
//! decisions on the root-to-drop entrypoint).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const PROCS: &str = "cgroup.procs";
const SUBTREE_CONTROL: &str = "cgroup.subtree_control";
const CONTROLLERS: &str = "cgroup.controllers";
const MEMORY_MAX: &str = "memory.max";
const KILL: &str = "cgroup.kill";
const REMOVE_ATTEMPTS: u32 = 4;

/// Errors from cgroup operations.
#[derive(Debug, Error)]
pub enum CgroupError {
    /// A cgroup file could not be written or read.
    #[error("cgroup io on {path}: {source}")]
    Io {
        /// The offending path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

fn write(path: &Path, value: &str) -> Result<(), CgroupError> {
    fs::write(path, value).map_err(|source| CgroupError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read(path: &Path) -> Result<String, CgroupError> {
    fs::read_to_string(path).map_err(|source| CgroupError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// The base cgroup directory this process belongs to, from `/proc/self/cgroup`.
#[must_use]
pub fn own_cgroup_dir() -> Option<PathBuf> {
    let content = fs::read_to_string("/proc/self/cgroup").ok()?;
    // v2 line: "0::/path/relative/to/root"
    let rel = content
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim();
    let rel = rel.strip_prefix('/').unwrap_or(rel);
    Some(Path::new(CGROUP_ROOT).join(rel))
}

/// Controllers the parent has delegated (from its `cgroup.controllers`).
#[must_use]
pub fn available_controllers(dir: &Path) -> Vec<String> {
    read(&dir.join(CONTROLLERS))
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Enables `+memory` in a directory's `subtree_control` so child leaves can cap memory.
///
/// # Errors
///
/// [`CgroupError::Io`] when the write is refused (not delegated / not writable).
pub fn enable_memory_controller(dir: &Path) -> Result<(), CgroupError> {
    write(&dir.join(SUBTREE_CONTROL), "+memory")
}

/// A per-session cgroup leaf. Dropping it does NOT remove the cgroup; call
/// [`Cgroup::kill_and_remove`] explicitly during teardown.
pub struct Cgroup {
    dir: PathBuf,
}

impl Cgroup {
    /// Creates a leaf cgroup `name` under `parent`.
    ///
    /// # Errors
    ///
    /// [`CgroupError::Io`] when the directory cannot be created.
    pub fn create(parent: &Path, name: &str) -> Result<Self, CgroupError> {
        let dir = parent.join(name);
        fs::create_dir_all(&dir).map_err(|source| CgroupError::Io {
            path: dir.clone(),
            source,
        })?;
        Ok(Self { dir })
    }

    /// Moves a process into this cgroup.
    ///
    /// # Errors
    ///
    /// [`CgroupError::Io`] when `cgroup.procs` cannot be written.
    pub fn attach(&self, pid: i32) -> Result<(), CgroupError> {
        write(&self.dir.join(PROCS), &pid.to_string())
    }

    /// Sets the hard memory ceiling in bytes. Zero means unlimited (`"max"`).
    ///
    /// # Errors
    ///
    /// [`CgroupError::Io`] when `memory.max` cannot be written.
    pub fn set_memory_max(&self, bytes: u64) -> Result<(), CgroupError> {
        let value = if bytes == 0 {
            String::from("max")
        } else {
            bytes.to_string()
        };
        write(&self.dir.join(MEMORY_MAX), &value)
    }

    /// Whether `cgroup.kill` (kernel ≥5.14) is available in this cgroup.
    #[must_use]
    pub fn supports_kill(&self) -> bool {
        self.dir.join(KILL).exists()
    }

    /// Atomically SIGKILLs every process in the subtree, then removes the
    /// cgroup (retried, since rmdir races the kernel freeing it). Returns
    /// whether `cgroup.kill` was used.
    pub async fn kill_and_remove(self) -> bool {
        let used_kill = if self.dir.join(KILL).exists() {
            write(&self.dir.join(KILL), "1").is_ok()
        } else {
            false
        };
        for attempt in 1..=REMOVE_ATTEMPTS {
            if fs::remove_dir(&self.dir).is_ok() {
                break;
            }
            if attempt < REMOVE_ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt))).await;
            }
        }
        used_kill
    }

    /// This cgroup's directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_max_zero_is_unlimited() {
        let tmp = tempfile::tempdir().unwrap();
        let cg = Cgroup {
            dir: tmp.path().to_path_buf(),
        };
        cg.set_memory_max(0).unwrap();
        assert_eq!(read(&tmp.path().join(MEMORY_MAX)).unwrap(), "max");
    }

    #[test]
    fn memory_max_nonzero_is_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let cg = Cgroup {
            dir: tmp.path().to_path_buf(),
        };
        cg.set_memory_max(1_048_576).unwrap();
        assert_eq!(read(&tmp.path().join(MEMORY_MAX)).unwrap(), "1048576");
    }

    #[test]
    fn attach_writes_pid_to_procs() {
        let tmp = tempfile::tempdir().unwrap();
        let cg = Cgroup {
            dir: tmp.path().to_path_buf(),
        };
        cg.attach(4242).unwrap();
        assert_eq!(read(&tmp.path().join(PROCS)).unwrap(), "4242");
    }

    #[test]
    fn create_makes_the_leaf_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cg = Cgroup::create(tmp.path(), "session-x").unwrap();
        assert!(cg.dir().is_dir());
        assert!(cg.dir().ends_with("session-x"));
    }

    #[test]
    fn supports_kill_reflects_file_presence() {
        let tmp = tempfile::tempdir().unwrap();
        let cg = Cgroup {
            dir: tmp.path().to_path_buf(),
        };
        assert!(!cg.supports_kill());
        fs::write(tmp.path().join(KILL), "").unwrap();
        assert!(cg.supports_kill());
    }

    // Note: these tests simulate a cgroup with a regular directory. A real
    // cgroup v2 dir is removable via rmdir even with its interface files
    // present (kernel-special), but a regular non-empty dir is not — so the
    // "kill present" and "dir removal" paths are asserted separately here.

    #[tokio::test]
    async fn kill_and_remove_writes_kill_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let leaf = tmp.path().join("session-y");
        fs::create_dir(&leaf).unwrap();
        fs::write(leaf.join(KILL), "").unwrap();
        let cg = Cgroup { dir: leaf.clone() };
        let used = cg.kill_and_remove().await;
        assert!(used, "cgroup.kill should be used when present");
        assert_eq!(fs::read_to_string(leaf.join(KILL)).unwrap(), "1");
    }

    #[tokio::test]
    async fn kill_and_remove_deletes_an_empty_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let leaf = tmp.path().join("session-z");
        fs::create_dir(&leaf).unwrap();
        let cg = Cgroup { dir: leaf.clone() };
        let used = cg.kill_and_remove().await;
        assert!(!used, "no cgroup.kill file means kill was not used");
        assert!(!leaf.exists(), "an empty leaf must be removed");
    }
}
