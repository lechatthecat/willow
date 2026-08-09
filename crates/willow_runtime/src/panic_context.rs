//! Explicit Willow language-panic state (willow-s9ej.2).
//!
//! This module is deliberately independent of Rust unwinding. A context owns a
//! stack of GC-rooted [`PanicInfo`] objects; generated code in later stages will
//! raise, query, recover, and finish those records through the stable C ABI.
//! The scheduler installs the context belonging to the task it is polling, and
//! synchronous `main` installs a standalone context in `runtime_start`.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::gc::{
    willow_alloc_typed, willow_gc_add_runtime_root, willow_gc_remove_runtime_root,
    willow_pop_roots, willow_push_root,
};
use crate::string::{willow_string_alloc, willow_string_as_str};

/// `PanicInfo` has the ordinary Willow class-object prefix followed by its four
/// public read-only fields:
///
/// ```text
/// offset  0: runtime type id (reserved; no user construction/dispatch)
/// offset  8: message: String
/// offset 16: file: String
/// offset 24: line: i64
/// offset 32: column: i64
/// ```
pub const PANIC_INFO_PAYLOAD_SIZE: i64 = 40;
const PANIC_INFO_REF_MASK: u64 = (1 << 1) | (1 << 2);

#[derive(Debug, Clone)]
struct PanicRecord {
    info: usize,
    /// Diagnostic metadata is captured at raise time. Generated propagation
    /// balances and removes caller shadow frames before an outer boundary
    /// eventually reports the panic (willow-s9ej.4).
    call_stack: String,
    reference_call: Option<String>,
}

#[derive(Debug, Default)]
struct PanicState {
    active: Vec<PanicRecord>,
    /// A recovered value remains a runtime root until generated code has put it
    /// in a GC-visible Option/local and acknowledges the ownership transfer.
    returned_roots: Vec<usize>,
    panic_defer_depth: u32,
}

/// Panic state owned by one Willow execution context (task or synchronous
/// entry). It is `Arc`-backed so its address and ownership survive task
/// migration; TLS only caches the context currently installed on a worker.
#[derive(Debug)]
pub struct PanicContext {
    owner: u64,
    state: Mutex<PanicState>,
}

impl PanicContext {
    pub fn new(owner: u64) -> Self {
        Self {
            owner,
            state: Mutex::new(PanicState::default()),
        }
    }

    pub fn owner(&self) -> u64 {
        self.owner
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PanicState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn active(&self) -> bool {
        !self.lock().active.is_empty()
    }

    fn depth(&self) -> usize {
        self.lock().active.len()
    }

    fn push(&self, info: *mut u8) {
        self.lock().active.push(PanicRecord {
            info: info as usize,
            call_stack: crate::stack_trace::current_call_stack_text(),
            reference_call: crate::reference_debug::current_reference_call()
                .as_ref()
                .map(crate::reference_debug::reference_call_context_text),
        });
    }

    fn enter_defer(&self) {
        let mut state = self.lock();
        state.panic_defer_depth = state
            .panic_defer_depth
            .checked_add(1)
            .unwrap_or_else(|| fatal_invariant("panic-defer depth overflow"));
    }

    fn leave_defer(&self) {
        let mut state = self.lock();
        state.panic_defer_depth = state
            .panic_defer_depth
            .checked_sub(1)
            .unwrap_or_else(|| fatal_invariant("panic-defer depth underflow"));
    }

    fn recover(&self) -> *mut u8 {
        let mut state = self.lock();
        if state.panic_defer_depth == 0 {
            return std::ptr::null_mut();
        }
        let Some(record) = state.active.pop() else {
            return std::ptr::null_mut();
        };
        state.returned_roots.push(record.info);
        record.info as *mut u8
    }

    fn release_recovered(&self, info: *mut u8) -> bool {
        if info.is_null() {
            return false;
        }
        let removed = {
            let mut state = self.lock();
            let Some(index) = state
                .returned_roots
                .iter()
                .rposition(|candidate| *candidate == info as usize)
            else {
                return false;
            };
            state.returned_roots.swap_remove(index);
            true
        };
        if removed {
            willow_gc_remove_runtime_root(info);
        }
        removed
    }

    fn snapshot(&self) -> PanicContextSnapshot {
        let state = self.lock();
        PanicContextSnapshot {
            owner: self.owner,
            active: state.active.len(),
            returned_roots: state.returned_roots.len(),
            panic_defer_depth: state.panic_defer_depth,
        }
    }

    fn diagnostic_records(&self) -> Vec<PanicRecord> {
        self.lock().active.clone()
    }
}

impl Drop for PanicContext {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for info in state
            .active
            .drain(..)
            .map(|record| record.info)
            .chain(state.returned_roots.drain(..))
        {
            willow_gc_remove_runtime_root(info as *mut u8);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanicContextSnapshot {
    pub owner: u64,
    pub active: usize,
    pub returned_roots: usize,
    pub panic_defer_depth: u32,
}

thread_local! {
    static CURRENT_CONTEXT: RefCell<Option<Arc<PanicContext>>> = const { RefCell::new(None) };
}

/// Replace the worker-local cache and return the previously installed context.
/// Ownership remains in `Arc`; TLS never owns panic state by itself.
pub fn replace_current_context(context: Option<Arc<PanicContext>>) -> Option<Arc<PanicContext>> {
    CURRENT_CONTEXT.with(|slot| slot.replace(context))
}

pub fn current_context() -> Option<Arc<PanicContext>> {
    CURRENT_CONTEXT.with(|slot| slot.borrow().clone())
}

pub fn current_snapshot() -> Option<PanicContextSnapshot> {
    current_context().map(|context| context.snapshot())
}

fn require_current_context() -> Arc<PanicContext> {
    current_context().unwrap_or_else(|| {
        fatal_invariant("language panic ABI called without an installed execution context")
    })
}

pub(crate) fn fatal_invariant(message: &str) -> ! {
    eprintln!("runtime fatal: {message}");
    std::process::abort();
}

fn alloc_string_or_empty(input: *const u8, fallback: &str) -> *mut u8 {
    if !input.is_null() {
        return input as *mut u8;
    }
    willow_string_alloc(fallback.as_ptr(), fallback.len() as i64)
}

fn alloc_panic_info(message: *const u8, file: *const u8, line: i64, column: i64) -> *mut u8 {
    let mut message = alloc_string_or_empty(message, "explicit panic");
    willow_push_root(&mut message);
    let mut file = alloc_string_or_empty(file, "");
    willow_push_root(&mut file);

    let info = willow_alloc_typed(PANIC_INFO_PAYLOAD_SIZE, PANIC_INFO_REF_MASK);
    if info.is_null() {
        fatal_invariant("PanicInfo allocation failed");
    }
    unsafe {
        *(info as *mut i64) = 0;
        *(info.add(8) as *mut *mut u8) = message;
        *(info.add(16) as *mut *mut u8) = file;
        *(info.add(24) as *mut i64) = line.max(0);
        *(info.add(32) as *mut i64) = column.max(0);
    }
    willow_pop_roots(2);
    willow_gc_add_runtime_root(info);
    info
}

/// Raise a recoverable user-visible language fault from a Rust runtime helper.
/// The helper must return an ABI-neutral value immediately afterwards; it must
/// never continue the failed operation. `file` is an optional Willow String.
pub(crate) fn raise_language_message_at(message: &str, file: *const u8, line: i64, column: i64) {
    let mut rooted_file = file as *mut u8;
    let root_file = !rooted_file.is_null();
    if root_file {
        willow_push_root(&mut rooted_file);
    }
    let message = crate::string::willow_string_from_str(message);
    willow_panic_raise(message, rooted_file, line, column);
    if root_file {
        willow_pop_roots(1);
    }
}

/// Raise a runtime fault that has no location of its own (array bounds, a
/// blocked channel operation, awaiting a cancelled task). Debug builds publish
/// the enclosing statement through [`willow_fault_site_set`] before every
/// runtime call that can raise, so the fault still records `file:line:column`;
/// release builds keep the empty location (willow-s9ej.7).
pub(crate) fn raise_language_message(message: &str) {
    let (file, line, column) = current_fault_site();
    if file.is_empty() {
        raise_language_message_at(message, std::ptr::null(), line, column);
        return;
    }
    let mut file_string = willow_string_alloc(file.as_ptr(), file.len() as i64);
    willow_push_root(&mut file_string);
    raise_language_message_at(message, file_string, line, column);
    willow_pop_roots(1);
}

thread_local! {
    /// Statement being executed by generated debug-build code on this thread.
    /// Set immediately before each runtime call that can raise, so it is never
    /// read as a stale value by the fault it describes.
    static FAULT_SITE: RefCell<(String, i64, i64)> = const { RefCell::new((String::new(), 0, 0)) };
}

fn current_fault_site() -> (String, i64, i64) {
    FAULT_SITE.with(|site| site.borrow().clone())
}

/// Publish the source location generated code is about to execute. `file` is a
/// pointer to raw static UTF-8 bytes (NOT a Willow String) with an explicit
/// length, copied onto the Rust heap so the fault site never allocates on the
/// GC heap. Debug builds only; release builds never call this.
#[unsafe(no_mangle)]
pub extern "C" fn willow_fault_site_set(file: *const u8, file_len: i64, line: i64, column: i64) {
    let file = if file.is_null() || file_len <= 0 {
        String::new()
    } else {
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(file, file_len as usize) })
            .into_owned()
    };
    FAULT_SITE.with(|site| *site.borrow_mut() = (file, line.max(0), column.max(0)));
}

/// Drop the published fault site (used by tests and by contexts that leave
/// generated code, so a later fault cannot inherit an unrelated location).
#[unsafe(no_mangle)]
pub extern "C" fn willow_fault_site_clear() {
    FAULT_SITE.with(|site| *site.borrow_mut() = (String::new(), 0, 0));
}

/// True while generated code is running a defer as compiler-driven cleanup —
/// panic unwinding or cancellation cleanup. Scheduler-driving compatibility
/// APIs use this to reject nested drives before another task can run.
///
/// The depth alone is the signal: `willow_panic_enter_defer` is emitted only by
/// the panic-cleanup and cancel-cleanup entries, never by ordinary defer
/// execution. Requiring an active panic record on top of it would drop the
/// guard exactly where it is still needed — after `recover()` consumed the
/// panic but the deferred body is still running, and throughout cancellation
/// cleanup, which has no panic record at all (willow-s9ej.7).
pub(crate) fn panic_unwind_cleanup_active() -> bool {
    current_context().is_some_and(|context| context.snapshot().panic_defer_depth > 0)
}

/// Push a recoverable language panic onto the current execution context.
/// Unlike the legacy `willow_panic` entry, this returns to compiler-generated
/// propagation code and never invokes host unwinding.
#[unsafe(no_mangle)]
pub extern "C" fn willow_panic_raise(message: *const u8, file: *const u8, line: i64, column: i64) {
    let context = require_current_context();
    let info = alloc_panic_info(message, file, line, column);
    context.push(info);
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_panic_active() -> i32 {
    current_context().is_some_and(|context| context.active()) as i32
}

/// Number of active language-panic records in the current execution context.
/// Generated calls compare before/after depth so an already-active outer panic
/// does not make a harmless helper look like it raised a nested panic.
#[unsafe(no_mangle)]
pub extern "C" fn willow_panic_depth() -> i32 {
    current_context().map_or(0, |context| context.depth().try_into().unwrap_or(i32::MAX))
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_panic_enter_defer() {
    require_current_context().enter_defer();
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_panic_leave_defer() {
    require_current_context().leave_defer();
}

/// Consume the top panic only while compiler-generated panic-defer code is
/// active. The returned object remains runtime-rooted until
/// [`willow_panic_release_recovered`] acknowledges transfer to GC-visible
/// generated storage.
#[unsafe(no_mangle)]
pub extern "C" fn willow_panic_recover() -> *mut u8 {
    require_current_context().recover()
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_panic_release_recovered(info: *mut u8) {
    if !require_current_context().release_recovered(info) {
        fatal_invariant("released PanicInfo without a matching recover transfer");
    }
}

/// Finish an unhandled language panic. Compiler/runtime corruption must call a
/// separate fatal ABI and therefore never reaches this recoverable path.
#[unsafe(no_mangle)]
pub extern "C" fn willow_panic_finish_unhandled() -> ! {
    let async_chain = crate::scheduler::async_chain_text();
    finish_unhandled_with_async_chain(&async_chain)
}

/// Report an active task-owned language panic after the scheduler has already
/// published/reaped the task as Panicked. The chain is captured before task
/// removal, while the TLS-held PanicContext keeps every PanicInfo rooted.
pub(crate) fn finish_unhandled_with_async_chain(async_chain: &str) -> ! {
    let context = require_current_context();
    let records = context.diagnostic_records();
    if records.is_empty() {
        fatal_invariant("finish-unhandled called without an active panic");
    }
    for (index, record) in records.iter().rev().enumerate() {
        let info = record.info as *mut u8;
        let message = unsafe { panic_info_message(info) };
        let file = unsafe { panic_info_file(info) };
        let line = unsafe { panic_info_line(info) };
        let column = unsafe { panic_info_column(info) };
        if index == 0 {
            if file.is_empty() {
                eprintln!("runtime panic: {message}");
            } else {
                eprintln!("runtime panic: {message} at {file}:{line}:{column}");
            }
        } else {
            eprintln!("while unwinding panic: {message}");
        }
        if index == 0 {
            if let Some(reference) = &record.reference_call {
                eprintln!("{reference}");
            }
            if !record.call_stack.is_empty() {
                eprintln!("{}", record.call_stack);
            }
        }
    }
    if !async_chain.is_empty() {
        eprintln!("{async_chain}");
    }
    std::process::abort();
}

/// # Safety
/// `info` must be a live `PanicInfo` payload allocated by this module.
pub unsafe fn panic_info_message(info: *mut u8) -> &'static str {
    unsafe { willow_string_as_str(*(info.add(8) as *mut *mut u8)) }
}

/// # Safety
/// `info` must be a live `PanicInfo` payload allocated by this module.
pub unsafe fn panic_info_file(info: *mut u8) -> &'static str {
    unsafe { willow_string_as_str(*(info.add(16) as *mut *mut u8)) }
}

/// # Safety
/// `info` must be a live `PanicInfo` payload allocated by this module.
pub unsafe fn panic_info_line(info: *mut u8) -> i64 {
    unsafe { *(info.add(24) as *mut i64) }
}

/// # Safety
/// `info` must be a live `PanicInfo` payload allocated by this module.
pub unsafe fn panic_info_column(info: *mut u8) -> i64 {
    unsafe { *(info.add(32) as *mut i64) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{
        runtime_root_count, runtime_test_guard, willow_gc_allocated_bytes, willow_gc_collect,
        willow_gc_init, willow_gc_minor_collect,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ContextTestGuard {
        previous: Option<Arc<PanicContext>>,
    }

    impl ContextTestGuard {
        fn install(owner: u64) -> Self {
            Self {
                previous: replace_current_context(Some(Arc::new(PanicContext::new(owner)))),
            }
        }
    }

    impl Drop for ContextTestGuard {
        fn drop(&mut self) {
            replace_current_context(self.previous.take());
        }
    }

    fn ws(value: &str) -> *mut u8 {
        willow_string_alloc(value.as_ptr(), value.len() as i64)
    }

    fn raise(message: &str, file: &str, line: i64, column: i64) {
        // The second string allocation can collect under
        // WILLOW_GC_STRESS=alloc; model generated call-argument rooting rather
        // than leaving the first raw pointer unprotected in the fixture.
        let mut message = ws(message);
        willow_push_root(&mut message);
        let file = ws(file);
        willow_panic_raise(message, file, line, column);
        willow_pop_roots(1);
    }

    fn recover_one() -> *mut u8 {
        willow_panic_enter_defer();
        let info = willow_panic_recover();
        willow_panic_leave_defer();
        info
    }

    #[test]
    fn panic_context_01_normal_path_is_inactive_and_recover_is_null() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(1);
        let before = willow_gc_allocated_bytes();
        assert_eq!(willow_panic_active(), 0);
        assert!(recover_one().is_null());
        assert_eq!(willow_gc_allocated_bytes(), before);
    }

    #[test]
    fn panic_context_02_raise_records_all_fields() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(2);
        raise("boom", "main.wi", 7, 9);
        assert_eq!(willow_panic_active(), 1);
        let info = recover_one();
        assert_eq!(unsafe { panic_info_message(info) }, "boom");
        assert_eq!(unsafe { panic_info_file(info) }, "main.wi");
        assert_eq!(unsafe { panic_info_line(info) }, 7);
        assert_eq!(unsafe { panic_info_column(info) }, 9);
        willow_panic_release_recovered(info);
    }

    #[test]
    fn panic_context_03_recover_requires_panic_defer_depth() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(3);
        raise("boom", "", 0, 0);
        assert!(willow_panic_recover().is_null());
        assert_eq!(willow_panic_active(), 1);
        let info = recover_one();
        assert!(!info.is_null());
        willow_panic_release_recovered(info);
    }

    #[test]
    fn panic_context_04_one_record_can_only_be_recovered_once() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(4);
        raise("once", "", 0, 0);
        let info = recover_one();
        assert_eq!(willow_panic_active(), 0);
        assert!(recover_one().is_null());
        willow_panic_release_recovered(info);
    }

    #[test]
    fn panic_context_05_nested_recover_pops_only_the_top_record() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(5);
        raise("outer", "", 0, 0);
        raise("inner", "", 0, 0);
        let inner = recover_one();
        assert_eq!(unsafe { panic_info_message(inner) }, "inner");
        assert_eq!(willow_panic_active(), 1);
        let outer = recover_one();
        assert_eq!(unsafe { panic_info_message(outer) }, "outer");
        assert_eq!(willow_panic_active(), 0);
        willow_panic_release_recovered(inner);
        willow_panic_release_recovered(outer);
    }

    #[test]
    fn panic_context_06_defer_depth_is_checked_and_nested() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(6);
        willow_panic_enter_defer();
        willow_panic_enter_defer();
        assert_eq!(current_snapshot().unwrap().panic_defer_depth, 2);
        willow_panic_leave_defer();
        willow_panic_leave_defer();
        assert_eq!(current_snapshot().unwrap().panic_defer_depth, 0);
    }

    #[test]
    fn panic_context_07_active_info_survives_minor_and_major_gc() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(7);
        raise("rooted message", "rooted.wi", 11, 12);
        willow_gc_minor_collect();
        willow_gc_collect();
        let info = recover_one();
        assert_eq!(unsafe { panic_info_message(info) }, "rooted message");
        assert_eq!(unsafe { panic_info_file(info) }, "rooted.wi");
        willow_panic_release_recovered(info);
    }

    #[test]
    fn panic_context_08_recovered_transfer_root_survives_collection() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(8);
        raise("transferred", "", 0, 0);
        let mut info = recover_one();
        willow_gc_collect();
        assert_eq!(unsafe { panic_info_message(info) }, "transferred");
        willow_push_root(&mut info);
        willow_panic_release_recovered(info);
        willow_gc_collect();
        assert_eq!(unsafe { panic_info_message(info) }, "transferred");
        willow_pop_roots(1);
    }

    #[test]
    fn panic_context_09_contexts_are_isolated_on_one_thread() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let first = Arc::new(PanicContext::new(91));
        let second = Arc::new(PanicContext::new(92));
        let previous = replace_current_context(Some(Arc::clone(&first)));
        raise("first", "", 0, 0);
        replace_current_context(Some(Arc::clone(&second)));
        assert_eq!(willow_panic_active(), 0);
        raise("second", "", 0, 0);
        let second_info = recover_one();
        replace_current_context(Some(Arc::clone(&first)));
        assert_eq!(willow_panic_active(), 1);
        let first_info = recover_one();
        willow_panic_release_recovered(first_info);
        replace_current_context(Some(second));
        willow_panic_release_recovered(second_info);
        replace_current_context(previous);
    }

    #[test]
    fn panic_context_10_owner_and_root_counts_are_observable() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(10);
        raise("a", "", 0, 0);
        raise("b", "", 0, 0);
        let snapshot = current_snapshot().unwrap();
        assert_eq!(snapshot.owner, 10);
        assert_eq!(snapshot.active, 2);
        let b = recover_one();
        assert_eq!(current_snapshot().unwrap().returned_roots, 1);
        willow_panic_release_recovered(b);
        let a = recover_one();
        willow_panic_release_recovered(a);
        assert_eq!(current_snapshot().unwrap().active, 0);
        assert_eq!(current_snapshot().unwrap().returned_roots, 0);
    }

    #[test]
    fn panic_context_11_null_inputs_receive_required_defaults() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(11);
        willow_panic_raise(std::ptr::null(), std::ptr::null(), -1, -1);
        let info = recover_one();
        assert_eq!(unsafe { panic_info_message(info) }, "explicit panic");
        assert_eq!(unsafe { panic_info_file(info) }, "");
        assert_eq!(unsafe { panic_info_line(info) }, 0);
        assert_eq!(unsafe { panic_info_column(info) }, 0);
        willow_panic_release_recovered(info);
    }

    static TASK_CONTEXT_OBSERVATION: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn context_observer_poll(_frame: *mut std::ffi::c_void) -> i32 {
        let before = willow_panic_active() as usize;
        willow_panic_raise(std::ptr::null(), std::ptr::null(), 0, 0);
        let after = willow_panic_active() as usize;
        TASK_CONTEXT_OBSERVATION.store(before | (after << 1), Ordering::Release);
        crate::task::RUNTIME_POLL_READY
    }

    #[test]
    fn panic_context_12_scheduler_installs_task_context_and_restores_sync_context() {
        let _heap = runtime_test_guard();
        let _single = crate::scheduler::single_worker_for_test();
        willow_gc_init();
        crate::scheduler::reset_global_scheduler_for_test();
        let _sync = ContextTestGuard::install(120);
        TASK_CONTEXT_OBSERVATION.store(usize::MAX, Ordering::Release);
        crate::scheduler::willow_sched_spawn(context_observer_poll, std::ptr::null_mut());
        crate::scheduler::willow_sched_run();
        assert_eq!(TASK_CONTEXT_OBSERVATION.load(Ordering::Acquire), 2);
        assert_eq!(willow_panic_active(), 0, "sync context must be restored");
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0, "task panic roots leaked");
    }

    static NESTED_CONTEXT_OBSERVATION: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn nested_inner_poll(_frame: *mut std::ffi::c_void) -> i32 {
        if willow_panic_active() == 0 {
            NESTED_CONTEXT_OBSERVATION.fetch_or(1, Ordering::AcqRel);
        }
        crate::task::RUNTIME_POLL_READY
    }

    unsafe extern "C" fn nested_outer_poll(_frame: *mut std::ffi::c_void) -> i32 {
        willow_panic_raise(std::ptr::null(), std::ptr::null(), 0, 0);
        let inner = crate::scheduler::willow_sched_spawn(nested_inner_poll, std::ptr::null_mut());
        crate::scheduler::willow_sched_run_until(inner);
        if willow_panic_active() != 0 {
            NESTED_CONTEXT_OBSERVATION.fetch_or(2, Ordering::AcqRel);
        }
        crate::task::RUNTIME_POLL_READY
    }

    #[test]
    fn panic_context_13_nested_scheduler_drive_isolates_and_restores_outer_task() {
        let _heap = runtime_test_guard();
        let _single = crate::scheduler::single_worker_for_test();
        willow_gc_init();
        crate::scheduler::reset_global_scheduler_for_test();
        let _sync = ContextTestGuard::install(130);
        NESTED_CONTEXT_OBSERVATION.store(0, Ordering::Release);
        let outer = crate::scheduler::willow_sched_spawn(nested_outer_poll, std::ptr::null_mut());
        crate::scheduler::willow_sched_run_until(outer);
        assert_eq!(NESTED_CONTEXT_OBSERVATION.load(Ordering::Acquire), 3);
        assert_eq!(willow_panic_active(), 0);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0, "nested task roots leaked");
    }

    #[test]
    fn panic_context_14_missing_execution_context_is_process_fatal() {
        const CHILD: &str = "WILLOW_TEST_FATAL_PANIC_CONTEXT";
        if std::env::var_os(CHILD).is_some() {
            // This is an ABI/integration violation, not a recoverable language
            // fault. It must abort before allocating a PanicInfo.
            replace_current_context(None);
            willow_panic_raise(std::ptr::null(), std::ptr::null(), 0, 0);
            unreachable!("fatal panic ABI returned");
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "panic_context::tests::panic_context_14_missing_execution_context_is_process_fatal",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("run fatal panic-context subprocess");
        assert!(
            !output.status.success(),
            "fatal invariant unexpectedly returned"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(
                "runtime fatal: language panic ABI called without an installed execution context"
            ),
            "missing fatal diagnostic: {stderr}"
        );
    }

    #[test]
    fn panic_context_15_scheduler_reentry_during_unwind_is_process_fatal() {
        const CHILD: &str = "WILLOW_TEST_FATAL_PANIC_REENTRY";
        if std::env::var_os(CHILD).is_some() {
            willow_gc_init();
            let _context = ContextTestGuard::install(15);
            raise("active", "", 0, 0);
            willow_panic_enter_defer();
            // The guard in sched_run_with_mutator must fire before a run loop
            // can claim or poll another task.
            crate::scheduler::willow_sched_run();
            unreachable!("scheduler re-entry returned");
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "panic_context::tests::panic_context_15_scheduler_reentry_during_unwind_is_process_fatal",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("run scheduler-reentry subprocess");
        assert!(
            !output.status.success(),
            "fatal re-entry unexpectedly returned"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr
                .contains("runtime fatal: scheduler re-entry attempted from panic-unwinding defer"),
            "missing fatal diagnostic: {stderr}"
        );
    }

    // ── Stage 6 acceptance: counters, stress, and worker isolation ──────────
    //
    // These prove cleanup with numbers instead of program output
    // (willow-s9ej.7). Every one of them measures runtime roots, context
    // counters, or per-task ownership rather than only asserting that a panic
    // message came back.

    fn baseline_snapshot_is_clean(label: &str) {
        let snapshot = current_snapshot().expect("an execution context must be installed");
        assert_eq!(snapshot.active, 0, "{label}: active panic records leaked");
        assert_eq!(
            snapshot.returned_roots, 0,
            "{label}: recovered roots leaked"
        );
        assert_eq!(
            snapshot.panic_defer_depth, 0,
            "{label}: panic-defer depth leaked"
        );
    }

    /// Perspective: a completed raise/recover/release cycle returns every
    /// runtime root it took, so repeated recoveries cannot grow the root table.
    #[test]
    fn panic_context_16_full_cycle_returns_every_runtime_root() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(16);
        let baseline = runtime_root_count();
        for _ in 0..64 {
            raise("cycle", "cycle.wi", 1, 2);
            assert_eq!(
                runtime_root_count(),
                baseline + 1,
                "an active PanicInfo must be runtime-rooted"
            );
            let info = recover_one();
            willow_panic_release_recovered(info);
            assert_eq!(runtime_root_count(), baseline, "recover leaked a root");
        }
        baseline_snapshot_is_clean("full cycle");
    }

    /// Perspective: a context that dies with unrecovered panics (task reaped
    /// after an unhandled panic was reported) still releases its roots.
    #[test]
    fn panic_context_17_dropped_context_releases_unrecovered_roots() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let baseline = runtime_root_count();
        {
            let _context = ContextTestGuard::install(17);
            raise("outer", "", 0, 0);
            raise("inner", "", 0, 0);
            let recovered = recover_one();
            assert_eq!(runtime_root_count(), baseline + 2);
            // One active record plus one recovered-but-unreleased transfer root:
            // dropping the context must clear both.
            assert_eq!(current_snapshot().unwrap().returned_roots, 1);
            assert!(!recovered.is_null());
        }
        assert_eq!(
            runtime_root_count(),
            baseline,
            "dropped context leaked panic roots"
        );
        willow_gc_collect();
    }

    /// Perspective: a nested panic stack keeps every payload alive across minor
    /// and major collections, and unwinds in LIFO order afterwards.
    #[test]
    fn panic_context_18_nested_stack_survives_minor_and_major_collections() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(18);
        let baseline = runtime_root_count();
        for depth in 0..8 {
            raise(&format!("level-{depth}"), "nested.wi", depth + 1, 3);
            willow_gc_minor_collect();
        }
        willow_gc_collect();
        assert_eq!(runtime_root_count(), baseline + 8);
        for depth in (0..8).rev() {
            let info = recover_one();
            assert_eq!(
                unsafe { panic_info_message(info) },
                format!("level-{depth}")
            );
            assert_eq!(unsafe { panic_info_line(info) }, depth + 1);
            willow_gc_collect();
            willow_panic_release_recovered(info);
        }
        assert_eq!(runtime_root_count(), baseline);
        baseline_snapshot_is_clean("nested stack");
    }

    /// Perspective: release only accepts a pointer this context actually handed
    /// out, so a stray release cannot silently drop someone else's root.
    #[test]
    fn panic_context_19_release_rejects_untransferred_pointers() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let context = Arc::new(PanicContext::new(19));
        let previous = replace_current_context(Some(Arc::clone(&context)));
        assert!(!context.release_recovered(std::ptr::null_mut()));
        raise("live", "", 0, 0);
        let info = recover_one();
        assert!(context.release_recovered(info));
        assert!(
            !context.release_recovered(info),
            "the same transfer must not be released twice"
        );
        replace_current_context(previous);
    }

    /// Perspective: leaving panic-defer scope closes the recover window again,
    /// so ordinary code after cleanup cannot consume a later panic.
    #[test]
    fn panic_context_20_recover_window_closes_with_defer_scope() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(20);
        willow_panic_enter_defer();
        willow_panic_leave_defer();
        raise("outside", "", 0, 0);
        assert!(
            willow_panic_recover().is_null(),
            "recover outside panic-defer scope must yield nothing"
        );
        assert_eq!(willow_panic_depth(), 1);
        let info = recover_one();
        willow_panic_release_recovered(info);
        baseline_snapshot_is_clean("recover window");
    }

    static POOL_CLEAN_POLLS: AtomicUsize = AtomicUsize::new(0);
    static POOL_DIRTY_POLLS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn pool_recover_poll(_frame: *mut std::ffi::c_void) -> i32 {
        let entry_clean = willow_panic_active() == 0
            && current_snapshot().is_some_and(|snapshot| {
                snapshot.returned_roots == 0 && snapshot.panic_defer_depth == 0
            });
        let owner_ok = current_snapshot().is_some_and(|snapshot| {
            snapshot.owner == crate::scheduler::willow_sched_current_task()
        });
        willow_panic_raise(std::ptr::null(), std::ptr::null(), 0, 0);
        crate::gc::willow_gc_minor_collect();
        let info = recover_one();
        let recovered = !info.is_null() && unsafe { panic_info_message(info) } == "explicit panic";
        if !info.is_null() {
            willow_panic_release_recovered(info);
        }
        let exit_clean = current_snapshot().is_some_and(|snapshot| {
            snapshot.active == 0 && snapshot.returned_roots == 0 && snapshot.panic_defer_depth == 0
        });
        if entry_clean && owner_ok && recovered && exit_clean {
            POOL_CLEAN_POLLS.fetch_add(1, Ordering::AcqRel);
        } else {
            POOL_DIRTY_POLLS.fetch_add(1, Ordering::AcqRel);
        }
        crate::task::RUNTIME_POLL_READY
    }

    /// Perspective: many tasks recovering on a real worker pool each see only
    /// their own panic state, and the panic context belongs to the task rather
    /// than to the worker thread that polled it.
    #[test]
    fn panic_context_21_pool_tasks_recover_independently_without_bleed() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        crate::scheduler::reset_global_scheduler_for_test();
        let _sync = ContextTestGuard::install(210);
        POOL_CLEAN_POLLS.store(0, Ordering::Release);
        POOL_DIRTY_POLLS.store(0, Ordering::Release);
        let baseline = runtime_root_count();

        // The synchronous entry context holds a live panic across the whole
        // drive: no worker task may observe, consume, or disturb it.
        raise("sync-owned", "sync.wi", 5, 6);

        const TASKS: usize = 48;
        for _ in 0..TASKS {
            crate::scheduler::willow_sched_spawn(pool_recover_poll, std::ptr::null_mut());
        }
        crate::scheduler::willow_sched_run();

        assert_eq!(POOL_CLEAN_POLLS.load(Ordering::Acquire), TASKS);
        assert_eq!(POOL_DIRTY_POLLS.load(Ordering::Acquire), 0);
        assert_eq!(willow_panic_active(), 1, "sync context lost its panic");
        assert_eq!(
            runtime_root_count(),
            baseline + 1,
            "task panic roots outlived their tasks"
        );
        let info = recover_one();
        assert_eq!(unsafe { panic_info_message(info) }, "sync-owned");
        assert_eq!(unsafe { panic_info_line(info) }, 5);
        willow_panic_release_recovered(info);
        assert_eq!(runtime_root_count(), baseline);
        baseline_snapshot_is_clean("pool drive");
    }

    static REUSE_OBSERVATIONS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn reuse_probe_poll(_frame: *mut std::ffi::c_void) -> i32 {
        if willow_panic_active() == 0
            && current_snapshot().is_some_and(|snapshot| {
                snapshot.returned_roots == 0 && snapshot.panic_defer_depth == 0
            })
        {
            REUSE_OBSERVATIONS.fetch_add(1, Ordering::AcqRel);
        }
        willow_panic_raise(std::ptr::null(), std::ptr::null(), 0, 0);
        let info = recover_one();
        willow_panic_release_recovered(info);
        crate::task::RUNTIME_POLL_READY
    }

    /// Perspective: reusing one worker for a sequence of recovering tasks never
    /// carries panic state, transfer roots, or defer depth into the next task.
    #[test]
    fn panic_context_22_single_worker_reuse_starts_every_task_clean() {
        let _heap = runtime_test_guard();
        let _single = crate::scheduler::single_worker_for_test();
        willow_gc_init();
        crate::scheduler::reset_global_scheduler_for_test();
        let _sync = ContextTestGuard::install(220);
        REUSE_OBSERVATIONS.store(0, Ordering::Release);
        let baseline = runtime_root_count();

        for _ in 0..16 {
            let id = crate::scheduler::willow_sched_spawn(reuse_probe_poll, std::ptr::null_mut());
            crate::scheduler::willow_sched_run_until(id);
        }

        assert_eq!(REUSE_OBSERVATIONS.load(Ordering::Acquire), 16);
        assert_eq!(willow_panic_active(), 0, "sync context must stay clean");
        assert_eq!(runtime_root_count(), baseline, "worker reuse leaked roots");
        baseline_snapshot_is_clean("worker reuse");
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0, "recovered payloads leaked");
    }

    static STRESS_FAILURES: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn gc_stress_recover_poll(_frame: *mut std::ffi::c_void) -> i32 {
        let mut message = ws("stressed");
        willow_push_root(&mut message);
        let file = ws("stress.wi");
        willow_panic_raise(message, file, 41, 42);
        willow_pop_roots(1);
        willow_gc_minor_collect();
        willow_gc_collect();
        let info = recover_one();
        willow_gc_collect();
        let intact = !info.is_null()
            && unsafe { panic_info_message(info) } == "stressed"
            && unsafe { panic_info_file(info) } == "stress.wi"
            && unsafe { panic_info_line(info) } == 41
            && unsafe { panic_info_column(info) } == 42;
        if !intact {
            STRESS_FAILURES.fetch_add(1, Ordering::AcqRel);
        }
        if !info.is_null() {
            willow_panic_release_recovered(info);
        }
        crate::task::RUNTIME_POLL_READY
    }

    /// Perspective: a task-owned payload keeps every field readable when full
    /// collections run between the raise and the recover.
    #[test]
    fn panic_context_23_task_payload_survives_collections_between_raise_and_recover() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        crate::scheduler::reset_global_scheduler_for_test();
        let _sync = ContextTestGuard::install(230);
        STRESS_FAILURES.store(0, Ordering::Release);
        let baseline = runtime_root_count();

        for _ in 0..24 {
            crate::scheduler::willow_sched_spawn(gc_stress_recover_poll, std::ptr::null_mut());
        }
        crate::scheduler::willow_sched_run();

        assert_eq!(STRESS_FAILURES.load(Ordering::Acquire), 0);
        assert_eq!(runtime_root_count(), baseline);
        baseline_snapshot_is_clean("gc stress drive");
    }

    /// Perspective: the guard that keeps compiler-driven cleanup from
    /// re-entering the scheduler follows the cleanup entry, not the panic
    /// record. It must stay set for the whole deferred body — including after
    /// `recover()` consumed the panic — and for cancellation cleanup, which
    /// runs `enter_defer` with no panic record at all.
    #[test]
    fn panic_context_24_unwind_cleanup_flag_tracks_the_cleanup_entry() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(24);
        assert!(!panic_unwind_cleanup_active());

        // Cancellation cleanup: an entry with no panic record still forbids a
        // nested scheduler drive.
        willow_panic_enter_defer();
        assert!(
            panic_unwind_cleanup_active(),
            "cancellation cleanup is compiler-driven cleanup"
        );
        willow_panic_leave_defer();
        assert!(!panic_unwind_cleanup_active());

        raise("unwinding", "", 0, 0);
        assert!(
            !panic_unwind_cleanup_active(),
            "an active panic outside cleanup code is not cleanup"
        );
        willow_panic_enter_defer();
        assert!(panic_unwind_cleanup_active());
        let info = willow_panic_recover();
        assert!(
            panic_unwind_cleanup_active(),
            "the deferred body still runs after recover() consumed the panic"
        );
        willow_panic_leave_defer();
        assert!(
            !panic_unwind_cleanup_active(),
            "leaving the cleanup entry ends the guard"
        );
        willow_panic_release_recovered(info);
        baseline_snapshot_is_clean("unwind flag");
    }

    /// Perspective: contexts are addressed by owner, so a migrated task that is
    /// re-installed on another worker resumes with its own records and depth.
    #[test]
    fn panic_context_25_migrated_context_resumes_with_its_own_records() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let baseline = runtime_root_count();
        let migrating = Arc::new(PanicContext::new(25));
        let other = Arc::new(PanicContext::new(26));
        let previous = replace_current_context(Some(Arc::clone(&migrating)));
        raise("migrated", "migrate.wi", 3, 4);
        willow_panic_enter_defer();

        // Another worker installs an unrelated context in between.
        replace_current_context(Some(Arc::clone(&other)));
        assert_eq!(willow_panic_active(), 0);
        assert_eq!(current_snapshot().unwrap().panic_defer_depth, 0);
        raise("other", "", 0, 0);
        let other_info = recover_one();

        let handle = std::thread::spawn({
            let migrating = Arc::clone(&migrating);
            move || {
                // A different OS thread can adopt the same context; TLS only
                // caches it.
                replace_current_context(Some(migrating));
                let snapshot = current_snapshot().unwrap();
                let info = willow_panic_recover();
                let message = unsafe { panic_info_message(info) }.to_string();
                willow_panic_leave_defer();
                willow_panic_release_recovered(info);
                replace_current_context(None);
                (snapshot, message)
            }
        });
        let (snapshot, message) = handle.join().expect("migrated worker thread panicked");
        assert_eq!(snapshot.owner, 25);
        assert_eq!(snapshot.active, 1);
        assert_eq!(snapshot.panic_defer_depth, 1);
        assert_eq!(message, "migrated");

        replace_current_context(Some(Arc::clone(&other)));
        willow_panic_release_recovered(other_info);
        replace_current_context(previous);
        drop(other);
        drop(migrating);
        assert_eq!(runtime_root_count(), baseline, "migration leaked roots");
    }

    // ── Review follow-up: runtime faults carry a source location ────────────
    //
    // Array bounds, blocked channel operations and cancelled awaits are raised
    // from Rust, which has no Willow span. Debug builds publish the enclosing
    // statement first, so the recovered `PanicInfo` is not `:0:0`
    // (willow-s9ej.7 review finding 6).

    /// Publish a fault site the way generated debug code does.
    fn set_fault_site(file: &str, line: i64, column: i64) {
        willow_fault_site_set(file.as_ptr(), file.len() as i64, line, column);
    }

    /// Perspective: a published fault site becomes the location of the next
    /// runtime-raised fault, and the file lands on the GC heap as a Willow
    /// `String` the recovered `PanicInfo` can read.
    #[test]
    fn panic_context_26_runtime_fault_adopts_the_published_site() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(26);
        willow_fault_site_clear();

        set_fault_site("bounds.wi", 12, 5);
        raise_language_message("index out of bounds");
        let info = recover_one();
        assert!(!info.is_null());
        unsafe {
            assert_eq!(panic_info_message(info), "index out of bounds");
            assert_eq!(panic_info_file(info), "bounds.wi");
            assert_eq!(panic_info_line(info), 12);
            assert_eq!(panic_info_column(info), 5);
        }
        willow_panic_release_recovered(info);
        willow_fault_site_clear();
        baseline_snapshot_is_clean("fault site adopted");
    }

    /// Perspective: with no site published — a release build, which never emits
    /// the marker — the fault still raises and still carries its message; only
    /// the location degrades to empty.
    #[test]
    fn panic_context_27_runtime_fault_without_a_site_still_raises() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(27);
        willow_fault_site_clear();

        raise_language_message("channel closed");
        let info = recover_one();
        unsafe {
            assert_eq!(panic_info_message(info), "channel closed");
            assert_eq!(panic_info_file(info), "");
            assert_eq!(panic_info_line(info), 0);
            assert_eq!(panic_info_column(info), 0);
        }
        willow_panic_release_recovered(info);
        baseline_snapshot_is_clean("no fault site");
    }

    /// Perspective: the site is last-write-wins, so a fault reports the
    /// statement that raised it, never an earlier one that already returned.
    #[test]
    fn panic_context_28_latest_fault_site_wins() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(28);
        willow_fault_site_clear();

        set_fault_site("first.wi", 1, 1);
        set_fault_site("second.wi", 40, 9);
        raise_language_message("boom");
        let info = recover_one();
        unsafe {
            assert_eq!(panic_info_file(info), "second.wi");
            assert_eq!(panic_info_line(info), 40);
            assert_eq!(panic_info_column(info), 9);
        }
        willow_panic_release_recovered(info);
        willow_fault_site_clear();
        baseline_snapshot_is_clean("latest site");
    }

    /// Perspective: `willow_fault_site_clear` really drops the site, so a
    /// context that leaves generated code cannot bequeath its location to an
    /// unrelated later fault.
    #[test]
    fn panic_context_29_cleared_fault_site_is_not_inherited() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(29);

        set_fault_site("stale.wi", 7, 3);
        willow_fault_site_clear();
        raise_language_message("later fault");
        let info = recover_one();
        unsafe {
            assert_eq!(panic_info_file(info), "", "stale location inherited");
            assert_eq!(panic_info_line(info), 0);
        }
        willow_panic_release_recovered(info);
        baseline_snapshot_is_clean("cleared site");
    }

    /// Perspective: the site is thread-local. Workers run different statements
    /// at the same time, so one worker's location must never describe another
    /// worker's fault.
    #[test]
    fn panic_context_30_fault_site_is_per_thread() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(30);
        willow_fault_site_clear();
        set_fault_site("main-thread.wi", 11, 2);

        let worker = std::thread::spawn(|| {
            let context = Arc::new(PanicContext::new(31));
            replace_current_context(Some(context));
            // This thread never published a site of its own.
            raise_language_message("worker fault");
            let info = recover_one();
            let file = unsafe { panic_info_file(info) }.to_string();
            let line = unsafe { panic_info_line(info) };
            willow_panic_release_recovered(info);
            replace_current_context(None);
            (file, line)
        });
        let (file, line) = worker.join().expect("fault-site worker panicked");
        assert_eq!(file, "", "fault site bled across threads");
        assert_eq!(line, 0);

        // The publishing thread still has its own site.
        raise_language_message("main fault");
        let info = recover_one();
        unsafe {
            assert_eq!(panic_info_file(info), "main-thread.wi");
            assert_eq!(panic_info_line(info), 11);
        }
        willow_panic_release_recovered(info);
        willow_fault_site_clear();
        baseline_snapshot_is_clean("per-thread site");
    }

    /// Perspective: malformed marker arguments cannot corrupt the site. A null
    /// or empty file, and negative line/column, degrade to "no location"
    /// instead of reading unmapped memory or storing nonsense.
    #[test]
    fn panic_context_31_fault_site_rejects_malformed_arguments() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(32);

        willow_fault_site_set(std::ptr::null(), 12, 3, 4);
        assert_eq!(current_fault_site(), (String::new(), 3, 4));

        set_fault_site("", 5, 6);
        assert_eq!(current_fault_site(), (String::new(), 5, 6));

        set_fault_site("neg.wi", -9, -1);
        assert_eq!(current_fault_site(), ("neg.wi".to_string(), 0, 0));

        // An empty file with a line still raises without allocating a file
        // String, and the panic remains recoverable.
        willow_fault_site_set(std::ptr::null(), 0, 8, 2);
        raise_language_message("degraded");
        let info = recover_one();
        unsafe {
            assert_eq!(panic_info_message(info), "degraded");
            assert_eq!(panic_info_file(info), "");
            assert_eq!(panic_info_line(info), 8);
            assert_eq!(panic_info_column(info), 2);
        }
        willow_panic_release_recovered(info);
        willow_fault_site_clear();
        baseline_snapshot_is_clean("malformed site");
    }

    /// Perspective: repeated runtime faults with a site return every root they
    /// take, including the file `String` the site allocates on the GC heap.
    #[test]
    fn panic_context_32_repeated_runtime_faults_do_not_leak_roots() {
        let _heap = runtime_test_guard();
        willow_gc_init();
        let _context = ContextTestGuard::install(33);
        let baseline = runtime_root_count();
        set_fault_site("loop.wi", 3, 1);

        for _ in 0..64 {
            raise_language_message("repeat");
            let info = recover_one();
            unsafe {
                assert_eq!(panic_info_file(info), "loop.wi");
            }
            willow_panic_release_recovered(info);
        }
        willow_fault_site_clear();
        assert_eq!(
            runtime_root_count(),
            baseline,
            "runtime fault sites leaked roots"
        );
        baseline_snapshot_is_clean("repeated runtime faults");
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0, "fault-site strings leaked");
    }
}
