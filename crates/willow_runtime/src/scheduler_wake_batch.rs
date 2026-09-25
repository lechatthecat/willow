//! Shard-grouped wake publication (willow-ijui.5).
use super::*;

/// Reuse the buffers across channel notifications. Results last until the next batch.
#[derive(Default)]
pub(crate) struct WakeBatchScratch {
    grouped: Vec<RuntimeTaskId>,
    pub(crate) enqueued: Vec<RuntimeTaskId>,
    pub(crate) terminal: Vec<RuntimeTaskId>,
    #[cfg(test)]
    shard_locks: usize,
    #[cfg(test)]
    queue_batches: usize,
}

impl ShardedTaskTable {
    fn wake_many(
        &self,
        run_queues: &RunQueues,
        ids: &[RuntimeTaskId],
        scratch: &mut WakeBatchScratch,
    ) {
        scratch.enqueued.clear();
        scratch.terminal.clear();
        #[cfg(test)]
        {
            scratch.shard_locks = 0;
            scratch.queue_batches = 0;
        }
        scratch.grouped.clear();
        if ids.is_empty() {
            return;
        }
        // Group ids by shard, stable within a shard, so each shard is locked
        // once and in ascending index order (as with_two_mut). A batch at
        // least as large as the shard count uses a counting partition,
        // O(ids + shards) = O(ids); a smaller one a stable sort, O(ids log ids),
        // so a small batch pays nothing per shard (willow-8hq4.19). Both group
        // in the reused `grouped` buffer.
        scratch.grouped.extend_from_slice(ids);
        if ids.len() >= TASK_TABLE_SHARDS {
            let mut offsets = [0; TASK_TABLE_SHARDS + 1];
            for &id in ids {
                offsets[self.shard_index(id) + 1] += 1;
            }
            for index in 0..TASK_TABLE_SHARDS {
                offsets[index + 1] += offsets[index];
            }
            for &id in ids {
                let slot = &mut offsets[self.shard_index(id)];
                scratch.grouped[*slot] = id;
                *slot += 1;
            }
        } else {
            scratch.grouped.sort_by_key(|&id| self.shard_index(id));
        }
        // Retain every participating shard through queue publication and
        // blocked-count reconciliation: releasing early could let
        // cancellation/reaping consume an unpublished token, or let idle
        // detection miss the last blocked-syscall completion. A one-id batch,
        // the common channel handoff, holds its guard without allocating; a
        // larger one allocates one guard vector, O(1) per batch.
        let mut single = None;
        let mut several = Vec::new();
        if ids.len() == 1 {
            single = Some((0..1, self.lock_shard(self.shard_index(ids[0]))));
        } else {
            let mut start = 0;
            while start < scratch.grouped.len() {
                let index = self.shard_index(scratch.grouped[start]);
                let mut end = start + 1;
                while end < scratch.grouped.len() && self.shard_index(scratch.grouped[end]) == index
                {
                    end += 1;
                }
                several.push((start..end, self.lock_shard(index)));
                start = end;
            }
        }
        #[cfg(test)]
        {
            scratch.shard_locks = usize::from(single.is_some()) + several.len();
        }
        let mut unblocked = 0;
        for (range, shard) in single.iter_mut().chain(several.iter_mut()) {
            for &id in &scratch.grouped[range.clone()] {
                let Some(task) = shard.get_mut(&id) else {
                    scratch.terminal.push(id);
                    continue;
                };
                let (before, outcome) = wake_task_state(task);
                match outcome {
                    WakeOutcome::Enqueue => scratch.enqueued.push(id),
                    WakeOutcome::Terminal => scratch.terminal.push(id),
                    _ => {}
                }
                if before == TaskLifecycle::BlockedSyscall
                    && task.state.lifecycle() != TaskLifecycle::BlockedSyscall
                {
                    unblocked += 1;
                }
            }
        }
        run_queues.push_woken_batch(&scratch.enqueued);
        #[cfg(test)]
        {
            scratch.queue_batches = usize::from(!scratch.enqueued.is_empty());
        }
        // wake() only exits BlockedSyscall; it never enters it. All affected
        // shards remain locked, so the aggregate decrement is equivalent to
        // per-task reconcile_blocked_transition, after the same publication edge.
        if unblocked != 0 {
            let previous = self.blocked_syscall.fetch_sub(unblocked, Ordering::AcqRel);
            assert!(previous >= unblocked, "blocked-syscall counter underflow");
        }
    }
}

fn wake_tasks_outcome_in(
    tasks: &ShardedTaskTable,
    run_queues: &RunQueues,
    ids: &[RuntimeTaskId],
    scratch: &mut WakeBatchScratch,
) {
    tasks.wake_many(run_queues, ids, scratch);
}

/// Terminal IDs identify reservations the channel must clean up. Other owners
/// keep their reservations, including already queued and currently polling tasks.
pub(crate) fn wake_channel_owners(ids: &[u64], scratch: &mut WakeBatchScratch) {
    if ids.is_empty() {
        scratch.enqueued.clear();
        scratch.terminal.clear();
        return;
    }
    crate::gc::stress_collect("scheduler");
    wake_tasks_outcome_in(global_task_table(), global_run_queues(), ids, scratch);
    let worker = current_task_id().map(|_| current_worker());
    for &id in &scratch.enqueued {
        crate::observability::record(
            crate::observability::RuntimeEventKind::TaskWake,
            worker,
            id,
            0,
        );
    }
    crate::gc::stress_collect("scheduler");
}

#[cfg(test)]
#[path = "scheduler_wake_batch_tests.rs"]
mod tests;
