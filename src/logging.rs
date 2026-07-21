//! Structured logging: JSON when detached, compact when on a terminal.

use std::io::IsTerminal;

/// Initializes the global tracing subscriber.
///
/// Level filtering comes from the `BROWSERSERVE_LOG` environment variable (`info` when
/// unset). Output goes to stderr: compact format on a terminal, JSON otherwise.
/// Safe to call more than once; later calls are no-ops.
pub fn init() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("BROWSERSERVE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    if std::io::stderr().is_terminal() {
        let _ = builder.compact().try_init();
    } else {
        let _ = builder.json().try_init();
    }
}
