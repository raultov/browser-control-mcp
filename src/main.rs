//! `browser-control-mcp` binary: a thin wrapper around [`browser_control_mcp::run`].

use std::process::ExitCode;

fn main() -> ExitCode {
    browser_control_mcp::run()
}
