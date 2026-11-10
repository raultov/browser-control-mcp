//! Small, protocol-neutral helpers shared by the tools and both backends.

pub mod bounded;
pub mod headers;
pub mod js;
pub mod json;
#[cfg(test)]
pub(crate) mod log_capture;
pub mod outline;
pub mod sync;
pub mod text;
pub mod url;

/// Applies the directives that keep secrets out of the logs whatever the
/// user asked for: `cdp-lite` 0.3 logs whole command payloads at `debug`
/// (including `Fetch.continueWithAuth` credentials) and `rust-mcp-transport`
/// logs every incoming JSON-RPC line at `trace` (a tool call's arguments:
/// proxy passwords, raw protocol params), so both are capped at `info`. So
/// are the wire layers under the browser clients, which dump every frame
/// or byte they move at `trace` (#114: a `--secrets` value typed with
/// `fill_input`): the WebSocket (`tungstenite`, `tokio_tungstenite`) and
/// HTTP (`hyper`, `hyper_util`, `h2`) crates. Later directives win over
/// `RUST_LOG`'s.
pub fn log_filter(base: tracing_subscriber::EnvFilter) -> tracing_subscriber::EnvFilter {
    [
        "cdp_lite=info",
        "rust_mcp_transport=info",
        "tungstenite=info",
        "tokio_tungstenite=info",
        "hyper=info",
        "hyper_util=info",
        "h2=info",
    ]
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

    /// AUD-RW-02 (#114): the WebSocket and HTTP layers under the browser
    /// clients dump every frame at `trace` (a `--secrets` value typed with
    /// fill_input, a proxy password); none of their lines reach the logs
    /// below `info`.
    #[test]
    fn wire_frames_are_not_logged_at_trace() {
        // Given the production filter at its most verbose
        let (logs, _guard) = super::log_capture::capture();
        // When each wire layer logs a frame at trace and debug
        tracing::trace!(target: "tungstenite::protocol", "frame: WIRE-SECRET-1");
        tracing::debug!(target: "tungstenite::protocol", "frame: WIRE-SECRET-2");
        tracing::trace!(target: "tokio_tungstenite::compat", "frame: WIRE-SECRET-3");
        tracing::trace!(target: "hyper::proto::h1::io", "bytes: WIRE-SECRET-4");
        tracing::trace!(target: "hyper_util::client::legacy", "bytes: WIRE-SECRET-5");
        tracing::trace!(target: "h2::codec", "bytes: WIRE-SECRET-6");
        // Then none is captured, but a warning still is
        tracing::warn!(target: "tungstenite::protocol", "connection reset");
        let text = logs.text();
        assert!(!text.contains("WIRE-SECRET"), "{text}");
        assert!(text.contains("connection reset"), "{text}");
    }
}
