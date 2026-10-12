//! Errors every browser backend can report to a tool.

use super::capabilities::{BrowserKind, Capability};

/// A failure a tool turns into a clear MCP error. Messages never contain
/// protocol payloads (they may carry credentials).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackendError {
    /// The instance's browser (or the server's flags) cannot serve the tool.
    #[error("`{tool}` is not available on {browser}: {reason}")]
    Unsupported {
        /// The tool that was called.
        tool: String,
        /// The first capability it needs and the instance lacks.
        capability: Capability,
        /// The instance's browser.
        browser: BrowserKind,
        /// Why it is missing.
        reason: &'static str,
    },
    /// The browser could not be found or started.
    #[error("{0}")]
    Launch(String),
    /// A protocol command failed.
    #[error("{browser} protocol error in {method}: {message}")]
    Protocol {
        /// The instance's browser.
        browser: BrowserKind,
        /// The command.
        method: String,
        /// What the browser (or the client library) said.
        message: String,
    },
    /// The arguments do not make sense for this browser.
    #[error("{0}")]
    InvalidArgument(String),
}

impl From<BackendError> for rust_mcp_sdk::schema::CallToolError {
    fn from(error: BackendError) -> Self {
        Self::from_message(error.to_string())
    }
}
