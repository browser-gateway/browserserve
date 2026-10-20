//! Per-session upstream proxy.
//!
//! A session may ask to send all of its browser traffic through a proxy that
//! needs a login, uses TLS, or speaks SOCKS5 with a login: forms Chrome cannot
//! use on its own. For such a session browserserve starts a small relay on
//! `127.0.0.1` that Chrome uses as a plain HTTP proxy with no login; the relay
//! carries every connection to the one upstream proxy and adds the login there.
//! The upstream address and login never reach Chrome's command line.

mod address;
mod dial;
mod head;
mod relay;
mod socks;
mod upstream;

pub use address::is_blocked_ip;
pub use relay::{Relay, RelayOptions};
pub use upstream::{ProxyScheme, ProxyUpstream, UpstreamParseError};

use thiserror::Error;

/// Why the relay could not reach a target through the upstream proxy.
#[derive(Debug, Error)]
pub enum ProxyError {
    /// The upstream host did not resolve, or resolved only to blocked addresses.
    #[error("proxy address not allowed: {0}")]
    AddressNotAllowed(String),
    /// TCP, TLS or protocol failure talking to the upstream proxy.
    #[error("proxy unreachable: {0}")]
    Unreachable(String),
    /// The upstream proxy rejected the login.
    #[error("proxy login failed")]
    LoginFailed,
    /// The upstream proxy refused to open the requested connection.
    #[error("proxy refused the connection: {0}")]
    Refused(String),
    /// Connecting and negotiating with the upstream took too long.
    #[error("proxy did not answer in time")]
    Timeout,
}
