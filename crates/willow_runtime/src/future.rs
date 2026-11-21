//! Legacy futures are native, nonmoving allocations, separate from task frames.
//! Compatibility ABI v1 (0.1.x) handles are opaque, process-local tokens, never
//! allocation addresses. Tokens are never reused; all operations check type and
//! liveness.
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
use std::sync::Mutex;
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
    release: unsafe fn(usize),
}
impl Drop for Entry {
    fn drop(&mut self) {
        if self.address != 0 {
            // SAFETY: an entry owns its Box until checked_address transfers it.
            unsafe { (self.release)(self.address) };
        }
    }
}

#[derive(Default)]
pub(crate) struct Handles {
    next: usize,
    live: HashMap<usize, Entry>,
    operations: crate::observation::Value<HashMap<std::thread::ThreadId, [usize; 3]>>,
}

fn handles() -> &'static Mutex<Handles> {
    &crate::runtime::current().future_handles
}

impl Handles {
    fn record(&mut self, insert: usize, lookup: usize, remove: usize) {
        self.operations.observe_mut(|threads| {
            let counts = threads.entry(std::thread::current().id()).or_default();
            counts[0] += insert;
            counts[1] += lookup;
            counts[2] += remove;
        });
    }

    #[cfg(test)]
    fn counts(&self) -> [usize; 3] {
        self.operations
            .get(&std::thread::current().id())
            .copied()
            .unwrap_or_default()
    }

    fn register<T: 'static>(&mut self, value: T) -> *mut c_void {
        unsafe fn release<T>(address: usize) {
            // SAFETY: the matching registration created this typed Box.
            unsafe { drop(Box::from_raw(address as *mut T)) };
        }
        let owned = Box::new(value);
        let Some(token) = self.next.checked_add(1) else {
            crate::panic_context::fatal_invariant("legacy future handle space exhausted");
        };
        self.next = token;
        self.live.insert(
            token,
            Entry {
                address: Box::into_raw(owned) as usize,
                ty: TypeId::of::<T>(),
                release: release::<T>,
            },
        );
        self.record(1, 0, 0);
        std::ptr::without_provenance_mut(token)
    }

    fn checked_address<T: 'static>(&mut self, raw: *mut c_void, release: bool) -> *mut T {
        if raw.is_null() {
            crate::panic_context::fatal_invariant("null legacy future handle");
        }
        let token = raw.addr();
        let Some(entry) = self.live.get(&token) else {
            crate::panic_context::fatal_invariant("invalid or released legacy future handle");
        };
        if entry.ty != TypeId::of::<T>() {
            crate::panic_context::fatal_invariant("legacy future handle type mismatch");
        }
        let address = entry.address as *mut T;
        if release {
            // Transfer ownership; the matching ABI release consumes the Box.
            let mut transferred = self.live.remove(&token).unwrap();
            transferred.address = 0;
        }
        self.record(0, 1, usize::from(release));
        address
    }
}

fn register<T: 'static>(value: T) -> *mut c_void {
    handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .register(value)
}

fn checked_address<T: 'static>(raw: *mut c_void, release: bool) -> *mut T {
    handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .checked_address(raw, release)
}

fn into_raw<T: 'static>(future: RuntimeFuture<T>) -> *mut c_void {
    register(future)
}

/// # Safety
/// On a successful lookup the caller must own the future exclusively, with
/// all borrows ended. Removing the entry consumes that ownership exactly once.
unsafe fn release_raw<T: 'static>(raw: *mut c_void) {
    let address = checked_address::<T>(raw, true);
    // Drop outside the registry lock: destructors may create/release futures.
    // SAFETY: The caller ended all borrows. Removal transfers the original typed Box
    // to this sole owner and prevents a second successful removal.
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
    /// For a matching live token in the current runtime the caller must own
    /// the allocation exclusively, with all borrows ended. Adoption consumes
    /// that token. Invalid tokens terminate at lookup before ownership moves.
    pub unsafe fn from_abi(raw: *mut c_void) -> Self {
        let address = checked_address::<T>(raw, true);
        // SAFETY: Removal transferred the original typed Box to its sole owner after all
        // borrows ended. The registry can no longer reclaim this allocation.
        Self::new(unsafe { *Box::from_raw(address) })
    }
}

macro_rules! release_abi {
    ($name:ident, $ty:ty) => {
        /// Release an owned legacy future and its native storage.
        ///
        /// # Safety
        /// For a matching live token in the current runtime, the caller must
        /// transfer sole ownership after all borrows have ended and exclude
        /// further access. This consumes the future exactly once. Invalid
        /// tokens terminate at lookup. Pointer payloads remain borrowed.
        #[unsafe(no_mangle)]
        #[willow_runtime_macros::ffi_boundary]
        pub unsafe extern "C" fn $name(raw: *mut c_void) {
            // SAFETY: The ABI caller transfers sole ownership with all borrows ended.
            // The typed lookup removes the entry before reclaiming the Box.
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
/// If lookup succeeds, the future must remain live and shared-only for the
/// lifetime of the returned reference.
unsafe fn future_from_raw<'a, T: Clone + 'static>(raw: *mut c_void) -> &'a RuntimeFuture<T> {
    // SAFETY: The registry returns the original typed Box address; the caller keeps it live during
    // the shared borrow.
    unsafe { &*checked_address::<RuntimeFuture<T>>(raw, false) }
}

/// # Safety
///
/// If lookup succeeds, the caller must keep the future live and exclusively
/// accessible for the lifetime of the returned mutable reference.
unsafe fn future_from_raw_mut<'a, T: Clone + 'static>(
    raw: *mut c_void,
) -> &'a mut RuntimeFuture<T> {
    // SAFETY: The registry recovers the typed Box address. The caller excludes release and
    // competing borrows during mutation.
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
                // SAFETY: the sleep touches no runtime state, and neither this
                // frame nor its callers keep a managed pointer across the wait.
                unsafe { crate::gc::run_blocking(|| std::thread::sleep(remaining)) };
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
/// On successful lookup the future must stay live with only shared access
/// for the entire returned borrow, including a blocking await.
unsafe fn void_future_from_raw<'a>(raw: *mut c_void) -> &'a WillowFutureVoid {
    // SAFETY: The registry returns the original typed Box address; the caller keeps it live during
    // the shared borrow.
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
    // SAFETY: Lookup checks the live type. Adoption and release require exclusive
    // ownership and cannot overlap this shared borrow.
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
    // SAFETY: Lookup checks the live type. Adoption and release require exclusive
    // ownership and cannot overlap this shared borrow.
    unsafe { void_future_from_raw(raw) }.block_until_ready();
    0
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_is_ready_i64(raw: *mut c_void) -> u8 {
    // SAFETY: Lookup checks the type and liveness. Completion, adoption and release
    // require exclusive access, so none may overlap this shared poll.
    match unsafe { future_from_raw::<i64>(raw) }.poll() {
        Poll::Ready(_) => 1,
        Poll::Pending => 0,
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_await_i64(raw: *mut c_void) -> i64 {
    // SAFETY: Lookup checks the type and liveness. Completion, adoption and release
    // require exclusive access, so none may overlap this shared poll.
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
    // SAFETY: Lookup checks the live type. Adoption and release require exclusive
    // ownership and cannot overlap this shared borrow.
    match unsafe { future_from_raw::<u8>(raw) }.poll() {
        Poll::Ready(_) => 1,
        Poll::Pending => 0,
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_await_bool(raw: *mut c_void) -> u8 {
    // SAFETY: Lookup checks the live type. Adoption and release require exclusive
    // ownership and cannot overlap this shared borrow.
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
    // SAFETY: Lookup checks the live type. Adoption and release require exclusive
    // ownership and cannot overlap this shared borrow.
    match unsafe { future_from_raw::<f64>(raw) }.poll() {
        Poll::Ready(_) => 1,
        Poll::Pending => 0,
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_await_f64(raw: *mut c_void) -> f64 {
    // SAFETY: Lookup checks the live type. Adoption and release require exclusive
    // ownership and cannot overlap this shared borrow.
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
    // SAFETY: Lookup checks the live type. Adoption and release require exclusive
    // ownership and cannot overlap this shared borrow.
    match unsafe { future_from_raw::<*mut c_void>(raw) }.poll() {
        Poll::Ready(_) => 1,
        Poll::Pending => 0,
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_future_await_ptr(raw: *mut c_void) -> *mut c_void {
    // SAFETY: Lookup checks the live type. Adoption and release require exclusive
    // ownership and cannot overlap this shared borrow.
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

/// Store the result of a legacy i64 future and return 1. The registry rejects
/// null, stale and wrong-type tokens before accessing the future.
///
/// # Safety
///
/// If `raw` identifies a live i64 future in the current runtime, the caller
/// must have exclusive access to that future from entry until return. All
/// earlier borrows must have ended, and no other caller may poll, await,
/// complete, adopt or release it through any copy of the token during the call.
///
/// ```compile_fail,E0133
/// use willow_runtime::future::{willow_future_complete_i64, willow_future_pending_i64};
/// let token = willow_future_pending_i64();
/// willow_future_complete_i64(token, 42);
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_future_complete_i64(raw: *mut c_void, value: i64) -> u8 {
    // SAFETY: The completion ABI requires exclusive access to the live i64 future.
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
        // SAFETY: Each constructor transferred one fresh token to this test. No
        // future was borrowed or shared. Each matching adoption consumes it once.
        let (void_owner, i64_owner, bool_owner, f64_owner, ptr_owner) = unsafe {
            (
                LegacyFutureOwner::<WillowFutureVoid>::from_abi(void),
                LegacyFutureOwner::<RuntimeFuture<i64>>::from_abi(i64),
                LegacyFutureOwner::<RuntimeFuture<u8>>::from_abi(boolean),
                LegacyFutureOwner::<RuntimeFuture<f64>>::from_abi(f64),
                LegacyFutureOwner::<RuntimeFuture<*mut c_void>>::from_abi(ptr),
            )
        };
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
                    // SAFETY: This closure alone owns the fresh token; no borrow preceded adoption.
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
        // SAFETY: Direct registration gave this test the only handle and it has never been
        // borrowed.
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
                // SAFETY: This guard alone owns the token. Its synchronous awaits have
                // ended; the observer only checks registry membership by numeric key.
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
        let _guard = crate::gc::isolated_runtime();
        reset_global_scheduler_for_test();
        DROPS.store(0, Ordering::SeqCst);
        for (index, callback) in [finish as crate::task::RuntimePollFn, cancel]
            .into_iter()
            .enumerate()
        {
            // SAFETY: The callback accepts a null frame and uses no frame storage.
            let task = unsafe { willow_sched_spawn(callback, std::ptr::null_mut()) };
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
    fn private_legacy_handle_tables_release_only_their_owned_allocations() {
        struct Owned(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Owned {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let a = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let b = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let token = first
            .future_handles
            .lock()
            .unwrap()
            .register(Owned(a.clone()));
        let peer = second
            .future_handles
            .lock()
            .unwrap()
            .register(Owned(b.clone()));
        assert_eq!(token, peer, "tokens are scoped by the selected table");
        let address = first
            .future_handles
            .lock()
            .unwrap()
            .checked_address::<Owned>(token, true);
        assert_eq!(a.load(std::sync::atomic::Ordering::SeqCst), 0);
        // Explicit release transfers ownership exactly once.
        // SAFETY: Removal transferred the original typed Box to its sole owner after all
        // borrows ended. The registry can no longer reclaim this allocation.
        unsafe { drop(Box::from_raw(address)) };
        drop(first);
        assert_eq!(a.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(b.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(second.future_handles.lock().unwrap().live.len(), 1);
        drop(second);
        assert_eq!(b.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(std::sync::Arc::strong_count(&a), 1);
        assert_eq!(std::sync::Arc::strong_count(&b), 1);
    }

    #[test]
    fn legacy_registry_fragmentation_and_generation_counts() {
        for count in [1_usize, 16, 256, 1024] {
            let before = handles().lock().unwrap().counts();
            let tokens: Vec<_> = (0..count)
                .map(|i| willow_future_ready_i64(i as i64))
                .collect();
            for (i, &token) in tokens.iter().enumerate().step_by(2) {
                assert_eq!(willow_future_await_i64(token), i as i64);
                // SAFETY: This stride consumes each even token once, after its await returned.
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
                // SAFETY: The odd originals and fresh replacements are disjoint, owned solely here.
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
            let after = registry.counts();
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
                // SAFETY: The registry checks the opaque token and type before any dereference,
                // including on this deliberately invalid release or adoption.
                "wrong_adopt" => unsafe {
                    LegacyFutureOwner::<WillowFutureVoid>::from_abi(raw);
                },
                // SAFETY: The registry checks the opaque token and type before any dereference,
                // including on this deliberately invalid release or adoption.
                "null_adopt" => unsafe {
                    LegacyFutureOwner::<RuntimeFuture<i64>>::from_abi(std::ptr::null_mut());
                },
                "wrong_poll" => {
                    willow_future_is_ready_bool(raw);
                }
                "wrong_await" => {
                    willow_future_await_f64(raw);
                }
                // SAFETY: The registry checks the opaque token and type before any dereference,
                // including on this deliberately invalid release or adoption.
                "wrong_release" => unsafe { willow_future_release_ptr(raw) },
                // SAFETY: The fresh bool token fails the type check before any borrow.
                "wrong_complete" => unsafe {
                    willow_future_complete_i64(willow_future_ready_bool(1), 5);
                },
                "forged" => {
                    willow_future_await_void(std::ptr::without_provenance_mut(usize::MAX));
                }
                "double_void" => {
                    let raw = willow_future_ready_void();
                    // SAFETY: The test exclusively owns this fresh token. Removal ends ownership;
                    // any duplicate release is rejected before it can touch the allocation.
                    unsafe {
                        willow_future_release_void(raw);
                        willow_future_release_void(raw);
                    }
                }
                "double_bool" => {
                    let raw = willow_future_ready_bool(1);
                    // SAFETY: The test exclusively owns this fresh token. Removal ends ownership;
                    // any duplicate release is rejected before it can touch the allocation.
                    unsafe {
                        willow_future_release_bool(raw);
                        willow_future_release_bool(raw);
                    }
                }
                "double_f64" => {
                    let raw = willow_future_ready_f64(1.0);
                    // SAFETY: The test exclusively owns this fresh token. Removal ends ownership;
                    // any duplicate release is rejected before it can touch the allocation.
                    unsafe {
                        willow_future_release_f64(raw);
                        willow_future_release_f64(raw);
                    }
                }
                "double_ptr" => {
                    let raw = willow_future_ready_ptr(std::ptr::null_mut());
                    // SAFETY: The test exclusively owns this fresh token. Removal ends ownership;
                    // any duplicate release is rejected before it can touch the allocation.
                    unsafe {
                        willow_future_release_ptr(raw);
                        willow_future_release_ptr(raw);
                    }
                }
                _ => {
                    // SAFETY: The test exclusively owns this fresh token. Removal ends ownership;
                    // any duplicate release is rejected before it can touch the allocation.
                    unsafe {
                        willow_future_release_i64(raw);
                    }
                    // Allocating another future cannot resurrect the old token.
                    let other = willow_future_ready_i64(9);
                    assert_ne!(raw, other);
                    match case.as_str() {
                        // SAFETY: The registry checks the opaque token and type before any
                        // dereference, including on this deliberately invalid release or adoption.
                        "double_i64" => unsafe { willow_future_release_i64(raw) },
                        // SAFETY: The registry checks the opaque token and type before any
                        // dereference, including on this deliberately invalid release or adoption.
                        "stale_adopt" => unsafe {
                            LegacyFutureOwner::<RuntimeFuture<i64>>::from_abi(raw);
                        },
                        "stale_poll" => {
                            willow_future_is_ready_i64(raw);
                        }
                        "stale_await" => {
                            willow_future_await_i64(raw);
                        }
                        // SAFETY: Release removed this token. Lookup rejects it before any borrow.
                        "stale_complete" => unsafe {
                            willow_future_complete_i64(raw, 10);
                        },
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
                    // SAFETY: Draining the vector consumes each unique, never-borrowed token once.
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
        // SAFETY: The constructor gave this test sole ownership; all synchronous borrows have
        // ended.
        unsafe { willow_future_release_ptr(raw) };
        assert_eq!(*payload, 73);
        *payload = 91;
        assert_eq!(*payload, 91);
    }

    #[test]
    fn release_sleep_does_not_await_its_deadline() {
        let raw = void_future_into_raw(WillowFutureVoid::sleep_after_millis(3_600_000));
        assert_eq!(willow_future_is_ready_void(raw), 0);
        // SAFETY: The constructor gave this test sole ownership; all synchronous borrows have
        // ended.
        unsafe { willow_future_release_void(raw) };
    }

    #[test]
    fn future_unit_01_ready_i64_abi_awaits_value() {
        let raw = willow_future_ready_i64(42);
        assert_eq!(willow_future_await_i64(raw), 42);
        // SAFETY: The constructor gave this test sole ownership; all synchronous borrows have
        // ended.
        unsafe { willow_future_release_i64(raw) };
    }

    #[test]
    fn future_unit_02_ready_bool_abi_canonicalizes_true() {
        let raw = willow_future_ready_bool(7);
        assert_eq!(willow_future_await_bool(raw), 1);
        // SAFETY: The constructor gave this test sole ownership; all synchronous borrows have
        // ended.
        unsafe { willow_future_release_bool(raw) };
    }

    #[test]
    fn future_unit_03_ready_bool_abi_preserves_false() {
        let raw = willow_future_ready_bool(0);
        assert_eq!(willow_future_await_bool(raw), 0);
        // SAFETY: The constructor gave this test sole ownership; all synchronous borrows have
        // ended.
        unsafe { willow_future_release_bool(raw) };
    }

    #[test]
    fn future_unit_04_ready_f64_abi_awaits_value() {
        let raw = willow_future_ready_f64(3.5);
        assert_eq!(willow_future_await_f64(raw), 3.5);
        // SAFETY: The constructor gave this test sole ownership; all synchronous borrows have
        // ended.
        unsafe { willow_future_release_f64(raw) };
    }

    #[test]
    fn future_unit_05_ready_ptr_abi_awaits_value() {
        let mut value = 10_i64;
        let ptr = (&mut value as *mut i64).cast::<c_void>();
        let raw = willow_future_ready_ptr(ptr);
        assert_eq!(willow_future_await_ptr(raw), ptr);
        // SAFETY: The constructor gave this test sole ownership; all synchronous borrows have
        // ended.
        unsafe { willow_future_release_ptr(raw) };
    }

    #[test]
    fn future_unit_06_ready_void_abi_awaits_unit() {
        let raw = willow_future_ready_void();
        assert_eq!(willow_future_await_void(raw), 0);
        // SAFETY: The constructor gave this test sole ownership; all synchronous borrows have
        // ended.
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
                // SAFETY: The registry checks the opaque token and type before any dereference,
                // including on this deliberately invalid release or adoption.
                "release_void" => unsafe { willow_future_release_void(raw) },
                // SAFETY: The registry checks the opaque token and type before any dereference,
                // including on this deliberately invalid release or adoption.
                "release_i64" => unsafe { willow_future_release_i64(raw) },
                // SAFETY: The registry checks the opaque token and type before any dereference,
                // including on this deliberately invalid release or adoption.
                "release_bool" => unsafe { willow_future_release_bool(raw) },
                // SAFETY: The registry checks the opaque token and type before any dereference,
                // including on this deliberately invalid release or adoption.
                "release_f64" => unsafe { willow_future_release_f64(raw) },
                // SAFETY: The registry checks the opaque token and type before any dereference,
                // including on this deliberately invalid release or adoption.
                "release_ptr" => unsafe { willow_future_release_ptr(raw) },
                // SAFETY: The null token is rejected before any borrow.
                "complete_i64" => unsafe {
                    willow_future_complete_i64(raw, 42);
                },
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
                // SAFETY: The constructor gave this test sole ownership; all synchronous borrows
                // have ended.
                unsafe {
                    $release(raw);
                }
                let raw = $ready($value);
                assert_eq!($poll(raw), 1);
                assert_eq!($await(raw), $value);
                assert_eq!($await(raw), $value);
                // SAFETY: The constructor gave this test sole ownership; all synchronous borrows
                // have ended.
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
        // SAFETY: This test alone owns the fresh token and polls it only after completion.
        assert_eq!(unsafe { willow_future_complete_i64(raw, 73) }, 1);
        assert_eq!(willow_future_is_ready_i64(raw), 1);
        assert_eq!(willow_future_await_i64(raw), 73);
        // SAFETY: The constructor gave this test sole ownership; all synchronous borrows have
        // ended.
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
            // SAFETY: The constructor gave this test sole ownership; all synchronous borrows have
            // ended.
            unsafe {
                willow_future_release_void(raw);
            }
        }
    }

    #[test]
    fn future_unit_11_pending_i64_await_returns_zero_for_mvp() {
        let raw = into_raw(RuntimeFuture::<i64>::pending());
        assert_eq!(willow_future_await_i64(raw), 0);
        // SAFETY: The constructor gave this test sole ownership; all synchronous borrows have
        // ended.
        unsafe { willow_future_release_i64(raw) };
    }

    #[test]
    fn future_unit_12_cancelled_i64_await_returns_zero_for_mvp() {
        let mut future = RuntimeFuture::<i64>::pending();
        future.cancel();
        let raw = into_raw(future);
        assert_eq!(willow_future_await_i64(raw), 0);
        // SAFETY: The constructor gave this test sole ownership; all synchronous borrows have
        // ended.
        unsafe { willow_future_release_i64(raw) };
    }
}
