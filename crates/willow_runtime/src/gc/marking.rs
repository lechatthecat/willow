use super::*;

/// Trace the GC graph from `worklist` (the marked-set fixpoint via the TypeInfo
/// registry + gc_ref_mask interior pointers). Shared by the single-mutator and
/// stop-the-world collection paths.
/// An epoch owns views of persistent region metadata. References are read from
/// the live heap while mutators execute; each generated allocation publishes
/// its start bit after initialization. New old allocations publish black.
pub(super) struct ConcurrentCycle {
    pub(super) collector: ThreadId,
    pub(super) assist_epoch: u64,
    pub(super) assist_rate: u64,
    pub(super) objects: epoch_index::EpochIndex,
    pub(super) legacy_traces: HashSet<u32>,
    pub(super) traces: HashMap<u32, ConcurrentTraceFn>,
    pub(super) slices: HashMap<u32, ConcurrentTraceSliceFn>,
    pub(super) worker_failed: std::sync::atomic::AtomicBool,
    pub(super) closing: std::sync::atomic::AtomicBool,
    pub(super) tracing_enabled: std::sync::atomic::AtomicBool,
    pub(super) active_drains: AtomicUsize,
    pub(super) deferred: Mutex<Vec<usize>>,
    pub(super) unindexed: Mutex<HashSet<usize>>,
    pub(super) queue: Arc<crate::gc_mark_queue::MarkWorkQueue>,
    pub(super) work: Mutex<crate::gc_telemetry::MarkWork>,
}

// Publish accounting and legacy/unindexed exceptions once per bounded drain,
// including unwind. Ordinary object tracing takes no cycle-global mutex.
pub(super) struct MarkBatch<'a> {
    pub(super) cycle: &'a ConcurrentCycle,
    pub(super) work: crate::gc_telemetry::MarkWork,
    pub(super) deferred: Vec<usize>,
    pub(super) unindexed: HashSet<usize>,
    pub(super) cpu_start: Option<u64>,
}
impl Drop for MarkBatch<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Publish failure before the reader can disappear. The outer catch
            // may run after queue retirement makes outstanding reach zero.
            self.cycle.worker_failed.store(true, Ordering::Release);
        }
        let cpu = self
            .cpu_start
            .zip(crate::gc_telemetry::workers::thread_cpu_ns())
            .and_then(|(start, end)| end.checked_sub(start));
        {
            let mut total = self.cycle.work.lock().unwrap();
            total.marked_bytes = total.marked_bytes.saturating_add(self.work.marked_bytes);
            total.scanned_bytes = total.scanned_bytes.saturating_add(self.work.scanned_bytes);
            total.descriptor_bytes = total
                .descriptor_bytes
                .saturating_add(self.work.descriptor_bytes);
            if let Some(cpu) = cpu.and_then(|cpu| total.cpu_ns.checked_add(cpu)) {
                total.cpu_ns = cpu;
            } else {
                total.cpu_incomplete = true;
            }
        }
        if !self.deferred.is_empty() {
            self.cycle
                .deferred
                .lock()
                .unwrap()
                .append(&mut self.deferred);
        }
        if !self.unindexed.is_empty() {
            self.cycle
                .unindexed
                .lock()
                .unwrap()
                .extend(self.unindexed.drain());
        }
        self.cycle.active_drains.fetch_sub(1, Ordering::Release);
    }
}

impl ConcurrentCycle {
    #[cfg(test)]
    pub(super) fn new(
        objects: impl IntoIterator<Item = (usize, raw_heap::TraceMetadata)>,
        legacy_traces: HashSet<u32>,
        traces: HashMap<u32, ConcurrentTraceFn>,
        roots: usize,
    ) -> Self {
        Self::with_index(
            epoch_index::EpochIndex::synthetic(objects),
            legacy_traces,
            traces,
            roots,
        )
    }

    pub(super) fn with_index(
        objects: epoch_index::EpochIndex,
        legacy_traces: HashSet<u32>,
        traces: HashMap<u32, ConcurrentTraceFn>,
        roots: usize,
    ) -> Self {
        // One slot per allowed background worker, plus the collector. Assists
        // beyond these slots use the queue's existing slotless consumer path.
        let queue = crate::gc_mark_queue::MarkWorkQueue::new(mark_workers::configured_count() + 1);
        queue.begin_epoch();
        static ASSIST_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let assist_epoch = ASSIST_EPOCH
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |epoch| {
                epoch.checked_add(1)
            })
            .expect("assist epoch exhausted");
        Self {
            collector: std::thread::current().id(),
            assist_epoch,
            assist_rate: assist::SCALE,
            objects,
            worker_failed: std::sync::atomic::AtomicBool::new(false),
            closing: std::sync::atomic::AtomicBool::new(false),
            tracing_enabled: std::sync::atomic::AtomicBool::new(true),
            active_drains: AtomicUsize::new(0),
            legacy_traces,
            traces,
            slices: HashMap::new(),
            deferred: Mutex::new(Vec::new()),
            unindexed: Mutex::new(HashSet::new()),
            queue,
            work: Mutex::new(crate::gc_telemetry::MarkWork::roots(roots)),
        }
    }

    pub(super) fn is_marked(&self, address: usize) -> bool {
        self.objects.is_marked(address)
    }

    pub(super) fn claim_or_defer(&self, address: usize) -> bool {
        if address == 0 {
            return false;
        }
        if self.objects.claim(address) {
            return true;
        }
        if !self.objects.contains(address) {
            // A captured active TLAB may not have published its starts yet.
            // Never discard SATB/roots just because retirement has not arrived.
            self.unindexed.lock().unwrap().insert(address);
        }
        false
    }

    pub(super) fn recheck_unindexed(&self) {
        let pending = std::mem::take(&mut *self.unindexed.lock().unwrap());
        for address in pending {
            self.enqueue(address as *mut u8);
        }
    }

    pub(super) fn enqueue(&self, value: *mut u8) {
        use crate::gc_mark_queue::{MarkWork, ObjectRef};
        if self.claim_or_defer(value as usize) {
            // Claim at publication, not at consumption: repeated deletions or
            // equivalent edges must not fill the queue with duplicate jobs.
            // Producers never safepoint between claiming and injecting. Remark
            // stops all producers before checking queue termination.
            let object = ObjectRef::from_ptr(value).expect("candidate is non-null");
            self.queue
                .inject(MarkWork::object(self.queue.current_epoch(), object))
                .expect("mark publication belongs to active cycle");
        }
    }

    pub(super) fn enqueue_satb_batch(&self, values: &[usize]) {
        use crate::gc_mark_queue::{MarkWork, MarkWorkItem, ObjectRef};
        // Bound one owned queue item even with the maximum configured SATB
        // buffer. Claim and publication cannot safepoint or lose epoch ownership.
        const MAX_BATCH: usize = 32;
        for chunk in values.chunks(MAX_BATCH) {
            let objects: Vec<_> = chunk
                .iter()
                .copied()
                .filter(|&address| self.claim_or_defer(address))
                .map(|address| {
                    ObjectRef::from_ptr(address as *mut u8).expect("candidate is non-null")
                })
                .collect();
            if !objects.is_empty() {
                self.queue
                    .inject(MarkWork::new(
                        self.queue.current_epoch(),
                        MarkWorkItem::ObjectBatch(objects.into_boxed_slice()),
                    ))
                    .expect("SATB publication belongs to active cycle");
            }
        }
    }

    pub(super) fn trace(
        &self,
        value: usize,
        children: &mut Vec<*mut u8>,
        batch: &mut MarkBatch<'_>,
    ) {
        // Also discard a partial snapshot left by an unwinding native hook.
        children.clear();
        let Some(metadata) = self.objects.metadata(value) else {
            return;
        };
        if self.legacy_traces.contains(&metadata.type_id)
            && !self.traces.contains_key(&metadata.type_id)
            && !self.slices.contains_key(&metadata.type_id)
        {
            // Legacy extension callbacks expose mutable slots with an STW-only
            // contract. Preserve that contract instead of racing their payloads.
            batch.deferred.push(value);
            return;
        }
        let payload = value as *mut u8;
        let words = metadata.payload_size / GC_STORAGE_WORD_BYTES;
        let mut slots = 0;
        let inline_mask = metadata.inline_ref_mask();
        for index in 0..words.min(64) {
            if inline_mask & (1u64 << index) != 0 {
                // SAFETY: immutable epoch metadata bounds the live allocation;
                // generated and native reference stores use atomic publication.
                children.push(unsafe {
                    load_gc_reference(payload.add(index * GC_STORAGE_WORD_BYTES).cast::<*mut u8>())
                });
                slots += 1;
            }
        }
        if metadata.type_id == willow_abi::GC_BITMAP_TYPE_ID {
            let descriptor = metadata.gc_ref_mask as *const u64;
            // SAFETY: bitmap descriptors are validated immutable static data.
            let count = unsafe { *descriptor } as usize;
            if count.min(words.div_ceil(64)) > 1 {
                self.enqueue_trace_slice(value, 1);
            }
        }
        if self.slices.contains_key(&metadata.type_id) {
            slots += self.snapshot_native_slice(value, metadata.type_id, 0, children);
        } else if let Some(trace) = self.traces.get(&metadata.type_id) {
            let before = children.len();
            // SAFETY: concurrent hooks copy values under their own locks or
            // atomics, and no object is reclaimed before final remark.
            unsafe {
                trace(payload, children);
            }
            slots += children.len() - before;
        }
        for child in children.drain(..) {
            if !child.is_null() && !self.objects.contains(child as usize) {
                batch.unindexed.insert(child as usize);
            }
            self.enqueue(child);
        }
        batch
            .work
            .object(GC_HEADER_SIZE + metadata.payload_size, slots);
    }

    pub(super) fn enqueue_trace_slice(&self, value: usize, word: usize) {
        use crate::gc_mark_queue::{MarkWork, MarkWorkItem, ObjectRef};
        self.queue
            .inject(MarkWork::new(
                self.queue.current_epoch(),
                MarkWorkItem::ObjectSlice {
                    object: ObjectRef::from_addr(value).unwrap(),
                    word,
                },
            ))
            .expect("bitmap continuation belongs to active cycle");
    }

    pub(super) fn snapshot_native_slice(
        &self,
        value: usize,
        type_id: u32,
        offset: usize,
        children: &mut Vec<*mut u8>,
    ) -> usize {
        const MAX_SLOTS: usize = 512;
        let trace = self.slices.get(&type_id).expect("captured slice callback");
        let before = children.len();
        // SAFETY: an epoch reader retains payload storage; this registered hook
        // snapshots values through atomics/container locks with a stable cursor.
        let next = unsafe { trace(value as *mut u8, offset, MAX_SLOTS, children) };
        let slots = children.len() - before;
        assert!(slots <= MAX_SLOTS, "native trace exceeded its slot budget");
        match next {
            TraceSliceProgress::Done => {}
            TraceSliceProgress::Continue(next) => {
                assert!(next > offset, "native trace did not advance its cursor");
                self.enqueue_trace_slice(value, next);
            }
            TraceSliceProgress::Retry => {
                assert_eq!(slots, 0, "retry must not publish a partial native slice");
                self.enqueue_trace_slice(value, offset);
                std::thread::yield_now();
            }
        }
        slots
    }

    pub(super) fn trace_native_slice(
        &self,
        value: usize,
        offset: usize,
        children: &mut Vec<*mut u8>,
        batch: &mut MarkBatch<'_>,
    ) {
        children.clear();
        let metadata = self
            .objects
            .metadata(value)
            .expect("native epoch retains its object");
        let slots = self.snapshot_native_slice(value, metadata.type_id, offset, children);
        for child in children.drain(..) {
            self.enqueue(child);
        }
        batch.work.object(0, slots);
    }

    pub(super) fn trace_bitmap_slice(&self, value: usize, start: usize, batch: &mut MarkBatch<'_>) {
        const DESCRIPTOR_WORDS_PER_SLICE: usize = 8;
        let metadata = self
            .objects
            .metadata(value)
            .expect("bitmap epoch retains its object");
        assert_eq!(metadata.type_id, willow_abi::GC_BITMAP_TYPE_ID);
        let descriptor = metadata.gc_ref_mask as *const u64;
        let words = metadata.payload_size / GC_STORAGE_WORD_BYTES;
        // SAFETY: allocation validates the immutable descriptor. Start and
        // end are engine-generated and bounded by both descriptor and payload.
        let count = (unsafe { *descriptor } as usize).min(words.div_ceil(64));
        let end = start.saturating_add(DESCRIPTOR_WORDS_PER_SLICE).min(count);
        let mut slots = 0;
        for word in start..end {
            let mut bits = unsafe { *descriptor.add(word + 1) };
            while bits != 0 {
                let index = word * 64 + bits.trailing_zeros() as usize;
                if index < words {
                    let child = unsafe {
                        load_gc_reference(
                            (value as *mut u8)
                                .add(index * GC_STORAGE_WORD_BYTES)
                                .cast::<*mut u8>(),
                        )
                    };
                    self.enqueue(child);
                    slots += 1;
                }
                bits &= bits - 1;
            }
        }
        batch.work.object(0, slots);
        batch.work.descriptor_bytes = batch
            .work
            .descriptor_bytes
            .saturating_add((end - start) as u64 * 8);
        if end < count {
            self.enqueue_trace_slice(value, end);
        }
    }

    #[cfg(test)]
    pub(super) fn drain_checked(&self, limit: usize) -> u64 {
        self.drain_checked_until(limit, None)
    }

    pub(super) fn drain_checked_until(
        &self,
        limit: usize,
        deadline: Option<std::time::Instant>,
    ) -> u64 {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.drain_until(limit, deadline)
        }));
        match result {
            Ok(work) => work,
            Err(payload) => {
                crate::gc_telemetry::workers::record_failure(
                    crate::gc_telemetry::workers::Failure::MarkerPanic,
                );
                self.worker_failed.store(true, Ordering::Release);
                discard_callback_panic(payload);
                0
            }
        }
    }

    pub(super) fn drain(&self, limit: usize) -> u64 {
        self.drain_until(limit, None)
    }

    pub(super) fn drain_until(&self, limit: usize, deadline: Option<std::time::Instant>) -> u64 {
        // Concurrent hooks may neither allocate GC memory nor reach a safepoint,
        // so marking cannot reenter this thread's scratch-buffer borrow. Keep
        // capacity across bounded assists; successful drains leave the buffer empty.
        std::thread_local! {
            static CHILDREN: std::cell::RefCell<Vec<*mut u8>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }
        CHILDREN.with_borrow_mut(|children| {
            let mut worker = self.queue.register_assist();
            self.drain_worker_until(limit, children, &mut worker, deadline)
                .1
        })
    }

    pub(super) fn drain_background(&self, limit: usize, children: &mut Vec<*mut u8>) -> usize {
        let Some(mut worker) = self.queue.register_worker() else {
            // Mutator assists may temporarily occupy every slot. The collector
            // has a slotless consumer and will drain work not taken here.
            return 0;
        };
        self.drain_worker(limit, children, &mut worker).0
    }

    pub(super) fn drain_worker(
        &self,
        limit: usize,
        children: &mut Vec<*mut u8>,
        worker: &mut crate::gc_mark_queue::MarkWorker,
    ) -> (usize, u64) {
        self.drain_worker_until(limit, children, worker, None)
    }

    pub(super) fn drain_worker_until(
        &self,
        limit: usize,
        children: &mut Vec<*mut u8>,
        worker: &mut crate::gc_mark_queue::MarkWorker,
        deadline: Option<std::time::Instant>,
    ) -> (usize, u64) {
        use crate::gc_mark_queue::MarkWorkItem;
        // Barriers may enqueue before activation completes, but scanning then
        // could miss a store whose insertion barrier ran before phase=1.
        if !self.tracing_enabled.load(Ordering::Acquire) {
            return (0, 0);
        }
        let expired = || deadline.is_some_and(|end| std::time::Instant::now() >= end);
        self.active_drains.fetch_add(1, Ordering::AcqRel);
        let mut scanned = 0;
        let mut batch = MarkBatch {
            cycle: self,
            work: Default::default(),
            deferred: Vec::new(),
            unindexed: HashSet::new(),
            cpu_start: crate::gc_telemetry::workers::thread_cpu_ns(),
        };
        while scanned < limit {
            // Always permit one work item, even after descheduling at entry.
            if scanned != 0 && expired() {
                break;
            }
            let Some(work) = worker.next_work() else {
                break;
            };
            match work.item {
                MarkWorkItem::Object(object) => {
                    self.trace(object.addr(), children, &mut batch);
                    scanned += 1;
                }
                MarkWorkItem::ObjectBatch(objects) => {
                    // SATB queue items contain at most 32 objects. A drain can
                    // exceed its object budget by at most 31, never by B.
                    let mut objects = objects.into_vec().into_iter();
                    while let Some(object) = objects.next() {
                        self.trace(object.addr(), children, &mut batch);
                        scanned += 1;
                        if expired() && objects.len() != 0 {
                            // Publish the unfinished tail before completing the
                            // original item; closure must never observe a gap.
                            self.queue
                                .inject(crate::gc_mark_queue::MarkWork::new(
                                    self.queue.current_epoch(),
                                    MarkWorkItem::ObjectBatch(
                                        objects.collect::<Vec<_>>().into_boxed_slice(),
                                    ),
                                ))
                                .expect("assist tail belongs to active epoch");
                            break;
                        }
                    }
                }
                MarkWorkItem::ObjectSlice { object, word } => {
                    let metadata = self
                        .objects
                        .metadata(object.addr())
                        .expect("live continuation");
                    if metadata.type_id == willow_abi::GC_BITMAP_TYPE_ID {
                        self.trace_bitmap_slice(object.addr(), word, &mut batch);
                    } else {
                        self.trace_native_slice(object.addr(), word, children, &mut batch);
                    }
                    scanned += 1;
                }
                _ => unreachable!("major marker queues only object work"),
            }
            worker.complete_current();
        }
        // Drop publishes remaining private work and publishes private work before a
        // mutator can reach its next safepoint.
        (
            scanned,
            batch
                .work
                .marked_bytes
                .saturating_add(batch.work.scanned_bytes)
                .saturating_add(batch.work.descriptor_bytes),
        )
    }
}

pub(super) static MARK_WORKERS: Mutex<Option<mark_workers::Pool>> = Mutex::new(None);

pub(crate) fn shutdown_mark_workers() {
    coordinator::shutdown();
    // Called after user main returns, or by isolated runtime reset. No active
    // cycle can outlive Pool::run(), which holds this mutex until readers leave.
    let pool = MARK_WORKERS.lock().unwrap().take();
    drop(pool);
}

/// Enumerate allocation headers only, never payload graph edges. All generated
/// TLABs must be retired and mutators stopped while this index is captured.
pub(super) fn epoch_objects(
    state: &GcState,
    work: &mut crate::gc_telemetry::stops::StopWorkV2,
) -> Vec<HeapObject> {
    let mut objects = Vec::new();
    for object in old_region_objects(state) {
        work.metadata_objects += 1;
        work.metadata_bytes += GC_HEADER_SIZE as u64;
        objects.push(object);
    }
    for chunk in &state.tlab_chunks {
        assert!(
            chunk.owner_state.is_none(),
            "epoch index requires retired TLABs"
        );
        for &offset in &chunk.header_offsets {
            // Retirement validates physical headers once; subsequent snapshots
            // use its persistent ordered index rather than reparsing boundaries.
            let object =
                HeapObject::from_raw(unsafe { chunk.base.add(usize::from(offset)) }.cast())
                    .unwrap();
            work.metadata_objects += 1;
            work.metadata_bytes += GC_HEADER_SIZE as u64;
            if object.allocated() {
                objects.push(object);
            }
        }
    }
    objects
}

/// Bounded allocation assistance. Only call outside runtime container locks.
pub(super) fn assist_concurrent_mark(allocated: u64) {
    if GC_MARK_PHASE.load(Ordering::Acquire) != 1 {
        assist::reset();
        return;
    }
    let cycle = runtime().heap.lock().unwrap().concurrent_cycle.clone();
    if let Some(cycle) = cycle
        && !cycle.closing.load(Ordering::Acquire)
        && cycle.assist_rate != 0
        && assist::charge(cycle.assist_epoch, allocated, cycle.assist_rate)
    {
        // One bounded object batch per slow allocation. An empty queue cannot
        // block allocation or manufacture credit; background marking proceeds.
        // A scheduling budget, not a hard callback-duration guarantee: legacy
        // native hooks can overrun it. Bitmap/array/map slices have bounded work.
        let deadline = std::time::Instant::now() + std::time::Duration::from_micros(250);
        let work = cycle.drain_checked_until(8, Some(deadline));
        assist::credit(cycle.assist_epoch, work);
    }
}

pub(super) fn mark_worklist(
    mut worklist: Vec<*mut u8>,
    completed: Option<(&epoch_index::EpochIndex, &HashSet<usize>)>,
) -> crate::gc_telemetry::MarkWork {
    let started = std::time::Instant::now();
    let mut work = crate::gc_telemetry::MarkWork::roots(worklist.len());
    while let Some(obj_ptr) = worklist.pop() {
        let header = checked_payload_to_header(obj_ptr, "GC root graph");
        let object = HeapObject::from_raw(header).expect("validated payload has a header");
        let Some(metadata) = object.begin_trace() else {
            continue; // already visited — handles cycles
        };
        // Concurrently completed objects need no second graph walk. Deferred
        // legacy hooks still trace even though their mark bit was claimed.
        if completed.is_some_and(|(index, deferred)| {
            let address = obj_ptr as usize;
            (!index.contains(address) || index.is_marked(address)) && !deferred.contains(&address)
        }) {
            continue;
        }
        let payload_words = metadata.payload_size / GC_STORAGE_WORD_BYTES;
        let mut scanned_slots = 0usize;
        let inline_mask = metadata.inline_ref_mask();
        for i in 0..payload_words.min(64) {
            if (inline_mask & (1u64 << i)) != 0 {
                scanned_slots += 1;
                if let Some(child) = object.payload_word(i) {
                    worklist.push(child.as_ptr());
                }
            }
        }
        let mut bitmap_slots = Vec::new();
        append_bitmap_slots(object, &mut bitmap_slots);
        for slot in bitmap_slots {
            scanned_slots += 1;
            // SAFETY: bitmap slots belong to this live object's payload.
            let child = unsafe { *slot };
            if !child.is_null() {
                worklist.push(child);
            }
        }
        let trace_fn = type_registry()
            .lock()
            .unwrap()
            .get(&metadata.type_id)
            .copied();
        if let Some(trace) = trace_fn {
            let mut child_slots: Vec<*mut *mut u8> = Vec::new();
            // SAFETY: trace is the registered function for this type_id.
            unsafe { trace(object.payload().as_ptr(), &mut child_slots) };
            for slot in child_slots.into_iter().filter(|slot| !slot.is_null()) {
                scanned_slots += 1;
                // SAFETY: registered trace callbacks expose live GC-reference
                // slots owned by this object or its runtime payload.
                let child = unsafe { *slot };
                if !child.is_null() {
                    worklist.push(child);
                }
            }
        }
        work.object(object.size(), scanned_slots);
    }
    work.mark_ns = crate::gc_telemetry::elapsed_ns(started);
    work
}
