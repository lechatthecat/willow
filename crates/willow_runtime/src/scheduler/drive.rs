use super::*;

// Drive the global scheduler until no task is ready (idle). Each ready task is
// polled once: `Ready` completes it (and unroots its frame); `Pending` parks it
// (a waker must later re-queue it). Returns the number of tasks completed.
//
// The poll function is invoked with no scheduler borrow held, so a task may
// re-enter the scheduler (spawn/wake) from inside its own poll.
thread_local! {
    /// Re-entrancy depth of `willow_sched_run` on this thread. `await` block-runs
    /// the scheduler recursively, so the driver registers as a GC mutator on the
    /// OUTERMOST entry and unregisters on the matching exit.
    pub(super) static SCHED_RUN_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_run() -> i64 {
    sched_run_with_mutator(None, None)
}

/// Drive the scheduler only until `target` completes (or the scheduler goes
/// genuinely idle), then return — the `await` of a concrete task
/// handle. Reuses the mutator-registration wrapper so GC
/// coordination is identical to `willow_sched_run`.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_run_until(target: u64) -> i64 {
    let target = target as RuntimeTaskId;
    let mut completed = sched_run_with_mutator(Some(target), None);
    // An unbounded drive that returns while its target is still RUNNABLE has
    // not honoured its contract: the caller (`await handle`, and the program
    // entry point that drives `main`) reads the target's result from its frame
    // and would observe an uninitialized one. The scheduler stops the worker
    // pool on an idleness snapshot, and any residual race in that snapshot
    // strands exactly this way, so treat an early return as a signal to drive
    // again rather than as an answer. `scheduler_has_wake_source` is false once
    // the target is genuinely blocked forever, so this cannot spin: a real
    // deadlock still returns and the awaiter reports it.
    while !target_is_done(Some(target)) && scheduler_has_wake_source() {
        completed += sched_run_with_mutator(Some(target), None);
    }
    completed
}

/// Program-entry drive: unlike an idle probe, an unfinished main cannot be
/// returned to its caller as a successful program exit. `run_until` has waited
/// for the outer workers and exhausted timers, I/O and blocking completions
/// before returning an incomplete target. Keep ordinary idle/deadline drives
/// unchanged: their callers may supply work after they return.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_run_main(target: u64) -> i64 {
    let completed = willow_sched_run_until(target);
    if let Some(message) = deadlock::main_witness(global_task_table(), target) {
        crate::failure::deadlock(format_args!("{message}"));
    }
    completed
}

/// Drive the scheduler, but no longer than until the absolute monotonic
/// deadline `deadline_ms` (the `willow_monotonic_millis` base).
///
/// A sync `select` with a `sleep(ms)` case owns a deadline that belongs to no
/// task, so an unbounded `willow_sched_run()` could sit inside a five-second
/// task before ever re-checking a thirty-millisecond timeout. This variant
/// stops the run loop once the deadline passes and clamps every idle wait to
/// it, so the caller always gets its turn back in time.
///
/// Like `willow_sched_run`, it returns as soon as the scheduler has nothing
/// left to do — the caller then decides whether to wait out the deadline.
/// Returns the number of tasks completed.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_run_until_deadline(deadline_ms: i64) -> i64 {
    let remaining = (deadline_ms - willow_monotonic_millis()).max(0) as u64;
    let deadline = crate::timer::scheduler_now() + Duration::from_millis(remaining);
    sched_run_with_mutator(None, Some(deadline))
}

/// True while some source can still make a parked or blocked task runnable:
/// queued work, a claim in flight, another actively polling task, an armed
/// timer, a parked netpoll waiter, or an outstanding blocking-pool syscall.
///
/// A drive returning "no task completed" is NOT the same as "nothing can ever
/// happen": the drive may simply have raced a claim, or stopped on its own
/// deadline. Callers that turn quiescence into a blocking-forever diagnostic
/// must consult this first.
pub(crate) fn scheduler_has_wake_source() -> bool {
    scheduler_has_wake_source_with_peer_probe(|| {
        CURRENT_RUN_STATE.with(|slot| {
            slot.borrow().as_ref().is_some_and(|state| {
                // The calling task may itself be blocked in a synchronous operation.
                let caller = usize::from(current_task_id().is_some());
                state.active_polls() > caller
            })
        })
    })
}

pub(super) fn scheduler_has_wake_source_with_peer_probe(peer_poll: impl FnOnce() -> bool) -> bool {
    let claims = global_claim_words();
    let before = claims.snapshot();
    // Match IDLE_READ_ORDER: a poll publishes its continuation before ending,
    // so inspect polls before queues. The surrounding claim epochs cover a
    // poll starting or moving across accounting shards during this scan.
    peer_poll()
        || global_next_timer_deadline().is_some()
        || global_task_table().blocked_syscall_count() > 0
        || global_run_queues().len() > 0
        || claims.changed_or_in_flight(&before)
        || crate::netpoll::has_waiters()
}

/// Report drive quiescence without diagnosing select failure. A peer may have
/// published a ready case before becoming quiescent, so generated code must
/// re-probe its cases after this returns true and before its final idle step.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_select_drive_is_quiescent() -> i8 {
    i8::from(!scheduler_has_wake_source())
}

/// Wait for an idle select's absolute deadline or a newly published event.
/// Generated code has already re-probed after its scheduler drive. A source
/// appearing since the quiescence observation requires another drive instead
/// of sleeping through a channel or task becoming ready.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_select_wait_until(deadline_ms: i64) {
    let generation = current_wake_generation();
    if scheduler_has_wake_source() {
        return;
    }
    let remaining = deadline_ms.saturating_sub(willow_monotonic_millis());
    if remaining > 0 {
        wait_for_wake_since(generation, Duration::from_millis(remaining as u64));
    }
}

/// Park a synchronous caller briefly while it waits for a wake source to fire.
/// Bounded so a missed notification degrades into a re-probe rather than a
/// hang, and cut short by the next wake.
pub(crate) fn wait_for_any_wake_briefly() {
    let generation = current_wake_generation();
    wait_for_wake_since(generation, Duration::from_millis(1));
}

/// Idle step for a `select` with no `default` and no timeout case.
///
/// Such a select must block until one of its cases becomes ready: falling
/// through would run no case at all and silently continue past the select. The
/// synchronous lowering re-probes in a loop and calls this whenever a scheduler
/// drive reported no progress. While any wake source is live this waits briefly
/// and returns, so the caller re-probes. When nothing can make a case ready the
/// select would block forever, so it raises a language panic instead of
/// spinning — the same policy sync channel `recv`/`send` use for a blocking
/// operation with no counterpart.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_select_idle_wait() {
    if scheduler_has_wake_source() {
        wait_for_any_wake_briefly();
        return;
    }
    // The caller re-probed all cases after observing drive quiescence. No
    // elapsed grace period can substitute for that readiness observation.
    crate::panic_context::raise_language_message(
        "select would block forever: no case can become ready and there is no default case",
    );
}

/// Restore drive-local bookkeeping on both return and Rust unwind. Frame-root
/// retirement stays on the successful path: an unwinding pool may not be quiescent.
pub(super) struct SchedulerDriveGuard {
    pub(super) saved_depth: u32,
    pub(super) saved_running: Option<RuntimeTaskId>,
    pub(super) saved_execution: TaskExecution,
    pub(super) saved_panic_context: Option<Arc<crate::panic_context::PanicContext>>,
    pub(super) shared_state: Option<Arc<ParallelRunState>>,
    pub(super) paused_parallel_poll: bool,
    pub(super) mutator: Option<crate::gc::MutatorRegistration<'static>>,
}

impl SchedulerDriveGuard {
    pub(super) fn new() -> Self {
        let saved_depth = SCHED_RUN_DEPTH.with(Cell::get);
        let saved_running = current_task_id();
        let saved_execution = CURRENT_EXECUTION.with(|slot| slot.take());
        let saved_panic_context = crate::panic_context::current_context();
        let shared_state = CURRENT_RUN_STATE.with(|slot| slot.borrow().clone());
        let mutator = (saved_depth == 0).then(crate::gc::MutatorRegistration::new);
        let paused_parallel_poll =
            saved_depth != 0 && shared_state.is_some() && saved_running.is_some();
        SCHED_RUN_DEPTH.with(|depth| depth.set(saved_depth + 1));
        if paused_parallel_poll && let Some(state) = shared_state.as_ref() {
            state.pause_active_poll();
        }
        Self {
            saved_depth,
            saved_running,
            saved_execution,
            saved_panic_context,
            shared_state,
            paused_parallel_poll,
            mutator,
        }
    }
}

impl Drop for SchedulerDriveGuard {
    fn drop(&mut self) {
        install_current_task(self.saved_running);
        CURRENT_EXECUTION.with(|slot| slot.replace(std::mem::take(&mut self.saved_execution)));
        crate::panic_context::replace_current_context(self.saved_panic_context.take());
        if let Some(id) = self.saved_running {
            // The outer task still owns its frame and lifecycle; only its TLS
            // marker and preemption budget were displaced by the nested drive.
            if let Some(flag) = global_task_table().with(id, RuntimeTask::preempt_flag_ptr) {
                // SAFETY: The running task owns its stable flag until the poll or cleanup
                // quantum is unbound, including nested drives.
                unsafe { crate::preempt::willow_preempt_begin(flag) };
            }
        }
        if self.paused_parallel_poll
            && let Some(state) = self.shared_state.as_ref()
        {
            state.resume_paused_poll();
        }
        SCHED_RUN_DEPTH.with(|depth| depth.set(self.saved_depth));
        let outermost_exit = self.saved_depth == 0;
        debug_assert_eq!(outermost_exit, self.mutator.is_some());
        // The mutator field unregisters after all bookkeeping is restored.
    }
}

pub(super) fn sched_run_with_mutator(
    target: Option<RuntimeTaskId>,
    deadline: Option<Instant>,
) -> i64 {
    crate::gc::scheduler_start(runtime_worker_config().active_workers());
    crate::stack_overflow::protect_current_thread();
    if crate::panic_context::panic_unwind_cleanup_active() {
        crate::panic_context::fatal_invariant(
            "scheduler re-entry attempted from panic-unwinding defer",
        );
    }
    // Register the driver thread as a GC mutator while it drives tasks so a
    // future parallel collector can stop it at a safepoint. Single-mutator runs
    // have exactly one registered thread, so `multi_mutator_active()` stays false
    // and GC behavior is unchanged.
    let drive = SchedulerDriveGuard::new();
    crate::observation::SchedulerHooks::drive(crate::runtime::current);
    let outermost = drive.mutator.is_some();
    let shared_state = drive.shared_state.as_deref();
    let active_workers = runtime_worker_config().active_workers();
    let completed = if outermost {
        willow_sched_run_parallel(target, active_workers, deadline)
    } else if let Some(state) = shared_state {
        scheduler_run_loop(target, current_worker(), Some(state), false, deadline)
    } else {
        scheduler_run_loop(target, current_worker(), None, false, deadline)
    };
    // External registrations never require the worker pool to remain alive,
    // so purge them promptly after each drive, including nested drives.
    drain_terminal_cleanups();
    if outermost {
        // The parallel pool has joined, so its workers' units have ended.
        // Release everything no other thread's unit still covers, including
        // the sub-threshold remainder the run loop never scans for.
        release_quiescent_frame_roots(true);
    }
    drop(drive);
    completed
}

/// Purge captured external registrations of terminal tasks. Runs on every
/// run-loop iteration, so the empty case must not take `GLOBAL_SCHEDULER`: the flag is set
/// under the mutex after the push, and a
/// push seen by no drain yet keeps it raised until some drain takes the list,
/// at the latest the drive-end drain, which runs after the workers have joined.
///
/// One caller takes a burst: the swap hands the list to its winner, and a push
/// after the swap raises the flag again. The winner can be a worker with no
/// active poll, and a purge can publish work (re-handing a lock reservation
/// wakes the next waiter), so the drain holds a [`ClaimInFlight`] marker that
/// resolves after its purges, entered BEFORE the swap. A caller that loses
/// the swap then returns only after the winner's marker is in the claim word,
/// so a stop decision that follows the loser's poll sees the drain as a claim
/// in flight, or its epoch advance, and refuses.
pub(super) fn drain_terminal_cleanups() {
    drain_terminal_cleanups_in(crate::runtime::current(), global_components());
}

pub(super) fn drain_terminal_cleanups_in(
    owner: &crate::runtime::Runtime,
    components: &SchedulerComponents,
) {
    let retirements = &components.retirements;
    let pending = &retirements.cleanups_pending;
    if !pending.load(Ordering::Acquire) {
        return;
    }
    let mut in_flight = ClaimInFlight::enter_in(&components.claims);
    if !pending.swap(false, Ordering::AcqRel) {
        return;
    }
    in_flight.popped();
    let cleanups = retirements.take_cleanups();
    for cleanup in cleanups {
        owner.netpoll.purge_task(cleanup.task_id);
        crate::channel::purge_task_from_tokens(cleanup.task_id, cleanup.channel_waits);
        // A task that dies while queued on (or holding a reservation for) a
        // scheduler-aware lock must not strand it: phase-driven cleanup removes
        // a `Waiting` entry, and re-hands ownership that was already reserved
        // for this task (scheduler-aware mutex support, spec §12.3).
        crate::async_mutex::purge_captured_task_lock_wait(cleanup.task_id, cleanup.lock_wait);
    }
}

/// Unroot the retired frames whose grace period has ended.
/// Unless `force`d, runs only once a retirement raised `TerminalRetirements::frame_release_due`,
/// so the run loop's empty case takes no lock. The run loop calls outside its
/// execution unit. Explicit GC may call inside a unit: that conservatively
/// keeps covered retirements while allowing older, quiescent ones to progress.
pub(crate) fn release_quiescent_frame_roots(force: bool) {
    let owner = crate::runtime::current();
    release_quiescent_frame_roots_in(owner, global_components(), force);
}

pub(super) fn release_quiescent_frame_roots_in(
    owner: &crate::runtime::Runtime,
    components: &SchedulerComponents,
    force: bool,
) {
    let retirements = &components.retirements;
    let due = retirements.frame_release_due.load(Ordering::Acquire)
        && retirements.frame_release_due.swap(false, Ordering::AcqRel);
    if !due && !force {
        return;
    }
    let bound = retirements.reclaim.release_bound();
    let frames = retirements.take_quiescent_frames(bound, &components.frame_roots);
    #[cfg(test)]
    retirements
        .released_below
        .fetch_max(bound, Ordering::SeqCst);
    for retired in frames {
        crate::gc::remove_runtime_root_in(owner, retired.frame as *mut u8);
        owner.frame_lifetime.record_with_sweep(
            crate::frame_lifetime::FrameEventKind::SchedulerRootReleased,
            retired.frame,
            retired.tag,
            0,
            0,
        );
    }
}

#[derive(Debug)]
pub(super) struct ParallelRunState {
    pub(super) parking: Arc<parking::Parking>,
    pub(super) claims: Arc<AccountingWords>,
    pub(super) stop: AtomicBool,
    /// Serializes stop decisions. A claim takes it only while
    /// `stop_intent` is raised, so the claim hot path writes no shared lock
    /// word.
    pub(super) claim_gate: Mutex<()>,
    /// Raised by a stop decision, under `claim_gate`, for the whole snapshot.
    /// With the scheduler’s claim words it forms a store/load handshake: a claim increments
    /// the word and then reads this flag, a decision raises this flag and then
    /// reads the word, all `SeqCst`. Either the decision sees the claim, or
    /// the claim sees the flag and waits on `claim_gate` for the decision.
    pub(super) stop_intent: AtomicBool,
    /// Thread-sharded polls of this run: each word's low bits count running
    /// polls, its high bits count polls paused inside a nested drive. A poll
    /// starts, pauses, resumes and ends on the same OS thread, even if its task
    /// migrates between polls. Each transfer is one atomic operation.
    pub(super) polls: AccountingWords,
    /// The pool's shared view of the drive target, used by the pool workers
    /// (`stop_pool_on_exit`) only. A nested drive has its own target.
    pub(super) target: SharedTargetCheck,
    pub(super) completed: AtomicI64,
}

/// One look at the drive target per terminal epoch for the whole pool. Every completion
/// advances the epoch, and without this
/// every worker would lock the target's shard after every completion.
#[derive(Debug, Default)]
pub(super) struct SharedTargetCheck {
    /// One past the highest epoch some worker has claimed to check; 0 before
    /// any check.
    pub(super) checked: AtomicU64,
    /// Set once a check found the target done. Final.
    pub(super) done: AtomicBool,
}

pub(super) const POLL_ACTIVE_BITS: u32 = 32;
pub(super) const POLL_ACTIVE_MASK: u64 = (1 << POLL_ACTIVE_BITS) - 1;
/// Adding this moves one poll from active to paused; subtracting it moves one
/// back.
pub(super) const POLL_PAUSE: u64 = (1 << POLL_ACTIVE_BITS) - 1;

impl Default for ParallelRunState {
    fn default() -> Self {
        Self::new(Arc::clone(global_claim_words()))
    }
}

impl ParallelRunState {
    pub(super) fn new(claims: Arc<AccountingWords>) -> Self {
        Self::new_in(claims, Arc::clone(&crate::runtime::current().parking))
    }

    pub(super) fn new_in(claims: Arc<AccountingWords>, parking: Arc<parking::Parking>) -> Self {
        Self {
            parking,
            claims,
            stop: AtomicBool::new(false),
            claim_gate: Mutex::new(()),
            stop_intent: AtomicBool::new(false),
            polls: AccountingWords::new(),
            target: SharedTargetCheck::default(),
            completed: AtomicI64::new(0),
        }
    }

    /// Publish `stop` if `quiescent` holds with every claim excluded. Returns
    /// whether the stop was published.
    ///
    /// Claims do not take `claim_gate` unless `stop_intent` is raised, so the
    /// decision excludes them through the handshake instead: raise the intent,
    /// then require that no claim is in flight and that none entered, popped
    /// and resolved while `quiescent` was evaluated. A claim that entered
    /// before the intent was raised is in the word; one that entered after it
    /// reads the intent and queues on `claim_gate` behind this decision, then
    /// observes the stop. The stop is published only once it is final, and before the intent
    /// is lowered, so a claim that reads
    /// the lowered intent also reads the stop.
    pub(super) fn publish_stop_if(&self, quiescent: impl FnOnce(&Self) -> bool) -> bool {
        let _gate = self
            .claim_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.stop_intent.store(true, Ordering::SeqCst);
        // A shard is read in both passes. A claim resolving between its reads
        // changes its epoch; a claim entering after its first read sees the
        // raised intent before claiming. Thus no coherent all-shard instant
        // is needed. Never sum epochs here: compare each shard independently.
        let before = self.claims.snapshot();
        let stop = before.iter().all(|word| word & CLAIM_COUNT_MASK == 0)
            && quiescent(self)
            && self.claims.snapshot() == before;
        if stop {
            self.stop.store(true, Ordering::SeqCst);
            self.parking.notify_all();
        }
        self.stop_intent.store(false, Ordering::SeqCst);
        stop
    }

    pub(super) fn active_polls(&self) -> usize {
        self.polls
            .0
            .iter()
            .map(|word| (word.0.load(Ordering::Acquire) & POLL_ACTIVE_MASK) as usize)
            .sum()
    }

    /// Unlike a diagnostic sum, a liveness decision must not miss an active
    /// poll handed between shards during its scan. A new poll is published
    /// under a claim; pause/resume transfers use the same epoch protocol.
    /// Retain these epochs through the count scan and retry on a change.
    /// The caller's own active poll is excluded for deadlock checks.
    #[cfg(test)]
    pub(super) fn has_active_peer(&self, caller: usize) -> bool {
        self.has_active_peer_during(caller, || self.active_polls())
    }

    #[cfg(test)]
    pub(super) fn has_active_peer_during(
        &self,
        caller: usize,
        scan: impl FnOnce() -> usize,
    ) -> bool {
        let before = self.claims.snapshot();
        scan() > caller || self.claims.changed_or_in_flight(&before)
    }

    pub(super) fn paused_polls(&self) -> usize {
        self.polls
            .0
            .iter()
            .map(|word| (word.0.load(Ordering::Acquire) >> POLL_ACTIVE_BITS) as usize)
            .sum()
    }

    /// Is a poll running, or paused inside a nested drive?
    pub(super) fn has_live_poll(&self) -> bool {
        self.polls
            .0
            .iter()
            .any(|word| word.0.load(Ordering::Acquire) != 0)
    }

    pub(super) fn begin_poll(&self) {
        self.polls.local().fetch_add(1, Ordering::AcqRel);
    }

    pub(super) fn end_poll(&self) {
        let previous = self.polls.local().fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            previous & POLL_ACTIVE_MASK > 0,
            "parallel poll depth underflow"
        );
    }

    /// Move the current poll from active to paused for a nested drive.
    pub(super) fn pause_active_poll(&self) {
        let _transition = ClaimInFlight::poll_transition(&self.claims);
        let previous = self.polls.local().fetch_add(POLL_PAUSE, Ordering::AcqRel);
        debug_assert!(
            previous & POLL_ACTIVE_MASK > 0,
            "parallel poll depth underflow"
        );
    }

    pub(super) fn resume_paused_poll(&self) {
        let _transition = ClaimInFlight::poll_transition(&self.claims);
        let previous = self.polls.local().fetch_sub(POLL_PAUSE, Ordering::AcqRel);
        debug_assert!(
            previous >> POLL_ACTIVE_BITS > 0,
            "parallel paused poll underflow"
        );
    }

    #[cfg(test)]
    pub(super) fn set_polls_for_test(&self, active: usize, paused: usize) {
        self.polls.local().store(
            ((paused as u64) << POLL_ACTIVE_BITS) | active as u64,
            Ordering::Release,
        );
    }

    /// A stop decision is in progress; the claim must wait for it.
    pub(super) fn stop_decision_pending(&self) -> bool {
        self.stop_intent.load(Ordering::SeqCst)
    }
}

pub(super) struct WorkerDrive {
    pub(super) target: Option<RuntimeTaskId>,
    pub(super) state: Arc<ParallelRunState>,
    pub(super) deadline: Option<Instant>,
    pub(super) finished: Arc<ParallelCompletion>,
}

pub(super) fn persistent_worker_loop(
    worker: usize,
    receiver: std::sync::mpsc::Receiver<WorkerDrive>,
    run: impl FnMut(usize, Option<RuntimeTaskId>, Arc<ParallelRunState>, Option<Instant>),
) {
    // A Rust invariant panic cannot leave this thread: its driver is waiting
    // for completion, and the runtime may already be partially unwound. Willow
    // language panics use task outcomes and never unwind through this boundary.
    // The panic hook emits the original diagnostic before we terminate. Do not
    // drop the panic payload (its destructor can itself panic).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut run = run;
        while let Ok(drive) = receiver.recv() {
            run(worker, drive.target, drive.state, drive.deadline);
            drive.finished.finish();
        }
        // Captured service owners are part of the worker failure boundary too.
        drop(run);
    }));
    if result.is_err() {
        std::process::abort();
    }
}

/// The driver owns this pool; runners may own services, but not the pool itself.
/// Keeping that ownership acyclic lets teardown disconnect and join every worker.
#[derive(Default)]
pub(crate) struct PersistentWorkers {
    pub(super) senders: Vec<std::sync::mpsc::Sender<WorkerDrive>>,
    pub(super) handles: Vec<std::thread::JoinHandle<()>>,
}

impl PersistentWorkers {
    pub(crate) const fn new() -> Self {
        Self {
            senders: Vec::new(),
            handles: Vec::new(),
        }
    }

    /// The runner owns the services used by this worker. Production supplies
    /// the runtime driver; isolated fixtures can supply their own executor.
    /// A `serving` runtime is selected on the worker thread, so its implicit
    /// default paths and thread-local caches belong to that runtime.
    pub(super) fn start_worker(
        &mut self,
        serving: Option<&'static crate::runtime::Runtime>,
        run: impl FnMut(usize, Option<RuntimeTaskId>, Arc<ParallelRunState>, Option<Instant>)
        + Send
        + 'static,
    ) {
        let worker = self.senders.len();
        let (sender, receiver) = std::sync::mpsc::channel::<WorkerDrive>();
        let handle = std::thread::Builder::new()
            .name(format!("willow-worker-{worker}"))
            .spawn(move || {
                if let Some(serving) = serving {
                    serving.select_for_thread();
                }
                persistent_worker_loop(worker, receiver, run)
            })
            .unwrap_or_else(|_| {
                crate::failure::resource_exhausted(format_args!(
                    "cannot start persistent scheduler worker"
                ))
            });
        self.senders.push(sender);
        self.handles.push(handle);
    }
}

impl Drop for PersistentWorkers {
    fn drop(&mut self) {
        // Disconnect every inbox before joining any worker. Queued drives
        // finish in FIFO order; each worker then releases its captured owner.
        self.senders.clear();
        for handle in self.handles.drain(..) {
            // A queued drive can collect. A registered joining thread must
            // publish its roots rather than block that collection in join().
            while !handle.is_finished() {
                crate::gc::relocatable_safepoint();
                std::thread::yield_now();
            }
            handle.join().unwrap_or_else(|_| {
                crate::panic_context::fatal_invariant("persistent scheduler worker teardown failed")
            });
        }
    }
}

/// Set once by the build-wide static initializer before any user code runs,
/// on the thread that serves the program's runtime.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sched_require_single_worker_statics() {
    crate::runtime::current()
        .single_worker_statics
        .store(true, Ordering::Release);
}

pub(super) fn static_mut_workers_allowed(has_static_mut: bool, workers: usize) -> bool {
    !has_static_mut || workers <= 1
}

pub(super) fn willow_sched_run_parallel(
    target: Option<RuntimeTaskId>,
    workers: usize,
    deadline: Option<Instant>,
) -> i64 {
    sched_run_parallel_in(crate::runtime::current(), target, workers, deadline)
}

pub(super) fn sched_run_parallel_in(
    owner: &crate::runtime::Runtime,
    target: Option<RuntimeTaskId>,
    workers: usize,
    deadline: Option<Instant>,
) -> i64 {
    let _mutator = crate::gc::MutatorRegistration::new_in(owner);
    // Hold drive ownership until all workers finish. Wait cooperatively because
    // another driver may be collecting while this caller is a registered mutator.
    let mut pool = loop {
        match owner.worker_pool.try_lock() {
            Ok(pool) => break pool,
            Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                crate::gc::relocatable_safepoint_in(owner);
                std::thread::yield_now();
            }
        }
    };
    let workers = workers.max(1);
    let workers = workers.max(owner.affinities.required_workers());
    if !static_mut_workers_allowed(owner.single_worker_statics.load(Ordering::Acquire), workers) {
        crate::panic_context::fatal_invariant(
            "static mut requires single-worker execution; set WILLOW_WORKERS=1 or use immutable static Atomic/Mutex values",
        );
    }
    while pool.senders.len() < workers {
        if std::ptr::eq(owner, crate::runtime::current()) {
            pool.start_worker(Some(crate::runtime::current()), run_parallel_worker);
        } else {
            let weak = owner
                .service_owner
                .get()
                .expect("persistent workers require a shared runtime")
                .clone();
            pool.start_worker(None, move |worker, target, state, deadline| {
                // The synchronous drive keeps its owner alive until every
                // callback returns. Idle workers hold only a Weak, so the
                // runtime does not own a strong cycle through its own pool.
                let owner = weak.upgrade().expect("active drive retains its runtime");
                run_parallel_worker_in(&owner, worker, target, state, deadline);
            });
        }
    }
    let state = Arc::new(ParallelRunState::new_in(
        Arc::clone(&owner.scheduler().components().claims),
        Arc::clone(&owner.parking),
    ));
    let finished = Arc::new(ParallelCompletion::new_in(
        workers,
        Arc::clone(&owner.parking),
    ));
    for sender in pool.senders.iter().take(workers) {
        sender
            .send(WorkerDrive {
                target,
                state: Arc::clone(&state),
                deadline,
                finished: Arc::clone(&finished),
            })
            .expect("scheduler worker terminated");
    }
    finished.wait_in(owner);
    state.completed.load(Ordering::Acquire)
}

/// One completion counter per drive, independent of worker count. The waiter
/// samples the wake generation before the counter, so the final wake (a
/// generation bump) cannot be lost between the check and the park.
pub(super) struct ParallelCompletion {
    pub(super) parking: Arc<parking::Parking>,
    pub(super) remaining: AtomicUsize,
}

impl ParallelCompletion {
    #[cfg(test)]
    pub(super) fn new(workers: usize) -> Self {
        Self::new_in(workers, Arc::clone(&crate::runtime::current().parking))
    }

    pub(super) fn new_in(workers: usize, parking: Arc<parking::Parking>) -> Self {
        Self {
            parking,
            remaining: AtomicUsize::new(workers),
        }
    }

    pub(super) fn finish(&self) {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.parking.notify_all();
        }
    }

    #[cfg(test)]
    pub(super) fn wait(&self) {
        self.wait_in(crate::runtime::current());
    }

    pub(super) fn wait_in(&self, owner: &crate::runtime::Runtime) {
        loop {
            // The waiter publishes its roots for the whole idle interval, so
            // only completion needs to interrupt its park.
            let generation = self.parking.completion_generation();
            crate::gc::relocatable_safepoint_in(owner);
            if self.remaining.load(Ordering::SeqCst) == 0 {
                return;
            }
            self.parking.wait_completion(owner, generation);
        }
    }
}

pub(super) fn run_parallel_worker(
    worker: usize,
    target: Option<RuntimeTaskId>,
    state: Arc<ParallelRunState>,
    deadline: Option<Instant>,
) {
    run_parallel_worker_in(crate::runtime::current(), worker, target, state, deadline);
}

pub(super) fn run_parallel_worker_in(
    owner: &crate::runtime::Runtime,
    worker: usize,
    target: Option<RuntimeTaskId>,
    state: Arc<ParallelRunState>,
    deadline: Option<Instant>,
) {
    crate::stack_overflow::protect_current_thread();
    SCHED_RUN_DEPTH.with(|depth| depth.set(1));
    // An unwind out of the run loop must still unregister: a worker that dies
    // registered blocks every later collection forever.
    let mutator = crate::gc::MutatorRegistration::new_in(owner);
    let worker_state = Arc::clone(&state);
    with_parallel_context(worker, worker_state, || {
        owner
            .scheduler()
            .components()
            .run_queues
            .with_owner(worker, || {
                scheduler_run_loop_in(owner, target, worker, Some(state.as_ref()), true, deadline);
            });
    });
    set_current_task(None);
    drop(mutator);
    SCHED_RUN_DEPTH.with(|depth| depth.set(0));
}

pub(super) fn with_parallel_context<R>(
    worker: usize,
    state: Arc<ParallelRunState>,
    f: impl FnOnce() -> R,
) -> R {
    let previous_worker = CURRENT_WORKER.with(|slot| {
        let previous = slot.get();
        slot.set(worker);
        previous
    });
    let previous_state = CURRENT_RUN_STATE.with(|slot| slot.replace(Some(state)));
    let result = f();
    CURRENT_RUN_STATE.with(|slot| {
        slot.replace(previous_state);
    });
    CURRENT_WORKER.with(|slot| slot.set(previous_worker));
    result
}

pub(super) fn target_is_done(target: Option<RuntimeTaskId>) -> bool {
    target_is_done_in(global_task_table(), target)
}

pub(super) fn target_is_done_in(tasks: &ShardedTaskTable, target: Option<RuntimeTaskId>) -> bool {
    let Some(t) = target else {
        return false;
    };
    !tasks
        .with(t, |task| !task.state.lifecycle().is_terminal())
        .unwrap_or(false)
}

/// Per-loop view of `target_is_done` that locks the target's shard only after
/// the task table's terminal epoch moves.
///
/// Every worker of a drive asks on every iteration. Before, each question
/// locked the one shard holding the target, so the workers contended on it.
/// A target is done once its record is `Terminal` or gone, and both are
/// final, so a done answer is cached; a live answer holds only until the
/// epoch advances. The pool workers of one drive share one look per epoch
/// through [`SharedTargetCheck`].
pub(super) struct TargetWatch<'a> {
    pub(super) tasks: &'a ShardedTaskTable,
    pub(super) target: Option<RuntimeTaskId>,
    pub(super) shared: Option<&'a SharedTargetCheck>,
    pub(super) checked_at: Option<u64>,
    pub(super) done: bool,
}

impl<'a> TargetWatch<'a> {
    #[cfg(test)]
    pub(super) fn new(
        target: Option<RuntimeTaskId>,
        shared: Option<&'a SharedTargetCheck>,
    ) -> Self {
        Self::new_in(global_task_table(), target, shared)
    }

    pub(super) fn new_in(
        tasks: &'a ShardedTaskTable,
        target: Option<RuntimeTaskId>,
        shared: Option<&'a SharedTargetCheck>,
    ) -> Self {
        Self {
            tasks,
            target,
            shared,
            checked_at: None,
            done: false,
        }
    }

    pub(super) fn is_done(&mut self) -> bool {
        if self.done || self.target.is_none() {
            return self.done;
        }
        if let Some(shared) = self.shared
            && shared.done.load(Ordering::Acquire)
        {
            self.done = true;
            return true;
        }
        // Read the epoch BEFORE the state: a transition after this read
        // advances the epoch past it, so a later call looks again.
        let epoch = self.tasks.terminal_epoch();
        if self.checked_at == Some(epoch) {
            return false;
        }
        self.checked_at = Some(epoch);
        if let Some(shared) = self.shared {
            // Only the worker that raises `checked` past this epoch looks. A
            // worker that raised it further read a later epoch, so its look
            // also follows every transition this epoch covers.
            let claim = epoch + 1;
            if shared.checked.load(Ordering::Relaxed) >= claim
                || shared.checked.fetch_max(claim, Ordering::Relaxed) >= claim
            {
                return false;
            }
        }
        self.done = target_is_done_in(self.tasks, self.target);
        if self.done
            && let Some(shared) = self.shared
        {
            shared.done.store(true, Ordering::Release);
        }
        self.done
    }
}

pub(super) fn finish_active_poll(shared: Option<&ParallelRunState>) {
    if let Some(state) = shared {
        state.end_poll();
    }
}

pub(super) fn record_completed_task(completed: &mut i64, shared: Option<&ParallelRunState>) {
    *completed += 1;
    if let Some(state) = shared {
        state.completed.fetch_add(1, Ordering::AcqRel);
    }
}

pub(super) fn task_requires_cancel_poll(task: &RuntimeTask) -> bool {
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
    if task
        .native_stack
        .as_ref()
        .is_some_and(|stack| stack.is_suspended())
    {
        return true;
    }
    task.cancel.is_some() && !task.frame.is_null()
}

/// Run the async frame cleanup after native synchronous callers have unwound.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_sync_poll_cancel_cleanup() {
    let Some(id) = current_task_id() else {
        return;
    };
    let cleanup = global_task_table()
        .with_mut(id, |task| {
            task.cancel.take().map(|cancel| (cancel, task.frame))
        })
        .flatten();
    if let Some((cancel, frame)) = cleanup {
        crate::preempt::willow_sync_cleanup_enter();
        // SAFETY: The claimed task retains its frame and pairs it with the installed callback for
        // this execution.
        unsafe {
            crate::generated_context::invoke(cancel as *const (), frame);
        }
        crate::preempt::willow_sync_cleanup_leave();
    }
}

/// Pop/steal without the scheduler metadata mutex, then take only the task
/// shard needed for atomic state validation and task-work lookup.
#[cfg(test)]
pub(super) fn claim_global_ready_for_worker(
    worker: usize,
    shared: Option<&ParallelRunState>,
) -> Option<(RuntimeTaskId, ClaimedTaskWork)> {
    claim_ready_for_worker_in(crate::runtime::current().scheduler(), worker, shared)
}

pub(super) fn claim_ready_for_worker_in(
    services: &SchedulerServices,
    worker: usize,
    shared: Option<&ParallelRunState>,
) -> Option<(RuntimeTaskId, ClaimedTaskWork)> {
    enum Claim {
        Work(ClaimedTaskWork, Arc<crate::panic_context::PanicContext>),
        FinalizeCancelled,
        Reroute(usize),
        Spread,
        Drop,
    }

    let components = services.components();
    let queues = &components.run_queues;
    let tasks = &components.tasks;
    loop {
        // Publish the claim BEFORE the pop removes the id from every queue, and
        // keep it published until the claim has resolved into an active poll or
        // a requeue.
        let mut in_flight = ClaimInFlight::enter_in(&components.claims);
        let id = queues.pop_for_worker(worker)?;
        in_flight.popped();
        // Only a stop decision in progress sends the claim through the gate. Outside one, the
        // in-flight word already makes the
        // whole resolution, including a foreign-affinity reroute or a
        // requeue, visible to any later decision: resolving advances the
        // epoch, so a snapshot that read an empty queue before the requeue
        // cannot accept a word read after it.
        let claim_guard = shared
            .filter(|state| state.stop_decision_pending())
            .map(|state| {
                #[cfg(test)]
                let lock = crate::test_sync::lock(&state.claim_gate, "scheduler_claim_gate");
                #[cfg(not(test))]
                let lock = state.claim_gate.lock();
                lock.unwrap_or_else(|poisoned| poisoned.into_inner())
            });
        if shared.is_some_and(|state| state.stop.load(Ordering::Acquire)) {
            drop(claim_guard);
            // The queue token is still set because no claim occurred.
            queues.push_global(id);
            return None;
        }
        let claim = tasks
            .with_mut(id, |task| {
                if let Some(affinity) = &task.worker_affinity
                    && affinity.worker() != worker
                {
                    return Claim::Reroute(affinity.worker());
                }
                #[cfg(feature = "borrow-migration-probe")]
                let probe_poll = std::cell::Cell::new(task.cooperative_poll);
                #[cfg(feature = "borrow-migration-probe")]
                let probe_native = std::cell::Cell::new(false);
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
                    if let Some(owner) = task
                        .native_stack
                        .as_ref()
                        .and_then(|stack| stack.required_worker())
                        && owner != worker
                    {
                        return Claim::Reroute(owner);
                    }
                    // Native stacks remain pinned across implicit preemption;
                    // the probe verifies ownership as well as source-await migration.
                    #[cfg(feature = "borrow-migration-probe")]
                    {
                        probe_poll.set(
                            task.cooperative_poll
                                && task
                                    .native_stack
                                    .as_ref()
                                    .is_none_or(|stack| stack.required_worker().is_none()),
                        );
                        probe_native.set(
                            task.native_stack
                                .as_ref()
                                .is_some_and(|stack| stack.is_suspended()),
                        );
                    }
                }
                #[cfg(feature = "borrow-migration-probe")]
                if probe_poll.get()
                    && task.worker_affinity.is_none()
                    && task.state.lifecycle() == TaskLifecycle::Ready
                    && let Some(owner) = crate::borrow_migration_probe::route(
                        task.frame as usize,
                        worker,
                        queues.worker_count(),
                    )
                {
                    return Claim::Reroute(owner);
                }
                if task_should_spread(task, worker, queues, shared) {
                    task.spread_hops = task.spread_hops.saturating_add(1);
                    return Claim::Spread;
                }
                match task.claim_for_poll() {
                    ClaimOutcome::Drop => Claim::Drop,
                    ClaimOutcome::Poll => {
                        #[cfg(feature = "borrow-migration-probe")]
                        if task.cooperative_poll {
                            crate::borrow_migration_probe::claimed(
                                task.frame as usize,
                                worker,
                                id,
                                probe_native.get(),
                                task.worker_affinity.is_some() || !probe_poll.get(),
                            );
                        }
                        task.yield_requested = false;
                        task.started = true;
                        Claim::Work(
                            claimed_task_work(task, &tasks.cancel_epoch),
                            task.panic_context(),
                        )
                    }
                    ClaimOutcome::Cancel if task_requires_cancel_poll(task) => Claim::Work(
                        claimed_task_work(task, &tasks.cancel_epoch),
                        task.panic_context(),
                    ),
                    ClaimOutcome::Cancel => Claim::FinalizeCancelled,
                }
            })
            .unwrap_or(Claim::Drop);
        if matches!(claim, Claim::Work(..))
            && let Some(state) = shared
        {
            state.begin_poll();
        }
        drop(claim_guard);
        match claim {
            Claim::Work(work, panic_context) => {
                set_current_task_context(id, panic_context);
                return Some((id, work));
            }
            Claim::FinalizeCancelled => {
                SchedulerServices::with_scheduler(
                    || services,
                    |sched| sched.finalize_cancelled(id),
                );
                crate::observability::record(
                    crate::observability::RuntimeEventKind::TaskCancelled,
                    Some(worker),
                    id,
                    0,
                );
                set_current_task(None);
                // A terminal state is a boundary, like the end of a poll.
                // Worker 0 may be parked without a timeout after it saw this
                // task's previous poll as active; a Preempted poll end and a
                // sole local requeue bump no generation, so wake it here,
                // after the claim marker has resolved.
                drop(in_flight);
                queues.parking.notify_worker(0);
            }
            Claim::Reroute(owner) => {
                queues.push_rerouted(owner, id);
                return None;
            }
            Claim::Spread => {
                // The queue token is still set because no claim occurred.
                queues.push_global(id);
                queues.notify_idle();
                queues.defer_global_pop(worker);
                return None;
            }
            Claim::Drop => {}
        }
    }
}

/// A first poll pins a suspending task to its worker for the rest of its life
/// (native stack or quantum affinity). A worker that still has queued local
/// work would time-slice the new task with that work while another worker sits
/// idle, so it hands an unpinned Ready task back to the shared queue and wakes
/// an idle worker instead. Idleness is "not running a poll",
/// not "registered to park": a worker between parks, or one just woken for
/// this very task, is not registered either, and treating it as busy pinned
/// two tasks to one worker. Each hop is paired with a local pop by the busy
/// worker (`defer_global_pop`), and a task hops at most once per pool worker.
/// A peer with queued local work is not idle either, and a shared queue that
/// already holds a task for every idle peer gains nothing from one more:
/// requeueing then only churns the shared queue and wakes peers that find
/// other work anyway, as when batch pops spread a spawn burst over every
/// local queue.
pub(super) fn task_should_spread(
    task: &RuntimeTask,
    worker: usize,
    queues: &RunQueues,
    shared: Option<&ParallelRunState>,
) -> bool {
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
    if task.native_stack.is_some() {
        return false;
    }
    !task.started
        && usize::from(task.spread_hops) < queues.worker_count()
        && task.worker_affinity.is_none()
        && task.state.lifecycle() == TaskLifecycle::Ready
        && queues
            .locals
            .get(worker)
            .is_some_and(|queue| !queue.looks_empty())
        && shared.is_some_and(|state| {
            // The claiming worker has not begun its poll, so at most `workers
            // - 1 - active polls` peers are idle.
            // Test the per-worker lengths first: a busy pool answers there
            // without reading every accounting word.
            let waiting = queues.global_len.load(Ordering::Relaxed);
            queues.idle_peers_exceed(worker, waiting)
                && waiting
                    < queues
                        .worker_count()
                        .saturating_sub(state.active_polls().saturating_add(1))
        })
}

pub(super) fn duration_until(deadline: Instant) -> Duration {
    deadline
        .checked_duration_since(crate::timer::scheduler_now())
        .unwrap_or_default()
}

pub(super) fn bounded_parallel_wait(duration: Duration) -> Duration {
    std::cmp::min(duration, Duration::from_millis(1))
}

/// Clamp an idle wait to a drive deadline, so a drive bounded by the caller's
/// own deadline never blocks past it on an unrelated (possibly far-off) timer.
pub(super) fn deadline_bounded(wait: Duration, deadline: Option<Instant>) -> Duration {
    match deadline {
        Some(d) => std::cmp::min(wait, duration_until(d)),
        None => wait,
    }
}

// Timed/event-source and transient-claim fallback. Idle waiters publish GC
// roots across their wait; pure compute needs no periodic fallback.
pub(super) fn idle_wait_bound(
    components: &SchedulerComponents,
    deadline: Option<Instant>,
) -> Duration {
    let wait = next_timer_deadline_in(components)
        .map(|(_, timer)| bounded_parallel_wait(duration_until(timer)))
        .unwrap_or(Duration::from_millis(1));
    deadline_bounded(wait, deadline)
}

// Pure compute has no timed event to service. Work publication and poll
// completion explicitly wake this wait. Retain bounded polling for netpoll,
// whose readiness is supplied by the OS rather than the scheduler wait list.
// Nested drives also retain bounded waits: a peer completion targets worker 0,
// whereas their awaited target can belong to any worker.
pub(super) fn wait_for_scheduler_event(
    owner: &crate::runtime::Runtime,
    components: &SchedulerComponents,
    generation: u64,
    deadline: Option<Instant>,
) {
    if SCHED_RUN_DEPTH.with(Cell::get) > 1
        || deadline.is_some()
        || next_timer_deadline_in(components).is_some()
        || owner.netpoll.has_waiters()
    {
        parking::wait_for_owner_event(
            owner,
            generation,
            Some(idle_wait_bound(components, deadline)),
        );
    } else {
        parking::wait_for_owner_event(owner, generation, None);
    }
}

pub(super) fn wake_netpoll_in(
    owner: &crate::runtime::Runtime,
    components: &SchedulerComponents,
    timeout: Option<Duration>,
) -> usize {
    let tasks = owner.netpoll.wait_ready_tasks(timeout);
    let count = tasks.len();
    for task in tasks {
        crate::observability::record(
            crate::observability::RuntimeEventKind::NetpollWake,
            None,
            task,
            1,
        );
        wake_task_in(&components.tasks, &components.run_queues, task);
    }
    if count > 0 {
        crate::gc::stress_collect_in(owner, "scheduler");
    }
    count
}

pub(super) fn scheduler_idle_step(
    owner: &crate::runtime::Runtime,
    components: &SchedulerComponents,
    worker: usize,
    shared: Option<&ParallelRunState>,
    keep_alive_for_paused: bool,
    deadline: Option<Instant>,
    generation: u64,
) -> bool {
    let parallel = shared.is_some();
    let claims = shared.map_or_else(|| components.claims.as_ref(), |state| state.claims.as_ref());

    // A worker may have claimed the last ready task immediately before this
    // worker observed an empty queue. Read the poll count only after that queue
    // observation: using a value captured earlier can falsely declare global
    // idle while the other worker is still publishing a timer/netpoll waiter.
    // A claim that has popped its task but not yet reached `active_polls` is
    // equally in-flight work, and is invisible in every other check.
    //
    if claims.in_flight() {
        // Empty claims publish no event when they finish. Reprobe this short
        // transition instead of waiting indefinitely for a nonexistent poll.
        parking::wait_for_owner_event(
            owner,
            generation,
            Some(idle_wait_bound(components, deadline)),
        );
        return true;
    }
    if let Some(state) = shared {
        let before = claims.snapshot();
        // Scan once, distinguishing an observed poll from a racing empty
        // claim. Only a poll promises a future boundary notification.
        if state.active_polls() > 0 {
            wait_for_scheduler_event(owner, components, generation, deadline);
            return true;
        }
        if claims.changed_or_in_flight(&before) {
            parking::wait_for_owner_event(
                owner,
                generation,
                Some(idle_wait_bound(components, deadline)),
            );
            return true;
        }
    }

    let earliest = next_timer_deadline_in(components);
    if owner.netpoll.has_waiters() {
        if !parallel || worker == 0 {
            let timeout = if parallel {
                Some(
                    earliest
                        .map(|(_, deadline)| bounded_parallel_wait(duration_until(deadline)))
                        .unwrap_or_else(|| Duration::from_millis(1)),
                )
            } else {
                earliest.map(|(_, deadline)| duration_until(deadline))
            };
            let timeout = match (timeout, deadline) {
                (Some(wait), _) => Some(deadline_bounded(wait, deadline)),
                // An unbounded netpoll wait must still respect a drive deadline.
                (None, Some(d)) => Some(duration_until(d)),
                (None, None) => None,
            };
            if wake_netpoll_in(owner, components, timeout) > 0 {
                crate::gc::stress_collect_in(owner, "scheduler");
                return true;
            }
            // Parallel polling uses a bounded wait so worker 0 can also service
            // timers and scheduler state. A timeout is not global idleness:
            // the registered I/O task may simply not be ready yet.
            if parallel {
                return true;
            }
        } else {
            parking::wait_for_owner_event(
                owner,
                generation,
                Some(idle_wait_bound(components, deadline)),
            );
            return true;
        }
    }

    match earliest {
        Some((_, timer_deadline)) => {
            let wait = duration_until(timer_deadline);
            if !wait.is_zero() {
                let wait = if parallel {
                    bounded_parallel_wait(wait)
                } else {
                    wait
                };
                let wait = deadline_bounded(wait, deadline);
                if !wait.is_zero() {
                    parking::wait_for_owner_event(owner, generation, Some(wait));
                }
            }
            let woken = wake_due_timers_in(components, crate::timer::scheduler_now());
            for _ in 0..woken {
                crate::gc::stress_collect_in(owner, "scheduler");
            }
            true
        }
        None if parallel
            && keep_alive_for_paused
            && shared.is_some_and(|state| state.paused_polls() > 0) =>
        {
            parking::wait_for_owner_event(
                owner,
                generation,
                Some(idle_wait_bound(components, deadline)),
            );
            true
        }
        None => {
            // Snapshot BEFORE checking BlockedSyscall state. If completion
            // races after the check, wait_for_wake_since observes the changed
            // generation and returns immediately instead of sleeping 50ms.
            let generation = owner.parking.generation();
            // Wake publishes its queue entry before decrementing the blocked
            // count. Observe those in the matching order: if the count is
            // already zero, the subsequent queue read must see the handoff.
            let has_blocked_syscall = components.tasks.blocked_syscall_count() > 0;
            if has_blocked_syscall {
                // The blocking-pool completion wake is the only signal, so
                // keep the scheduler alive. The 50ms bound is only a portable
                // fallback for missed/foreign notifications.
                parking::wait_for_owner_event(
                    owner,
                    generation,
                    Some(deadline_bounded(Duration::from_millis(50), deadline)),
                );
                true
            } else if components.run_queues.len() > 0 {
                // Completion moved the task to Ready after this idle worker's
                // earlier empty-queue observation.
                true
            } else {
                false
            }
        }
    }
}

/// A source that can hold — or produce — a runnable task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkSource {
    /// A poll running or paused inside a nested drive. It can still requeue
    /// its task or wake another one before it ends.
    Poll,
    /// An armed wake-deadline in the global timer heap.
    Timer,
    /// A task the blocking pool still owes a completion wake.
    BlockedSyscall,
    /// A task sitting in the global or a worker-local run queue.
    RunQueue,
    /// A worker holding a popped-but-unclaimed task, or one mid-poll.
    Claim,
    /// A task parked on I/O readiness.
    Netpoll,
}

/// The order [`parallel_run_is_idle_locked`] must read the work sources in.
///
/// This is the whole correctness argument for ending a parallel drive, so it
/// is declared as data and checked by a test rather than left implicit in the
/// order of a few `if` statements. Every producer publishes a "still busy"
/// marker and its queue entry in a fixed order, and the snapshot has to read
/// the two in the opposite order so that no producer can slip through both:
///
/// * Timer promotion holds the heap lock until its queue entry is published,
///   so [`WorkSource::Timer`] is read BEFORE [`WorkSource::RunQueue`].
///   Reversing that permits: `idle: queue empty` / `timer worker: pop timer,
///   enqueue task, release heap` / `idle: heap empty -> stop`.
/// * Blocked-syscall completion publishes its queue entry before decrementing
///   the counter, so [`WorkSource::BlockedSyscall`] is read BEFORE
///   [`WorkSource::RunQueue`].
/// * A poll publishes its requeue or wakes BEFORE `finish_active_poll`, so
///   [`WorkSource::Poll`] is read BEFORE [`WorkSource::RunQueue`]. Reading
///   the poll counters only after the queue permits: `idle: queue empty` /
///   `worker: yield pushes the task, poll ends` / `idle: no active poll ->
///   stop`.
/// * A claim publishes its in-flight marker BEFORE it pops, so
///   [`WorkSource::Claim`] is read AFTER [`WorkSource::RunQueue`]. Reversing
///   that permits: `idle: no claims` / `worker: enter claim, pop the last
///   task, block on claim_gate` / `idle: queue empty -> stop`, after which the
///   worker sees the stop, requeues the task it is holding and leaves —
///   stranding a runnable task and returning from the drive with its target
///   still alive. `claim_gate` alone does not cover this: it
///   excludes claims that have not popped yet, not the pop itself.
///
///
/// None of these independently synchronized structures requires
/// `GLOBAL_SCHEDULER`.
pub(super) const IDLE_READ_ORDER: [WorkSource; 6] = [
    WorkSource::Poll,
    WorkSource::Timer,
    WorkSource::BlockedSyscall,
    WorkSource::RunQueue,
    WorkSource::Claim,
    WorkSource::Netpoll,
];

#[cfg(test)]
pub(super) fn work_source_is_live(state: &ParallelRunState, source: WorkSource) -> bool {
    work_source_is_live_in(
        crate::runtime::current(),
        global_components(),
        state,
        source,
    )
}

pub(super) fn work_source_is_live_in(
    owner: &crate::runtime::Runtime,
    components: &SchedulerComponents,
    state: &ParallelRunState,
    source: WorkSource,
) -> bool {
    match source {
        WorkSource::Poll => state.has_live_poll(),
        WorkSource::Timer => next_timer_deadline_in(components).is_some(),
        WorkSource::BlockedSyscall => components.tasks.blocked_syscall_count() > 0,
        WorkSource::RunQueue => components.run_queues.len() > 0,
        WorkSource::Claim => state.claims.in_flight() || state.has_live_poll(),
        WorkSource::Netpoll => owner.netpoll.has_waiters(),
    }
}

/// Is this parallel run globally idle — nothing runnable now, and no source
/// that could make something runnable later?
///
/// Only coherent with claims excluded: production calls it through
/// [`ParallelRunState::publish_stop_if`], which also validates the claim
/// word around it and publishes the stop. See [`IDLE_READ_ORDER`] for why the
/// order of the reads is what makes this answer trustworthy.
#[cfg(test)]
pub(super) fn parallel_run_is_idle_locked(state: &ParallelRunState) -> bool {
    parallel_run_is_idle_in(crate::runtime::current(), global_components(), state)
}

pub(super) fn parallel_run_is_idle_in(
    owner: &crate::runtime::Runtime,
    components: &SchedulerComponents,
    state: &ParallelRunState,
) -> bool {
    !IDLE_READ_ORDER
        .iter()
        .any(|source| work_source_is_live_in(owner, components, state, *source))
}

pub(super) fn scheduler_run_loop(
    target: Option<RuntimeTaskId>,
    worker: usize,
    shared: Option<&ParallelRunState>,
    stop_pool_on_exit: bool,
    deadline: Option<Instant>,
) -> i64 {
    scheduler_run_loop_in(
        crate::runtime::current(),
        target,
        worker,
        shared,
        stop_pool_on_exit,
        deadline,
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "one scheduler turn keeps claim guards, GC safepoints, frame retirement and park-generation checks in lexical order; splitting its early exits obscures guard lifetimes on the hot poll path"
)]
pub(super) fn scheduler_run_loop_in(
    owner: &crate::runtime::Runtime,
    target: Option<RuntimeTaskId>,
    worker: usize,
    shared: Option<&ParallelRunState>,
    stop_pool_on_exit: bool,
    deadline: Option<Instant>,
) -> i64 {
    let _mutator = crate::gc::MutatorRegistration::new_in(owner);
    let services = owner.scheduler();
    let components = services.components();
    let frame_participant = components.retirements.reclaim.participant();
    let mut completed = 0i64;
    let mut target_watch = TargetWatch::new_in(
        &components.tasks,
        target,
        shared
            .filter(|_| stop_pool_on_exit)
            .map(|state| &state.target),
    );
    loop {
        // Snapshot before stop/target/GC observations: their notifications
        // must remain visible even if they race the later registration.
        let wake_generation = owner.parking.generation();
        if SPAWN_NOTIFICATION_PENDING.with(|pending| pending.replace(false)) {
            owner.parking.notify_idle();
        }
        components.run_queues.flush_local_wake_notification(worker);
        if fatal_panic_pending(owner) {
            park_until_fatal_exit();
        }
        if shared.is_some_and(|state| state.stop.load(Ordering::Acquire)) {
            break;
        }
        // A drive deadline belongs to the CALLER (sync `select` with a
        // `sleep(ms)` case), not to any task: give the caller its turn back on
        // time instead of running unrelated tasks to quiescence first.
        //
        if deadline.is_some_and(|d| crate::timer::scheduler_now() >= d) {
            break;
        }
        // Stop as soon as the TARGET task (an `await` of a concrete
        // handle) is done, instead of draining the whole scheduler to quiescence
        // — so awaiting one task does not run unrelated tasks to completion and
        // cannot hang on an unrelated non-terminating task. A
        // completed task may have been pruned (state None); treat that as done
        // too — the awaiter reads the result from the frame, not the task.
        if target_watch.is_done() {
            // The task state becomes Completed before its worker runs the
            // post-poll GC boundaries. Do not tear down the scoped pool while
            // that worker may still be collecting: the collector would wait
            // for worker 0 at a safepoint while worker 0 waits to join it.
            if stop_pool_on_exit && let Some(state) = shared {
                // Publish the stop with every claim excluded. Either an
                // in-flight claim is visible to the decision (and it retries),
                // or it observes the stop after us; there is no gap in which
                // worker 0 can start joining a newly active collector.
                let stopped = state.publish_stop_if(|state| !state.has_live_poll());
                if !stopped && !state.has_live_poll() {
                    // Only a claim was in flight. It resolves within a few
                    // instructions, so retry without the poll back-off.
                    crate::gc::relocatable_safepoint_in(owner);
                    std::thread::yield_now();
                    continue;
                }
                if !stopped {
                    // Never enter the GC while holding `claim_gate`: a
                    // collector can be waiting for a worker that needs this
                    // gate before it reaches its own safepoint.
                    crate::gc::relocatable_safepoint_in(owner);
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
            }
            break;
        }
        // Cooperative GC safepoint: cheap (one atomic load) when no collection is
        // pending; lets a parallel collector stop this driver between task polls.
        //
        crate::gc::relocatable_safepoint_in(owner);
        // A runnable CPU task can keep the ready queue non-empty forever.
        // Promote expired timers before selecting work so those tasks still get
        // a turn without waiting for the scheduler to become idle. This runs on
        // every worker on every iteration, so it must stay cheap when no timer
        // exists: `wake_global_due_timers` answers that case with one atomic
        // load and takes no lock at all.
        let woken_timers = wake_due_timers_now_in(components);
        let next = if shared.is_some_and(|state| state.stop.load(Ordering::Acquire)) {
            None
        } else {
            claim_ready_for_worker_in(services, worker, shared)
        };
        // A claim can finalize a cancel-requested task. Purge its captured
        // external registrations outside the scheduler lock before selecting
        // more work.
        drain_terminal_cleanups_in(owner, components);
        // Outside any unit of this worker, so its own last poll no longer
        // holds back the frames retired during it.
        release_quiescent_frame_roots_in(owner, components, false);
        for _ in 0..woken_timers {
            crate::gc::stress_collect_in(owner, "scheduler");
        }
        let Some((id, work)) = next else {
            crate::observation::SchedulerHooks::idle(|| owner, 0);
            // No ready task. If a parked task has a wake-deadline (e.g. it is
            // sleeping), block until the earliest one and wake it, then keep
            // running. If netpoll has parked I/O waiters, wait for readiness
            // first (bounded by the nearest timer deadline) and wake matching
            // tasks. Otherwise there is genuinely nothing left to do.
            //
            // Only worker 0 decides that a parallel run is globally idle.
            // Letting any worker stop the pool races with another worker that
            // is publishing a timer/netpoll waiter as its poll returns Pending.
            if stop_pool_on_exit && shared.is_some() && worker != 0 {
                wait_for_scheduler_event(owner, components, wake_generation, deadline);
                continue;
            }
            // A nested run_until may wait on a target whose poll is itself
            // paused inside another nested scheduler drive. Keep waiting while
            // any such target chain is paused instead of returning a zero/
            // uninitialized result to its awaiter.
            if scheduler_idle_step(
                owner,
                components,
                worker,
                shared,
                stop_pool_on_exit || target.is_some(),
                deadline,
                wake_generation,
            ) {
                continue;
            }
            if stop_pool_on_exit && let Some(state) = shared {
                // Revalidate global idleness while claims are excluded. Work
                // can be published between the earlier empty pop and this
                // point; stopping without this check strands that task in the
                // queue. The stop is published only once it is FINAL: an
                // earlier version set it optimistically and rolled it back
                // when a later check failed; any worker that read the flag
                // inside that rollback window left the pool for good, and any
                // claim that read it requeued the task it was holding and went
                // idle.
                if !state.publish_stop_if(|state| parallel_run_is_idle_in(owner, components, state))
                {
                    continue;
                }
            }
            break;
        };
        // The gate can close between the check above and this claim, and the
        // task in hand may be exactly the awaiter that the fatal publication
        // just woke. Re-check before dispatching any work.
        if fatal_panic_pending(owner) {
            park_until_fatal_exit();
        }
        // Poll, cleanup and their terminal boundaries form one unit: frames
        // retired while it runs keep their roots until it ends. A poll that
        // returned to a live boundary ends it before publishing the task, and
        // every path ends it before the poll accounting.
        // Idle waits stay outside it so a parked worker never delays release.
        let unit = frame_participant.enter();
        // A task the claim moved to Cancelling: run its cleanup entry WITHOUT
        // the scheduler lock (poll-like), then finalize as Cancelled. The frame stays rooted
        // until finalization.
        let cancel_work = work.cancel;
        let frame_identity = cancel_work
            .as_ref()
            .map(|(_, frame)| *frame as usize)
            .or_else(|| work.poll.as_ref().map(|(_, frame, _)| *frame as usize))
            .unwrap_or(0);
        let _poll_lifetime =
            crate::frame_lifetime::PollLifetime::enter_in(&owner.frame_lifetime, frame_identity);
        if let Some((cancel_fn, cancel_frame)) = cancel_work {
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
                if work.direct_cancel {
                    // SAFETY: The claimed task retains its frame and pairs it with the installed
                    // callback for this execution.
                    unsafe {
                        crate::generated_context::invoke(cancel_fn as *const (), cancel_frame)
                    };
                } else {
                    let mut stack = crate::native_stack::NativeStack::acquire_cleanup_in(
                        owner,
                        cancel_fn,
                        cancel_frame,
                    );
                    let flag = components
                        .tasks
                        .with(id, RuntimeTask::preempt_flag_ptr)
                        .unwrap_or(std::ptr::null());
                    // SAFETY: The running task owns its stable flag until the poll or cleanup
                    // quantum is unbound, including nested drives.
                    unsafe { crate::preempt::willow_preempt_begin(flag) };
                    // SAFETY: The claimed task retains its frame and pairs it with the installed
                    // callback for this execution.
                    let result = unsafe { crate::native_stack::NativeStack::resume(&mut *stack) };
                    crate::preempt::willow_preempt_end();
                    if stack.is_suspended() {
                        CURRENT_EXECUTION.with(|slot| slot.borrow_mut().stack = Some(stack));
                        let boundary = match result {
                            RUNTIME_POLL_PENDING => GlobalPollBoundary::Pending,
                            RUNTIME_POLL_BLOCKED_SYSCALL => GlobalPollBoundary::BlockedSyscall,
                            RUNTIME_POLL_PREEMPTED => GlobalPollBoundary::Preempted,
                            _ => GlobalPollBoundary::Runnable,
                        };
                        finish_poll_boundary_in(components, id, boundary);
                        let event = match result {
                            RUNTIME_POLL_PENDING => {
                                crate::observability::RuntimeEventKind::TaskPark
                            }
                            RUNTIME_POLL_BLOCKED_SYSCALL => {
                                crate::observability::RuntimeEventKind::BlockingDetach
                            }
                            _ => crate::observability::RuntimeEventKind::TaskPreempt,
                        };
                        crate::observability::record(event, Some(worker), id, i64::from(result));
                        set_current_task(None);
                        drop(unit);
                        finish_active_poll(shared);
                        // As at the end of a regular poll: a runnable
                        // boundary published its requeue before this
                        // accounting ended, so a peer may already have
                        // finished the continuation.
                        if worker != 0
                            || matches!(result, RUNTIME_POLL_PENDING | RUNTIME_POLL_BLOCKED_SYSCALL)
                        {
                            owner.parking.notify_worker(0);
                        }
                        continue;
                    }
                    crate::native_stack::NativeStack::recycle(stack);
                }
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
            // SAFETY: this worker exclusively claimed the registered cancellation callback
            // and its live task frame. The runtime callback ABI does not unwind.
            unsafe {
                crate::generated_context::invoke(cancel_fn as *const (), cancel_frame)
            };
            let cleanup_panicked = crate::panic_context::willow_panic_active() != 0;
            let panic_chain = cleanup_panicked.then(async_chain_text);
            if cleanup_panicked {
                // Same ordering rule as the poll path: close the claim gate
                // before the terminal publication wakes any awaiter.
                begin_fatal_panic(owner);
            }
            SchedulerServices::with_scheduler(
                || services,
                |sched| {
                    if cleanup_panicked {
                        sched.finalize_panicked(id);
                    } else {
                        sched.finalize_cancelled(id);
                    }
                },
            );
            crate::observability::record(
                if cleanup_panicked {
                    crate::observability::RuntimeEventKind::TaskPanicked
                } else {
                    crate::observability::RuntimeEventKind::TaskCancelled
                },
                Some(worker),
                id,
                0,
            );
            drain_terminal_cleanups_in(owner, components);
            crate::gc::stress_collect_in(owner, "scheduler");
            drop(unit);
            finish_active_poll(shared);
            owner.parking.notify_worker(0);
            if cleanup_panicked {
                crate::panic_context::finish_unhandled_with_async_chain(
                    panic_chain.as_deref().unwrap_or_default(),
                );
            }
            set_current_task(None);
            continue;
        }
        let Some((poll, frame, preempt_flag)) = work.poll else {
            // Placeholder task with no executable work: just complete it.
            SchedulerServices::with_scheduler(
                || services,
                |sched| {
                    sched.complete(id);
                    sched.clear_running();
                },
            );
            crate::observability::record(
                crate::observability::RuntimeEventKind::TaskComplete,
                Some(worker),
                id,
                0,
            );
            drain_terminal_cleanups_in(owner, components);
            crate::gc::stress_collect_in(owner, "await");
            crate::gc::stress_collect_in(owner, "scheduler");
            drop(unit);
            finish_active_poll(shared);
            owner.parking.notify_worker(0);
            record_completed_task(&mut completed, shared);
            continue;
        };
        crate::gc::stress_collect_in(owner, "await");
        crate::observability::record(
            crate::observability::RuntimeEventKind::TaskPoll,
            Some(worker),
            id,
            0,
        );
        // SAFETY: The running task owns its stable flag until the poll or cleanup quantum is
        // unbound, including nested drives.
        unsafe { crate::preempt::willow_preempt_begin(preempt_flag) };
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
        let (result, native_cancelled, suspended) = {
            if work.direct_poll {
                (
                    // SAFETY: The claimed task retains its frame and pairs it with the installed
                    // callback for this execution.
                    unsafe { crate::generated_context::invoke(poll as *const (), frame) },
                    false,
                    false,
                )
            } else {
                let mut stack = CURRENT_EXECUTION
                    .with(|slot| slot.borrow_mut().stack.take())
                    .unwrap_or_else(|| {
                        crate::native_stack::NativeStack::acquire_in(owner, poll, frame)
                    });
                // SAFETY: The claimed task retains its frame and pairs it with the installed
                // callback for this execution.
                let result = unsafe { crate::native_stack::NativeStack::resume(&mut *stack) };
                let cancelled = stack.is_cancelled();
                let suspended = stack.is_suspended();
                if suspended {
                    CURRENT_EXECUTION.with(|slot| slot.borrow_mut().stack = Some(stack));
                } else {
                    crate::native_stack::NativeStack::recycle(stack);
                }
                (result, cancelled, suspended)
            }
        };
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
        let (result, native_cancelled, suspended) = (
            // SAFETY: the worker owns the poll claim and its registered frame remains live
            // until this non-unwinding callback returns.
            unsafe { crate::generated_context::invoke(poll as *const (), frame) },
            false,
            false,
        );
        crate::preempt::willow_preempt_end();
        // Native runtime polls (e.g. parallel-map chunks) need not implement
        // the generated async cancellation epilogue. Preserve their registered
        // cleanup callback and requeue into the ordinary cancellation entry.
        let result = if native_cancelled
            && result != RUNTIME_POLL_PANICKED
            && components
                .tasks
                .with(id, |task| task.cancel.is_some())
                .unwrap_or(false)
        {
            RUNTIME_POLL_PREEMPTED
        } else {
            result
        };
        let outcome = classify_poll_result(result);
        let fatal_chain = matches!(outcome, PollOutcome::Panicked | PollOutcome::Invalid(_))
            .then(async_chain_text);
        // A poll that returned to a live boundary holds no frame pointer any
        // more: its own frame stays rooted, and nothing of the poll remains on
        // a stack. End the unit before the boundary publishes the task, or a
        // peer that claims the continuation at once would still see this
        // worker's older epoch and keep everything retired during the poll
        // rooted. A suspended native stack still holds the poll's registers.
        let unit = if !suspended
            && matches!(
                outcome,
                PollOutcome::Yield
                    | PollOutcome::Preempted
                    | PollOutcome::Pending
                    | PollOutcome::BlockedSyscall
            ) {
            drop(unit);
            None
        } else {
            Some(unit)
        };
        match outcome {
            PollOutcome::Ready if native_cancelled => {
                SchedulerServices::with_scheduler(
                    || services,
                    |sched| sched.finalize_cancelled(id),
                );
            }
            PollOutcome::Ready => {
                // Awaiters read the result once the terminal status is
                // published: hand it over to the handle's holder, or publish
                // it first when the handle escaped. A GC point: the poll
                // returned and the frame stays rooted.
                crate::gc::finish_task_result_in(owner, id, frame as *mut u8);
                // An executable task owns its frame here, so the placeholder
                // preparation of `RuntimeScheduler::complete` does not apply.
                let _no_preempt = crate::preempt::NoPreemptGuard::enter();
                components
                    .terminal_target()
                    .finish(id, RuntimeTaskState::Completed);
            }
            PollOutcome::Yield | PollOutcome::Preempted => {
                // Runnable outcome (spec §7): gave up the worker but is not
                // waiting on an event. This hot boundary stays off the global
                // scheduler metadata lock.
                finish_poll_boundary_in(
                    components,
                    id,
                    if matches!(outcome, PollOutcome::Preempted) {
                        GlobalPollBoundary::Preempted
                    } else {
                        GlobalPollBoundary::Runnable
                    },
                );
            }
            PollOutcome::BlockedSyscall => {
                finish_poll_boundary_in(components, id, GlobalPollBoundary::BlockedSyscall);
            }
            PollOutcome::Pending => {
                finish_poll_boundary_in(components, id, GlobalPollBoundary::Pending);
            }
            PollOutcome::Panicked | PollOutcome::Invalid(_) => {
                // Close the claim gate BEFORE the terminal publication. Spec
                // §23 requires publishing PANICKED, detaching relationships and
                // releasing task roots before the process ends, but that publication
                // wakes this task's awaiters — without the gate another worker
                // claims one and runs ordinary code past `await <panicked
                // task>` in the window before the exit lands.
                begin_fatal_panic(owner);
                SchedulerServices::with_scheduler(|| services, |sched| sched.finalize_panicked(id));
            }
        }
        let event = match outcome {
            PollOutcome::Ready => crate::observability::RuntimeEventKind::TaskComplete,
            PollOutcome::Yield => crate::observability::RuntimeEventKind::TaskYield,
            PollOutcome::Preempted => crate::observability::RuntimeEventKind::TaskPreempt,
            PollOutcome::Pending => crate::observability::RuntimeEventKind::TaskPark,
            PollOutcome::BlockedSyscall => crate::observability::RuntimeEventKind::BlockingDetach,
            PollOutcome::Panicked | PollOutcome::Invalid(_) => {
                crate::observability::RuntimeEventKind::TaskPanicked
            }
        };
        crate::observability::record(event, Some(worker), id, i64::from(result));
        // Done polling this task: drop the running marker so a later
        // out-of-poll willow_sched_sleep/await does not target a stale task.
        if outcome != PollOutcome::Panicked {
            set_current_task(None);
        }
        if matches!(
            outcome,
            PollOutcome::Ready | PollOutcome::Panicked | PollOutcome::Invalid(_)
        ) {
            // Only terminal outcomes can have appended cleanup work. Pending,
            // blocked, yield, and preempt boundaries therefore stay entirely
            // off the scheduler metadata mutex.
            drain_terminal_cleanups_in(owner, components);
        }
        crate::gc::stress_collect_in(owner, "await");
        crate::gc::stress_collect_in(owner, "scheduler");
        // Keep this worker visible as active through the post-poll GC
        // boundaries. Otherwise worker 0 can leave the scoped pool and wait to
        // join this worker while its collection is waiting for worker 0 to
        // reach a safepoint.
        //
        // End the frame unit first: a drive that sees no active poll may
        // return and collect at once, and must not find this worker still
        // covering the frames it retired.
        drop(unit);
        finish_active_poll(shared);
        // Only the coordinator must recheck timers/netpoll/idleness.
        // Waking the whole pool here would cost tasks * workers. A runnable
        // outcome on another worker still ends an active poll that worker 0
        // may be waiting on: the requeue above is published before this
        // accounting ends, so a peer can steal and finish the continuation
        // first, and no later poll is left to wake worker 0.
        if worker != 0 || !matches!(outcome, PollOutcome::Yield | PollOutcome::Preempted) {
            owner.parking.notify_worker(0);
        }
        if outcome == PollOutcome::Ready {
            record_completed_task(&mut completed, shared);
        }
        match outcome {
            PollOutcome::Panicked => crate::panic_context::finish_unhandled_with_async_chain(
                fatal_chain.as_deref().unwrap_or_default(),
            ),
            PollOutcome::Invalid(value) => {
                report_poll_failure(id, Some(value), fatal_chain.as_deref().unwrap_or_default())
            }
            _ => {}
        }
    }
    completed
}
