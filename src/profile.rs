//! Profile native layer: snapshot and seed the on-disk stores that CDP cannot
//! carry.
//!
//! A profile's cookies and localStorage are the portable core and travel via
//! CDP (see `bridge`/CDP inject). `IndexedDB` and service workers cannot be
//! moved over the protocol, so browserserve moves them as whole store
//! directories: seed them into a fresh session dir before launch, snapshot them
//! back after the browser process tree is dead (`LevelDB` is copy-safe only once
//! the writer is gone). Reuses the reflink-aware copy from `template`.

pub mod cdp;
pub mod cookie;
pub mod localstorage;
pub mod manifest;
pub mod payload;

use crate::linux::tiers::ProfileTier;
use crate::template::copy_tree;
use std::io;
use std::path::Path;

/// User-data-dir-relative store directories that make up the native layer.
///
/// Cookies (`Default/Network/Cookies`) and localStorage
/// (`Default/Local Storage`) are deliberately absent: they are the portable
/// core and move via CDP so external providers can consume the same profile.
pub const NATIVE_STORES: &[&str] = &["Default/IndexedDB", "Default/Service Worker"];

/// Copies the native-layer stores out of a session's `user_data_dir` into
/// `dest`, preserving the relative store paths. A store that does not exist is
/// skipped (a profile may legitimately have no `IndexedDB` or service worker).
///
/// Caller MUST ensure the browser process tree is already reaped before calling
/// this: `LevelDB` stores are only crash-consistent to copy once the writer is
/// gone.
///
/// # Errors
///
/// Propagates the first I/O error from reading a present store or writing it.
pub fn snapshot_native(user_data_dir: &Path, dest: &Path, tier: ProfileTier) -> io::Result<()> {
    copy_present_stores(user_data_dir, dest, tier)
}

/// Copies previously-snapshotted native stores from `src` into a fresh session
/// `user_data_dir` before launch, so Chrome loads them natively. A store absent
/// from `src` is skipped.
///
/// # Errors
///
/// Propagates the first I/O error from reading a present store or writing it.
pub fn seed_native(src: &Path, user_data_dir: &Path, tier: ProfileTier) -> io::Result<()> {
    copy_present_stores(src, user_data_dir, tier)
}

fn copy_present_stores(from_root: &Path, to_root: &Path, tier: ProfileTier) -> io::Result<()> {
    let reflink = tier == ProfileTier::Reflink;
    for rel in NATIVE_STORES {
        let from = from_root.join(rel);
        if !from.is_dir() {
            continue;
        }
        copy_tree(&from, &to_root.join(rel), reflink)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn seed_store(root: &Path, rel: &str, files: &[(&str, &[u8])]) {
        let dir = root.join(rel);
        fs::create_dir_all(&dir).unwrap();
        for (name, bytes) in files {
            fs::write(dir.join(name), bytes).unwrap();
        }
    }

    #[test]
    fn snapshot_copies_native_stores_and_ignores_portable_core() {
        let tmp = tempfile::tempdir().unwrap();
        let udd = tmp.path().join("udd");
        seed_store(
            &udd,
            "Default/IndexedDB",
            &[("CURRENT", b"MANIFEST-000001")],
        );
        seed_store(&udd, "Default/Service Worker", &[("index", b"sw")]);
        // Portable core + a cache dir must NOT be captured by the native snapshot.
        seed_store(
            &udd,
            "Default/Local Storage/leveldb",
            &[("000003.log", b"ls")],
        );
        seed_store(&udd, "Default/Cache", &[("data_0", b"cache")]);

        let snap = tmp.path().join("snap");
        snapshot_native(&udd, &snap, ProfileTier::PlainCopy).unwrap();

        assert_eq!(
            fs::read(snap.join("Default/IndexedDB/CURRENT")).unwrap(),
            b"MANIFEST-000001"
        );
        assert_eq!(
            fs::read(snap.join("Default/Service Worker/index")).unwrap(),
            b"sw"
        );
        assert!(
            !snap.join("Default/Local Storage").exists(),
            "portable core excluded"
        );
        assert!(!snap.join("Default/Cache").exists(), "caches excluded");
    }

    #[test]
    fn seed_restores_native_stores_into_fresh_session_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = tmp.path().join("snap");
        seed_store(&snap, "Default/IndexedDB", &[("000005.ldb", b"idb-data")]);

        let udd = tmp.path().join("session/user-data-dir");
        fs::create_dir_all(udd.join("Default")).unwrap();
        seed_native(&snap, &udd, ProfileTier::Reflink).unwrap();

        assert_eq!(
            fs::read(udd.join("Default/IndexedDB/000005.ldb")).unwrap(),
            b"idb-data"
        );
    }

    #[test]
    fn missing_stores_are_skipped_not_errored() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty-udd");
        fs::create_dir_all(&empty).unwrap();
        let snap = tmp.path().join("snap");
        snapshot_native(&empty, &snap, ProfileTier::PlainCopy).unwrap();
        // No stores present -> nothing written, no error.
        assert!(!snap.join("Default").exists());
    }

    #[test]
    fn round_trips_indexeddb_blob_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let udd = tmp.path().join("udd");
        seed_store(
            &udd,
            "Default/IndexedDB/https_x.indexeddb.leveldb",
            &[("CURRENT", b"m")],
        );
        seed_store(
            &udd,
            "Default/IndexedDB/https_x.indexeddb.blob/1",
            &[("00", b"blob")],
        );

        let snap = tmp.path().join("snap");
        snapshot_native(&udd, &snap, ProfileTier::PlainCopy).unwrap();
        let restored = tmp.path().join("restored");
        fs::create_dir_all(restored.join("Default")).unwrap();
        seed_native(&snap, &restored, ProfileTier::PlainCopy).unwrap();

        assert_eq!(
            fs::read(restored.join("Default/IndexedDB/https_x.indexeddb.blob/1/00")).unwrap(),
            b"blob"
        );
    }
}
