//! # browser-control-mcp
//!
//! An MCP (Model Context Protocol) server that lets an LLM agent **control
//! and debug** real browsers at runtime: breakpoints, pause on load, call
//! frames, evaluation in a paused frame, stepping and script search on
//! Chrome (CDP) and Firefox (WebDriver BiDi + the Firefox Remote Debugging
//! Protocol).
//!
//! The binary (`src/main.rs`) only parses the command line and calls
//! [`run`]; everything else lives in this library so that every public item
//! is documented and testable.

pub mod backend;
pub mod cli;
mod handler;
pub mod secrets;
pub mod server_info;
pub mod util;

pub use cli::Args;
use handler::{HandlerParams, McpHandler};
use rust_mcp_sdk::{mcp_server::server_runtime, *};

/// Runs the MCP server over stdio until its transport ends or the process
/// is asked to stop, then stops every browser it started (Firefox keeps an
/// orphan automation session otherwise). The exit code reports failures.
pub async fn run(args: Args) -> std::process::ExitCode {
    init_logging();
    let transport = match StdioTransport::new(TransportOptions::default()) {
        Ok(transport) => transport,
        Err(e) => {
            tracing::error!(error = %e, "cannot open the stdio transport");
            return std::process::ExitCode::FAILURE;
        }
    };
    let secrets = match args.secrets.secrets.as_deref().map(secrets::Secrets::load) {
        None => secrets::Secrets::default(),
        Some(Ok(secrets)) => secrets,
        // The message names the file and the line, never a value.
        Some(Err(e)) => {
            tracing::error!(error = %e, "cannot load the secrets");
            return std::process::ExitCode::FAILURE;
        }
    };
    let handler = build_handler(args, secrets);
    let shutdown = handler.shutdown_hook();
    let server = server_runtime::create_server(mcp_server::McpServerOptions {
        server_details: server_info::initialize_result(),
        transport,
        handler: handler.to_mcp_server_handler(),
        task_store: None,
        client_task_store: None,
        message_observer: None,
    });
    let served = tokio::select! {
        result = server.start() => result.map_err(|e| e.to_string()),
        () = stop_signal() => Ok(()),
    };
    if let Err(e) = &served {
        tracing::error!(error = %e, "server stopped with an error");
    }
    let failures = shutdown.stop_all(std::time::Duration::from_secs(5)).await;
    if served.is_ok() && failures.is_empty() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

/// Ctrl-C on every OS, and SIGTERM on Unix. If a handler cannot be
/// installed it never fires (the server then stops only with its transport).
async fn stop_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("stop requested; shutting down");
}

/// Logs go to stderr (stdout is the MCP channel), filtered by `RUST_LOG`
/// (default `info`). A second call is a no-op, which keeps tests simple.
fn init_logging() {
    let base = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(util::log_filter(base))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

/// Builds the MCP handler from the command line. Logs the arguments through
/// their redacting `Debug` (never the raw values).
fn build_handler(args: Args, secrets: secrets::Secrets) -> McpHandler {
    tracing::info!(args = ?args, "starting browser-control-mcp");
    let max_instances = args.max_instances;
    let handler = McpHandler::new_with_params(HandlerParams {
        host: args.host,
        port: args.port,
        default_browser: args.browser_kind,
        browser: args.browser,
        server: args.server,
        proxy: args.proxy,
        firefox: args.firefox,
        upload: args.upload,
        tools: args.tools,
        secrets,
    });
    handler
        .registry
        .max_instances
        .store(max_instances, std::sync::atomic::Ordering::SeqCst);
    handler
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// SEC-02: Startup logs never carry secrets.
    #[test]
    fn startup_logs_never_contain_the_proxy_password() {
        // Given a captured subscriber and arguments with a proxy password
        let (logs, _guard) = util::log_capture::capture();
        let args = Args::parse_from([
            "browser-control-mcp",
            "--proxy-server",
            "http://u:p@proxy:8080",
            "--proxy-password",
            "s3cr3t",
        ]);
        // When the start-up path builds the handler
        let _handler = build_handler(args, secrets::Secrets::default());
        // Then the start-up line was logged without any secret
        let text = logs.text();
        assert!(text.contains("starting browser-control-mcp"), "{text}");
        assert!(!text.contains("s3cr3t") && !text.contains("u:p@"), "{text}");
    }
}
