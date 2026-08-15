//! Scheduler-aware reader/writer lock runtime (willow-38w.1.5).
//!
//! Readers and writers use the same `(LockId, RegistrationToken)` reverse-link
//! protocol as [`crate::async_mutex`].  The native state admits readers while
//! no writer is active and the FIFO is empty; once anything queues, newcomers
//! cannot barge.  Handoff promotes either one writer or the contiguous reader
//! prefix at the queue head.

use crate::async_mutex::{
    MUTEX_STATUS_ACQUIRED, MUTEX_STATUS_CANCELLED, MUTEX_STATUS_LOST, MUTEX_STATUS_PENDING,
    MUTEX_STATUS_RECURSIVE, MutexCancel, purge_task_lock_wait,
};
use crate::lock_wait::{
    AcquireOutcome, AsyncLockState, LockAccess, LockId, LockWaitPhase, RegistrationToken,
    consume_handoff, lock_wait_link_of,
};
use crate::task::RuntimeTaskId;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

const ASYNC_RWLOCK_TYPE_ID: u32 = 0x10C4_0002;

pub const RWLOCK_MODE_READ: i32 = 1;
pub const RWLOCK_MODE_WRITE: i32 = 2;
pub const RWLOCK_STATUS_PHASE_ACQUIRE: i32 = 0;
pub const RWLOCK_STATUS_PHASE_POLL: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RwAcquire {
    Acquired(RegistrationToken),
    Pending(RegistrationToken),
    Recursive,
    Ineligible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RwResume {
    Acquired,
    Pending,
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RwRelease {
    Released { woken: usize },
    NotOwner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RwReclamationStatus {
    Reclaimable,
    Retained {
        writer: bool,
        readers: usize,
        waiters: usize,
        frame_refs: usize,
    },
}

#[derive(Debug)]
pub struct AsyncRwLock {
    state: Box<AsyncLockState>,
    expected_lock_id: LockId,
    value: AtomicI64,
    is_ref: bool,
    frame_refs: AtomicUsize,
}

impl AsyncRwLock {
    pub fn new(value: i64, is_ref: bool) -> Box<Self> {
        Box::new(Self::new_payload(value, is_ref))
    }

    fn new_payload(value: i64, is_ref: bool) -> Self {
        let state = AsyncLockState::new_rwlock();
        let expected_lock_id = state.lock_id();
        Self {
            state,
            expected_lock_id,
            value: AtomicI64::new(value),
            is_ref,
            frame_refs: AtomicUsize::new(0),
        }
    }

    fn validated_state(&self) -> &AsyncLockState {
        if self.state.lock_id() != self.expected_lock_id {
            crate::panic_context::fatal_invariant(
                "scheduler-aware RwLock handle/state LockId mismatch",
            );
        }
        &self.state
    }

    pub fn lock_id(&self) -> LockId {
        self.validated_state().lock_id()
    }

    pub fn active_readers(&self) -> Vec<(RuntimeTaskId, RegistrationToken)> {
        self.validated_state().active_readers()
    }

    pub fn writer(&self) -> Option<(RuntimeTaskId, RegistrationToken)> {
        self.validated_state().owner()
    }

    pub fn waiter_count(&self) -> usize {
        self.validated_state().waiter_count()
    }

    pub fn frame_refs(&self) -> usize {
        self.frame_refs.load(Ordering::Acquire)
    }

    pub fn acquire(&self, task: RuntimeTaskId, access: LockAccess) -> RwAcquire {
        debug_assert!(matches!(access, LockAccess::Read | LockAccess::Write));
        if self.validated_state().task_has_relationship(task)
            || matches!(lock_wait_link_of(task), Some(link) if link.lock_id == self.lock_id())
        {
            return RwAcquire::Recursive;
        }
        match self.validated_state().acquire_rw_or_register(task, access) {
            AcquireOutcome::Acquired(token) => {
                self.frame_refs.fetch_add(1, Ordering::AcqRel);
                RwAcquire::Acquired(token)
            }
            AcquireOutcome::Queued(token) => RwAcquire::Pending(token),
            AcquireOutcome::Ineligible => RwAcquire::Ineligible,
        }
    }

    pub fn poll_acquire(&self, task: RuntimeTaskId, token: RegistrationToken) -> RwResume {
        if consume_handoff(task, self.lock_id(), token) {
            self.frame_refs.fetch_add(1, Ordering::AcqRel);
            return RwResume::Acquired;
        }
        match lock_wait_link_of(task) {
            Some(link) if link.lock_id == self.lock_id() && link.token == token => {
                match link.phase {
                    LockWaitPhase::Waiting => RwResume::Pending,
                    LockWaitPhase::HandoffOwned => RwResume::Lost,
                }
            }
            _ if self.validated_state().active_access(task, token).is_some() => RwResume::Acquired,
            _ => RwResume::Lost,
        }
    }

    pub fn load(&self, task: RuntimeTaskId, token: RegistrationToken) -> Option<i64> {
        self.validated_state().active_access(task, token)?;
        Some(self.value.load(Ordering::Acquire))
    }

    pub fn commit(&self, task: RuntimeTaskId, token: RegistrationToken, value: i64) -> bool {
        if self.validated_state().active_access(task, token) != Some(LockAccess::Write) {
            return false;
        }
        self.value.store(value, Ordering::Release);
        true
    }

    pub fn release(&self, task: RuntimeTaskId, token: RegistrationToken) -> RwRelease {
        let state = self.validated_state();
        let Some(access) = state.active_access(task, token) else {
            return RwRelease::NotOwner;
        };
        let released = match access {
            LockAccess::Read => state.release_reader(task, token),
            LockAccess::Write => state.release_owner(task, token),
            LockAccess::Mutex => false,
        };
        if !released {
            return RwRelease::NotOwner;
        }
        self.release_frame_ref();
        let woken = state.handoff_rw_waiters_and_wake().len();
        RwRelease::Released { woken }
    }

    fn release_frame_ref(&self) {
        let _ = self
            .frame_refs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(1)
            });
    }

    pub fn reclamation_status(&self) -> RwReclamationStatus {
        let frame_refs = self.frame_refs();
        let readers = self.active_readers().len();
        let waiters = self.waiter_count();
        let writer = self.writer().is_some();
        if frame_refs == 0 && self.validated_state().is_reclaimable() {
            RwReclamationStatus::Reclaimable
        } else {
            RwReclamationStatus::Retained {
                writer,
                readers,
                waiters,
                frame_refs,
            }
        }
    }

    fn gc_root(&self) -> Option<*mut u8> {
        if !self.is_ref {
            return None;
        }
        let word = self.value.load(Ordering::Acquire) as *mut u8;
        (!word.is_null()).then_some(word)
    }
}

unsafe fn trace_async_rwlock(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    let lock = unsafe { &*(payload as *const AsyncRwLock) };
    lock.validated_state();
    if lock.is_ref && lock.value.load(Ordering::Acquire) != 0 {
        slots.push(lock.value.as_ptr().cast::<*mut u8>());
    }
}

unsafe fn drop_async_rwlock(payload: *mut u8) {
    let lock = unsafe { &*(payload as *const AsyncRwLock) };
    if lock.reclamation_status() != RwReclamationStatus::Reclaimable {
        crate::panic_context::fatal_invariant(
            "collector attempted to reclaim an active scheduler-aware RwLock",
        );
    }
    unsafe { std::ptr::drop_in_place(payload as *mut AsyncRwLock) };
    #[cfg(test)]
    ASYNC_RWLOCK_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
}

#[cfg(test)]
static ASYNC_RWLOCK_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
static ASYNC_RWLOCK_REGISTRATION: crate::gc::NativeGcRegistration =
    crate::gc::NativeGcRegistration::new();
const ASYNC_RWLOCK_GC_TYPES: &[crate::gc::NativeGcType] = &[crate::gc::NativeGcType::new(
    ASYNC_RWLOCK_TYPE_ID,
    Some(trace_async_rwlock),
    Some(drop_async_rwlock),
)];

fn ensure_async_rwlock_registered() {
    ASYNC_RWLOCK_REGISTRATION.ensure(ASYNC_RWLOCK_GC_TYPES);
}

/// # Safety
///
/// `raw` must name a live, initialized `AsyncRwLock` GC payload for the whole
/// lifetime of the returned borrow. The caller must keep the owning Willow
/// handle rooted while the reference is used.
unsafe fn rwlock_from_raw<'a>(raw: *mut c_void) -> Option<&'a AsyncRwLock> {
    (!raw.is_null()).then(|| unsafe { &*(raw as *const AsyncRwLock) })
}

fn current_task() -> Option<RuntimeTaskId> {
    let id = crate::scheduler::willow_sched_current_task();
    (id != 0).then_some(id as RuntimeTaskId)
}

fn access_from_abi(mode: i32) -> Option<LockAccess> {
    match mode {
        RWLOCK_MODE_READ => Some(LockAccess::Read),
        RWLOCK_MODE_WRITE => Some(LockAccess::Write),
        _ => None,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_async_rwlock_new(value: i64, is_ref: i64) -> *mut c_void {
    ensure_async_rwlock_registered();
    let payload = crate::gc::willow_alloc_with_layout(
        crate::gc::GcObjectKind::LockHandle,
        ASYNC_RWLOCK_TYPE_ID,
        std::mem::size_of::<AsyncRwLock>() as i64,
        0,
    );
    if payload.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        (payload as *mut AsyncRwLock).write(AsyncRwLock::new_payload(value, is_ref != 0));
    }
    if is_ref != 0 {
        crate::gc::willow_gc_write_barrier(
            payload,
            value as *mut u8,
            crate::gc::GcStoreDestination::AsyncRwLockCell as i64,
        );
    }
    payload as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_async_rwlock_acquire(
    raw: *mut c_void,
    mode: i32,
    out_token: *mut i64,
) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let (Some(lock), Some(task), Some(access)) = (
        unsafe { rwlock_from_raw(raw) },
        current_task(),
        access_from_abi(mode),
    ) else {
        return MUTEX_STATUS_LOST;
    };
    let store = |token: RegistrationToken| {
        if !out_token.is_null() {
            unsafe { *out_token = token as i64 };
        }
    };
    match lock.acquire(task, access) {
        RwAcquire::Acquired(token) => {
            store(token);
            MUTEX_STATUS_ACQUIRED
        }
        RwAcquire::Pending(token) => {
            store(token);
            MUTEX_STATUS_PENDING
        }
        RwAcquire::Recursive => MUTEX_STATUS_RECURSIVE,
        RwAcquire::Ineligible => MUTEX_STATUS_CANCELLED,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_async_rwlock_poll(raw: *mut c_void, token: i64) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let (Some(lock), Some(task)) = (unsafe { rwlock_from_raw(raw) }, current_task()) else {
        return MUTEX_STATUS_LOST;
    };
    match lock.poll_acquire(task, token as RegistrationToken) {
        RwResume::Acquired => MUTEX_STATUS_ACQUIRED,
        RwResume::Pending => MUTEX_STATUS_PENDING,
        RwResume::Lost => MUTEX_STATUS_LOST,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_async_rwlock_load(raw: *mut c_void, token: i64) -> i64 {
    let (Some(lock), Some(task)) = (unsafe { rwlock_from_raw(raw) }, current_task()) else {
        return 0;
    };
    lock.load(task, token as RegistrationToken).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_async_rwlock_commit(raw: *mut c_void, token: i64, value: i64) -> i32 {
    let (Some(lock), Some(task)) = (unsafe { rwlock_from_raw(raw) }, current_task()) else {
        return 0;
    };
    lock.commit(task, token as RegistrationToken, value) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_async_rwlock_release(raw: *mut c_void, token: i64) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let (Some(lock), Some(task)) = (unsafe { rwlock_from_raw(raw) }, current_task()) else {
        return 0;
    };
    matches!(
        lock.release(task, token as RegistrationToken),
        RwRelease::Released { .. }
    ) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_async_rwlock_cancel() -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let Some(task) = current_task() else {
        return 0;
    };
    !matches!(purge_task_lock_wait(task), MutexCancel::NoLink) as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_async_rwlock_recursive_panic(file: *const u8, line: i32, col: i32) {
    crate::panic_context::raise_language_message_at(
        "recursive or upgrade/downgrade acquisition on non-reentrant RwLock",
        file,
        line.into(),
        col.into(),
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_async_rwlock_invalid_status(status: i32, phase: i32) -> ! {
    let phase = match phase {
        RWLOCK_STATUS_PHASE_ACQUIRE => "acquire",
        RWLOCK_STATUS_PHASE_POLL => "poll",
        _ => "unknown phase",
    };
    crate::panic_context::fatal_invariant(&format!(
        "async rwlock returned unknown status {status} during {phase}"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{
        runtime_test_guard, willow_gc_add_runtime_root, willow_gc_collect, willow_gc_init,
        willow_gc_remove_runtime_root,
    };
    use crate::scheduler::{reset_global_scheduler_for_test, with_global_for_test};
    use crate::string::{willow_string_as_str, willow_string_from_str};

    fn parked_task() -> RuntimeTaskId {
        with_global_for_test(|scheduler| scheduler.spawn_parked_placeholder())
    }

    #[test]
    fn two_uncontended_readers_share_the_lock() {
        let lock = AsyncRwLock::new(7, false);
        let RwAcquire::Acquired(a) = lock.acquire(10, LockAccess::Read) else {
            panic!("first reader");
        };
        let RwAcquire::Acquired(b) = lock.acquire(20, LockAccess::Read) else {
            panic!("second reader");
        };
        assert_eq!(lock.load(10, a), Some(7));
        assert_eq!(lock.load(20, b), Some(7));
        assert_eq!(lock.active_readers().len(), 2);
        assert!(matches!(lock.release(10, a), RwRelease::Released { .. }));
        assert!(matches!(lock.release(20, b), RwRelease::Released { .. }));
        assert_eq!(lock.reclamation_status(), RwReclamationStatus::Reclaimable);
    }

    #[test]
    fn only_writer_can_commit() {
        let lock = AsyncRwLock::new(7, false);
        let RwAcquire::Acquired(reader) = lock.acquire(10, LockAccess::Read) else {
            panic!("reader");
        };
        assert!(!lock.commit(10, reader, 8));
        lock.release(10, reader);
        let RwAcquire::Acquired(writer) = lock.acquire(20, LockAccess::Write) else {
            panic!("writer");
        };
        assert!(lock.commit(20, writer, 8));
        assert_eq!(lock.load(20, writer), Some(8));
    }

    #[test]
    fn reclamation_waits_for_readers_writer_waiters_and_frame_refs() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let lock = AsyncRwLock::new(7, false);
        let reader = parked_task();
        let writer = parked_task();

        let RwAcquire::Acquired(reader_token) = lock.acquire(reader, LockAccess::Read) else {
            panic!("reader should acquire immediately");
        };
        let RwAcquire::Pending(writer_token) = lock.acquire(writer, LockAccess::Write) else {
            panic!("writer should queue behind the reader");
        };
        assert_eq!(
            lock.reclamation_status(),
            RwReclamationStatus::Retained {
                writer: false,
                readers: 1,
                waiters: 1,
                frame_refs: 1,
            }
        );

        assert!(matches!(
            lock.release(reader, reader_token),
            RwRelease::Released { woken: 1 }
        ));
        assert_eq!(lock.writer(), Some((writer, writer_token)));
        assert!(matches!(
            lock.reclamation_status(),
            RwReclamationStatus::Retained {
                writer: true,
                readers: 0,
                waiters: 0,
                frame_refs: 0,
            }
        ));

        assert_eq!(lock.poll_acquire(writer, writer_token), RwResume::Acquired);
        assert_eq!(lock.frame_refs(), 1);
        assert!(matches!(
            lock.release(writer, writer_token),
            RwRelease::Released { woken: 0 }
        ));
        assert_eq!(lock.reclamation_status(), RwReclamationStatus::Reclaimable);
    }

    #[test]
    fn queued_writer_is_not_starved_by_continuing_reader_arrivals() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let lock = AsyncRwLock::new(0, false);
        let first_reader = parked_task();
        let writer = parked_task();
        let RwAcquire::Acquired(first_token) = lock.acquire(first_reader, LockAccess::Read) else {
            panic!("first reader should acquire");
        };
        let RwAcquire::Pending(writer_token) = lock.acquire(writer, LockAccess::Write) else {
            panic!("writer should queue");
        };

        let later_readers: Vec<_> = (0..1_000)
            .map(|_| {
                let task = parked_task();
                let RwAcquire::Pending(token) = lock.acquire(task, LockAccess::Read) else {
                    panic!("new readers must queue behind the waiting writer");
                };
                (task, token)
            })
            .collect();

        assert!(matches!(
            lock.release(first_reader, first_token),
            RwRelease::Released { woken: 1 }
        ));
        assert_eq!(lock.writer(), Some((writer, writer_token)));
        assert_eq!(lock.poll_acquire(writer, writer_token), RwResume::Acquired);
        assert!(lock.commit(writer, writer_token, 1));
        assert!(matches!(
            lock.release(writer, writer_token),
            RwRelease::Released { woken: 1_000 }
        ));

        for (task, token) in later_readers {
            assert_eq!(lock.poll_acquire(task, token), RwResume::Acquired);
            assert_eq!(lock.load(task, token), Some(1));
            assert!(matches!(
                lock.release(task, token),
                RwRelease::Released { .. }
            ));
        }
        assert_eq!(lock.reclamation_status(), RwReclamationStatus::Reclaimable);
    }

    #[test]
    fn ten_thousand_mixed_waiters_drain_by_fair_ownership_units() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        const FIRST_READERS: usize = 4_999;
        const SECOND_READERS: usize = 5_000;

        let lock = AsyncRwLock::new(0, false);
        let owner = parked_task();
        let RwAcquire::Acquired(owner_token) = lock.acquire(owner, LockAccess::Write) else {
            panic!("initial writer should acquire");
        };
        let first: Vec<_> = (0..FIRST_READERS)
            .map(|_| {
                let task = parked_task();
                let RwAcquire::Pending(token) = lock.acquire(task, LockAccess::Read) else {
                    panic!("first reader batch must queue");
                };
                (task, token)
            })
            .collect();
        let middle_writer = parked_task();
        let RwAcquire::Pending(middle_token) = lock.acquire(middle_writer, LockAccess::Write)
        else {
            panic!("middle writer must queue");
        };
        let second: Vec<_> = (0..SECOND_READERS)
            .map(|_| {
                let task = parked_task();
                let RwAcquire::Pending(token) = lock.acquire(task, LockAccess::Read) else {
                    panic!("second reader batch must queue");
                };
                (task, token)
            })
            .collect();
        assert_eq!(lock.waiter_count(), 10_000);

        assert!(matches!(
            lock.release(owner, owner_token),
            RwRelease::Released {
                woken: FIRST_READERS
            }
        ));
        for &(task, token) in &first {
            assert_eq!(lock.poll_acquire(task, token), RwResume::Acquired);
        }
        for &(task, token) in &first {
            assert!(matches!(
                lock.release(task, token),
                RwRelease::Released { .. }
            ));
        }

        assert_eq!(lock.writer(), Some((middle_writer, middle_token)));
        assert_eq!(
            lock.poll_acquire(middle_writer, middle_token),
            RwResume::Acquired
        );
        assert!(matches!(
            lock.release(middle_writer, middle_token),
            RwRelease::Released {
                woken: SECOND_READERS
            }
        ));
        for (task, token) in second {
            assert_eq!(lock.poll_acquire(task, token), RwResume::Acquired);
            assert!(matches!(
                lock.release(task, token),
                RwRelease::Released { .. }
            ));
        }

        assert!(lock.active_readers().is_empty());
        assert_eq!(lock.writer(), None);
        assert_eq!(lock.waiter_count(), 0);
        assert_eq!(lock.reclamation_status(), RwReclamationStatus::Reclaimable);
    }

    #[test]
    fn gc_managed_handle_traces_value_and_reclaims_native_state() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        willow_gc_init();
        let protected = willow_string_from_str("rw protected");
        let raw = willow_async_rwlock_new(protected as i64, 1);
        willow_gc_add_runtime_root(raw.cast::<u8>());
        let _garbage = willow_string_from_str("garbage");

        willow_gc_collect();
        assert_eq!(unsafe { willow_string_as_str(protected) }, "rw protected");
        let lock = unsafe { rwlock_from_raw(raw) }.expect("rwlock handle");
        assert_eq!(lock.gc_root(), Some(protected));
        assert_eq!(lock.reclamation_status(), RwReclamationStatus::Reclaimable);

        let before = ASYNC_RWLOCK_DROP_COUNT.load(Ordering::SeqCst);
        willow_gc_remove_runtime_root(raw.cast::<u8>());
        willow_gc_collect();
        assert_eq!(ASYNC_RWLOCK_DROP_COUNT.load(Ordering::SeqCst), before + 1);
    }

    #[test]
    fn repeated_unreachable_handles_are_finalized() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        willow_gc_init();
        let before = ASYNC_RWLOCK_DROP_COUNT.load(Ordering::SeqCst);
        for value in 0..1_000 {
            let _ = willow_async_rwlock_new(value, 0);
        }
        willow_gc_collect();
        assert_eq!(
            ASYNC_RWLOCK_DROP_COUNT.load(Ordering::SeqCst),
            before + 1_000,
            "native states must not accumulate for dead handles"
        );
    }
}
