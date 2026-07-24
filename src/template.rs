//! Warmed, sealed profile template and the per-session clone ladder.
//!
//! A template is a user-data-dir that a browser has already initialized once
//! (first-run paid), stripped of per-instance singleton state, and sealed. Each
//! session clones it (reflink where possible, else copy) so no session pays the
//! first-run cost and none observes another's state.

use crate::linux::tiers::ProfileTier;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Strips per-instance state so a copied template launches as a fresh profile.
///
/// Removes Chrome's `Singleton*` symlinks and lock/version markers and the
/// GPU/shader caches (safe: the image pins one browser build). Idempotent.
///
/// # Errors
///
/// Propagates I/O errors other than missing entries.
pub fn strip_for_seal(profile_dir: &Path) -> io::Result<()> {
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        remove_if_present(&profile_dir.join(name))?;
    }
    let default = profile_dir.join("Default");
    for rel in [
        "RunningChromeVersion",
        "GPUCache",
        "ShaderCache",
        "GrShaderCache",
    ] {
        remove_if_present(&default.join(rel))?;
        remove_if_present(&profile_dir.join(rel))?;
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Clones a sealed template into `dest` using the best available strategy for
/// `tier`, falling back to a plain recursive copy.
///
/// # Errors
///
/// Propagates the underlying copy error when even the plain copy fails.
pub fn clone_template(template: &Path, dest: &Path, tier: ProfileTier) -> io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    copy_tree(template, dest, tier == ProfileTier::Reflink)
}

// One recursive tree copier; `reflink` chooses the per-file copy strategy.
// Symlinks are intentionally dropped (sealed templates and Chrome storage
// stores are plain files). Shared with the profile snapshot/seed path.
pub(crate) fn copy_tree(src: &Path, dest: &Path, reflink: bool) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let meta = entry.metadata()?;
        if meta.is_dir() {
            copy_tree(&from, &to, reflink)?;
        } else if meta.is_file() {
            copy_file(&from, &to, reflink)?;
        }
    }
    Ok(())
}

fn copy_file(from: &Path, to: &Path, reflink: bool) -> io::Result<()> {
    if reflink && reflink_copy::reflink_or_copy(from, to).is_ok() {
        return Ok(());
    }
    fs::copy(from, to)?;
    Ok(())
}

/// The on-disk location of a sealed template for one browser build.
#[must_use]
pub fn template_path(data_dir: &Path, browser_key: &str) -> PathBuf {
    data_dir.join("templates").join(browser_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn strip_removes_singletons_and_caches() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path();
        symlink("/proc/1/whatever", profile.join("SingletonLock")).unwrap();
        fs::create_dir_all(profile.join("Default/GPUCache")).unwrap();
        fs::write(profile.join("Default/RunningChromeVersion"), b"149").unwrap();
        fs::write(profile.join("Default/Cookies"), b"keep-me").unwrap();

        strip_for_seal(profile).unwrap();

        assert!(!profile.join("SingletonLock").exists());
        assert!(!profile.join("Default/GPUCache").exists());
        assert!(!profile.join("Default/RunningChromeVersion").exists());
        assert!(profile.join("Default/Cookies").exists(), "real data kept");
    }

    #[test]
    fn strip_is_idempotent_on_clean_profile() {
        let tmp = tempfile::tempdir().unwrap();
        strip_for_seal(tmp.path()).unwrap();
        strip_for_seal(tmp.path()).unwrap();
    }

    #[test]
    fn clone_reproduces_files_via_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let template = tmp.path().join("tmpl");
        fs::create_dir_all(template.join("Default")).unwrap();
        fs::write(template.join("Local State"), b"state").unwrap();
        fs::write(template.join("Default/Cookies"), b"c").unwrap();

        let dest = tmp.path().join("sessions/abc/user-data-dir");
        clone_template(&template, &dest, ProfileTier::PlainCopy).unwrap();

        assert_eq!(fs::read(dest.join("Local State")).unwrap(), b"state");
        assert_eq!(fs::read(dest.join("Default/Cookies")).unwrap(), b"c");
    }

    #[test]
    fn clone_reflink_tier_falls_back_to_copy_when_unsupported() {
        let tmp = tempfile::tempdir().unwrap();
        let template = tmp.path().join("tmpl");
        fs::create_dir_all(&template).unwrap();
        fs::write(template.join("f"), b"x").unwrap();
        let dest = tmp.path().join("out");
        clone_template(&template, &dest, ProfileTier::Reflink).unwrap();
        assert_eq!(fs::read(dest.join("f")).unwrap(), b"x");
    }

    #[test]
    fn template_path_is_keyed_by_browser() {
        let p = template_path(Path::new("/data"), "chrome-149.0.7827.0");
        assert!(p.ends_with("templates/chrome-149.0.7827.0"));
    }
}
