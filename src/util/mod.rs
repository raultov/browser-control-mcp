//! Small, protocol-neutral helpers shared by the tools and both backends.

pub mod bounded;
pub mod headers;
pub mod js;
pub mod json;
#[cfg(test)]
pub(crate) mod log_capture;
pub mod sync;
pub mod text;
pub mod url;

/// Applies the directives that keep secrets out of the logs whatever the
/// user asked for: `cdp-lite` 0.3 logs whole command payloads at `debug`
/// (including `Fetch.continueWithAuth` credentials) and `rust-mcp-transport`
/// logs every incoming JSON-RPC line at `trace` (a tool call's arguments:
/// proxy passwords, raw protocol params), so both are capped at `info`.
/// Later directives win over `RUST_LOG`'s.
pub fn log_filter(base: tracing_subscriber::EnvFilter) -> tracing_subscriber::EnvFilter {
    ["cdp_lite=info", "rust_mcp_transport=info"]
        .into_iter()
        .filter_map(|directive| directive.parse().ok())
        .fold(base, tracing_subscriber::EnvFilter::add_directive)
}

#[cfg(test)]
mod tests {
    /// AUD-RW-01: the MCP transport's raw payload lines (`trace`, the whole
    /// tool call: proxy password, raw protocol params) never reach the logs,
    /// while the server's own trace lines still do.
    #[test]
    fn transport_payloads_are_not_logged_at_trace() {
        // Given the production filter at its most verbose
        let (logs, _guard) = super::log_capture::capture();
        // When the transport logs a raw payload and the server a trace line
        tracing::trace!(target: "rust_mcp_transport::mcp_stream", "raw payload: AUDIT-PROXY-PW");
        tracing::trace!(target: "browser_control_mcp::handler", "own trace line");
        // Then only the server's line is captured
        let text = logs.text();
        assert!(!text.contains("AUDIT-PROXY-PW"), "{text}");
        assert!(text.contains("own trace line"), "{text}");
    }
}
