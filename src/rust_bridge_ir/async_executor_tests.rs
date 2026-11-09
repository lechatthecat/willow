//! Exercise the exact generated executor with a native-value-only frame stub.
use willow_abi::ffi::WillowBridgeValue as BridgeValue;
struct BridgeFrame;
impl BridgeFrame {
    fn new() -> Self {
        Self
    }
}
#[path = "../../crates/willow_runtime/src/rust_bridge_abi/handles.rs"]
mod handles;
include!("../../crates/willow_runtime/src/rust_bridge_abi/futures.rs");
unsafe fn willow_rust_bridge_panic() -> ! {
    panic!("RustPanic");
}

mod runtime_stubs {
    use super::*;
    pub static WAKES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    #[unsafe(no_mangle)]
    extern "C" fn willow_sched_wake(_: u64) {
        WAKES.fetch_add(1, Ordering::SeqCst);
    }
    #[unsafe(no_mangle)]
    extern "C" fn willow_rust_bridge_async_spawn(
        _: *mut u8,
        _: unsafe extern "C" fn(*mut u8, u64),
        _: unsafe extern "C" fn(*mut u8, *mut BridgeValue) -> i32,
        _: unsafe extern "C" fn(*mut u8) -> i32,
        _: u32,
    ) -> *mut u8 {
        panic!("test uses executor directly")
    }
}
fn job(future: impl std::future::Future<Output = Completion> + Send + 'static) -> Arc<FutureJob> {
    Arc::new(FutureJob {
        state: Mutex::new(FutureState {
            future: Some(Box::pin(future)),
            result: None,
        }),
        queued: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        cancel_done: AtomicBool::new(false),
        done: AtomicBool::new(false),
        task: AtomicU64::new(1),
    })
}
fn value(n: u64) -> Completion {
    Box::new(move |_| BridgeValue { low: n, high: 0 })
}
static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn async_executor_completion_counts_are_linear() {
    let _guard = SERIAL.lock().unwrap();
    for n in [1, 16, 256, 4096] {
        let jobs: Vec<_> = (0..n).map(|i| job(async move { value(i) })).collect();
        runtime_stubs::WAKES.store(0, Ordering::SeqCst);
        // Adverse completion order, independent of allocation order.
        for j in jobs.iter().rev() {
            j.clone().run();
        }
        assert_eq!(runtime_stubs::WAKES.load(Ordering::SeqCst), n as usize);
        for (i, j) in jobs.iter().enumerate() {
            assert!(j.done.load(Ordering::Acquire));
            let result = j.state.lock().unwrap().result.take().unwrap().unwrap();
            assert_eq!(result(&mut BridgeFrame).low, i as u64);
            // Duplicate completion processing cannot notify again.
            j.clone().run();
        }
        assert_eq!(runtime_stubs::WAKES.load(Ordering::SeqCst), n as usize);
        eprintln!("async futures={n} completions={n} notification_calls={n}");
    }
}

#[test]
fn async_executor_cancel_drops_pending_and_completed_values_once() {
    let _guard = SERIAL.lock().unwrap();
    struct DropCount(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    for ready in [false, true] {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = DropCount(drops.clone());
        let j = job(async move {
            if !ready {
                std::future::pending::<()>().await;
            }
            Box::new(move |_: &mut BridgeFrame| {
                drop(counted);
                BridgeValue::default()
            }) as Completion
        });
        j.clone().run();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        j.cancelled.store(true, Ordering::Release);
        j.clone().run();
        j.clone().run();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(j.state.lock().unwrap().future.is_none());
        assert!(j.state.lock().unwrap().result.is_none());
    }
}

#[test]
fn async_executor_contains_poll_drop_and_payload_panics() {
    let _guard = SERIAL.lock().unwrap();
    struct Bad;
    impl Drop for Bad {
        fn drop(&mut self) {
            panic!("drop panic");
        }
    }
    for drop_panic in [false, true] {
        let j = job(async move {
            if drop_panic {
                let _bad = Bad;
                std::future::pending::<()>().await;
            } else {
                std::panic::panic_any(Bad);
            }
            value(0)
        });
        j.clone().run();
        if drop_panic {
            j.cancelled.store(true, Ordering::Release);
            assert!(std::panic::catch_unwind(|| j.clone().run()).is_err());
        } else {
            assert!(matches!(j.state.lock().unwrap().result, Some(Err(()))));
        }
        assert!(j.done.load(Ordering::Acquire));
    }
}

#[test]
fn async_executor_pending_finish_never_waits_for_poll_lock() {
    let j = job(std::future::pending());
    let context = Box::into_raw(Box::new(j.clone())).cast::<u8>();
    let lock = j.state.lock().unwrap();
    let mut output = BridgeValue::default();
    assert_eq!(unsafe { finish_future(context, &mut output) }, 0);
    drop(lock);
    unsafe {
        drop(Box::from_raw(context.cast::<Arc<FutureJob>>()));
    }
}

#[test]
fn async_executor_cancel_cannot_hide_a_completed_poll_panic() {
    let _guard = SERIAL.lock().unwrap();
    let j = job(async {
        panic!("poll failed");
    });
    j.clone().run();
    assert!(matches!(j.state.lock().unwrap().result, Some(Err(()))));
    j.cancelled.store(true, Ordering::Release);
    assert!(std::panic::catch_unwind(|| j.clone().run()).is_err());
    assert!(!j.cancel_done.load(Ordering::Acquire));
}

#[test]
fn async_executor_self_wake_before_pending_is_not_lost() {
    let _guard = SERIAL.lock().unwrap();
    runtime_stubs::WAKES.store(0, Ordering::SeqCst);
    let mut first = true;
    let j = job(std::future::poll_fn(move |cx| {
        if first {
            first = false;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(value(42))
        }
    }));
    j.enqueue();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while runtime_stubs::WAKES.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline, "lost self wake");
        std::thread::yield_now();
    }
    assert!(j.done.load(Ordering::Acquire));
    let result = j.state.lock().unwrap().result.take().unwrap().unwrap();
    assert_eq!(result(&mut BridgeFrame).low, 42);
}
