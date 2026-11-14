//! Ownership of GC storage and worker policy in the process-default ABI instance.
//!
//! Worker configuration exists before lazy GC or scheduler initialization, so
//! selecting a worker configuration cannot recursively initialize a scheduler.
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use crate::scheduler::RuntimeWorkerConfig;

pub(crate) struct Runtime {
    pub(crate) scheduler_hooks: crate::observation::SchedulerHooks,
    pub(crate) service_owner: OnceLock<std::sync::Weak<Runtime>>,
    // Fields drop in declaration order. Join workers and release their service
    // owners before reclaiming collector storage. Runners must not own this
    // Runtime (or its pool), which would create an ownership cycle/self-join.
    pub(crate) worker_pool: Mutex<crate::scheduler::PersistentWorkers>,
    pub(crate) affinities: LazyLock<
        std::sync::Arc<
            crate::worker_affinity::WorkerAffinities<crate::worker_affinity::RuntimeObservation>,
        >,
    >,
    pub(crate) parking: LazyLock<std::sync::Arc<crate::scheduler::Parking>>,
    pub(crate) future_handles: LazyLock<Mutex<crate::future::Handles>>,
    pub(crate) netpoll: crate::netpoll::NetpollService,
    pub(crate) blocking: crate::blocking::BlockingPools,
    scheduler: OnceLock<crate::scheduler::SchedulerServices>,
    pub(crate) gc: LazyLock<crate::gc::GcRuntime>,
    pub(crate) workers: WorkerConfiguration,
    pub(crate) marker_count: LazyLock<usize>,
    pub(crate) optional_gc_deadline: AtomicU64,
    /// The program declared mutable statics, so drives must use one worker.
    pub(crate) single_worker_statics: std::sync::atomic::AtomicBool,
    /// Closed by a worker finalizing a fatal panic; see the scheduler claim gate.
    pub(crate) fatal_panic_pending: std::sync::atomic::AtomicBool,
    /// `willow_gc_init` ran, so diagnostics may inspect this runtime's heap.
    pub(crate) diagnostic_ready: std::sync::atomic::AtomicBool,
    /// Generated literal slots this runtime initialized, cleared on heap reset.
    pub(crate) literal_slots: Mutex<Vec<&'static std::sync::atomic::AtomicPtr<u8>>>,
    pub(crate) events: crate::observability::RuntimeEvents,
    pub(crate) barrier_counts: crate::gc::BarrierCounts,
    pub(crate) diagnostics: crate::diagnostic_settings::RuntimeDiagnostics,
    pub(crate) telemetry: crate::gc_telemetry::Telemetry,
    pub(crate) frame_reclaim: LazyLock<std::sync::Arc<crate::frame_reclaim::Domain>>,
    pub(crate) frame_lifetime: crate::frame_lifetime::FrameLifetime,
    pub(crate) args: crate::args::Arguments,
}

impl Runtime {
    pub(crate) const fn new(source: fn() -> RuntimeWorkerConfig) -> Self {
        Self::with_gc_initializer(source, crate::gc::GcRuntime::initialize)
    }

    const fn with_gc_initializer(
        source: fn() -> RuntimeWorkerConfig,
        gc: fn() -> crate::gc::GcRuntime,
    ) -> Self {
        Self {
            service_owner: OnceLock::new(),
            scheduler_hooks: crate::observation::SchedulerHooks::new(),
            frame_reclaim: LazyLock::new(|| {
                std::sync::Arc::new(crate::frame_reclaim::Domain::new())
            }),
            frame_lifetime: crate::frame_lifetime::FrameLifetime::new(),
            args: crate::args::Arguments::new(),
            affinities: LazyLock::new(|| {
                std::sync::Arc::new(crate::worker_affinity::WorkerAffinities::default())
            }),
            worker_pool: Mutex::new(crate::scheduler::PersistentWorkers::new()),
            scheduler: OnceLock::new(),
            netpoll: crate::netpoll::NetpollService::new(),
            blocking: crate::blocking::BlockingPools::new(),
            future_handles: LazyLock::new(|| Mutex::new(crate::future::Handles::default())),
            parking: LazyLock::new(|| std::sync::Arc::new(crate::scheduler::Parking::default())),
            gc: LazyLock::new(gc),
            workers: WorkerConfiguration::new(source),
            marker_count: crate::gc::marker_configuration(),
            optional_gc_deadline: AtomicU64::new(0),
            single_worker_statics: std::sync::atomic::AtomicBool::new(false),
            fatal_panic_pending: std::sync::atomic::AtomicBool::new(false),
            diagnostic_ready: std::sync::atomic::AtomicBool::new(false),
            literal_slots: Mutex::new(Vec::new()),
            events: crate::observability::RuntimeEvents::new(),
            barrier_counts: crate::gc::BarrierCounts::new(),
            diagnostics: crate::diagnostic_settings::RuntimeDiagnostics::new(),
            telemetry: crate::gc_telemetry::Telemetry::new(),
        }
    }

    /// Shared private owners can safely lend services to background workers.
    pub(crate) fn shared(source: fn() -> RuntimeWorkerConfig) -> std::sync::Arc<Self> {
        let owner = std::sync::Arc::new(Self::new(source));
        owner
            .service_owner
            .set(std::sync::Arc::downgrade(&owner))
            .expect("new runtime has no service owner");
        owner
    }

    pub(crate) fn scheduler(&self) -> &crate::scheduler::SchedulerServices {
        self.scheduler.get_or_init(|| {
            crate::scheduler::SchedulerServices::new_in(
                self.workers.effective().active_workers(),
                std::sync::Arc::clone(&self.frame_reclaim),
                std::sync::Arc::clone(&self.parking),
                crate::worker_affinity::Registry::for_runtime(self),
            )
        })
    }
}

static DEFAULT_RUNTIME: Runtime = Runtime::with_gc_initializer(
    crate::scheduler::environment_worker_config,
    crate::gc::GcRuntime::initialize_default,
);

thread_local! {
    // The runtime this OS thread serves. Constant-initialized without a
    // destructor, so it stays readable during TLS teardown. Runtime-spawned
    // threads select their creator once at start; nothing switches it later.
    static SELECTED: std::cell::Cell<*const Runtime> =
        const { std::cell::Cell::new(&raw const DEFAULT_RUNTIME) };
}

/// The runtime served by this thread: the process-default ABI instance unless
/// the thread was started for another `'static` runtime. ABI entry points and
/// internal default paths resolve it with one initialized TLS pointer read.
#[inline]
pub(crate) fn current() -> &'static Runtime {
    // SAFETY: only `&'static Runtime` values are ever stored.
    unsafe { &*SELECTED.get() }
}

/// The process-default instance whose storage backs exported ABI data symbols.
#[inline]
pub(crate) fn process_default() -> &'static Runtime {
    &DEFAULT_RUNTIME
}

impl Runtime {
    #[inline]
    pub(crate) fn is_process_default(&self) -> bool {
        std::ptr::eq(self, &DEFAULT_RUNTIME)
    }

    /// The SATB phase gate for this runtime. The process default keeps the
    /// exported ABI symbol and never forces lazy collector initialization.
    #[inline]
    pub(crate) fn mark_phase(&self) -> &std::sync::atomic::AtomicU8 {
        if self.is_process_default() {
            crate::gc::default_mark_phase()
        } else {
            self.owned_mark_phase()
        }
    }

    // Non-default runtimes are test/private instances; keeping their lazy
    // collector lookup out of line leaves the default gate a straight-line
    // compare and address in every inlined barrier.
    #[cold]
    #[inline(never)]
    fn owned_mark_phase(&self) -> &std::sync::atomic::AtomicU8 {
        self.gc.mark_phase()
    }

    /// The card-cache epoch for this runtime; the process default keeps the
    /// exported ABI symbol.
    #[inline]
    pub(crate) fn card_epoch(&self) -> &AtomicU64 {
        if self.is_process_default() {
            crate::gc::default_card_epoch()
        } else {
            self.owned_card_epoch()
        }
    }

    #[cold]
    #[inline(never)]
    fn owned_card_epoch(&self) -> &AtomicU64 {
        self.gc.card_epoch()
    }

    /// Serve this runtime on the calling thread.
    /// Production threads call this once at start, so thread-keyed caches never
    /// mix instances. Test fixtures may switch a thread between runtimes; the
    /// served old-generation buffer is retired from the previous runtime first.
    pub(crate) fn select_for_thread(&'static self) {
        assert!(
            crate::gc::registration_permits_selection(self),
            "a registered mutator thread cannot change the runtime it serves"
        );
        let previous = current();
        if !std::ptr::eq(previous, self) {
            // Test fixtures switch runtimes on one thread; production threads
            // select once at start, where this is the same-runtime no-op.
            crate::gc::retire_served_old_lab_in(previous);
            SELECTED.set(self);
            crate::gc::refresh_served_registration(self);
        }
        crate::generated_context::publish_selected_gates();
    }
}

/// Spawn a named OS thread that serves the caller's current runtime.
pub(crate) fn spawn_serving<F, T>(
    builder: std::thread::Builder,
    f: F,
) -> std::io::Result<std::thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let owner = current();
    builder.spawn(move || {
        owner.select_for_thread();
        f()
    })
}

/// Bind a thread body to the caller's current runtime. Used where the thread
/// is spawned through an API other than [`spawn_serving`] (scoped threads).
pub(crate) fn serving<F, T>(f: F) -> impl FnOnce() -> T
where
    F: FnOnce() -> T,
{
    let owner = current();
    move || {
        owner.select_for_thread();
        f()
    }
}

/// Test fixture: serve an independent runtime on this test thread.
///
/// Tests that reset or inspect runtime state used to serialize on one lock
/// around the process-default instance. Each test thread now serves its own
/// runtime, so concurrent tests cannot reset each other's heap or scheduler.
/// Runtimes are leaked (`'static`, like the default) and recycled: a thread
/// returns its runtime when it exits, so the number created equals the peak
/// number of concurrent test threads, and reuse matches the old sequential
/// reuse of the default instance. Threads already serving a fixture runtime,
/// including threads a test bound with [`serving`], keep it.
#[cfg(test)]
pub(crate) fn isolated_runtime() -> IsolatedRuntime {
    // Registered first, so the lease's TLS destructor runs after every
    // runtime cache this thread later creates has flushed into the runtime.
    let runtime = isolated::LEASE.with(|lease| {
        let mut lease = lease.borrow_mut();
        if let Some(lease) = lease.as_ref() {
            return lease.0;
        }
        if !current().is_process_default() {
            // Serving a runtime it was bound to by its spawner.
            return current();
        }
        let runtime = isolated::POOL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop()
            .unwrap_or_else(|| {
                let mut runtime = Runtime::new(crate::scheduler::environment_worker_config);
                // Tests assert per-variant barrier telemetry.
                runtime.diagnostics.barrier_counting = std::sync::LazyLock::new(|| true);
                Box::leak(Box::new(runtime))
            });
        runtime.select_for_thread();
        *lease = Some(isolated::Lease(runtime));
        runtime
    });
    crate::gc_telemetry::failure_record::install_test_panic_hook();
    IsolatedRuntime { runtime }
}

/// Proof that the test thread serves a fixture runtime. Dropping it changes
/// nothing; the runtime returns to the pool when the thread exits.
#[cfg(test)]
pub(crate) struct IsolatedRuntime {
    pub(crate) runtime: &'static Runtime,
}

/// Test fixture state keyed by the runtime the accessing thread serves.
///
/// Tests observe runtime callbacks (`extern "C"` polls, drop hooks, tracers)
/// through statics. With one runtime per test thread, a static shared by
/// several tests is instead one value per runtime: callbacks run on threads
/// serving that runtime, so concurrent tests never see each other's values.
#[cfg(test)]
pub(crate) struct PerRuntime<T: 'static> {
    init: fn() -> T,
    values: std::sync::Mutex<Vec<(usize, &'static T)>>,
}

#[cfg(test)]
impl<T: Sync + 'static> PerRuntime<T> {
    pub(crate) const fn new(init: fn() -> T) -> Self {
        Self {
            init,
            values: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn get(&self) -> &'static T {
        let key = std::ptr::from_ref(current()) as usize;
        let mut values = self
            .values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, value)) = values.iter().find(|(owner, _)| *owner == key) {
            return value;
        }
        let value: &'static T = Box::leak(Box::new((self.init)()));
        values.push((key, value));
        value
    }
}

#[cfg(test)]
impl<T: Sync + 'static> std::ops::Deref for PerRuntime<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.get()
    }
}

#[cfg(test)]
mod isolated {
    use super::Runtime;

    pub(super) static POOL: std::sync::Mutex<Vec<&'static Runtime>> =
        std::sync::Mutex::new(Vec::new());

    pub(super) struct Lease(pub(super) &'static Runtime);

    impl Drop for Lease {
        fn drop(&mut self) {
            POOL.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(self.0);
        }
    }

    thread_local! {
        pub(super) static LEASE: std::cell::RefCell<Option<Lease>> =
            const { std::cell::RefCell::new(None) };
    }
}

/// A runtime's configurable worker policy. The source is queried per call so
/// the default environment policy retains its existing semantics. Explicit
/// counts and scoped single-worker leases belong to this owner, not a process
/// environment mutation or a test-build branch.
pub(crate) struct WorkerConfiguration {
    source: fn() -> RuntimeWorkerConfig,
    worker_override: AtomicUsize,
    single_worker_leases: AtomicUsize,
}

impl WorkerConfiguration {
    pub(crate) const fn new(source: fn() -> RuntimeWorkerConfig) -> Self {
        Self {
            source,
            worker_override: AtomicUsize::new(0),
            single_worker_leases: AtomicUsize::new(0),
        }
    }

    pub(crate) fn effective(&self) -> RuntimeWorkerConfig {
        if self.single_worker_leases.load(Ordering::Acquire) > 0 {
            return RuntimeWorkerConfig::with_workers(1);
        }
        if let workers @ 1.. = self.worker_override.load(Ordering::Acquire) {
            return RuntimeWorkerConfig::with_workers(workers);
        }
        (self.source)()
    }

    /// Zero restores the source policy; positive values select that count.
    pub(crate) fn replace_override(&self, workers: usize) -> usize {
        self.worker_override.swap(workers, Ordering::AcqRel)
    }

    pub(crate) fn single_worker(&self) -> SingleWorkerLease<'_> {
        self.single_worker_leases.fetch_add(1, Ordering::AcqRel);
        SingleWorkerLease { owner: self }
    }
}

pub struct SingleWorkerLease<'a> {
    owner: &'a WorkerConfiguration,
}

impl Drop for SingleWorkerLease<'_> {
    fn drop(&mut self) {
        let previous = self
            .owner
            .single_worker_leases
            .fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "single-worker lease depth underflow");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn four_workers() -> RuntimeWorkerConfig {
        RuntimeWorkerConfig::with_workers(4)
    }

    fn eight_workers() -> RuntimeWorkerConfig {
        RuntimeWorkerConfig::with_workers(8)
    }

    #[test]
    fn runtime_worker_policies_are_independent_during_concurrent_leases() {
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [
                (four_workers as fn() -> _, 4),
                (eight_workers as fn() -> _, 8),
            ]
            .into_iter()
            .map(|(source, expected)| {
                let barrier = &barrier;
                scope.spawn(move || {
                    let runtime = Runtime::new(source);
                    let mut correct = true;
                    for _ in 0..64 {
                        barrier.wait();
                        correct &= runtime.workers.effective().active_workers() == expected;
                        let previous = runtime.workers.replace_override(expected + 1);
                        correct &= previous == 0;
                        let first = runtime.workers.single_worker();
                        let second = runtime.workers.single_worker();
                        barrier.wait();
                        correct &= runtime.workers.effective().active_workers() == 1;
                        drop(first);
                        correct &= runtime.workers.effective().active_workers() == 1;
                        drop(second);
                        correct &= runtime.workers.effective().active_workers() == expected + 1;
                        runtime.workers.replace_override(0);
                    }
                    // Configuration must not initialize unrelated services.
                    correct && LazyLock::get(&runtime.gc).is_none()
                })
            })
            .collect();
            for handle in handles {
                assert!(handle.join().unwrap());
            }
        });
    }

    #[test]
    fn worker_policy_lease_unwind_and_override_restore_preserve_source() {
        let runtime = Runtime::new(four_workers);
        let old = runtime.workers.replace_override(8);
        let unwind = std::panic::catch_unwind(|| {
            let _lease = runtime.workers.single_worker();
            assert_eq!(runtime.workers.effective().active_workers(), 1);
            panic!("lease unwind");
        });
        assert!(unwind.is_err());
        assert_eq!(runtime.workers.effective().active_workers(), 8);
        assert_eq!(runtime.workers.replace_override(old), 8);
        assert_eq!(runtime.workers.effective().active_workers(), 4);
    }
}
