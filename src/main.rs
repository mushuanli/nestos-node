//! Binary entry point: load configuration, wire the runtime, serve, and shut
//! down in the order that keeps the exports fenced.
//!
//! Shutdown has two stages because a graceful stop has two waits:
//!
//! 1. stop admitting commands and let in-flight file work finish, bounded by
//!    each plugin drain deadline;
//! 2. let open connections end on their own, bounded by [`CONNECTION_GRACE`].
//!
//! The second bound exists because a stalled download would otherwise keep the
//! process alive forever: the client never reads, so the connection never
//! closes. The order matters — the file gate is drained before the process is
//! allowed to exit.

use cells::core::events::emit;
use cells::{app::Application, config::launch};
use serde_json::json;
use std::{sync::Arc, time::Duration};

mod startup;

/// Upper bound on how long open connections may take to finish afterwards.
const CONNECTION_GRACE: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            emit(
                cells::core::events::Level::Error,
                if std::env::args()
                    .nth(1)
                    .is_some_and(|arg| arg == "access" || arg == "sync" || arg == "plugins")
                {
                    "cli.command_failed"
                } else {
                    "server.start_failed"
                },
                json!({"version": env!("CARGO_PKG_VERSION"), "error": error.to_string()}),
            );
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .first()
        .is_some_and(|arg| arg == "--version" || arg == "-V")
    {
        println!("cells {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{}", cells::cli::HELP);
        return Ok(());
    }
    eprintln!("cells {}", env!("CARGO_PKG_VERSION"));
    if args.first().is_some_and(|arg| arg == "plugins") {
        return cells::cli::plugins(&args[1..]).map_err(Into::into);
    }
    if args.first().is_some_and(|arg| arg == "access") {
        return cells::cli::access(&args[1..]).map_err(Into::into);
    }
    if args.first().is_some_and(|s| s == "sync") {
        return cells::sync::admin(&args[1..]).map_err(Into::into);
    }
    if args.len() > 1 || args.first().is_some_and(|arg| arg.starts_with('-')) {
        return Err("Invalid server arguments; use cells --help".into());
    }
    let path = launch::resolve(args.first().map(String::as_str))?;
    let config = launch::load(&path)?;
    cells::core::events::set_level(config.log_level);
    let log_file = cells::core::events::init_file(&config.logging, &path)?;
    eprintln!("  Log file: {}", log_file.display());
    emit(
        cells::core::events::Level::Info,
        "server.starting",
        json!({"version": env!("CARGO_PKG_VERSION"), "config": path, "logFile": log_file}),
    );
    let application = Arc::new(Application::from_config(&config)?);
    let app = application.router()?;
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    application.start().await?;
    let state = application.state();
    emit(
        cells::core::events::Level::Info,
        "server.ready",
        json!({"address": listener.local_addr()?.to_string(), "serverId": state.auth.server_id(),
        "version": env!("CARGO_PKG_VERSION"), "logFile": log_file,
        "principals":state.auth.clients().iter().map(|c| c.authorization()).collect::<Vec<_>>(),
        "execution": state.plugins.enabled("process"), "plugins":state.plugins.statuses(), "exportCount": config.exports.len(), "exports": startup::export_access(state)}),
    );
    if cells::core::events::enabled(cells::core::events::Level::Info) {
        eprintln!(
            "cells listening on {} (config {})",
            listener.local_addr()?,
            path.display()
        );
        startup::print(&config, listener.local_addr()?, state);
        if config.principals.is_empty() && state.auth.client(0).username().is_none() {
            eprintln!("  API Key: {}", state.auth.client(0).secret());
        }
    }
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(application.clone()))
        .await;
    application.shutdown().await?;
    result?;
    Ok(())
}

/// Close plugin admission and drain resources before the connection grace deadline.
async fn shutdown(application: Arc<Application>) {
    let signal = shutdown_signal().await.unwrap_or("signal-error");
    emit(
        cells::core::events::Level::Info,
        "server.stopping",
        json!({"signal":signal}),
    );
    if application.shutdown().await.is_err() {
        emit(
            cells::core::events::Level::Error,
            "server.plugin_shutdown_failed",
            json!({"code":"PLUGIN_SHUTDOWN_FAILED"}),
        );
    }
    // Nothing left to protect; do not wait forever for a stalled client.
    tokio::spawn(async move {
        tokio::time::sleep(CONNECTION_GRACE).await;
        emit(
            cells::core::events::Level::Info,
            "server.connection_grace_elapsed",
            json!({}),
        );
        std::process::exit(0);
    });
}

async fn shutdown_signal() -> std::io::Result<&'static str> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result=tokio::signal::ctrl_c()=>{result?;Ok("SIGINT")},
        _=terminate.recv()=>Ok("SIGTERM"),
    }
}
