//! Self-hosted browser runtime: launches, supervises, and tears down isolated
//! Chrome sessions. One fresh browser process and one fresh profile directory
//! per session, never reused.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "browserserve targets Linux (the production Docker image); macOS builds exist only for the development loop"
);

pub mod bridge;
pub mod capacity;
pub mod chrome;
pub mod config;
pub mod factory;
pub mod linux;
pub mod logging;
pub mod pool;
pub mod pressure;
pub mod rss;
pub mod server;
pub mod session_dirs;
pub mod template;
