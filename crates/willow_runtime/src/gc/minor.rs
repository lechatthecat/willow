//! Stop-the-world minor collection and nursery survivor promotion.

use std::alloc::{Layout, dealloc};
use std::collections::{HashMap, HashSet};

use super::{
    DropFn, GC_GENERATION_OLD, GC_GENERATION_YOUNG, GC_HEADER_SIZE, GcHeader, GcPayload, GcState,
    HeapObject, RegionKind, TraceFn, all_registered_stack_roots, allocate_old_region_object_locked,
    drop_registry, foreign_root_stack_owner_active, object_reference_slots, retire_tlabs_with_work,
    runtime, runtime_roots_snapshot, type_registry, verify_old_region_metadata,
    verify_remembered_set, willow_gc_safepoint, with_stw,
};

#[cfg(test)]
#[path = "minor_tests.rs"]
mod tests;

struct MinorCollector<'a> {
    work: crate::gc_telemetry::MarkWork,
    stop_work: &'a mut crate::gc_telemetry::stops::StopWorkV2,
    state: &'a mut GcState,
    young_objects: HashMap<usize, HeapObject>,
    forwarding: HashMap<usize, *mut u8>,
    worklist: Vec<HeapObject>,
    scanned: HashSet<usize>,
    trace_registry: HashMap<u32, TraceFn>,
    drop_registry: HashMap<u32, DropFn>,
}

impl<'a> MinorCollector<'a> {
    fn new(
        state: &'a mut GcState,
        trace_registry: HashMap<u32, TraceFn>,
        drop_registry: HashMap<u32, DropFn>,
        stop_work: &'a mut crate::gc_telemetry::stops::StopWorkV2,
    ) -> Self {
        let mut young_objects = HashMap::new();
        for chunk in &state.tlab_chunks {
            debug_assert!(
                chunk.owner_state.is_none(),
                "minor collection requires retired TLABs"
            );
            let mut offset = 0usize;
            while offset < chunk.used {
                stop_work.metadata_objects += 1;
                stop_work.metadata_bytes += GC_HEADER_SIZE as u64;
                // SAFETY: retired chunks contain a stable sequential header prefix.
                let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())
                    .expect("TLAB header address is non-null");
                let size = object.size();
                if size < GC_HEADER_SIZE || size > chunk.used - offset {
                    panic!(
                        "willow gc: corrupt nursery header at 0x{:x}: size={size}, remaining={}",
                        object.as_ptr() as usize,
                        chunk.used - offset
                    );
                }
                if object.allocated() && object.generation() == GC_GENERATION_YOUNG {
                    young_objects.insert(object.payload().as_ptr() as usize, object);
                }
                offset += size;
            }
        }
        Self {
            work: crate::gc_telemetry::MarkWork::default(),
            stop_work,
            state,
            young_objects,
            forwarding: HashMap::new(),
            worklist: Vec::new(),
            scanned: HashSet::new(),
            trace_registry,
            drop_registry,
        }
    }

    /// Current generated code can retain an SSA alias after registering a root
    /// slot. Until precise relocation-aware reloads exist, directly rooted
    /// young objects are promoted in place. Their children can still move and
    /// are updated through object/container slots below.
    fn pin_root(&mut self, payload: *mut u8) {
        if payload.is_null() {
            return;
        }
        let address = payload as usize;
        if let Some(&object) = self.young_objects.get(&address)
            && object.generation() == GC_GENERATION_YOUNG
        {
            object.set_generation(GC_GENERATION_OLD);
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

    fn evacuate(&mut self, payload: *mut u8) -> *mut u8 {
        if payload.is_null() {
            return payload;
        }
        let address = payload as usize;
        let Some(&source) = self.young_objects.get(&address) else {
            return payload;
        };
        // Direct roots were already promoted in place.
        if source.generation() != GC_GENERATION_YOUNG {
            return payload;
        }
        if let Some(&forwarded) = self.forwarding.get(&address) {
            return forwarded;
        }

        let metadata = source.trace_metadata();
        let size = source.size();
        let Some(target) = allocate_old_region_object_locked(
            self.state,
            metadata.layout_id,
            metadata.type_id,
            metadata.payload_size,
            metadata.gc_ref_mask,
            false,
        ) else {
            if self.state.memory_limit_bytes.is_some() {
                // A hard region budget must not require extra evacuation
                // storage. Retain this nursery object in place, using the same
                // promotion path as SSA-pinned roots, and keep tracing it.
                self.pin_root(payload);
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
        self.state.promoted_objects = self.state.promoted_objects.saturating_add(1);
        self.state.promoted_bytes = self.state.promoted_bytes.saturating_add(size as u64);
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
        for slot in slots {
            self.scan_slot(slot);
        }
    }

    fn run(
        mut self,
        roots: Vec<*mut u8>,
        remembered: HashSet<usize>,
    ) -> (usize, crate::gc_telemetry::MarkWork) {
        let started = std::time::Instant::now();
        self.work.root_scan_bytes =
            crate::gc_telemetry::MarkWork::roots(roots.len()).root_scan_bytes;
        self.stop_work.root_values += roots.len() as u64;
        // Pin every direct root before scanning any interior edge so a duplicate
        // stack/runtime root can never observe a moved stale SSA pointer.
        for root in roots {
            self.pin_root(root);
        }
        for owner in remembered {
            self.pin_root(owner as *mut u8);
        }
        while let Some(object) = self.worklist.pop() {
            self.scan_object(object);
        }

        self.work.mark_ns = crate::gc_telemetry::elapsed_ns(started);
        let mut reclaimed_bytes = 0usize;
        for (&payload, &object) in &self.young_objects {
            self.stop_work.swept_objects += 1;
            if !object.allocated() || object.generation() != GC_GENERATION_YOUNG {
                continue;
            }
            let size = object.size();
            if !self.forwarding.contains_key(&payload) {
                if let Some(drop_fn) = self.drop_registry.get(&object.type_id()).copied() {
                    // SAFETY: unreachable young objects still own their runtime payload.
                    unsafe { super::run_drop_hook(drop_fn, object.payload().as_ptr()) };
                }
                self.state.total_frees = self.state.total_frees.saturating_add(1);
            }
            object.reclaim_in_place();
            self.state.allocated_bytes = self.state.allocated_bytes.saturating_sub(size);
            self.state.young_allocated_bytes =
                self.state.young_allocated_bytes.saturating_sub(size);
            reclaimed_bytes = reclaimed_bytes.saturating_add(size);
        }

        let mut chunk_identities: Vec<_> = (0..self.state.tlab_chunks.len()).collect();
        let mut chunk_positions = vec![None; self.state.tlab_chunks.len()];
        let mut chunk_index = 0usize;
        while chunk_index < self.state.tlab_chunks.len() {
            let base = self.state.tlab_chunks[chunk_index].base;
            let used = self.state.tlab_chunks[chunk_index].used;
            let mut offset = 0usize;
            let mut has_allocated = false;
            let mut live_bytes = 0usize;
            self.state.tlab_chunks[chunk_index].mark_bitmap.clear();
            while offset < used {
                self.stop_work.metadata_objects += 1;
                self.stop_work.metadata_bytes += GC_HEADER_SIZE as u64;
                // SAFETY: minor collection has already validated this retired prefix.
                let object = HeapObject::from_raw(unsafe { base.add(offset) }.cast())
                    .expect("TLAB header address is non-null");
                if object.allocated() {
                    debug_assert_eq!(
                        object.generation(),
                        GC_GENERATION_OLD,
                        "every minor survivor is promoted"
                    );
                    has_allocated = true;
                    live_bytes = live_bytes.saturating_add(object.size());
                    self.state.tlab_chunks[chunk_index].mark_bitmap.mark(offset);
                }
                offset += object.size();
            }
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
                self.state.tlab_chunks[chunk_index].kind = RegionKind::Pinned;
                self.state.tlab_chunks[chunk_index].live_bytes = live_bytes;
                chunk_index += 1;
            }
        }
        for (index, &identity) in chunk_identities.iter().enumerate() {
            chunk_positions[identity] = Some(index);
        }
        self.state.tlab_addresses.remap(&chunk_positions);
        (reclaimed_bytes, self.work)
    }
}

fn minor_collect_with_roots(
    mut roots: Vec<*mut u8>,
    stop_work: &mut crate::gc_telemetry::stops::StopWorkV2,
) -> (u64, u64, crate::gc_telemetry::MarkWork) {
    roots.extend(runtime_roots_snapshot());
    let trace_registry = type_registry().lock().unwrap().clone();
    let drop_registry = drop_registry().lock().unwrap().clone();
    let mut state = runtime().heap.lock().unwrap();
    if std::env::var("WILLOW_GC_VERIFY_BARRIER").is_ok()
        && let Err(message) = verify_remembered_set(&state, &trace_registry)
    {
        panic!("willow gc: write barrier verification failed: {message}");
    }
    let remembered = std::mem::take(&mut state.remembered_set);
    state.dirty_cards.clear();
    state.minor_collections = state.minor_collections.saturating_add(1);
    let before = state.allocated_bytes as u64;
    let (_, work) = MinorCollector::new(&mut state, trace_registry, drop_registry, stop_work)
        .run(roots, remembered);
    if std::env::var("WILLOW_GC_VERIFY_REGIONS").is_ok()
        && let Err(message) = verify_old_region_metadata(&state)
    {
        panic!("willow gc: region verification failed after minor collection: {message}");
    }
    (before, state.allocated_bytes as u64, work)
}

pub(super) fn minor_collect_internal() {
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
    let (before, after, work) = with_stw(
        crate::gc_telemetry::stops::StopReason::Minor,
        |coord, stop_work| {
            {
                let mut state = runtime().heap.lock().unwrap();
                retire_tlabs_with_work(&mut state, stop_work);
            }
            let roots = all_registered_stack_roots(coord);
            minor_collect_with_roots(roots, stop_work)
        },
    );
    let event = cycle.finish(before, after, work);
    drop(_serialize);
    crate::gc_telemetry::emit_cycle(event);
}
