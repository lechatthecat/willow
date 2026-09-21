use super::*;

// ---------------------------------------------------------------------------
// TypeInfo registry
// ---------------------------------------------------------------------------

/// Trace function: given a payload pointer, expose the addresses of all mutable
/// GC-reference slots it owns. Full marking loads the slots; minor collection
/// can additionally replace a moved young pointer in place.
pub type TraceFn = unsafe fn(payload: *mut u8, slots: &mut Vec<*mut *mut u8>);

/// Snapshot child VALUES while holding any required container locks. Unlike
/// TraceFn this callback must be safe while mutators run; exported slots cannot
/// outlive a lock guard. It must not allocate GC memory or reach a safepoint.
/// Objects without this hook are traced during remark.
pub type ConcurrentTraceFn = unsafe fn(payload: *mut u8, children: &mut Vec<*mut u8>);

/// Bounded concurrent snapshot. Append at most `limit` child values and return
/// a strictly advancing cursor, Done, or Retry without children when a native
/// lock is busy. Cursors must survive mutation and have a finite epoch bound:
/// stable indexed slots plus deletion/insertion barriers are sufficient; restarting
/// a mutable hash iterator or skipping a changing prefix is not. The callback
/// must bound all work (including empty slots) and may not allocate GC memory,
/// safepoint, retain a lock across calls or return mutable slot addresses.
pub type ConcurrentTraceSliceFn = unsafe fn(
    payload: *mut u8,
    cursor: usize,
    limit: usize,
    children: &mut Vec<*mut u8>,
) -> TraceSliceProgress;

pub enum TraceSliceProgress {
    Done,
    Continue(usize),
    Retry,
}

/// Load a GC reference shared with the concurrent marker.
/// # Safety
/// The aligned slot must stay allocated; all concurrent writes must be atomic.
pub(crate) unsafe fn load_gc_reference(slot: *mut *mut u8) -> *mut u8 {
    unsafe { std::sync::atomic::AtomicPtr::from_ptr(slot).load(Ordering::Acquire) }
}

/// Publish a GC reference shared with the concurrent marker. Call the write
/// barrier before publishing any non-null edge.
/// # Safety
/// The aligned slot must stay allocated and contain a reference-sized word.
pub(crate) unsafe fn store_gc_reference(slot: *mut *mut u8, value: *mut u8) {
    unsafe { std::sync::atomic::AtomicPtr::from_ptr(slot).store(value, Ordering::Release) }
}

pub(super) fn type_registry() -> &'static Mutex<HashMap<u32, TraceFn>> {
    &runtime().trace_registry
}

/// Register a trace function for `type_id`.  Call once per class at startup.
pub fn willow_register_type(type_id: u32, trace: TraceFn) {
    type_registry().lock().unwrap().insert(type_id, trace);
    // A replacement legacy hook must not inherit another implementation's
    // concurrent contract. Native registration installs its matched hook next.
    runtime()
        .concurrent_trace_registry
        .lock()
        .unwrap()
        .remove(&type_id);
    runtime()
        .concurrent_slice_registry
        .lock()
        .unwrap()
        .remove(&type_id);
}

/// Unregister the trace function for `type_id`.
pub fn willow_unregister_type(type_id: u32) {
    type_registry().lock().unwrap().remove(&type_id);
    runtime()
        .concurrent_trace_registry
        .lock()
        .unwrap()
        .remove(&type_id);
    runtime()
        .concurrent_slice_registry
        .lock()
        .unwrap()
        .remove(&type_id);
    runtime()
        .registry_generation
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

/// Finalizer: given a payload pointer, release any non-GC resources the object
/// owns (e.g. a boxed Rust collection) just before the object is freed by the
/// sweep phase.  Must not allocate GC memory or touch GC state.
pub type DropFn = unsafe fn(payload: *mut u8);

/// One runtime-native GC payload's trace/finalizer hooks.
///
/// Native containers use this target-independent descriptor with
/// [`NativeGcRegistration`] instead of open-coding generation atomics, a
/// registration mutex, and two registry calls in every module.
#[derive(Clone, Copy)]
pub struct NativeGcType {
    pub type_id: u32,
    pub trace: Option<TraceFn>,
    pub drop_fn: Option<DropFn>,
    pub concurrent_trace: Option<ConcurrentTraceFn>,
    pub concurrent_slice: Option<ConcurrentTraceSliceFn>,
}

impl NativeGcType {
    pub const fn new(type_id: u32, trace: Option<TraceFn>, drop_fn: Option<DropFn>) -> Self {
        Self {
            type_id,
            trace,
            drop_fn,
            concurrent_trace: None,
            concurrent_slice: None,
        }
    }
    pub const fn with_concurrent_trace(mut self, trace: ConcurrentTraceFn) -> Self {
        self.concurrent_trace = Some(trace);
        self
    }
    pub const fn with_concurrent_slice(mut self, trace: ConcurrentTraceSliceFn) -> Self {
        self.concurrent_slice = Some(trace);
        self
    }
}

/// Installs a module's native GC hooks at most once per registry generation.
///
/// `willow_gc_init` and explicit unregistration invalidate the runtime
/// registries by advancing their generation. The allocation hot path pays one
/// acquire load; only the first allocation in a generation takes this mutex.
pub struct NativeGcRegistration {
    registered_generation: AtomicU64,
    lock: Mutex<()>,
}

impl NativeGcRegistration {
    pub const fn new() -> Self {
        Self {
            registered_generation: AtomicU64::new(0),
            lock: Mutex::new(()),
        }
    }

    /// Ensure every descriptor is installed. Returns `true` only to the caller
    /// that performed registration for this generation.
    pub fn ensure(&self, types: &[NativeGcType]) -> bool {
        let generation = registry_generation();
        if self.registered_generation.load(Ordering::Acquire) == generation {
            return false;
        }
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let generation = registry_generation();
        if self.registered_generation.load(Ordering::Acquire) == generation {
            return false;
        }
        for native in types {
            if let Some(trace) = native.trace {
                willow_register_type(native.type_id, trace);
            }
            if let Some(trace) = native.concurrent_trace {
                runtime()
                    .concurrent_trace_registry
                    .lock()
                    .unwrap()
                    .insert(native.type_id, trace);
            }
            if let Some(trace) = native.concurrent_slice {
                runtime()
                    .concurrent_slice_registry
                    .lock()
                    .unwrap()
                    .insert(native.type_id, trace);
            }
            if let Some(drop_fn) = native.drop_fn {
                willow_register_drop(native.type_id, drop_fn);
            }
        }
        self.registered_generation
            .store(generation, Ordering::Release);
        true
    }
}

impl Default for NativeGcRegistration {
    fn default() -> Self {
        Self::new()
    }
}

pub(super) fn drop_registry() -> &'static Mutex<HashMap<u32, DropFn>> {
    &runtime().drop_registry
}

/// Register a finalizer for `type_id`, run by the sweep phase before an object
/// of that type is deallocated.
pub fn willow_register_drop(type_id: u32, drop_fn: DropFn) {
    drop_registry().lock().unwrap().insert(type_id, drop_fn);
}

pub(super) fn lookup_drop(type_id: u32) -> Option<DropFn> {
    drop_registry().lock().unwrap().get(&type_id).copied()
}

/// Dead storage must be reclaimed exactly once even when a native destructor
/// unwinds. Retrying a partially executed destructor can double-free its native
/// resources. Hooks must remain leaf operations: no GC access or safepoints.
pub(super) unsafe fn run_drop_hook(drop_fn: DropFn, payload: *mut u8) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: caller owns a dead, still-allocated payload for this hook.
        unsafe { drop_fn(payload) };
    }));
    if let Err(panic_payload) = result {
        crate::gc_telemetry::workers::record_failure(
            crate::gc_telemetry::workers::Failure::DropPanic,
        );
        discard_callback_panic(panic_payload);
    }
}

pub(super) fn discard_callback_panic(payload: Box<dyn std::any::Any + Send>) {
    // A panic-payload destructor must not escape a recovered native callback.
    if let Err(secondary) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
    {
        std::mem::forget(secondary);
    }
}

/// Current hook-registry generation. This changes only when existing
/// registrations are invalidated, never when another type is merely added.
pub(crate) fn registry_generation() -> u64 {
    runtime()
        .registry_generation
        .load(std::sync::atomic::Ordering::Acquire)
}
