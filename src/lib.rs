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

pub mod cli;
mod handler;

pub use cli::Args;
use handler::{HandlerParams, McpHandler};
use rust_mcp_sdk::{error::SdkResult, mcp_server::server_runtime, schema::*, *};

/// Runs the MCP server over stdio until its transport ends.
pub async fn run(args: Args) -> SdkResult<()> {
    eprintln!("[DEBUG] Starting with args: {:?}", args);

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

    // Create handler with max_instances support
    let handler_params = HandlerParams {
        host: args.host,
        port: args.port,
        local_only: args.local,
        enable_automation: args.enable_automation,
        headless: args.headless,
        user_profile: args.user_profile,
        allow_cookie_import: args.allow_cookie_import,
        proxy_server: args.proxy_server,
        proxy_username: args.proxy_username,
        proxy_password: args.proxy_password,
        enable_webmcp: args.enable_webmcp,
    };
    let handler = McpHandler::new_with_params(handler_params);
    handler
        .registry
        .max_instances
        .store(args.max_instances, std::sync::atomic::Ordering::SeqCst);
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
        eprintln!("Server error: {:?}", e);
        return Err(e);
    }
    Ok(())
}
