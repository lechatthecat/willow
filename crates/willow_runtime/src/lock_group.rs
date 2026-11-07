//! Ordered simultaneous acquisition of scheduler-aware locks.
//!
//! Entries stay in source order for constant-time binding access. A separate
//! permutation is sorted once by the process-wide lock identity. Only its
//! current entry can be pending; acquired entries are released in reverse.
//! Before waiting, every acquired entry is released. A successful handoff is
//! released before retrying the whole permutation, so group acquisition never
//! holds its prefix while waiting for a lock acquired in a different lexical
//! order elsewhere. The caller's enclosing locks remain its responsibility.
//! The compiler owns this native object until release/cancel and must keep its
//! pointer in a non-GC frame slot. Each lock handle has its own relocatable root.

use crate::async_mutex::*;
use crate::async_rwlock::*;
use crate::gc::GcRootHandle;
use crate::preempt::NoPreemptGuard;
use crate::task::RuntimeTaskId;
use std::ffi::c_void;

#[derive(Default)]
struct SortWork {
    #[cfg(test)]
    key_visits: usize,
    #[cfg(test)]
    bucket_visits: usize,
}

impl SortWork {
    fn key(&mut self) {
        #[cfg(test)]
        {
            self.key_visits += 1;
        }
    }

    fn bucket(&mut self) {
        #[cfg(test)]
        {
            self.bucket_visits += 1;
        }
    }
}

/// Stable least-significant-byte radix ordering. Identities have fixed width,
/// so eight passes give O(n + 256) space and O(n + 256) time, including adverse
/// permutations and duplicate identities. Counters disappear outside tests.
fn radix_order(order: &mut Vec<usize>, identity: impl Fn(usize) -> u64) -> SortWork {
    let mut work = SortWork::default();
    if order.len() < 2 {
        return work;
    }
    let mut scratch = vec![0; order.len()];
    for shift in (0..64).step_by(8) {
        let mut offsets = [0usize; 256];
        for &index in order.iter() {
            offsets[((identity(index) >> shift) & 255) as usize] += 1;
            work.key();
        }
        let mut total = 0;
        for offset in &mut offsets {
            let count = *offset;
            *offset = total;
            total += count;
            work.bucket();
        }
        for &index in order.iter() {
            let bucket = ((identity(index) >> shift) & 255) as usize;
            scratch[offsets[bucket]] = index;
            offsets[bucket] += 1;
            work.key();
        }
        std::mem::swap(order, &mut scratch);
    }
    work
}

struct Entry {
    handle: GcRootHandle,
    id: u64,
    mode: i32,
    token: i64,
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
    held: usize,
    pending: Option<usize>,
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
            held: 0,
            pending: None,
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
        radix_order(&mut self.order, |i| self.entries[i].id);
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
        if let Some(index) = self.pending {
            debug_assert_eq!(self.held, 0);
            let entry = &self.entries[index];
            match entry.poll() {
                MUTEX_STATUS_ACQUIRED => {
                    // Handoff ownership is a notification to retry, not an
                    // out-of-order member of a partially held permutation.
                    entry.release();
                    self.pending = None;
                }
                MUTEX_STATUS_LOST => {
                    self.pending = None;
                    return MUTEX_STATUS_LOST;
                }
                status => return status,
            }
        }
        while self.held < self.order.len() {
            let index = self.order[self.held];
            match self.entries[index].acquire() {
                MUTEX_STATUS_ACQUIRED => {
                    self.held += 1;
                }
                MUTEX_STATUS_PENDING => {
                    self.pending = Some(index);
                    self.release_prefix();
                    return MUTEX_STATUS_PENDING;
                }
                status => {
                    self.release_prefix();
                    return status;
                }
            }
        }
        MUTEX_STATUS_ACQUIRED
    }

    fn is_owner(&self) -> bool {
        self.owner.is_some() && self.owner == crate::scheduler::current_task()
    }

    fn cleanup(&mut self) {
        // A pending handoff was never counted in `held`. Purge compensates it
        // exactly once, even when the scheduler already removed the link.
        if self.pending.take().is_some() {
            willow_async_mutex_cancel();
        }
        self.release_prefix();
    }

    fn release_prefix(&mut self) {
        while self.held > 0 {
            self.held -= 1;
            let entry = &self.entries[self.order[self.held]];
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
    if !group.is_owner() || group.held != group.entries.len() {
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
    if !group.is_owner() || group.held != group.entries.len() {
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
    fn group_radix_order_has_linear_deterministic_work_for_adverse_shapes() {
        for size in [16, 64, 256, 1024, 4096] {
            let increasing: Vec<u64> = (0..size as u64).collect();
            let reverse: Vec<u64> = increasing.iter().rev().copied().collect();
            let mut seed = 0x7d5a_f329_18bc_046eu64;
            let random: Vec<u64> = (0..size)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed
                })
                .collect();
            for ids in [increasing, reverse, random] {
                let mut order: Vec<_> = (0..size).collect();
                let mut expected = order.clone();
                expected.sort_by_key(|&i| ids[i]);
                let work = radix_order(&mut order, |i| ids[i]);
                assert_eq!(order, expected);
                assert_eq!(work.key_visits, 16 * size);
                assert_eq!(work.bucket_visits, 8 * 256);
            }
        }
    }

    #[test]
    fn group_radix_order_is_stable_for_duplicates_and_full_width_ids() {
        let ids = [u64::MAX, 1 << 63, 0, 1, u64::MAX, 0, 1 << 63, 256];
        let mut order: Vec<_> = (0..ids.len()).collect();
        radix_order(&mut order, |i| ids[i]);
        assert_eq!(order, [2, 5, 3, 7, 1, 6, 0, 4]);
        for size in [0, 1] {
            let mut trivial = (0..size).collect();
            let work = radix_order(&mut trivial, |_| 0);
            assert_eq!(work.key_visits, 0);
            assert_eq!(work.bucket_visits, 0);
        }
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
            assert_eq!(unsafe { &*raw.cast::<LockGroup>() }.held, 0);
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
    fn group_partial_wait_cancellation_releases_owned_prefix() {
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
            assert_eq!(unsafe { &*raw.cast::<LockGroup>() }.held, 0);
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
    fn group_wait_releases_prefix_against_reverse_lexical_order() {
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
            group.held = 1;
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
            assert_eq!(group.held, 0);
            assert_eq!(group.pending, Some(0));
        });
        // B's handoff lets the ordinary A→B owner finish instead of forming
        // the permanent A→B→A wait cycle of a retained group prefix.
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
