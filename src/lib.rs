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
pub mod util;

pub use cli::Args;
use handler::{HandlerParams, McpHandler};
use rust_mcp_sdk::{error::SdkResult, mcp_server::server_runtime, schema::*, *};

/// Runs the MCP server over stdio until its transport ends.
pub async fn run(args: Args) -> SdkResult<()> {
    init_logging();

    let instructions = concat!(
        "Browser Control MCP Server Instructions:\n",
        "- Chrome instances are launched lazily on the first tool call that requires connection.\n",
        "- Instances: a 'default' instance always exists. To run additional isolated browser sessions, call `open_instance` with a unique 'label'; that label becomes the returned instance_id. Pass the instance_id as the 'instance_id' argument of any tool to address that instance (omit it for the default). Use `list_instances` to enumerate them and `close_instance` to stop one.\n",
        "- Tabs: within an instance, create extra tabs with `open_tab` and enumerate them with `list_tabs`. Every tool accepts a 'tab_id' argument; omit it to target the active tab, and use `switch_tab` to change which tab is active. Per-tab state (console logs, breakpoints, network traffic) is isolated between tabs.\n",
        "- Cookie import: when the server is launched with `--allow-cookie-import`, tools (`navigate`, `open_instance`, `restart_chrome`) support `copy_cookies: true` to populate isolated instances with the user's real Chrome session. Always ask the user before copying their real cookies.\n",
        "- Proxy support: when the server is launched with `--proxy-server <URL>`, Chrome instances route traffic through the proxy and proxy tools (`enable_proxy_auth`) are exposed.\n",
        "- WebMCP: when the server is launched with `--enable-webmcp`, Chrome instances enable WebMCP testing features and WebMCP tools (`webmcp_list_tools`, `webmcp_invoke_tool`, `webmcp_get_invocation`, `webmcp_list_invocations`) are exposed."
    );

    let server_info = InitializeResult {
        server_info: Implementation {
            name: env!("CARGO_PKG_NAME").into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some("Browser Control MCP".into()),
            description: Some("Control and debug browsers at runtime (Chrome via CDP; Firefox support in progress).".into()),
            icons: vec![],
            website_url: None,
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools { list_changed: None }),
            ..Default::default()
        },
        protocol_version: ProtocolVersion::V2025_11_25.into(),
        instructions: Some(instructions.to_string()),
        meta: None,
    };

    let transport = StdioTransport::new(TransportOptions::default())?;

    let handler = build_handler(args);
    let handler = handler.to_mcp_server_handler();
    let server = server_runtime::create_server(mcp_server::McpServerOptions {
        server_details: server_info,
        transport,
        handler,
        task_store: None,
        client_task_store: None,
        message_observer: None,
    });

    if let Err(e) = server.start().await {
        tracing::error!(error = %e, "server stopped with an error");
        return Err(e);
    }
    Ok(())
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
fn build_handler(args: Args) -> McpHandler {
    tracing::info!(args = ?args, "starting browser-control-mcp");
    let max_instances = args.max_instances;
    let handler = McpHandler::new_with_params(HandlerParams {
        host: args.host,
        port: args.port,
        browser: args.browser,
        server: args.server,
        proxy: args.proxy,
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
        let _handler = build_handler(args);
        // Then the start-up line was logged without any secret
        let text = logs.text();
        assert!(text.contains("starting browser-control-mcp"), "{text}");
        assert!(!text.contains("s3cr3t") && !text.contains("u:p@"), "{text}");
    }
}
