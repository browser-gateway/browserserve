//! browserserve command line interface.

#![forbid(unsafe_code)]

use browserserve::chrome::{self, LaunchSpec};
use browserserve::{config, logging, session_dirs};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "browserserve",
    version,
    about = "Self-hosted browser server",
    after_help = "by Monostellar Labs \u{b7} https://monostellar.com"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Launch one browser, verify CDP readiness, tear it down, report timings.
    Check {
        /// Config file path. Default: `BROWSERSERVE_CONFIG`, then `./browserserve.yml` if present.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Browser executable override.
        #[arg(long)]
        chrome: Option<PathBuf>,
    },
    /// Diagnose the host: browser discovery, limits, data directory.
    Doctor {
        /// Config file path. Default: `BROWSERSERVE_CONFIG`, then `./browserserve.yml` if present.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Run the server: warm pool, CDP WebSocket endpoint, and health probes.
    Serve {
        /// Config file path. Default: `BROWSERSERVE_CONFIG`, then `./browserserve.yml` if present.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    logging::init();
    let cli = Cli::parse();
    let env: HashMap<String, String> = std::env::vars().collect();
    let outcome = match cli.command {
        Cmd::Check { config, chrome } => run_check(config.as_deref(), chrome, &env).await,
        Cmd::Doctor { config } => run_doctor(config.as_deref(), &env),
        Cmd::Serve { config } => run_serve(config.as_deref(), &env).await,
    };
    outcome.unwrap_or_else(|message| {
        eprintln!("error: {message}");
        ExitCode::FAILURE
    })
}

async fn run_serve(
    config_path: Option<&Path>,
    env: &HashMap<String, String>,
) -> Result<ExitCode, String> {
    let loaded = config::load_from_env(env, config_path).map_err(|e| e.to_string())?;
    if loaded.config.chrome.transport == config::Transport::Port {
        return Err(String::from(
            "chrome.transport: port is not available yet; use pipe",
        ));
    }
    browserserve::server::serve(loaded).await?;
    Ok(ExitCode::SUCCESS)
}

async fn run_check(
    config_path: Option<&Path>,
    chrome_override: Option<PathBuf>,
    env: &HashMap<String, String>,
) -> Result<ExitCode, String> {
    let loaded = config::load_from_env(env, config_path).map_err(|e| e.to_string())?;
    let mut cfg = loaded.config;
    if let Some(path) = chrome_override {
        cfg.chrome.executable_path = Some(path);
    }
    if cfg.chrome.transport == config::Transport::Port {
        return Err(String::from(
            "chrome.transport: port is not available yet; use pipe",
        ));
    }

    let executable =
        chrome::find_chrome(cfg.chrome.executable_path.as_deref()).map_err(|e| e.to_string())?;
    println!("executable     {}", executable.display());

    let dirs = session_dirs::SessionDirs::provision_plain(&cfg.data_dir, uuid::Uuid::new_v4())
        .await
        .map_err(|e| format!("provision session dir: {e}"))?;
    println!("user data dir  {}", dirs.user_data_dir.display());

    let spec = LaunchSpec {
        executable: &executable,
        user_data_dir: &dirs.user_data_dir,
        no_sandbox: cfg.chrome.no_sandbox,
        extra_flags: &cfg.chrome.extra_flags,
        launch_timeout: Duration::from_millis(cfg.chrome.launch_timeout_ms),
        max_frame_bytes: cfg.chrome.max_frame_bytes,
    };
    let browser = match chrome::launch(&spec).await {
        Ok(browser) => browser,
        Err(e) => {
            let _ = dirs.teardown().await;
            let mut message = format!("launch failed: {e}");
            if let Some(hint) = e.remediation() {
                message.push_str("\n\nhint: ");
                message.push_str(hint);
            }
            return Err(message);
        }
    };
    println!("pid            {}", browser.pid);
    println!("product        {}", browser.version.product);
    println!("protocol       {}", browser.version.protocol_version);
    println!("cdp ready in   {} ms", browser.ready_in.as_millis());

    let report = chrome::teardown(browser, Duration::from_millis(cfg.session.kill_grace_ms)).await;
    println!(
        "teardown       graceful={} sigkill={} reaped={} exit={}",
        report.graceful,
        report.escalated_sigkill,
        report.reaped,
        report.exit_status.as_deref().unwrap_or("unknown")
    );

    dirs.teardown()
        .await
        .map_err(|e| format!("session dir teardown: {e}"))?;
    println!("session dir    removed");

    if report.reaped {
        println!("check          PASS");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("check          FAIL: browser was not reaped");
        Ok(ExitCode::FAILURE)
    }
}

fn run_doctor(
    config_path: Option<&Path>,
    env: &HashMap<String, String>,
) -> Result<ExitCode, String> {
    let loaded = config::load_from_env(env, config_path).map_err(|e| e.to_string())?;
    let cfg = loaded.config;
    let mut ok = true;

    match chrome::find_chrome(cfg.chrome.executable_path.as_deref()) {
        Ok(path) => {
            println!("browser        {}", path.display());
            match std::process::Command::new(&path).arg("--version").output() {
                Ok(out) if out.status.success() => {
                    println!(
                        "version        {}",
                        String::from_utf8_lossy(&out.stdout).trim()
                    );
                }
                _ => println!("version        could not run --version"),
            }
        }
        Err(e) => {
            ok = false;
            println!("browser        MISSING: {e}");
        }
    }

    match nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE) {
        Ok((soft, _hard)) => {
            println!("fd limit       {soft}");
            if soft < 4096 {
                println!("               warning: below 4096; each session holds several fds");
            }
        }
        Err(_) => println!("fd limit       unknown"),
    }

    match probe_writable(&cfg.data_dir) {
        Ok(()) => println!("data dir       {} writable", cfg.data_dir.display()),
        Err(e) => {
            ok = false;
            println!(
                "data dir       {} NOT writable: {e}",
                cfg.data_dir.display()
            );
        }
    }

    #[cfg(target_os = "linux")]
    match nix::sys::statvfs::statvfs("/dev/shm") {
        Ok(stat) => {
            let available_mb = stat.blocks_available() * stat.fragment_size() / (1024 * 1024);
            println!("/dev/shm       {available_mb} MiB available");
            if available_mb < 512 {
                println!("               warning: run the container with --shm-size=1g or larger");
            }
        }
        Err(_) => println!("/dev/shm       unknown"),
    }

    #[cfg(target_os = "macos")]
    println!("platform       macOS: development only; production is the Linux image");

    let tiers = browserserve::linux::probe::detect(&cfg.data_dir);
    println!("isolation      {}", tiers.summary());
    for note in &tiers.notes {
        println!("               {note}");
    }

    if ok {
        println!("doctor         PASS");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("doctor         FAIL");
        Ok(ExitCode::FAILURE)
    }
}

fn probe_writable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(".doctor-write-probe");
    std::fs::write(&probe, b"probe")?;
    std::fs::remove_file(&probe)
}
