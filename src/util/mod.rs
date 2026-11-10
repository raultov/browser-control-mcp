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

/// The crates capped at `info` whatever `RUST_LOG` asks for, because they
/// log payloads below it: `cdp-lite` 0.3 logs whole command payloads at
/// `debug` (including `Fetch.continueWithAuth` credentials),
/// `rust-mcp-transport` logs every incoming JSON-RPC line at `trace` (a
/// tool call's arguments: proxy passwords, raw protocol params), `reqwest`
/// logs the proxy URL (credentials included) at `debug`, and the WebSocket
/// (`tungstenite`, `tokio_tungstenite`) and HTTP (`hyper`, `hyper_util`,
/// `h2`) crates dump every frame or byte they move at `trace` (#114: a
/// `--secrets` value typed with `fill_input`).
const CAPPED: [&str; 8] = [
    "cdp_lite",
    "rust_mcp_transport",
    "reqwest",
    "tungstenite",
    "tokio_tungstenite",
    "hyper",
    "hyper_util",
    "h2",
];

/// The log filter for `rust_log` (`RUST_LOG`'s directives), with the
/// crates of `CAPPED` at `info`. `EnvFilter` lets a more specific
/// directive win (`tungstenite::protocol=trace` over `tungstenite=info`),
/// so a directive of `rust_log` that asks a capped crate or one of its
/// modules for more than `info` is dropped first; one that asks a whole
/// capped crate for less is kept instead of the cap. An invalid `rust_log`
/// means `info`.
pub fn log_filter(rust_log: &str) -> tracing_subscriber::EnvFilter {
    let kept: Vec<&str> = rust_log
        .split(',')
        .map(str::trim)
        .filter(|directive| !directive.is_empty() && !lifts_a_cap(directive))
        .collect();
    let base = tracing_subscriber::EnvFilter::builder()
        .parse(kept.join(","))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // A kept `name=<level>` already asks for `info` or less: the cap would
    // replace it (same target), so it is left as asked.
    let quieter = |name: &&str| {
        kept.iter().any(|d| {
            d.split_once('=')
                .is_some_and(|(target, _)| target.trim() == *name)
        })
    };
    CAPPED
        .iter()
        .filter(|name| !quieter(name))
        .filter_map(|name| format!("{name}=info").parse().ok())
        .fold(base, tracing_subscriber::EnvFilter::add_directive)
}

/// Whether `directive` (`target[span]=level`) asks a [`CAPPED`] crate, or
/// one of its modules, for more than `info` (no level means `trace`).
fn lifts_a_cap(directive: &str) -> bool {
    let target = directive
        .split(['[', '='])
        .next()
        .unwrap_or_default()
        .trim();
    let capped = CAPPED.iter().any(|name| {
        target == *name
            || target
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with("::"))
    });
    if !capped {
        return false;
    }
    let level = directive.rsplit_once('=').map(|(_, level)| level.trim());
    let quiet = ["off", "error", "warn", "info"];
    !level.is_some_and(|level| quiet.iter().any(|q| level.eq_ignore_ascii_case(q)))
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

    /// AUD-RW-03 (#114): a `RUST_LOG` that names a capped crate, or one of
    /// its modules (more specific directives win in `EnvFilter`), cannot
    /// lift the cap; one that asks for less still applies, and the rest of
    /// `RUST_LOG` is kept.
    #[test]
    fn rust_log_cannot_lift_the_caps() {
        // Given RUST_LOG asking for the wire layers' frames by module
        let rust_log = "warn,tungstenite::protocol=trace,reqwest::connect=debug,\
            hyper=TRACE,tokio_tungstenite,h2=warn,browser_control_mcp=debug";
        let (logs, _guard) = super::log_capture::capture_with(rust_log);
        // When they log at trace and debug, and info
        tracing::trace!(target: "tungstenite::protocol", "frame: WIRE-SECRET-1");
        tracing::debug!(target: "reqwest::connect", "proxy(http://u:WIRE-SECRET-2@p)");
        tracing::trace!(target: "hyper::proto", "bytes: WIRE-SECRET-3");
        tracing::trace!(target: "tokio_tungstenite", "frame: WIRE-SECRET-4");
        tracing::info!(target: "h2::codec", "h2 info is below its warn");
        tracing::info!(target: "tungstenite::protocol", "tungstenite info");
        tracing::debug!(target: "browser_control_mcp::handler", "own debug line");
        tracing::info!(target: "other_crate", "other info is below warn");
        // Then no frame is captured, and the other directives still apply
        let text = logs.text();
        assert!(!text.contains("WIRE-SECRET"), "{text}");
        assert!(
            !text.contains("h2 info") && !text.contains("other info"),
            "{text}"
        );
        assert!(
            text.contains("tungstenite info") && text.contains("own debug line"),
            "{text}"
        );
    }
}
