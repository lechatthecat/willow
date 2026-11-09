// Included once into the generated bridge. No Willow pointer enters a Future.
type Completion = Box<dyn FnOnce(&mut BridgeFrame) -> BridgeValue + Send>;
type BridgeFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Completion> + Send>>;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// An owned lease: close invalidates the public handle but cannot destroy an
/// object still used by a Future. Adapters may borrow through Deref while polling.
pub struct AsyncHandle<T> {
    lease: handles::Lease,
    marker: std::marker::PhantomData<T>,
}
impl<T: std::any::Any + Send + Sync> std::ops::Deref for AsyncHandle<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.lease
            .downcast_ref()
            .expect("opaque adapter type invariant")
    }
}
impl<T: std::any::Any + Send + Sync> AsyncHandle<T> {
    fn new(id: u64, name: std::any::TypeId) -> Self {
        let lease =
            handles::get(id, name).unwrap_or_else(|error| std::panic::panic_any((error, id)));
        assert!(
            lease.downcast_ref::<T>().is_some(),
            "opaque adapter type invariant"
        );
        Self {
            lease,
            marker: std::marker::PhantomData,
        }
    }
}

struct FutureState {
    future: Option<BridgeFuture>,
    result: Option<Result<Completion, ()>>,
}
struct FutureJob {
    state: Mutex<FutureState>,
    queued: AtomicBool,
    cancelled: AtomicBool,
    cancel_done: AtomicBool,
    done: AtomicBool,
    task: AtomicU64,
}
unsafe extern "C" {
    fn willow_sched_wake(task: u64);
    fn willow_rust_bridge_async_spawn(
        context: *mut u8,
        start: unsafe extern "C" fn(*mut u8, u64),
        finish: unsafe extern "C" fn(*mut u8, *mut BridgeValue) -> i32,
        cancel: unsafe extern "C" fn(*mut u8) -> i32,
        reference: u32,
    ) -> *mut u8;
}
fn future_queue() -> &'static std::sync::mpsc::Sender<Arc<FutureJob>> {
    static QUEUE: OnceLock<std::sync::mpsc::Sender<Arc<FutureJob>>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (send, receive) = std::sync::mpsc::channel::<Arc<FutureJob>>();
        std::thread::Builder::new()
            .name("willow-rust-futures".into())
            .spawn(move || {
                while let Ok(job) = receive.recv() {
                    job.run();
                }
            })
            .expect("Rust bridge executor thread");
        send
    })
}
// A malicious panic payload destructor must not unwind across our boundary.
fn contained<T>(f: impl FnOnce() -> T) -> Result<T, ()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|payload| {
        std::mem::forget(payload);
    })
}
impl FutureJob {
    fn enqueue(self: &Arc<Self>) {
        if !self.queued.swap(true, Ordering::AcqRel) {
            future_queue()
                .send(self.clone())
                .expect("Rust bridge executor stopped");
        }
    }
    fn run(self: Arc<Self>) {
        self.queued.store(false, Ordering::Release);
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if self.cancelled.load(Ordering::Acquire) {
            if self.cancel_done.load(Ordering::Acquire) {
                return;
            }
            let future = state.future.take();
            let result = state.result.take();
            let failed = matches!(result, Some(Err(())));
            self.done.store(true, Ordering::Release);
            drop(state);
            let future_drop = contained(|| drop(future));
            let result_drop = contained(|| drop(result));
            if failed || future_drop.is_err() || result_drop.is_err() {
                unsafe {
                    willow_rust_bridge_panic();
                }
            }
            self.cancel_done.store(true, Ordering::Release);
            unsafe {
                willow_sched_wake(self.task.load(Ordering::Acquire));
            }
            return;
        }
        let Some(future) = state.future.as_mut() else {
            return;
        };
        let waker = std::task::Waker::from(self.clone());
        let polled = contained(|| {
            future
                .as_mut()
                .poll(&mut std::task::Context::from_waker(&waker))
        });
        let outcome = match polled {
            Ok(std::task::Poll::Pending) => return,
            Ok(std::task::Poll::Ready(value)) => Ok(value),
            Err(()) => Err(()),
        };
        let future = state.future.take();
        // Destroy user state here, never on a scheduler worker.
        let dropped = contained(|| drop(future));
        state.result = Some(if dropped.is_ok() {
            outcome
        } else {
            let _ = contained(|| drop(outcome));
            Err(())
        });
        self.done.store(true, Ordering::Release);
        drop(state);
        // One ID lookup, no scan of live futures or waiters.
        unsafe {
            willow_sched_wake(self.task.load(Ordering::Acquire));
        }
    }
}
impl std::task::Wake for FutureJob {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        if !self.done.load(Ordering::Acquire) {
            self.enqueue();
        }
    }
}
unsafe extern "C" fn start_future(context: *mut u8, id: u64) {
    let job = unsafe { &*context.cast::<Arc<FutureJob>>() };
    job.task.store(id, Ordering::Release);
    if contained(|| job.enqueue()).is_err() {
        unsafe {
            willow_rust_bridge_panic();
        }
    }
}
unsafe extern "C" fn cancel_future(context: *mut u8) -> i32 {
    let job = unsafe { &*context.cast::<Arc<FutureJob>>() };
    if !job.cancelled.swap(true, Ordering::AcqRel) && contained(|| job.enqueue()).is_err() {
        unsafe {
            willow_rust_bridge_panic();
        }
    }
    if !job.cancel_done.load(Ordering::Acquire) {
        return 0;
    }
    unsafe {
        drop(Box::from_raw(context.cast::<Arc<FutureJob>>()));
    }
    1
}
unsafe extern "C" fn finish_future(context: *mut u8, output: *mut BridgeValue) -> i32 {
    let job = unsafe { &*context.cast::<Arc<FutureJob>>() };
    if !job.done.load(Ordering::Acquire) {
        return 0;
    }
    let result = {
        let Ok(mut state) = job.state.try_lock() else {
            // A redundant queued wake can briefly own the completed state.
            // Preserve a retry rather than consuming the sole completion wake.
            unsafe {
                willow_sched_wake(job.task.load(Ordering::Acquire));
            }
            return 0;
        };
        state.result.take()
    };
    let Some(result) = result else {
        return 0;
    };
    unsafe {
        drop(Box::from_raw(context.cast::<Arc<FutureJob>>()));
    }
    match result {
        Ok(encode) => match contained(|| {
            let mut frame = BridgeFrame::new();
            unsafe {
                *output = encode(&mut frame);
            }
        }) {
            Ok(()) => 1,
            Err(()) => 2,
        },
        Err(()) => 2,
    }
}
fn spawn_future(future: BridgeFuture, reference: bool) -> BridgeValue {
    let job = Arc::new(FutureJob {
        state: Mutex::new(FutureState {
            future: Some(future),
            result: None,
        }),
        queued: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        cancel_done: AtomicBool::new(false),
        done: AtomicBool::new(false),
        task: AtomicU64::new(0),
    });
    let context = Box::into_raw(Box::new(job)).cast::<u8>();
    BridgeValue {
        low: unsafe {
            willow_rust_bridge_async_spawn(
                context,
                start_future,
                finish_future,
                cancel_future,
                u32::from(reference),
            )
        } as u64,
        high: 0,
    }
}
