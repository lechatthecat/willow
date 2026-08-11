//! Scheduler/runtime tracing, metrics, and profiling hooks (willow-2vq).
//!
//! Recording is allocation-free unless textual tracing is enabled. Hooks are
//! invoked only from call sites that have released scheduler, task-shard,
//! timer, netpoll, and channel locks. A hook must still be fast and must not
//! panic across the C ABI.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::Instant;

pub const RUNTIME_EVENT_VERSION: u32 = 1;
pub const RUNTIME_METRICS_VERSION: u32 = 1;

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeEventKind {
    TaskSpawn = 1,
    TaskWake = 2,
    TaskPoll = 3,
    TaskPark = 4,
    TaskYield = 5,
    TaskPreempt = 6,
    TaskComplete = 7,
    TaskCancelRequested = 8,
    TaskCancelled = 9,
    TaskPanicked = 10,
    TimerWake = 11,
    NetpollWait = 12,
    NetpollWake = 13,
    ChannelWait = 14,
    BlockingDetach = 15,
}

impl RuntimeEventKind {
    const fn name(self) -> &'static str {
        match self {
            Self::TaskSpawn => "task_spawn",
            Self::TaskWake => "task_wake",
            Self::TaskPoll => "task_poll",
            Self::TaskPark => "task_park",
            Self::TaskYield => "task_yield",
            Self::TaskPreempt => "task_preempt",
            Self::TaskComplete => "task_complete",
            Self::TaskCancelRequested => "task_cancel_requested",
            Self::TaskCancelled => "task_cancelled",
            Self::TaskPanicked => "task_panicked",
            Self::TimerWake => "timer_wake",
            Self::NetpollWait => "netpoll_wait",
            Self::NetpollWake => "netpoll_wake",
            Self::ChannelWait => "channel_wait",
            Self::BlockingDetach => "blocking_detach",
        }
    }

    const fn is_task_event(self) -> bool {
        matches!(
            self,
            Self::TaskSpawn
                | Self::TaskWake
                | Self::TaskPoll
                | Self::TaskPark
                | Self::TaskYield
                | Self::TaskPreempt
                | Self::TaskComplete
                | Self::TaskCancelRequested
                | Self::TaskCancelled
                | Self::TaskPanicked
                | Self::BlockingDetach
        )
    }
}

/// Stable event record passed to an optional native profiler callback.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WillowRuntimeEventV1 {
    pub version: u32,
    pub struct_size: u32,
    pub kind: u32,
    /// `u32::MAX` when the event does not belong to a scheduler worker.
    pub worker: u32,
    pub task_id: u64,
    /// Event-specific scalar: count, fd, interest bits, or zero.
    pub value: i64,
    pub timestamp_ns: u64,
}

/// Monotonic cumulative snapshot for diagnostics and benchmarks. Individual
/// counters are atomic; a concurrent snapshot may span adjacent events.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WillowRuntimeMetricsV1 {
    pub version: u32,
    pub struct_size: u32,
    pub task_spawns: u64,
    pub task_wakes: u64,
    pub task_polls: u64,
    pub task_parks: u64,
    pub task_yields: u64,
    pub task_preempts: u64,
    pub task_completions: u64,
    pub task_cancel_requests: u64,
    pub task_cancellations: u64,
    pub task_panics: u64,
    pub timer_wakes: u64,
    pub netpoll_waits: u64,
    pub netpoll_wakes: u64,
    pub channel_waits: u64,
    pub blocking_detaches: u64,
}

type RuntimeEventHookV1 = unsafe extern "C" fn(*const WillowRuntimeEventV1, *mut c_void);

struct HookRegistration {
    hook: RuntimeEventHookV1,
    context: usize,
    active_callbacks: AtomicUsize,
    wait_lock: Mutex<()>,
    quiescent: Condvar,
}

#[derive(Default)]
struct HookState {
    registration: Option<Arc<HookRegistration>>,
}

static START: LazyLock<Instant> = LazyLock::new(Instant::now);
static SCHED_TRACE: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_SCHED_TRACE").is_some());
static TASK_TRACE: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_TASK_TRACE").is_some());
static HOOK_ENABLED: AtomicBool = AtomicBool::new(false);
static HOOK_STATE: LazyLock<Mutex<HookState>> = LazyLock::new(|| Mutex::new(HookState::default()));

static TASK_SPAWNS: AtomicU64 = AtomicU64::new(0);
static TASK_WAKES: AtomicU64 = AtomicU64::new(0);
static TASK_POLLS: AtomicU64 = AtomicU64::new(0);
static TASK_PARKS: AtomicU64 = AtomicU64::new(0);
static TASK_YIELDS: AtomicU64 = AtomicU64::new(0);
static TASK_PREEMPTS: AtomicU64 = AtomicU64::new(0);
static TASK_COMPLETIONS: AtomicU64 = AtomicU64::new(0);
static TASK_CANCEL_REQUESTS: AtomicU64 = AtomicU64::new(0);
static TASK_CANCELLATIONS: AtomicU64 = AtomicU64::new(0);
static TASK_PANICS: AtomicU64 = AtomicU64::new(0);
static TIMER_WAKES: AtomicU64 = AtomicU64::new(0);
static NETPOLL_WAITS: AtomicU64 = AtomicU64::new(0);
static NETPOLL_WAKES: AtomicU64 = AtomicU64::new(0);
static CHANNEL_WAITS: AtomicU64 = AtomicU64::new(0);
static BLOCKING_DETACHES: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static BUILT_EVENTS: AtomicU64 = AtomicU64::new(0);

fn counter(kind: RuntimeEventKind) -> &'static AtomicU64 {
    match kind {
        RuntimeEventKind::TaskSpawn => &TASK_SPAWNS,
        RuntimeEventKind::TaskWake => &TASK_WAKES,
        RuntimeEventKind::TaskPoll => &TASK_POLLS,
        RuntimeEventKind::TaskPark => &TASK_PARKS,
        RuntimeEventKind::TaskYield => &TASK_YIELDS,
        RuntimeEventKind::TaskPreempt => &TASK_PREEMPTS,
        RuntimeEventKind::TaskComplete => &TASK_COMPLETIONS,
        RuntimeEventKind::TaskCancelRequested => &TASK_CANCEL_REQUESTS,
        RuntimeEventKind::TaskCancelled => &TASK_CANCELLATIONS,
        RuntimeEventKind::TaskPanicked => &TASK_PANICS,
        RuntimeEventKind::TimerWake => &TIMER_WAKES,
        RuntimeEventKind::NetpollWait => &NETPOLL_WAITS,
        RuntimeEventKind::NetpollWake => &NETPOLL_WAKES,
        RuntimeEventKind::ChannelWait => &CHANNEL_WAITS,
        RuntimeEventKind::BlockingDetach => &BLOCKING_DETACHES,
    }
}

pub(crate) fn record(kind: RuntimeEventKind, worker: Option<usize>, task_id: u64, value: i64) {
    let increment = if matches!(
        kind,
        RuntimeEventKind::TimerWake | RuntimeEventKind::NetpollWake
    ) {
        value.max(0) as u64
    } else {
        1
    };
    counter(kind).fetch_add(increment, Ordering::Relaxed);

    let sched_trace = *SCHED_TRACE;
    let task_trace = *TASK_TRACE && kind.is_task_event();
    let registration = if HOOK_ENABLED.load(Ordering::Acquire) {
        let registration = HOOK_STATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .registration
            .clone();
        if let Some(registration) = &registration {
            // Increment while the registration is still protected by
            // HOOK_STATE. A concurrent clear removes it under the same lock and
            // then waits for this lease to reach zero.
            registration.active_callbacks.fetch_add(1, Ordering::AcqRel);
        }
        registration
    } else {
        None
    };

    // Metrics are always updated, but the hot scheduler path should not read
    // the clock or build an event when no textual trace or native sink exists.
    if !sched_trace && !task_trace && registration.is_none() {
        return;
    }

    #[cfg(test)]
    BUILT_EVENTS.fetch_add(1, Ordering::Relaxed);

    let event = WillowRuntimeEventV1 {
        version: RUNTIME_EVENT_VERSION,
        struct_size: std::mem::size_of::<WillowRuntimeEventV1>() as u32,
        kind: kind as u32,
        worker: worker
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(u32::MAX),
        task_id,
        value,
        timestamp_ns: START.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
    };

    if sched_trace {
        eprintln!(
            "[sched] t_ns={} event={} worker={} task={} value={}",
            event.timestamp_ns,
            kind.name(),
            event.worker,
            event.task_id,
            event.value
        );
    }
    if task_trace {
        eprintln!(
            "[task] t_ns={} event={} worker={} task={} value={}",
            event.timestamp_ns,
            kind.name(),
            event.worker,
            event.task_id,
            event.value
        );
    }

    if let Some(registration) = registration {
        unsafe { (registration.hook)(&event, registration.context as *mut c_void) };
        if registration.active_callbacks.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _wait = registration
                .wait_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registration.quiescent.notify_all();
        }
    }
}

/// Register or clear a process-global native profiling callback.
///
/// Passing `None` clears the callback. The caller owns `context` and must keep
/// it alive until the hook is cleared. Clearing/replacing the hook waits for
/// callbacks already using the prior context. A callback must not itself call
/// this registration function.
#[unsafe(no_mangle)]
pub extern "C" fn willow_runtime_set_event_hook_v1(
    hook: Option<RuntimeEventHookV1>,
    context: *mut c_void,
) {
    HOOK_ENABLED.store(false, Ordering::Release);
    let mut state = HOOK_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = state.registration.take();
    if let Some(previous) = previous {
        let mut wait = previous
            .wait_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while previous.active_callbacks.load(Ordering::Acquire) != 0 {
            wait = previous
                .quiescent
                .wait(wait)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
    if let Some(hook) = hook {
        state.registration = Some(Arc::new(HookRegistration {
            hook,
            context: context as usize,
            active_callbacks: AtomicUsize::new(0),
            wait_lock: Mutex::new(()),
            quiescent: Condvar::new(),
        }));
        HOOK_ENABLED.store(true, Ordering::Release);
    }
}

/// Write a versioned cumulative runtime snapshot. Returns 0 on success and -1
/// for a null output pointer.
#[unsafe(no_mangle)]
pub extern "C" fn willow_runtime_metrics_snapshot_v1(out: *mut WillowRuntimeMetricsV1) -> i32 {
    let Some(out) = (unsafe { out.as_mut() }) else {
        return -1;
    };
    *out = WillowRuntimeMetricsV1 {
        version: RUNTIME_METRICS_VERSION,
        struct_size: std::mem::size_of::<WillowRuntimeMetricsV1>() as u32,
        task_spawns: TASK_SPAWNS.load(Ordering::Relaxed),
        task_wakes: TASK_WAKES.load(Ordering::Relaxed),
        task_polls: TASK_POLLS.load(Ordering::Relaxed),
        task_parks: TASK_PARKS.load(Ordering::Relaxed),
        task_yields: TASK_YIELDS.load(Ordering::Relaxed),
        task_preempts: TASK_PREEMPTS.load(Ordering::Relaxed),
        task_completions: TASK_COMPLETIONS.load(Ordering::Relaxed),
        task_cancel_requests: TASK_CANCEL_REQUESTS.load(Ordering::Relaxed),
        task_cancellations: TASK_CANCELLATIONS.load(Ordering::Relaxed),
        task_panics: TASK_PANICS.load(Ordering::Relaxed),
        timer_wakes: TIMER_WAKES.load(Ordering::Relaxed),
        netpoll_waits: NETPOLL_WAITS.load(Ordering::Relaxed),
        netpoll_wakes: NETPOLL_WAKES.load(Ordering::Relaxed),
        channel_waits: CHANNEL_WAITS.load(Ordering::Relaxed),
        blocking_detaches: BLOCKING_DETACHES.load(Ordering::Relaxed),
    };
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static HOOK_TASK: AtomicU64 = AtomicU64::new(0);

    struct BlockingHookContext {
        entered: AtomicBool,
        release: AtomicBool,
    }

    unsafe extern "C" fn capture(event: *const WillowRuntimeEventV1, context: *mut c_void) {
        let event = unsafe { &*event };
        HOOK_TASK.store(event.task_id, Ordering::Release);
        let calls = context.cast::<AtomicU64>();
        unsafe { &*calls }.fetch_add(1, Ordering::AcqRel);
    }

    unsafe extern "C" fn blocking_capture(
        _event: *const WillowRuntimeEventV1,
        context: *mut c_void,
    ) {
        let context = unsafe { &*context.cast::<BlockingHookContext>() };
        context.entered.store(true, Ordering::Release);
        while !context.release.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
    }

    #[test]
    fn observability_01_snapshot_is_versioned_and_null_checked() {
        assert_eq!(willow_runtime_metrics_snapshot_v1(std::ptr::null_mut()), -1);
        let mut snapshot = WillowRuntimeMetricsV1::default();
        assert_eq!(willow_runtime_metrics_snapshot_v1(&mut snapshot), 0);
        assert_eq!(snapshot.version, RUNTIME_METRICS_VERSION);
        assert_eq!(
            snapshot.struct_size as usize,
            std::mem::size_of::<WillowRuntimeMetricsV1>()
        );
    }

    #[test]
    fn observability_02_record_updates_counter_and_hook() {
        let _guard = crate::gc::runtime_test_guard();
        let mut before = WillowRuntimeMetricsV1::default();
        willow_runtime_metrics_snapshot_v1(&mut before);
        let calls = AtomicU64::new(0);
        willow_runtime_set_event_hook_v1(
            Some(capture),
            (&calls as *const AtomicU64).cast_mut().cast(),
        );
        record(RuntimeEventKind::TaskSpawn, Some(3), 77, 0);
        willow_runtime_set_event_hook_v1(None, std::ptr::null_mut());

        let mut after = WillowRuntimeMetricsV1::default();
        willow_runtime_metrics_snapshot_v1(&mut after);
        assert_eq!(after.task_spawns, before.task_spawns + 1);
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(HOOK_TASK.load(Ordering::Acquire), 77);
    }

    #[test]
    fn observability_03_count_events_add_the_reported_batch() {
        let _guard = crate::gc::runtime_test_guard();
        let mut before = WillowRuntimeMetricsV1::default();
        willow_runtime_metrics_snapshot_v1(&mut before);
        record(RuntimeEventKind::TimerWake, None, 0, 5);
        record(RuntimeEventKind::NetpollWake, None, 0, 3);
        let mut after = WillowRuntimeMetricsV1::default();
        willow_runtime_metrics_snapshot_v1(&mut after);
        assert_eq!(after.timer_wakes, before.timer_wakes + 5);
        assert_eq!(after.netpoll_wakes, before.netpoll_wakes + 3);
    }

    #[test]
    fn observability_04_clearing_hook_waits_for_active_context_user() {
        let _guard = crate::gc::runtime_test_guard();
        let context = BlockingHookContext {
            entered: AtomicBool::new(false),
            release: AtomicBool::new(false),
        };
        let cleared = AtomicBool::new(false);
        willow_runtime_set_event_hook_v1(
            Some(blocking_capture),
            (&context as *const BlockingHookContext).cast_mut().cast(),
        );
        std::thread::scope(|scope| {
            scope.spawn(|| record(RuntimeEventKind::TaskPoll, Some(0), 91, 0));
            while !context.entered.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            scope.spawn(|| {
                willow_runtime_set_event_hook_v1(None, std::ptr::null_mut());
                cleared.store(true, Ordering::Release);
            });
            std::thread::sleep(std::time::Duration::from_millis(5));
            assert!(!cleared.load(Ordering::Acquire));
            context.release.store(true, Ordering::Release);
        });
        assert!(cleared.load(Ordering::Acquire));
    }

    #[test]
    fn observability_05_no_sink_updates_metrics_without_building_an_event() {
        let _guard = crate::gc::runtime_test_guard();
        willow_runtime_set_event_hook_v1(None, std::ptr::null_mut());
        if *SCHED_TRACE || *TASK_TRACE {
            return;
        }
        let before = BUILT_EVENTS.load(Ordering::Relaxed);
        record(RuntimeEventKind::TaskPoll, Some(0), 123, 0);
        assert_eq!(BUILT_EVENTS.load(Ordering::Relaxed), before);
    }
}
