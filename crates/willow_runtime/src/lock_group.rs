//! Ordered simultaneous acquisition of scheduler-aware locks.
//!
//! Entries stay in source order for constant-time binding access. A separate
//! permutation is sorted once by the process-wide lock identity. One entry at
//! most is pending; acquired entries are released in reverse.
//! Before waiting, every acquired entry is released, so group acquisition never
//! holds a member while waiting for a lock acquired in a different lexical
//! order elsewhere. The caller's enclosing locks remain its responsibility.
//!
//! A handoff is kept while the other members are tried, because giving it back
//! would rejoin that lock's FIFO queue at the tail and a steady stream of
//! waiters on one member could then starve the group forever. A kept handoff
//! may be out of identity order, so two groups could trade members in lockstep.
//! After a kept handoff fails, the next one is therefore given back and the
//! attempt restarts from the lowest identity, where one group must win. Every
//! retry sleeps in a queue, so nothing spins and no backoff is needed. Groups
//! are still not fair: other tasks can keep different members busy in turn,
//! and retries have no upper bound because acquisition has no failure result.
//! The compiler owns this native object until release/cancel and must keep its
//! pointer in a non-GC frame slot. Each lock handle has its own relocatable root.

use crate::async_mutex::*;
use crate::async_rwlock::*;
use crate::gc::GcRootHandle;
use crate::preempt::NoPreemptGuard;
use crate::task::RuntimeTaskId;
use std::ffi::c_void;

struct Entry {
    handle: GcRootHandle,
    id: u64,
    mode: i32,
    token: i64,
    held: bool,
}

impl Entry {
    fn raw(&self) -> *mut c_void {
        self.handle.load().cast()
    }

    fn acquire(&mut self) -> i32 {
        if self.mode == 0 {
            willow_async_mutex_acquire(self.raw(), &mut self.token)
        } else {
            willow_async_rwlock_acquire(self.raw(), self.mode, &mut self.token)
        }
    }

    fn poll(&self) -> i32 {
        if self.mode == 0 {
            willow_async_mutex_poll(self.raw(), self.token)
        } else {
            willow_async_rwlock_poll(self.raw(), self.token)
        }
    }

    fn release(&self) -> i32 {
        if self.mode == 0 {
            willow_async_mutex_release(self.raw(), self.token)
        } else {
            willow_async_rwlock_release(self.raw(), self.token)
        }
    }
}

struct LockGroup {
    entries: Vec<Entry>,
    order: Vec<usize>,
    prepared: bool,
    duplicate: bool,
    /// Entry indices in acquisition order; a handoff may precede lower ids.
    acquired: Vec<usize>,
    pending: Option<usize>,
    /// The last attempt kept a handoff and failed; give the next one back.
    restart: bool,
    owner: Option<RuntimeTaskId>,
    generation: i64,
    #[cfg(test)]
    released: Vec<u64>,
}

impl LockGroup {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            order: Vec::new(),
            prepared: false,
            duplicate: false,
            acquired: Vec::new(),
            pending: None,
            restart: false,
            owner: None,
            generation: crate::lock_wait::next_lock_id() as i64,
            #[cfg(test)]
            released: Vec::new(),
        }
    }

    fn prepare(&mut self) {
        if self.prepared {
            return;
        }
        self.order.extend(0..self.entries.len());
        // O(k log k) rather than an O(k) radix pass: k is the member count
        // written in one `lock` statement, and eight 256-bucket passes cost
        // more than this until k reaches thousands.
        self.order.sort_unstable_by_key(|&i| self.entries[i].id);
        self.duplicate = self
            .order
            .windows(2)
            .any(|pair| self.entries[pair[0]].id == self.entries[pair[1]].id);
        self.prepared = true;
    }

    fn drive(&mut self) -> i32 {
        self.prepare();
        if self.duplicate {
            return MUTEX_STATUS_RECURSIVE;
        }
        let mut kept = false;
        if let Some(index) = self.pending {
            debug_assert!(self.acquired.is_empty());
            match self.entries[index].poll() {
                MUTEX_STATUS_ACQUIRED => {
                    self.pending = None;
                    if self.restart {
                        self.entries[index].release();
                    } else {
                        self.keep(index);
                        kept = true;
                    }
                }
                MUTEX_STATUS_LOST => {
                    self.pending = None;
                    return MUTEX_STATUS_LOST;
                }
                status => return status,
            }
        }
        for position in 0..self.order.len() {
            let index = self.order[position];
            if self.entries[index].held {
                continue;
            }
            match self.entries[index].acquire() {
                MUTEX_STATUS_ACQUIRED => self.keep(index),
                MUTEX_STATUS_PENDING => {
                    self.pending = Some(index);
                    self.restart = kept;
                    self.release_acquired();
                    return MUTEX_STATUS_PENDING;
                }
                status => {
                    self.release_acquired();
                    return status;
                }
            }
        }
        MUTEX_STATUS_ACQUIRED
    }

    fn keep(&mut self, index: usize) {
        self.entries[index].held = true;
        self.acquired.push(index);
    }

    fn complete(&self) -> bool {
        self.acquired.len() == self.entries.len()
    }

    fn is_owner(&self) -> bool {
        self.owner.is_some() && self.owner == crate::scheduler::current_task()
    }

    fn cleanup(&mut self) {
        // A pending handoff is not in `acquired` yet. Purge compensates it
        // exactly once, even when the scheduler already removed the link.
        if self.pending.take().is_some() {
            willow_async_mutex_cancel();
        }
        self.release_acquired();
    }

    fn release_acquired(&mut self) {
        while let Some(index) = self.acquired.pop() {
            let entry = &mut self.entries[index];
            entry.held = false;
            #[cfg(test)]
            self.released.push(entry.id);
            entry.release();
        }
    }
}

/// A native, compiler-owned pointer; never expose it as a Willow object.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_lock_group_new() -> *mut c_void {
    let _guard = NoPreemptGuard::enter();
    Box::into_raw(Box::new(LockGroup::new())).cast()
}

/// Add a live, rooted lock before acquisition. Mode: mutex=0, read=1, write=2.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_lock_group_add(group: *mut c_void, handle: *mut c_void, mode: i32) {
    let _guard = NoPreemptGuard::enter();
    let Some(group) = (unsafe { group.cast::<LockGroup>().as_mut() }) else {
        crate::panic_context::fatal_invariant("null simultaneous-lock group during construction");
    };
    if group.prepared || handle.is_null() || !(0..=2).contains(&mode) {
        crate::panic_context::fatal_invariant("invalid simultaneous-lock group entry");
    }
    // The compiler proves the target's type and supplies the matching mode.
    let id = if mode == 0 {
        unsafe { &*handle.cast::<AsyncMutex>() }.lock_id()
    } else {
        unsafe { &*handle.cast::<AsyncRwLock>() }.lock_id()
    };
    group.entries.push(Entry {
        handle: GcRootHandle::new(handle.cast()),
        id,
        mode,
        token: 0,
        held: false,
    });
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_lock_group_acquire(group: *mut c_void, out_token: *mut i64) -> i32 {
    let _guard = NoPreemptGuard::enter();
    let (Some(group), Some(task)) = (
        unsafe { group.cast::<LockGroup>().as_mut() },
        crate::scheduler::current_task(),
    ) else {
        return MUTEX_STATUS_LOST;
    };
    if group.owner.is_some_and(|owner| owner != task) {
        return MUTEX_STATUS_LOST;
    }
    group.owner = Some(task);
    if !out_token.is_null() {
        unsafe { *out_token = group.generation };
    }
    group.drive()
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_lock_group_poll(group: *mut c_void, token: i64) -> i32 {
    let _guard = NoPreemptGuard::enter();
    let Some(group) = (unsafe { group.cast::<LockGroup>().as_mut() }) else {
        return MUTEX_STATUS_LOST;
    };
    if !group.is_owner() || group.generation != token {
        return MUTEX_STATUS_LOST;
    }
    group.drive()
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_lock_group_load(group: *mut c_void, index: i64) -> i64 {
    let _guard = NoPreemptGuard::enter();
    let Some(group) = (unsafe { group.cast::<LockGroup>().as_ref() }) else {
        return 0;
    };
    if !group.is_owner() || !group.complete() {
        return 0;
    }
    let Some(entry) = usize::try_from(index)
        .ok()
        .and_then(|i| group.entries.get(i))
    else {
        return 0;
    };
    if entry.mode == 0 {
        willow_async_mutex_load(entry.raw(), entry.token)
    } else {
        willow_async_rwlock_load(entry.raw(), entry.token)
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_lock_group_commit(group: *mut c_void, index: i64, value: i64) -> i32 {
    let _guard = NoPreemptGuard::enter();
    let Some(group) = (unsafe { group.cast::<LockGroup>().as_ref() }) else {
        return 0;
    };
    if !group.is_owner() || !group.complete() {
        return 0;
    }
    let Some(entry) = usize::try_from(index)
        .ok()
        .and_then(|i| group.entries.get(i))
    else {
        return 0;
    };
    match entry.mode {
        0 => willow_async_mutex_commit(entry.raw(), entry.token, value),
        2 => willow_async_rwlock_commit(entry.raw(), entry.token, value),
        _ => 0,
    }
}

/// Release in reverse acquisition order and retire the native group.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_lock_group_release(group: *mut c_void, token: i64) -> i32 {
    let _guard = NoPreemptGuard::enter();
    let Some(borrowed) = (unsafe { group.cast::<LockGroup>().as_ref() }) else {
        return 0;
    };
    if !borrowed.is_owner() || borrowed.generation != token {
        return 0;
    }
    let mut group = unsafe { Box::from_raw(group.cast::<LockGroup>()) };
    group.cleanup();
    1
}

/// Cancel a partial acquisition or release a held group, then retire it.
/// The caller first commits initialized protected bindings, if any.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_lock_group_cancel(group: *mut c_void) {
    let _guard = NoPreemptGuard::enter();
    let Some(borrowed) = (unsafe { group.cast::<LockGroup>().as_ref() }) else {
        return;
    };
    // A group created before the first acquire can be cancelled as well.
    if borrowed.owner.is_some() && !borrowed.is_owner() {
        return;
    }
    let mut group = unsafe { Box::from_raw(group.cast::<LockGroup>()) };
    group.cleanup();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::runtime_test_guard;
    use crate::scheduler::{
        reset_global_scheduler_for_test, with_current_task_for_test, with_global_for_test,
    };

    fn task() -> RuntimeTaskId {
        with_global_for_test(|scheduler| scheduler.spawn_parked_placeholder())
    }

    fn mutex(value: i64) -> GcRootHandle {
        GcRootHandle::new(willow_async_mutex_new(value, 0).cast())
    }

    fn rwlock(value: i64) -> GcRootHandle {
        GcRootHandle::new(willow_async_rwlock_new(value, 0).cast())
    }

    #[test]
    fn group_source_bindings_follow_identity_order_and_reverse_release() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let a = mutex(10);
        let b = mutex(20);
        let c = mutex(30);
        with_current_task_for_test(task(), || {
            let raw = willow_lock_group_new();
            for handle in [&c, &a, &b] {
                willow_lock_group_add(raw, handle.load().cast(), 0);
            }
            let mut token = 0;
            assert_eq!(
                willow_lock_group_acquire(raw, &mut token),
                MUTEX_STATUS_ACQUIRED
            );
            assert_eq!(willow_lock_group_load(raw, 0), 30);
            assert_eq!(willow_lock_group_load(raw, 1), 10);
            assert_eq!(willow_lock_group_load(raw, 2), 20);
            assert_eq!(willow_lock_group_commit(raw, 1, 42), 1);
            let group = unsafe { &mut *raw.cast::<LockGroup>() };
            assert_eq!(group.order, [1, 2, 0]);
            let expected = group
                .order
                .iter()
                .rev()
                .map(|&i| group.entries[i].id)
                .collect::<Vec<_>>();
            group.cleanup();
            assert_eq!(group.released, expected);
            assert_eq!(willow_lock_group_release(raw, token), 1);
            let mut acquired = 0;
            assert_eq!(
                willow_async_mutex_acquire(a.load().cast(), &mut acquired),
                MUTEX_STATUS_ACQUIRED
            );
            assert_eq!(willow_async_mutex_load(a.load().cast(), acquired), 42);
            assert_eq!(willow_async_mutex_release(a.load().cast(), acquired), 1);
        });
    }

    #[test]
    fn group_duplicate_identity_rejects_before_any_acquisition() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let a = mutex(10);
        let b = mutex(20);
        with_current_task_for_test(task(), || {
            let raw = willow_lock_group_new();
            for handle in [&a, &b, &b] {
                willow_lock_group_add(raw, handle.load().cast(), 0);
            }
            let mut token = 0;
            assert_eq!(
                willow_lock_group_acquire(raw, &mut token),
                MUTEX_STATUS_RECURSIVE
            );
            assert!(unsafe { &*raw.cast::<LockGroup>() }.acquired.is_empty());
            willow_lock_group_cancel(raw);
            assert!(unsafe { &*a.load().cast::<AsyncMutex>() }.owner().is_none());
        });
    }

    #[test]
    fn group_mixed_modes_preserve_read_only_bindings() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let a = mutex(10);
        let b = rwlock(20);
        let c = rwlock(30);
        with_current_task_for_test(task(), || {
            let raw = willow_lock_group_new();
            willow_lock_group_add(raw, c.load().cast(), 2);
            willow_lock_group_add(raw, a.load().cast(), 0);
            willow_lock_group_add(raw, b.load().cast(), 1);
            let mut token = 0;
            assert_eq!(
                willow_lock_group_acquire(raw, &mut token),
                MUTEX_STATUS_ACQUIRED
            );
            assert_eq!(willow_lock_group_load(raw, 2), 20);
            assert_eq!(willow_lock_group_commit(raw, 2, 99), 0);
            assert_eq!(willow_lock_group_commit(raw, 0, 99), 1);
            assert_eq!(willow_lock_group_release(raw, token), 1);
        });
    }

    #[test]
    fn group_read_read_duplicate_is_rejected() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let a = rwlock(10);
        with_current_task_for_test(task(), || {
            let raw = willow_lock_group_new();
            willow_lock_group_add(raw, a.load().cast(), 1);
            willow_lock_group_add(raw, a.load().cast(), 1);
            assert_eq!(
                willow_lock_group_acquire(raw, std::ptr::null_mut()),
                MUTEX_STATUS_RECURSIVE
            );
            willow_lock_group_cancel(raw);
        });
    }

    #[test]
    fn group_partial_wait_cancellation_releases_acquired_members() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let a = mutex(10);
        let b = mutex(20);
        let owner = task();
        let waiter = task();
        let mut held = 0;
        with_current_task_for_test(owner, || {
            assert_eq!(
                willow_async_mutex_acquire(b.load().cast(), &mut held),
                MUTEX_STATUS_ACQUIRED
            );
        });
        with_current_task_for_test(waiter, || {
            let raw = willow_lock_group_new();
            willow_lock_group_add(raw, b.load().cast(), 0);
            willow_lock_group_add(raw, a.load().cast(), 0);
            let mut token = 0;
            assert_eq!(
                willow_lock_group_acquire(raw, &mut token),
                MUTEX_STATUS_PENDING
            );
            assert!(unsafe { &*raw.cast::<LockGroup>() }.acquired.is_empty());
            assert!(unsafe { &*a.load().cast::<AsyncMutex>() }.owner().is_none());
            assert_eq!(willow_lock_group_poll(raw, token), MUTEX_STATUS_PENDING);
            willow_lock_group_cancel(raw);
            assert!(crate::lock_wait::lock_wait_link_of(waiter).is_none());
            assert!(unsafe { &*a.load().cast::<AsyncMutex>() }.owner().is_none());
        });
        with_current_task_for_test(owner, || {
            assert_eq!(willow_async_mutex_release(b.load().cast(), held), 1);
        });
    }

    #[test]
    fn group_handoff_poll_finishes_remaining_acquisitions() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let a = mutex(10);
        let b = mutex(20);
        let c = mutex(30);
        let owner = task();
        let waiter = task();
        let mut held = 0;
        let mut token = 0;
        let raw = willow_lock_group_new();
        for handle in [&c, &b, &a] {
            willow_lock_group_add(raw, handle.load().cast(), 0);
        }
        with_current_task_for_test(owner, || {
            assert_eq!(
                willow_async_mutex_acquire(b.load().cast(), &mut held),
                MUTEX_STATUS_ACQUIRED
            );
        });
        with_current_task_for_test(waiter, || {
            assert_eq!(
                willow_lock_group_acquire(raw, &mut token),
                MUTEX_STATUS_PENDING
            );
            assert_eq!(
                willow_lock_group_poll(raw, token.wrapping_add(1)),
                MUTEX_STATUS_LOST
            );
        });
        with_current_task_for_test(owner, || {
            assert_eq!(willow_async_mutex_release(b.load().cast(), held), 1);
        });
        with_current_task_for_test(waiter, || {
            assert_eq!(willow_lock_group_poll(raw, token), MUTEX_STATUS_ACQUIRED);
            assert_eq!(willow_lock_group_load(raw, 0), 30);
            assert_eq!(willow_lock_group_release(raw, token), 1);
        });
    }

    #[test]
    fn group_handoff_cancellation_does_not_double_release() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let a = mutex(10);
        let owner = task();
        let waiter = task();
        let successor = task();
        let mut held = 0;
        let mut successor_token = 0;
        let raw = willow_lock_group_new();
        willow_lock_group_add(raw, a.load().cast(), 0);
        with_current_task_for_test(owner, || {
            assert_eq!(
                willow_async_mutex_acquire(a.load().cast(), &mut held),
                MUTEX_STATUS_ACQUIRED
            );
        });
        with_current_task_for_test(waiter, || {
            assert_eq!(
                willow_lock_group_acquire(raw, std::ptr::null_mut()),
                MUTEX_STATUS_PENDING
            );
        });
        with_current_task_for_test(successor, || {
            assert_eq!(
                willow_async_mutex_acquire(a.load().cast(), &mut successor_token),
                MUTEX_STATUS_PENDING
            );
        });
        with_current_task_for_test(owner, || {
            assert_eq!(willow_async_mutex_release(a.load().cast(), held), 1);
        });
        with_current_task_for_test(waiter, || willow_lock_group_cancel(raw));
        with_current_task_for_test(successor, || {
            assert_eq!(
                willow_async_mutex_poll(a.load().cast(), successor_token),
                MUTEX_STATUS_ACQUIRED
            );
            assert_eq!(
                willow_async_mutex_release(a.load().cast(), successor_token),
                1
            );
        });
    }

    #[test]
    fn group_handoff_beats_a_steady_queue_on_any_member() {
        for contended in 0..2 {
            let _guard = runtime_test_guard();
            reset_global_scheduler_for_test();
            let locks = [mutex(10), mutex(20)];
            let lock = locks[contended].load().cast();
            let owner = task();
            let grouped = task();
            let successor = task();
            let mut held = 0;
            let mut token = 0;
            let mut successor_token = 0;
            let raw = willow_lock_group_new();
            for handle in &locks {
                willow_lock_group_add(raw, handle.load().cast(), 0);
            }
            with_current_task_for_test(owner, || {
                assert_eq!(
                    willow_async_mutex_acquire(lock, &mut held),
                    MUTEX_STATUS_ACQUIRED
                );
            });
            with_current_task_for_test(grouped, || {
                assert_eq!(
                    willow_lock_group_acquire(raw, &mut token),
                    MUTEX_STATUS_PENDING
                );
            });
            with_current_task_for_test(successor, || {
                assert_eq!(
                    willow_async_mutex_acquire(lock, &mut successor_token),
                    MUTEX_STATUS_PENDING
                );
            });
            with_current_task_for_test(owner, || {
                assert_eq!(willow_async_mutex_release(lock, held), 1);
            });
            // Giving the handoff back would pass the lock to the successor and
            // requeue the group behind it, for as long as waiters keep coming.
            with_current_task_for_test(grouped, || {
                assert_eq!(willow_lock_group_poll(raw, token), MUTEX_STATUS_ACQUIRED);
                assert_eq!(willow_lock_group_release(raw, token), 1);
            });
            with_current_task_for_test(successor, || {
                assert_eq!(
                    willow_async_mutex_poll(lock, successor_token),
                    MUTEX_STATUS_ACQUIRED
                );
                assert_eq!(willow_async_mutex_release(lock, successor_token), 1);
            });
        }
    }

    #[test]
    fn group_kept_handoff_is_released_before_waiting_on_another_member() {
        for first in 0..2 {
            let _guard = runtime_test_guard();
            reset_global_scheduler_for_test();
            let locks = [mutex(10), mutex(20)];
            let contended = locks[first].load().cast();
            let busy = locks[1 - first].load().cast();
            let owner = task();
            let other = task();
            let grouped = task();
            let mut held = 0;
            let mut busy_token = 0;
            let mut token = 0;
            let raw = willow_lock_group_new();
            for handle in &locks {
                willow_lock_group_add(raw, handle.load().cast(), 0);
            }
            with_current_task_for_test(owner, || {
                assert_eq!(
                    willow_async_mutex_acquire(contended, &mut held),
                    MUTEX_STATUS_ACQUIRED
                );
            });
            with_current_task_for_test(grouped, || {
                assert_eq!(
                    willow_lock_group_acquire(raw, &mut token),
                    MUTEX_STATUS_PENDING
                );
            });
            with_current_task_for_test(other, || {
                assert_eq!(
                    willow_async_mutex_acquire(busy, &mut busy_token),
                    MUTEX_STATUS_ACQUIRED
                );
            });
            with_current_task_for_test(owner, || {
                assert_eq!(willow_async_mutex_release(contended, held), 1);
            });
            with_current_task_for_test(grouped, || {
                assert_eq!(willow_lock_group_poll(raw, token), MUTEX_STATUS_PENDING);
                let group = unsafe { &*raw.cast::<LockGroup>() };
                assert!(group.acquired.is_empty());
                assert!(group.entries.iter().all(|entry| !entry.held));
                assert_eq!(group.pending, Some(1 - first));
            });
            assert!(
                unsafe { &*contended.cast::<AsyncMutex>() }
                    .owner()
                    .is_none()
            );
            with_current_task_for_test(other, || {
                assert_eq!(willow_async_mutex_release(busy, busy_token), 1);
            });
            with_current_task_for_test(grouped, || {
                assert_eq!(willow_lock_group_poll(raw, token), MUTEX_STATUS_ACQUIRED);
                assert_eq!(willow_lock_group_release(raw, token), 1);
            });
        }
    }

    #[test]
    fn group_kept_rwlock_and_mutex_handoffs_load_and_commit() {
        for mode in [1, 2, 0] {
            let _guard = runtime_test_guard();
            reset_global_scheduler_for_test();
            let contended = if mode == 0 { mutex(10) } else { rwlock(10) };
            let other = mutex(20);
            let lock = contended.load().cast();
            let owner = task();
            let grouped = task();
            let mut held = 0;
            let mut token = 0;
            let raw = willow_lock_group_new();
            willow_lock_group_add(raw, lock, mode);
            willow_lock_group_add(raw, other.load().cast(), 0);
            with_current_task_for_test(owner, || {
                let status = if mode == 0 {
                    willow_async_mutex_acquire(lock, &mut held)
                } else {
                    willow_async_rwlock_acquire(lock, 2, &mut held)
                };
                assert_eq!(status, MUTEX_STATUS_ACQUIRED);
            });
            with_current_task_for_test(grouped, || {
                assert_eq!(
                    willow_lock_group_acquire(raw, &mut token),
                    MUTEX_STATUS_PENDING
                );
            });
            with_current_task_for_test(owner, || {
                let released = if mode == 0 {
                    willow_async_mutex_release(lock, held)
                } else {
                    willow_async_rwlock_release(lock, held)
                };
                assert_eq!(released, 1);
            });
            with_current_task_for_test(grouped, || {
                assert_eq!(willow_lock_group_poll(raw, token), MUTEX_STATUS_ACQUIRED);
                assert_eq!(willow_lock_group_load(raw, 0), 10);
                assert_eq!(willow_lock_group_load(raw, 1), 20);
                let writable = i32::from(mode != 1);
                assert_eq!(willow_lock_group_commit(raw, 0, 11), writable);
                assert_eq!(willow_lock_group_load(raw, 0), 10 + i64::from(writable));
                assert_eq!(willow_lock_group_release(raw, token), 1);
            });
        }
    }

    #[test]
    fn group_failed_kept_handoff_restarts_from_lowest_identity() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let low = mutex(10);
        let high = mutex(20);
        let (low_lock, high_lock) = (low.load().cast(), high.load().cast());
        let owner = task();
        let grouped = task();
        let mut held = 0;
        let mut token = 0;
        let raw = willow_lock_group_new();
        willow_lock_group_add(raw, low_lock, 0);
        willow_lock_group_add(raw, high_lock, 0);
        let acquire = |lock, held: &mut i64| {
            with_current_task_for_test(owner, || {
                assert_eq!(
                    willow_async_mutex_acquire(lock, held),
                    MUTEX_STATUS_ACQUIRED
                );
            });
        };
        let release = |lock, held: i64| {
            with_current_task_for_test(owner, || {
                assert_eq!(willow_async_mutex_release(lock, held), 1);
            });
        };
        acquire(high_lock, &mut held);
        with_current_task_for_test(grouped, || {
            assert_eq!(
                willow_lock_group_acquire(raw, &mut token),
                MUTEX_STATUS_PENDING
            );
            let group = unsafe { &*raw.cast::<LockGroup>() };
            assert_eq!(group.pending, Some(1));
            assert!(!group.restart);
        });
        // The high handoff is kept out of order, then the low member is busy.
        let mut low_held = 0;
        acquire(low_lock, &mut low_held);
        release(high_lock, held);
        with_current_task_for_test(grouped, || {
            assert_eq!(willow_lock_group_poll(raw, token), MUTEX_STATUS_PENDING);
            let group = unsafe { &*raw.cast::<LockGroup>() };
            assert_eq!(group.pending, Some(0));
            assert!(group.restart);
        });
        // The next handoff is given back, so the attempt runs in identity order
        // and, with the high member busy, cannot leave a member held.
        let mut high_held = 0;
        acquire(high_lock, &mut high_held);
        release(low_lock, low_held);
        with_current_task_for_test(grouped, || {
            assert_eq!(willow_lock_group_poll(raw, token), MUTEX_STATUS_PENDING);
            let group = unsafe { &*raw.cast::<LockGroup>() };
            assert_eq!(group.pending, Some(1));
            assert!(group.acquired.is_empty());
            assert!(!group.restart);
        });
        release(high_lock, high_held);
        with_current_task_for_test(grouped, || {
            assert_eq!(willow_lock_group_poll(raw, token), MUTEX_STATUS_ACQUIRED);
            assert_eq!(willow_lock_group_release(raw, token), 1);
        });
    }

    #[test]
    fn group_wait_releases_acquired_members_against_reverse_lexical_order() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        // IDs sort B before A; ordinary code instead acquires A then B.
        let b = mutex(20);
        let a = mutex(10);
        let ordinary = task();
        let grouped = task();
        let mut a_token = 0;
        let mut b_token = 0;
        let mut group_token = 0;
        let raw = willow_lock_group_new();
        willow_lock_group_add(raw, a.load().cast(), 0);
        willow_lock_group_add(raw, b.load().cast(), 0);
        with_current_task_for_test(ordinary, || {
            assert_eq!(
                willow_async_mutex_acquire(a.load().cast(), &mut a_token),
                MUTEX_STATUS_ACQUIRED
            );
        });
        // Pause the group deterministically between its two acquisitions.
        // This is the state another worker can observe during `drive`.
        with_current_task_for_test(grouped, || {
            let group = unsafe { &mut *raw.cast::<LockGroup>() };
            group.prepare();
            group.owner = Some(grouped);
            assert_eq!(group.order, [1, 0]);
            assert_eq!(group.entries[1].acquire(), MUTEX_STATUS_ACQUIRED);
            group.keep(1);
        });
        with_current_task_for_test(ordinary, || {
            assert_eq!(
                willow_async_mutex_acquire(b.load().cast(), &mut b_token),
                MUTEX_STATUS_PENDING
            );
        });
        with_current_task_for_test(grouped, || {
            assert_eq!(
                willow_lock_group_acquire(raw, &mut group_token),
                MUTEX_STATUS_PENDING
            );
            let group = unsafe { &*raw.cast::<LockGroup>() };
            assert!(group.acquired.is_empty());
            assert_eq!(group.pending, Some(0));
        });
        // B's handoff lets the ordinary A→B owner finish instead of forming
        // the permanent A→B→A wait cycle of a member held while waiting.
        with_current_task_for_test(ordinary, || {
            assert_eq!(
                willow_async_mutex_poll(b.load().cast(), b_token),
                MUTEX_STATUS_ACQUIRED
            );
            assert_eq!(willow_async_mutex_release(b.load().cast(), b_token), 1);
            assert_eq!(willow_async_mutex_release(a.load().cast(), a_token), 1);
        });
        with_current_task_for_test(grouped, || {
            assert_eq!(
                willow_lock_group_poll(raw, group_token),
                MUTEX_STATUS_ACQUIRED
            );
            assert_eq!(willow_lock_group_load(raw, 0), 10);
            assert_eq!(willow_lock_group_load(raw, 1), 20);
            assert_eq!(willow_lock_group_release(raw, group_token), 1);
        });
    }
}
