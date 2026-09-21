//! C ABI entry points and GC registration for scheduler-aware mutexes.

use super::{
    AsyncMutex, MutexAcquire, MutexCancel, MutexRelease, MutexResume, ReclamationStatus,
    purge_task_lock_wait,
};
use crate::lock_wait::RegistrationToken;
use crate::task::RuntimeTaskId;
use std::os::raw::c_void;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use willow_abi::runtime_type_ids::ASYNC_MUTEX_TYPE_ID;

// ── C ABI ────────────────────────────────────────────────────────────────────
//
// Status codes are shared by the acquire/poll entry points so the generated
// frame branches on one convention. Negative values are failures.

/// Ownership is the caller's; the frame may load the protected value.
pub const MUTEX_STATUS_ACQUIRED: i32 = willow_abi::LockAcquireStatus::Acquired as i32;
/// Registered and parked; the frame returns `Pending`.
pub const MUTEX_STATUS_PENDING: i32 = willow_abi::LockAcquireStatus::Pending as i32;
/// Same task, same mutex: non-reentrant (§11).
pub const MUTEX_STATUS_RECURSIVE: i32 = willow_abi::LockAcquireStatus::Recursive as i32;
/// This acquisition's generation is gone (a revoked reservation, a stale
/// token). Recovery is a brand-new acquire, which either takes an uncontended
/// lock or joins the queue.
pub const MUTEX_STATUS_LOST: i32 = willow_abi::LockAcquireStatus::Lost as i32;
/// No registration was published because the task may not park: it is
/// cancel-requested, already `Cancelling`, or terminal.
///
/// Deliberately distinct from [`MUTEX_STATUS_LOST`] (willow-38w.1.4 review): a
/// retry cannot clear this condition, since only the scheduler can — by
/// claiming the task and running its cancellation entry. Generated code
/// therefore returns `Pending` and gives the worker back instead of re-arming
/// the acquire, which would spin on this status for as long as the current
/// owner holds the lock, and forever when that owner needs this worker.
pub const MUTEX_STATUS_CANCELLED: i32 = willow_abi::LockAcquireStatus::Cancelled as i32;

/// Call-site discriminator for a fatal unknown status. These are wire values
/// shared with generated code so the diagnostic can name the broken ABI edge.
pub const MUTEX_STATUS_PHASE_ACQUIRE: i32 = willow_abi::LockStatusPhase::Acquire as i32;
pub const MUTEX_STATUS_PHASE_POLL: i32 = willow_abi::LockStatusPhase::Poll as i32;

unsafe fn snapshot_async_mutex(payload: *mut u8, children: &mut Vec<*mut u8>) {
    let value = unsafe { &*(payload as *const AsyncMutex) };
    if value.is_ref {
        children.push(value.value.load(Ordering::Acquire) as *mut u8);
    }
}

unsafe fn trace_async_mutex(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    let mutex = unsafe { &*(payload as *const AsyncMutex) };
    mutex.validated_state();
    if mutex.is_ref && mutex.value.load(Ordering::Acquire) != 0 {
        // STW tracing may rewrite this slot when the protected object moves.
        slots.push(mutex.value.as_ptr().cast::<*mut u8>());
    }
}

unsafe fn drop_async_mutex(payload: *mut u8) {
    let mutex = unsafe { &*(payload as *const AsyncMutex) };
    if mutex.reclamation_status() != ReclamationStatus::Reclaimable {
        crate::panic_context::fatal_invariant(
            "collector attempted to reclaim an active scheduler-aware Mutex",
        );
    }
    unsafe { std::ptr::drop_in_place(payload as *mut AsyncMutex) };
    #[cfg(test)]
    ASYNC_MUTEX_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
}

#[cfg(test)]
static ASYNC_MUTEX_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
static ASYNC_MUTEX_REGISTRATION: crate::gc::NativeGcRegistration =
    crate::gc::NativeGcRegistration::new();
const ASYNC_MUTEX_GC_TYPES: &[crate::gc::NativeGcType] = &[crate::gc::NativeGcType::new(
    ASYNC_MUTEX_TYPE_ID,
    Some(trace_async_mutex),
    Some(drop_async_mutex),
)
.with_concurrent_trace(snapshot_async_mutex)];

fn ensure_async_mutex_registered() {
    ASYNC_MUTEX_REGISTRATION.ensure(ASYNC_MUTEX_GC_TYPES);
}

/// # Safety
///
/// `raw` must name a live, initialized `AsyncMutex` GC payload for the whole
/// lifetime of the returned borrow. The caller must also keep the owning
/// Willow handle rooted so collection cannot reclaim it while borrowed.
unsafe fn mutex_from_raw<'a>(raw: *mut c_void) -> Option<&'a AsyncMutex> {
    (!raw.is_null()).then(|| unsafe { &*(raw as *const AsyncMutex) })
}

/// The running task, or `None` outside any task. A lock statement is only legal
/// inside an `async fn` (§6.1), so `None` means the caller is misusing the ABI.
fn current_task() -> Option<RuntimeTaskId> {
    let id = crate::scheduler::willow_sched_current_task();
    (id != 0).then_some(id as RuntimeTaskId)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_new(value: i64, is_ref: i64) -> *mut c_void {
    ensure_async_mutex_registered();
    let payload = crate::gc::willow_alloc_with_layout(
        crate::gc::GcObjectKind::LockHandle,
        ASYNC_MUTEX_TYPE_ID,
        std::mem::size_of::<AsyncMutex>() as i64,
        0,
    );
    if payload.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        (payload as *mut AsyncMutex).write(AsyncMutex::new_payload(value, is_ref != 0));
    }
    if is_ref != 0 {
        crate::gc::willow_gc_write_barrier(
            payload,
            std::ptr::null_mut(),
            value as *mut u8,
            crate::gc::GcStoreDestination::AsyncMutexCell as i64,
        );
    }
    payload as *mut c_void
}

/// Acquire for the running task. Writes the registration token to `out_token`
/// on `ACQUIRED` and `PENDING`; leaves it untouched otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_acquire(raw: *mut c_void, out_token: *mut i64) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let (Some(mutex), Some(task)) = (unsafe { mutex_from_raw(raw) }, current_task()) else {
        return MUTEX_STATUS_LOST;
    };
    let store = |token: RegistrationToken| {
        if !out_token.is_null() {
            unsafe { *out_token = token as i64 };
        }
    };
    match mutex.acquire(task) {
        MutexAcquire::Acquired(token) => {
            store(token);
            MUTEX_STATUS_ACQUIRED
        }
        MutexAcquire::Pending(token) => {
            store(token);
            MUTEX_STATUS_PENDING
        }
        MutexAcquire::Recursive => MUTEX_STATUS_RECURSIVE,
        MutexAcquire::Ineligible => MUTEX_STATUS_CANCELLED,
    }
}

/// Re-poll a pending acquire after a wake.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_poll(raw: *mut c_void, token: i64) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let (Some(mutex), Some(task)) = (unsafe { mutex_from_raw(raw) }, current_task()) else {
        return MUTEX_STATUS_LOST;
    };
    match mutex.poll_acquire(task, token as RegistrationToken) {
        MutexResume::Acquired => MUTEX_STATUS_ACQUIRED,
        MutexResume::Pending => MUTEX_STATUS_PENDING,
        MutexResume::Lost => MUTEX_STATUS_LOST,
    }
}

/// Load the protected value. Returns `0` when the caller does not own the lock;
/// the generated frame only calls this after an `ACQUIRED` status, so a `0` here
/// means the ABI was misused rather than that the value is zero.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_load(raw: *mut c_void, token: i64) -> i64 {
    let (Some(mutex), Some(task)) = (unsafe { mutex_from_raw(raw) }, current_task()) else {
        return 0;
    };
    mutex.load(task, token as RegistrationToken).unwrap_or(0)
}

/// Commit the protected value. Returns 1 on success, 0 if the caller is not the
/// current owner.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_commit(raw: *mut c_void, token: i64, value: i64) -> i32 {
    let (Some(mutex), Some(task)) = (unsafe { mutex_from_raw(raw) }, current_task()) else {
        return 0;
    };
    mutex.commit(task, token as RegistrationToken, value) as i32
}

/// Release ownership and hand off. Returns 1 on success, 0 if the caller was not
/// the owner (a stale or double release).
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_release(raw: *mut c_void, token: i64) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let (Some(mutex), Some(task)) = (unsafe { mutex_from_raw(raw) }, current_task()) else {
        return 0;
    };
    matches!(
        mutex.release(task, token as RegistrationToken),
        MutexRelease::Released { .. }
    ) as i32
}

/// Cancellation cleanup for the running task, whatever phase it is in.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_cancel() -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let Some(task) = current_task() else {
        return 0;
    };
    !matches!(purge_task_lock_wait(task), MutexCancel::NoLink) as i32
}

/// Raise the non-reentrant-mutex fault (§11, §22 "Reentrant lock"). Separate
/// from the acquire entry point so the generated code supplies the source
/// location of the offending `lock` statement.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_recursive_panic(file: *const u8, line: i32, col: i32) {
    crate::panic_context::raise_language_message_at(
        "recursive lock acquisition on non-reentrant Mutex",
        file,
        line.into(),
        col.into(),
    );
}

/// Abort on an async-mutex status outside the closed ABI set. This is not a
/// recoverable Willow panic: accepting an unknown state would either read a
/// value without ownership or retry forever after compiler/runtime drift.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_invalid_status(status: i32, phase: i32) -> ! {
    crate::panic_context::fatal_invariant(&invalid_status_message(status, phase));
}

fn invalid_status_message(status: i32, phase: i32) -> String {
    let phase = match phase {
        MUTEX_STATUS_PHASE_ACQUIRE => "acquire",
        MUTEX_STATUS_PHASE_POLL => "poll",
        _ => "unknown phase",
    };
    format!("async mutex returned unknown status {status} during {phase}")
}

#[cfg(test)]
mod tests;
