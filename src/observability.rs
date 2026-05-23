use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Initialise `tracing_subscriber` with sensible defaults for tmons.
///
/// * `RUST_LOG` overrides the default filter.
/// * `--verbose` upgrades the default to debug.
/// * The query-string token (`?t=…`) and the session cookie value are NEVER
///   logged. We achieve this by restricting `TraceLayer` to fields we control
///   (see `server::trace_layer`) — this function only sets up the formatter.
pub fn init(verbose: bool) {
    let default = if verbose { "tmons=debug,warn" } else { "tmons=info,warn" };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_target(true)
                .with_level(true)
                .with_writer(std::io::stderr),
        )
        .init();
}
