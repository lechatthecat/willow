//! Cold diagnostics after the program-entry drive has established quiescence.
//! Registrations are evidence of waiting, not a proof that each select edge is
//! required, nor a claim about which task could produce a channel value.
use super::{RuntimeTask, RuntimeTaskId, ShardedTaskTable};
use crate::id_hash::IdSet;
use std::fmt::Write;

fn operation(task: &RuntimeTask) -> &'static str {
    if task.awaiting_count() > 0 {
        "await"
    } else if !task.wait_channels().is_empty() {
        "channel/select"
    } else if task.lock_wait().is_some() {
        "lock"
    } else {
        "parked operation"
    }
}

/// Call only after `run_until` returns, with no live runtime wake source.
/// All shard borrows end before another task is read or stderr is locked.
#[cold]
pub(super) fn main_witness(tasks: &ShardedTaskTable, root: RuntimeTaskId) -> Option<String> {
    let mut message = tasks.with(root, |task| {
        (!task.state.lifecycle().is_terminal()).then(|| {
            format!(
                "task {root} ({}) blocked on {}; no runnable task or wake source remains\nwitness (registered waits; select entries are alternatives):",
                task.name().unwrap_or("main"), operation(task)
            )
        })
    })??;
    append_waits(
        root,
        |id, visit| tasks.with(id, visit).is_some(),
        &mut message,
    );
    Some(message)
}

// Bounds keep a fatal diagnostic manageable even for an enormous wait graph.
// No recursion, frame dereferences, channel locks, or hot-path instrumentation.
const MAX_TASKS: usize = 64;
const MAX_LINKS: usize = 256;

fn append_waits(
    root: RuntimeTaskId,
    mut read: impl FnMut(RuntimeTaskId, &mut dyn FnMut(&RuntimeTask)) -> bool,
    message: &mut String,
) {
    let mut seen = IdSet::default();
    seen.insert(root);
    let mut pending = vec![root];
    let mut cursor = 0;
    let mut links = 0;
    let mut truncated = false;
    while cursor < pending.len() {
        let id = pending[cursor];
        cursor += 1;
        let found = read(id, &mut |task| {
            let _ = write!(
                message,
                "\n  task {id} ({})",
                task.name().unwrap_or("unnamed")
            );
            if let Some((file, line)) = task.spawn_site() {
                let _ = write!(message, " spawned at {file}:{line}");
            }
            let _ = write!(message, " blocked on {}", operation(task));
            for target in task.awaiting_ids() {
                if links == MAX_LINKS {
                    truncated = true;
                    break;
                }
                links += 1;
                let _ = write!(message, "\n    await -> task {target}");
                if !seen.contains(&target) {
                    if pending.len() == MAX_TASKS {
                        truncated = true;
                    } else {
                        seen.insert(target);
                        pending.push(target);
                    }
                }
            }
            for token in task.wait_channels() {
                if links == MAX_LINKS {
                    truncated = true;
                    break;
                }
                links += 1;
                let _ = write!(
                    message,
                    "\n    {:?} -> channel {:#x}",
                    token.role, token.channel
                );
            }
            if let Some(link) = task.lock_wait() {
                let _ = write!(
                    message,
                    "\n    {:?} -> lock {:?}",
                    link.access, link.lock_id
                );
            }
        });
        if !found {
            let _ = write!(message, "\n  task {id}: record unavailable");
        }
    }
    if truncated {
        let _ = write!(
            message,
            "\n  witness truncated (at most {MAX_TASKS} tasks, {MAX_LINKS} await/channel registrations)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{ChannelOwnershipToken, ChannelRole};

    #[test]
    fn deadlock_witness_after_scheduler_stops_on_await_cycle() {
        use super::super::{
            RUNTIME_POLL_PENDING, global_task_table, register_waiter_sharded,
            reset_global_scheduler_for_test, scheduler_has_wake_source, willow_sched_run_until,
            willow_sched_spawn_cooperative, willow_sched_task_state,
        };
        unsafe extern "C" fn pending(_: *mut std::ffi::c_void) -> i32 {
            RUNTIME_POLL_PENDING
        }
        for size in [2, 8] {
            let _runtime = crate::gc::isolated_runtime();
            let _watchdog = crate::test_sync::Watchdog::new("await cycle quiescence");
            reset_global_scheduler_for_test();
            let ids: Vec<_> = (0..size)
                .map(|_| {
                    // SAFETY: The poll ignores its null frame and never completes.
                    unsafe { willow_sched_spawn_cooperative(pending, std::ptr::null_mut()) as u64 }
                })
                .collect();
            for i in 0..size {
                assert!(register_waiter_sharded(
                    global_task_table(),
                    ids[(i + 1) % size],
                    ids[i]
                ));
            }
            assert_eq!(willow_sched_run_until(ids[0]), 0);
            assert!(!scheduler_has_wake_source());
            let witness = main_witness(global_task_table(), ids[0]).unwrap();
            assert_eq!(witness.matches("await -> task").count(), size);
            for id in ids {
                assert_eq!(willow_sched_task_state(id), 2);
                assert!(witness.contains(&format!("await -> task {id}")));
            }
        }
    }

    #[test]
    fn deadlock_witness_channel_fanout_is_explicitly_truncated() {
        let tasks = ShardedTaskTable::new();
        let mut task = RuntimeTask::new(1);
        for channel in 1..=512 {
            task.install_channel_ownership(ChannelOwnershipToken {
                channel,
                role: ChannelRole::RecvWait,
                generation: 1,
            });
        }
        tasks.insert(1, task);
        let witness = main_witness(&tasks, 1).unwrap();
        assert_eq!(witness.matches("RecvWait -> channel").count(), MAX_LINKS);
        assert!(witness.contains("witness truncated"));
        assert_eq!(tasks.with(1, |task| task.wait_channels().len()), Some(512));
    }

    #[test]
    fn deadlock_witness_preserves_registrations_and_identifies_resources() {
        let tasks = ShardedTaskTable::new();
        let mut root = RuntimeTask::new(1);
        root.set_name("main");
        root.add_awaiting(2);
        tasks.insert(1, root);
        let mut child = RuntimeTask::new(2);
        child.set_name("receiver");
        child.set_spawn_site("worker.wi".into(), 7);
        for (i, role) in [
            ChannelRole::RecvWait,
            ChannelRole::SendWait,
            ChannelRole::RecvClaim,
            ChannelRole::SendHandoff,
        ]
        .into_iter()
        .enumerate()
        {
            child.install_channel_ownership(ChannelOwnershipToken {
                channel: 0x100 + i,
                role,
                generation: 1,
            });
        }
        tasks.insert(2, child);
        let witness = main_witness(&tasks, 1).unwrap();
        for expected in [
            "task 1 (main) blocked on await",
            "await -> task 2",
            "task 2 (receiver) spawned at worker.wi:7 blocked on channel/select",
            "RecvWait -> channel 0x100",
            "SendWait -> channel 0x101",
            "RecvClaim -> channel 0x102",
            "SendHandoff -> channel 0x103",
        ] {
            assert!(witness.contains(expected), "{witness}");
        }
        assert_eq!(main_witness(&tasks, 1).unwrap(), witness);
        assert_eq!(tasks.with(1, RuntimeTask::awaiting_count), Some(1));
        assert_eq!(tasks.with(2, |t| t.wait_channels().len()), Some(4));
        assert!(main_witness(&tasks, 99).is_none());
        tasks.with_mut(1, |task| {
            assert!(task.state.claim_queue_slot());
            assert_eq!(
                task.state.claim_for_poll(),
                crate::task_state::ClaimOutcome::Poll
            );
            assert!(task.state.finish_terminal());
        });
        assert!(main_witness(&tasks, 1).is_none());
        tasks.remove(1);
        assert!(main_witness(&tasks, 1).is_none());
    }

    #[test]
    fn deadlock_witness_cycles_shared_targets_and_missing_records() {
        let tasks = ShardedTaskTable::new();
        for (id, targets) in [(1, vec![1, 2, 3]), (2, vec![3]), (3, vec![1, 99])] {
            let mut task = RuntimeTask::new(id);
            for target in targets {
                task.add_awaiting(target);
            }
            tasks.insert(id, task);
        }
        let witness = main_witness(&tasks, 1).unwrap();
        for id in 1..=3 {
            assert_eq!(witness.matches(&format!("\n  task {id} (")).count(), 1);
        }
        assert!(witness.contains("task 99: record unavailable"));
        assert_eq!(witness.matches("await ->").count(), 6);
        assert!(!witness.contains("truncated"));
    }

    #[test]
    fn deadlock_witness_bounds_visits_and_output_for_deep_wide_and_fragmented_graphs() {
        for n in [8, 64, 512, 4096] {
            for shape in ["chain", "fanout", "shared", "fragmented"] {
                let mut tasks = Vec::new();
                for id in 0..n {
                    let mut task = RuntimeTask::new(id as u64);
                    match shape {
                        "chain" => {
                            task.add_awaiting(((id + 1) % n) as u64);
                        }
                        "fanout" if id == 0 => {
                            for target in 1..n {
                                task.add_awaiting(target as u64);
                            }
                        }
                        "shared" => {
                            task.add_awaiting(0);
                            task.add_awaiting(((id + 1) % n) as u64);
                        }
                        "fragmented" => {
                            for target in 0..n {
                                task.add_awaiting(target as u64);
                            }
                            for target in 1..n {
                                task.remove_awaiting(target as u64);
                            }
                        }
                        _ => {}
                    }
                    tasks.push(task);
                    if shape == "fragmented" {
                        break;
                    }
                }
                let mut reads = 0;
                let mut witness = String::new();
                append_waits(
                    0,
                    |id, visit| {
                        reads += 1;
                        visit(&tasks[id as usize]);
                        true
                    },
                    &mut witness,
                );
                let edges = witness.matches("await ->").count();
                assert!(reads <= MAX_TASKS && edges <= MAX_LINKS);
                if shape == "fragmented" {
                    assert_eq!((reads, edges), (1, 1));
                } else if n <= MAX_TASKS {
                    assert_eq!(reads, n);
                } else {
                    assert!(witness.contains("truncated"));
                }
                println!("{shape} n={n} task_reads={reads} emitted_edges={edges}");
            }
        }
    }
}
