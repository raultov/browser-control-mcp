//! `browser-control-mcp` binary: a thin wrapper around [`browser_control_mcp::run`].

use browser_control_mcp::{Args, run};
use clap::Parser;

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(run(args));
    // The stdin reader is a blocking thread that returns only on input or
    // EOF: after a signal, waiting for it would keep the process alive.
    runtime.shutdown_background();
    code
}
