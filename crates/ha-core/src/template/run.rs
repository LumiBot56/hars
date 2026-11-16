//! Where templates are compiled and rendered: never on the async runtime.
//!
//! Every render, from the WebSocket's `render_template` or from an
//! automation, runs on a thread of its own with [`STACK`](super::STACK)
//! (compiling and printing recurse up to the template's depth limits), at
//! most [`RENDERS_AT_ONCE`] at a time across the whole instance. A job
//! takes a permit first and gives it back when its thread is done with it,
//! even if nobody waits for the answer any more (a timed-out render runs on
//! to its fuel budget). When every permit is taken, the next job waits.
//!
//! The thread is started from tokio's blocking pool, which waits for it:
//! that is what tells a paused test clock that work is under way, so it
//! doesn't jump ahead while a render runs (the pool's own threads have the
//! runtime's default stack, too small for a deep template). Nothing is kept
//! between jobs: the render thread ends with its job, and the pool's thread
//! after its usual 10 s.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

/// Renders running at once, across the instance.
pub const RENDERS_AT_ONCE: usize = 2;

static RENDERS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(RENDERS_AT_ONCE)));

/// A render permit: held while a job runs.
pub async fn permit() -> Option<OwnedSemaphorePermit> {
    RENDERS.clone().acquire_owned().await.ok()
}

/// Jobs asked of [`run`] and not yet answered (waiting for a permit or
/// rendering).
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Counts a job in flight while it lives.
struct InFlight;

impl InFlight {
    fn new() -> InFlight {
        IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
        InFlight
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Jobs of [`run`] not yet answered: what a test under a paused clock
/// waits for before it looks.
pub fn in_flight() -> usize {
    IN_FLIGHT.load(Ordering::Acquire)
}

/// Run template work on a render thread, holding a permit until it
/// returns. `None` if it panicked or no thread could start.
pub async fn run<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let _counted = InFlight::new();
    let permit = permit().await?;
    on_render_thread(permit, work)?.await.ok()
}

/// Start `work` on a render thread, holding `permit` until it returns. The
/// answer comes back on the receiver.
pub fn on_render_thread<T: Send + 'static>(
    permit: OwnedSemaphorePermit,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<oneshot::Receiver<T>> {
    let (tx, rx) = oneshot::channel();
    let handle = tokio::runtime::Handle::try_current().ok()?;
    let runtime = handle.clone();
    handle.spawn_blocking(move || {
        let _permit = permit;
        let thread = std::thread::Builder::new()
            .name("hars-template".into())
            .stack_size(super::STACK)
            .spawn(move || {
                // In the runtime's context, so the clock a render reads
                // (`now()`) is the runtime's, paused in tests.
                let _context = runtime.enter();
                work()
            });
        if let Ok(Ok(answer)) = thread.map(std::thread::JoinHandle::join) {
            let _ = tx.send(answer);
        }
    });
    Some(rx)
}

/// Render permits free now.
pub fn renders_free() -> usize {
    RENDERS.available_permits()
}

/// Take every render permit (until dropped): for tests of what waits.
pub async fn take_all_renders() -> OwnedSemaphorePermit {
    RENDERS
        .clone()
        .acquire_many_owned(RENDERS_AT_ONCE as u32)
        .await
        .expect("open")
}
