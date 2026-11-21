use super::*;

/// Immutable minor-collection view, including derived drop dispatch metadata.
/// The public trace/drop registration calls are separate transactions; capture
/// observes one point between those calls, never a mixture of different times.
#[derive(Default)]
pub(super) struct MinorRegistrySnapshot {
    pub trace: IdMap<u32, TraceFn>,
    /// Fast ID hashing is prepared once per registry generation.
    pub drops: IdMap<u32, DropFn>,
    /// Only opaque native payloads are excluded from inline-TLAB allocation.
    pub generated_drop: bool,
    generation: u64,
    /// Smallest traced type id and the span up to the largest: a type id
    /// outside the range has no hook without a map probe (generated
    /// objects, the common case, carry no hook).
    trace_min: u32,
    trace_span: u32,
}

impl MinorRegistrySnapshot {
    /// The trace hook registered for `type_id`. O(1); a probe only for a type
    /// id inside the registered range.
    #[inline]
    pub(super) fn trace_hook(&self, type_id: u32) -> Option<TraceFn> {
        if type_id.wrapping_sub(self.trace_min) > self.trace_span || self.trace.is_empty() {
            return None;
        }
        self.trace.get(&type_id).copied()
    }
}

#[derive(Default)]
pub(super) struct MinorRegistryCache {
    generation: u64,
    snapshot: Option<std::sync::Arc<MinorRegistrySnapshot>>,
}

impl MinorRegistryCache {
    fn changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        // Also clear on wraparound; a stale generation can never alias a hit.
        self.snapshot = None;
    }
}

/// Lock order for writers/capture: minor cache, trace, concurrent hooks, drop.
/// Read-only legacy lookups need no cache lock. Registration invalidates without copying;
/// the next capture rebuilds once, avoiding quadratic startup registration.
pub(super) fn minor_registry_snapshot() -> std::sync::Arc<MinorRegistrySnapshot> {
    minor_registry_snapshot_in(runtime())
}

pub(super) fn minor_registry_snapshot_in(gc: &GcRuntime) -> std::sync::Arc<MinorRegistrySnapshot> {
    let mut cache = gc.minor_registry_cache.lock().unwrap();
    if let Some(snapshot) = &cache.snapshot
        && snapshot.generation == cache.generation
    {
        return std::sync::Arc::clone(snapshot);
    }
    let trace = gc.trace_registry.lock().unwrap();
    let drops = gc.drop_registry.lock().unwrap();
    let mut generated_drop = false;
    let drops = drops
        .iter()
        .map(|(&id, &hook)| {
            generated_drop = generated_drop
                || willow_abi::runtime_type_ids::runtime_type(id).is_none_or(|ty| {
                    ty.layout != willow_abi::runtime_type_ids::TypeLayout::OpaqueNative
                });
            (id, hook)
        })
        .collect();
    let trace_min = trace.keys().copied().min().unwrap_or(0);
    let trace_max = trace.keys().copied().max().unwrap_or(0);
    let snapshot = std::sync::Arc::new(MinorRegistrySnapshot {
        trace: trace.iter().map(|(&id, &hook)| (id, hook)).collect(),
        drops,
        generated_drop,
        generation: cache.generation,
        trace_min,
        trace_span: trace_max - trace_min,
    });
    cache.snapshot = Some(std::sync::Arc::clone(&snapshot));
    snapshot
}

pub(super) fn clear_hook_registries() {
    clear_hook_registries_in(runtime());
}

/// No type id has a trace hook: minimum `u32::MAX` above maximum zero.
pub(super) const EMPTY_TRACE_TYPE_RANGE: u64 = (u32::MAX as u64) << 32;

/// Called with the registry locks held, so widenings never race. An object
/// of a hooked type is allocated after its type's registration, so any
/// publication of it observes the widened range.
fn widen_trace_type_range(gc: &GcRuntime, type_id: u32) {
    let (min, max) = trace_type_range_in(gc);
    let range = (u64::from(min.min(type_id)) << 32) | u64::from(max.max(type_id));
    gc.trace_type_range.store(range, Ordering::Release);
}

/// Inclusive range of type ids that may have a trace hook (empty when the
/// minimum exceeds the maximum). It only widens: unregistering keeps it.
#[inline(always)]
pub(super) fn trace_type_range_in(gc: &GcRuntime) -> (u32, u32) {
    let range = gc.trace_type_range.load(Ordering::Acquire);
    ((range >> 32) as u32, range as u32)
}

fn clear_hook_registries_in(gc: &GcRuntime) {
    let mut cache = gc.minor_registry_cache.lock().unwrap();
    gc.trace_registry.lock().unwrap().clear();
    gc.concurrent_trace_registry.lock().unwrap().clear();
    gc.concurrent_slice_registry.lock().unwrap().clear();
    gc.drop_registry.lock().unwrap().clear();
    cache.changed();
    gc.minor_registry_generation.fetch_add(1, Ordering::Release);
}

#[cfg(test)]
#[path = "minor_registry_tests.rs"]
mod minor_registry_tests;

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
/// Legacy compatibility hook for collectors without concurrent old generation.
/// Concurrent old-generation collection requires `ConcurrentTraceSliceFn`.
pub type ConcurrentTraceFn = unsafe fn(payload: *mut u8, children: &mut Vec<*mut u8>);

/// Bounded concurrent snapshot. Append at most `limit` child values and return
/// a strictly advancing cursor, Done, or Retry without children when a native
/// lock is busy. Cursors must survive mutation and have a finite epoch bound:
/// stable indexed slots plus deletion/insertion barriers are sufficient; restarting
/// a mutable hash iterator or skipping a changing prefix is not. The callback
/// must bound all work (including empty slots) and may not allocate GC memory,
/// safepoint, retain a lock across calls or return mutable slot addresses.
/// `limit` is positive; Continue must advance by at most `limit` inspected
/// positions. Retry must append nothing. Every callback invocation, including
/// its synchronization, must complete with bounded work: use try-lock, never
/// wait for a mutator. A panic or invalid progress is a fatal contract violation;
/// the collector must never retry the object through an unrestricted hook.
/// These unsafe native callbacks are trusted to honor the work bound; the
/// collector additionally checks returned progress and child counts.
/// Recovery rejects the 128th consecutive Retry for the same object/cursor.
/// Successful progress resets that cursor's allowance. Retry must therefore be
/// transient; an indefinitely contended or non-progressing callback is fatal
/// before reclamation rather than eligible for unrestricted stopped tracing.
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

/// Effective native tracing contract. Bitmap-only payloads need no native hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceMode {
    StopOnly,
    ConcurrentSafe,
    SliceConcurrentSafe,
}

/// A concurrency capability carries its implementation, never just a flag.
#[derive(Clone, Copy)]
pub enum NativeTraceCapability {
    StopOnly,
    ConcurrentSafe(ConcurrentTraceFn),
    SliceConcurrentSafe(ConcurrentTraceSliceFn),
}

pub(super) fn trace_mode(concurrent: bool, slice: bool) -> TraceMode {
    if slice {
        TraceMode::SliceConcurrentSafe
    } else if concurrent {
        TraceMode::ConcurrentSafe
    } else {
        TraceMode::StopOnly
    }
}

/// Register matched hooks before publishing instances. Existing marking epochs
/// retain their captured callbacks. Replacing a live type's payload layout is
/// unsupported. All hooks must avoid GC allocation, safepoints and GC reentry;
/// concurrent hooks require synchronized value snapshots and short critical
/// sections. Slices additionally require bounded work and stable finite cursors.
///
/// # Safety
/// Use a type ID dedicated to this layout, excluding the untyped and bitmap IDs. Every
/// instance these callbacks may receive must be aligned, live and initialized. Previously captured
/// hooks must remain valid through their last collector epoch. Callbacks must not allocate GC
/// memory, enter the collector or safepoint. The stopped tracer must report exactly the
/// initialized managed edges of each such payload. Concurrent hooks must provide synchronized
/// snapshots without racing mutation or reclamation. A sliced hook must bound its work, use
/// valid stable cursors and eventually finish or request a retry.
///
/// ```compile_fail,E0133
/// use willow_runtime::gc::{willow_register_type_capability, NativeTraceCapability, TraceFn};
/// fn install(trace: TraceFn) {
///     willow_register_type_capability(1, trace, NativeTraceCapability::StopOnly);
/// }
/// ```
pub unsafe fn willow_register_type_capability(
    type_id: u32,
    stopped: TraceFn,
    capability: NativeTraceCapability,
) {
    let (trace, slice) = match capability {
        NativeTraceCapability::StopOnly => (None, None),
        NativeTraceCapability::ConcurrentSafe(trace) => (Some(trace), None),
        NativeTraceCapability::SliceConcurrentSafe(slice) => (None, Some(slice)),
    };
    // SAFETY: The registration contract couples these callbacks and execution modes to every
    // instance of the selected type.
    unsafe { register_hooks_in(runtime(), type_id, stopped, trace, slice) };
}

/// # Safety
/// Use a type ID dedicated to this layout, excluding the untyped and bitmap IDs. Every
/// instance these callbacks may receive must be aligned, live and initialized. Previously captured
/// hooks must remain valid through their last collector epoch. Callbacks must not allocate GC
/// memory, enter the collector or safepoint. The stopped tracer must report exactly the
/// initialized managed edges of each such payload. Concurrent hooks must provide synchronized
/// snapshots without racing mutation or reclamation. A sliced hook must bound its work, use
/// valid stable cursors and eventually finish or request a retry.
unsafe fn register_hooks_in(
    gc: &GcRuntime,
    type_id: u32,
    stopped: TraceFn,
    trace: Option<ConcurrentTraceFn>,
    slice: Option<ConcurrentTraceSliceFn>,
) {
    // Common lock order also makes epoch capture and diagnostics coherent.
    let mut cache = gc.minor_registry_cache.lock().unwrap();
    let mut legacy = gc.trace_registry.lock().unwrap();
    let mut traces = gc.concurrent_trace_registry.lock().unwrap();
    let mut slices = gc.concurrent_slice_registry.lock().unwrap();
    legacy.insert(type_id, stopped);
    widen_trace_type_range(gc, type_id);
    traces.remove(&type_id);
    slices.remove(&type_id);
    if let Some(trace) = trace {
        traces.insert(type_id, trace);
    }
    if let Some(slice) = slice {
        slices.insert(type_id, slice);
    }
    cache.changed();
    gc.minor_registry_generation.fetch_add(1, Ordering::Release);
}

/// Current registered native capabilities, in unspecified order. Already-running
/// epochs retain their own snapshot. Absent types have no native trace hook.
pub fn native_trace_modes() -> Vec<(u32, TraceMode)> {
    native_trace_modes_in(runtime())
}

fn native_trace_modes_in(gc: &GcRuntime) -> Vec<(u32, TraceMode)> {
    let legacy = gc.trace_registry.lock().unwrap();
    let traces = gc.concurrent_trace_registry.lock().unwrap();
    let slices = gc.concurrent_slice_registry.lock().unwrap();
    legacy
        .keys()
        .map(|&id| {
            (
                id,
                trace_mode(traces.contains_key(&id), slices.contains_key(&id)),
            )
        })
        .collect()
}

// One lookup per encountered type per stopped walk, including absent hooks.
// This avoids scanning every registered type for a small/empty stopped graph.
pub(super) fn lookup_trace_contract(type_id: u32) -> Option<(TraceFn, TraceMode)> {
    lookup_trace_contract_in(runtime(), type_id)
}

pub(super) fn lookup_trace_contract_in(
    gc: &GcRuntime,
    type_id: u32,
) -> Option<(TraceFn, TraceMode)> {
    let legacy = gc.trace_registry.lock().unwrap();
    let trace = *legacy.get(&type_id)?;
    let traces = gc.concurrent_trace_registry.lock().unwrap();
    let slices = gc.concurrent_slice_registry.lock().unwrap();
    Some((
        trace,
        trace_mode(traces.contains_key(&type_id), slices.contains_key(&type_id)),
    ))
}

pub(super) fn trace_registry_snapshot() -> (
    IdSet<u32>,
    IdMap<u32, ConcurrentTraceFn>,
    IdMap<u32, ConcurrentTraceSliceFn>,
) {
    trace_registry_snapshot_in(runtime())
}

pub(super) fn trace_registry_snapshot_in(
    gc: &GcRuntime,
) -> (
    IdSet<u32>,
    IdMap<u32, ConcurrentTraceFn>,
    IdMap<u32, ConcurrentTraceSliceFn>,
) {
    let legacy = gc.trace_registry.lock().unwrap();
    let traces = gc.concurrent_trace_registry.lock().unwrap();
    let slices = gc.concurrent_slice_registry.lock().unwrap();
    (
        legacy.keys().copied().collect(),
        traces.clone(),
        slices.clone(),
    )
}

/// Load a GC reference shared with the concurrent marker.
/// # Safety
/// The aligned slot must stay allocated; all concurrent writes must be atomic.
pub(crate) unsafe fn load_gc_reference(slot: *mut *mut u8) -> *mut u8 {
    // SAFETY: The traced slot is live and pointer-aligned. Concurrent readers and writers use
    // atomic pointer operations.
    unsafe { std::sync::atomic::AtomicPtr::from_ptr(slot).load(Ordering::Acquire) }
}

/// Publish a GC reference shared with the concurrent marker. Call the write
/// barrier before publishing any non-null edge.
/// # Safety
/// The aligned slot must stay allocated and contain a reference-sized word.
pub(crate) unsafe fn store_gc_reference(slot: *mut *mut u8, value: *mut u8) {
    // SAFETY: The traced slot is live and pointer-aligned. Concurrent readers and writers use
    // atomic pointer operations.
    unsafe { std::sync::atomic::AtomicPtr::from_ptr(slot).store(value, Ordering::Release) }
}

pub(super) fn type_registry() -> &'static Mutex<HashMap<u32, TraceFn>> {
    &runtime().trace_registry
}

/// Register a trace function for `type_id`.  Call once per class at startup.
///
/// # Safety
/// Use a type ID dedicated to this layout, excluding the untyped and bitmap IDs. Every
/// instance these callbacks may receive must be aligned, live and initialized. Previously captured
/// hooks must remain valid through their last collector epoch. Callbacks must not allocate GC
/// memory, enter the collector or safepoint. The stopped tracer must report exactly the
/// initialized managed edges of each such payload.
pub unsafe fn willow_register_type(type_id: u32, trace: TraceFn) {
    // SAFETY: The registration contract couples these callbacks and execution modes to every
    // instance of the selected type.
    unsafe { willow_register_type_capability(type_id, trace, NativeTraceCapability::StopOnly) };
}

/// Unregister all trace capabilities for `type_id`.
///
/// # Safety
/// Retire all instances that still need these trace hooks or retain a compatible tracing path.
/// Coordinate replacement with mutators and outstanding collector snapshots.
pub unsafe fn willow_unregister_type(type_id: u32) {
    // SAFETY: The caller has coordinated retirement of this type in the selected runtime.
    unsafe { unregister_type_in(runtime(), type_id) };
}

/// # Safety
/// All affected objects and collector epochs must satisfy [willow_unregister_type].
unsafe fn unregister_type_in(gc: &GcRuntime, type_id: u32) {
    let mut cache = gc.minor_registry_cache.lock().unwrap();
    let mut legacy = gc.trace_registry.lock().unwrap();
    let mut traces = gc.concurrent_trace_registry.lock().unwrap();
    let mut slices = gc.concurrent_slice_registry.lock().unwrap();
    legacy.remove(&type_id);
    traces.remove(&type_id);
    slices.remove(&type_id);
    cache.changed();
    gc.minor_registry_generation.fetch_add(1, Ordering::Release);
    gc.registry_generation.fetch_add(1, Ordering::AcqRel);
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
    type_id: u32,
    trace: Option<TraceFn>,
    drop_fn: Option<DropFn>,
    concurrent_trace: Option<ConcurrentTraceFn>,
    concurrent_slice: Option<ConcurrentTraceSliceFn>,
}

impl NativeGcType {
    /// # Safety
    /// The chosen ID must be reserved for this exact native representation. Every matching
    /// instance must satisfy the selected stopped trace and finalizer callbacks whenever the
    /// collector can invoke them. The stopped tracer must report exactly the initialized
    /// managed edges of each such payload. A finalizer must accept exclusive access to each
    /// dead initialized payload and release its owned resources exactly once.
    ///
    /// ```compile_fail,E0133
    /// use willow_runtime::gc::{NativeGcType, DropFn};
    /// fn descriptor(type_id: u32, drop_fn: DropFn) {
    ///     NativeGcType::new(type_id, None, Some(drop_fn));
    /// }
    /// ```
    pub const unsafe fn new(type_id: u32, trace: Option<TraceFn>, drop_fn: Option<DropFn>) -> Self {
        let Some(contract) = willow_abi::runtime_type_ids::runtime_type(type_id) else {
            panic!("native GC type is absent from the ABI schema");
        };
        assert!(
            contract.accepts_native_hooks(trace.is_some(), drop_fn.is_some()),
            "native GC hooks disagree with the ABI schema"
        );
        Self {
            type_id,
            trace,
            drop_fn,
            concurrent_trace: None,
            concurrent_slice: None,
        }
    }
    /// # Safety
    /// The callback must trace the same edges as the stopped tracer for every payload of this
    /// type, using synchronized reads that are safe alongside mutators and without GC reentry
    /// or safepoints.
    pub const unsafe fn with_concurrent_trace(mut self, trace: ConcurrentTraceFn) -> Self {
        assert!(
            self.trace.is_some(),
            "concurrent tracing requires a stopped hook"
        );
        self.concurrent_trace = Some(trace);
        self
    }
    /// # Safety
    /// The callback must traverse the same edges as the stopped tracer under concurrent
    /// mutation without GC reentry. It must respect the work budget, snapshot consistently and
    /// preserve a finite, valid cursor across calls or request a retry.
    pub const unsafe fn with_concurrent_slice(mut self, trace: ConcurrentTraceSliceFn) -> Self {
        assert!(
            self.trace.is_some(),
            "sliced tracing requires a stopped hook"
        );
        self.concurrent_slice = Some(trace);
        self
    }
}

/// Selects one native hook group in the owning collector. The selector is
/// immutable; generations and registration locks belong to `GcRuntime`.
/// Each group has one fixed descriptor set; replacing live native layouts is
/// unsupported, just as with direct hook registration.
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum NativeGcRegistration {
    Array,
    Map,
    Channel,
    BlockingCell,
    AsyncMutex,
    AsyncRwLock,
    Cancellation,
    Network,
}

const NATIVE_REGISTRATION_COUNT: usize = NativeGcRegistration::Network as usize + 1;

pub(super) struct NativeGcRegistrations {
    states: [NativeGcRegistrationState; NATIVE_REGISTRATION_COUNT],
}

impl NativeGcRegistrations {
    pub(super) const fn new() -> Self {
        Self {
            states: [const { NativeGcRegistrationState::new() }; NATIVE_REGISTRATION_COUNT],
        }
    }
}

/// Installs a module's native GC hooks at most once per owning registry generation.
///
/// `willow_gc_init` and explicit unregistration invalidate the runtime
/// registries by advancing their generation. The allocation hot path pays one
/// generation load and one cached-generation load; only the first allocation
/// in a generation takes this mutex.
struct NativeGcRegistrationState {
    registered_generation: AtomicU64,
    lock: Mutex<()>,
}

impl NativeGcRegistrationState {
    const fn new() -> Self {
        Self {
            registered_generation: AtomicU64::new(0),
            lock: Mutex::new(()),
        }
    }
}

impl NativeGcRegistration {
    /// Ensure every descriptor is installed. Returns `true` only to the caller
    /// that performed registration for this generation.
    ///
    /// # Safety
    /// Supply this group’s complete designated descriptor set, with callback layouts
    /// compatible with all live instances and collector snapshots.
    pub unsafe fn ensure(&self, types: &[NativeGcType]) -> bool {
        // SAFETY: The caller supplies the designated set for this runtime and group.
        unsafe { self.ensure_in(runtime(), types) }
    }

    /// # Safety
    /// In this collector every group consumer must agree on its complete descriptor set.
    /// All installed callbacks must be compatible with every payload they may receive.
    unsafe fn ensure_in(&self, gc: &GcRuntime, types: &[NativeGcType]) -> bool {
        let state = &gc.native_registrations.states[*self as usize];
        let generation = gc.registry_generation.load(Ordering::Acquire);
        if state.registered_generation.load(Ordering::Acquire) == generation {
            return false;
        }
        let _guard = state
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let generation = gc.registry_generation.load(Ordering::Acquire);
        if state.registered_generation.load(Ordering::Acquire) == generation {
            return false;
        }
        for native in types {
            if let Some(trace) = native.trace {
                // SAFETY: The registration contract couples these callbacks and execution
                // modes to every instance of the selected type.
                unsafe {
                    register_hooks_in(
                        gc,
                        native.type_id,
                        trace,
                        native.concurrent_trace,
                        native.concurrent_slice,
                    )
                };
            }
            if let Some(drop_fn) = native.drop_fn {
                // SAFETY: The registration contract couples these callbacks and execution
                // modes to every instance of the selected type.
                unsafe { register_drop_in(gc, native.type_id, drop_fn) };
            }
        }
        state
            .registered_generation
            .store(generation, Ordering::Release);
        true
    }
}

pub(super) fn drop_registry() -> &'static Mutex<HashMap<u32, DropFn>> {
    &runtime().drop_registry
}

/// Register a finalizer for `type_id`, run by the sweep phase before an object
/// of that type is deallocated.
///
/// # Safety
/// Use a type ID dedicated to this layout, excluding the untyped and bitmap IDs. Every
/// instance these callbacks may receive must be aligned, live and initialized. Previously captured
/// hooks must remain valid through their last collector epoch. Callbacks must not allocate GC
/// memory, enter the collector or safepoint. A finalizer must accept exclusive access to each
/// dead initialized payload and release its owned resources exactly once.
pub unsafe fn willow_register_drop(type_id: u32, drop_fn: DropFn) {
    // SAFETY: The registration contract couples these callbacks and execution modes to every
    // instance of the selected type.
    unsafe { register_drop_in(runtime(), type_id, drop_fn) };
}

/// # Safety
/// Use a type ID dedicated to this layout, excluding the untyped and bitmap IDs. Every
/// instance these callbacks may receive must be aligned, live and initialized. Previously captured
/// hooks must remain valid through their last collector epoch. Callbacks must not allocate GC
/// memory, enter the collector or safepoint. A finalizer must accept exclusive access to each
/// dead initialized payload and release its owned resources exactly once.
unsafe fn register_drop_in(gc: &GcRuntime, type_id: u32, drop_fn: DropFn) {
    let mut cache = gc.minor_registry_cache.lock().unwrap();
    gc.drop_registry.lock().unwrap().insert(type_id, drop_fn);
    cache.changed();
    gc.minor_registry_generation.fetch_add(1, Ordering::Release);
}

pub(super) fn lookup_drop(type_id: u32) -> Option<DropFn> {
    lookup_drop_in(runtime(), type_id)
}

pub(super) fn lookup_drop_in(gc: &GcRuntime, type_id: u32) -> Option<DropFn> {
    gc.drop_registry.lock().unwrap().get(&type_id).copied()
}

/// Hooks must remain leaf operations: no GC access or safepoints. A partially
/// executed destructor cannot safely be retried or ignored; terminate on panic.
pub(super) unsafe fn run_drop_hook(
    gc: &GcRuntime,
    drop_fn: DropFn,
    payload: *mut u8,
    observed_type: crate::observation::Value<u32>,
) {
    crate::failure::ffi_boundary("GC native destructor", || {
        // SAFETY: caller owns a dead, still-allocated payload for this hook.
        unsafe { drop_fn(payload) };
        observed_type.observe(|type_id| {
            crate::observation::Native::dropped(|| &gc.native_observation, || Some(*type_id));
        });
    });
}

/// Current hook-registry generation. This changes only when existing
/// registrations are invalidated, never when another type is merely added.
pub(crate) fn registry_generation() -> u64 {
    runtime()
        .registry_generation
        .load(std::sync::atomic::Ordering::Acquire)
}
