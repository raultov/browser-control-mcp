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

use std::process::ExitCode;

/// Runs the server until its transport ends and returns the process exit
/// code. Work in progress: the server itself arrives with the imported code.
pub fn run() -> ExitCode {
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_succeeds_on_the_empty_crate() {
        assert_eq!(run(), ExitCode::SUCCESS);
    }
}
