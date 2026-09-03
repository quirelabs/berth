//! The tokio side of the house.
//!
//! GPUI runs its own executor and russh needs tokio's reactor, so the two are
//! bridged with channels at exactly this boundary, never by mixing executors
//! inside a core crate. One multi-threaded runtime lives for the life of the
//! process; UI code only ever holds a `Handle`.

use std::sync::OnceLock;

use tokio::runtime::{Handle, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// The shared runtime, started on first use.
pub fn handle() -> Handle {
    RUNTIME
        .get_or_init(|| Runtime::new().expect("the tokio runtime should start"))
        .handle()
        .clone()
}

/// Run a future on the tokio runtime and hand the result back through a
/// oneshot, so a GPUI task can await it without tokio ever touching the
/// foreground executor.
pub fn spawn<F>(future: F) -> futures::channel::oneshot::Receiver<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    handle().spawn(async move {
        let _ = tx.send(future.await);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_starts_and_is_reused() {
        let a = handle();
        let b = handle();
        // Both handles must refer to the same runtime, or tasks spawned from
        // different call sites would not share a reactor.
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn spawn_delivers_a_result() {
        let rx = spawn(async { 6 * 7 });
        assert_eq!(futures::executor::block_on(rx).unwrap(), 42);
    }
}
