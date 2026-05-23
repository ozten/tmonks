use std::net::SocketAddr;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use tokio_util::sync::CancellationToken;

use tmons::{
    auth::{self, print_startup_url},
    cli,
    observability,
    server::{self, AppState},
};

fn main() -> ExitCode {
    let args = cli::Cli::parse();
    if let Err(e) = args.validate() {
        eprintln!("error: {e}");
        return ExitCode::from(2);
    }

    observability::init(args.verbose);

    if args.no_auth {
        eprintln!();
        eprintln!("  ⚠️  --no-auth is active. Anyone on this host can drive your shells.");
        eprintln!("      Starting in 3 seconds. Press Ctrl-C to abort.");
        eprintln!();
        std::thread::sleep(std::time::Duration::from_secs(3));
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: failed to start tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    match runtime.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "fatal");
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run(args: cli::Cli) -> Result<()> {
    let token = auth::Token::new_random().context("generate auth token")?;

    let bind_addr = SocketAddr::new(args.bind, args.port);
    let listener = server::bind(bind_addr).await?;
    let bound_addr = listener.local_addr().context("read bound socket address")?;

    let shutdown = CancellationToken::new();
    let state = AppState::new(
        token,
        args.no_auth,
        args.socket.clone(),
        bound_addr,
        shutdown.clone(),
    );

    let url = print_startup_url(bound_addr, &state.token);
    println!("Open: {url}");
    if args.no_auth {
        println!("Note: --no-auth is active. Token in URL is ignored.");
    }

    let router = server::router(state);

    let shutdown_for_signal = shutdown.clone();
    tokio::spawn(async move {
        wait_for_shutdown().await;
        tracing::info!("shutdown requested");
        shutdown_for_signal.cancel();
    });

    server::serve(listener, router, shutdown).await?;
    Ok(())
}

#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = sigterm.recv() => {},
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
