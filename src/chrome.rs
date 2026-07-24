//! Chrome process management: discovery, launch, readiness, teardown.

pub mod client;
pub mod find;
pub mod flags;
pub mod kill;
pub mod launch;
pub mod pipe;

pub use client::{CdpClient, CdpError};
pub use find::{FindError, find_chrome};
pub use kill::{TeardownReport, force_kill_group, teardown};
pub use launch::{Browser, BrowserVersion, LaunchError, LaunchSpec, launch};
pub use pipe::{CdpPipe, CdpReader, CdpWriter, PipeError};
