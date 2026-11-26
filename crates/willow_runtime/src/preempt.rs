//! Preemption primitives (preemption primitives, preemption spec §7-9, §22-23).
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
//!     will call, plus no-preempt-region guards (§22).
//!
//! The scheduler binds a task's flag and starts a fresh quantum around every
//! poll. Compiler-generated async statement boundaries and loop backedges call
//! `willow_preempt_check` and return `PREEMPTED` with a saved resume state when
//! it trips. Additional task-aware safepoints inside synchronous
//! helpers and generated runtime loops remain staged work.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Production fairness is time-based. Explicit WILLOW_TASK_BUDGET values
/// retain exact deterministic safepoint budgets for tests and tuning.
/// A small default turns fast arithmetic into repeated scheduler round trips.
pub const DEFAULT_TASK_BUDGET: u64 = u64::MAX;
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

/// The preemption config from the environment, read once per process.
///
/// The scheduler starts a quantum around every task poll, so this is on the
/// per-poll path. `std::env::var` takes std's process-global environment
/// `RwLock` and scans `environ`; with every worker doing that per poll, the
/// lock's reader count bounces between cores and cost ~10% of `yield_switch`. Tuning goes
/// through the environment of a fresh process, which
/// is how every test that sets these variables runs; tests that need a specific
/// config in-process use [`begin_quantum`] instead.
pub fn runtime_preempt_config() -> PreemptConfig {
    static CONFIG: LazyLock<PreemptConfig> = LazyLock::new(|| {
        PreemptConfig::from_env_values(
            std::env::var("WILLOW_TASK_BUDGET").ok().as_deref(),
            std::env::var("WILLOW_TIME_QUANTUM_MS").ok().as_deref(),
        )
    });
    *CONFIG
}

/// Per-worker-thread quantum state. A worker runs at most one task at a time, so
/// the "currently running task" quantum is naturally thread-local. The bound
/// `flag` points at the running task's [`AtomicBool`]; `null` means no per-task
/// flag is bound (budget/time still apply).
struct QuantumState {
    budget: Cell<u64>,
    direct_countdown: Cell<i32>,
    direct_granted: Cell<i32>,
    direct_saved: Cell<(i32, i32)>,
    deadline: Cell<Option<Instant>>,
    /// Inline safepoints granted per runtime check while no budget caps the
    /// grant; see [`next_poll_stride`].
    poll_stride: Cell<u64>,
    /// Clock reading of the previous time-quantum check and the target
    /// interval between checks.
    checked_at: Cell<Option<Instant>>,
    check_interval: Cell<Duration>,
    flag: Cell<*const AtomicBool>,
    /// Re-entrant no-preempt nesting depth (§22). While > 0, `willow_preempt_check`
    /// always reports "do not preempt" even if the flag/budget/time say otherwise.
    no_preempt_depth: Cell<u32>,
}

thread_local! {
    static QUANTUM: QuantumState = const {
        QuantumState {
            budget: Cell::new(0),
            direct_countdown: Cell::new(0),
            direct_granted: Cell::new(0),
            direct_saved: Cell::new((0, 0)),
            deadline: Cell::new(None),
            poll_stride: Cell::new(SYNC_POLL_STRIDE),
            checked_at: Cell::new(None),
            check_interval: Cell::new(Duration::ZERO),
            flag: Cell::new(std::ptr::null()),
            no_preempt_depth: Cell::new(0),
        }
    };
}

#[inline]
/// # Safety
///
/// `flag` must name a live `AtomicBool` owned by a task or a preemption flag
/// allocation for the lifetime of the returned reference.
unsafe fn flag_ref<'a>(flag: *const c_void) -> Option<&'a AtomicBool> {
    if flag.is_null() {
        None
    } else {
        // SAFETY: The caller retains the aligned task-owned or standalone AtomicBool
        // through the borrow.
        Some(unsafe { &*(flag as *const AtomicBool) })
    }
}

// ── Config introspection ────────────────────────────────────────────────────

/// Safepoint budget per quantum from `WILLOW_TASK_BUDGET` (or the default).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_preempt_task_budget() -> u64 {
    runtime_preempt_config().task_budget()
}

/// Time quantum in milliseconds from `WILLOW_TIME_QUANTUM_MS` (or the default).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_preempt_time_quantum_ms() -> u64 {
    runtime_preempt_config().time_quantum_ms()
}

// ── Per-task preemption flag lifecycle ──────────────────────────────────────

/// Allocate a fresh per-task preemption flag (initially clear). Owned by the
/// caller; release with [`willow_preempt_flag_free`].
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_preempt_flag_new() -> *mut c_void {
    Box::into_raw(Box::new(AtomicBool::new(false))) as *mut c_void
}

/// Free a flag from [`willow_preempt_flag_new`]. `null` is ignored.
///
/// # Safety
/// A non-null flag must be the unique allocation returned by willow_preempt_flag_new.
/// Release it exactly once, after all accesses and every bound or suspended quantum have
/// ended.
///
/// ```compile_fail,E0133
/// let flag = willow_runtime::preempt::willow_preempt_flag_new();
/// willow_runtime::preempt::willow_preempt_flag_free(flag);
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_preempt_flag_free(flag: *mut c_void) {
    if !flag.is_null() {
        // Safety: `flag` came from `willow_preempt_flag_new` and is freed once.
        drop(unsafe { Box::from_raw(flag as *mut AtomicBool) });
    }
}

/// Request preemption of the task owning `flag` (spec §9): set the flag with
/// release ordering so a safepoint observing it (acquire) sees a happens-before
/// edge. Safe to call from any thread (scheduler, GC, cancellation).
///
/// # Safety
/// A non-null flag must point to a live, aligned AtomicBool accessed only atomically.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_preempt_request(flag: *const c_void) {
    // SAFETY: The caller keeps the optional AtomicBool live and aligned through this access.
    if let Some(flag) = unsafe { flag_ref(flag) } {
        flag.store(true, Ordering::Release);
    }
}

/// Clear/acknowledge a preemption request on `flag` (spec §9 step 1).
///
/// # Safety
/// A non-null flag must point to a live, aligned AtomicBool accessed only atomically.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_preempt_clear(flag: *const c_void) {
    // SAFETY: The caller keeps the optional AtomicBool live and aligned through this access.
    if let Some(flag) = unsafe { flag_ref(flag) } {
        flag.store(false, Ordering::Release);
    }
}

/// Query whether `flag` currently has a pending preemption request (acquire).
///
/// # Safety
/// A non-null flag must point to a live, aligned AtomicBool accessed only atomically.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_preempt_requested(flag: *const c_void) -> i32 {
    // SAFETY: The caller keeps the optional AtomicBool live and aligned through this access.
    match unsafe { flag_ref(flag) } {
        Some(flag) if flag.load(Ordering::Acquire) => 1,
        _ => 0,
    }
}

// ── Quantum lifecycle (called by the scheduler around each poll) ─────────────

/// Begin a fresh scheduling quantum for the task identified by `flag` before its
/// poll runs: reset the safepoint budget and time deadline, bind the flag, and
/// clear any stale request (the previous preemption was already honored by the
/// requeue). `flag` may be `null` for budget/time-only quanta.
///
/// # Safety
/// A non-null flag must point to a live, aligned AtomicBool accessed only atomically. Keep
/// its address and allocation live throughout the bound quantum, including suspension and
/// restoration, until unbound.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_preempt_begin(flag: *const c_void) {
    // SAFETY: The caller retains the flag at this address until the quantum has ended.
    unsafe { begin_quantum(runtime_preempt_config(), flag) };
}

/// Core of [`willow_preempt_begin`] with an explicit config — env-free so tests
/// exercise the quantum machinery deterministically without mutating the
/// process-global environment (cargo runs unit tests in parallel threads).
///
/// # Safety
/// A non-null flag must point to a live, aligned AtomicBool accessed only atomically. Keep
/// its address and allocation live throughout the bound quantum, including suspension and
/// restoration, until unbound.
pub(crate) unsafe fn begin_quantum(config: PreemptConfig, flag: *const c_void) {
    let now = Instant::now();
    let quantum = Duration::from_millis(config.time_quantum_ms());
    let deadline = now.checked_add(quantum);
    QUANTUM.with(|q| {
        q.flag.set(flag as *const AtomicBool);
        q.budget.set(config.task_budget());
        q.deadline.set(deadline);
        // Every quantum starts at the minimum stride: the previous task's
        // safepoint rate says nothing about this one.
        q.poll_stride.set(SYNC_POLL_STRIDE);
        q.checked_at.set(Some(now));
        q.check_interval.set(quantum / POLL_CHECKS_PER_QUANTUM);
        q.no_preempt_depth.set(0);
        q.direct_countdown.set(0);
        q.direct_granted.set(0);
    });
    // SAFETY: The caller keeps the optional AtomicBool live and aligned through this access.
    if let Some(flag) = unsafe { flag_ref(flag) } {
        flag.store(false, Ordering::Release);
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
    // Nested scheduler drives rearm the enclosing task without resuming its
    // native stack. Invalidate that task's old grant here as well as in resume.
    crate::native_stack::poll_refill(0);
}

/// End the current quantum after a poll returns: unbind the flag and zero the
/// budget so a stray safepoint outside a poll never reports "preempt".
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_preempt_end() {
    QUANTUM.with(|q| {
        q.flag.set(std::ptr::null());
        q.budget.set(0);
        q.deadline.set(None);
        q.checked_at.set(None);
        q.no_preempt_depth.set(0);
        q.direct_countdown.set(0);
        q.direct_granted.set(0);
    });
}

/// Enter a no-preempt region (§22): nestable; while open, `willow_preempt_check`
/// reports no-preempt regardless of flag/budget/time.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_preempt_enter_no_preempt() {
    enter_no_preempt();
}

/// [`willow_preempt_enter_no_preempt`] for runtime guards, which already run
/// inside an FFI boundary and must not pay a nested one per region.
fn enter_no_preempt() {
    QUANTUM.with(|q| {
        if q.no_preempt_depth.get() == 0 {
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
            crate::native_stack::poll_no_preempt(true);
            q.direct_saved
                .set((q.direct_countdown.replace(0), q.direct_granted.replace(0)));
        }
        q.no_preempt_depth
            .set(q.no_preempt_depth.get().saturating_add(1))
    });
}

/// Leave a no-preempt region (§22).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_preempt_leave_no_preempt() {
    leave_no_preempt();
}

/// [`willow_preempt_leave_no_preempt`] for runtime guards.
fn leave_no_preempt() {
    QUANTUM.with(|q| {
        // Preserve pre-region debt without sampling the clock, and discard
        // inline work executed while suppressed.
        if q.no_preempt_depth.get() == 1 {
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
            crate::native_stack::poll_no_preempt(false);
            let (countdown, granted) = q.direct_saved.get();
            q.direct_countdown.set(countdown);
            q.direct_granted.set(granted);
        }
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
        enter_no_preempt();
        Self
    }
}

impl Drop for NoPreemptGuard {
    fn drop(&mut self) {
        leave_no_preempt();
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
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_preempt_check() -> i32 {
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
        // A sync helper can return before consuming its entire inline grant.
        // Settle that debt together with this async/runtime safepoint, then
        // cap the next grant by the SAME remaining quantum budget. Otherwise
        // alternating async/sync code can overrun or double-charge the budget.
        let result = charge_safepoints(1 + crate::native_stack::poll_pending());
        crate::native_stack::poll_refill(sync_poll_grant());
        return result;
    }
    let result = charge_safepoints(1);
    let grant = sync_poll_grant();
    QUANTUM.with(|q| {
        q.direct_countdown.set(grant);
        q.direct_granted.set(grant);
    });
    result
}

/// Invocation-stable countdown for cooperative generated polls. Native polls
/// share their stack's counter with sync callees; direct polls cannot migrate
/// while their worker-stack invocation is live and use the quantum's Cell.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_async_poll_counter() -> *mut i32 {
    let native = willow_sync_poll_counter();
    if !native.is_null() {
        return native;
    }
    QUANTUM.with(|q| q.direct_countdown.as_ptr())
}

/// [`willow_preempt_check`] for `count` safepoints executed since the last
/// check. The budget trips at the same safepoint count as `count` single
/// checks would; the time quantum and flag are sampled once per batch.
/// Native callback completion also uses this to settle its remaining inline
/// debt before returning to a cooperative poll in the same quantum.
pub(crate) fn charge_safepoints(count: u64) -> i32 {
    QUANTUM.with(|q| {
        // A direct poll may call a native helper before exhausting its grant.
        // Settle its debt at every runtime check and invalidate the old grant
        // before a helper can consume the remaining shared quantum budget.
        let pending = (i64::from(q.direct_granted.replace(0))
            - i64::from(q.direct_countdown.replace(0)))
        .max(0) as u64;
        let count = count.saturating_add(pending);
        if q.no_preempt_depth.get() > 0 {
            return 0;
        }
        // SAFETY: Binding a quantum requires its optional flag to stay live until unbinding.
        let flag = unsafe { flag_ref(q.flag.get() as *const c_void) };

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
        let remaining = budget.saturating_sub(count);
        q.budget.set(remaining);
        if remaining == 0 {
            if let Some(flag) = flag {
                flag.store(true, Ordering::Release);
            }
            return 1;
        }

        // (3) Wall-clock time quantum.
        if let Some(deadline) = q.deadline.get() {
            let now = Instant::now();
            if now >= deadline {
                if let Some(flag) = flag {
                    flag.store(true, Ordering::Release);
                }
                return 1;
            }
            if let Some(previous) = q.checked_at.replace(Some(now)) {
                q.poll_stride.set(next_poll_stride(
                    q.poll_stride.get(),
                    now.saturating_duration_since(previous),
                    q.check_interval.get(),
                ));
            }
        }

        0
    })
}

/// Minimum inline safepoints generated synchronous code runs between runtime
/// checks, and the stride outside a timed quantum. The per-safepoint cost
/// stays one counter decrement either way.
pub const SYNC_POLL_STRIDE: u64 = 256;

/// Largest stride a fast loop grows to. Together with the check interval it
/// bounds how stale the time quantum, preemption flag, and cancellation state
/// can be.
pub const SYNC_POLL_STRIDE_MAX: u64 = 8192;

/// A timed quantum aims for this many runtime checks: often enough to keep
/// the deadline, preemption flag, and cancellation fresh, rarely enough that
/// a tight loop does not spend its time reading the clock.
const POLL_CHECKS_PER_QUANTUM: u32 = 64;

/// The stride after a check that found `elapsed` since the previous one.
/// Checks arriving in under half the target interval double the stride; a
/// check later than the interval halves it. Strides stay within
/// [`SYNC_POLL_STRIDE`, `SYNC_POLL_STRIDE_MAX`], so a loop whose safepoints
/// slow down returns to the minimum in a few checks.
pub(crate) fn next_poll_stride(stride: u64, elapsed: Duration, interval: Duration) -> u64 {
    let next = if elapsed.saturating_mul(2) < interval {
        stride.saturating_mul(2)
    } else if elapsed > interval {
        stride / 2
    } else {
        stride
    };
    next.clamp(SYNC_POLL_STRIDE, SYNC_POLL_STRIDE_MAX)
}

/// The next inline safepoint grant: never past the remaining budget, so the
/// budget still trips at the exact safepoint it would without batching.
fn sync_poll_grant() -> i32 {
    QUANTUM.with(|q| {
        let budget = q.budget.get();
        if budget == 0 || q.no_preempt_depth.get() > 0 {
            SYNC_POLL_STRIDE as i32
        } else {
            budget.min(q.poll_stride.get()) as i32
        }
    })
}

/// Invocation-constant inline safepoint countdown for generated synchronous
/// code, or null outside a scheduler-managed native stack. Generated loops
/// decrement it and call [`willow_sync_safepoint`] only when it reaches zero.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sync_poll_counter() -> *mut i32 {
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
    {
        crate::native_stack::poll_counter()
    }
    #[cfg(not(any(
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
    )))]
    std::ptr::null_mut()
}

/// Native-stack counterpart of the async poll safepoint. Cancellation returns
/// through compiler-generated cleanup edges; yielding preserves the call chain.
/// Charges every safepoint consumed from the inline countdown, then refills it.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sync_safepoint() -> i32 {
    // Generated callers reload every managed local from its root slot.
    let _relocation = crate::gc::RelocationSafeScope::enter();
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
    {
        // These non-inlined phases compute TLS addresses on their own worker.
        // Inlining both across the switch could let LLVM reuse the old TLS base.
        #[inline(never)]
        fn prepare() -> i32 {
            if crate::native_stack::cancelled() != 0 {
                return -1;
            }
            crate::gc::willow_gc_safepoint();
            charge_safepoints(crate::native_stack::poll_consumed())
        }
        #[inline(never)]
        fn finish() -> i32 {
            crate::native_stack::poll_refill(sync_poll_grant());
            crate::native_stack::cancelled()
        }
        if prepare() > 0 {
            crate::native_stack::suspend_generated();
        }
        finish()
    }
    #[cfg(not(any(
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
    )))]
    {
        crate::gc::willow_gc_safepoint();
        0
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sync_cancelled() -> i32 {
    sync_cancelled()
}

pub(crate) fn sync_cancelled() -> i32 {
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
    {
        crate::native_stack::cancelled()
    }
    #[cfg(not(any(
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
    )))]
    {
        0
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sync_cleanup_enter() {
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
    crate::native_stack::cleanup_enter();
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sync_cleanup_leave() {
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
    crate::native_stack::cleanup_leave();
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

    fn direct_check() -> i32 {
        let counter = willow_async_poll_counter();
        // SAFETY: This thread owns the active poll counter and keeps its storage live until the
        // quantum ends.
        unsafe {
            if *counter > 1 {
                *counter -= 1;
                0
            } else {
                willow_preempt_check()
            }
        }
    }

    #[test]
    fn production_budget_uses_deadline_and_bounds_runtime_checks() {
        for safepoints in [1024usize, 4096, 16384] {
            // SAFETY: A null flag binds no storage and is explicitly supported.
            unsafe {
                begin_quantum(
                    PreemptConfig::from_env_values(None, Some("100000")),
                    std::ptr::null(),
                )
            };
            let mut calls = 0;
            for _ in 0..safepoints {
                // SAFETY: This thread owns the active poll counter and keeps its storage live
                // until the quantum ends.
                calls += usize::from(unsafe { *willow_async_poll_counter() } <= 1);
                assert_eq!(direct_check(), 0);
            }
            // The stride adapts to the clock, so only its range is fixed.
            let most = 1 + (safepoints - 1) / SYNC_POLL_STRIDE as usize;
            let least = 1 + (safepoints - 1) / SYNC_POLL_STRIDE_MAX as usize;
            assert!((least..=most).contains(&calls), "{safepoints}: {calls}");
            QUANTUM.with(|q| q.deadline.set(Some(Instant::now())));
            let mut until_preempt = 0;
            while direct_check() == 0 {
                until_preempt += 1;
                assert!(until_preempt < SYNC_POLL_STRIDE_MAX);
            }
            willow_preempt_end();
        }
    }

    #[test]
    fn direct_async_batches_preserve_exact_budgets_and_bound_checks() {
        for budget in [
            1, 2, 3, 63, 64, 65, 127, 255, 256, 257, 1000, 4096, 8191, 8192, 8193, 20000,
        ] {
            // SAFETY: A null flag binds no storage and is explicitly supported.
            unsafe { begin_quantum(cfg(budget, 100000), std::ptr::null()) };
            let mut calls = 0;
            for n in 1..=budget {
                // SAFETY: This thread owns the active poll counter and keeps its storage live
                // until the quantum ends.
                if unsafe { *willow_async_poll_counter() } <= 1 {
                    calls += 1;
                }
                assert_eq!(
                    direct_check(),
                    i32::from(n == budget),
                    "budget={budget} n={n}"
                );
            }
            let most = 1 + (budget - 1).div_ceil(SYNC_POLL_STRIDE);
            let least = 1 + (budget - 1).div_ceil(SYNC_POLL_STRIDE_MAX);
            assert!((least..=most).contains(&calls), "budget={budget}: {calls}");
            willow_preempt_end();
        }
    }

    #[test]
    fn poll_stride_follows_the_check_interval() {
        let interval = Duration::from_micros(100);
        let early = Duration::from_micros(10);
        let on_time = Duration::from_micros(80);
        let late = Duration::from_micros(500);
        for (stride, elapsed, expected) in [
            (SYNC_POLL_STRIDE, early, 2 * SYNC_POLL_STRIDE),
            (1024, early, 2048),
            (SYNC_POLL_STRIDE_MAX, early, SYNC_POLL_STRIDE_MAX),
            (1024, on_time, 1024),
            (1024, interval, 1024),
            (1024, late, 512),
            (SYNC_POLL_STRIDE, late, SYNC_POLL_STRIDE),
            (
                SYNC_POLL_STRIDE_MAX,
                Duration::MAX,
                SYNC_POLL_STRIDE_MAX / 2,
            ),
        ] {
            assert_eq!(
                next_poll_stride(stride, elapsed, interval),
                expected,
                "{stride} {elapsed:?}"
            );
        }
        // A fast loop reaches the largest stride in log2(MAX / MIN) checks,
        // and a slow one falls back to the smallest in as many.
        let mut stride = SYNC_POLL_STRIDE;
        let mut steps = 0;
        while stride < SYNC_POLL_STRIDE_MAX {
            stride = next_poll_stride(stride, early, interval);
            steps += 1;
        }
        let doublings = (SYNC_POLL_STRIDE_MAX / SYNC_POLL_STRIDE).ilog2();
        assert_eq!(steps, doublings);
        for _ in 0..doublings {
            stride = next_poll_stride(stride, late, interval);
        }
        assert_eq!(stride, SYNC_POLL_STRIDE);
    }

    #[test]
    fn fast_quantum_grows_the_grant_and_new_quantum_resets_it() {
        // An unreachable check interval makes every check early, so the
        // stride doubles once per runtime check regardless of timing.
        // SAFETY: A null flag binds no storage and is explicitly supported.
        unsafe { begin_quantum(cfg(u64::MAX, 100000), std::ptr::null()) };
        QUANTUM.with(|q| q.check_interval.set(Duration::MAX));
        let doublings = (SYNC_POLL_STRIDE_MAX / SYNC_POLL_STRIDE).ilog2() as usize;
        let mut grants = Vec::new();
        for _ in 0..=doublings + 1 {
            assert_eq!(willow_preempt_check(), 0);
            grants.push(QUANTUM.with(|q| q.direct_granted.get()) as u64);
        }
        let expected: Vec<u64> = (1..=doublings + 2)
            .map(|k| (SYNC_POLL_STRIDE << k.min(doublings)).min(SYNC_POLL_STRIDE_MAX))
            .collect();
        assert_eq!(grants, expected);
        willow_preempt_end();
        // SAFETY: A null flag binds no storage and is explicitly supported.
        unsafe { begin_quantum(cfg(u64::MAX, 100000), std::ptr::null()) };
        assert_eq!(QUANTUM.with(|q| q.poll_stride.get()), SYNC_POLL_STRIDE);
        // A budget still caps the grown grant at the exact remaining count.
        QUANTUM.with(|q| {
            q.poll_stride.set(SYNC_POLL_STRIDE_MAX);
            q.budget.set(300);
        });
        assert_eq!(sync_poll_grant(), 300);
        willow_preempt_end();
    }

    #[test]
    fn direct_async_partial_batch_settles_before_helper_checks() {
        // SAFETY: A null flag binds no storage and is explicitly supported.
        unsafe { begin_quantum(cfg(100, 100000), std::ptr::null()) };
        for _ in 0..20 {
            assert_eq!(direct_check(), 0);
        }
        assert_eq!(charge_safepoints(79), 0);
        assert_eq!(direct_check(), 1);
        willow_preempt_end();
    }

    #[test]
    fn direct_async_request_and_deadline_trip_within_one_batch() {
        for explicit in [false, true] {
            let flag = AtomicBool::new(false);
            // SAFETY: The fixture keeps the flag at its registered address throughout this
            // quantum and unbinds it before release.
            unsafe { begin_quantum(cfg(10000, 100000), std::ptr::from_ref(&flag).cast()) };
            assert_eq!(direct_check(), 0);
            if explicit {
                flag.store(true, Ordering::Release);
            } else {
                QUANTUM.with(|q| q.deadline.set(Some(Instant::now())));
            }
            // The pending request is seen at the end of the current grant.
            let grant = QUANTUM.with(|q| q.direct_granted.get()) as u64;
            let mut checks = 0;
            loop {
                checks += 1;
                if direct_check() != 0 {
                    break;
                }
                assert!(checks < grant);
            }
            assert_eq!(checks, grant);
            assert!((SYNC_POLL_STRIDE..=SYNC_POLL_STRIDE_MAX).contains(&grant));
            willow_preempt_end();
        }
    }

    #[test]
    fn direct_async_suppressed_work_does_not_consume_budget() {
        // SAFETY: A null flag binds no storage and is explicitly supported.
        unsafe { begin_quantum(cfg(100, 100000), std::ptr::null()) };
        for _ in 0..20 {
            assert_eq!(direct_check(), 0);
        }
        willow_preempt_enter_no_preempt();
        for _ in 0..200 {
            assert_eq!(direct_check(), 0);
        }
        willow_preempt_leave_no_preempt();
        for n in 1..=80 {
            assert_eq!(direct_check(), i32::from(n == 80));
        }
        willow_preempt_end();
    }

    #[test]
    fn direct_async_new_quantum_discards_old_grant() {
        // SAFETY: A null flag binds no storage and is explicitly supported.
        unsafe { begin_quantum(cfg(100, 100000), std::ptr::null()) };
        for _ in 0..20 {
            assert_eq!(direct_check(), 0);
        }
        willow_preempt_end();
        // SAFETY: A null flag binds no storage and is explicitly supported.
        unsafe { begin_quantum(cfg(1, 100000), std::ptr::null()) };
        assert_eq!(direct_check(), 1);
        willow_preempt_end();
        assert_eq!(direct_check(), 0);
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
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        assert_eq!(unsafe { willow_preempt_requested(flag) }, 0);
        // SAFETY: This test owns the standalone flag and all accesses and bound quanta have
        // ended.
        unsafe { willow_preempt_flag_free(flag) };
    }

    // 7. request sets, clear resets (acquire/release visible on same thread).
    #[test]
    fn request_then_clear() {
        let flag = willow_preempt_flag_new();
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        unsafe { willow_preempt_request(flag) };
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        assert_eq!(unsafe { willow_preempt_requested(flag) }, 1);
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        unsafe { willow_preempt_clear(flag) };
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        assert_eq!(unsafe { willow_preempt_requested(flag) }, 0);
        // SAFETY: This test owns the standalone flag and all accesses and bound quanta have
        // ended.
        unsafe { willow_preempt_flag_free(flag) };
    }

    // 8. Null flag is inert across all flag ops.
    #[test]
    fn null_flag_is_inert() {
        let null = std::ptr::null();
        // SAFETY: A null flag binds no storage and is explicitly supported.
        assert_eq!(unsafe { willow_preempt_requested(null) }, 0);
        // SAFETY: A null flag binds no storage and is explicitly supported.
        unsafe { willow_preempt_request(null) }; // no panic
        // SAFETY: A null flag binds no storage and is explicitly supported.
        unsafe { willow_preempt_clear(null) }; // no panic
        // SAFETY: A null flag binds no storage and is explicitly supported.
        assert_eq!(unsafe { willow_preempt_requested(null) }, 0);
        // SAFETY: A null flag binds no storage and is explicitly supported.
        unsafe { willow_preempt_flag_free(std::ptr::null_mut()) }; // no panic
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
        // SAFETY: The fixture keeps the flag at its registered address throughout this
        // quantum and unbinds it before release.
        unsafe { begin_quantum(cfg(4, 60_000), flag) };
        // budget=4 → first 3 checks pass, 4th trips.
        assert_eq!(willow_preempt_check(), 0);
        assert_eq!(willow_preempt_check(), 0);
        assert_eq!(willow_preempt_check(), 0);
        assert_eq!(willow_preempt_check(), 1);
        // Flag was set by the budget trip.
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        assert_eq!(unsafe { willow_preempt_requested(flag) }, 1);
        willow_preempt_end();
        // SAFETY: This test owns the standalone flag and all accesses and bound quanta have
        // ended.
        unsafe { willow_preempt_flag_free(flag) };
    }

    // 11. begin clears a stale request from a prior quantum.
    #[test]
    fn begin_clears_stale_request() {
        let flag = willow_preempt_flag_new();
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        unsafe { willow_preempt_request(flag) };
        // SAFETY: The fixture keeps the flag at its registered address throughout this
        // quantum and unbinds it before release.
        unsafe { begin_quantum(cfg(1000, 60_000), flag) };
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        assert_eq!(unsafe { willow_preempt_requested(flag) }, 0);
        assert_eq!(willow_preempt_check(), 0);
        willow_preempt_end();
        // SAFETY: This test owns the standalone flag and all accesses and bound quanta have
        // ended.
        unsafe { willow_preempt_flag_free(flag) };
    }

    // 12. An explicit request mid-quantum trips the very next check.
    #[test]
    fn explicit_request_preempts_immediately() {
        let flag = willow_preempt_flag_new();
        // SAFETY: The fixture keeps the flag at its registered address throughout this
        // quantum and unbinds it before release.
        unsafe { begin_quantum(cfg(1000, 60_000), flag) };
        assert_eq!(willow_preempt_check(), 0);
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        unsafe { willow_preempt_request(flag) };
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
        // SAFETY: This test owns the standalone flag and all accesses and bound quanta have
        // ended.
        unsafe { willow_preempt_flag_free(flag) };
    }

    // 13. Time quantum trips even with budget remaining.
    #[test]
    fn time_quantum_preempts() {
        let flag = willow_preempt_flag_new();
        // SAFETY: The fixture keeps the flag at its registered address throughout this
        // quantum and unbinds it before release.
        unsafe { begin_quantum(cfg(1_000_000, 1), flag) };
        // Spin past the 1ms deadline, then a check must trip on time.
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(3) {
            std::hint::spin_loop();
        }
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
        // SAFETY: This test owns the standalone flag and all accesses and bound quanta have
        // ended.
        unsafe { willow_preempt_flag_free(flag) };
    }

    // 14. No-preempt region overrides an explicit request.
    #[test]
    fn no_preempt_region_overrides_request() {
        let flag = willow_preempt_flag_new();
        // SAFETY: The fixture keeps the flag at its registered address throughout this
        // quantum and unbinds it before release.
        unsafe { begin_quantum(cfg(1000, 60_000), flag) };
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        unsafe { willow_preempt_request(flag) };
        willow_preempt_enter_no_preempt();
        assert_eq!(willow_preempt_check(), 0);
        willow_preempt_leave_no_preempt();
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
        // SAFETY: This test owns the standalone flag and all accesses and bound quanta have
        // ended.
        unsafe { willow_preempt_flag_free(flag) };
    }

    // 15. No-preempt nesting requires matching leaves.
    #[test]
    fn no_preempt_region_nests() {
        let flag = willow_preempt_flag_new();
        // SAFETY: The fixture keeps the flag at its registered address throughout this
        // quantum and unbinds it before release.
        unsafe { begin_quantum(cfg(1000, 60_000), flag) };
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        unsafe { willow_preempt_request(flag) };
        willow_preempt_enter_no_preempt();
        willow_preempt_enter_no_preempt();
        willow_preempt_leave_no_preempt();
        assert_eq!(willow_preempt_check(), 0); // still one region open
        willow_preempt_leave_no_preempt();
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
        // SAFETY: This test owns the standalone flag and all accesses and bound quanta have
        // ended.
        unsafe { willow_preempt_flag_free(flag) };
    }

    // 16. end() unbinds: a later check (no new begin) never preempts.
    #[test]
    fn end_unbinds_quantum() {
        let flag = willow_preempt_flag_new();
        // SAFETY: The fixture keeps the flag at its registered address throughout this
        // quantum and unbinds it before release.
        unsafe { begin_quantum(cfg(2, 60_000), flag) };
        willow_preempt_end();
        for _ in 0..10 {
            assert_eq!(willow_preempt_check(), 0);
        }
        // SAFETY: This test owns the standalone flag and all accesses and bound quanta have
        // ended.
        unsafe { willow_preempt_flag_free(flag) };
    }

    // 17. begin with a null flag still enforces budget (no per-task flag).
    #[test]
    fn null_flag_quantum_still_counts_budget() {
        // SAFETY: A null flag binds no storage and is explicitly supported.
        unsafe { begin_quantum(cfg(2, 60_000), std::ptr::null()) };
        assert_eq!(willow_preempt_check(), 0);
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
    }

    // 18. budget=1 trips on the first check.
    #[test]
    fn budget_one_trips_immediately() {
        // SAFETY: A null flag binds no storage and is explicitly supported.
        unsafe { begin_quantum(cfg(1, 60_000), std::ptr::null()) };
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
    }

    // 19. A new begin re-arms the budget after a prior exhaustion.
    #[test]
    fn rearm_after_exhaustion() {
        let flag = willow_preempt_flag_new();
        // SAFETY: The fixture keeps the flag at its registered address throughout this
        // quantum and unbinds it before release.
        unsafe { begin_quantum(cfg(2, 60_000), flag) };
        assert_eq!(willow_preempt_check(), 0);
        assert_eq!(willow_preempt_check(), 1);
        // Re-arm for the next quantum.
        // SAFETY: The fixture keeps the flag at its registered address throughout this
        // quantum and unbinds it before release.
        unsafe { begin_quantum(cfg(2, 60_000), flag) };
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        assert_eq!(unsafe { willow_preempt_requested(flag) }, 0);
        assert_eq!(willow_preempt_check(), 0);
        assert_eq!(willow_preempt_check(), 1);
        willow_preempt_end();
        // SAFETY: This test owns the standalone flag and all accesses and bound quanta have
        // ended.
        unsafe { willow_preempt_flag_free(flag) };
    }

    // 20. ABI config getters return defaults when env is unset (no env mutation,
    // so this stays deterministic under parallel test execution).
    #[test]
    fn abi_config_getters_default() {
        // These read the process env once; no test sets WILLOW_TASK_BUDGET /
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
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        assert_eq!(unsafe { willow_preempt_requested(flag) }, 0);
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        unsafe { willow_preempt_request(flag) };
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        unsafe { willow_preempt_request(flag) }; // idempotent
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        assert_eq!(unsafe { willow_preempt_requested(flag) }, 1);
        // SAFETY: This test owns the standalone flag and all accesses and bound quanta have
        // ended.
        unsafe { willow_preempt_flag_free(flag) };
    }

    // 22. Cross-thread request is observed via acquire/release (§23).
    #[test]
    fn cross_thread_request_visible() {
        let flag = willow_preempt_flag_new();
        let addr = flag as usize;
        std::thread::spawn(crate::runtime::serving(move || {
            // SAFETY: The fixture retains the initialized AtomicBool through this access,
            // including concurrent requests.
            unsafe { willow_preempt_request(addr as *const c_void) };
        }))
        .join()
        .unwrap();
        // SAFETY: The fixture retains the initialized AtomicBool through this access,
        // including concurrent requests.
        assert_eq!(unsafe { willow_preempt_requested(flag) }, 1);
        // SAFETY: This test owns the standalone flag and all accesses and bound quanta have
        // ended.
        unsafe { willow_preempt_flag_free(flag) };
    }
}
