use super::*;

/// Spawn a cooperative task on the global scheduler. The frame is rooted with
/// the GC so it (and the values it references) survives collection while the
/// task is pending. Returns the task id.
///
/// # Safety
/// The callback must accept this frame and remain callable until retirement. A non-null
/// frame must be an initialized nonmoving managed task frame in this runtime with the header
/// and slots its callbacks access. Keep it live until the scheduler has taken its root.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_sched_spawn(poll: RuntimePollFn, frame: *mut c_void) -> u64 {
    // SAFETY: The supplied callback matches this frame layout; the frame storage remains live
    // across scheduling and callback execution.
    unsafe { spawn_global_task_initialized(poll, frame, None, |_| {}) }
}

/// Spawn a native runtime task only after all state needed by its first poll
/// and cancellation cleanup has been initialized. The closure runs after its
/// id is reserved but outside `GLOBAL_SCHEDULER`, and before the task is visible
/// in either the task table or a run queue. It may call scheduler APIs; the
/// reserved task itself remains deliberately unobservable until publication.
///
/// # Safety
/// The callback must accept this frame and remain callable until retirement. A non-null
/// frame must be an initialized nonmoving managed task frame in this runtime with the header
/// and slots its callbacks access. Keep it live until the scheduler has taken its root.
pub(crate) unsafe fn spawn_global_task_initialized(
    poll: RuntimePollFn,
    frame: *mut c_void,
    cancel: Option<RuntimeCancelFn>,
    initialize: impl FnOnce(RuntimeTaskId),
) -> u64 {
    // SAFETY: The supplied callback matches this frame layout; the frame storage remains live
    // across scheduling and callback execution.
    unsafe { spawn_global_task_initialized_inner(poll, frame, cancel, initialize, false) }
}

/// Spawn a generated poll whose synchronous calls enter a stack lazily.
///
/// # Safety
/// The callback must accept this frame and remain callable until retirement. A non-null
/// frame must be an initialized nonmoving managed task frame in this runtime with the header
/// and slots its callbacks access. Keep it live until the scheduler has taken its root.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_sched_spawn_cooperative(
    poll: RuntimePollFn,
    frame: *mut c_void,
) -> i64 {
    // SAFETY: The supplied callback matches this frame layout; the frame storage remains live
    // across scheduling and callback execution.
    (unsafe { spawn_global_task_initialized_inner(poll, frame, None, |_| {}, true) }) as i64
}

/// # Safety
/// The callback must accept this frame and remain callable until retirement. A non-null
/// frame must be an initialized nonmoving managed task frame in this runtime with the header
/// and slots its callbacks access. Keep it live until the scheduler has taken its root.
pub(super) unsafe fn spawn_global_task_initialized_inner(
    poll: RuntimePollFn,
    frame: *mut c_void,
    cancel: Option<RuntimeCancelFn>,
    initialize: impl FnOnce(RuntimeTaskId),
    cooperative_poll: bool,
) -> u64 {
    // The caller holds `frame` unrooted once the task is published and may
    // finish on another worker.
    let _unit = crate::frame_reclaim::ExecutionUnit::enter();
    // Reserve and publish through the internally synchronized components;
    // `GLOBAL_SCHEDULER` is not taken. The frame is rooted before
    // the callback because native initialization may allocate or explicitly
    // collect. A panic leaves an id gap but the guard rolls the unpublished
    // runtime root back.
    let target = global_components().spawn_target();
    let id = target.reserve_task_id();
    let runtime = crate::runtime::current();
    let mut root = PendingSpawnRoot::new(runtime, frame as *mut u8);
    initialize(id);
    // Captures and arguments become reachable by the new task, which may
    // run on any worker. Spawn is a GC point (see the stress collection
    // below) and the frame is rooted, so the publication may park.
    crate::gc::publish_frame_slots_in(
        runtime,
        frame as *mut u8,
        None,
        crate::gc::PublicationPoint::Spawn,
        true,
    );
    target.publish(id, poll, frame, cancel, cooperative_poll);
    root.publish();
    crate::observability::record(
        crate::observability::RuntimeEventKind::TaskSpawn,
        current_task_id().map(|_| current_worker()),
        id,
        0,
    );
    crate::gc::stress_collect("scheduler");
    id
}

/// Owns the runtime root between id reservation and task publication.
/// Published tasks transfer that ownership to `RuntimeTask::frame_rooted`;
/// unwinding initializers drop it here instead.
pub(super) struct PendingSpawnRoot<'a> {
    pub(super) owner: &'a crate::runtime::Runtime,
    pub(super) frame: *mut u8,
    pub(super) published: bool,
}

impl<'a> PendingSpawnRoot<'a> {
    pub(super) fn new(owner: &'a crate::runtime::Runtime, frame: *mut u8) -> Self {
        crate::gc::add_task_frame_root_in(owner, frame);
        Self {
            owner,
            frame,
            published: false,
        }
    }

    pub(super) fn publish(&mut self) {
        self.published = true;
    }
}

impl Drop for PendingSpawnRoot<'_> {
    fn drop(&mut self) {
        if !self.published {
            crate::gc::remove_runtime_root_in(self.owner, self.frame);
        }
    }
}

/// Wake a parked task, re-queueing it as ready.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_wake(id: u64) {
    let _ = try_wake_parked_task(id);
}

/// Wake a contiguous array of task IDs. A zero length accepts a null pointer.
///
/// # Safety
/// For nonzero `len`, `ids` must reference `len` initialized, aligned u64 values
/// readable for this call; their storage must remain valid across GC safepoints.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_sched_wake_many(ids: *const u64, len: usize) {
    if len == 0 {
        return;
    }
    // SAFETY: The caller supplies len initialized task IDs in a contiguous slice that stays live
    // during wake processing.
    let ids = unsafe { std::slice::from_raw_parts(ids, len) };
    wake_channel_owners(ids, &mut WakeBatchScratch::default());
}

/// Wake a channel waiter and report whether it actually consumed the wake.
/// A caller can skip stale/cancelled/already-runnable waiter ids until one
/// parked task becomes Ready, avoiding both lost capacity and a thundering
/// herd when a bounded channel frees one slot.
pub(crate) fn try_wake_parked_task(id: u64) -> bool {
    crate::gc::stress_collect("scheduler");
    let transitioned = wake_global_task(id);
    if transitioned {
        crate::observability::record(
            crate::observability::RuntimeEventKind::TaskWake,
            current_task_id().map(|_| current_worker()),
            id,
            0,
        );
    }
    crate::gc::stress_collect("scheduler");
    transitioned
}

/// A channel reservation already published under its mutex remains valid for
/// a live task that is queued or polling: it may have observed readiness before
/// this wake. Only terminal owners may lose it; exact cleanup handles races.
pub(crate) fn wake_channel_owner(id: u64) -> bool {
    crate::gc::stress_collect("scheduler");
    let outcome = wake_global_task_outcome(id);
    if outcome == WakeOutcome::Enqueue {
        crate::observability::record(
            crate::observability::RuntimeEventKind::TaskWake,
            current_task_id().map(|_| current_worker()),
            id,
            0,
        );
    }
    crate::gc::stress_collect("scheduler");
    outcome != WakeOutcome::Terminal
}

/// The id of the currently-running task (0 if none). Used by blocking runtime
/// primitives (e.g. cooperative channel `recv`) to register the running task as
/// a waiter before it suspends.
/// Request cooperative cancellation of `id`. A parked task is
/// re-queued so the cancellation is observed promptly; the task is finalized
/// (state Cancelled, never polled again) at the next scheduler claim.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_cancel(id: u64) {
    let id = id as RuntimeTaskId;
    let tasks = global_task_table();
    let run_queues = global_run_queues();
    let changed = tasks
        .with_mut(id, |task| {
            let before = task.state.lifecycle();
            let outcome = task.state.request_cancel(&tasks.cancel_epoch);
            if outcome != CancelOutcome::NoChange {
                // Mirror the request into the frame header so
                // `Task::is_cancelled()` is a plain load.
                // SAFETY: The frame owner retains the initialized ABI status word through
                // this atomic access.
                unsafe { crate::async_frame::frame_request_cancel(task.frame) };
            }
            if outcome == CancelOutcome::Enqueue {
                task.wake_deadline = None;
                run_queues.push_global(id);
            }
            tasks.reconcile_blocked_transition(before, task.state.lifecycle());
            outcome != CancelOutcome::NoChange
        })
        .unwrap_or(false);
    if changed {
        crate::observability::record(
            crate::observability::RuntimeEventKind::TaskCancelRequested,
            current_task_id().map(|_| current_worker()),
            id,
            0,
        );
    }
}

/// Record the source location of the call that spawned task `id` (file is a
/// WillowString; copied out of the GC heap). Shown in panic/debug traces.
///
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_sched_set_spawn_site(id: u64, file: *const u8, line: i64) {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let file = unsafe { crate::string::willow_string_as_str(file) }.to_string();
    let id = id as RuntimeTaskId;
    global_task_table().with_mut(id, |task| {
        task.set_spawn_site(file, line as u32);
    });
}

/// Publish channel ownership under its task shard, without a safepoint.
pub(crate) fn install_channel_ownership(task_id: u64, token: ChannelOwnershipToken) -> bool {
    global_task_table()
        .with_mut(task_id, |task| {
            let state = task.state.load();
            if state.lifecycle().is_terminal()
                || state.lifecycle() == TaskLifecycle::Cancelling
                || state.cancel_requested()
            {
                return false;
            }
            task.install_channel_ownership(token)
        })
        .unwrap_or(false)
}

pub(crate) fn transition_channel_ownership(
    task_id: u64,
    old: ChannelOwnershipToken,
    new: ChannelOwnershipToken,
) -> bool {
    global_task_table()
        .with_mut(task_id, |task| {
            let state = task.state.load();
            if state.lifecycle().is_terminal()
                || state.lifecycle() == TaskLifecycle::Cancelling
                || state.cancel_requested()
            {
                return false;
            }
            task.transition_channel_ownership(old, new)
        })
        .unwrap_or(false)
}

pub(crate) fn clear_channel_ownership(task_id: u64, token: ChannelOwnershipToken) -> bool {
    global_task_table()
        .with_mut(task_id, |task| task.clear_channel_ownership(token))
        .unwrap_or(false)
}

#[cfg(test)]
pub(crate) fn take_channel_waits(task_id: u64) -> Vec<ChannelOwnershipToken> {
    global_task_table()
        .with_mut(task_id, RuntimeTask::take_wait_channels)
        .unwrap_or_default()
}

// ── Lock waiter reverse links ──────────────────────────────
//
// The task-shard half of the `LockState -> TaskShard` protocol in
// `lock_wait.rs`. Each of these takes exactly one shard lock and releases it
// before returning, so a caller holding a lock's state never holds a shard
// across anything else, and a caller holding no lock state (cancellation) never
// acquires one while a shard is held.

/// Publish `link` as `task_id`'s lock registration. Returns whether it was
/// installed: an unknown, terminal, or cancel-requested task is not eligible to
/// join a wait queue, and neither is one that already has a registration.
///
/// The eligibility check happens inside the shard's critical section, so the
/// lock cannot enqueue a task that became terminal a moment earlier.
pub(crate) fn install_lock_wait_link(
    task_id: RuntimeTaskId,
    link: LockWaitLink,
    publish_lock_side: impl FnOnce() -> bool,
) -> bool {
    let mut publish_lock_side = Some(publish_lock_side);
    global_task_table()
        .with_mut(task_id, |task| {
            let state = task.state.load();
            let lifecycle = state.lifecycle();
            if lifecycle.is_terminal()
                || lifecycle == TaskLifecycle::Cancelling
                || state.cancel_requested()
            {
                return false;
            }
            if task.lock_wait().is_some() {
                return false;
            }
            if !(publish_lock_side.take().expect("lock publish callback"))() {
                return false;
            }
            assert!(
                task.install_lock_wait(link),
                "lock wait link changed while its TaskShard was held"
            );
            true
        })
        .unwrap_or(false)
}

/// Move `task_id`'s registration `Waiting -> HandoffOwned` for exactly
/// `(lock_id, token)`. Returns whether the transition happened; the lock treats
/// `false` as "this candidate was cancelled or reaped, skip it".
///
/// A terminal task is refused here rather than in the lock: handing ownership to
/// a task that will never be polled again would strand the lock.
pub(crate) fn promote_lock_wait_link(
    task_id: RuntimeTaskId,
    lock_id: LockId,
    token: RegistrationToken,
    reserve_lock_side: impl FnOnce(),
) -> bool {
    let mut reserve_lock_side = Some(reserve_lock_side);
    global_task_table()
        .with_mut(task_id, |task| {
            if task.state.lifecycle().is_terminal() {
                return false;
            }
            if !task.promote_lock_wait(lock_id, token) {
                return false;
            }
            (reserve_lock_side.take().expect("lock reserve callback"))();
            true
        })
        .unwrap_or(false)
}

/// Reconcile a handoff whose scheduler wake observed a terminal task. The lock
/// side callback runs while the task shard is still held when the record exists;
/// when it has already been reaped, task ids are never reused and the callback
/// runs immediately afterwards. The caller holds LockState in both cases.
pub(crate) fn revoke_terminal_lock_handoff(
    task_id: RuntimeTaskId,
    lock_id: LockId,
    token: RegistrationToken,
    clear_lock_side: impl FnOnce(),
) -> bool {
    let mut clear_lock_side = Some(clear_lock_side);
    let removed = global_task_table().with_mut(task_id, |task| {
        let removed = task.take_lock_handoff(lock_id, token).is_some();
        (clear_lock_side.take().expect("lock clear callback"))();
        removed
    });
    if removed.is_none() {
        (clear_lock_side.take().expect("lock clear callback"))();
    }
    removed.unwrap_or(false)
}

/// Take `task_id`'s lock registration, whatever phase it is in. The caller
/// reconciles it at the lock afterwards, with no shard held.
pub(crate) fn take_lock_wait_link(task_id: RuntimeTaskId) -> Option<LockWaitLink> {
    global_task_table()
        .with_mut(task_id, RuntimeTask::take_lock_wait)
        .flatten()
}

/// Drop `task_id`'s registration only if it is exactly `(lock_id, token)` in
/// `HandoffOwned` — the resumed frame consuming its reserved ownership.
pub(crate) fn consume_lock_handoff_link(
    task_id: RuntimeTaskId,
    lock_id: LockId,
    token: RegistrationToken,
) -> bool {
    global_task_table()
        .with_mut(task_id, |task| {
            task.take_lock_handoff(lock_id, token).is_some()
        })
        .unwrap_or(false)
}

/// `task_id`'s current registration, for cleanup paths and diagnostics.
pub(crate) fn lock_wait_link(task_id: RuntimeTaskId) -> Option<LockWaitLink> {
    global_task_table()
        .with(task_id, RuntimeTask::lock_wait)
        .flatten()
}

#[cfg(test)]
pub(crate) fn task_waiter_count_for_test(task_id: RuntimeTaskId) -> usize {
    global_task_table()
        .with(task_id, RuntimeTask::waiter_count)
        .unwrap_or(0)
}

/// Wake a task that was just handed a lock. Deliberately the ordinary wake path:
/// the lock runtime does not resolve the wake/park race itself, it relies on the
/// task state word (a wake landing during a poll is recorded and consumed at the
/// poll boundary rather than lost).
pub(crate) fn wake_lock_waiter(task_id: RuntimeTaskId) -> WakeOutcome {
    crate::gc::stress_collect("scheduler");
    let outcome = wake_global_task_outcome(task_id);
    crate::gc::stress_collect("scheduler");
    outcome
}

/// True (1) if `id` was cancel-requested or already finalized as Cancelled.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_is_cancelled(id: u64) -> i64 {
    let id = id as RuntimeTaskId;
    global_task_table()
        .with(id, |task| {
            let state = task.state.load();
            state.cancel_requested() || state.lifecycle() == TaskLifecycle::Cancelling
        })
        .unwrap_or(false) as i64
}

/// Post-await fallback for a null frame: a CANCELLED task has
/// no result to read, so it is a located runtime panic.
pub(super) fn sched_await_check(id: u64) {
    // The lifecycle collapses every terminal outcome into `Terminal`, so the
    // id-only path cannot tell Panicked from Completed. The gate answers the
    // question that matters: an unhandled panic is being published, and no
    // awaiter may resume before the process ends.
    if fatal_panic_pending(crate::runtime::current()) {
        park_until_fatal_exit();
    }
    let id = id as RuntimeTaskId;
    let cancelled = global_task_table()
        .with(id, |task| {
            task.state.lifecycle() == TaskLifecycle::Cancelling
        })
        .unwrap_or(false);
    if cancelled {
        report_await_of_cancelled_task(id);
    }
}

/// The recoverable language fault shared by the id-only and frame-backed await
/// checks. Cancellation itself remains an ordinary terminal task state;
/// strict `await` is the operation that turns it into a panic.
pub(super) fn report_await_of_cancelled_task(id: u64) {
    crate::panic_context::raise_language_message(&format!("awaited a cancelled task (task {id})"));
}

/// A poll ABI violation cannot be treated as an ordinary Pending task: no
/// event is guaranteed to wake it. Publish Panicked/reap first, then terminate
/// the process according to Willow's current non-recoverable panic policy.
pub(super) fn report_poll_failure(id: u64, invalid_result: Option<i32>, async_chain: &str) -> ! {
    match invalid_result {
        Some(result) => {
            eprintln!("runtime panic: task {id} returned invalid poll status {result}");
        }
        None => {
            eprintln!("runtime panic: task {id} returned panicked poll status");
        }
    }
    crate::stack_trace::print_current_call_stack();
    if !async_chain.is_empty() {
        eprintln!("{async_chain}");
    }
    std::process::abort();
}

/// Attach the compiler-generated cancellation cleanup entry to a task. Called by the async-fn
/// constructor right after spawn.
///
/// # Safety
/// The callback must accept the frame registered for this task and remain callable through
/// cancellation.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_sched_set_cancel_fn(id: u64, cancel: RuntimeCancelFn) {
    let id = id as RuntimeTaskId;
    global_task_table().with_mut(id, |task| {
        task.cancel = Some(cancel);
    });
}

/// Attach cooperative cleanup with its compiler-proven native stack requirement.
///
/// # Safety
/// The callback must accept the frame registered for this task and remain callable through
/// cancellation.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_sched_set_cancel_fn_cooperative(
    id: u64,
    cancel: RuntimeCancelFn,
    needs_stack: i32,
) {
    global_task_table().with_mut(id, |task| {
        task.cancel = Some(cancel);
        task.native_cancel_cleanup = needs_stack != 0;
    });
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_current_task() -> u64 {
    current_task_id().unwrap_or(0)
}

/// Tag the currently-running task with its async fn name (raw static UTF-8 bytes
/// plus length, which must remain valid for the process lifetime). Emitted at
/// the top of each async poll fn so a panic can render the async chain. No-op when no task is
/// running.
///
/// # Safety
/// For each non-null pointer with a positive length, provide that many initialized immutable
/// bytes in a single allocation at a stable address for the lifetime of the process.
/// Diagnostics retain the address.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_sched_tag_current_task(name: *const u8, name_len: i64) {
    if name.is_null() || name_len <= 0 {
        return;
    }
    // Every poll entry tags, so this only publishes thread-locally; a task
    // copies its name into its record when it registers as an awaiter.
    if let Some(id) = current_task_id() {
        // SAFETY: generated code passes its static string data.
        unsafe { crate::stack_trace::tag_async_task(id, name, name_len) };
    }
}

/// Record an explicit panic location for the current task (cold path).
pub(crate) fn set_current_execution_site(frame: crate::stack_trace::RuntimeFrame) {
    if let Some(id) = current_task_id() {
        global_task_table().with_mut(id, |task| {
            task.set_execution_site(crate::stack_trace::ExecutionSite::Panic(frame))
        });
    }
}

/// Record that `id` holds a netpoll registration, so its terminal transition
/// schedules the purge. Tasks that never touch netpoll skip that lock.
pub(crate) fn note_netpoll_registration(id: RuntimeTaskId) {
    global_task_table().with_mut(id, |task| task.netpoll_registered = true);
}

pub(crate) fn current_task() -> Option<RuntimeTaskId> {
    current_task_id()
}

/// A bounded lock spin can make progress only if another worker can execute
/// its owner. Off-task callers and single-worker drives must park immediately.
pub(crate) fn lock_spinning_allowed(task: RuntimeTaskId) -> bool {
    current_task_id() == Some(task) && global_run_queues().worker_count() > 1
}

/// Render the active async chain for panic diagnostics.
pub fn async_chain_text() -> String {
    let current = current_task_id();
    let Some(mut id) = current else {
        return String::new();
    };
    let mut lines = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Walk current task -> its awaiter -> ... via the reverse `waiters` link.
    while seen.insert(id) {
        let Some((line, awaiter)) = global_task_table().with(id, |task| {
            // The running task's name and latest site stay thread-local;
            // suspended awaiters carry the copies made when they registered.
            let (published_name, published_site) = if Some(id) == current {
                crate::stack_trace::async_trace_of(id)
            } else {
                (None, None)
            };
            let name = published_name
                .as_deref()
                .or(task.name())
                .unwrap_or("<async task>");
            let location = match published_site {
                Some(site) if crate::stack_trace::async_trace_calls(id) => format!(
                    "{} (in a call made here)",
                    crate::stack_trace::ExecutionSite::Published(site).location_text()
                ),
                Some(site) => crate::stack_trace::ExecutionSite::Published(site).location_text(),
                None => task
                    .execution_site()
                    .map_or_else(String::new, |site| site.location_text()),
            };
            let site = match task.spawn_site() {
                Some((file, line)) => format!(" [task {id}, spawned at {file}:{line}]"),
                None => format!(" [task {id}]"),
            };
            // Only inspect the first live waiter; do not copy the task's
            // other relationships while rendering its diagnostic line.
            (
                format!("  {}: async {}{}{}", lines.len(), name, location, site),
                task.first_live_waiter(),
            )
        }) else {
            break;
        };
        lines.push(line);
        match awaiter {
            Some(awaiter) => id = awaiter,
            None => break,
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = String::from("async stack (current task first):");
    for line in lines {
        out.push('\n');
        out.push_str(&line);
    }
    out
}

/// Requested worker count from `WILLOW_WORKERS`, or available parallelism.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_requested_workers() -> u64 {
    runtime_worker_config().requested_workers() as u64
}

/// Worker count the current runtime will run. Any positive `WILLOW_WORKERS`
/// value is honored; invalid values use available parallelism (fallback: one).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_active_workers() -> u64 {
    runtime_worker_config().active_workers() as u64
}

/// Deterministic scheduler ownership counters for diagnostics and plateau
/// tests. They intentionally do not claim to measure RSS.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_heavy_task_count() -> u64 {
    with_global(|sched| sched.metadata_snapshot().heavy_tasks as u64)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_queue_entry_count() -> u64 {
    with_global(|sched| sched.metadata_snapshot().queue_entries as u64)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_pending_cleanup_count() -> u64 {
    with_global(|sched| sched.metadata_snapshot().pending_cleanups as u64)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_frame_root_count() -> u64 {
    with_global(|sched| sched.metadata_snapshot().frame_roots as u64)
}

/// Register a wake-deadline on the currently-running task: after the poll fn
/// returns Pending, the timer-aware run loop wakes it once `millis` elapse.
/// Called by a cooperative poll fn that is awaiting a sleep.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_sleep(millis: i64) {
    set_global_wake_after_millis(millis);
    crate::gc::stress_collect("await");
}

/// Cooperatively yield the currently-running task. The compiler emits this from
/// `await yield()` immediately before returning Pending from the poll fn.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_yield() {
    if let Some(id) = current_task_id() {
        global_task_table().with_mut(id, |task| task.yield_requested = true);
    }
    crate::gc::stress_collect("await");
}

/// Await another task's completion (for `await <task>`): returns 1 if `awaitee`
/// has already completed (the caller may read its result and continue), else
/// registers the currently-running task as a waiter and returns 0 — the caller
/// then returns Pending and is woken when `awaitee` completes.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_await(awaitee: u64) -> i32 {
    let tasks = global_task_table();
    let ready = match current_task_id() {
        Some(waiter) if register_waiter_sharded(tasks, awaitee, waiter) => 0,
        Some(_) => 1,
        None => i32::from(tasks.with(awaitee, |_| ()).is_none()),
    };
    crate::gc::stress_collect("await");
    ready
}

/// Frame-backed `await <task>`.
///
/// Same contract as [`willow_sched_await`], but the terminal check is an
/// `Acquire` load of the awaitee's frame-header status instead of a scheduler
/// table lookup: a finished task is observed without taking the global
/// scheduler lock, and the answer no longer depends on the heavy `RuntimeTask`
/// record still being retained. The scheduler is consulted only on the slow
/// path, to register the waiter.
///
/// `frame` null falls back to the id-only path so a synthetic/placeholder task
/// still behaves.
///
/// # Safety
/// A non-null frame must remain live through the call and provide the initialized, naturally
/// aligned async-frame status word for atomic access.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_frame_await(frame: *mut c_void, awaitee: u64) -> i32 {
    if frame.is_null() {
        return willow_sched_await(awaitee);
    }
    // SAFETY: The frame owner retains the initialized ABI status word through this atomic
    // access.
    if unsafe { crate::async_frame::frame_is_terminal(frame) } {
        crate::gc::stress_collect("await");
        return 1;
    }
    willow_sched_await(awaitee)
}

/// Frame-backed post-await check: the cancelled test is a
/// header load, and the id is carried only for the diagnostic message.
///
/// # Safety
/// A non-null frame must remain live through the call and provide the initialized, naturally
/// aligned async-frame status word for atomic access.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_frame_await_check(frame: *mut c_void, id: u64) {
    if frame.is_null() {
        sched_await_check(id);
        return;
    }
    if fatal_panic_pending(crate::runtime::current()) {
        // Awaiting a task whose panic escaped is not recoverable and has no
        // result to read: the process is ending, so this task stops here
        // instead of running its continuation. The gate closes before the
        // PANICKED status is published, so a woken awaiter always observes it. Frame status
        // alone is NOT the trigger: a PANICKED
        // frame with no fatal exit in flight (a recovered panic republished by a
        // test harness) must still return normally.
        park_until_fatal_exit();
    }
    // SAFETY: The frame owner retains the initialized ABI status word through this atomic
    // access.
    let status = unsafe { crate::async_frame::frame_terminal_status(frame) };
    if status == crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED {
        report_await_of_cancelled_task(id);
    }
}

/// Milliseconds since process start (monotonic), for select timeout cases:
/// the deadline is fixed once at select entry and re-checked on every
/// (re-)probe.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_monotonic_millis() -> i64 {
    #[cfg(test)]
    if let Some(elapsed) = crate::test_sync::scheduler_elapsed() {
        return elapsed.as_millis().min(i64::MAX as u128) as i64;
    }
    #[cfg(feature = "deterministic-test-clock")]
    {
        crate::test_clock::elapsed()
            .as_millis()
            .min(i64::MAX as u128) as i64
    }
    #[cfg(not(feature = "deterministic-test-clock"))]
    pub(super) static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    #[cfg(not(feature = "deterministic-test-clock"))]
    {
        crate::timer::scheduler_now()
            .saturating_duration_since(*START)
            .as_millis() as i64
    }
}

/// Sleep the CALLING OS THREAD until the monotonic deadline (sync select's
/// timeout wait when nothing else can progress; select deadline handling). No-op if the
/// deadline already passed.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sleep_until_monotonic(deadline_ms: i64) {
    let now = willow_monotonic_millis();
    if deadline_ms > now {
        let remaining = Duration::from_millis((deadline_ms - now) as u64);
        // SAFETY: the sleep touches no managed state.
        unsafe { crate::gc::run_blocking(|| std::thread::sleep(remaining)) };
    }
}

/// Remove the currently-running task from `awaitee`'s waiter list — a select
/// that registered on a task-completion case must unregister when another
/// case wins, exactly like channel waiters.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_unregister_task_waiter(awaitee: u64) {
    let Some(current) = current_task_id() else {
        return;
    };
    unregister_waiter_sharded(global_task_table(), awaitee, current);
}

/// Current state of a task as an integer: 0 ready, 1 running, 2 parked,
/// 3 completed, 4 panicked, 5 cancelled, 6 cancelling, 7 blocked-syscall,
/// -1 unknown.
///
/// This is the ID-ONLY query, and it is answered from the scheduler's task
/// table. It is therefore a DIAGNOSTIC: once a finished task is reaped the
/// record is gone and this returns -1 (Unknown), with no tombstone kept. Language-visible
/// questions about a task that the caller
/// holds a handle to must use the frame-backed queries — `willow_frame_status`,
/// `willow_frame_await`, `willow_frame_await_check`, `willow_frame_is_cancelled`
/// — whose answer is stored in the frame the handle already points at and so
/// survives reaping.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_task_state(id: u64) -> i32 {
    match global_task_table().with(id, |task| task.state.lifecycle()) {
        Some(TaskLifecycle::Ready) => 0,
        Some(TaskLifecycle::Running) => 1,
        Some(TaskLifecycle::Parked) => 2,
        Some(TaskLifecycle::Cancelling) => 6,
        Some(TaskLifecycle::BlockedSyscall) => 7,
        // Terminal records are removed immediately and the atomic lifecycle
        // intentionally does not duplicate Completed/Cancelled/Panicked. The
        // frame status is the authoritative terminal diagnostic.
        Some(TaskLifecycle::Terminal) | None => -1,
    }
}
