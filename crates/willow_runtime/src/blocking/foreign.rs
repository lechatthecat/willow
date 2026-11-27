//! Owned foreign jobs. Cancellation linearizes against start/publication under
//! one short mutex; no user code or destructor runs under that mutex.
use super::{BlockingPool, BlockingWork, PoolKind};
use std::sync::{Arc, Mutex};

struct State<T> {
    cancelled: bool,
    done: bool,
    result: Option<T>,
}

/// `work` must contain its panics and accept cancellation by destroying its
/// captured native inputs without invoking the foreign body. Both inputs and
/// results own native storage, never GC pointers. A cancelled call waits for
/// destruction on the pool, including when admission itself is backpressured.
pub(crate) fn run<T: Send + 'static>(work: impl FnOnce(bool) -> T + Send + 'static) -> Option<T> {
    #[cfg(any(
        all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
    ))]
    if crate::native_stack::is_active() {
        return run_on(PoolKind::Foreign.pool(), work);
    }
    Some(work(false))
}

#[cfg(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
))]
fn run_on<T: Send + 'static>(
    pool: &BlockingPool,
    work: impl FnOnce(bool) -> T + Send + 'static,
) -> Option<T> {
    let task = crate::scheduler::willow_sched_current_task();
    let shared = Arc::new(Mutex::new(State {
        cancelled: false,
        done: false,
        result: None,
    }));
    let cleanup = crate::native_stack::cancellation_suppressed();
    let output = shared.clone();
    let job: BlockingWork = Box::new(move || {
        // This is the start sequence point: a cancellation observed before it
        // skips the body. Later cancellation discards the owned result.
        let cancelled = output.lock().unwrap().cancelled
            || (!cleanup && crate::scheduler::willow_sched_is_cancelled(task) != 0);
        let value = work(cancelled);
        let mut state = output.lock().unwrap();
        if state.cancelled {
            drop(state);
            drop(value);
            state = output.lock().unwrap();
        } else {
            state.result = Some(value);
        }
        state.done = true;
        drop(state);
        crate::scheduler::willow_sched_wake(task);
    });
    let mut pending = Some(job);
    loop {
        let mut state = shared.lock().unwrap();
        state.cancelled |= crate::native_stack::cancelled() != 0;
        #[cfg(test)]
        if state.cancelled {
            crate::test_sync::reached("foreign cancellation observed");
        }
        if state.done {
            let value = state.result.take();
            if !state.cancelled || value.is_none() {
                return value;
            }
            // Cancellation after publication: dispose on the same bounded
            // pool, never on a scheduler worker. No extra unbounded queue.
            state.done = false;
            let output = shared.clone();
            pending = Some(Box::new(move || {
                drop(value);
                output.lock().unwrap().done = true;
                crate::scheduler::willow_sched_wake(task);
            }));
        }
        drop(state);
        if let Some(job) = pending.take()
            && let Err(job) = pool.try_submit_or_wait(task, job)
        {
            pending = Some(job);
        }
        crate::native_stack::suspend_with_result(crate::task::RUNTIME_POLL_BLOCKED_SYSCALL);
    }
}

#[cfg(test)]
#[path = "foreign_tests.rs"]
mod tests;
