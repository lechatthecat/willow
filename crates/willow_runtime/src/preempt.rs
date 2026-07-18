//! Preemption primitives (willow-0a6k.1, preemption spec §7-9, §22-23).
//!
//! Stage 1 of the preemptible-task roadmap: the *runtime-side* protocol and
//! quantum machinery that later stages build on. It provides
//!
//!   * the extended poll-result protocol (`RUNTIME_POLL_YIELD` / `PREEMPTED` /
//!     `PANICKED`, defined in [`crate::task`]),
//!   * a per-task preemption flag (an `AtomicBool` with acquire/release
//!     ordering, §23),
//!   * a hybrid scheduling quantum — a safepoint counter for deterministic
//!     tests plus a wall-clock time quantum for production fairness (§8),
//!   * env configuration via `WILLOW_TASK_BUDGET` / `WILLOW_TIME_QUANTUM_MS`,
//!   * `willow_preempt_check`, the safepoint hook compiler-inserted safepoints
//!     (willow-0a6k.2) will call, plus no-preempt-region guards (§22).
//!
//! The scheduler binds a task's flag and starts a fresh quantum around every
//! poll. Compiler-generated async statement boundaries and loop backedges call
//! `willow_preempt_check` and return `PREEMPTED` with a saved resume state when
//! it trips (willow-0a6k.2). Additional task-aware safepoints inside synchronous
//! helpers and generated runtime loops remain staged work.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Default safepoint budget per quantum (spec §8 example).
pub const DEFAULT_TASK_BUDGET: u64 = 1024;
/// Default wall-clock time quantum in milliseconds (spec §8 example, upper end
/// of the 1ms–10ms range for production fairness).
pub const DEFAULT_TIME_QUANTUM_MS: u64 = 10;

/// Resolved preemption tuning for a run (spec §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreemptConfig {
    task_budget: u64,
    time_quantum_ms: u64,
}

impl PreemptConfig {
    /// Parse a config from raw env strings. Invalid, zero, or absent values fall
    /// back to the defaults so a long-running task can never be handed a
    /// degenerate zero-length quantum.
    pub fn from_env_values(budget: Option<&str>, quantum_ms: Option<&str>) -> Self {
        let task_budget = budget
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_TASK_BUDGET);
        let time_quantum_ms = quantum_ms
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_TIME_QUANTUM_MS);
        Self {
            task_budget,
            time_quantum_ms,
        }
    }

    pub fn task_budget(self) -> u64 {
        self.task_budget
    }

    pub fn time_quantum_ms(self) -> u64 {
        self.time_quantum_ms
    }
}

/// Read the active preemption config from the environment. Read per quantum
/// (like [`crate::scheduler::runtime_worker_config`]) so tests and tuning can
/// override it without a process restart.
pub fn runtime_preempt_config() -> PreemptConfig {
    PreemptConfig::from_env_values(
        std::env::var("WILLOW_TASK_BUDGET").ok().as_deref(),
        std::env::var("WILLOW_TIME_QUANTUM_MS").ok().as_deref(),
    )
}

/// Per-worker-thread quantum state. A worker runs at most one task at a time, so
/// the "currently running task" quantum is naturally thread-local. The bound
/// `flag` points at the running task's [`AtomicBool`]; `null` means no per-task
/// flag is bound (budget/time still apply).
struct QuantumState {
    budget: Cell<u64>,
    deadline: Cell<Option<Instant>>,
    flag: Cell<*const AtomicBool>,
    /// Re-entrant no-preempt nesting depth (§22). While > 0, `willow_preempt_check`
    /// always reports "do not preempt" even if the flag/budget/time say otherwise.
    no_preempt_depth: Cell<u32>,
}

thread_local! {
    static QUANTUM: QuantumState = const {
        QuantumState {
            budget: Cell::new(0),
            deadline: Cell::new(None),
            flag: Cell::new(std::ptr::null()),
            no_preempt_depth: Cell::new(0),
        }
    };
}

#[inline]
fn flag_ref<'a>(flag: *const c_void) -> Option<&'a AtomicBool> {
    if flag.is_null() {
        None
    } else {
        // Safety: callers pass a pointer obtained from `willow_preempt_flag_new`
        // (a `Box<AtomicBool>`), still live for the duration of the call.
        Some(unsafe { &*(flag as *const AtomicBool) })
    }
}

// ── Config introspection ────────────────────────────────────────────────────

/// Safepoint budget per quantum from `WILLOW_TASK_BUDGET` (or the default).
#[unsafe(no_mangle)]
pub extern "C" fn willow_preempt_task_budget() -> u64 {
    runtime_preempt_config().task_budget()
}

/// Time quantum in milliseconds from `WILLOW_TIME_QUANTUM_MS` (or the default).
#[unsafe(no_mangle)]
pub extern "C" fn willow_preempt_time_quantum_ms() -> u64 {
    runtime_preempt_config().time_quantum_ms()
}

// ── Per-task preemption flag lifecycle ──────────────────────────────────────

/// Allocate a fresh per-task preemption flag (initially clear). Owned by the
/// caller; release with [`willow_preempt_flag_free`].
#[unsafe(no_mangle)]
pub extern "C" fn willow_preempt_flag_new() -> *mut c_void {
    Box::into_raw(Box::new(AtomicBool::new(false))) as *mut c_void
}

/// Free a flag from [`willow_preempt_flag_new`]. `null` is ignored.
#[unsafe(no_mangle)]
pub extern "C" fn willow_preempt_flag_free(flag: *mut c_void) {
    if !flag.is_null() {
        // Safety: `flag` came from `willow_preempt_flag_new` and is freed once.
        drop(unsafe { Box::from_raw(flag as *mut AtomicBool) });
    }
}

/// Request preemption of the task owning `flag` (spec §9): set the flag with
/// release ordering so a safepoint observing it (acquire) sees a happens-before
/// edge. Safe to call from any thread (scheduler, GC, cancellation).
#[unsafe(no_mangle)]
pub extern "C" fn willow_preempt_request(flag: *const c_void) {
    if let Some(flag) = flag_ref(flag) {
        flag.store(true, Ordering::Release);
    }
}

/// Clear/acknowledge a preemption request on `flag` (spec §9 step 1).
#[unsafe(no_mangle)]
pub extern "C" fn willow_preempt_clear(flag: *const c_void) {
    if let Some(flag) = flag_ref(flag) {
        flag.store(false, Ordering::Release);
    }
}

/// Query whether `flag` currently has a pending preemption request (acquire).
#[unsafe(no_mangle)]
pub extern "C" fn willow_preempt_requested(flag: *const c_void) -> i32 {
    match flag_ref(flag) {
        Some(flag) if flag.load(Ordering::Acquire) => 1,
        _ => 0,
    }
}

// ── Quantum lifecycle (called by the scheduler around each poll) ─────────────

/// Begin a fresh scheduling quantum for the task identified by `flag` before its
/// poll runs: reset the safepoint budget and time deadline, bind the flag, and
/// clear any stale request (the previous preemption was already honored by the
/// requeue). `flag` may be `null` for budget/time-only quanta.
#[unsafe(no_mangle)]
pub extern "C" fn willow_preempt_begin(flag: *const c_void) {
    begin_quantum(runtime_preempt_config(), flag);
}

/// Core of [`willow_preempt_begin`] with an explicit config — env-free so tests
/// exercise the quantum machinery deterministically without mutating the
/// process-global environment (cargo runs unit tests in parallel threads).
pub(crate) fn begin_quantum(config: PreemptConfig, flag: *const c_void) {
    let deadline = Instant::now().checked_add(Duration::from_millis(config.time_quantum_ms()));
    QUANTUM.with(|q| {
        q.flag.set(flag as *const AtomicBool);
        q.budget.set(config.task_budget());
        q.deadline.set(deadline);
        q.no_preempt_depth.set(0);
    });
    if let Some(flag) = flag_ref(flag) {
        flag.store(false, Ordering::Release);
    }
}

/// End the current quantum after a poll returns: unbind the flag and zero the
/// budget so a stray safepoint outside a poll never reports "preempt".
#[unsafe(no_mangle)]
pub extern "C" fn willow_preempt_end() {
    QUANTUM.with(|q| {
        q.flag.set(std::ptr::null());
        q.budget.set(0);
        q.deadline.set(None);
        q.no_preempt_depth.set(0);
    });
}

/// Enter a no-preempt region (§22): nestable; while open, `willow_preempt_check`
/// reports no-preempt regardless of flag/budget/time.
#[unsafe(no_mangle)]
pub extern "C" fn willow_preempt_enter_no_preempt() {
    QUANTUM.with(|q| {
        q.no_preempt_depth
            .set(q.no_preempt_depth.get().saturating_add(1))
    });
}

/// Leave a no-preempt region (§22).
#[unsafe(no_mangle)]
pub extern "C" fn willow_preempt_leave_no_preempt() {
    QUANTUM.with(|q| {
        q.no_preempt_depth
            .set(q.no_preempt_depth.get().saturating_sub(1))
    });
}

/// RAII guard for runtime invariants that must not be observed halfway through
/// an update (root-stack edits, scheduler queue/task-id publication, waiter
/// registration, partially initialized frames).
pub(crate) struct NoPreemptGuard;

impl NoPreemptGuard {
    pub(crate) fn enter() -> Self {
        willow_preempt_enter_no_preempt();
        Self
    }
}

impl Drop for NoPreemptGuard {
    fn drop(&mut self) {
        willow_preempt_leave_no_preempt();
    }
}

// ── The safepoint hook ───────────────────────────────────────────────────────

/// Safepoint check (spec §6.3). Returns `1` when the running task should stop at
/// this safepoint and yield to the scheduler, `0` to keep running.
///
/// Preempt when, in order: (1) the bound flag has a pending request (acquire);
/// (2) the safepoint budget is exhausted — the deterministic path tests rely on;
/// (3) the wall-clock time quantum elapsed — production fairness. A no-preempt
/// region (§22) overrides all three. When budget/time trip, the bound flag is
/// also set so the scheduler/diagnostics can see the task was preempted.
#[unsafe(no_mangle)]
pub extern "C" fn willow_preempt_check() -> i32 {
    QUANTUM.with(|q| {
        if q.no_preempt_depth.get() > 0 {
            return 0;
        }
        let flag = flag_ref(q.flag.get() as *const c_void);

        // (1) Explicit request already pending.
        if let Some(flag) = flag
            && flag.load(Ordering::Acquire)
        {
            return 1;
        }

        // (2) Safepoint budget. `budget == 0` means no active quantum
        // (begin not called / already ended) → never preempt.
        let budget = q.budget.get();
        if budget == 0 {
            return 0;
        }
        let remaining = budget - 1;
        q.budget.set(remaining);
        if remaining == 0 {
            if let Some(flag) = flag {
                flag.store(true, Ordering::Release);
            }
            return 1;
        }

        // (3) Wall-clock time quantum.
        if let Some(deadline) = q.deadline.get()
            && Instant::now() >= deadline
        {
            if let Some(flag) = flag {
                flag.store(true, Ordering::Release);
            }
            return 1;
        }

        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a config without touching the process environment (cargo runs unit
    // tests in parallel threads; mutating env would race across tests). The
    // quantum machinery is driven via `begin_quantum`, and env *parsing* is
    // tested separately through the pure `PreemptConfig::from_env_values`.
    fn cfg(budget: u64, quantum_ms: u64) -> PreemptConfig {
        PreemptConfig::from_env_values(Some(&budget.to_string()), Some(&quantum_ms.to_string()))
    }

    // 1. Defaults when env is absent.
    #[test]
    fn config_defaults_when_absent() {
        let c = PreemptConfig::from_env_values(None, None);
        assert_eq!(c.task_budget(), DEFAULT_TASK_BUDGET);
        assert_eq!(c.time_quantum_ms(), DEFAULT_TIME_QUANTUM_MS);
    }

    // 2. Valid env values parsed.
    #[test]
    fn config_parses_valid_values() {
        let c = PreemptConfig::from_env_values(Some("32"), Some("5"));
        assert_eq!(c.task_budget(), 32);
        assert_eq!(c.time_quantum_ms(), 5);
    }

    // 3. Whitespace tolerated.
    #[test]
    fn config_trims_whitespace() {
        let c = PreemptConfig::from_env_values(Some("  64 "), Some(" 7 "));
        assert_eq!(c.task_budget(), 64);
        assert_eq!(c.time_quantum_ms(), 7);
    }

    // 4. Zero rejected → default (no degenerate zero-length quantum).
    #[test]
    fn config_rejects_zero() {
        let c = PreemptConfig::from_env_values(Some("0"), Some("0"));
        assert_eq!(c.task_budget(), DEFAULT_TASK_BUDGET);
        assert_eq!(c.time_quantum_ms(), DEFAULT_TIME_QUANTUM_MS);
    }

    // 5. Garbage / negative rejected → default.
    #[test]
    fn config_rejects_garbage() {
        let c = PreemptConfig::from_env_values(Some("abc"), Some("-3"));
        assert_eq!(c.task_budget(), DEFAULT_TASK_BUDGET);
        assert_eq!(c.time_quantum_ms(), DEFAULT_TIME_QUANTUM_MS);
    }

    // 6. Flag new/free roundtrip + initial state clear.
    #[test]
    fn flag_starts_clear_and_frees() {
        let flag = willow_preempt_flag_new();
        assert_eq!(willow_preempt_requested(flag), 0);
        willow_preempt_flag_free(flag);
    }

    // 7. request sets, clear resets (acquire/release visible on same thread).
    #[test]
    fn request_then_clear() {
        let flag = willow_preempt_flag_new();
        willow_preempt_request(flag);
        assert_eq!(willow_preempt_requested(flag), 1);
        willow_preempt_clear(flag);
        assert_eq!(willow_preempt_requested(flag), 0);
        willow_preempt_flag_free(flag);
    }

    // 8. Null flag is inert across all flag ops.
    #[test]
    fn null_flag_is_inert() {
        let null = std::ptr::null();
        assert_eq!(willow_preempt_requested(null), 0);
        willow_preempt_request(null); // no panic
        willow_preempt_clear(null); // no panic
        assert_eq!(willow_preempt_requested(null), 0);
        willow_preempt_flag_free(std::ptr::null_mut()); // no panic
    }

    // 9. No active quantum (no begin) → check never preempts.
    #[test]
    fn check_without_quantum_never_preempts() {
        willow_preempt_end();
        for _ in 0..10 {
            assert_eq!(willow_preempt_check(), 0);
        }
    }

    // 10. Budget exhaustion forces preemption after exactly `budget` checks.
    #[test]
    fn budget_exhaustion_preempts() {
        let flag = willow_preempt_flag_new();
        begin_quantum(cfg(4, 60_000), flag);
        // budget=4 → first 3 checks pass, 4th trips.
        assert_eq!(willow_preempt_check(), 0);
        assert_eq!(willow_preempt_check(), 0);
        assert_eq!(willow_preempt_check(), 0);
        assert_eq!(willow_preempt_check(), 1);
        // Flag was set by the budget trip.
        assert_eq!(willow_preempt_requested(flag), 1);
        willow_preempt_end();
        willow_preempt_flag_free(flag);
    }

    // 11. begin clears a stale request from a prior quantum.
    #[test]
    fn begin_clears_stale_request() {
        let flag = willow_preempt_flag_new();
        willow_preempt_request(flag);
        begin_quantum(cfg(1000, 60_000), flag);
        assert_eq!(willow_preempt_requested(flag), 0);
        assert_eq!(willow_preempt_check(), 0);
        willow_preempt_end();
        willow_preempt_flag_free(flag);
    }

    // 12. An explicit request mid-quantum trips the very next check.
    #[test]
    fn explicit_request_preempts_immediately() {
        let flag = willow_preempt_flag_new();
        begin_quantum(cfg(1000, 60_000), flag);
        assert_eq!(willow_preempt_check(), 0);
        willow_preempt_request(flag);
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
        willow_preempt_flag_free(flag);
    }

    // 13. Time quantum trips even with budget remaining.
    #[test]
    fn time_quantum_preempts() {
        let flag = willow_preempt_flag_new();
        begin_quantum(cfg(1_000_000, 1), flag);
        // Spin past the 1ms deadline, then a check must trip on time.
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(3) {
            std::hint::spin_loop();
        }
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
        willow_preempt_flag_free(flag);
    }

    // 14. No-preempt region overrides an explicit request.
    #[test]
    fn no_preempt_region_overrides_request() {
        let flag = willow_preempt_flag_new();
        begin_quantum(cfg(1000, 60_000), flag);
        willow_preempt_request(flag);
        willow_preempt_enter_no_preempt();
        assert_eq!(willow_preempt_check(), 0);
        willow_preempt_leave_no_preempt();
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
        willow_preempt_flag_free(flag);
    }

    // 15. No-preempt nesting requires matching leaves.
    #[test]
    fn no_preempt_region_nests() {
        let flag = willow_preempt_flag_new();
        begin_quantum(cfg(1000, 60_000), flag);
        willow_preempt_request(flag);
        willow_preempt_enter_no_preempt();
        willow_preempt_enter_no_preempt();
        willow_preempt_leave_no_preempt();
        assert_eq!(willow_preempt_check(), 0); // still one region open
        willow_preempt_leave_no_preempt();
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
        willow_preempt_flag_free(flag);
    }

    // 16. end() unbinds: a later check (no new begin) never preempts.
    #[test]
    fn end_unbinds_quantum() {
        let flag = willow_preempt_flag_new();
        begin_quantum(cfg(2, 60_000), flag);
        willow_preempt_end();
        for _ in 0..10 {
            assert_eq!(willow_preempt_check(), 0);
        }
        willow_preempt_flag_free(flag);
    }

    // 17. begin with a null flag still enforces budget (no per-task flag).
    #[test]
    fn null_flag_quantum_still_counts_budget() {
        begin_quantum(cfg(2, 60_000), std::ptr::null());
        assert_eq!(willow_preempt_check(), 0);
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
    }

    // 18. budget=1 trips on the first check.
    #[test]
    fn budget_one_trips_immediately() {
        begin_quantum(cfg(1, 60_000), std::ptr::null());
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
    }

    // 19. A new begin re-arms the budget after a prior exhaustion.
    #[test]
    fn rearm_after_exhaustion() {
        let flag = willow_preempt_flag_new();
        begin_quantum(cfg(2, 60_000), flag);
        assert_eq!(willow_preempt_check(), 0);
        assert_eq!(willow_preempt_check(), 1);
        // Re-arm for the next quantum.
        begin_quantum(cfg(2, 60_000), flag);
        assert_eq!(willow_preempt_requested(flag), 0);
        assert_eq!(willow_preempt_check(), 0);
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
        willow_preempt_flag_free(flag);
    }

    // 20. ABI config getters return defaults when env is unset (no env mutation,
    // so this stays deterministic under parallel test execution).
    #[test]
    fn abi_config_getters_default() {
        // These read the live env; no test sets WILLOW_TASK_BUDGET /
        // WILLOW_TIME_QUANTUM_MS, so defaults must hold. Env *override* is
        // covered by the pure-parse tests (#2/#3).
        assert_eq!(willow_preempt_task_budget(), DEFAULT_TASK_BUDGET);
        assert_eq!(willow_preempt_time_quantum_ms(), DEFAULT_TIME_QUANTUM_MS);
    }

    // 21. requested() reads back an externally-set flag (acquire) without check;
    // request is idempotent.
    #[test]
    fn requested_reads_external_set() {
        let flag = willow_preempt_flag_new();
        assert_eq!(willow_preempt_requested(flag), 0);
        willow_preempt_request(flag);
        willow_preempt_request(flag); // idempotent
        assert_eq!(willow_preempt_requested(flag), 1);
        willow_preempt_flag_free(flag);
    }

    // 22. Cross-thread request is observed via acquire/release (§23).
    #[test]
    fn cross_thread_request_visible() {
        let flag = willow_preempt_flag_new();
        let addr = flag as usize;
        std::thread::spawn(move || {
            willow_preempt_request(addr as *const c_void);
        })
        .join()
        .unwrap();
        assert_eq!(willow_preempt_requested(flag), 1);
        willow_preempt_flag_free(flag);
    }
}
