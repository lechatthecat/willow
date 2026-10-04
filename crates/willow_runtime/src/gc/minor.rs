//! Stop-the-world minor collection, survivor copying, and tenuring.

const TENURE_THRESHOLD: u8 = 2;

use std::alloc::{Layout, dealloc};
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

use super::{
    DropFn, GC_GENERATION_OLD, GC_GENERATION_YOUNG, GC_HEADER_SIZE, GC_REGION_MARK_GRANULE,
    GcHeader, GcPayload, GcState, HeapObject, RegionKind, TraceFn,
    allocate_old_region_object_locked, append_parked_roots, drop_registry,
    foreign_root_stack_owner_active, foreign_root_stack_owner_active_locked, minor_stack_roots,
    object_reference_slots, retire_tlabs_with_work, runtime, runtime_root_slots,
    tlab_payload_generation, type_registry, verify_old_region_metadata, verify_remembered_set,
    willow_gc_safepoint, with_stw,
};

/// Folded-multiply hash for aligned heap addresses. SipHash's DoS resistance
/// is unnecessary for collector-private address keys and dominated minor
/// collection time (willow-8hq4.16).
#[derive(Default)]
struct AddressHasher(u64);
impl Hasher for AddressHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }
    fn write_u64(&mut self, value: u64) {
        let product = u128::from(self.0 ^ value) * 0x9e37_79b9_7f4a_7c15;
        self.0 = (product as u64) ^ ((product >> 64) as u64);
    }
    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }
    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}
type AddressMap<V> = HashMap<usize, V, BuildHasherDefault<AddressHasher>>;
type AddressSet = HashSet<usize, BuildHasherDefault<AddressHasher>>;
type DropMap = HashMap<u32, DropFn, BuildHasherDefault<AddressHasher>>;

#[cfg(test)]
#[path = "minor_tests.rs"]
mod tests;

struct MinorCollector<'a> {
    work: crate::gc_telemetry::MarkWork,
    stop_work: &'a mut crate::gc_telemetry::stops::StopWorkV2,
    state: &'a mut GcState,
    survivor_destination: Option<usize>,
    /// Chunks at indices below this existed before the cycle; later ones are
    /// survivor destinations allocated by this cycle.
    source_chunks: usize,
    forwarding: AddressMap<*mut u8>,
    worklist: Vec<HeapObject>,
    scanned: AddressSet,
    trace_registry: HashMap<u32, TraceFn>,
    /// Consulted once per swept young object, so rehashed from the global
    /// SipHash registry once per cycle (O(types)).
    drop_registry: DropMap,
}

impl<'a> MinorCollector<'a> {
    fn new(
        state: &'a mut GcState,
        trace_registry: HashMap<u32, TraceFn>,
        drop_registry: HashMap<u32, DropFn>,
        stop_work: &'a mut crate::gc_telemetry::stops::StopWorkV2,
    ) -> Self {
        debug_assert!(
            state
                .tlab_chunks
                .iter()
                .all(|chunk| chunk.owner_state.is_none()),
            "minor collection requires retired TLABs"
        );
        // Retirement validated every header of a newly retired chunk, and the
        // collector wrote every survivor header. Young membership is answered
        // by the chunk index plus start bitmap, so no per-object index is
        // built for the (mostly dead) nursery.
        let source_chunks = state.tlab_chunks.len();
        Self {
            work: crate::gc_telemetry::MarkWork::default(),
            stop_work,
            state,
            survivor_destination: None,
            source_chunks,
            forwarding: AddressMap::default(),
            worklist: Vec::new(),
            scanned: AddressSet::default(),
            trace_registry,
            drop_registry: drop_registry.into_iter().collect(),
        }
    }

    /// The young object whose payload starts at `address`, if it lies in a
    /// nursery or survivor chunk that existed when this cycle began. Chunks
    /// created by `allocate_survivor` are destinations, never sources.
    /// O(log chunks) for the ordered chunk index, O(1) for the start bit.
    fn young_source(&self, address: usize) -> Option<HeapObject> {
        let header = address.checked_sub(GC_HEADER_SIZE)?;
        let index = self.state.tlab_addresses.candidate(header)?;
        if index >= self.source_chunks {
            return None;
        }
        let chunk = &self.state.tlab_chunks[index];
        let offset = header - chunk.base as usize;
        if chunk.kind == RegionKind::Pinned
            || offset >= chunk.used
            || !offset.is_multiple_of(GC_REGION_MARK_GRANULE)
            || !chunk.mark_bitmap.is_marked(offset)
        {
            return None;
        }
        // SAFETY: a set start bit inside the retired prefix names a header.
        let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())?;
        (object.allocated() && object.generation() == GC_GENERATION_YOUNG).then_some(object)
    }

    /// Retain a young object in place by promoting it where it lies. Only a
    /// hard memory budget uses this for young objects: a root published from
    /// runtime code that cannot be deferred, or a nursery object left without
    /// evacuation space. Root slots are rewritten instead (`scan_root_slot`).
    /// Children of a retained object can still move; its reference slots are
    /// updated when it is scanned.
    fn pin_root(&mut self, payload: *mut u8, source: PinSource) {
        if payload.is_null() {
            return;
        }
        let address = payload as usize;
        if let Some(object) = self.young_source(address) {
            if verify_no_pin() {
                crate::failure::fatal_invariant(&format!(
                    "WILLOW_GC_VERIFY_NO_PIN: minor collection pinned a young object ({})",
                    source.describe()
                ));
            }
            object.set_generation(GC_GENERATION_OLD);
            self.state.survivor_stats.pinned_promotions += 1;
            let size = object.size();
            self.state.young_allocated_bytes =
                self.state.young_allocated_bytes.saturating_sub(size);
            self.state.promoted_objects = self.state.promoted_objects.saturating_add(1);
            self.state.promoted_bytes = self.state.promoted_bytes.saturating_add(size as u64);
            self.worklist.push(object);
            return;
        }
        if let Some(payload) = GcPayload::from_raw(payload) {
            let object = HeapObject::from_payload(payload);
            if object.allocated() && object.generation() == GC_GENERATION_OLD {
                self.worklist.push(object);
                return;
            }
        }
        #[cfg(debug_assertions)]
        panic!("willow gc: invalid root 0x{address:x} during minor collection");
    }

    /// A per-cycle bump cursor: destinations are never evacuation sources in
    /// this cycle, and no search over old survivor chunks is needed per copy.
    fn allocate_survivor(&mut self, source: HeapObject, age: u8) -> Option<HeapObject> {
        use super::{BumpChunk, GC_TLAB_CHUNK_SIZE, RegionMarkBitmap};
        let size = source.size();
        if size > GC_TLAB_CHUNK_SIZE {
            return None;
        }
        if self.survivor_destination.is_none_or(|index| {
            let chunk = &self.state.tlab_chunks[index];
            chunk.capacity - chunk.used < size
        }) {
            if !super::can_reserve(self.state, GC_TLAB_CHUNK_SIZE) {
                return None;
            }
            let layout =
                Layout::from_size_align(GC_TLAB_CHUNK_SIZE, std::mem::align_of::<GcHeader>())
                    .ok()?;
            // SAFETY: valid aligned layout, owned until the common chunk cleanup.
            let base = unsafe { super::allocate_region_storage(layout) };
            if base.is_null() {
                return None;
            }
            let index = self.state.tlab_chunks.len();
            self.state.tlab_addresses.insert(base as usize, index);
            self.state.tlab_chunks.push(BumpChunk {
                base,
                capacity: GC_TLAB_CHUNK_SIZE,
                used: 0,
                owner_state: None,
                kind: RegionKind::Survivor,
                live_bytes: 0,
                mark_bitmap: RegionMarkBitmap::new(GC_TLAB_CHUNK_SIZE),
                concurrent_marks: std::sync::Arc::new(
                    super::concurrent_bitmap::ConcurrentMarkBits::new(
                        GC_TLAB_CHUNK_SIZE / GC_REGION_MARK_GRANULE,
                    ),
                ),
                header_offsets: Vec::new(),
            });
            self.state.tlab_reserved_bytes += GC_TLAB_CHUNK_SIZE;
            self.state.survivor_stats.survivor_space_reserved += GC_TLAB_CHUNK_SIZE as u64;
            self.survivor_destination = Some(index);
        }
        let chunk = &mut self.state.tlab_chunks[self.survivor_destination.unwrap()];
        let metadata = source.trace_metadata();
        // SAFETY: this exclusive collector cursor owns sufficient aligned space.
        let target = HeapObject::initialize_at(
            unsafe { chunk.base.add(chunk.used) },
            size,
            metadata.type_id,
            metadata.layout_id,
            metadata.gc_ref_mask,
            GC_GENERATION_YOUNG,
        )?;
        unsafe {
            (*target.as_ptr()).age = age;
        }
        chunk
            .header_offsets
            .push(u16::try_from(chunk.used).expect("chunk offset fits u16"));
        chunk.mark_bitmap.mark(chunk.used);
        chunk.used += size;
        chunk.live_bytes += size;
        self.state.survivor_stats.survivor_space_live += size as u64;
        self.state.allocated_bytes += size;
        self.state.young_allocated_bytes += size;
        Some(target)
    }

    fn evacuate(&mut self, payload: *mut u8) -> *mut u8 {
        if payload.is_null() {
            return payload;
        }
        let address = payload as usize;
        // Value roots were already retained in place and are no longer young;
        // destinations of this cycle are never sources.
        let Some(source) = self.young_source(address) else {
            return payload;
        };
        if let Some(&forwarded) = self.forwarding.get(&address) {
            return forwarded;
        }

        let metadata = source.trace_metadata();
        let size = source.size();
        // SAFETY: the source is a validated young allocation under STW.
        let age = unsafe { (*source.as_ptr()).age }.saturating_add(1);
        let target = if age < TENURE_THRESHOLD {
            self.allocate_survivor(source, age)
        } else {
            allocate_old_region_object_locked(
                self.state,
                metadata.layout_id,
                metadata.type_id,
                metadata.payload_size,
                metadata.gc_ref_mask,
                false,
            )
        };
        let Some(target) = target else {
            if self.state.memory_limit_bytes.is_some() {
                // A hard region budget must not require extra evacuation
                // storage. Retain this nursery object in place, using the same
                // promotion path as value roots, and keep tracing it.
                self.pin_root(payload, PinSource::MemoryBudget);
                return payload;
            }
            std::process::abort();
        };
        // SAFETY: source and target are distinct allocations with identical
        // payload sizes. Header/list metadata remains owned by the collector.
        unsafe {
            std::ptr::copy_nonoverlapping(
                source.payload().as_ptr(),
                target.payload().as_ptr(),
                metadata.payload_size,
            );
        }
        if target.generation() == GC_GENERATION_OLD {
            self.state.promoted_objects = self.state.promoted_objects.saturating_add(1);
            self.state.promoted_bytes = self.state.promoted_bytes.saturating_add(size as u64);
            self.state.survivor_stats.tenured_objects += 1;
            self.state.survivor_stats.tenured_bytes += size as u64;
        } else {
            self.state.survivor_stats.survivor_copies += 1;
            self.state.survivor_stats.survivor_bytes += size as u64;
        }
        self.state.moved_objects = self.state.moved_objects.saturating_add(1);
        let forwarded = target.payload().as_ptr();
        self.forwarding.insert(address, forwarded);
        self.worklist.push(target);
        forwarded
    }

    fn scan_slot(&mut self, slot: *mut *mut u8) {
        if slot.is_null() {
            return;
        }
        // SAFETY: slots come from layout masks or registered mutable trace hooks.
        let old = unsafe { *slot };
        let new = self.evacuate(old);
        if new != old {
            // SAFETY: minor collection owns all heap mutation under STW.
            unsafe { *slot = new };
        }
    }

    /// Evacuate a root slot's young referent and rewrite the slot (willow-9tls.9).
    /// The slot is the holder's only copy across this collection, so the
    /// referent need not stay in place. A slot already rewritten earlier in
    /// this cycle (an aliased registration) names a destination and is left
    /// alone; old referents are traced as before.
    fn scan_root_slot(&mut self, slot: *mut *mut u8) {
        // SAFETY: published root slots stay valid while their owners are stopped.
        let value = unsafe { *slot };
        if value.is_null() {
            return;
        }
        if self.young_source(value as usize).is_some() {
            self.scan_slot(slot);
            return;
        }
        if !self.is_destination(value as usize) {
            self.pin_root(value, PinSource::OldOrForeign);
        }
    }

    /// Whether `address` names an object copied into survivor storage by this
    /// cycle. O(log chunks).
    fn is_destination(&self, address: usize) -> bool {
        let Some(header) = address.checked_sub(GC_HEADER_SIZE) else {
            return false;
        };
        self.state
            .tlab_addresses
            .candidate(header)
            .filter(|&index| index >= self.source_chunks)
            .is_some_and(|index| {
                let chunk = &self.state.tlab_chunks[index];
                header < chunk.base as usize + chunk.used
            })
    }

    fn scan_object(&mut self, object: HeapObject) {
        let address = object.payload().as_ptr() as usize;
        if !object.allocated() || !self.scanned.insert(address) {
            return;
        }
        let slots = object_reference_slots(object, &self.trace_registry);
        self.work.object(
            object.size(),
            slots.iter().filter(|slot| !slot.is_null()).count(),
        );
        let old_owner = object.generation() == GC_GENERATION_OLD;
        let mut has_young_child = false;
        for slot in slots {
            self.scan_slot(slot);
            if old_owner && !has_young_child && !slot.is_null() {
                // SAFETY: scan_slot has updated this validated reference slot.
                let child = unsafe { *slot };
                if !child.is_null()
                    && HeapObject::from_payload(GcPayload::from_raw(child).unwrap()).generation()
                        == GC_GENERATION_YOUNG
                {
                    has_young_child = true;
                }
            }
        }
        if has_young_child {
            super::barrier::remember_owner(self.state, address);
        }
    }

    fn run(
        mut self,
        roots: MinorRoots,
        remembered: HashSet<usize>,
    ) -> (usize, crate::gc_telemetry::MarkWork) {
        let started = std::time::Instant::now();
        let count = roots.slots.len() + roots.values.len();
        self.work.root_scan_bytes = crate::gc_telemetry::MarkWork::roots(count).root_scan_bytes;
        self.stop_work.root_values += count as u64;
        // Value roots cannot be rewritten, so retain them in place before any
        // slot evacuates: a slot aliasing one of them then observes an old
        // object and keeps its value.
        for (root, source) in roots.values {
            self.pin_root(root, source);
        }
        for owner in remembered {
            // The set was taken; `scan_object` re-remembers owners that keep
            // a young child after this collection.
            HeapObject::from_payload(GcPayload::from_raw(owner as *mut u8).unwrap())
                .set_remembered(false);
            self.pin_root(owner as *mut u8, PinSource::RememberedOwner);
        }
        for slot in roots.slots {
            self.scan_root_slot(slot);
        }
        while let Some(object) = self.worklist.pop() {
            self.scan_object(object);
        }

        self.work.mark_ns = crate::gc_telemetry::elapsed_ns(started);
        let mut reclaimed_bytes = 0usize;
        let mut reclaimed_young = 0u64;
        // Nursery runs are usually one type; skip the registry probe for them.
        let mut last_type: Option<(u32, Option<DropFn>)> = None;
        let poison_moved = cfg!(debug_assertions) || super::gc_stress_enabled("relocate");
        self.state.survivor_stats.survivor_space_reserved = 0;
        self.state.survivor_stats.survivor_space_live = 0;
        let mut chunk_identities: Vec<_> = (0..self.state.tlab_chunks.len()).collect();
        let mut chunk_positions = vec![None; self.state.tlab_chunks.len()];
        let mut chunk_index = 0usize;
        while chunk_index < self.state.tlab_chunks.len() {
            // No allocation in an already-pinned chunk changes during a minor.
            // Keep its bitmap/live bytes and its identity for address remapping.
            if self.state.tlab_chunks[chunk_index].kind == RegionKind::Pinned {
                chunk_index += 1;
                continue;
            }
            let base = self.state.tlab_chunks[chunk_index].base;
            let used = self.state.tlab_chunks[chunk_index].used;
            let source = chunk_identities[chunk_index] < self.source_chunks;
            let mut has_allocated = false;
            let mut has_young = false;
            let mut live_bytes = 0usize;
            self.state.tlab_chunks[chunk_index].mark_bitmap.clear();
            // Retirement and survivor copying index every header, so visit the
            // index instead of chasing `offset += size`; independent header
            // loads overlap instead of serializing on the previous header.
            let offsets = std::mem::take(&mut self.state.tlab_chunks[chunk_index].header_offsets);
            debug_assert!(
                offsets.last().is_none_or(|&last| {
                    // SAFETY: indexed offsets name headers in the retired prefix.
                    let object =
                        HeapObject::from_raw(unsafe { base.add(usize::from(last)) }.cast())
                            .unwrap();
                    usize::from(last) + object.size() == used
                }) && (used == 0) == offsets.is_empty(),
                "minor collection requires a complete TLAB header index"
            );
            for &offset in &offsets {
                let offset = usize::from(offset);
                self.stop_work.metadata_objects += 1;
                self.stop_work.metadata_bytes += GC_HEADER_SIZE as u64;
                // SAFETY: minor collection has already validated this retired prefix.
                let object = HeapObject::from_raw(unsafe { base.add(offset) }.cast())
                    .expect("TLAB header address is non-null");
                // Young objects left in a source chunk were either copied
                // (forwarded) or unreachable; reclaim both in this same walk.
                if source && object.allocated() && object.generation() == GC_GENERATION_YOUNG {
                    self.stop_work.swept_objects += 1;
                    let size = object.size();
                    let type_id = object.type_id();
                    let drop_fn = match last_type {
                        Some((cached, drop_fn)) if cached == type_id => drop_fn,
                        _ => {
                            let drop_fn = self.drop_registry.get(&type_id).copied();
                            last_type = Some((type_id, drop_fn));
                            drop_fn
                        }
                    };
                    let forwarded = self
                        .forwarding
                        .contains_key(&(object.payload().as_ptr() as usize));
                    if let Some(drop_fn) = drop_fn
                        && !forwarded
                    {
                        // SAFETY: unreachable young objects still own their runtime payload.
                        unsafe { super::run_drop_hook(drop_fn, object.payload().as_ptr()) };
                    }
                    if poison_moved && forwarded {
                        // A holder that kept the pre-move address instead of
                        // reloading its root slot now reads poison, not a
                        // plausible stale copy.
                        // SAFETY: the moved source payload is collector-owned.
                        unsafe {
                            std::ptr::write_bytes(
                                object.payload().as_ptr(),
                                MOVED_PAYLOAD_POISON,
                                size - GC_HEADER_SIZE,
                            )
                        };
                    }
                    object.reclaim_in_place();
                    reclaimed_young += 1;
                    self.state.allocated_bytes = self.state.allocated_bytes.saturating_sub(size);
                    self.state.young_allocated_bytes =
                        self.state.young_allocated_bytes.saturating_sub(size);
                    reclaimed_bytes = reclaimed_bytes.saturating_add(size);
                }
                if object.allocated() {
                    has_young |= object.generation() == GC_GENERATION_YOUNG;
                    debug_assert!(
                        object.generation() == GC_GENERATION_OLD
                            || self.state.tlab_chunks[chunk_index].kind == RegionKind::Survivor,
                        "young survivors must be in collector-owned destination storage"
                    );
                    has_allocated = true;
                    live_bytes = live_bytes.saturating_add(object.size());
                    self.state.tlab_chunks[chunk_index].mark_bitmap.mark(offset);
                }
            }
            self.state.tlab_chunks[chunk_index].header_offsets = offsets;
            if !has_allocated {
                let chunk = self.state.tlab_chunks.swap_remove(chunk_index);
                chunk_identities.swap_remove(chunk_index);
                let layout =
                    Layout::from_size_align(chunk.capacity, std::mem::align_of::<GcHeader>())
                        .expect("TLAB chunk layout remains valid");
                // SAFETY: every object in this retired chunk was reclaimed or moved.
                unsafe { dealloc(chunk.base, layout) };
                self.state.released_bytes = self
                    .state
                    .released_bytes
                    .saturating_add(chunk.capacity as u64);
                self.state.tlab_reserved_bytes = self
                    .state
                    .tlab_reserved_bytes
                    .saturating_sub(chunk.capacity);
            } else {
                if !has_young {
                    self.state.tlab_chunks[chunk_index].kind = RegionKind::Pinned;
                } else {
                    self.state.survivor_stats.survivor_space_reserved +=
                        self.state.tlab_chunks[chunk_index].capacity as u64;
                    self.state.survivor_stats.survivor_space_live += live_bytes as u64;
                }
                self.state.tlab_chunks[chunk_index].live_bytes = live_bytes;
                chunk_index += 1;
            }
        }
        // Every forwarded source is a distinct reclaimed young object; the
        // rest were unreachable and count as frees.
        self.state.total_frees = self
            .state
            .total_frees
            .saturating_add(reclaimed_young.saturating_sub(self.forwarding.len() as u64));
        for (index, &identity) in chunk_identities.iter().enumerate() {
            chunk_positions[identity] = Some(index);
        }
        self.state.tlab_addresses.remap(&chunk_positions);
        (reclaimed_bytes, self.work)
    }
}

/// Fill for the payload a minor collection moved away from, written by debug
/// runtimes and under `WILLOW_GC_STRESS=relocate`.
/// Pointer words become non-canonical addresses on every 64-bit target.
pub(super) const MOVED_PAYLOAD_POISON: u8 = 0xA5;

/// Roots of one minor collection. Slots are rewritten when their referent
/// moves; values come from owners parked inside runtime code, which cannot
/// observe a move. A young value defers the collection unless a hard memory
/// budget forces in-place retention.
#[derive(Default)]
pub(super) struct MinorRoots {
    pub(super) slots: Vec<*mut *mut u8>,
    pub(super) values: Vec<(*mut u8, PinSource)>,
}

/// Why a minor collection retained a young object in place. Reported by
/// `WILLOW_GC_VERIFY_NO_PIN`, which turns any young pin into a fatal error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PinSource {
    /// A root slot published outside a relocation-safe scope, where runtime
    /// Rust frames may hold raw copies of the referent. Reaches `pin_root`
    /// only under a hard memory budget or after the bounded deferral
    /// (`should_defer_minor`) runs out.
    RuntimeCodeStack,
    /// An address-registered runtime root (never young: registration rejects
    /// young objects).
    RuntimeAddressRoot,
    /// A remembered-set owner (always old in a consistent heap).
    RememberedOwner,
    /// A root slot naming an object that is neither a live young source nor a
    /// destination of this cycle.
    OldOrForeign,
    /// A hard memory budget left no evacuation space.
    MemoryBudget,
}

impl PinSource {
    fn describe(self) -> &'static str {
        match self {
            Self::RuntimeCodeStack => "root slot published from runtime code",
            Self::RuntimeAddressRoot => "address-registered runtime root",
            Self::RememberedOwner => "remembered-set owner",
            Self::OldOrForeign => "root slot outside the young source chunks",
            Self::MemoryBudget => "memory budget left no evacuation space",
        }
    }
}

/// Whether `WILLOW_GC_VERIFY_NO_PIN` is enabled: set to a non-empty value
/// other than `0`. Read once; consulted only when a young object is about to
/// be pinned.
fn verify_no_pin() -> bool {
    static VERIFY: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        verify_no_pin_enabled(std::env::var_os("WILLOW_GC_VERIFY_NO_PIN").as_deref())
    });
    *VERIFY
}

fn verify_no_pin_enabled(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}

/// Run one minor collection over `roots`, or return `None` to defer it.
///
/// Value roots come from threads parked inside runtime code, whose Rust frames
/// may keep raw copies of a young referent (willow-9tls.9). Such a park is
/// normally transient: the frame resumes after this stop and a later
/// allocation slow path retries, one TLAB chunk later (`minor_retry_bytes`).
/// Deferring instead of pinning keeps every nursery object movable. The
/// deferral is bounded: once the young bytes have grown by one nursery
/// threshold since the first consecutive deferral, or under a hard memory
/// budget that cannot grow the nursery, the collection runs and promotes only
/// those runtime-held referents in place.
fn minor_collect_with_roots(
    mut roots: MinorRoots,
    stop_work: &mut crate::gc_telemetry::stops::StopWorkV2,
) -> Option<(u64, u64, crate::gc_telemetry::MarkWork)> {
    append_parked_roots(&mut roots);
    roots.slots.extend(runtime_root_slots());
    // Address roots are never young (checked at registration), so retaining
    // them moves nothing; scanning them still finds young children stored by
    // runtime code, as before this ticket.
    roots.values.extend(
        runtime()
            .runtime_roots
            .snapshot()
            .into_iter()
            .map(|root| (root, PinSource::RuntimeAddressRoot)),
    );
    let trace_registry = type_registry().lock().unwrap().clone();
    let drop_registry = drop_registry().lock().unwrap().clone();
    let mut state = runtime().heap.lock().unwrap();
    if should_defer_minor(&mut state, &roots) {
        state.survivor_stats.deferred_minor_collections += 1;
        state.minor_retry_bytes = state
            .young_allocated_bytes
            .saturating_add(super::GC_TLAB_CHUNK_SIZE);
        return None;
    }
    state.minor_deferral_start_bytes = None;
    state.minor_retry_bytes = 0;
    if std::env::var("WILLOW_GC_VERIFY_BARRIER").is_ok()
        && let Err(message) = verify_remembered_set(&state, &trace_registry)
    {
        panic!("willow gc: write barrier verification failed: {message}");
    }
    let remembered = std::mem::take(&mut state.remembered_set);
    state.dirty_cards.clear();
    state.minor_collections = state.minor_collections.saturating_add(1);
    let before = state.allocated_bytes as u64;
    let young_before = state.young_allocated_bytes;
    let promoted_before = state.promoted_bytes;
    let copied_before = state.survivor_stats.survivor_bytes;
    let (_, work) = MinorCollector::new(&mut state, trace_registry, drop_registry, stop_work)
        .run(roots, remembered);
    state.nursery_threshold_bytes = state.nursery_policy.next(
        state.nursery_threshold_bytes,
        young_before,
        state.promoted_bytes.saturating_sub(promoted_before)
            + state
                .survivor_stats
                .survivor_bytes
                .saturating_sub(copied_before),
        state.memory_limit_bytes,
    );
    if std::env::var("WILLOW_GC_VERIFY_REGIONS").is_ok()
        && let Err(message) = verify_old_region_metadata(&state)
    {
        panic!("willow gc: region verification failed after minor collection: {message}");
    }
    Some((before, state.allocated_bytes as u64, work))
}

/// Whether a runtime frame parked outside a relocation-safe scope holds a
/// young value root and the collection may still wait for it to resume. The
/// wait is bounded by one nursery threshold of young growth (measured from the
/// first consecutive deferral), so a park that never ends costs at most one
/// extra nursery's worth of memory before the in-place fallback.
fn should_defer_minor(state: &mut super::GcState, roots: &MinorRoots) -> bool {
    if state.memory_limit_bytes.is_some()
        || force_pin_fallback()
        || !roots.values.iter().any(|&(value, source)| {
            source == PinSource::RuntimeCodeStack
                && tlab_payload_generation(state, value as usize) == Some(GC_GENERATION_YOUNG)
        })
    {
        return false;
    }
    let young = state.young_allocated_bytes;
    let start = *state.minor_deferral_start_bytes.get_or_insert(young);
    young.saturating_sub(start) < state.nursery_threshold_bytes
}

#[cfg(test)]
thread_local! {
    static FORCE_PIN_FALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn force_pin_fallback() -> bool {
    FORCE_PIN_FALLBACK.with(std::cell::Cell::get)
}

#[cfg(not(test))]
#[inline(always)]
fn force_pin_fallback() -> bool {
    false
}

#[cfg(test)]
pub(crate) fn deferred_minor_collections_for_test() -> u64 {
    runtime()
        .heap
        .lock()
        .unwrap()
        .survivor_stats
        .deferred_minor_collections
}

/// Run a minor collection that promotes young referents of runtime-held value
/// roots in place, as under a hard memory budget, instead of deferring. Unit
/// fixtures use it to drive the remaining pinned-region paths without a
/// budget (willow-9tls.9). The flag is thread-local so concurrently running
/// tests that expect deferral are unaffected.
#[cfg(test)]
pub(crate) fn minor_collect_pinning_for_test() {
    FORCE_PIN_FALLBACK.with(|force| force.set(true));
    minor_collect_internal();
    FORCE_PIN_FALLBACK.with(|force| force.set(false));
}

pub(crate) fn minor_collect_internal() {
    if runtime()
        .stop_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        willow_gc_safepoint();
        return;
    }
    let _serialize = match runtime().collect_lock.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            willow_gc_safepoint();
            return;
        }
    };
    if foreign_root_stack_owner_active() {
        runtime()
            .skipped_foreign_owner_collections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }

    let cycle = crate::gc_telemetry::Cycle::begin(crate::gc_telemetry::CycleKind::Minor);
    // Stop the world and scan every registered mutator, for the reason spelled
    // out over the major cycle's mark phase: a registration that races a
    // single-mutator scan leaves the newcomer's objects unmarked (willow-v6k0).
    let collected = with_stw(
        crate::gc_telemetry::stops::StopReason::Minor,
        |coord, stop_work| {
            // Recheck under the stop's coord hold: an owner may have left the
            // registry, stack still nonempty, after the unlocked check above.
            if foreign_root_stack_owner_active_locked(coord) {
                return None;
            }
            {
                let mut state = runtime().heap.lock().unwrap();
                retire_tlabs_with_work(&mut state, stop_work);
            }
            let roots = minor_stack_roots(coord);
            Some(minor_collect_with_roots(roots, stop_work))
        },
    );
    let Some(collected) = collected else {
        runtime()
            .skipped_foreign_owner_collections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    };
    let Some((before, after, work)) = collected else {
        return;
    };
    let event = cycle.finish(before, after, work);
    drop(_serialize);
    crate::gc_telemetry::emit_cycle(event);
}
