//! Task-owned native stacks. A synchronous safepoint suspends the complete
//! call chain; the scheduler can run another task without recursive re-entry.
//!
//! Context objects and stack mappings stay at stable addresses. No TLS borrow,
//! scheduler lock, or GC registry lock crosses a context switch. Implicit
//! safepoints retain worker affinity: source locals need not be Send. Completed
//! cooperative polls can migrate after a source suspension, once this native
//! stack has returned and the compiler has checked the surviving frame.

#[cfg(unix)]
use corosensei::stack::Stack;
use corosensei::{Coroutine, CoroutineResult, Yielder};
use std::cell::Cell;
use std::ffi::c_void;

use crate::stack_trace::RuntimeStackTrace;
use crate::task::{RUNTIME_POLL_PREEMPTED, RuntimeCancelFn, RuntimePollFn};

thread_local! {
    static CURRENT: Cell<*mut NativeStack> = const { Cell::new(std::ptr::null_mut()) };
    // NativeStack and the coroutine stack retain stable addresses across cache moves.
    #[allow(clippy::vec_box)]
    static IDLE_STACKS: std::cell::RefCell<Vec<Box<NativeStack>>> = const { std::cell::RefCell::new(Vec::new()) };
}

use crate::worker_affinity::{WorkerAffinity, acquire};
#[cfg(test)]
pub(crate) use crate::worker_affinity::{affinity_counts_for_test, required_workers};

pub(crate) struct NativeStack {
    context: crate::generated_context::Context,
    pub(crate) worker: usize,
    affinity: Option<WorkerAffinity<'static>>,
    owner_thread: std::thread::ThreadId,
    native_frames: usize,
    coroutine: Coroutine<(), (), ()>,
    yielder: *const Yielder<(), ()>,
    #[cfg(unix)]
    guard: (usize, usize),
    poll: RuntimePollFn,
    cancel_entry: Option<RuntimeCancelFn>,
    frame: *mut c_void,
    result: i32,
    suspended: bool,
    cancelled: bool,
    /// Cancel-request epoch at this stack's last task-state look; 0 forces a
    /// look. `cancelled()` re-reads the shared task table only when the epoch
    /// has moved, so per-call checks stay thread-local (willow-jz15.120).
    cancel_epoch: u64,
    cleanup_depth: u32,
    /// Countdown that generated synchronous safepoints decrement inline; they
    /// enter `willow_sync_safepoint` only when it reaches zero. Generated code
    /// reaches it through the stable pointer from `willow_sync_poll_counter`.
    poll_countdown: i32,
    /// Safepoints granted by the last `willow_sync_safepoint` refill.
    poll_granted: i32,
    /// Countdown/debt before the outermost no-preempt region.
    poll_saved: (i32, i32),
    entry_panic_depth: i32,
    root_depth: usize,
    parked_roots: Option<u64>,
    trace: RuntimeStackTrace,
    overflow_helper: crate::stack_overflow::TaskHelper,
    reference_context: crate::reference_debug::ReferenceCallState,
}

impl std::fmt::Debug for NativeStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeStack")
            .field("suspended", &self.suspended)
            .finish_non_exhaustive()
    }
}

// SAFETY: the allocation has a stable address and is owned by one task.
// Scheduler claims honor required_worker before resuming or cancelling a live
// stack, and resume independently verifies its OS-thread owner. Thus moving a
// suspended allocation through the shared task table never executes or drops
// its thread-bound frames on another worker. Completed stacks contain only the
// idle trampoline; roots, trace and reference state are restored before entry.
unsafe impl Send for NativeStack {}

impl NativeStack {
    pub(crate) fn acquire(poll: RuntimePollFn, frame: *mut c_void) -> Box<Self> {
        if let Some(mut stack) = IDLE_STACKS.with(|pool| pool.borrow_mut().pop()) {
            stack.affinity = Some(acquire(stack.worker));
            stack.context.clear_helper();
            stack.native_frames = 0;
            stack.poll = poll;
            stack.cancel_entry = None;
            stack.frame = frame;
            stack.cancelled = false;
            stack.cancel_epoch = 0;
            stack.cleanup_depth = 0;
            stack.poll_countdown = 0;
            stack.poll_granted = 0;
            stack.entry_panic_depth = crate::panic_context::willow_panic_depth();
            stack.trace = RuntimeStackTrace::default();
            stack.overflow_helper = Some(b"<unknown>");
            stack.reference_context = Default::default();
            stack
        } else {
            Self::new(poll, frame)
        }
    }

    pub(crate) fn required_worker(&self) -> Option<usize> {
        Some(self.worker)
    }

    pub(crate) fn entry_panic_depth(&self) -> i32 {
        self.entry_panic_depth
    }

    pub(crate) fn acquire_cleanup(cancel: RuntimeCancelFn, frame: *mut c_void) -> Box<Self> {
        #[willow_runtime_macros::ffi_boundary]
        unsafe extern "C" fn unused_poll(_: *mut c_void) -> i32 {
            unreachable!()
        }
        let mut stack = Self::acquire(unused_poll, frame);
        stack.cancel_entry = Some(cancel);
        stack.cancelled = true;
        stack.cleanup_depth = 1;
        stack
    }

    pub(crate) fn recycle(mut stack: Box<Self>) {
        assert!(!stack.suspended);
        stack.affinity = None;
        IDLE_STACKS.with(|pool| pool.borrow_mut().push(stack));
    }

    pub(crate) fn new(poll: RuntimePollFn, frame: *mut c_void) -> Box<Self> {
        // Windows stacks inherit the current thread's overflow guarantee.
        crate::stack_overflow::protect_current_thread();
        let storage = corosensei::stack::DefaultStack::new(8 * 1024 * 1024).unwrap_or_else(|_| {
            crate::failure::resource_exhausted(format_args!("cannot allocate task native stack"))
        });
        #[cfg(unix)]
        let guard = {
            let lower = storage.limit().get();
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            (lower, lower + page)
        };
        Box::new(Self {
            context: crate::generated_context::Context::new(),
            worker: crate::scheduler::current_worker(),
            affinity: Some(acquire(crate::scheduler::current_worker())),
            owner_thread: std::thread::current().id(),
            native_frames: 0,
            coroutine: Coroutine::with_stack(storage, |yielder, ()| trampoline(yielder)),
            yielder: std::ptr::null(),
            #[cfg(unix)]
            guard,
            poll,
            cancel_entry: None,
            frame,
            result: 0,
            suspended: false,
            cancelled: false,
            cancel_epoch: 0,
            cleanup_depth: 0,
            poll_countdown: 0,
            poll_granted: 0,
            poll_saved: (0, 0),
            entry_panic_depth: crate::panic_context::willow_panic_depth(),
            root_depth: 0,
            parked_roots: None,
            trace: RuntimeStackTrace::default(),
            overflow_helper: Some(b"<unknown>"),
            reference_context: Default::default(),
        })
    }

    pub(crate) fn is_cleanup(&self) -> bool {
        self.cancel_entry.is_some()
    }

    pub(crate) fn is_suspended(&self) -> bool {
        self.suspended
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// The lifecycle grants exclusive stack ownership, but no Rust reference
    /// spans the context switch: generated code and safepoint hooks access CURRENT via
    /// raw pointers while the scheduler's Rust activation is suspended.
    pub(crate) unsafe fn resume(stack: *mut Self) -> i32 {
        // Ensure per-thread overflow protection before entering a task stack.
        crate::stack_overflow::protect_current_thread();
        unsafe {
            assert_eq!(
                (*stack).owner_thread,
                std::thread::current().id(),
                "thread-bound native task stack resumed on a different OS worker"
            );
            // Every scheduler resume starts a fresh quantum, including a
            // return from a blocking runtime operation or an async poll. Old
            // inline debt belongs to the completed quantum, not the new one.
            // Zero forces the next sync safepoint to obtain a current grant.
            (*stack).poll_countdown = 0;
            (*stack).poll_granted = 0;
            let previous = CURRENT.with(|slot| slot.replace(stack));
            let outer_trace = crate::stack_trace::snapshot_current();
            let outer_reference = crate::reference_debug::snapshot_current();
            (*stack).root_depth = crate::gc::gc_thread_root_depth();
            let outer_base = crate::gc::replace_root_base((*stack).root_depth);
            let outer_relocation = crate::gc::set_relocation_safe(false);
            let context = std::ptr::addr_of!((*stack).context);
            let outer_context = crate::generated_context::replace(context);
            if (*stack).suspended {
                crate::stack_trace::replace_current(std::mem::take(&mut (*stack).trace));
                crate::reference_debug::replace_current(std::mem::take(
                    &mut (*stack).reference_context,
                ));
                if let Some(token) = (*stack).parked_roots.take() {
                    crate::gc::resume_parked_roots(token);
                }
            }
            #[cfg(unix)]
            let guard = crate::stack_overflow::replace_guard((*stack).guard.0, (*stack).guard.1);
            let outer_helper = crate::stack_overflow::replace_task_helper((*stack).overflow_helper);
            // Borrow only the coroutine field: user code accesses the other
            // fields through CURRENT while this activation is suspended.
            (*stack).context.refresh();
            let outer_diagnostic = crate::stack_overflow::replace_generated_context(context);
            let switched = (*std::ptr::addr_of_mut!((*stack).coroutine)).resume(());
            (*stack).overflow_helper = crate::stack_overflow::replace_task_helper(outer_helper);
            crate::stack_overflow::replace_generated_context(outer_diagnostic);
            crate::generated_context::replace(outer_context);
            #[cfg(unix)]
            crate::stack_overflow::replace_guard(guard.0, guard.1);
            assert!(matches!(switched, CoroutineResult::Yield(())));
            (*stack).trace = crate::stack_trace::replace_current(outer_trace);
            (*stack).reference_context = crate::reference_debug::replace_current(outer_reference);
            crate::gc::replace_root_base(outer_base);
            crate::gc::set_relocation_safe(outer_relocation);
            CURRENT.with(|slot| slot.set(previous));
            (*stack).result
        }
    }
}

impl Drop for NativeStack {
    fn drop(&mut self) {
        // A live synchronous frame must first run generated cancellation cleanup.
        assert!(!self.suspended, "dropping a suspended task native stack");
        assert!(self.parked_roots.is_none());
        // Only the idle trampoline remains; no generated frames or roots are
        // live. Finish it without unwinding through runtime TLS during teardown.
        // SAFETY: the trampoline holds only raw pointers and trivial values at
        // its idle yield, and all user calls have returned (asserted above).
        unsafe { self.coroutine.force_reset() };
    }
}

// The trampoline itself can outlive many migrations and cache reuses. Keep
// every TLS access behind a call on its current side of the migrating callback.
#[inline(never)]
fn trampoline_current() -> *mut NativeStack {
    CURRENT.with(Cell::get)
}

#[inline(never)]
fn settle_completed_quantum() {
    let pending = poll_pending();
    if pending != 0 {
        crate::preempt::charge_safepoints(pending);
        poll_refill(0);
    }
}

fn trampoline(yielder: &Yielder<(), ()>) {
    loop {
        let stack = trampoline_current();
        assert!(!stack.is_null());
        unsafe {
            (*stack).yielder = yielder;
        }
        // Do not keep an exclusive reference across the user poll: safepoints
        // access this same allocation through CURRENT while the poll is active.
        let (poll, cancel, frame) =
            unsafe { ((*stack).poll, (*stack).cancel_entry, (*stack).frame) };
        let result = if let Some(cancel) = cancel {
            unsafe {
                crate::generated_context::invoke(cancel as *const (), frame);
                (*stack).cleanup_depth = 0;
            }
            if crate::panic_context::willow_panic_active() != 0 {
                crate::task::RUNTIME_POLL_PANICKED
            } else {
                crate::task::RUNTIME_POLL_READY
            }
        } else {
            unsafe { crate::generated_context::invoke(poll as *const (), frame) }
        };
        // A completed helper can return to a cooperative poll on the worker
        // stack in the SAME quantum. Settle its partial inline grant before
        // resume restores CURRENT and leave recycles this stack. This is not
        // another safepoint: charge only work already executed, and do not
        // suspend a callback that has finished its result/cleanup handling.
        settle_completed_quantum();
        unsafe {
            (*stack).result = result;
            (*stack).suspended = false;
            yielder.suspend(());
        }
    }
}

/// Yield the active task's native call chain after a tripped quantum check.
/// Returns false outside task stacks (ordinary synchronous entry code).
pub(crate) fn suspend() -> bool {
    suspend_with_result(RUNTIME_POLL_PREEMPTED)
}

/// Suspend a synchronous runtime operation on its existing task stack. The
/// scheduler must preserve the wait reason rather than immediately requeue it.
pub(crate) fn suspend_with_result(result: i32) -> bool {
    suspend_impl(result)
}

/// A generated safepoint is not a source suspension. Synchronous locals
/// (including non-Send values) stay on their original worker until return.
pub(crate) fn suspend_generated() -> bool {
    suspend_impl(RUNTIME_POLL_PREEMPTED)
}

fn suspend_impl(result: i32) -> bool {
    let stack = CURRENT.with(Cell::get);
    if stack.is_null() {
        return false;
    }
    unsafe {
        (*stack).parked_roots = Some(crate::gc::park_current_roots((*stack).root_depth));
        (*stack).result = result;
        (*stack).suspended = true;
        (*(*stack).yielder).suspend(());
    }
    true
}

/// Tracks native callback nesting by stack identity: a nested scheduler can
/// temporarily replace CURRENT. Both native and generated extents stay affine.
pub(crate) struct NativeFrameGuard(*mut NativeStack);

impl NativeFrameGuard {
    pub(crate) fn enter() -> Self {
        let stack = CURRENT.with(Cell::get);
        if !stack.is_null() {
            unsafe { (*stack).native_frames += 1 };
        }
        Self(stack)
    }
}

impl Drop for NativeFrameGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (*self.0).native_frames -= 1 };
        }
    }
}

/// Whether the current invocation owns a scheduler-managed native stack.
pub(crate) fn is_active() -> bool {
    CURRENT.with(|current| !current.get().is_null())
}

/// Address of the active stack's inline safepoint countdown, or null outside
/// task stacks. The `NativeStack` allocation is stable while generated frames
/// run on it, so the pointer is invocation-constant for the caller.
pub(crate) fn poll_counter() -> *mut i32 {
    let stack = CURRENT.with(Cell::get);
    if stack.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { std::ptr::addr_of_mut!((*stack).poll_countdown) }
}

/// Safepoints executed since the last refill, including the current one. A
/// generated caller arrives with the countdown at zero (the whole grant); a
/// direct runtime caller leaves it untouched and accounts for one safepoint.
pub(crate) fn poll_consumed() -> u64 {
    poll_pending().max(1)
}

/// Uncharged generated sync safepoints, excluding the caller's own async or
/// runtime check. Zero is significant: a new async check must not charge a
/// phantom sync safepoint when there was no intervening generated work.
pub(crate) fn poll_pending() -> u64 {
    let stack = CURRENT.with(Cell::get);
    if stack.is_null() {
        return 0;
    }
    unsafe {
        let consumed = i64::from((*stack).poll_granted) - i64::from((*stack).poll_countdown);
        consumed.max(0) as u64
    }
}

/// Grant the next batch of inline safepoints to the active stack.
pub(crate) fn poll_refill(grant: i32) {
    let stack = CURRENT.with(Cell::get);
    if !stack.is_null() {
        unsafe {
            (*stack).poll_granted = grant;
            (*stack).poll_countdown = grant;
        }
    }
}

/// Isolate inline polls inside an outermost no-preempt region. The quantum
/// owns nesting; the saved grant belongs to this stable native stack.
pub(crate) fn poll_no_preempt(enter: bool) {
    let stack = CURRENT.with(Cell::get);
    if !stack.is_null() {
        unsafe {
            if enter {
                (*stack).poll_saved = ((*stack).poll_countdown, (*stack).poll_granted);
                (*stack).poll_countdown = 0;
                (*stack).poll_granted = 0;
            } else {
                let (countdown, granted) = (*stack).poll_saved;
                (*stack).poll_countdown = countdown;
                (*stack).poll_granted = granted;
            }
        }
    }
}

/// Sticky task cancellation, suppressed while generated defer cleanup executes.
pub(crate) fn cancelled() -> i32 {
    let stack = CURRENT.with(Cell::get);
    if stack.is_null() {
        return 0;
    }
    unsafe {
        if (*stack).cleanup_depth != 0 {
            return 0;
        }
        if !(*stack).cancelled {
            // Load the epoch BEFORE the state: a request that lands after this
            // load advances the epoch again, so the next call looks.
            let epoch = crate::task_state::cancel_epoch();
            if epoch != (*stack).cancel_epoch {
                let task = crate::scheduler::willow_sched_current_task();
                (*stack).cancelled = crate::scheduler::willow_sched_is_cancelled(task) != 0;
                (*stack).cancel_epoch = epoch;
            }
        }
        i32::from((*stack).cancelled)
    }
}

pub(crate) fn cleanup_enter() {
    let stack = CURRENT.with(Cell::get);
    if !stack.is_null() {
        unsafe {
            (*stack).cleanup_depth += 1;
        }
    }
}

pub(crate) fn cleanup_leave() {
    let stack = CURRENT.with(Cell::get);
    if !stack.is_null() {
        unsafe {
            (*stack).cleanup_depth = (*stack).cleanup_depth.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preempt::{PreemptConfig, begin_quantum, willow_preempt_end, willow_sync_safepoint};
    use crate::task::RUNTIME_POLL_READY;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn generated_stack_keeps_affinity_across_implicit_preemption() {
        unsafe extern "C" fn helper(_: *mut c_void) -> i32 {
            // A runtime entry may hold !Send state and call generated code.
            // Its guard pins the entire extent, including nested callbacks.
            crate::failure::ffi_boundary("native_callback", || {
                let local = std::rc::Rc::new(17);
                let original = std::thread::current().id();
                crate::failure::ffi_boundary("nested_callback", || {
                    willow_sync_safepoint();
                    assert_eq!(std::thread::current().id(), original);
                    assert_eq!(*local, 17);
                });
            });
            willow_sync_safepoint();
            RUNTIME_POLL_READY
        }
        let _guard = crate::gc::runtime_test_guard();
        let mut stack = NativeStack::new(helper, std::ptr::null_mut());
        for expected_owner in [Some(stack.worker), Some(stack.worker)] {
            begin_quantum(
                PreemptConfig::from_env_values(Some("1"), Some("100000")),
                std::ptr::null(),
            );
            assert_eq!(
                unsafe { NativeStack::resume(&mut *stack) },
                RUNTIME_POLL_PREEMPTED
            );
            willow_preempt_end();
            assert_eq!(stack.required_worker(), expected_owner);
        }
        begin_quantum(
            PreemptConfig::from_env_values(Some("1"), Some("100000")),
            std::ptr::null(),
        );
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_READY
        );
        willow_preempt_end();
        assert_eq!(stack.native_frames, 0);
    }

    #[test]
    fn blocking_work_parks_without_stalling_the_scheduler() {
        use crate::scheduler::*;
        use std::sync::{Mutex, mpsc};
        use std::time::Duration;
        static GATE: Mutex<Option<mpsc::Receiver<()>>> = Mutex::new(None);
        static COMPLETED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        unsafe extern "C" fn blocked(_: *mut c_void) -> i32 {
            let gate = GATE.lock().unwrap().take().unwrap();
            let result = crate::blocking::run_owned(move || {
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
                73
            });
            COMPLETED.store(result.unwrap_or(0), std::sync::atomic::Ordering::SeqCst);
            RUNTIME_POLL_READY
        }
        let _guard = crate::gc::runtime_test_guard();
        for cancel in [false, true] {
            reset_global_scheduler_for_test();
            COMPLETED.store(999, std::sync::atomic::Ordering::SeqCst);
            let (send, receive) = mpsc::channel();
            *GATE.lock().unwrap() = Some(receive);
            let task = willow_sched_spawn(blocked, std::ptr::null_mut());
            willow_sched_run_until_deadline(willow_monotonic_millis() + 20);
            assert_eq!(willow_sched_task_state(task), 7);
            assert_eq!(COMPLETED.load(std::sync::atomic::Ordering::SeqCst), 999);
            if cancel {
                willow_sched_cancel(task);
                willow_sched_run_until(task);
                assert_eq!(COMPLETED.load(std::sync::atomic::Ordering::SeqCst), 0);
            }
            send.send(()).unwrap();
            if !cancel {
                willow_sched_run_until(task);
                assert_eq!(COMPLETED.load(std::sync::atomic::Ordering::SeqCst), 73);
            }
            crate::blocking::test_support::wait_until("owned job completes", || {
                crate::blocking::willow_blocking_active_jobs() == 0
                    && crate::blocking::willow_blocking_queued_jobs() == 0
            });
        }
        reset_global_scheduler_for_test();
    }

    #[test]
    fn owned_work_backpressure_cancellation_drops_unsubmitted_jobs() {
        use crate::blocking::test_support::*;
        use crate::scheduler::*;
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        static RAN: AtomicUsize = AtomicUsize::new(0);
        struct Owned;
        impl Drop for Owned {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }
        unsafe extern "C" fn blocked(_: *mut c_void) -> i32 {
            let owned = Owned;
            assert!(
                crate::blocking::run_owned(move || {
                    RAN.fetch_add(1, Ordering::SeqCst);
                    drop(owned);
                })
                .is_none()
            );
            RUNTIME_POLL_READY
        }
        let _guard = crate::gc::runtime_test_guard();
        for count in [1, 16, 64] {
            reset_global_scheduler_for_test();
            DROPS.store(0, Ordering::SeqCst);
            RAN.store(0, Ordering::SeqCst);
            let mut stalled = stall_pool_threads();
            stalled.fill_queue_gated(crate::blocking::willow_blocking_queue_capacity() as usize);
            let ids: Vec<_> = (0..count)
                .map(|_| willow_sched_spawn(blocked, std::ptr::null_mut()))
                .collect();
            willow_sched_run_until_deadline(willow_monotonic_millis() + 50);
            assert_eq!(crate::blocking::willow_blocking_slot_waiters(), count);
            for &id in ids.iter().rev() {
                willow_sched_cancel(id);
            }
            for &id in &ids {
                willow_sched_run_until(id);
            }
            assert_eq!(DROPS.load(Ordering::SeqCst), count as usize);
            assert_eq!(RAN.load(Ordering::SeqCst), 0);
            assert_eq!(crate::blocking::willow_blocking_slot_waiters(), 0);
            println!(
                "owned-backpressure tasks={count} drops={count} jobs_run=0 remaining_waiters=0"
            );
            stalled.release_all();
            wait_until("pool drains", || {
                crate::blocking::willow_blocking_queued_jobs() == 0
                    && crate::blocking::willow_blocking_active_jobs() == 0
            });
        }
        reset_global_scheduler_for_test();
    }

    #[test]
    fn legacy_timer_wait_completes_through_cooperative_boundary() {
        use crate::scheduler::*;
        unsafe extern "C" fn sleep(_: *mut c_void) -> i32 {
            let future = crate::future::LegacyFutureOwner::new(
                crate::future::WillowFutureVoid::sleep_after_millis(5),
            );
            future.get().block_until_ready();
            assert!(future.get().is_ready());
            future.get().block_until_ready();
            RUNTIME_POLL_READY
        }
        unsafe extern "C" fn poll(_: *mut c_void) -> i32 {
            let result = willow_task_stack_enter(sleep, std::ptr::null_mut());
            willow_task_stack_leave();
            assert!(result == crate::task::RUNTIME_POLL_PENDING || result == RUNTIME_POLL_READY);
            result
        }
        let _guard = crate::gc::runtime_test_guard();
        reset_global_scheduler_for_test();
        let task = willow_sched_spawn_cooperative(poll, std::ptr::null_mut()) as u64;
        willow_sched_run_until(task);
        reset_global_scheduler_for_test();
    }

    #[test]
    fn legacy_timer_wait_parks_and_cancels_without_sleeping_worker() {
        use crate::scheduler::*;
        unsafe extern "C" fn sleeper(_: *mut c_void) -> i32 {
            let future = crate::future::LegacyFutureOwner::new(
                crate::future::WillowFutureVoid::sleep_after_millis(60_000),
            );
            future.get().block_until_ready();
            assert_ne!(cancelled(), 0);
            RUNTIME_POLL_READY
        }
        let _guard = crate::gc::runtime_test_guard();
        reset_global_scheduler_for_test();
        let task = willow_sched_spawn(sleeper, std::ptr::null_mut());
        willow_sched_run_until_deadline(willow_monotonic_millis() + 20);
        assert_eq!(willow_sched_task_state(task), 2);
        willow_sched_cancel(task);
        willow_sched_run_until(task);
        reset_global_scheduler_for_test();
    }

    #[test]
    fn timer_value_wait_parks_and_cancels_without_sleeping_worker() {
        use crate::scheduler::*;
        unsafe extern "C" fn sleeper(_: *mut c_void) -> i32 {
            let value = crate::timer::willow_timer_value_sleep(i64::MAX);
            crate::timer::willow_timer_value_await(value);
            assert_ne!(cancelled(), 0);
            RUNTIME_POLL_READY
        }
        let _guard = crate::gc::runtime_test_guard();
        reset_global_scheduler_for_test();
        let task = willow_sched_spawn(sleeper, std::ptr::null_mut());
        willow_sched_run_until_deadline(willow_monotonic_millis() + 20);
        assert_eq!(willow_sched_task_state(task), 2);
        willow_sched_cancel(task);
        willow_sched_run_until(task);
        reset_global_scheduler_for_test();
    }

    #[test]
    fn affinity_lease_is_released_by_recycle_and_drop() {
        let _guard = crate::gc::runtime_test_guard();
        crate::scheduler::reset_global_scheduler_for_test();
        unsafe extern "C" fn ready(_: *mut c_void) -> i32 {
            RUNTIME_POLL_READY
        }
        assert_eq!(required_workers(), 0);
        let stack = NativeStack::acquire(ready, std::ptr::null_mut());
        assert_eq!(required_workers(), stack.worker + 1);
        NativeStack::recycle(stack);
        assert_eq!(required_workers(), 0);
        let stack = NativeStack::acquire_cleanup(cleanup_noop, std::ptr::null_mut());
        assert_eq!(required_workers(), stack.worker + 1);
        drop(stack);
        assert_eq!(required_workers(), 0);

        unsafe extern "C" fn cleanup_noop(_: *mut c_void) {}
    }

    #[test]
    fn native_stack_overflow_reports_the_runtime_diagnostic() {
        const CHILD: &str = "WILLOW_TASK_STACK_OVERFLOW_CHILD";
        if std::env::var_os(CHILD).is_some() {
            #[inline(never)]
            fn recurse(depth: usize) -> usize {
                let padding = [1u8; 8192];
                std::hint::black_box(&padding);
                if std::hint::black_box(depth) == 0 {
                    return 0;
                }
                recurse(depth - 1).wrapping_add(std::hint::black_box(padding[0]) as usize)
            }
            unsafe extern "C" fn overflow(_: *mut c_void) -> i32 {
                crate::stack_overflow::willow_stack_helper_set(b"recurse".as_ptr(), 7);
                std::hint::black_box(recurse(1_000_000));
                RUNTIME_POLL_READY
            }
            let mut stack = NativeStack::new(overflow, std::ptr::null_mut());
            unsafe { NativeStack::resume(&mut *stack) };
            panic!("task stack exhaustion was not fatal");
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_stack::tests::native_stack_overflow_reports_the_runtime_diagnostic",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(101), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("task stack overflow while executing synchronous helper recurse"),
            "{output:?}"
        );
    }

    #[test]
    fn recycled_stack_preserves_float_locals_and_uses_the_new_frame() {
        let _guard = crate::gc::runtime_test_guard();
        unsafe extern "C" fn poll(frame: *mut c_void) -> i32 {
            let initial = unsafe { *frame.cast::<f64>() };
            let locals = std::array::from_fn::<_, 32, _>(|i| initial + i as f64 * 0.25);
            for _ in 0..20 {
                std::hint::black_box(&locals);
                assert!(suspend());
                for (i, value) in locals.iter().enumerate() {
                    assert_eq!(*value, initial + i as f64 * 0.25);
                }
            }
            unsafe { *frame.cast::<f64>() = locals.iter().sum() };
            RUNTIME_POLL_READY
        }
        let mut address = None;
        for initial in [1.25, 7.5, -4.0] {
            let mut output = initial;
            let mut stack = NativeStack::acquire(poll, (&mut output as *mut f64).cast());
            let current = std::ptr::from_ref(&*stack);
            assert_eq!(*address.get_or_insert(current), current);
            for _ in 0..20 {
                assert_eq!(
                    unsafe { NativeStack::resume(&mut *stack) },
                    RUNTIME_POLL_PREEMPTED
                );
            }
            assert_eq!(
                unsafe { NativeStack::resume(&mut *stack) },
                RUNTIME_POLL_READY
            );
            assert_eq!(
                output,
                (0..32).map(|i| initial + i as f64 * 0.25).sum::<f64>()
            );
            NativeStack::recycle(stack);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_switch_updates_and_restores_thread_stack_bounds() {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
        }
        fn bounds() -> (usize, usize) {
            let (mut low, mut high) = (0, 0);
            unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
            let marker = 0u8;
            let address = std::ptr::from_ref(&marker) as usize;
            assert!(low <= address && address < high);
            (low, high)
        }
        unsafe extern "C" fn poll(frame: *mut c_void) -> i32 {
            let task_bounds = bounds();
            unsafe { *frame.cast::<(usize, usize)>() = task_bounds };
            assert!(suspend());
            // StackLimit may grow as pages are committed; StackBase is fixed.
            assert_eq!(bounds().1, task_bounds.1);
            RUNTIME_POLL_READY
        }
        let _guard = crate::gc::runtime_test_guard();
        let parent = bounds();
        let mut task_bounds = (0usize, 0usize);
        let mut stack = NativeStack::new(poll, std::ptr::from_mut(&mut task_bounds).cast());
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_PREEMPTED
        );
        assert_ne!(task_bounds.1, parent.1);
        assert_eq!(bounds().1, parent.1);
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_READY
        );
        assert_eq!(bounds().1, parent.1);
    }

    fn quantum() {
        begin_quantum(
            PreemptConfig::from_env_values(Some("1"), Some("10000")),
            std::ptr::null(),
        );
    }

    unsafe extern "C" fn recursive_poll(frame: *mut c_void) -> i32 {
        fn recurse(depth: u64) -> u64 {
            let local = depth + 1;
            if depth == 0 {
                return 1;
            }
            assert_eq!(willow_sync_safepoint(), 0);
            std::hint::black_box(local) + recurse(depth - 1)
        }
        unsafe {
            *frame.cast::<u64>() = recurse(100);
        }
        RUNTIME_POLL_READY
    }

    #[test]
    fn native_recursion_resumes_values_without_growing_scheduler_stack() {
        let _guard = crate::gc::runtime_test_guard();
        let mut output = 0u64;
        let mut stack = NativeStack::new(recursive_poll, (&mut output as *mut u64).cast());
        let mut yields = 0;
        loop {
            quantum();
            let result = unsafe { NativeStack::resume(&mut *stack) };
            willow_preempt_end();
            if result == RUNTIME_POLL_READY {
                break;
            }
            assert_eq!(result, RUNTIME_POLL_PREEMPTED);
            yields += 1;
        }
        assert_eq!(yields, 100);
        assert_eq!(output, 5151);
    }

    /// Models generated synchronous safepoints (willow-jz15.40): decrement
    /// the inline countdown, enter the runtime only once it runs out.
    struct CountedLoop {
        safepoints: u64,
        runtime_calls: u64,
        resumes: u64,
        trips: Vec<u64>,
    }

    unsafe extern "C" fn counted_poll(frame: *mut c_void) -> i32 {
        let state = frame.cast::<CountedLoop>();
        let counter = crate::preempt::willow_sync_poll_counter();
        assert!(!counter.is_null());
        unsafe {
            for index in 1..=(*state).safepoints {
                *counter -= 1;
                if *counter <= 0 {
                    (*state).runtime_calls += 1;
                    let before = (*state).resumes;
                    assert_eq!(willow_sync_safepoint(), 0);
                    if (*state).resumes != before {
                        (*state).trips.push(index);
                    }
                }
            }
        }
        RUNTIME_POLL_READY
    }

    fn run_counted(budget: u64, safepoints: u64) -> CountedLoop {
        let mut state = CountedLoop {
            safepoints,
            runtime_calls: 0,
            resumes: 0,
            trips: Vec::new(),
        };
        let config = PreemptConfig::from_env_values(Some(&budget.to_string()), Some("100000"));
        let mut stack = NativeStack::new(counted_poll, std::ptr::from_mut(&mut state).cast());
        loop {
            begin_quantum(config, std::ptr::null());
            let result = unsafe { NativeStack::resume(&mut *stack) };
            willow_preempt_end();
            if result == RUNTIME_POLL_READY {
                break;
            }
            assert_eq!(result, RUNTIME_POLL_PREEMPTED);
            state.resumes += 1;
        }
        state
    }

    #[test]
    fn inline_countdown_bounds_runtime_calls_and_keeps_budget_trip_points() {
        use crate::preempt::SYNC_POLL_STRIDE;
        let _guard = crate::gc::runtime_test_guard();
        for (budget, safepoints) in [(1000, 10_000), (1000, 100_000), (1, 50), (64, 640)] {
            let state = run_counted(budget, safepoints);
            // Unbatched checks trip at exactly every `budget`-th safepoint.
            let expected: Vec<u64> = (1..=safepoints / budget).map(|k| k * budget).collect();
            assert_eq!(state.trips, expected, "budget {budget}");
            assert_eq!(state.resumes, safepoints / budget);
            // The first call charges one safepoint, then each quantum is
            // covered by grants of at most SYNC_POLL_STRIDE safepoints.
            let quanta = safepoints / budget;
            let per_quantum = budget.div_ceil(SYNC_POLL_STRIDE);
            let first = 1 + (budget - 1).div_ceil(SYNC_POLL_STRIDE);
            assert_eq!(
                state.runtime_calls,
                first + (quanta - 1) * per_quantum,
                "budget {budget}, safepoints {safepoints}"
            );
            assert!(state.runtime_calls <= safepoints / SYNC_POLL_STRIDE + 2 * quanta);
        }
    }

    struct BoundaryBudget {
        budget: u64,
        async_prefix: u64,
        helpers: u64,
        polls_per_helper: u64,
        poll_entries: u64,
        completed_helpers: u64,
        async_checks_until_trip: u64,
    }

    unsafe extern "C" fn boundary_budget_helper(frame: *mut c_void) -> i32 {
        let state = frame.cast::<BoundaryBudget>();
        let counter = crate::preempt::willow_sync_poll_counter();
        unsafe {
            for _ in 0..(*state).polls_per_helper {
                *counter -= 1;
                if *counter <= 0 {
                    willow_sync_safepoint();
                }
            }
        }
        RUNTIME_POLL_READY
    }

    unsafe extern "C" fn boundary_budget_cooperative_poll(frame: *mut c_void) -> i32 {
        use crate::scheduler::{willow_task_stack_enter, willow_task_stack_leave};
        let state = frame.cast::<BoundaryBudget>();
        // Keep the real cooperative task/ABI boundary but use deterministic,
        // per-test configuration instead of changing process-global env.
        let config = PreemptConfig::from_env_values(
            Some(&unsafe { (*state).budget }.to_string()),
            Some("100000"),
        );
        begin_quantum(config, std::ptr::null());
        assert!(crate::preempt::willow_sync_poll_counter().is_null());
        unsafe {
            if (*state).poll_entries == 0 {
                for _ in 0..(*state).async_prefix {
                    assert_eq!(crate::preempt::willow_preempt_check(), 0);
                }
            }
            (*state).poll_entries += 1;
            while (*state).completed_helpers < (*state).helpers {
                let result = willow_task_stack_enter(boundary_budget_helper, frame);
                // CURRENT is restored already; leave recycles only a finished
                // helper and preserves a suspended helper for the next poll.
                assert!(crate::preempt::willow_sync_poll_counter().is_null());
                willow_task_stack_leave();
                if result == RUNTIME_POLL_PREEMPTED {
                    return result;
                }
                assert_eq!(result, RUNTIME_POLL_READY);
                (*state).completed_helpers += 1;
            }
            for count in 1..=(*state).budget {
                if crate::preempt::willow_preempt_check() != 0 {
                    (*state).async_checks_until_trip = count;
                    break;
                }
            }
        }
        RUNTIME_POLL_READY
    }

    #[test]
    fn cooperative_helper_return_settles_exact_sync_budget() {
        use crate::scheduler::{
            reset_global_scheduler_for_test, willow_sched_run_until, willow_sched_spawn_cooperative,
        };
        let _guard = crate::gc::runtime_test_guard();
        for (budget, async_prefix, helpers, polls_per_helper) in [
            (8, 0, 1, 3), // Reviewer reproducer: five checks, not seven.
            (8, 2, 1, 3), // Async -> sync -> async in one quantum.
            (8, 0, 2, 3), // Recycled stack must settle every invocation once.
            (8, 0, 1, 0), // No phantom charge for a bounded helper.
            (8, 0, 1, 1), // Already charged, no pending debt.
            (65, 0, 1, 63),
            (65, 0, 1, 64),
            (1024, 2, 1, 65),  // Exactly exhausted inline batch.
            (1024, 2, 1, 129), // Multiple charged batches, no double charge.
            (1024, 0, 4, 3),
            (1024, 0, 16, 3), // Repeated short helpers do not escape accounting.
            (1024, 0, 256, 3),
            (8, 0, 1, 8),  // Helper completes just after resuming an exhausted quantum.
            (8, 0, 1, 11), // Partial final grant after a suspension.
            (8, 2, 4, 3),  // Resume/recycle boundaries between short helpers.
        ] {
            reset_global_scheduler_for_test();
            let mut state = BoundaryBudget {
                budget,
                async_prefix,
                helpers,
                polls_per_helper,
                poll_entries: 0,
                completed_helpers: 0,
                async_checks_until_trip: 0,
            };
            let id = willow_sched_spawn_cooperative(
                boundary_budget_cooperative_poll,
                std::ptr::from_mut(&mut state).cast(),
            ) as u64;
            willow_sched_run_until(id);
            assert_eq!(
                state.async_checks_until_trip,
                budget - (async_prefix + helpers * polls_per_helper) % budget,
                "budget={budget} prefix={async_prefix} helpers={helpers} polls={polls_per_helper}"
            );
            assert_eq!(
                state.poll_entries,
                1 + (async_prefix + helpers * polls_per_helper) / budget
            );
            assert_eq!(state.completed_helpers, helpers);
        }
        reset_global_scheduler_for_test();
    }

    struct MixedPolls {
        total: u64,
        sync_run: u64,
        trips: Vec<u64>,
        resumes: u64,
    }

    // Mirrors the generated async fast path; the last point is charged by
    // the existing hook rather than decrementing it twice.
    unsafe fn batched_async_check(counter: *mut i32) -> i32 {
        unsafe {
            if *counter > 1 {
                *counter -= 1;
                0
            } else {
                crate::preempt::willow_preempt_check()
            }
        }
    }

    unsafe extern "C" fn batched_mixed_poll(frame: *mut c_void) -> i32 {
        let state = frame.cast::<MixedPolls>();
        let counter = crate::preempt::willow_sync_poll_counter();
        unsafe {
            for index in 1..=(*state).total {
                let before = (*state).resumes;
                if index % ((*state).sync_run + 1) == 0 {
                    if batched_async_check(counter) != 0 {
                        suspend();
                    }
                } else {
                    *counter -= 1;
                    if *counter <= 0 {
                        willow_sync_safepoint();
                    }
                }
                if before != (*state).resumes {
                    (*state).trips.push(index);
                }
            }
        }
        RUNTIME_POLL_READY
    }

    unsafe extern "C" fn suppressed_batched_native_poll(frame: *mut c_void) -> i32 {
        let (prefix, suppressed, nested) = unsafe { *frame.cast::<(u64, u64, bool)>() };
        let counter = crate::preempt::willow_async_poll_counter();
        for _ in 0..prefix {
            assert_eq!(unsafe { batched_async_check(counter) }, 0);
        }
        crate::preempt::willow_preempt_enter_no_preempt();
        for _ in 0..suppressed {
            assert_eq!(unsafe { batched_async_check(counter) }, 0);
        }
        if nested {
            crate::preempt::willow_preempt_enter_no_preempt();
            for _ in 0..130 {
                assert_eq!(unsafe { batched_async_check(counter) }, 0);
            }
            crate::preempt::willow_preempt_leave_no_preempt();
            for _ in 0..suppressed {
                assert_eq!(unsafe { batched_async_check(counter) }, 0);
            }
        }
        crate::preempt::willow_preempt_leave_no_preempt();
        for n in 1..=100 - prefix {
            assert_eq!(
                unsafe { batched_async_check(counter) },
                i32::from(n == 100 - prefix),
                "prefix={prefix} suppressed={suppressed} nested={nested} remaining point={n}"
            );
        }
        RUNTIME_POLL_READY
    }

    #[test]
    fn native_batched_no_preempt_preserves_partial_debt() {
        let _guard = crate::gc::runtime_test_guard();
        for prefix in [0u64, 1, 20, 63, 64, 65, 99] {
            for suppressed in [0u64, 5, 63, 64, 65, 200] {
                for nested in [false, true] {
                    let mut state = (prefix, suppressed, nested);
                    let mut stack = NativeStack::new(
                        suppressed_batched_native_poll,
                        std::ptr::from_mut(&mut state).cast(),
                    );
                    begin_quantum(
                        PreemptConfig::from_env_values(Some("100"), Some("100000")),
                        std::ptr::null(),
                    );
                    assert_eq!(
                        unsafe { NativeStack::resume(&mut *stack) },
                        RUNTIME_POLL_READY
                    );
                    willow_preempt_end();
                }
            }
        }
    }

    #[test]
    fn batched_async_and_sync_share_exact_budget() {
        let _guard = crate::gc::runtime_test_guard();
        for budget in [1u64, 2, 3, 63, 64, 65, 127, 1024] {
            for sync_run in [0, 1, 2, 7, 63, 64, 100] {
                let mut state = MixedPolls {
                    total: budget * 10,
                    sync_run,
                    trips: Vec::new(),
                    resumes: 0,
                };
                let config =
                    PreemptConfig::from_env_values(Some(&budget.to_string()), Some("100000"));
                let mut stack =
                    NativeStack::new(batched_mixed_poll, std::ptr::from_mut(&mut state).cast());
                loop {
                    begin_quantum(config, std::ptr::null());
                    let result = unsafe { NativeStack::resume(&mut *stack) };
                    willow_preempt_end();
                    if result == RUNTIME_POLL_READY {
                        break;
                    }
                    assert_eq!(result, RUNTIME_POLL_PREEMPTED);
                    state.resumes += 1;
                }
                assert_eq!(
                    state.trips,
                    (1..=10).map(|n| n * budget).collect::<Vec<_>>(),
                    "budget={budget} sync_run={sync_run}"
                );
            }
        }
    }

    unsafe extern "C" fn mixed_poll(frame: *mut c_void) -> i32 {
        let state = frame.cast::<MixedPolls>();
        let counter = crate::preempt::willow_sync_poll_counter();
        unsafe {
            for index in 1..=(*state).total {
                let before = (*state).resumes;
                if index % ((*state).sync_run + 1) == 0 {
                    if crate::preempt::willow_preempt_check() != 0 {
                        suspend();
                    }
                } else {
                    *counter -= 1;
                    if *counter <= 0 {
                        willow_sync_safepoint();
                    }
                }
                if before != (*state).resumes {
                    (*state).trips.push(index);
                }
            }
        }
        RUNTIME_POLL_READY
    }

    #[test]
    fn mixed_sync_async_safepoints_share_exact_budget() {
        let _guard = crate::gc::runtime_test_guard();
        for budget in [1u64, 3, 8, 64, 65, 1024] {
            for sync_run in [0, 1, 2, 7, 63, 64, 100] {
                let mut state = MixedPolls {
                    total: budget * 10,
                    sync_run,
                    trips: Vec::new(),
                    resumes: 0,
                };
                let config =
                    PreemptConfig::from_env_values(Some(&budget.to_string()), Some("100000"));
                let mut stack = NativeStack::new(mixed_poll, std::ptr::from_mut(&mut state).cast());
                loop {
                    begin_quantum(config, std::ptr::null());
                    let result = unsafe { NativeStack::resume(&mut *stack) };
                    willow_preempt_end();
                    if result == RUNTIME_POLL_READY {
                        break;
                    }
                    assert_eq!(result, RUNTIME_POLL_PREEMPTED);
                    state.resumes += 1;
                }
                let expected: Vec<_> = (1..=10).map(|n| n * budget).collect();
                assert_eq!(state.trips, expected, "budget={budget} sync_run={sync_run}");
            }
        }
    }

    unsafe extern "C" fn partial_grant_across_wait(frame: *mut c_void) -> i32 {
        let counter = crate::preempt::willow_sync_poll_counter();
        unsafe {
            // Charge one, then consume only part of a 64-poll grant.
            *counter -= 1;
            willow_sync_safepoint();
            *counter -= 2;
            suspend_with_result(crate::task::RUNTIME_POLL_PENDING);
            // The scheduler resumes with a smaller budget. A stale grant
            // would incorrectly let this first new safepoint keep running.
            *counter -= 1;
            if *counter <= 0 {
                willow_sync_safepoint();
            }
            *frame.cast::<bool>() = true;
        }
        RUNTIME_POLL_READY
    }

    #[test]
    fn sync_inline_grant_is_invalidated_after_runtime_wait() {
        let _guard = crate::gc::runtime_test_guard();
        let mut finished = false;
        let mut stack = NativeStack::new(
            partial_grant_across_wait,
            std::ptr::from_mut(&mut finished).cast(),
        );
        begin_quantum(
            PreemptConfig::from_env_values(Some("1024"), Some("100000")),
            std::ptr::null(),
        );
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            crate::task::RUNTIME_POLL_PENDING
        );
        willow_preempt_end();
        quantum();
        let result = unsafe { NativeStack::resume(&mut *stack) };
        willow_preempt_end();
        // Drain the stack before asserting so a regression cannot panic in
        // NativeStack::drop while a generated activation remains suspended.
        let finished_before_resume = finished;
        if result == RUNTIME_POLL_PREEMPTED {
            quantum();
            assert_eq!(
                unsafe { NativeStack::resume(&mut *stack) },
                RUNTIME_POLL_READY
            );
            willow_preempt_end();
        }
        assert_eq!(result, RUNTIME_POLL_PREEMPTED);
        assert!(!finished_before_resume);
        assert!(finished);
    }

    unsafe extern "C" fn partial_grant_across_nested_quantum(frame: *mut c_void) -> i32 {
        let counter = crate::preempt::willow_sync_poll_counter();
        unsafe {
            *counter -= 1;
            willow_sync_safepoint();
            *counter -= 2;
            // SchedulerDriveGuard restores the outer task with a fresh
            // quantum, without a NativeStack::resume for that outer task.
            quantum();
            *counter -= 1;
            if *counter <= 0 {
                willow_sync_safepoint();
            }
            *frame.cast::<bool>() = true;
        }
        RUNTIME_POLL_READY
    }

    #[test]
    fn sync_inline_grant_is_invalidated_after_nested_quantum() {
        let _guard = crate::gc::runtime_test_guard();
        let mut finished = false;
        let mut stack = NativeStack::new(
            partial_grant_across_nested_quantum,
            std::ptr::from_mut(&mut finished).cast(),
        );
        begin_quantum(
            PreemptConfig::from_env_values(Some("1024"), Some("100000")),
            std::ptr::null(),
        );
        let result = unsafe { NativeStack::resume(&mut *stack) };
        willow_preempt_end();
        let finished_before_resume = finished;
        if result == RUNTIME_POLL_PREEMPTED {
            quantum();
            assert_eq!(
                unsafe { NativeStack::resume(&mut *stack) },
                RUNTIME_POLL_READY
            );
            willow_preempt_end();
        }
        assert_eq!(result, RUNTIME_POLL_PREEMPTED);
        assert!(!finished_before_resume);
        assert!(finished);
    }

    unsafe extern "C" fn suppressed_mixed_poll(frame: *mut c_void) -> i32 {
        use crate::preempt::{
            willow_preempt_check, willow_preempt_enter_no_preempt, willow_preempt_leave_no_preempt,
        };
        let counter = crate::preempt::willow_sync_poll_counter();
        willow_preempt_enter_no_preempt();
        willow_preempt_enter_no_preempt();
        for _ in 0..100 {
            unsafe {
                *counter -= 1;
            }
            if unsafe { *counter } <= 0 {
                willow_sync_safepoint();
            }
            assert_eq!(willow_preempt_check(), 0);
        }
        willow_preempt_leave_no_preempt();
        assert_eq!(willow_preempt_check(), 0);
        willow_preempt_leave_no_preempt();
        // The region did not consume either of the shared budget's two units.
        unsafe {
            *frame.cast::<[i32; 2]>() = [willow_preempt_check(), willow_preempt_check()];
        }
        RUNTIME_POLL_READY
    }

    #[test]
    fn mixed_sync_async_no_preempt_region_preserves_budget() {
        let _guard = crate::gc::runtime_test_guard();
        let mut checks = [-1; 2];
        let mut stack = NativeStack::new(
            suppressed_mixed_poll,
            std::ptr::from_mut(&mut checks).cast(),
        );
        begin_quantum(
            PreemptConfig::from_env_values(Some("2"), Some("100000")),
            std::ptr::null(),
        );
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_READY
        );
        willow_preempt_end();
        assert_eq!(checks, [0, 1]);
    }

    struct MixedDeadline {
        sync_last: bool,
        reached_end: bool,
    }

    unsafe extern "C" fn mixed_deadline_poll(frame: *mut c_void) -> i32 {
        let state = frame.cast::<MixedDeadline>();
        if unsafe { (*state).sync_last } {
            crate::preempt::willow_preempt_check();
        } else {
            willow_sync_safepoint();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        if unsafe { (*state).sync_last } {
            willow_sync_safepoint();
        } else if crate::preempt::willow_preempt_check() != 0 {
            suspend();
        }
        unsafe {
            (*state).reached_end = true;
        }
        RUNTIME_POLL_READY
    }

    #[test]
    fn mixed_sync_async_share_deadline_without_rearming() {
        let _guard = crate::gc::runtime_test_guard();
        for sync_last in [false, true] {
            for quantum_ms in ["1", "100000"] {
                let mut state = MixedDeadline {
                    sync_last,
                    reached_end: false,
                };
                let config =
                    PreemptConfig::from_env_values(Some("1000000000000"), Some(quantum_ms));
                let mut stack =
                    NativeStack::new(mixed_deadline_poll, std::ptr::from_mut(&mut state).cast());
                begin_quantum(config, std::ptr::null());
                let result = unsafe { NativeStack::resume(&mut *stack) };
                willow_preempt_end();
                let reached_end = state.reached_end;
                // Drain even an unexpected early suspension before asserting.
                let mut resumed = result;
                while resumed != RUNTIME_POLL_READY {
                    begin_quantum(config, std::ptr::null());
                    resumed = unsafe { NativeStack::resume(&mut *stack) };
                    willow_preempt_end();
                }
                if quantum_ms == "1" {
                    assert_eq!(result, RUNTIME_POLL_PREEMPTED);
                    assert!(!reached_end);
                } else {
                    assert_eq!(result, RUNTIME_POLL_READY);
                    assert!(reached_end);
                }
            }
        }
    }

    unsafe extern "C" fn timed_poll(frame: *mut c_void) -> i32 {
        let state = frame.cast::<CountedLoop>();
        let counter = crate::preempt::willow_sync_poll_counter();
        unsafe {
            while (*state).resumes == 0 {
                (*state).safepoints += 1;
                std::thread::sleep(std::time::Duration::from_micros(20));
                *counter -= 1;
                if *counter <= 0 {
                    (*state).runtime_calls += 1;
                    assert_eq!(willow_sync_safepoint(), 0);
                }
            }
        }
        RUNTIME_POLL_READY
    }

    #[test]
    fn inline_countdown_still_honors_the_time_quantum() {
        use crate::preempt::SYNC_POLL_STRIDE;
        let _guard = crate::gc::runtime_test_guard();
        let mut state = CountedLoop {
            safepoints: 0,
            runtime_calls: 0,
            resumes: 0,
            trips: Vec::new(),
        };
        let mut stack = NativeStack::new(timed_poll, std::ptr::from_mut(&mut state).cast());
        // A budget that cannot trip: only the 1ms time quantum preempts.
        let config = PreemptConfig::from_env_values(Some("1000000000000"), Some("1"));
        begin_quantum(config, std::ptr::null());
        let result = unsafe { NativeStack::resume(&mut *stack) };
        willow_preempt_end();
        assert_eq!(result, RUNTIME_POLL_PREEMPTED);
        state.resumes += 1;
        begin_quantum(config, std::ptr::null());
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_READY
        );
        willow_preempt_end();
        assert!(state.runtime_calls <= state.safepoints.div_ceil(SYNC_POLL_STRIDE) + 1);
    }

    fn set_reference(name: &str) {
        let ptr = crate::string::willow_string_alloc(name.as_bytes().as_ptr(), name.len() as i64);
        crate::reference_debug::willow_debug_reference_call(
            ptr, 1, 1, ptr, ptr, ptr, ptr, ptr, ptr,
        );
    }

    unsafe extern "C" fn reference_poll(_: *mut c_void) -> i32 {
        crate::reference_debug::willow_debug_reference_call_scope_push();
        set_reference("outer-task-call");
        crate::reference_debug::willow_debug_reference_call_scope_push();
        set_reference("inner-task-call");
        assert!(suspend());
        assert_eq!(
            crate::reference_debug::current_reference_call()
                .unwrap()
                .callee,
            "inner-task-call"
        );
        crate::reference_debug::clear_current_reference_call();
        assert_eq!(
            crate::reference_debug::current_reference_call()
                .unwrap()
                .callee,
            "outer-task-call"
        );
        crate::reference_debug::clear_current_reference_call();
        RUNTIME_POLL_READY
    }

    #[test]
    fn suspended_stack_keeps_nested_reference_context_private() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        let saved = crate::reference_debug::replace_current(Default::default());
        let mut stack = NativeStack::new(reference_poll, std::ptr::null_mut());
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_PREEMPTED
        );
        assert!(crate::reference_debug::current_reference_call().is_none());
        set_reference("scheduler-context");
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_READY
        );
        assert_eq!(
            crate::reference_debug::current_reference_call()
                .unwrap()
                .callee,
            "scheduler-context"
        );
        crate::reference_debug::replace_current(saved);
    }

    #[test]
    fn task_overflow_names_survive_suspend_peers_and_recycling() {
        let _guard = crate::gc::runtime_test_guard();
        const NAMES: [&[u8]; 3] = [b"first", b"second_longer", b"third"];
        unsafe extern "C" fn poll(frame: *mut c_void) -> i32 {
            let name = NAMES[frame as usize % NAMES.len()];
            crate::stack_overflow::willow_stack_helper_set(name.as_ptr(), name.len() as i64);
            assert!(suspend());
            assert_eq!(
                crate::stack_overflow::replace_task_helper(Some(name)),
                Some(name)
            );
            RUNTIME_POLL_READY
        }
        unsafe extern "C" fn ready(_: *mut c_void) -> i32 {
            RUNTIME_POLL_READY
        }
        for count in [1, 8, 64] {
            let mut stacks: Vec<_> = (0..count)
                .map(|index| NativeStack::acquire(poll, index as *mut c_void))
                .collect();
            for (index, stack) in stacks.iter_mut().enumerate() {
                assert_eq!(
                    unsafe { NativeStack::resume(&mut **stack) },
                    RUNTIME_POLL_PREEMPTED
                );
                assert_eq!(crate::stack_overflow::replace_task_helper(None), None);
                assert_eq!(stack.overflow_helper, Some(NAMES[index % NAMES.len()]));
            }
            for mut stack in stacks {
                assert_eq!(
                    unsafe { NativeStack::resume(&mut *stack) },
                    RUNTIME_POLL_READY
                );
                NativeStack::recycle(stack);
            }
            let mut recycled = NativeStack::acquire(ready, std::ptr::null_mut());
            assert_eq!(recycled.overflow_helper, Some(&b"<unknown>"[..]));
            assert_eq!(
                unsafe { NativeStack::resume(&mut *recycled) },
                RUNTIME_POLL_READY
            );
            assert_eq!(crate::stack_overflow::replace_task_helper(None), None);
            NativeStack::recycle(recycled);
        }
    }

    unsafe extern "C" fn trace_poll(_: *mut c_void) -> i32 {
        let name = b"native_helper";
        crate::stack_trace::willow_callstack_push(
            name.as_ptr(),
            name.len() as i64,
            std::ptr::null(),
            0,
            1,
            1,
        );
        assert!(suspend());
        assert!(crate::stack_trace::current_call_stack_text().contains("native_helper"));
        crate::stack_trace::willow_callstack_pop();
        RUNTIME_POLL_READY
    }

    #[test]
    fn suspended_stack_keeps_trace_private_on_its_owner_thread() {
        let _guard = crate::gc::runtime_test_guard();
        let mut stack = NativeStack::new(trace_poll, std::ptr::null_mut());
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_PREEMPTED
        );
        assert!(!crate::stack_trace::current_call_stack_text().contains("native_helper"));
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_READY
        );
        assert!(!crate::stack_trace::current_call_stack_text().contains("native_helper"));
    }

    unsafe extern "C" fn rooted_poll(_: *mut c_void) -> i32 {
        let mut object = crate::gc::willow_alloc(8);
        unsafe {
            *object.cast::<u64>() = 0x1234_abcd;
        }
        crate::gc::willow_push_root(&mut object);
        assert!(suspend());
        assert_eq!(unsafe { *object.cast::<u64>() }, 0x1234_abcd);
        crate::gc::willow_pop_root();
        RUNTIME_POLL_READY
    }

    #[test]
    fn parked_native_slots_survive_collection_and_leave_no_thread_roots() {
        let _guard = crate::gc::runtime_test_guard();
        let depth = crate::gc::gc_thread_root_depth();
        let mut stack = NativeStack::new(rooted_poll, std::ptr::null_mut());
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_PREEMPTED
        );
        assert_eq!(crate::gc::gc_thread_root_depth(), depth);
        crate::gc::willow_gc_collect();
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_READY
        );
        assert_eq!(crate::gc::gc_thread_root_depth(), depth);
    }
    #[test]
    fn unwind_reason_distinguishes_cancel_panic_and_cleanup_suppression() {
        use crate::panic_context::*;
        use willow_abi::unwind_reason::{CANCEL, NONE, PANIC};
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        let saved = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(901))));
        unsafe extern "C" fn poll(_: *mut c_void) -> i32 {
            assert_eq!(willow_unwind_reason(), CANCEL);
            assert_eq!(willow_panic_active(), 0);
            assert_eq!(willow_panic_depth(), 0);
            willow_panic_enter_defer();
            assert!(willow_panic_recover().is_null());
            willow_panic_leave_defer();
            let message = crate::string::willow_string_alloc(b"cleanup".as_ptr(), 7);
            willow_panic_raise(message, std::ptr::null(), 0, 0);
            assert_eq!(willow_unwind_reason(), PANIC | CANCEL);
            assert_eq!(willow_panic_depth(), 1);
            cleanup_enter();
            cleanup_enter();
            assert_eq!(willow_unwind_reason(), PANIC);
            willow_panic_enter_defer();
            let info = willow_panic_recover();
            willow_panic_release_recovered(info);
            willow_panic_leave_defer();
            assert_eq!(willow_unwind_reason(), NONE);
            cleanup_leave();
            assert_eq!(willow_unwind_reason(), NONE);
            cleanup_leave();
            assert_eq!(willow_unwind_reason(), CANCEL);
            assert_eq!(willow_panic_active(), 0);
            RUNTIME_POLL_READY
        }
        let mut stack = NativeStack::new(poll, std::ptr::null_mut());
        stack.cancelled = true;
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_READY
        );
        assert_eq!(willow_unwind_reason(), NONE);
        drop(stack);
        replace_current_context(saved);
    }

    /// Reproducible relative ABI call cost; timings are evidence, not a CI gate.
    #[test]
    #[ignore = "manual release microbenchmark"]
    fn unwind_reason_fast_path_measurement() {
        use crate::panic_context::*;
        fn measure(label: &str) {
            for n in [100_000, 1_000_000] {
                for sample in 0..5 {
                    for (name, query) in [
                        (
                            "panic_active",
                            willow_panic_active as extern "C" fn() -> i32,
                        ),
                        ("sync_cancelled", crate::preempt::willow_sync_cancelled),
                        ("unwind_reason", willow_unwind_reason),
                    ] {
                        let query = std::hint::black_box(query);
                        let start = std::time::Instant::now();
                        for _ in 0..n {
                            std::hint::black_box(query());
                        }
                        println!(
                            "state={label} n={n} sample={sample} query={name} ns={}",
                            start.elapsed().as_nanos()
                        );
                    }
                }
            }
        }
        unsafe extern "C" fn poll(_: *mut c_void) -> i32 {
            measure("task-clean");
            let stack = CURRENT.with(Cell::get);
            unsafe {
                (*stack).cancelled = true;
            }
            measure("task-cancelled");
            cleanup_enter();
            measure("task-cleanup");
            cleanup_leave();
            RUNTIME_POLL_READY
        }
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        let saved = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(902))));
        measure("sync-clean");
        let mut stack = NativeStack::new(poll, std::ptr::null_mut());
        assert_eq!(
            unsafe { NativeStack::resume(&mut *stack) },
            RUNTIME_POLL_READY
        );
        drop(stack);
        replace_current_context(saved);
    }

    static CANCEL_TARGET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static DEFER_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static PEER_RAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    unsafe extern "C" fn cancellable_poll(_: *mut c_void) -> i32 {
        fn helper(depth: usize) {
            if depth > 0 {
                helper(depth - 1);
            } else {
                while willow_sync_safepoint() == 0 {
                    std::hint::spin_loop();
                }
            }
            // Model the compiler's cancellation defer edges at every depth.
            assert_ne!(crate::preempt::willow_sync_cancelled(), 0);
            assert_eq!(
                crate::panic_context::willow_unwind_reason(),
                willow_abi::unwind_reason::CANCEL
            );
            assert_eq!(crate::panic_context::willow_panic_active(), 0);
            crate::preempt::willow_sync_cleanup_enter();
            assert_eq!(crate::preempt::willow_sync_cancelled(), 0);
            DEFER_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            crate::preempt::willow_sync_cleanup_leave();
        }
        helper(32);
        crate::scheduler::willow_sync_poll_cancel_cleanup();
        RUNTIME_POLL_READY
    }

    unsafe extern "C" fn cancel_peer(_: *mut c_void) -> i32 {
        PEER_RAN.store(true, std::sync::atomic::Ordering::SeqCst);
        crate::scheduler::willow_sched_cancel(
            CANCEL_TARGET.load(std::sync::atomic::Ordering::SeqCst),
        );
        RUNTIME_POLL_READY
    }

    unsafe extern "C" fn frame_cleanup(_: *mut c_void) {
        assert_eq!(DEFER_COUNT.load(std::sync::atomic::Ordering::SeqCst), 33);
        DEFER_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn synchronous_loop_yields_to_peer_and_cancellation_unwinds_every_frame() {
        let _guard = crate::gc::runtime_test_guard();
        let _single = crate::scheduler::single_worker_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        DEFER_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
        PEER_RAN.store(false, std::sync::atomic::Ordering::SeqCst);
        let frame = crate::async_frame::willow_async_frame_alloc(0, 0);
        let target = crate::scheduler::willow_sched_spawn(cancellable_poll, frame);
        crate::scheduler::willow_sched_set_cancel_fn(target, frame_cleanup);
        CANCEL_TARGET.store(target, std::sync::atomic::Ordering::SeqCst);
        crate::scheduler::willow_sched_spawn(cancel_peer, std::ptr::null_mut());
        crate::scheduler::willow_sched_run();
        assert!(PEER_RAN.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(DEFER_COUNT.load(std::sync::atomic::Ordering::SeqCst), 34);
        crate::scheduler::reset_global_scheduler_for_test();
    }

    static AFFINITY_THREAD: std::sync::Mutex<Option<std::thread::ThreadId>> =
        std::sync::Mutex::new(None);
    unsafe extern "C" fn affinity_poll(_: *mut c_void) -> i32 {
        let original = std::thread::current().id();
        *AFFINITY_THREAD.lock().unwrap() = Some(original);
        while willow_sync_safepoint() == 0 {
            assert_eq!(std::thread::current().id(), original);
        }
        assert_eq!(std::thread::current().id(), original);
        RUNTIME_POLL_READY
    }
    unsafe extern "C" fn ready_peer(_: *mut c_void) -> i32 {
        RUNTIME_POLL_READY
    }

    #[test]
    fn suspended_native_stack_keeps_os_owner_across_separate_scheduler_drives() {
        let _guard = crate::gc::runtime_test_guard();
        let _single = crate::scheduler::single_worker_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        *AFFINITY_THREAD.lock().unwrap() = None;
        let target = crate::scheduler::willow_sched_spawn(affinity_poll, std::ptr::null_mut());
        let peer = crate::scheduler::willow_sched_spawn(ready_peer, std::ptr::null_mut());
        crate::scheduler::willow_sched_run_until(peer);
        assert!(AFFINITY_THREAD.lock().unwrap().is_some());
        crate::scheduler::willow_sched_cancel(target);
        crate::scheduler::willow_sched_run();
        crate::scheduler::reset_global_scheduler_for_test();
    }

    unsafe extern "C" fn lengthy_async_cleanup(_: *mut c_void) {
        for _ in 0..10_000 {
            assert_eq!(willow_sync_safepoint(), 0);
        }
        assert!(PEER_RAN.load(std::sync::atomic::Ordering::SeqCst));
    }
    unsafe extern "C" fn mark_peer(_: *mut c_void) -> i32 {
        PEER_RAN.store(true, std::sync::atomic::Ordering::SeqCst);
        RUNTIME_POLL_READY
    }

    #[test]
    fn initial_async_cancellation_cleanup_can_yield_its_native_helpers() {
        let _guard = crate::gc::runtime_test_guard();
        let _single = crate::scheduler::single_worker_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        PEER_RAN.store(false, std::sync::atomic::Ordering::SeqCst);
        let frame = crate::async_frame::willow_async_frame_alloc(0, 0);
        let target = crate::scheduler::willow_sched_spawn(ready_peer, frame);
        crate::scheduler::willow_sched_set_cancel_fn(target, lengthy_async_cleanup);
        crate::scheduler::willow_sched_cancel(target);
        crate::scheduler::willow_sched_spawn(mark_peer, std::ptr::null_mut());
        crate::scheduler::willow_sched_run();
        crate::scheduler::reset_global_scheduler_for_test();
    }
    unsafe extern "C" fn runtime_mapper_poll(_: *mut c_void) -> i32 {
        while willow_sync_safepoint() == 0 {
            std::hint::spin_loop();
        }
        RUNTIME_POLL_READY
    }
    unsafe extern "C" fn runtime_mapper_cleanup(_: *mut c_void) {
        DEFER_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn runtime_poll_without_generated_epilogue_still_runs_registered_cleanup() {
        let _guard = crate::gc::runtime_test_guard();
        let _single = crate::scheduler::single_worker_for_test();
        crate::scheduler::reset_global_scheduler_for_test();
        DEFER_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
        let frame = crate::async_frame::willow_async_frame_alloc(0, 0);
        let target = crate::scheduler::willow_sched_spawn(runtime_mapper_poll, frame);
        crate::scheduler::willow_sched_set_cancel_fn(target, runtime_mapper_cleanup);
        CANCEL_TARGET.store(target, std::sync::atomic::Ordering::SeqCst);
        crate::scheduler::willow_sched_spawn(cancel_peer, std::ptr::null_mut());
        crate::scheduler::willow_sched_run();
        assert_eq!(DEFER_COUNT.load(std::sync::atomic::Ordering::SeqCst), 1);
        crate::scheduler::reset_global_scheduler_for_test();
    }
}
