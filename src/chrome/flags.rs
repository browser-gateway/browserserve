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
pub const DEFAULT_FLAGS: &[&str] = &[
    "--headless",
    "--allow-pre-commit-input",
    "--disable-background-networking",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-breakpad",
    "--disable-crash-reporter",
    "--disable-client-side-phishing-detection",
    "--disable-component-extensions-with-background-pages",
    "--disable-component-update",
    "--disable-default-apps",
    "--disable-dev-shm-usage",
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

/// Inputs for assembling the final argument list.
#[derive(Debug)]
pub struct FlagOptions<'a> {
    /// Fresh, session-owned profile directory.
    pub user_data_dir: &'a Path,
    /// Append `--no-sandbox`. Off unless the environment cannot sandbox.
    pub no_sandbox: bool,
    /// User-supplied flags appended after the built-in set.
    pub extra: &'a [String],
}

/// Builds the complete argument list for one browser launch (pipe transport).
#[must_use]
pub fn build_flags(opts: &FlagOptions<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = DEFAULT_FLAGS.iter().map(OsString::from).collect();
    args.push(OsString::from("--remote-debugging-pipe"));
    let mut user_data_dir = OsString::from("--user-data-dir=");
    user_data_dir.push(opts.user_data_dir);
    args.push(user_data_dir);
    if opts.no_sandbox {
        args.push(OsString::from("--no-sandbox"));
    }
    args.extend(opts.extra.iter().map(OsString::from));
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
            extra,
        })
        .into_iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
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
