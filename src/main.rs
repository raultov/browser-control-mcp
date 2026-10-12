//! `browser-control-mcp` binary: a thin wrapper around [`browser_control_mcp::run`].

use browser_control_mcp::{Args, run};
use clap::Parser;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    run(Args::parse()).await
}
