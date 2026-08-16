//! The tokio ↔ GPUI bridge.
//!
//! GPUI runs its own executor. OpenDAL's network-backed services run on reqwest
//! and need a live tokio reactor, so polling an OpenDAL future directly on
//! GPUI's executor panics with `there is no reactor running`.
//!
//! The bridge is [`Rt::spawn`]: the future runs on the tokio runtime, and the
//! `JoinHandle` it returns is itself a `Future`, so it can be awaited from
//! GPUI's executor. Every backend call goes through this one door.
//!
//! Three rules hold everywhere in this workspace:
//!
//! 1. An `Operator` is never awaited on a GPUI thread.
//! 2. Nothing ever calls `Handle::block_on` from a GPUI thread — that deadlocks.
//! 3. The UI layer holds a `Vfs`, never an `Operator`.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Builder, Handle, Runtime};

use crate::{Error, Result};

/// The runtime outlives every `Rt` handle and is never dropped, which also
/// avoids the "cannot drop a runtime from within an async context" panic.
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

#[derive(Clone)]
pub struct Rt {
    handle: Handle,
}

impl Rt {
    pub fn new() -> Result<Self> {
        let runtime = RUNTIME.get_or_init(|| {
            Builder::new_multi_thread()
                .worker_threads(4)
                .thread_name("roam-io")
                // enable_all() brings up the IO and time drivers. Local `fs`
                // does not need them, but M2's reqwest-backed services do, and
                // a missing reactor fails at runtime rather than compile time.
                .enable_all()
                .build()
                .expect("failed to build the roam-io tokio runtime")
        });

        Ok(Self {
            handle: runtime.handle().clone(),
        })
    }

    /// Attach to an already-running tokio runtime instead of creating one.
    /// Used by tests running under `#[tokio::test]`.
    pub fn from_current() -> Result<Self> {
        Handle::try_current()
            .map(|handle| Self { handle })
            .map_err(|e| Error::Config(format!("no tokio runtime available: {e}")))
    }

    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Run a fallible future on the tokio runtime and await it from any
    /// executor. A cancelled or panicked task surfaces as [`Error::Cancelled`].
    pub fn spawn<F, T>(&self, fut: F) -> impl Future<Output = Result<T>> + Send + 'static
    where
        F: Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let join = self.handle.spawn(fut);
        async move { join.await.map_err(Error::from)? }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawned_work_is_awaitable_off_the_tokio_runtime() {
        let rt = Rt::new().unwrap();

        // futures::executor::block_on stands in for GPUI's executor here: a
        // foreign executor that knows nothing about tokio.
        let got = futures::executor::block_on(rt.spawn(async { Ok::<_, Error>(7u32) }));
        assert_eq!(got.unwrap(), 7);
    }

    #[test]
    fn a_panicking_task_becomes_cancelled_not_a_process_abort() {
        let rt = Rt::new().unwrap();
        let got: Result<()> = futures::executor::block_on(rt.spawn(async {
            panic!("boom");
        }));
        assert!(got.unwrap_err().is_cancelled());
    }

    #[test]
    fn errors_propagate_through_the_bridge() {
        let rt = Rt::new().unwrap();
        let got: Result<()> =
            futures::executor::block_on(rt.spawn(async { Err(Error::Config("bad root".into())) }));
        assert_eq!(got.unwrap_err().user_message(), "bad root");
    }
}
