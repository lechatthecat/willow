//! Legacy futures are native, nonmoving allocations, separate from task frames.
//! Compatibility ABI v1 (0.1.x) handles are opaque, process-local tokens, never
//! allocation addresses. Tokens are never reused; all operations check type and
//! liveness. See docs/audits/legacy-future-abi.md for versioned migration.
//! Deprecated for new integrations since runtime 0.1.0: use typed Rust owners,
//! Task futures or allocation-free deadline values. The 0.2.x transition will
//! retain v1 symbols and introduce Rust warnings after replacement bindings are
//! published. Removal requires a future breaking ABI release with an announced
//! migration window; no exports are removed here.
//! Constructors transfer one owned handle to the caller. Poll, await and complete
//! borrow that handle; await does not consume it, so repeated awaits remain valid.
//! The owner must call the matching `willow_future_release_*` exactly once after
//! all uses and aliases have ended. Handles are not safe for concurrent access.
//! Pointer results are borrowed payloads: releasing their future does not free
//! the pointee. Generated sleep/yield futures use allocation-free deadline
//! values in `timer`, not these native handles.

use std::any::TypeId;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::trace::{GcRootSet, GcTrace, GcVisitor};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll<T> {
    Ready(T),
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeFutureState<T> {
    Pending,
    Ready(T),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFuture<T> {
    state: RuntimeFutureState<T>,
    roots: GcRootSet,
}

impl<T: Clone> RuntimeFuture<T> {
    pub fn pending() -> Self {
        Self {
            state: RuntimeFutureState::Pending,
            roots: GcRootSet::default(),
        }
    }

    pub fn complete(&mut self, value: T) {
        self.state = RuntimeFutureState::Ready(value);
    }

    pub fn cancel(&mut self) {
        self.state = RuntimeFutureState::Cancelled;
    }

    pub fn poll(&self) -> Poll<T> {
        match &self.state {
            RuntimeFutureState::Ready(value) => Poll::Ready(value.clone()),
            RuntimeFutureState::Pending | RuntimeFutureState::Cancelled => Poll::Pending,
        }
    }

    pub fn roots(&self) -> &GcRootSet {
        &self.roots
    }

    pub fn roots_mut(&mut self) -> &mut GcRootSet {
        &mut self.roots
    }
}

fn ready_future<T: Clone>(value: T) -> RuntimeFuture<T> {
    let mut future = RuntimeFuture::pending();
    future.complete(value);
    future
}

// Only the compatibility ABI registers allocations. Internal Rust users own
// typed values directly. Tokens never repeat, even if Box addresses are reused.
struct Entry {
    address: usize,
    ty: TypeId,
}

#[derive(Default)]
struct Handles {
    next: usize,
    live: HashMap<usize, Entry>,
}

fn handles() -> &'static Mutex<Handles> {
    static HANDLES: OnceLock<Mutex<Handles>> = OnceLock::new();
    HANDLES.get_or_init(Mutex::default)
}

#[cfg(test)]
thread_local! {
    // Per-thread counts isolate scaling evidence from concurrent test cases.
    static HANDLE_OPERATIONS: std::cell::Cell<[usize; 3]> = const {
        std::cell::Cell::new([0; 3])
    };
}

#[cfg(test)]
fn record_handle_operations(insert: usize, lookup: usize, remove: usize) {
    HANDLE_OPERATIONS.with(|counts| {
        let old = counts.get();
        counts.set([old[0] + insert, old[1] + lookup, old[2] + remove]);
    });
}

fn register<T: 'static>(value: T) -> *mut c_void {
    let owned = Box::new(value);
    let mut handles = handles().lock().unwrap_or_else(|e| e.into_inner());
    let Some(token) = handles.next.checked_add(1) else {
        crate::panic_context::fatal_invariant("legacy future handle space exhausted");
    };
    handles.next = token;
    handles.live.insert(
        token,
        Entry {
            address: Box::into_raw(owned) as usize,
            ty: TypeId::of::<T>(),
        },
    );
    #[cfg(test)]
    record_handle_operations(1, 0, 0);
    std::ptr::without_provenance_mut(token)
}

fn checked_address<T: 'static>(raw: *mut c_void, release: bool) -> *mut T {
    if raw.is_null() {
        crate::panic_context::fatal_invariant("null legacy future handle");
    }
    let mut handles = handles().lock().unwrap_or_else(|e| e.into_inner());
    let token = raw.addr();
    let Some(entry) = handles.live.get(&token) else {
        crate::panic_context::fatal_invariant("invalid or released legacy future handle");
    };
    if entry.ty != TypeId::of::<T>() {
        crate::panic_context::fatal_invariant("legacy future handle type mismatch");
    }
    let address = entry.address as *mut T;
    if release {
        handles.live.remove(&token);
    }
    #[cfg(test)]
    record_handle_operations(0, 1, usize::from(release));
    address
}

fn into_raw<T: 'static>(future: RuntimeFuture<T>) -> *mut c_void {
    register(future)
}

/// The caller must have ended all borrows; lookup rejects stale/wrong-type tokens.
unsafe fn release_raw<T: 'static>(raw: *mut c_void) {
    let address = checked_address::<T>(raw, true);
    // Drop outside the registry lock: destructors may create/release futures.
    unsafe { drop(Box::from_raw(address)) };
}

/// Typed ownership for Rust compatibility callers. No Clone or raw accessor:
/// the borrowed value cannot outlive its owner or be released independently.
pub struct LegacyFutureOwner<T> {
    value: T,
}

impl<T> LegacyFutureOwner<T> {
    pub fn new(value: T) -> Self {
        Self { value }
    }

    pub fn get(&self) -> &T {
        &self.value
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T: 'static> LegacyFutureOwner<T> {
    /// Transfer a compatibility handle into typed Rust ownership, invalidating
    /// the token. The allocation is reclaimed without waiting for completion.
    ///
    /// # Safety
    /// All borrowers and aliases of the handle must have ended. The caller
    /// transfers its sole ownership and must never use the token again.
    pub unsafe fn from_abi(raw: *mut c_void) -> Self {
        let address = checked_address::<T>(raw, true);
        Self::new(unsafe { *Box::from_raw(address) })
    }
}

macro_rules! release_abi {
    ($name:ident, $ty:ty) => {
        /// Release an owned legacy future and its native storage.
        ///
        /// # Safety
        /// The handle must come from the matching typed constructor, remain
        /// live, and have no remaining borrowers or concurrent users. This
        /// consumes it; using or releasing it again is invalid. A null handle
        /// is a fatal invariant violation. Pointer payloads are not freed.
        #[unsafe(no_mangle)]
        #[willow_runtime_macros::ffi_boundary]
        pub unsafe extern "C" fn $name(raw: *mut c_void) {
            unsafe { release_raw::<$ty>(raw) };
        }
    };
}

release_abi!(willow_future_release_void, WillowFutureVoid);
release_abi!(willow_future_release_i64, RuntimeFuture<i64>);
release_abi!(willow_future_release_bool, RuntimeFuture<u8>);
release_abi!(willow_future_release_f64, RuntimeFuture<f64>);
release_abi!(willow_future_release_ptr, RuntimeFuture<*mut c_void>);

/// # Safety
///
/// `raw` must have been created for `RuntimeFuture<T>` and remain allocated for
/// the lifetime of the returned reference.
unsafe fn future_from_raw<'a, T: Clone + 'static>(raw: *mut c_void) -> &'a RuntimeFuture<T> {
    unsafe { &*checked_address::<RuntimeFuture<T>>(raw, false) }
}

/// # Safety
///
/// In addition to naming a live `RuntimeFuture<T>`, `raw` must be exclusively
/// accessible for the lifetime of the returned mutable reference.
unsafe fn future_from_raw_mut<'a, T: Clone + 'static>(
    raw: *mut c_void,
) -> &'a mut RuntimeFuture<T> {
    unsafe { &mut *checked_address::<RuntimeFuture<T>>(raw, false) }
}

// ---------------------------------------------------------------------------
// WillowFutureVoid — unified void future type supporting timer-based sleep.
// ---------------------------------------------------------------------------

/// Unified void future: can be ready, unconditionally pending, or timer-based.
pub enum WillowFutureVoid {
    Ready,
    Pending,
    Sleep { deadline: Instant },
}

impl WillowFutureVoid {
    pub fn ready() -> Self {
        WillowFutureVoid::Ready
    }

    pub fn pending() -> Self {
        WillowFutureVoid::Pending
    }

    pub fn sleep_after_millis(ms: i64) -> Self {
        let millis = ms.max(0) as u64;
        WillowFutureVoid::Sleep {
            deadline: Instant::now() + Duration::from_millis(millis),
        }
    }

    pub fn is_ready(&self) -> bool {
        match self {
            WillowFutureVoid::Ready => true,
            WillowFutureVoid::Pending => false,
            WillowFutureVoid::Sleep { deadline } => Instant::now() >= *deadline,
        }
    }

    /// Wait for a timer, returning false when native-stack cancellation
    /// interrupts it. Ready/pending legacy variants keep their no-wait behavior.
    pub fn block_until_ready(&self) -> bool {
        if let WillowFutureVoid::Sleep { deadline } = self {
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
            if crate::native_stack::is_active() {
                // Both the legacy handle and generated deadline-value await
                // hold no managed pointer in their runtime frames, so the
                // parked task's root slots may be rewritten.
                let _relocation = crate::gc::RelocationSafeScope::enter();
                while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                    if crate::native_stack::cancelled() != 0 {
                        return false;
                    }
                    // Round up so a sub-millisecond tail does not busy-poll.
                    let millis = remaining
                        .as_millis()
                        .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0));
                    crate::scheduler::willow_sched_sleep(millis.min(i64::MAX as u128) as i64);
                    crate::native_stack::suspend_with_result(crate::task::RUNTIME_POLL_PENDING);
                }
                return true;
            }
            let remaining = deadline.checked_duration_since(Instant::now());
            if let Some(remaining) = remaining {
                std::thread::sleep(remaining);
            }
        }
        true
    }
}

fn void_future_into_raw(future: WillowFutureVoid) -> *mut c_void {
    register(future)
}

pub fn void_future_into_raw_pub(future: WillowFutureVoid) -> *mut c_void {
    void_future_into_raw(future)
}

/// # Safety
///
/// `raw` must name a live `WillowFutureVoid` allocated by this module.
unsafe fn void_future_from_raw<'a>(raw: *mut c_void) -> &'a WillowFutureVoid {
    unsafe { &*checked_address::<WillowFutureVoid>(raw, false) }
}

// ---------------------------------------------------------------------------
// Void future ABI (uses WillowFutureVoid).
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_ready_void() -> *mut c_void {
    void_future_into_raw(WillowFutureVoid::ready())
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_pending_void() -> *mut c_void {
    void_future_into_raw(WillowFutureVoid::pending())
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_is_ready_void(raw: *mut c_void) -> u8 {
    u8::from(unsafe { void_future_from_raw(raw) }.is_ready())
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_ready_i64(value: i64) -> *mut c_void {
    into_raw(ready_future(value))
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_ready_bool(value: u8) -> *mut c_void {
    into_raw(ready_future(if value == 0 { 0_u8 } else { 1_u8 }))
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_ready_f64(value: f64) -> *mut c_void {
    into_raw(ready_future(value))
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_ready_ptr(value: *mut c_void) -> *mut c_void {
    into_raw(ready_future(value))
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_await_void(raw: *mut c_void) -> u8 {
    unsafe { void_future_from_raw(raw) }.block_until_ready();
    0
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_is_ready_i64(raw: *mut c_void) -> u8 {
    match unsafe { future_from_raw::<i64>(raw) }.poll() {
        Poll::Ready(_) => 1,
        Poll::Pending => 0,
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_await_i64(raw: *mut c_void) -> i64 {
    match unsafe { future_from_raw::<i64>(raw) }.poll() {
        Poll::Ready(value) => value,
        Poll::Pending => 0,
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_pending_i64() -> *mut c_void {
    into_raw(RuntimeFuture::<i64>::pending())
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_is_ready_bool(raw: *mut c_void) -> u8 {
    match unsafe { future_from_raw::<u8>(raw) }.poll() {
        Poll::Ready(_) => 1,
        Poll::Pending => 0,
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_await_bool(raw: *mut c_void) -> u8 {
    match unsafe { future_from_raw::<u8>(raw) }.poll() {
        Poll::Ready(value) => {
            if value == 0 {
                0
            } else {
                1
            }
        }
        Poll::Pending => 0,
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_pending_bool() -> *mut c_void {
    into_raw(RuntimeFuture::<u8>::pending())
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_is_ready_f64(raw: *mut c_void) -> u8 {
    match unsafe { future_from_raw::<f64>(raw) }.poll() {
        Poll::Ready(_) => 1,
        Poll::Pending => 0,
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_await_f64(raw: *mut c_void) -> f64 {
    match unsafe { future_from_raw::<f64>(raw) }.poll() {
        Poll::Ready(value) => value,
        Poll::Pending => 0.0,
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_pending_f64() -> *mut c_void {
    into_raw(RuntimeFuture::<f64>::pending())
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_is_ready_ptr(raw: *mut c_void) -> u8 {
    match unsafe { future_from_raw::<*mut c_void>(raw) }.poll() {
        Poll::Ready(_) => 1,
        Poll::Pending => 0,
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_await_ptr(raw: *mut c_void) -> *mut c_void {
    match unsafe { future_from_raw::<*mut c_void>(raw) }.poll() {
        Poll::Ready(value) => value,
        Poll::Pending => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_pending_ptr() -> *mut c_void {
    into_raw(RuntimeFuture::<*mut c_void>::pending())
}

/// Complete a pending i64 future with a value (called by executor when async fn finishes).
/// Returns 1 on success. A null handle is a fatal runtime invariant violation.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_complete_i64(raw: *mut c_void, value: i64) -> u8 {
    unsafe { future_from_raw_mut::<i64>(raw) }.complete(value);
    1
}

impl<T: GcTrace> GcTrace for RuntimeFuture<T> {
    fn trace(&self, visitor: &mut GcVisitor) {
        self.roots.trace(visitor);
        if let RuntimeFutureState::Ready(value) = &self.state {
            value.trace(visitor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_owners_adopt_each_abi_type() {
        let void = willow_future_ready_void();
        let i64 = willow_future_ready_i64(42);
        let boolean = willow_future_ready_bool(7);
        let f64 = willow_future_ready_f64(3.5);
        let ptr = willow_future_ready_ptr(std::ptr::null_mut());
        let void_owner = unsafe { LegacyFutureOwner::<WillowFutureVoid>::from_abi(void) };
        let i64_owner = unsafe { LegacyFutureOwner::<RuntimeFuture<i64>>::from_abi(i64) };
        let bool_owner = unsafe { LegacyFutureOwner::<RuntimeFuture<u8>>::from_abi(boolean) };
        let f64_owner = unsafe { LegacyFutureOwner::<RuntimeFuture<f64>>::from_abi(f64) };
        let ptr_owner = unsafe { LegacyFutureOwner::<RuntimeFuture<*mut c_void>>::from_abi(ptr) };
        assert!(void_owner.get().is_ready());
        assert_eq!(i64_owner.get().poll(), Poll::Ready(42));
        assert_eq!(bool_owner.get().poll(), Poll::Ready(1));
        assert_eq!(f64_owner.get().poll(), Poll::Ready(3.5));
        assert_eq!(ptr_owner.get().poll(), Poll::Ready(std::ptr::null_mut()));
        let registry = handles().lock().unwrap();
        assert!(
            [void, i64, boolean, f64, ptr]
                .iter()
                .all(|p| !registry.live.contains_key(&p.addr()))
        );
    }

    #[test]
    fn legacy_owner_cleanup_on_panic_and_cancellation() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        #[derive(Clone)]
        struct DropCount(Arc<AtomicUsize>);
        impl Drop for DropCount {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let result = std::panic::catch_unwind({
            let drops = drops.clone();
            move || {
                let raw = into_raw(ready_future(DropCount(drops)));
                let _owner =
                    unsafe { LegacyFutureOwner::<RuntimeFuture<DropCount>>::from_abi(raw) };
                panic!("owner unwinds");
            }
        });
        assert!(result.is_err());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let mut owner = LegacyFutureOwner::new(RuntimeFuture::<i64>::pending());
        owner.get_mut().cancel();
        assert_eq!(owner.get().poll(), Poll::Pending);
        drop(owner);
        // Cancellation abandons ready values too; owner drop releases payload.
        let raw = into_raw(ready_future(DropCount(drops.clone())));
        let mut owner = unsafe { LegacyFutureOwner::<RuntimeFuture<DropCount>>::from_abi(raw) };
        owner.get_mut().cancel();
        assert!(matches!(owner.get().poll(), Poll::Pending));
        drop(owner);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    // Compatibility harness deliberately retains a registered token across the
    // ABI await; ordinary Rust consumers instead use LegacyFutureOwner.
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
    #[test]
    fn legacy_abi_timer_completion_and_cancellation_release_once() {
        use crate::scheduler::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static TOKEN: AtomicUsize = AtomicUsize::new(0);
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        struct VoidAbiOwner(*mut c_void);
        impl Drop for VoidAbiOwner {
            fn drop(&mut self) {
                unsafe {
                    willow_future_release_void(self.0);
                }
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn wait(ms: i64) -> i32 {
            let owner = VoidAbiOwner(crate::timer::willow_runtime_sleep(ms));
            TOKEN.store(owner.0.addr(), Ordering::SeqCst);
            willow_future_await_void(owner.0);
            if ms == 60_000 {
                assert_ne!(crate::native_stack::cancelled(), 0);
            } else {
                assert_eq!(willow_future_is_ready_void(owner.0), 1);
                willow_future_await_void(owner.0);
            }
            crate::task::RUNTIME_POLL_READY
        }
        unsafe extern "C" fn finish(_: *mut c_void) -> i32 {
            wait(1)
        }
        unsafe extern "C" fn cancel(_: *mut c_void) -> i32 {
            wait(60_000)
        }
        let _guard = crate::gc::runtime_test_guard();
        reset_global_scheduler_for_test();
        DROPS.store(0, Ordering::SeqCst);
        for (index, callback) in [finish as crate::task::RuntimePollFn, cancel]
            .into_iter()
            .enumerate()
        {
            let task = willow_sched_spawn(callback, std::ptr::null_mut());
            if index == 1 {
                willow_sched_run_until_deadline(willow_monotonic_millis() + 20);
                assert_eq!(willow_sched_task_state(task), 2);
                let token = TOKEN.load(Ordering::SeqCst);
                assert!(handles().lock().unwrap().live.contains_key(&token));
                willow_sched_cancel(task);
            }
            willow_sched_run_until(task);
            assert_eq!(DROPS.load(Ordering::SeqCst), index + 1);
            let token = TOKEN.load(Ordering::SeqCst);
            assert!(!handles().lock().unwrap().live.contains_key(&token));
        }
        reset_global_scheduler_for_test();
    }

    #[test]
    fn legacy_registry_fragmentation_and_generation_counts() {
        for count in [1_usize, 16, 256, 1024] {
            let before = HANDLE_OPERATIONS.with(|counts| counts.get());
            let tokens: Vec<_> = (0..count)
                .map(|i| willow_future_ready_i64(i as i64))
                .collect();
            for (i, &token) in tokens.iter().enumerate().step_by(2) {
                assert_eq!(willow_future_await_i64(token), i as i64);
                unsafe {
                    willow_future_release_i64(token);
                }
            }
            let replacements: Vec<_> = (0..count.div_ceil(2))
                .map(|_| willow_future_pending_i64())
                .collect();
            {
                let registry = handles().lock().unwrap();
                for (i, &token) in tokens.iter().enumerate() {
                    assert_eq!(registry.live.contains_key(&token.addr()), i % 2 == 1);
                }
                assert!(
                    replacements
                        .iter()
                        .all(|p| p.addr() > tokens.last().unwrap().addr())
                );
            }
            for &token in tokens.iter().skip(1).step_by(2).chain(replacements.iter()) {
                unsafe {
                    willow_future_release_i64(token);
                }
            }
            let registry = handles().lock().unwrap();
            assert!(
                tokens
                    .iter()
                    .chain(replacements.iter())
                    .all(|p| !registry.live.contains_key(&p.addr()))
            );
            let after = HANDLE_OPERATIONS.with(|counts| counts.get());
            let operations = std::array::from_fn::<_, 3, _>(|i| after[i] - before[i]);
            let releases = count + replacements.len();
            assert_eq!(
                operations,
                [releases, releases + count.div_ceil(2), releases]
            );
            println!(
                "legacy-registry originals={count} replacements={} released={} retained_entries=0 map_operations={operations:?}",
                replacements.len(),
                count + replacements.len()
            );
        }
    }

    #[test]
    fn legacy_handles_reject_stale_wrong_type_and_duplicate_release() {
        const CHILD: &str = "WILLOW_TEST_INVALID_LEGACY_HANDLE";
        if let Ok(case) = std::env::var(CHILD) {
            let raw = willow_future_ready_i64(7);
            match case.as_str() {
                "wrong_adopt" => unsafe {
                    LegacyFutureOwner::<WillowFutureVoid>::from_abi(raw);
                },
                "null_adopt" => unsafe {
                    LegacyFutureOwner::<RuntimeFuture<i64>>::from_abi(std::ptr::null_mut());
                },
                "wrong_poll" => {
                    willow_future_is_ready_bool(raw);
                }
                "wrong_await" => {
                    willow_future_await_f64(raw);
                }
                "wrong_release" => unsafe { willow_future_release_ptr(raw) },
                "wrong_complete" => {
                    willow_future_complete_i64(willow_future_ready_bool(1), 5);
                }
                "forged" => {
                    willow_future_await_void(std::ptr::without_provenance_mut(usize::MAX));
                }
                "double_void" => {
                    let raw = willow_future_ready_void();
                    unsafe {
                        willow_future_release_void(raw);
                        willow_future_release_void(raw);
                    }
                }
                "double_bool" => {
                    let raw = willow_future_ready_bool(1);
                    unsafe {
                        willow_future_release_bool(raw);
                        willow_future_release_bool(raw);
                    }
                }
                "double_f64" => {
                    let raw = willow_future_ready_f64(1.0);
                    unsafe {
                        willow_future_release_f64(raw);
                        willow_future_release_f64(raw);
                    }
                }
                "double_ptr" => {
                    let raw = willow_future_ready_ptr(std::ptr::null_mut());
                    unsafe {
                        willow_future_release_ptr(raw);
                        willow_future_release_ptr(raw);
                    }
                }
                _ => {
                    unsafe {
                        willow_future_release_i64(raw);
                    }
                    // Allocating another future cannot resurrect the old token.
                    let other = willow_future_ready_i64(9);
                    assert_ne!(raw, other);
                    match case.as_str() {
                        "double_i64" => unsafe { willow_future_release_i64(raw) },
                        "stale_adopt" => unsafe {
                            LegacyFutureOwner::<RuntimeFuture<i64>>::from_abi(raw);
                        },
                        "stale_poll" => {
                            willow_future_is_ready_i64(raw);
                        }
                        "stale_await" => {
                            willow_future_await_i64(raw);
                        }
                        "stale_complete" => {
                            willow_future_complete_i64(raw, 10);
                        }
                        _ => panic!("unknown case"),
                    }
                }
            }
            panic!("invalid handle returned");
        }
        for case in [
            "wrong_adopt",
            "null_adopt",
            "stale_adopt",
            "wrong_poll",
            "wrong_await",
            "wrong_release",
            "wrong_complete",
            "forged",
            "double_void",
            "double_i64",
            "double_bool",
            "double_f64",
            "double_ptr",
            "stale_poll",
            "stale_await",
            "stale_complete",
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "future::tests::legacy_handles_reject_stale_wrong_type_and_duplicate_release",
                    "--nocapture",
                ])
                .env(CHILD, case)
                .output()
                .unwrap();
            assert!(!output.status.success(), "{case}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            let expected = if case == "null_adopt" {
                "null legacy future handle"
            } else if case.starts_with("wrong") {
                "legacy future handle type mismatch"
            } else {
                "invalid or released legacy future handle"
            };
            assert!(stderr.contains(expected), "{case}: {stderr}");
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TestRoot(usize);

    impl GcTrace for TestRoot {
        fn trace(&self, visitor: &mut GcVisitor) {
            visitor.mark_root(self.0);
        }
    }

    #[test]
    fn future_moves_from_pending_to_ready() {
        let mut future = RuntimeFuture::pending();
        assert_eq!(future.poll(), Poll::Pending);
        future.complete(42);
        assert_eq!(future.poll(), Poll::Ready(42));
    }

    #[test]
    fn future_traces_roots_and_ready_result() {
        let mut future = RuntimeFuture::pending();
        future.roots_mut().push(11);
        future.complete(TestRoot(22));

        let mut visitor = GcVisitor::default();
        future.trace(&mut visitor);

        assert_eq!(visitor.roots(), &[11, 22]);
    }

    #[test]
    fn release_destroys_owned_state_once_at_increasing_sizes() {
        use std::cell::Cell;
        use std::rc::Rc;

        #[derive(Clone)]
        struct DropCount(Rc<Cell<usize>>);
        impl Drop for DropCount {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        for handles in [1, 16, 256] {
            for roots in [0, 16, 256] {
                let drops = Rc::new(Cell::new(0));
                let mut owned = Vec::new();
                for _ in 0..handles {
                    let mut future = ready_future(DropCount(drops.clone()));
                    for root in 1..=roots {
                        future.roots_mut().push(root);
                    }
                    owned.push(into_raw(future));
                }
                assert_eq!(drops.get(), 0);
                for raw in owned {
                    unsafe { release_raw::<RuntimeFuture<DropCount>>(raw) };
                }
                assert_eq!(drops.get(), handles);
                assert_eq!(Rc::strong_count(&drops), 1);
                println!(
                    "handles={handles} roots_per_handle={roots} drops={}",
                    drops.get()
                );
            }
        }
    }

    #[test]
    fn release_ptr_preserves_borrowed_payload() {
        let mut payload = Box::new(73_i64);
        let ptr = (&mut *payload as *mut i64).cast();
        let raw = willow_future_ready_ptr(ptr);
        assert_eq!(willow_future_await_ptr(raw), ptr);
        assert_eq!(willow_future_await_ptr(raw), ptr);
        unsafe { willow_future_release_ptr(raw) };
        assert_eq!(*payload, 73);
        *payload = 91;
        assert_eq!(*payload, 91);
    }

    #[test]
    fn release_sleep_does_not_await_its_deadline() {
        let raw = void_future_into_raw(WillowFutureVoid::sleep_after_millis(3_600_000));
        assert_eq!(willow_future_is_ready_void(raw), 0);
        unsafe { willow_future_release_void(raw) };
    }

    #[test]
    fn future_unit_01_ready_i64_abi_awaits_value() {
        let raw = willow_future_ready_i64(42);
        assert_eq!(willow_future_await_i64(raw), 42);
        unsafe { willow_future_release_i64(raw) };
    }

    #[test]
    fn future_unit_02_ready_bool_abi_canonicalizes_true() {
        let raw = willow_future_ready_bool(7);
        assert_eq!(willow_future_await_bool(raw), 1);
        unsafe { willow_future_release_bool(raw) };
    }

    #[test]
    fn future_unit_03_ready_bool_abi_preserves_false() {
        let raw = willow_future_ready_bool(0);
        assert_eq!(willow_future_await_bool(raw), 0);
        unsafe { willow_future_release_bool(raw) };
    }

    #[test]
    fn future_unit_04_ready_f64_abi_awaits_value() {
        let raw = willow_future_ready_f64(3.5);
        assert_eq!(willow_future_await_f64(raw), 3.5);
        unsafe { willow_future_release_f64(raw) };
    }

    #[test]
    fn future_unit_05_ready_ptr_abi_awaits_value() {
        let mut value = 10_i64;
        let ptr = (&mut value as *mut i64).cast::<c_void>();
        let raw = willow_future_ready_ptr(ptr);
        assert_eq!(willow_future_await_ptr(raw), ptr);
        unsafe { willow_future_release_ptr(raw) };
    }

    #[test]
    fn future_unit_06_ready_void_abi_awaits_unit() {
        let raw = willow_future_ready_void();
        assert_eq!(willow_future_await_void(raw), 0);
        unsafe { willow_future_release_void(raw) };
    }

    #[test]
    fn null_handles_are_process_fatal() {
        const CHILD: &str = "WILLOW_TEST_NULL_FUTURE";
        if let Ok(case) = std::env::var(CHILD) {
            let raw = std::ptr::null_mut();
            match case.as_str() {
                "is_ready_void" => {
                    willow_future_is_ready_void(raw);
                }
                "await_void" => {
                    willow_future_await_void(raw);
                }
                "is_ready_i64" => {
                    willow_future_is_ready_i64(raw);
                }
                "await_i64" => {
                    willow_future_await_i64(raw);
                }
                "is_ready_bool" => {
                    willow_future_is_ready_bool(raw);
                }
                "await_bool" => {
                    willow_future_await_bool(raw);
                }
                "is_ready_f64" => {
                    willow_future_is_ready_f64(raw);
                }
                "await_f64" => {
                    willow_future_await_f64(raw);
                }
                "is_ready_ptr" => {
                    willow_future_is_ready_ptr(raw);
                }
                "await_ptr" => {
                    willow_future_await_ptr(raw);
                }
                "release_void" => unsafe { willow_future_release_void(raw) },
                "release_i64" => unsafe { willow_future_release_i64(raw) },
                "release_bool" => unsafe { willow_future_release_bool(raw) },
                "release_f64" => unsafe { willow_future_release_f64(raw) },
                "release_ptr" => unsafe { willow_future_release_ptr(raw) },
                "complete_i64" => {
                    willow_future_complete_i64(raw, 42);
                }
                _ => panic!("unknown child case"),
            }
            return;
        }
        for case in [
            "is_ready_void",
            "await_void",
            "is_ready_i64",
            "await_i64",
            "is_ready_bool",
            "await_bool",
            "is_ready_f64",
            "await_f64",
            "is_ready_ptr",
            "await_ptr",
            "release_void",
            "release_i64",
            "release_bool",
            "release_f64",
            "release_ptr",
            "complete_i64",
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "future::tests::null_handles_are_process_fatal",
                    "--nocapture",
                ])
                .env(CHILD, case)
                .output()
                .expect("run null future subprocess");
            assert!(!output.status.success(), "{case}: null handle was accepted");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("runtime fatal: null legacy future handle"),
                "{case}: {stderr}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(output.status.signal(), Some(libc::SIGABRT), "{case}");
            }
        }
    }

    #[test]
    fn valid_handles_preserve_readiness_and_completion() {
        macro_rules! check {
            ($release:ident, $pending:ident, $ready:ident, $poll:ident, $await:ident, $value:expr, $zero:expr) => {{
                let raw = $pending();
                assert_eq!($poll(raw), 0);
                assert_eq!($await(raw), $zero);
                unsafe {
                    $release(raw);
                }
                let raw = $ready($value);
                assert_eq!($poll(raw), 1);
                assert_eq!($await(raw), $value);
                assert_eq!($await(raw), $value);
                unsafe {
                    $release(raw);
                }
            }};
        }
        check!(
            willow_future_release_i64,
            willow_future_pending_i64,
            willow_future_ready_i64,
            willow_future_is_ready_i64,
            willow_future_await_i64,
            42,
            0
        );
        check!(
            willow_future_release_bool,
            willow_future_pending_bool,
            willow_future_ready_bool,
            willow_future_is_ready_bool,
            willow_future_await_bool,
            1,
            0
        );
        check!(
            willow_future_release_f64,
            willow_future_pending_f64,
            willow_future_ready_f64,
            willow_future_is_ready_f64,
            willow_future_await_f64,
            3.5,
            0.0
        );
        // A null result pointer is valid; only the future handle must be non-null.
        check!(
            willow_future_release_ptr,
            willow_future_pending_ptr,
            willow_future_ready_ptr,
            willow_future_is_ready_ptr,
            willow_future_await_ptr,
            std::ptr::null_mut(),
            std::ptr::null_mut()
        );
        let raw = willow_future_pending_i64();
        assert_eq!(willow_future_complete_i64(raw, 73), 1);
        assert_eq!(willow_future_is_ready_i64(raw), 1);
        assert_eq!(willow_future_await_i64(raw), 73);
        unsafe {
            willow_future_release_i64(raw);
        }
        for (future, expected) in [
            (WillowFutureVoid::Ready, 1),
            (WillowFutureVoid::Pending, 0),
            (WillowFutureVoid::sleep_after_millis(0), 1),
        ] {
            let raw = void_future_into_raw(future);
            assert_eq!(willow_future_is_ready_void(raw), expected);
            assert_eq!(willow_future_await_void(raw), 0);
            unsafe {
                willow_future_release_void(raw);
            }
        }
    }

    #[test]
    fn future_unit_11_pending_i64_await_returns_zero_for_mvp() {
        let raw = into_raw(RuntimeFuture::<i64>::pending());
        assert_eq!(willow_future_await_i64(raw), 0);
        unsafe { willow_future_release_i64(raw) };
    }

    #[test]
    fn future_unit_12_cancelled_i64_await_returns_zero_for_mvp() {
        let mut future = RuntimeFuture::<i64>::pending();
        future.cancel();
        let raw = into_raw(future);
        assert_eq!(willow_future_await_i64(raw), 0);
        unsafe { willow_future_release_i64(raw) };
    }
}
