//! The Tokio side of the async bridge.
//!
//! Drivers run on one shared multi-threaded Tokio runtime. Callers on any
//! other executor talk to it through `futures` channels, which are
//! runtime-agnostic, so no Tokio type has to cross into the UI crate.

use std::sync::OnceLock;

use futures::channel::oneshot;
use tokio::runtime::{Builder, Handle, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Handle to the shared driver runtime, started on first use.
///
/// # Panics
///
/// Panics if the operating system refuses to create the runtime's threads.
/// Nothing in the application can work without them, so there is no error to
/// return to.
pub fn handle() -> Handle {
    RUNTIME
        .get_or_init(|| {
            // Every driver's TLS goes through rustls; settle its provider
            // before any of them connects.
            crate::tls_provider();
            Builder::new_multi_thread()
                .worker_threads(4)
                .thread_name("sonde-driver")
                .enable_all()
                .build()
                .expect("failed to start the driver runtime")
        })
        .handle()
        .clone()
}

/// Run `future` on the driver runtime and hand back its output through a
/// receiver any executor can await.
///
/// Dropping the receiver does not stop the future; it only discards the
/// output. Work that must be stoppable takes a channel it can observe closing.
pub fn spawn<F>(future: F) -> oneshot::Receiver<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    handle().spawn(async move {
        // The receiver may be gone; the output is then simply not wanted.
        let _ = tx.send(future.await);
    });
    rx
}
