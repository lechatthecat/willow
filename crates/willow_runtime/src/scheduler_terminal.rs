//! Terminal-task retirement without the scheduler metadata mutex.
//!
//! Every finished task passes through here, so one shared lock serialized all
//! workers once per task, and again to read the results back. Retired frames
//! live in per-worker shards instead. Tags are taken under a shard's lock, so
//! each shard stays ordered by tag and release pops a prefix of each.
//! External cleanups stay on one list because only tasks holding a channel,
//! lock or netpoll registration produce one.
use super::*;

/// Workers beyond this share shards; a release scan visits each shard once.
const RETIREMENT_SHARDS: usize = 16;

#[derive(Debug)]
struct RetirementShard {
    frames: VecDeque<RetiredFrame>,
    /// Pending length at which a retirement requests the next release scan.
    /// Doubling over the frames a scan had to keep amortizes the O(threads +
    /// shards) scan to O(1) per retirement while bounding each shard's
    /// retained frames by `max(FRAME_RELEASE_MIN_BATCH, 2 x frames retired
    /// under a still-running unit)`.
    scan_at: usize,
}

/// Padded so one worker's retirements do not share a line with a peer's.
#[derive(Debug)]
#[repr(align(128))]
struct PaddedShard(Mutex<RetirementShard>);

#[derive(Debug)]
pub(super) struct TerminalRetirements {
    shards: Box<[PaddedShard]>,
    /// Terminal tasks whose channel/netpoll registrations must be purged
    /// outside every task lock. Channel addresses are captured before the
    /// heavy task record is removed.
    cleanups: Mutex<Vec<TerminalCleanup>>,
    /// Set whenever `cleanups` may be non-empty. Written under its lock, read
    /// without it, so the run loop's per-iteration drain takes no lock when
    /// nothing is pending.
    pub(super) cleanups_pending: AtomicBool,
}

impl TerminalRetirements {
    pub(super) fn new() -> Self {
        Self {
            shards: (0..RETIREMENT_SHARDS)
                .map(|_| {
                    PaddedShard(Mutex::new(RetirementShard {
                        frames: VecDeque::new(),
                        scan_at: FRAME_RELEASE_MIN_BATCH,
                    }))
                })
                .collect(),
            cleanups: Mutex::new(Vec::new()),
            cleanups_pending: AtomicBool::new(false),
        }
    }

    fn lock(shard: &PaddedShard) -> std::sync::MutexGuard<'_, RetirementShard> {
        shard.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn lock_cleanups(&self) -> std::sync::MutexGuard<'_, Vec<TerminalCleanup>> {
        self.cleanups
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub(super) fn retire_frame(&self, frame: usize) {
        let mut shard = Self::lock(&self.shards[current_worker() % RETIREMENT_SHARDS]);
        let tag = crate::frame_reclaim::retire_tag();
        crate::frame_lifetime::record(
            crate::frame_lifetime::FrameEventKind::Terminal,
            frame,
            tag,
            0,
        );
        shard.frames.push_back(RetiredFrame { frame, tag });
        if shard.frames.len() >= shard.scan_at {
            FRAME_RELEASE_DUE.store(true, Ordering::Release);
        }
    }

    /// Take the retired frames whose tag is below `bound` (see
    /// [`crate::frame_reclaim::release_bound`]): O(frames released + shards).
    pub(super) fn take_quiescent_frames(
        &self,
        bound: u64,
        frame_roots: &AtomicUsize,
    ) -> Vec<RetiredFrame> {
        let mut frames = Vec::new();
        for shard in &self.shards {
            let mut shard = Self::lock(shard);
            while let Some(retired) = shard.frames.front()
                && retired.tag < bound
            {
                crate::frame_lifetime::record(
                    crate::frame_lifetime::FrameEventKind::Quiescent,
                    retired.frame,
                    retired.tag,
                    0,
                );
                frames.push(*retired);
                shard.frames.pop_front();
            }
            shard.scan_at = FRAME_RELEASE_MIN_BATCH.max(shard.frames.len() * 2);
        }
        if !frames.is_empty() {
            frame_roots
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |roots| {
                    roots.checked_sub(frames.len())
                })
                .expect("scheduler frame-root counter underflow");
        }
        frames
    }

    pub(super) fn push_cleanup(&self, cleanup: TerminalCleanup) {
        let mut cleanups = self.lock_cleanups();
        cleanups.push(cleanup);
        self.cleanups_pending.store(true, Ordering::Release);
    }

    pub(super) fn take_cleanups(&self) -> Vec<TerminalCleanup> {
        let mut cleanups = self.lock_cleanups();
        self.cleanups_pending.store(false, Ordering::Relaxed);
        std::mem::take(&mut *cleanups)
    }

    pub(super) fn pending_cleanups(&self) -> usize {
        self.lock_cleanups().len()
    }

    #[cfg(test)]
    pub(super) fn take_all_frames(&self) -> Vec<usize> {
        self.shards
            .iter()
            .flat_map(|shard| std::mem::take(&mut Self::lock(shard).frames))
            .map(|retired| retired.frame)
            .collect()
    }

    #[cfg(test)]
    pub(super) fn pending_tags(&self) -> Vec<u64> {
        let mut tags: Vec<u64> = self
            .shards
            .iter()
            .flat_map(|shard| {
                Self::lock(shard)
                    .frames
                    .iter()
                    .map(|retired| retired.tag)
                    .collect::<Vec<_>>()
            })
            .collect();
        tags.sort_unstable();
        tags
    }

    #[cfg(test)]
    pub(super) fn scan_threshold(&self, worker: usize) -> usize {
        Self::lock(&self.shards[worker % RETIREMENT_SHARDS]).scan_at
    }
}

/// The scheduler state a terminal transition touches. Every field is
/// internally synchronized, so executable tasks finish without
/// `GLOBAL_SCHEDULER`.
pub(super) struct TerminalTarget<'a> {
    pub(super) tasks: &'a ShardedTaskTable,
    pub(super) run_queues: &'a RunQueues,
    pub(super) retirements: &'a TerminalRetirements,
}

impl TerminalTarget<'_> {
    pub(super) fn finish(&self, id: RuntimeTaskId, state: RuntimeTaskState) -> bool {
        let Some(terminal_status) = terminal_frame_status(state) else {
            debug_assert!(false, "finish_terminal requires a terminal state");
            return false;
        };
        let Some(mut task) = self.tasks.finish_terminal_and_remove(id, terminal_status) else {
            return false;
        };

        // Both directions are detached in O(registrations), not O(all tasks):
        // `waiters` pops live entries in registration order and `awaiting` is a
        // set, so neither side rescans a vector per relation.
        let waiters = task.take_waiters();
        let awaiting = task.take_awaiting();
        let channel_waits = task.take_wait_channels();
        let lock_wait = task.take_lock_wait();

        for awaitee in awaiting {
            self.tasks.with_mut(awaitee, |task| {
                task.remove_waiter(id);
            });
        }
        for waiter in waiters {
            self.tasks.with_mut(waiter, |task| {
                task.remove_awaiting(id);
            });
            wake_task_in(self.tasks, self.run_queues, waiter);
        }

        if task.frame_rooted && !task.frame.is_null() {
            task.frame_rooted = false;
            self.retirements.retire_frame(task.frame as usize);
        }
        // Only a task holding an external registration needs cleanup. Skipping
        // the rest keeps the netpoll lock off every ordinary completion.
        if task.netpoll_registered || !channel_waits.is_empty() || lock_wait.is_some() {
            self.retirements.push_cleanup(TerminalCleanup {
                task_id: id,
                channel_waits,
                lock_wait,
            });
        }

        // `RuntimeTask::roots` and all diagnostic/wait metadata are active-task
        // ownership. They may be dropped now that the poll has returned and
        // terminal status is published. The result frame keeps its independent
        // runtime root until its grace period ends.
        drop(task);
        true
    }
}
