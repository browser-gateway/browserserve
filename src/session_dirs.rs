//! Per-session profile directories: provision fresh, destroy completely.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

/// Disk locations owned by exactly one browser session.
#[derive(Debug, Clone)]
pub struct SessionDirs {
    /// Root of this session's state; removed wholesale on teardown.
    pub root: PathBuf,
    /// The Chrome `--user-data-dir`, created empty.
    pub user_data_dir: PathBuf,
}

impl SessionDirs {
    /// Creates a fresh, empty session directory tree under `data_dir`.
    ///
    /// # Errors
    ///
    /// Propagates the underlying I/O error when the tree cannot be created.
    pub async fn provision_plain(data_dir: &Path, id: Uuid) -> io::Result<Self> {
        let root = data_dir.join("sessions").join(id.to_string());
        let user_data_dir = root.join("user-data-dir");
        tokio::fs::create_dir_all(&user_data_dir).await?;
        Ok(Self {
            root,
            user_data_dir,
        })
    }

    /// Removes the session tree. Retries briefly because Chrome may still be
    /// flushing files at the moment its process exits.
    ///
    /// # Errors
    ///
    /// The last I/O error when removal keeps failing after all retries.
    pub async fn teardown(&self) -> io::Result<()> {
        const ATTEMPTS: u32 = 5;
        let mut last_err: Option<io::Error> = None;
        for attempt in 1..=ATTEMPTS {
            match tokio::fs::remove_dir_all(&self.root).await {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => {
                    last_err = Some(e);
                    if attempt < ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt))).await;
                    }
                }
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Removes leftover session trees from previous runs. Returns how many were removed.
///
/// Call only at startup, before any session exists.
///
/// # Errors
///
/// Propagates directory-listing failures; individual tree removals that fail
/// are skipped and reflected in the count.
pub async fn clean_stale(data_dir: &Path) -> io::Result<usize> {
    let sessions = data_dir.join("sessions");
    let mut removed = 0;
    let mut entries = match tokio::fs::read_dir(&sessions).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    while let Some(entry) = entries.next_entry().await? {
        if tokio::fs::remove_dir_all(entry.path()).await.is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn provision_creates_fresh_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        let dirs = SessionDirs::provision_plain(tmp.path(), id).await.unwrap();
        assert!(dirs.user_data_dir.is_dir());
        assert!(dirs.root.starts_with(tmp.path()));
        assert!(dirs.root.to_string_lossy().contains(&id.to_string()));
    }

    #[tokio::test]
    async fn teardown_removes_everything_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = SessionDirs::provision_plain(tmp.path(), Uuid::new_v4())
            .await
            .unwrap();
        tokio::fs::write(dirs.user_data_dir.join("Cookies"), b"x")
            .await
            .unwrap();
        dirs.teardown().await.unwrap();
        assert!(!dirs.root.exists());
        dirs.teardown().await.unwrap();
    }

    #[tokio::test]
    async fn clean_stale_sweeps_leftovers() {
        let tmp = tempfile::tempdir().unwrap();
        SessionDirs::provision_plain(tmp.path(), Uuid::new_v4())
            .await
            .unwrap();
        SessionDirs::provision_plain(tmp.path(), Uuid::new_v4())
            .await
            .unwrap();
        let removed = clean_stale(tmp.path()).await.unwrap();
        assert_eq!(removed, 2);
        assert_eq!(clean_stale(tmp.path()).await.unwrap(), 0);
    }
}
