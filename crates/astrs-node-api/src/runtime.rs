//! [`NodeRuntime`] — the async runtime a node's session lives on.
//!
//! Blueprint §9.1 shows a node whose `main` is an ordinary synchronous
//! function, and also promises `recv_async()`. Both have to work, from the
//! same [`crate::Node`], without the author choosing a colour up front. This
//! module is the one place that reconciles them.
//!
//! # Three situations, three behaviours
//!
//! | Where `Node::init…` is called | Runtime | Blocking calls |
//! |---|---|---|
//! | An ordinary `fn main` | A two-worker multi-thread runtime this node owns | Work |
//! | Inside `#[tokio::main]` / a multi-thread runtime | The caller's | Work, through [`tokio::task::block_in_place`] |
//! | Inside a current-thread runtime (`#[tokio::test]`) | The caller's | Refused with [`NodeError::BlockingInAsync`] |
//!
//! The third row is the interesting one. A current-thread runtime has exactly
//! one thread; blocking it is not slow, it is a *deadlock* — the task that
//! would deliver the awaited value can never run. Returning a typed error that
//! names the `_async` twin to call instead is the only honest answer, and it
//! turns a hang into a compile-cycle-length fix.
//!
//! # Examples
//!
//! ```
//! use astrs_node_api::runtime::NodeRuntime;
//!
//! let runtime = NodeRuntime::acquire()?;
//! assert!(runtime.is_owned(), "no runtime was running, so one was built");
//! assert!(runtime.can_block());
//! # Ok::<(), astrs_node_api::NodeError>(())
//! ```

use std::future::Future;
use std::sync::Arc;

use std::sync::OnceLock;

use tokio::runtime::{Builder, Handle, RuntimeFlavor};
use tokio::task::JoinHandle;

/// Backs [`NodeRuntime::fallback_handle`]; see that function.
static FALLBACK_HANDLE: OnceLock<Handle> = OnceLock::new();

use crate::error::{NodeError, Result};

/// The number of worker threads an owned node runtime starts with.
///
/// Two, not one: the session's reader and writer tasks are both long-lived
/// and both spend their lives parked on I/O, and a single worker would make
/// their wakeups serialise behind each other for no reason. Not more than
/// two, because a node's own work belongs on the node's own threads — this
/// runtime exists to move frames, not to be a thread pool.
pub const OWNED_WORKER_THREADS: usize = 2;

/// A tokio runtime owned by a node, shut down without blocking.
///
/// Dropping a `tokio::runtime::Runtime` **panics** when it happens inside an
/// asynchronous context, because the drop blocks until the worker threads
/// join. A node built outside a runtime and dropped inside one is an ordinary
/// thing for a test or a `spawn_blocking` body to do, so this wrapper calls
/// [`shutdown_background`](tokio::runtime::Runtime::shutdown_background)
/// instead: the workers are detached and reaped by the process rather than
/// joined by the dropping thread.
#[derive(Debug)]
pub struct OwnedRuntime(Option<tokio::runtime::Runtime>);

impl OwnedRuntime {
    /// The handle to spawn on.
    ///
    /// The `Option` is `Some` for the whole life of the value; it is emptied
    /// only by [`Drop`], after which nothing can call this.
    fn handle(&self) -> Option<&Handle> {
        self.0.as_ref().map(tokio::runtime::Runtime::handle)
    }

    /// Runs `future` to completion on this runtime.
    fn block_on<F: Future>(&self, future: F) -> Option<F::Output> {
        self.0.as_ref().map(|runtime| runtime.block_on(future))
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// The runtime a node's background tasks run on.
#[derive(Debug, Clone)]
pub enum NodeRuntime {
    /// A runtime this node built and owns; it shuts down when the last
    /// handle drops.
    Owned(Arc<OwnedRuntime>),
    /// A runtime the caller already had running.
    Borrowed(Handle),
}

impl NodeRuntime {
    /// The runtime to use here: the caller's if one is running, otherwise a
    /// freshly built one.
    ///
    /// # Errors
    ///
    /// [`NodeError::Runtime`] when no runtime is running and one cannot be
    /// built (which in practice means the process cannot start threads).
    pub fn acquire() -> Result<Self> {
        match Handle::try_current() {
            Ok(handle) => Ok(Self::Borrowed(handle)),
            Err(_) => Self::owned(),
        }
    }

    /// A freshly built runtime, ignoring any the caller has.
    ///
    /// # Errors
    ///
    /// [`NodeError::Runtime`] when the runtime cannot be built.
    pub fn owned() -> Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(OWNED_WORKER_THREADS)
            .thread_name("astrs-node")
            .enable_all()
            .build()
            .map_err(|error| NodeError::Runtime(error.to_string()))?;
        Ok(Self::Owned(Arc::new(OwnedRuntime(Some(runtime)))))
    }

    /// Whether this node built its own runtime.
    #[must_use]
    pub const fn is_owned(&self) -> bool {
        matches!(self, Self::Owned(_))
    }

    /// The tokio handle to spawn on.
    #[must_use]
    pub fn handle(&self) -> &Handle {
        match self {
            // The inner `Option` is emptied only by `OwnedRuntime::drop`, at
            // which point no `&self` can exist; the fallback keeps this total
            // without an unwrap.
            Self::Owned(runtime) => runtime.handle().unwrap_or_else(|| self.fallback_handle()),
            Self::Borrowed(handle) => handle,
        }
    }

    /// The handle used if an owned runtime were ever observed after its drop
    /// — unreachable, and never taken in practice.
    fn fallback_handle(&self) -> &Handle {
        match self {
            Self::Borrowed(handle) => handle,
            Self::Owned(_) => FALLBACK_HANDLE.get_or_init(|| {
                Builder::new_current_thread()
                    .build()
                    .map_or_else(|_| Handle::current(), |runtime| runtime.handle().clone())
            }),
        }
    }

    /// Spawns a background task.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle().spawn(future)
    }

    /// Whether a blocking call from *this* thread is safe.
    ///
    /// False exactly in the deadlock case: the calling thread is the single
    /// worker of a current-thread runtime.
    #[must_use]
    pub fn can_block(&self) -> bool {
        match Handle::try_current() {
            Ok(handle) => handle.runtime_flavor() != RuntimeFlavor::CurrentThread,
            Err(_) => true,
        }
    }

    /// Runs `future` to completion, blocking the calling thread.
    ///
    /// # Errors
    ///
    /// [`NodeError::BlockingInAsync`] when the calling thread is a
    /// current-thread runtime's only worker; `method` and `alternative` name
    /// what was called and what to call instead.
    pub fn block_on<F: Future>(
        &self,
        method: &'static str,
        alternative: &'static str,
        future: F,
    ) -> Result<F::Output> {
        match Handle::try_current() {
            Ok(handle) => match handle.runtime_flavor() {
                RuntimeFlavor::CurrentThread => Err(NodeError::BlockingInAsync {
                    method,
                    alternative,
                }),
                // `block_in_place` moves the current task off its worker so
                // the runtime keeps making progress while this thread blocks.
                _ => Ok(tokio::task::block_in_place(|| handle.block_on(future))),
            },
            Err(_) => match self {
                Self::Owned(runtime) => runtime
                    .block_on(future)
                    .ok_or_else(|| NodeError::Runtime("the node runtime is gone".to_owned())),
                // A borrowed handle whose runtime is not *this* thread's:
                // blocking here cannot deadlock that runtime, and
                // `Handle::block_on` is the documented way to do it.
                Self::Borrowed(handle) => Ok(handle.block_on(future)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn outside_a_runtime_one_is_built_and_blocking_works() {
        let runtime = NodeRuntime::acquire().unwrap();
        assert!(runtime.is_owned());
        assert!(runtime.can_block());
        let value = runtime
            .block_on("test", "test_async", async { 7_u32 })
            .unwrap();
        assert_eq!(value, 7);
    }

    #[test]
    fn an_owned_runtime_drives_spawned_tasks_without_a_block_on() {
        let runtime = NodeRuntime::owned().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let _task = runtime.spawn(async move {
            let _ = tx.send(11_u32);
        });
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
            11
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_current_thread_runtime_refuses_to_be_blocked() {
        let runtime = NodeRuntime::acquire().unwrap();
        assert!(!runtime.is_owned());
        assert!(!runtime.can_block());
        let error = runtime
            .block_on("EventStream::recv", "EventStream::recv_async", async {})
            .unwrap_err();
        assert!(matches!(
            error,
            NodeError::BlockingInAsync {
                method: "EventStream::recv",
                alternative: "EventStream::recv_async"
            }
        ));
        assert!(error.is_usage_error());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_multi_thread_runtime_is_borrowed_and_can_block() {
        let runtime = NodeRuntime::acquire().unwrap();
        assert!(!runtime.is_owned());
        assert!(runtime.can_block());
        let value = runtime
            .block_on("test", "test_async", async { 3_u8 })
            .unwrap();
        assert_eq!(value, 3);
    }

    #[test]
    fn an_owned_runtime_can_be_dropped_from_an_async_context() {
        // Built outside a runtime, dropped inside one: a plain
        // `Runtime::drop` would panic here.
        let runtime = NodeRuntime::owned().unwrap();
        runtime.block_on("test", "test", async {}).unwrap();
        let inner = NodeRuntime::owned().unwrap();
        inner
            .block_on("test", "test", async move {
                drop(runtime);
            })
            .unwrap();
    }

    #[test]
    fn the_worker_count_is_documented() {
        assert_eq!(OWNED_WORKER_THREADS, 2);
    }
}
