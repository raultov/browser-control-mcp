//! Test helper: captures everything `tracing` writes while a guard lives, so
//! tests can assert that secrets never reach the logs.

use std::io::Write;
use std::sync::{Arc, Mutex};

/// Captured log text; clone it to read it after the code under test ran.
#[derive(Clone, Default)]
pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    /// Everything logged so far.
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs a subscriber with the production filter at its most verbose
/// (`trace`, then [`super::log_filter`]) writing to the returned buffer for the
/// current thread (a `#[tokio::test]` runtime is single-threaded, so spawned
/// tasks log into it too) until the guard is dropped.
pub(crate) fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(super::log_filter(tracing_subscriber::EnvFilter::new(
            "trace",
        )))
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    (captured, tracing::subscriber::set_default(subscriber))
}
