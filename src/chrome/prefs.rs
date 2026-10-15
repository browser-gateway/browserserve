//! Profile preferences written before every launch.

use serde_json::{Map, Value};
use std::io;
use std::path::Path;

/// Returns `existing` preferences (JSON, may be absent or unreadable) with
/// Chrome's leaked-password check turned off. That check opens a browser-level
/// dialog after a person submits a known-leaked password; a headless page
/// cannot show or dismiss it, and it swallows all input to the tab.
#[must_use]
pub fn with_leak_check_off(existing: Option<&str>) -> String {
    let mut root = existing
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new()));
    if let Some(obj) = root.as_object_mut() {
        let profile = obj
            .entry("profile")
            .or_insert_with(|| Value::Object(Map::new()));
        if !profile.is_object() {
            *profile = Value::Object(Map::new());
        }
        if let Some(profile) = profile.as_object_mut() {
            profile.insert(
                String::from("password_manager_leak_detection"),
                Value::Bool(false),
            );
        }
    }
    root.to_string()
}

/// Writes the launch preferences into `<user_data_dir>/Default/Preferences`,
/// keeping every other preference already there.
///
/// # Errors
///
/// Propagates I/O errors creating the directory or writing the file.
pub async fn apply_launch_prefs(user_data_dir: &Path) -> io::Result<()> {
    let dir = user_data_dir.join("Default");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join("Preferences");
    let existing = tokio::fs::read_to_string(&path).await.ok();
    tokio::fs::write(&path, with_leak_check_off(existing.as_deref())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leak_check(prefs: &str) -> Option<Value> {
        let v: Value = serde_json::from_str(prefs).unwrap();
        v.pointer("/profile/password_manager_leak_detection")
            .cloned()
    }

    #[test]
    fn creates_the_preference_when_there_is_no_file() {
        assert_eq!(
            leak_check(&with_leak_check_off(None)),
            Some(Value::Bool(false))
        );
    }

    #[test]
    fn keeps_every_other_preference() {
        let out = with_leak_check_off(Some(
            r#"{"profile":{"name":"x","password_manager_leak_detection":true},"intl":{"accept_languages":"de"}}"#,
        ));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(leak_check(&out), Some(Value::Bool(false)));
        assert_eq!(v.pointer("/profile/name"), Some(&Value::from("x")));
        assert_eq!(
            v.pointer("/intl/accept_languages"),
            Some(&Value::from("de"))
        );
    }

    #[test]
    fn replaces_unreadable_or_odd_files() {
        for bad in ["not json", "[1,2]", r#"{"profile":5}"#] {
            assert_eq!(
                leak_check(&with_leak_check_off(Some(bad))),
                Some(Value::Bool(false)),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn writes_into_the_default_profile_folder() {
        let tmp = tempfile::tempdir().unwrap();
        apply_launch_prefs(tmp.path()).await.unwrap();
        let written = std::fs::read_to_string(tmp.path().join("Default/Preferences")).unwrap();
        assert_eq!(leak_check(&written), Some(Value::Bool(false)));
    }
}
