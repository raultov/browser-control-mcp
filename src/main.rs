//! `browser-control-mcp` binary: a thin wrapper around [`browser_control_mcp::run`].

use browser_control_mcp::{Args, run};
use clap::Parser;
use rust_mcp_sdk::error::SdkResult;

#[tokio::main]
async fn main() -> SdkResult<()> {
    run(Args::parse()).await
}
