//! The locked Chrome flag set for server-side fleets.

use std::ffi::OsString;
use std::path::Path;

/// Flags applied to every launched browser.
///
/// The set is the 2026 consensus across major automation launchers: headless,
/// no first-run surfaces, no phone-home services, no throttling of background
/// work, deterministic rendering color space, keychain and password store
/// disabled, no crash-restore prompt (a seeded profile dir reads as "crashed"
/// after a kill-based teardown). Sandbox stays ON; `--no-sandbox` is a
/// per-config opt-in.
///
/// Not in this constant set: `--disable-dev-shm-usage`. It routes Chrome's shared
/// memory to disk at a real performance cost, so it is added per-launch ONLY when
/// `/dev/shm` is too small to run browsers safely (see [`small_dev_shm`]) — a
/// too-small `/dev/shm` crashes renderers under concurrency, not just slows them.
/// When `/dev/shm` is adequately sized (`docker run --shm-size=1g`), the faster
/// shared-memory path is kept. `check`/`doctor` still report the `/dev/shm` size.
pub const DEFAULT_FLAGS: &[&str] = &[
    "--headless",
    "--allow-pre-commit-input",
    "--disable-background-networking",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-renderer-backgrounding",
    "--disable-breakpad",
    "--disable-crash-reporter",
    "--disable-client-side-phishing-detection",
    "--disable-component-extensions-with-background-pages",
    "--disable-component-update",
    "--disable-default-apps",
    "--disable-extensions",
    "--disable-hang-monitor",
    "--disable-ipc-flooding-protection",
    "--disable-popup-blocking",
    "--disable-prompt-on-repost",
    "--disable-session-crashed-bubble",
    "--hide-crash-restore-bubble",
    "--disable-sync",
    "--disable-features=Translate,MediaRouter,DialMediaRouteProvider,OptimizationHints,AcceptCHFrame,DestroyProfileOnBrowserClose",
    "--export-tagged-pdf",
    "--force-color-profile=srgb",
    "--metrics-recording-only",
    "--mute-audio",
    "--no-default-browser-check",
    "--no-first-run",
    "--password-store=basic",
    "--use-mock-keychain",
    "--window-size=1280,720",
];

/// Minimum `/dev/shm` size (MiB) below which Chrome is launched off it. Matches
/// the `doctor`/`check` warning threshold.
#[cfg(target_os = "linux")]
const SHM_MIN_MB: u64 = 512;

/// Whether `/dev/shm` is too small to run Chrome renderers safely, so the launch
/// should add `--disable-dev-shm-usage`. Detected once by the caller (I/O is kept
/// out of [`build_flags`], which stays pure). Non-Linux hosts do not use
/// `/dev/shm`, so this is always false there.
#[must_use]
pub fn small_dev_shm() -> bool {
    #[cfg(target_os = "linux")]
    {
        match nix::sys::statvfs::statvfs("/dev/shm") {
            Ok(stat) => {
                let bytes = stat.blocks_available() * stat.fragment_size();
                bytes < SHM_MIN_MB * 1024 * 1024
            }
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Inputs for assembling the final argument list.
#[derive(Debug)]
pub struct FlagOptions<'a> {
    /// Fresh, session-owned profile directory.
    pub user_data_dir: &'a Path,
    /// Append `--no-sandbox`. Off unless the environment cannot sandbox.
    pub no_sandbox: bool,
    /// Append `--disable-dev-shm-usage`. On only when `/dev/shm` is too small
    /// (see [`small_dev_shm`]); routes Chrome's shared memory to disk.
    pub disable_dev_shm: bool,
    /// User-supplied flags appended after the built-in set.
    pub extra: &'a [String],
}

/// Chromium switches whose values are NOT unioned across repeated occurrences
/// on the command line — only the last occurrence is honored
/// (`base/command_line.cc`: `AppendSwitchNative` is a plain map assignment).
/// A user-supplied flag for one of these must be merged with our default,
/// never appended as a second, silently-overriding occurrence.
const MERGEABLE_FEATURE_SWITCHES: &[&str] = &["--disable-features=", "--enable-features="];

/// Merges any `extra` occurrence of a [`MERGEABLE_FEATURE_SWITCHES`] prefix
/// into `DEFAULT_FLAGS`'s own value for that switch (comma-joined, deduped,
/// order-preserving), and returns the merged flags plus whatever's left of
/// `extra` once those occurrences are removed.
fn merge_feature_switches(extra: &[String]) -> (Vec<OsString>, Vec<&String>) {
    let mut merged = Vec::new();
    for &prefix in MERGEABLE_FEATURE_SWITCHES {
        let mut values: Vec<&str> = DEFAULT_FLAGS
            .iter()
            .find(|f| f.starts_with(prefix))
            .map_or_else(Vec::new, |f| f[prefix.len()..].split(',').collect());
        for flag in extra {
            if let Some(v) = flag.strip_prefix(prefix) {
                for part in v.split(',') {
                    if !values.contains(&part) {
                        values.push(part);
                    }
                }
            }
        }
        if !values.is_empty() {
            merged.push(OsString::from(format!("{prefix}{}", values.join(","))));
        }
    }
    let remaining = extra
        .iter()
        .filter(|f| !MERGEABLE_FEATURE_SWITCHES.iter().any(|p| f.starts_with(p)))
        .collect();
    (merged, remaining)
}

/// Builds the complete argument list for one browser launch (pipe transport).
#[must_use]
pub fn build_flags(opts: &FlagOptions<'_>) -> Vec<OsString> {
    let (merged_feature_switches, remaining_extra) = merge_feature_switches(opts.extra);
    let mut args: Vec<OsString> = DEFAULT_FLAGS
        .iter()
        .filter(|f| !MERGEABLE_FEATURE_SWITCHES.iter().any(|p| f.starts_with(p)))
        .map(OsString::from)
        .collect();
    args.extend(merged_feature_switches);
    args.push(OsString::from("--remote-debugging-pipe"));
    let mut user_data_dir = OsString::from("--user-data-dir=");
    user_data_dir.push(opts.user_data_dir);
    args.push(user_data_dir);
    if opts.no_sandbox {
        args.push(OsString::from("--no-sandbox"));
    }
    if opts.disable_dev_shm {
        args.push(OsString::from("--disable-dev-shm-usage"));
    }
    args.extend(remaining_extra.into_iter().map(OsString::from));
    args.push(OsString::from("about:blank"));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags_for(no_sandbox: bool, extra: &[String]) -> Vec<String> {
        build_flags(&FlagOptions {
            user_data_dir: Path::new("/tmp/profile-x"),
            no_sandbox,
            disable_dev_shm: false,
            extra,
        })
        .into_iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
    }

    #[test]
    fn disable_dev_shm_added_only_when_requested() {
        assert!(
            !flags_for(false, &[])
                .iter()
                .any(|f| f == "--disable-dev-shm-usage"),
            "off by default (only added when /dev/shm is too small)"
        );
        let with: Vec<String> = build_flags(&FlagOptions {
            user_data_dir: Path::new("/tmp/p"),
            no_sandbox: false,
            disable_dev_shm: true,
            extra: &[],
        })
        .into_iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
        assert!(with.iter().any(|f| f == "--disable-dev-shm-usage"));
    }

    #[test]
    fn sandbox_stays_on_by_default() {
        let flags = flags_for(false, &[]);
        assert!(!flags.iter().any(|f| f == "--no-sandbox"));
        assert!(flags.contains(&String::from("--headless")));
        assert!(flags.contains(&String::from("--remote-debugging-pipe")));
        assert!(flags.contains(&String::from("--user-data-dir=/tmp/profile-x")));
        assert_eq!(flags.last().map(String::as_str), Some("about:blank"));
    }

    #[test]
    fn no_sandbox_is_opt_in() {
        assert!(flags_for(true, &[]).iter().any(|f| f == "--no-sandbox"));
    }

    #[test]
    fn extra_flags_come_after_defaults_before_url() {
        let extra = vec![String::from("--lang=de")];
        let flags = flags_for(false, &extra);
        let lang = flags.iter().position(|f| f == "--lang=de").unwrap();
        let url = flags.iter().position(|f| f == "about:blank").unwrap();
        assert!(lang < url);
    }

    #[test]
    fn crash_restore_prompt_is_suppressed() {
        let flags = flags_for(false, &[]);
        assert!(
            flags
                .iter()
                .any(|f| f == "--disable-session-crashed-bubble")
        );
        assert!(flags.iter().any(|f| f == "--hide-crash-restore-bubble"));
    }

    #[test]
    fn cookie_encryption_is_portable() {
        let flags = flags_for(false, &[]);
        assert!(flags.iter().any(|f| f == "--password-store=basic"));
        assert!(flags.iter().any(|f| f == "--use-mock-keychain"));
    }

    #[test]
    fn renderer_liveness_trio_is_complete() {
        let flags = flags_for(false, &[]);
        for required in [
            "--disable-background-timer-throttling",
            "--disable-backgrounding-occluded-windows",
            "--disable-renderer-backgrounding",
        ] {
            assert!(
                flags.iter().any(|f| f == required),
                "{required} must ship; a headless renderer without it can be \
                 deprioritized and stop painting under CPU pressure, which \
                 silently starves screencast frames on a static page"
            );
        }
    }

    #[test]
    fn user_disable_features_merges_instead_of_overriding() {
        // Chromium keeps only the LAST `--disable-features` occurrence on the
        // command line (base/command_line.cc: AppendSwitchNative is a plain
        // map assignment, values across repeated occurrences are not unioned).
        // A user-supplied `--disable-features` must merge with our default,
        // never silently re-enable everything the default disabled.
        let extra = vec![String::from("--disable-features=SomeUserFeature")];
        let flags = flags_for(false, &extra);

        let disable_features_flags: Vec<&String> = flags
            .iter()
            .filter(|f| f.starts_with("--disable-features="))
            .collect();
        assert_eq!(
            disable_features_flags.len(),
            1,
            "exactly one --disable-features flag must be emitted, got {disable_features_flags:?}"
        );
        let merged = disable_features_flags[0];
        assert!(
            merged.contains("SomeUserFeature"),
            "user value must survive: {merged}"
        );
        assert!(
            merged.contains("DestroyProfileOnBrowserClose"),
            "default value must survive alongside the user's: {merged}"
        );
    }

    #[test]
    fn user_enable_features_merges_instead_of_duplicating() {
        let extra = vec![String::from("--enable-features=SomeUserFeature")];
        let flags = flags_for(false, &extra);
        let enable_features_flags: Vec<&String> = flags
            .iter()
            .filter(|f| f.starts_with("--enable-features="))
            .collect();
        assert_eq!(
            enable_features_flags.len(),
            1,
            "exactly one --enable-features flag must be emitted, got {enable_features_flags:?}"
        );
        assert!(enable_features_flags[0].contains("SomeUserFeature"));
    }

    #[test]
    fn no_legacy_or_contested_flags() {
        let flags = flags_for(false, &[]);
        for banned in [
            "--disable-gpu",
            "--single-process",
            "--no-zygote",
            "--enable-automation",
        ] {
            assert!(!flags.iter().any(|f| f == banned), "{banned} must not ship");
        }
    }
}
