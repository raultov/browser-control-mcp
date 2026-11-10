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
    let (args, secrets) = match prepare(args) {
        Ok(prepared) => prepared,
        Err(e) => {
            tracing::error!(error = %e, "cannot start");
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
    let rust_log = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(util::log_filter(&rust_log))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

/// Builds the MCP handler from the command line. Logs the arguments through
/// their redacting `Debug` (never the raw values).
/// The arguments and secrets the server starts with, or why it cannot
/// start: the message names the flag, the file and the line, never a value.
fn prepare(mut args: Args) -> Result<(Args, secrets::Secrets), String> {
    checked_proxy_server(&mut args)?;
    let secrets = match args.secrets.secrets.as_deref() {
        None => secrets::Secrets::default(),
        Some(path) => secrets::Secrets::load(path)?,
    };
    Ok((args, secrets))
}

/// `--proxy-server` as the browsers take it: `host:port` gets `http://`;
/// a proxy of another shape stops the server at start-up (#110: Chrome ran
/// with `--proxy-server=<unparsable url>` and every navigation failed).
fn checked_proxy_server(args: &mut Args) -> Result<(), String> {
    if let Some(raw) = &args.proxy.proxy_server {
        let proxy =
            util::url::proxy_with_scheme(raw).map_err(|e| format!("--proxy-server: {e}"))?;
        args.proxy.proxy_server = Some(proxy);
    }
    Ok(())
}

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

    /// PROXY-11 (#110): `--proxy-server host:port` reaches the browsers as
    /// `http://host:port`; a proxy of another shape stops the start-up with
    /// a message that never repeats the value.
    #[test]
    fn proxy_server_gets_a_scheme_at_start_up() {
        let args = |proxy: &str| Args::parse_from(["browser-control-mcp", "--proxy-server", proxy]);
        let mut plain = args("127.0.0.1:18790");
        checked_proxy_server(&mut plain).unwrap();
        assert_eq!(
            plain.proxy.proxy_server.as_deref(),
            Some("http://127.0.0.1:18790")
        );
        let mut odd = args("ftp://u:s3cr3t@h:21");
        let error = checked_proxy_server(&mut odd).unwrap_err();
        assert!(
            error.starts_with("--proxy-server: ") && !error.contains("s3cr3t"),
            "{error}"
        );
        let mut none = Args::parse_from(["browser-control-mcp"]);
        checked_proxy_server(&mut none).unwrap();
        assert_eq!(none.proxy.proxy_server, None);
    }

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
