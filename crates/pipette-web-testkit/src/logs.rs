//! Capturing what a test's code logs.
//!
//! With one dispatcher alive, `tracing` caches a callsite's interest from the
//! first thread to reach it, so a sibling test with none caches *never* and
//! empties the capture; [`register_second_dispatcher`] keeps a second alive.

use std::sync::Arc;

use anyhow::Context;

/// Drive `f` to completion with the formatter writing into a buffer, down to
/// `TRACE`, and return what it wrote.
///
/// A closure, not a future: the dispatcher is thread-local, so `f` gets a
/// current-thread runtime that keeps every task on the capturing thread.
pub fn capture_logs<T>(
    f: impl FnOnce(&tokio::runtime::Runtime) -> T,
) -> anyhow::Result<(T, String)> {
    capture_logs_at_level(tracing::Level::TRACE, f)
}

/// Keep a do-nothing dispatcher alive for the rest of the process, so a capturing
/// one is never the only dispatcher (see the module doc). Call it before installing one.
pub fn register_second_dispatcher() {
    static SECOND: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
    SECOND.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
}

/// [`capture_logs`] with a caller-chosen ceiling on the level.
///
/// `Level::INFO` is the binary's default, so a test that passes it holds the
/// code to what production records.
pub fn capture_logs_at_level<T>(
    max_level: tracing::Level,
    f: impl FnOnce(&tokio::runtime::Runtime) -> T,
) -> anyhow::Result<(T, String)> {
    #[derive(Clone)]
    struct Writer(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Writer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            // Poisoned by a panic on another thread: surface it as a write error.
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("log capture buffer poisoned"))?
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Writer {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    register_second_dispatcher();

    let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(Writer(Arc::clone(&buf)))
        .with_ansi(false)
        .with_max_level(max_level)
        .finish();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("test runtime")?;
    let out = tracing::subscriber::with_default(subscriber, || f(&runtime));
    let captured = buf
        .lock()
        .map_err(|e| anyhow::anyhow!("log capture buffer poisoned: {e}"))?;
    let text = String::from_utf8_lossy(&captured).into_owned();
    Ok((out, text))
}
