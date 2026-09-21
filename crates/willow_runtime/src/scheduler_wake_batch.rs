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
        // Stable counting partition: one reusable allocation instead of one
        // high-water allocation per shard when successive batches change shape.
        let mut offsets = [0; TASK_TABLE_SHARDS + 1];
        for &id in ids {
            offsets[self.shard_index(id) + 1] += 1;
        }
        for index in 0..TASK_TABLE_SHARDS {
            offsets[index + 1] += offsets[index];
        }
        let mut next = offsets;
        scratch.grouped.resize(ids.len(), 0);
        for &id in ids {
            let slot = &mut next[self.shard_index(id)];
            scratch.grouped[*slot] = id;
            *slot += 1;
        }
        // Lock participating shards in the same ascending order as with_two_mut.
        // Retain them through queue publication and blocked-count reconciliation:
        // releasing early could let cancellation/reaping consume an unpublished
        // token, or let idle detection miss the last blocked-syscall completion.
        let mut shards: [_; TASK_TABLE_SHARDS] = std::array::from_fn(|index| {
            if offsets[index] == offsets[index + 1] {
                None
            } else {
                #[cfg(test)]
                {
                    scratch.shard_locks += 1;
                }
                Some(self.lock_shard(index))
            }
        });
        let mut unblocked = 0;
        for (index, shard) in shards.iter_mut().enumerate() {
            let Some(shard) = shard else { continue };
            for &id in &scratch.grouped[offsets[index]..offsets[index + 1]] {
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
    wake_tasks_outcome_in(&global_task_table(), &global_run_queues(), ids, scratch);
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
