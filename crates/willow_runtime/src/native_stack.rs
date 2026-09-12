//! Task-owned native stacks. A synchronous safepoint suspends the complete
//! call chain; the scheduler can run another task without recursive re-entry.
//!
//! Context objects and stack mappings stay at stable addresses. No TLS borrow,
//! scheduler lock, or GC registry lock crosses a context switch. Generated code
//! is pinned to its scheduler worker while native frames remain suspended.

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

pub(crate) struct NativeStack {
    pub(crate) worker: usize,
    owner_thread: std::thread::ThreadId,
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
    cleanup_depth: u32,
    root_depth: usize,
    parked_roots: Option<u64>,
    trace: RuntimeStackTrace,
    reference_context: crate::reference_debug::ReferenceCallState,
}

impl std::fmt::Debug for NativeStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeStack")
            .field("suspended", &self.suspended)
            .finish_non_exhaustive()
    }
}

// SAFETY: the scheduler transfers exclusive ownership only while this stack is
// suspended and dispatches it only to its owning worker. The task allocation and
// coroutine stack retain stable addresses; user frames never migrate.
unsafe impl Send for NativeStack {}

impl NativeStack {
    pub(crate) fn acquire(poll: RuntimePollFn, frame: *mut c_void) -> Box<Self> {
        if let Some(mut stack) = IDLE_STACKS.with(|pool| pool.borrow_mut().pop()) {
            stack.poll = poll;
            stack.cancel_entry = None;
            stack.frame = frame;
            stack.cancelled = false;
            stack.cleanup_depth = 0;
            stack.trace = RuntimeStackTrace::default();
            stack.reference_context = Default::default();
            stack
        } else {
            Self::new(poll, frame)
        }
    }

    pub(crate) fn acquire_cleanup(cancel: RuntimeCancelFn, frame: *mut c_void) -> Box<Self> {
        unsafe extern "C" fn unused_poll(_: *mut c_void) -> i32 {
            unreachable!()
        }
        let mut stack = Self::acquire(unused_poll, frame);
        stack.cancel_entry = Some(cancel);
        stack.cancelled = true;
        stack.cleanup_depth = 1;
        stack
    }

    pub(crate) fn recycle(stack: Box<Self>) {
        assert!(!stack.suspended);
        IDLE_STACKS.with(|pool| pool.borrow_mut().push(stack));
    }

    pub(crate) fn new(poll: RuntimePollFn, frame: *mut c_void) -> Box<Self> {
        // Windows stacks inherit the current thread's overflow guarantee.
        crate::stack_overflow::protect_current_thread();
        let storage = corosensei::stack::DefaultStack::new(8 * 1024 * 1024)
            .expect("cannot allocate task native stack");
        #[cfg(unix)]
        let guard = {
            let lower = storage.limit().get();
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            (lower, lower + page)
        };
        Box::new(Self {
            worker: crate::scheduler::current_worker(),
            owner_thread: std::thread::current().id(),
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
            cleanup_depth: 0,
            root_depth: 0,
            parked_roots: None,
            trace: RuntimeStackTrace::default(),
            reference_context: Default::default(),
        })
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
        unsafe {
            assert_eq!(
                (*stack).owner_thread,
                std::thread::current().id(),
                "native task stack resumed on a different OS worker"
            );
            let previous = CURRENT.with(|slot| slot.replace(stack));
            let outer_trace = crate::stack_trace::snapshot_current();
            let outer_reference = crate::reference_debug::snapshot_current();
            (*stack).root_depth = crate::gc::gc_thread_root_depth();
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
            // Borrow only the coroutine field: user code accesses the other
            // fields through CURRENT while this activation is suspended.
            let switched = (*std::ptr::addr_of_mut!((*stack).coroutine)).resume(());
            #[cfg(unix)]
            crate::stack_overflow::replace_guard(guard.0, guard.1);
            assert!(matches!(switched, CoroutineResult::Yield(())));
            (*stack).trace = crate::stack_trace::replace_current(outer_trace);
            (*stack).reference_context = crate::reference_debug::replace_current(outer_reference);
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

fn trampoline(yielder: &Yielder<(), ()>) {
    loop {
        let stack = CURRENT.with(Cell::get);
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
                cancel(frame);
                (*stack).cleanup_depth = 0;
            }
            if crate::panic_context::willow_panic_active() != 0 {
                crate::task::RUNTIME_POLL_PANICKED
            } else {
                crate::task::RUNTIME_POLL_READY
            }
        } else {
            unsafe { poll(frame) }
        };
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
    let stack = CURRENT.with(Cell::get);
    if stack.is_null() {
        return false;
    }
    unsafe {
        (*stack).parked_roots = Some(crate::gc::park_current_roots((*stack).root_depth));
        (*stack).result = RUNTIME_POLL_PREEMPTED;
        (*stack).suspended = true;
        (*(*stack).yielder).suspend(());
    }
    true
}

/// Whether the current invocation owns a scheduler-managed native stack.
pub(crate) fn is_active() -> bool {
    CURRENT.with(|current| !current.get().is_null())
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
            let task = crate::scheduler::willow_sched_current_task();
            (*stack).cancelled = crate::scheduler::willow_sched_is_cancelled(task) != 0;
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
                .contains("Willow runtime error: native stack overflow"),
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
