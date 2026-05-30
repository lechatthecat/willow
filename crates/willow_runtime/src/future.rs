use std::ffi::c_void;
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

fn into_raw<T>(future: RuntimeFuture<T>) -> *mut c_void {
    Box::into_raw(Box::new(future)) as *mut c_void
}

fn future_from_raw<T: Clone>(raw: *mut c_void) -> Option<&'static RuntimeFuture<T>> {
    if raw.is_null() {
        None
    } else {
        Some(unsafe { &*(raw as *mut RuntimeFuture<T>) })
    }
}

fn future_from_raw_mut<T: Clone>(raw: *mut c_void) -> Option<&'static mut RuntimeFuture<T>> {
    if raw.is_null() {
        None
    } else {
        Some(unsafe { &mut *(raw as *mut RuntimeFuture<T>) })
    }
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

    pub fn block_until_ready(&self) {
        if let WillowFutureVoid::Sleep { deadline } = self {
            let remaining = deadline.checked_duration_since(Instant::now());
            if let Some(remaining) = remaining {
                std::thread::sleep(remaining);
            }
        }
    }
}

fn void_future_into_raw(future: WillowFutureVoid) -> *mut c_void {
    Box::into_raw(Box::new(future)) as *mut c_void
}

pub fn void_future_into_raw_pub(future: WillowFutureVoid) -> *mut c_void {
    void_future_into_raw(future)
}

fn void_future_from_raw(raw: *mut c_void) -> Option<&'static WillowFutureVoid> {
    if raw.is_null() {
        None
    } else {
        Some(unsafe { &*(raw as *mut WillowFutureVoid) })
    }
}

// ---------------------------------------------------------------------------
// Void future ABI (uses WillowFutureVoid).
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_ready_void() -> *mut c_void {
    void_future_into_raw(WillowFutureVoid::ready())
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_pending_void() -> *mut c_void {
    void_future_into_raw(WillowFutureVoid::pending())
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_is_ready_void(raw: *mut c_void) -> u8 {
    void_future_from_raw(raw).map_or(1, |f| if f.is_ready() { 1 } else { 0 })
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_ready_i64(value: i64) -> *mut c_void {
    into_raw(ready_future(value))
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_ready_bool(value: u8) -> *mut c_void {
    into_raw(ready_future(if value == 0 { 0_u8 } else { 1_u8 }))
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_ready_f64(value: f64) -> *mut c_void {
    into_raw(ready_future(value))
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_ready_ptr(value: *mut c_void) -> *mut c_void {
    into_raw(ready_future(value))
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_await_void(raw: *mut c_void) -> u8 {
    if let Some(future) = void_future_from_raw(raw) {
        future.block_until_ready();
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_is_ready_i64(raw: *mut c_void) -> u8 {
    match future_from_raw::<i64>(raw).map(|f| f.poll()) {
        Some(Poll::Ready(_)) | None => 1,
        Some(Poll::Pending) => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_await_i64(raw: *mut c_void) -> i64 {
    match future_from_raw::<i64>(raw).map(RuntimeFuture::poll) {
        Some(Poll::Ready(value)) => value,
        Some(Poll::Pending) | None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_pending_i64() -> *mut c_void {
    into_raw(RuntimeFuture::<i64>::pending())
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_is_ready_bool(raw: *mut c_void) -> u8 {
    match future_from_raw::<u8>(raw).map(|f| f.poll()) {
        Some(Poll::Ready(_)) | None => 1,
        Some(Poll::Pending) => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_await_bool(raw: *mut c_void) -> u8 {
    match future_from_raw::<u8>(raw).map(RuntimeFuture::poll) {
        Some(Poll::Ready(value)) => {
            if value == 0 {
                0
            } else {
                1
            }
        }
        Some(Poll::Pending) | None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_pending_bool() -> *mut c_void {
    into_raw(RuntimeFuture::<u8>::pending())
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_is_ready_f64(raw: *mut c_void) -> u8 {
    match future_from_raw::<f64>(raw).map(|f| f.poll()) {
        Some(Poll::Ready(_)) | None => 1,
        Some(Poll::Pending) => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_await_f64(raw: *mut c_void) -> f64 {
    match future_from_raw::<f64>(raw).map(RuntimeFuture::poll) {
        Some(Poll::Ready(value)) => value,
        Some(Poll::Pending) | None => 0.0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_pending_f64() -> *mut c_void {
    into_raw(RuntimeFuture::<f64>::pending())
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_is_ready_ptr(raw: *mut c_void) -> u8 {
    match future_from_raw::<*mut c_void>(raw).map(|f| f.poll()) {
        Some(Poll::Ready(_)) | None => 1,
        Some(Poll::Pending) => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_await_ptr(raw: *mut c_void) -> *mut c_void {
    match future_from_raw::<*mut c_void>(raw).map(RuntimeFuture::poll) {
        Some(Poll::Ready(value)) => value,
        Some(Poll::Pending) | None => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_future_pending_ptr() -> *mut c_void {
    into_raw(RuntimeFuture::<*mut c_void>::pending())
}

/// Complete a pending i64 future with a value (called by executor when async fn finishes).
/// Returns 1 on success, 0 if future was null.
#[unsafe(no_mangle)]
pub extern "C" fn willow_future_complete_i64(raw: *mut c_void, value: i64) -> u8 {
    match future_from_raw_mut::<i64>(raw) {
        Some(future) => {
            future.complete(value);
            1
        }
        None => 0,
    }
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
    fn future_unit_01_ready_i64_abi_awaits_value() {
        let raw = willow_future_ready_i64(42);
        assert_eq!(willow_future_await_i64(raw), 42);
    }

    #[test]
    fn future_unit_02_ready_bool_abi_canonicalizes_true() {
        let raw = willow_future_ready_bool(7);
        assert_eq!(willow_future_await_bool(raw), 1);
    }

    #[test]
    fn future_unit_03_ready_bool_abi_preserves_false() {
        let raw = willow_future_ready_bool(0);
        assert_eq!(willow_future_await_bool(raw), 0);
    }

    #[test]
    fn future_unit_04_ready_f64_abi_awaits_value() {
        let raw = willow_future_ready_f64(3.5);
        assert_eq!(willow_future_await_f64(raw), 3.5);
    }

    #[test]
    fn future_unit_05_ready_ptr_abi_awaits_value() {
        let mut value = 10_i64;
        let ptr = (&mut value as *mut i64).cast::<c_void>();
        let raw = willow_future_ready_ptr(ptr);
        assert_eq!(willow_future_await_ptr(raw), ptr);
    }

    #[test]
    fn future_unit_06_ready_void_abi_awaits_unit() {
        let raw = willow_future_ready_void();
        assert_eq!(willow_future_await_void(raw), 0);
    }

    #[test]
    fn future_unit_07_null_i64_await_returns_zero() {
        assert_eq!(willow_future_await_i64(std::ptr::null_mut()), 0);
    }

    #[test]
    fn future_unit_08_null_bool_await_returns_false() {
        assert_eq!(willow_future_await_bool(std::ptr::null_mut()), 0);
    }

    #[test]
    fn future_unit_09_null_f64_await_returns_zero() {
        assert_eq!(willow_future_await_f64(std::ptr::null_mut()), 0.0);
    }

    #[test]
    fn future_unit_10_null_ptr_await_returns_null() {
        assert!(willow_future_await_ptr(std::ptr::null_mut()).is_null());
    }

    #[test]
    fn future_unit_11_pending_i64_await_returns_zero_for_mvp() {
        let raw = into_raw(RuntimeFuture::<i64>::pending());
        assert_eq!(willow_future_await_i64(raw), 0);
    }

    #[test]
    fn future_unit_12_cancelled_i64_await_returns_zero_for_mvp() {
        let mut future = RuntimeFuture::<i64>::pending();
        future.cancel();
        let raw = into_raw(future);
        assert_eq!(willow_future_await_i64(raw), 0);
    }
}
