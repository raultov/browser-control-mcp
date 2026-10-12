//! Small, protocol-neutral helpers shared by the tools and both backends.

pub mod js;
#[cfg(test)]
pub(crate) mod log_capture;
pub mod text;
pub mod url;

/// Applies the directives that keep secrets out of the logs whatever the
/// user asked for: `cdp-lite` 0.3 logs whole command payloads at `debug`
/// (including `Fetch.continueWithAuth` credentials), so it is capped at
/// `info`. Later directives win over `RUST_LOG`'s.
pub fn log_filter(base: tracing_subscriber::EnvFilter) -> tracing_subscriber::EnvFilter {
    match "cdp_lite=info".parse() {
        Ok(directive) => base.add_directive(directive),
        Err(_) => base,
    }
}
