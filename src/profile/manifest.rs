//! Flat file manifest for the profile native layer, and its hardened extractor.
//!
//! The IndexedDB/service-worker stores move between the gateway and browserserve
//! as a list of `{ relative path, bytes }` regular-file entries, never a tar.
//! Tar/zip extraction has a repeated history of path-traversal and symlink
//! escapes (RUSTSEC-2021-0080, RUSTSEC-2026-0067, `TARmageddon`); a manifest of
//! regular files cannot even express a symlink or traversal entry, and
//! [`extract_into`] re-validates every path against the destination root and
//! writes with `O_NOFOLLOW` + `O_EXCL`.

use crate::profile::NATIVE_STORES;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

/// One regular file in a native-layer manifest. `path` is destination-relative
/// and forward-slash separated; it is validated on extraction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Destination-relative path (no leading `/`, no `..`).
    pub path: String,
    /// File contents (base64 on the wire, to avoid JSON number-array bloat).
    #[serde(with = "base64_bytes")]
    pub bytes: Vec<u8>,
}

mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(d)?;
        STANDARD.decode(&encoded).map_err(serde::de::Error::custom)
    }
}

/// Error from extracting a manifest.
#[derive(Debug, Error)]
pub enum ExtractError {
    /// A manifest path was absolute, escaped the root, or was otherwise unsafe.
    #[error("unsafe manifest path rejected: {0:?}")]
    UnsafePath(String),
    /// An I/O error while writing the manifest.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Collects the native-layer stores under `root` (`Default/IndexedDB`,
/// `Default/Service Worker`) into a manifest, paths relative to `root`.
/// Symlinks are skipped. Must run only after the browser process tree is dead.
///
/// # Errors
///
/// Propagates I/O errors reading the stores.
pub fn pack_native(root: &Path) -> io::Result<Vec<ManifestEntry>> {
    let mut entries = Vec::new();
    for store in NATIVE_STORES {
        let dir = root.join(store);
        if dir.is_dir() {
            pack_walk(root, &dir, &mut entries)?;
        }
    }
    Ok(entries)
}

fn pack_walk(root: &Path, dir: &Path, out: &mut Vec<ManifestEntry>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            pack_walk(root, &path, out)?;
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(io::Error::other)?
                .to_string_lossy()
                .into_owned();
            out.push(ManifestEntry {
                path: rel,
                bytes: fs::read(&path)?,
            });
        }
        // symlinks and other node types are intentionally not packed.
    }
    Ok(())
}

/// Writes each manifest entry under `dest_root`, rejecting any path that is
/// absolute, contains `..`/`.`, or would otherwise escape the root. Files are
/// created with `O_EXCL` + `O_NOFOLLOW` and mode `0600`, so a pre-existing
/// symlink at the target is never followed and an existing file is never
/// clobbered. Returns the number of files written.
///
/// # Errors
///
/// [`ExtractError::UnsafePath`] on the first unsafe path (nothing further is
/// written); [`ExtractError::Io`] on a write failure.
pub fn extract_into(entries: &[ManifestEntry], dest_root: &Path) -> Result<usize, ExtractError> {
    fs::create_dir_all(dest_root)?;
    let mut written = 0;
    for entry in entries {
        let target = safe_join(dest_root, &entry.path)
            .ok_or_else(|| ExtractError::UnsafePath(entry.path.clone()))?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        write_no_follow(&target, &entry.bytes)?;
        written += 1;
    }
    Ok(written)
}

// Joins a relative manifest path under `root`, allowing ONLY normal path
// components. Absolute paths, `..`, `.`, and Windows prefixes are rejected, so
// the result can never escape `root`.
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return None;
    }
    let mut out = root.to_path_buf();
    let mut pushed = false;
    for component in rel_path.components() {
        match component {
            Component::Normal(part) => {
                out.push(part);
                pushed = true;
            }
            _ => return None,
        }
    }
    pushed.then_some(out)
}

fn write_no_follow(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, bytes: &[u8]) -> ManifestEntry {
        ManifestEntry {
            path: path.to_owned(),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn pack_then_extract_round_trips_native_stores_only() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("Default/IndexedDB/x.leveldb")).unwrap();
        fs::write(src.join("Default/IndexedDB/x.leveldb/CURRENT"), b"m").unwrap();
        fs::create_dir_all(src.join("Default/Service Worker")).unwrap();
        fs::write(src.join("Default/Service Worker/index"), b"sw").unwrap();
        // not a native store -> must be excluded
        fs::create_dir_all(src.join("Default/Cache")).unwrap();
        fs::write(src.join("Default/Cache/data_0"), b"cache").unwrap();

        let manifest = pack_native(&src).unwrap();
        assert!(manifest.iter().all(|e| !e.path.contains("Cache")));

        let dest = tmp.path().join("dest");
        let n = extract_into(&manifest, &dest).unwrap();
        assert_eq!(n, manifest.len());
        assert_eq!(
            fs::read(dest.join("Default/IndexedDB/x.leveldb/CURRENT")).unwrap(),
            b"m"
        );
        assert_eq!(
            fs::read(dest.join("Default/Service Worker/index")).unwrap(),
            b"sw"
        );
        assert!(!dest.join("Default/Cache").exists());
    }

    #[test]
    fn rejects_parent_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let bad = [entry("../escape", b"x")];
        assert!(matches!(
            extract_into(&bad, &dest),
            Err(ExtractError::UnsafePath(_))
        ));
        assert!(
            !tmp.path().join("escape").exists(),
            "nothing written outside root"
        );
    }

    #[test]
    fn rejects_nested_traversal_and_absolute_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        for bad_path in ["a/../../b", "/etc/passwd", "Default/../../x", "."] {
            let bad = [entry(bad_path, b"x")];
            assert!(
                matches!(extract_into(&bad, &dest), Err(ExtractError::UnsafePath(_))),
                "{bad_path} must be rejected"
            );
        }
    }

    #[test]
    fn does_not_follow_a_preexisting_symlink_at_the_target() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        fs::create_dir_all(&dest).unwrap();
        let outside = tmp.path().join("outside.txt");
        fs::write(&outside, b"original").unwrap();
        // Plant a symlink inside dest pointing outside, then try to write through it.
        symlink(&outside, dest.join("link")).unwrap();
        let bad = [entry("link", b"overwritten")];
        assert!(
            extract_into(&bad, &dest).is_err(),
            "must refuse to follow the symlink"
        );
        assert_eq!(
            fs::read(&outside).unwrap(),
            b"original",
            "target file untouched"
        );
    }

    #[test]
    fn bytes_serialize_as_base64_and_round_trip() {
        let entry = entry("Default/IndexedDB/CURRENT", &[0, 1, 2, 250, 255]);
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            json.contains("\"bytes\":\"AAEC+v8=\""),
            "bytes are base64, got {json}"
        );
        let back: ManifestEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn normal_nested_paths_are_written() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let ok = [entry("Default/IndexedDB/a/b/CURRENT", b"ok")];
        assert_eq!(extract_into(&ok, &dest).unwrap(), 1);
        assert_eq!(
            fs::read(dest.join("Default/IndexedDB/a/b/CURRENT")).unwrap(),
            b"ok"
        );
    }
}
