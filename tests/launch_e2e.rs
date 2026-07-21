//! Real-Chrome integration tests. Run with: `cargo test --test launch_e2e -- --ignored`

#![forbid(unsafe_code)]

use browserserve::chrome::{LaunchSpec, find_chrome, launch, teardown};
use browserserve::session_dirs::SessionDirs;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::time::Duration;

fn spec<'a>(
    executable: &'a std::path::Path,
    user_data_dir: &'a std::path::Path,
    launch_timeout: Duration,
) -> LaunchSpec<'a> {
    LaunchSpec {
        executable,
        user_data_dir,
        no_sandbox: false,
        extra_flags: &[],
        launch_timeout,
        max_frame_bytes: 64 * 1024 * 1024,
    }
}

fn group_alive(pid: i32) -> bool {
    kill(Pid::from_raw(-pid), None).is_ok()
}

#[tokio::test]
#[ignore = "requires a local Chrome installation"]
async fn launch_ready_teardown_leaves_nothing() {
    let executable = find_chrome(None).expect("chrome required for this test");
    let tmp = tempfile::tempdir().unwrap();
    let dirs = SessionDirs::provision_plain(tmp.path(), uuid::Uuid::new_v4())
        .await
        .unwrap();

    let browser = launch(&spec(
        &executable,
        &dirs.user_data_dir,
        Duration::from_secs(30),
    ))
    .await
    .expect("launch should succeed");
    let pid = browser.pid;
    assert!(!browser.version.product.is_empty());
    assert!(browser.version.product.contains('/'));
    assert!(group_alive(pid));

    let report = teardown(browser, Duration::from_secs(5)).await;
    assert!(report.reaped, "direct child must be reaped");
    assert!(!group_alive(pid), "whole process group must be gone");

    dirs.teardown().await.unwrap();
    assert!(!dirs.root.exists());
}

#[tokio::test]
#[ignore = "requires a local Chrome installation"]
async fn two_sessions_get_disjoint_profiles() {
    let executable = find_chrome(None).expect("chrome required for this test");
    let tmp = tempfile::tempdir().unwrap();
    let dirs_a = SessionDirs::provision_plain(tmp.path(), uuid::Uuid::new_v4())
        .await
        .unwrap();
    let dirs_b = SessionDirs::provision_plain(tmp.path(), uuid::Uuid::new_v4())
        .await
        .unwrap();
    assert_ne!(dirs_a.user_data_dir, dirs_b.user_data_dir);

    let browser_a = launch(&spec(
        &executable,
        &dirs_a.user_data_dir,
        Duration::from_secs(30),
    ))
    .await
    .expect("first launch");
    let browser_b = launch(&spec(
        &executable,
        &dirs_b.user_data_dir,
        Duration::from_secs(30),
    ))
    .await
    .expect("second concurrent launch");
    assert_ne!(browser_a.pid, browser_b.pid);

    let report_a = teardown(browser_a, Duration::from_secs(5)).await;
    let report_b = teardown(browser_b, Duration::from_secs(5)).await;
    assert!(report_a.reaped && report_b.reaped);

    dirs_a.teardown().await.unwrap();
    dirs_b.teardown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a local Chrome installation"]
async fn ready_timeout_kills_the_spawned_group() {
    let executable = find_chrome(None).expect("chrome required for this test");
    let tmp = tempfile::tempdir().unwrap();
    let dirs = SessionDirs::provision_plain(tmp.path(), uuid::Uuid::new_v4())
        .await
        .unwrap();

    let result = launch(&spec(
        &executable,
        &dirs.user_data_dir,
        Duration::from_millis(1),
    ))
    .await;
    assert!(result.is_err(), "1 ms budget must time out");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let marker = dirs.user_data_dir.to_string_lossy().into_owned();
    let leaked = std::process::Command::new("pgrep")
        .args(["-f", &marker])
        .status()
        .is_ok_and(|s| s.success());
    assert!(!leaked, "no process may still reference the session dir");

    dirs.teardown().await.unwrap();
}
