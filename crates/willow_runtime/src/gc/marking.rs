use super::*;

/// Visit only set reference bits, bounded by the immutable payload layout.
fn inline_references(
    value: usize,
    metadata: raw_heap::TraceMetadata,
    mut visit: impl FnMut(*mut u8),
) -> usize {
    let words = (metadata.payload_size / GC_STORAGE_WORD_BYTES).min(64);
    let mut bits =
        metadata.inline_ref_mask() & u64::MAX.checked_shr((64 - words) as u32).unwrap_or(0);
    let count = bits.count_ones() as usize;
    while bits != 0 {
        let word = bits.trailing_zeros() as usize;
        // SAFETY: the validated immutable layout bounds this live payload;
        // reference stores use matching atomic publication.
        visit(unsafe {
            load_gc_reference((value as *mut u8).add(word * GC_STORAGE_WORD_BYTES).cast())
        });
        bits &= bits - 1;
    }
    count
}

/// Trace the GC graph from `worklist` (the marked-set fixpoint via the TypeInfo
/// registry + gc_ref_mask interior pointers). Shared by the single-mutator and
/// stop-the-world collection paths.
/// An epoch owns views of persistent region metadata. References are read from
/// the live heap while mutators execute; each generated allocation publishes
/// its start bit after initialization. New old allocations publish black.
pub(super) struct ConcurrentCycle {
    pub(super) collector: ThreadId,
    pub(super) phase: Mutex<old_cycle::State>,
    /// Published old-region prefix; advanced only by the collector under the heap lock.
    pub(super) sweep_publication_cursor: AtomicUsize,
    pub(super) termination: mark_termination::Termination,
    pub(super) mark_epoch: crate::gc_mark_queue::MarkEpoch,
    pointer_checks: AtomicU64,
    duplicate_discoveries: AtomicU64,
    unindexed_discoveries: AtomicU64,
    pub(super) assist_epoch: u64,
    pub(super) assist_pacing: Option<assist::Pacing>,
    pub(super) objects: epoch_index::EpochIndex,
    pub(super) legacy_traces: IdSet<u32>,
    pub(super) traces: IdMap<u32, ConcurrentTraceFn>,
    pub(super) slices: IdMap<u32, ConcurrentTraceSliceFn>,
    pub(super) worker_failed: std::sync::atomic::AtomicBool,
    pub(super) stopped_completion_required: std::sync::atomic::AtomicBool,
    pub(super) recovering: std::sync::atomic::AtomicBool,
    pub(super) recovery_retry_generation: AtomicU64,
    recovery_native_retries: Mutex<IdMap<usize, (usize, usize)>>,
    pub(super) closing: std::sync::atomic::AtomicBool,
    pub(super) tracing_enabled: std::sync::atomic::AtomicBool,
    pub(super) active_drains: AtomicUsize,
    pub(super) deferred: Mutex<Vec<usize>>,
    pub(super) unindexed: Mutex<HashSet<usize>>,
    /// False between the root handshake and late-extent publication, when an
    /// unindexed value may still lie in an extent created after preparation.
    /// Written only under the `unindexed` lock.
    pub(super) late_published: std::sync::atomic::AtomicBool,
    /// Debug validation of a root outside the captured index, taken at the
    /// root stop; raised inside the cycle's recovery scope.
    #[cfg(debug_assertions)]
    pub(super) invalid_root: Mutex<Option<String>>,
    pub(super) queue: Arc<crate::gc_mark_queue::MarkWorkQueue>,
    pub(super) work: Mutex<crate::gc_telemetry::MarkWork>,
    /// Deletion buffers filled without the heap mutex while marking.
    pub(super) satb_shards: satb::SatbShards,
}

// These diagnostics are separate from the stable C snapshot layouts. Publish
// once when the last reader/owner releases the epoch, never per graph edge.
impl Drop for ConcurrentCycle {
    fn drop(&mut self) {
        crate::gc_telemetry::workers::record_discoveries(
            self.pointer_checks.load(Ordering::Relaxed),
            self.duplicate_discoveries.load(Ordering::Relaxed),
            self.unindexed_discoveries.load(Ordering::Relaxed),
        );
    }
}

// Publish accounting and legacy/unindexed exceptions once per bounded drain,
// including unwind. Ordinary object tracing takes no cycle-global mutex.
pub(super) struct MarkBatch<'a> {
    pub(super) cycle: &'a ConcurrentCycle,
    pub(super) work: crate::gc_telemetry::MarkWork,
    pub(super) deferred: Vec<usize>,
    pub(super) native_counts: crate::gc_telemetry::native_trace::Counts,
    pub(super) unindexed: HashSet<usize>,
    pub(super) cpu_start: Option<u64>,
    pub(super) inline_children: bool,
    pub(super) branch_children: Vec<crate::gc_mark_queue::ObjectRef>,
    pub(super) discovery_counts: [u64; 3],
    pub(super) region_hint: Option<usize>,
    pub(super) inline_types: [Option<(u32, bool)>; 4],
    pub(super) local_child: Option<(crate::gc_mark_queue::ObjectRef, raw_heap::TraceMetadata)>,
}
impl MarkBatch<'_> {
    fn generated_inline(&mut self, metadata: raw_heap::TraceMetadata) -> bool {
        let slot = metadata.type_id as usize % self.inline_types.len();
        if let Some((id, answer)) = self.inline_types[slot]
            && id == metadata.type_id
        {
            return answer;
        }
        let answer = self.cycle.generated_inline(metadata);
        self.inline_types[slot] = Some((metadata.type_id, answer));
        answer
    }

    fn publish_branches(&mut self) {
        if self.branch_children.is_empty() {
            return;
        }
        let objects = std::mem::take(&mut self.branch_children).into_boxed_slice();
        self.cycle
            .queue
            .inject(crate::gc_mark_queue::MarkWork::new(
                self.cycle.mark_epoch,
                crate::gc_mark_queue::MarkWorkItem::ObjectBatch(objects),
            ))
            .expect("claimed branches belong to active drain");
    }
}
impl Drop for MarkBatch<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Publish failure before the reader can disappear. The outer catch
            // may run after queue retirement makes outstanding reach zero.
            self.cycle.worker_failed.store(true, Ordering::Release);
        }
        self.publish_branches();
        if crate::gc_telemetry::m3::enabled() {
            crate::gc_telemetry::m3::graph(
                self.work,
                self.discovery_counts[1],
                self.cycle.queue.snapshot().outstanding,
            );
        }
        self.cycle
            .pointer_checks
            .fetch_add(self.discovery_counts[0], Ordering::Relaxed);
        self.cycle
            .duplicate_discoveries
            .fetch_add(self.discovery_counts[1], Ordering::Relaxed);
        self.cycle
            .unindexed_discoveries
            .fetch_add(self.discovery_counts[2], Ordering::Relaxed);
        // An idle drain (e.g. an assist finding an empty queue) contributes no
        // mark work, so it must not dilute the pacer's mark-rate sample.
        let idle = self.work.marked_bytes == 0
            && self.work.scanned_bytes == 0
            && self.work.descriptor_bytes == 0;
        if !idle {
            crate::gc_telemetry::observations::graph_work(self.work);
            let cpu = self.cpu_start.and_then(|start| {
                crate::gc_telemetry::workers::thread_cpu_ns()?.checked_sub(start)
            });
            assist::record_mark_cpu(cpu);
            let mut total = self.cycle.work.lock().unwrap();
            total.objects = total.objects.saturating_add(self.work.objects);
            total.frame_scans = total.frame_scans.saturating_add(self.work.frame_scans);
            total.marked_bytes = total.marked_bytes.saturating_add(self.work.marked_bytes);
            total.scanned_bytes = total.scanned_bytes.saturating_add(self.work.scanned_bytes);
            total.descriptor_bytes = total
                .descriptor_bytes
                .saturating_add(self.work.descriptor_bytes);
            if std::thread::current().id() == self.cycle.collector {
                total.collector_mark_cpu_ns =
                    total.collector_mark_cpu_ns.saturating_add(cpu.unwrap_or(0));
            }
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
            let mut unindexed = self.cycle.unindexed.lock().unwrap();
            let pending = std::mem::take(&mut self.unindexed);
            if self.cycle.late_published.load(Ordering::Relaxed) && !std::thread::panicking() {
                // Late publication already took the shared set: recheck these
                // with the complete index. No reader is held here. On unwind,
                // worker failure forces authoritative stopped tracing instead.
                drop(unindexed);
                for address in pending {
                    self.cycle.enqueue(address as *mut u8);
                }
            } else {
                unindexed.extend(pending);
            }
        }
        // A bounded drain owns termination until its claimed continuation is
        // visible again, including on unwind and before a minor safepoint.
        if let Some((child, _)) = self.local_child.take() {
            self.cycle
                .queue
                .inject(crate::gc_mark_queue::MarkWork::object(
                    self.cycle.mark_epoch,
                    child,
                ))
                .expect("local continuation belongs to active drain");
        }
        self.cycle.active_drains.fetch_sub(1, Ordering::Release);
    }
}

impl ConcurrentCycle {
    #[cfg(test)]
    pub(super) fn new(
        objects: impl IntoIterator<Item = (usize, raw_heap::TraceMetadata)>,
        legacy_traces: IdSet<u32>,
        traces: IdMap<u32, ConcurrentTraceFn>,
        roots: usize,
    ) -> Self {
        Self::with_index(
            epoch_index::EpochIndex::synthetic(objects),
            legacy_traces,
            traces,
            roots,
        )
    }

    #[cfg(test)]
    pub(super) fn with_index(
        objects: epoch_index::EpochIndex,
        legacy_traces: IdSet<u32>,
        traces: IdMap<u32, ConcurrentTraceFn>,
        roots: usize,
    ) -> Self {
        let cycle =
            Self::with_epoch_index(objects, legacy_traces, traces, roots, Self::reserve_epoch());
        cycle
            .phase
            .lock()
            .unwrap()
            .transition(old_cycle::Phase::ConcurrentMark);
        cycle
    }

    /// Reserve the monotonic identity and initial phase before requesting STW.
    pub(super) fn reserve_epoch() -> (u64, Mutex<old_cycle::State>) {
        static CYCLE_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let epoch = CYCLE_EPOCH
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |epoch| {
                epoch.checked_add(1)
            })
            .expect("cycle epoch exhausted");
        (epoch, Mutex::new(old_cycle::State::initial(epoch)))
    }

    pub(super) fn with_epoch_index(
        objects: epoch_index::EpochIndex,
        legacy_traces: IdSet<u32>,
        traces: IdMap<u32, ConcurrentTraceFn>,
        roots: usize,
        identity: (u64, Mutex<old_cycle::State>),
    ) -> Self {
        // One slot per allowed background worker, plus the collector. Assists
        // beyond these slots use the queue's existing slotless consumer path.
        let queue = crate::gc_mark_queue::MarkWorkQueue::new(mark_workers::configured_count() + 1);
        let (assist_epoch, phase) = identity;
        let mark_epoch = queue.begin_reserved_epoch(crate::gc_mark_queue::MarkEpoch(assist_epoch));
        Self {
            collector: std::thread::current().id(),
            phase,
            sweep_publication_cursor: AtomicUsize::new(0),
            termination: mark_termination::Termination::default(),
            mark_epoch,
            pointer_checks: AtomicU64::new(0),
            duplicate_discoveries: AtomicU64::new(0),
            unindexed_discoveries: AtomicU64::new(0),
            assist_epoch,
            assist_pacing: Some(assist::Pacing::new(1, 1)),
            objects,
            worker_failed: std::sync::atomic::AtomicBool::new(false),
            stopped_completion_required: std::sync::atomic::AtomicBool::new(false),
            recovering: std::sync::atomic::AtomicBool::new(false),
            recovery_retry_generation: AtomicU64::new(0),
            recovery_native_retries: Mutex::new(IdMap::default()),
            closing: std::sync::atomic::AtomicBool::new(false),
            tracing_enabled: std::sync::atomic::AtomicBool::new(true),
            active_drains: AtomicUsize::new(0),
            legacy_traces,
            traces,
            slices: IdMap::default(),
            deferred: Mutex::new(Vec::new()),
            unindexed: Mutex::new(HashSet::new()),
            late_published: std::sync::atomic::AtomicBool::new(true),
            #[cfg(debug_assertions)]
            invalid_root: Mutex::new(None),
            queue,
            work: Mutex::new(crate::gc_telemetry::MarkWork::roots(roots)),
            satb_shards: satb::SatbShards::default(),
        }
    }

    pub(super) fn is_marked(&self, address: usize) -> bool {
        self.objects.is_marked(address)
    }

    pub(super) fn claim_or_defer(&self, address: usize) -> bool {
        if address == 0 {
            return false;
        }
        self.pointer_checks.fetch_add(1, Ordering::Relaxed);
        let late_published = self.late_published.load(Ordering::Acquire);
        if self.objects.claim(address) {
            return true;
        }
        if !self.objects.contains(address) {
            // A captured active TLAB may not have published its starts yet.
            // Never discard SATB/roots just because retirement has not arrived.
            self.unindexed_discoveries.fetch_add(1, Ordering::Relaxed);
            // Extents allocated after capture are implicitly black through
            // sweep and minor relocation. Only a captured extent can need a
            // delayed start publication; do not retain every new allocation in
            // an additional address set. Before late publication, an extent
            // created after preparation is not indexed yet but holds unmarked
            // objects that roots and SATB values name: retain those until
            // `publish_late` rechecks them.
            if self.objects.reader().needs_start_publication(address) {
                self.unindexed.lock().unwrap().insert(address);
            } else if !late_published {
                let mut unindexed = self.unindexed.lock().unwrap();
                if !self.late_published.load(Ordering::Relaxed) {
                    unindexed.insert(address);
                } else {
                    // Published after this claim's lookup: decide again with
                    // the complete index.
                    drop(unindexed);
                    return self.claim_or_defer(address);
                }
            }
        } else {
            self.duplicate_discoveries.fetch_add(1, Ordering::Relaxed);
        }
        false
    }

    /// Publish late extents, then recheck every value retained meanwhile.
    pub(super) fn publish_late(&self, late: epoch_index::LatePreparation) {
        self.objects.publish_late(late);
        let pending = {
            let mut unindexed = self.unindexed.lock().unwrap();
            self.late_published.store(true, Ordering::Release);
            std::mem::take(&mut *unindexed)
        };
        for address in pending {
            self.enqueue(address as *mut u8);
        }
    }

    pub(super) fn enqueue(&self, value: *mut u8) {
        let _publication = self.termination.enter();
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
        let _publication = self.termination.enter();
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
        metadata: Option<raw_heap::TraceMetadata>,
        children: &mut Vec<*mut u8>,
        batch: &mut MarkBatch<'_>,
    ) {
        // Also discard a partial snapshot left by an unwinding native hook.
        children.clear();
        // SAFETY: queue publication validates/claims the start, and this
        // active drain prevents minor relocation or sweep reclamation.
        let metadata = metadata.unwrap_or_else(|| unsafe { self.objects.claimed_metadata(value) });
        if self.legacy_traces.contains(&metadata.type_id)
            && !self.slices.contains_key(&metadata.type_id)
        {
            // Never invoke an unrestricted callback in a concurrent epoch.
            // Completion rejects the unsupported live type before reclamation.
            batch.native_counts.deferred(metadata.type_id);
            batch.deferred.push(value);
            return;
        }
        batch.work.frame(
            metadata.layout_id,
            metadata.type_id,
            metadata.payload_size,
            metadata.gc_ref_mask,
        );
        let payload = value as *mut u8;
        let words = metadata.payload_size / GC_STORAGE_WORD_BYTES;
        let mut slots = inline_references(value, metadata, |child| children.push(child));
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
        {
            let index = self.objects.reader();
            for child in children.drain(..) {
                self.discover_child(child, batch, &index);
            }
        }
        batch
            .work
            .object(GC_HEADER_SIZE + metadata.payload_size, slots);
    }

    /// Generated inline layouts have no additional native/legacy trace work.
    pub(super) fn generated_inline(&self, metadata: raw_heap::TraceMetadata) -> bool {
        metadata.type_id != willow_abi::GC_BITMAP_TYPE_ID
            && !self.legacy_traces.contains(&metadata.type_id)
            && !self.traces.contains_key(&metadata.type_id)
            && !self.slices.contains_key(&metadata.type_id)
    }

    /// Credit an inline layout whose complete reference mask was already
    /// visited under STW. The caller must separately publish its old children.
    pub(super) fn account_scanned_inline(
        &self,
        metadata: raw_heap::TraceMetadata,
        work: &mut crate::gc_telemetry::MarkWork,
    ) -> bool {
        if !self.generated_inline(metadata) {
            return false;
        }
        let words = (metadata.payload_size / GC_STORAGE_WORD_BYTES).min(64);
        let mask = metadata.gc_ref_mask & u64::MAX.checked_shr((64 - words) as u32).unwrap_or(0);
        work.frame(
            metadata.layout_id,
            metadata.type_id,
            metadata.payload_size,
            metadata.gc_ref_mask,
        );
        work.object(
            GC_HEADER_SIZE + metadata.payload_size,
            mask.count_ones() as usize,
        );
        true
    }

    fn discover_child(
        &self,
        child: *mut u8,
        batch: &mut MarkBatch<'_>,
        index: &epoch_index::EpochReader<'_>,
    ) {
        if child.is_null() {
            return;
        }
        batch.discovery_counts[0] += 1;
        let metadata = match index.claim_metadata(child as usize, &mut batch.region_hint) {
            Some(Some(metadata)) => metadata,
            Some(None) => {
                batch.discovery_counts[1] += 1;
                return;
            }
            None => {
                batch.discovery_counts[2] += 1;
                // The held reader excludes late publication, so a published
                // flag here means `index` already contains late extents.
                // Otherwise a captured object may name a late-extent child
                // stored before the stop: retain it like roots and SATB.
                if index.needs_start_publication(child as usize)
                    || !self.late_published.load(Ordering::Acquire)
                {
                    batch.unindexed.insert(child as usize);
                }
                return;
            }
        };
        // A pointer-free generated leaf needs only its mark bit and byte
        // accounting. The active drain owns termination across this claim;
        // avoid allocating and synchronizing a queue item with no trace work.
        if metadata.gc_ref_mask == 0 && batch.generated_inline(metadata) {
            batch.work.frame(
                metadata.layout_id,
                metadata.type_id,
                metadata.payload_size,
                0,
            );
            batch.work.object(GC_HEADER_SIZE + metadata.payload_size, 0);
        } else {
            let child = crate::gc_mark_queue::ObjectRef::from_ptr(child).unwrap();
            // Keep one descendant in the active drain. Branches remain visible
            // to other workers; chains avoid a global enqueue/dequeue per edge.
            if batch.inline_children && batch.local_child.is_none() {
                batch.local_child = Some((child, metadata));
                return;
            }
            if batch.inline_children {
                batch.branch_children.push(child);
                if batch.branch_children.len() == 32 {
                    batch.publish_branches();
                }
                return;
            }
            self.queue
                .inject(crate::gc_mark_queue::MarkWork::object(
                    self.mark_epoch,
                    child,
                ))
                .expect("claimed child belongs to the active drain");
        }
    }

    pub(super) fn enqueue_trace_slice(&self, value: usize, word: usize) {
        let _publication = self.termination.enter();
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

    #[cfg(test)]
    pub(super) fn recovery_native_retry_count(&self, address: usize) -> usize {
        self.recovery_native_retries
            .lock()
            .unwrap()
            .get(&address)
            .map_or(0, |entry| entry.1)
    }

    /// Minor owns all readers and transports pending continuations before source
    /// reclamation. Move retry history with them: relocation is not progress.
    pub(super) fn remap_recovery_native_retries(&self, remap: impl Fn(usize) -> Option<usize>) {
        let mut retries = self.recovery_native_retries.lock().unwrap();
        *retries = std::mem::take(&mut *retries)
            .into_iter()
            .filter_map(|(address, history)| remap(address).map(|target| (target, history)))
            .collect();
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
        if self.recovering.load(Ordering::Acquire) {
            let mut retries = self.recovery_native_retries.lock().unwrap();
            if matches!(next, TraceSliceProgress::Retry) {
                let entry = retries.entry(value).or_insert((offset, 0));
                assert_eq!(entry.0, offset, "retry cursor changed without progress");
                entry.1 += 1;
                if entry.1 >= super::mark_closure::MAX_NATIVE_RECOVERY_RETRIES {
                    drop(retries);
                    crate::failure::fatal_invariant(
                        "native trace Retry exhausted bounded recovery allowance; graph not reclaimed",
                    );
                }
                self.recovery_retry_generation
                    .fetch_add(1, Ordering::Release);
            } else {
                retries.remove(&value);
            }
        }
        match next {
            TraceSliceProgress::Done => {}
            TraceSliceProgress::Continue(next) => {
                assert!(next > offset, "native trace did not advance its cursor");
                assert!(
                    next - offset <= MAX_SLOTS,
                    "native trace exceeded its cursor budget"
                );
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
        // SAFETY: native continuations originate in a validated queue object.
        let metadata = unsafe { self.objects.claimed_metadata(value) };
        let slots = self.snapshot_native_slice(value, metadata.type_id, offset, children);
        {
            let index = self.objects.reader();
            for child in children.drain(..) {
                self.discover_child(child, batch, &index);
            }
        }
        batch.work.object(0, slots);
    }

    pub(super) fn trace_bitmap_slice(&self, value: usize, start: usize, batch: &mut MarkBatch<'_>) {
        const DESCRIPTOR_WORDS_PER_SLICE: usize = 8;
        // SAFETY: bitmap continuations originate in a validated queue object.
        let metadata = unsafe { self.objects.claimed_metadata(value) };
        assert_eq!(metadata.type_id, willow_abi::GC_BITMAP_TYPE_ID);
        let descriptor = metadata.gc_ref_mask as *const u64;
        let words = metadata.payload_size / GC_STORAGE_WORD_BYTES;
        // SAFETY: allocation validates the immutable descriptor. Start and
        // end are engine-generated and bounded by both descriptor and payload.
        let count = (unsafe { *descriptor } as usize).min(words.div_ceil(64));
        let end = start.saturating_add(DESCRIPTOR_WORDS_PER_SLICE).min(count);
        let mut slots = 0;
        let index_reader = self.objects.reader();
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
                    self.discover_child(child, batch, &index_reader);
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
        self.drain_recovering(|| self.drain_until(limit, deadline))
    }

    /// Unwinding retires the queue consumer and publishes MarkBatch before
    /// requesting authoritative stopped tracing. Never continue partial scans.
    pub(super) fn drain_recovering<T: Default>(&self, drain: impl FnOnce() -> T) -> T {
        if self.worker_failed.load(Ordering::Acquire) {
            return T::default();
        }
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(drain)) {
            Ok(work) => work,
            Err(payload) => {
                let reason = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("marker panicked with a non-string payload");
                crate::gc_telemetry::failure_record::emit("marker_panic", reason);
                self.worker_failed.store(true, Ordering::Release);
                crate::gc_telemetry::workers::record_failure(
                    crate::gc_telemetry::workers::Failure::MarkerPanic,
                );
                T::default()
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
        let _owner = self.termination.enter();
        CHILDREN.with_borrow_mut(|children| {
            let mut worker = self.queue.register_assist();
            self.drain_worker_until(limit, children, &mut worker, deadline)
                .1
        })
    }

    pub(super) fn drain_background(&self, limit: usize, children: &mut Vec<*mut u8>) -> usize {
        let _owner = self.termination.enter();
        let Some(mut worker) = self.queue.register_worker() else {
            // Mutator assists may temporarily occupy every slot. The collector
            // has a slotless consumer and will drain work not taken here.
            return 0;
        };
        // Amortize registration/accounting over a larger claim while keeping
        // a bounded return to the worker safepoint. Minor requests are checked
        // between every object as before; native callbacks stay indivisible.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1);
        self.drain_worker_until_stride(limit, children, &mut worker, Some(deadline), 16)
            .0
    }

    pub(super) fn drain_worker(
        &self,
        limit: usize,
        children: &mut Vec<*mut u8>,
        worker: &mut crate::gc_mark_queue::MarkWorker,
    ) -> (usize, u64) {
        self.drain_worker_until(limit, children, worker, None)
    }

    fn assist_stop_requested(&self) -> bool {
        std::thread::current().id() != self.collector
            && (self.closing.load(Ordering::Acquire)
                || runtime().poll_requested.load(Ordering::Acquire))
    }

    pub(super) fn drain_worker_until(
        &self,
        limit: usize,
        children: &mut Vec<*mut u8>,
        worker: &mut crate::gc_mark_queue::MarkWorker,
        deadline: Option<std::time::Instant>,
    ) -> (usize, u64) {
        self.drain_worker_until_stride(limit, children, worker, deadline, 1)
    }

    fn drain_worker_until_stride(
        &self,
        limit: usize,
        children: &mut Vec<*mut u8>,
        worker: &mut crate::gc_mark_queue::MarkWorker,
        deadline: Option<std::time::Instant>,
        clock_stride: usize,
    ) -> (usize, u64) {
        let _owner = self.termination.enter();
        use crate::gc_mark_queue::MarkWorkItem;
        // Barriers may enqueue before activation completes, but scanning then
        // could miss a store whose insertion barrier ran before phase=1.
        if worker.epoch() != self.mark_epoch
            || self.queue.current_epoch() != self.mark_epoch
            || !self.tracing_enabled.load(Ordering::Acquire)
        {
            return (0, 0);
        }
        // Background claims amortize the clock over 16 bounded work items;
        // assists retain per-item deadline checks. Minor polling is unaffected.
        let clock_calls = std::cell::Cell::new(0);
        let expired = || {
            let calls = clock_calls.get();
            clock_calls.set(if calls + 1 == clock_stride {
                0
            } else {
                calls + 1
            });
            calls == 0 && deadline.is_some_and(|end| std::time::Instant::now() >= end)
        };
        self.active_drains.fetch_add(1, Ordering::AcqRel);
        let mut scanned = 0;
        let mut batch = MarkBatch {
            cycle: self,
            work: Default::default(),
            deferred: Vec::new(),
            native_counts: crate::gc_telemetry::native_trace::Counts::default(),
            unindexed: HashSet::new(),
            // Started at the first work item: an empty drain pays no clock read.
            cpu_start: None,
            inline_children: true,
            branch_children: Vec::new(),
            discovery_counts: [0; 3],
            region_hint: None,
            inline_types: [None; 4],
            local_child: None,
        };
        // Reuse the epoch reader only across generated inline continuations.
        // Drop before native/bitmap work, callbacks, or returning to a safepoint.
        // Declared after `batch`, so it drops before active_drains is released.
        let mut inline_reader: Option<epoch_index::EpochReader<'_>> = None;
        let retry_generation = self.recovery_retry_generation.load(Ordering::Acquire);
        while scanned < limit {
            // Recovery owns retry pacing. Mutator assists/background consumers
            // relinquish work so they cannot consume the allowance while the
            // collector is giving a contended owner time to run.
            if self.recovering.load(Ordering::Acquire)
                && std::thread::current().id() != self.collector
            {
                break;
            }
            // Always permit one work item, even after descheduling at entry.
            if scanned != 0 && (expired() || minor::requested() || self.assist_stop_requested()) {
                break;
            }
            let (work, metadata) = if let Some((child, metadata)) = batch.local_child.take() {
                (
                    crate::gc_mark_queue::MarkWork::object(self.mark_epoch, child),
                    Some(metadata),
                )
            } else if let Some(work) = worker.next_work() {
                (work, None)
            } else {
                break;
            };
            if scanned == 0 {
                batch.cpu_start = crate::gc_telemetry::workers::thread_cpu_ns();
            }
            match work.item {
                MarkWorkItem::Object(object) => {
                    // Queue ownership plus active_drains pins this claimed
                    // header until completion, just as for slice continuations.
                    let metadata = metadata.unwrap_or_else(|| unsafe {
                        match inline_reader.as_ref() {
                            Some(reader) => reader.claimed_metadata(object.addr()),
                            None => self.objects.claimed_metadata(object.addr()),
                        }
                    });
                    if batch.generated_inline(metadata) {
                        let reader = inline_reader.get_or_insert_with(|| self.objects.reader());
                        batch.work.frame(
                            metadata.layout_id,
                            metadata.type_id,
                            metadata.payload_size,
                            metadata.gc_ref_mask,
                        );
                        let slots = inline_references(object.addr(), metadata, |child| {
                            self.discover_child(child, &mut batch, reader);
                        });
                        batch
                            .work
                            .object(GC_HEADER_SIZE + metadata.payload_size, slots);
                    } else {
                        inline_reader = None;
                        self.trace(object.addr(), Some(metadata), children, &mut batch);
                    }
                    scanned += 1;
                }
                MarkWorkItem::ObjectBatch(objects) => {
                    // SATB queue items contain at most 32 objects. A drain can
                    // exceed its object budget by at most 31, never by B.
                    let mut objects = objects.into_vec().into_iter();
                    while let Some(object) = objects.next() {
                        // Batches contain already-claimed, validated objects.
                        let metadata = unsafe {
                            match inline_reader.as_ref() {
                                Some(reader) => reader.claimed_metadata(object.addr()),
                                None => self.objects.claimed_metadata(object.addr()),
                            }
                        };
                        if batch.generated_inline(metadata) {
                            let reader = inline_reader.get_or_insert_with(|| self.objects.reader());
                            batch.work.frame(
                                metadata.layout_id,
                                metadata.type_id,
                                metadata.payload_size,
                                metadata.gc_ref_mask,
                            );
                            let slots = inline_references(object.addr(), metadata, |child| {
                                self.discover_child(child, &mut batch, reader);
                            });
                            batch
                                .work
                                .object(GC_HEADER_SIZE + metadata.payload_size, slots);
                        } else {
                            inline_reader = None;
                            self.trace(object.addr(), Some(metadata), children, &mut batch);
                        }
                        scanned += 1;
                        if (expired() || minor::requested() || self.assist_stop_requested())
                            && objects.len() != 0
                        {
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
                    inline_reader = None;
                    // SAFETY: a continuation retains its validated allocation
                    // through the active drain, including after minor remapping.
                    let metadata = unsafe { self.objects.claimed_metadata(object.addr()) };
                    if metadata.type_id == willow_abi::GC_BITMAP_TYPE_ID {
                        self.trace_bitmap_slice(object.addr(), word, &mut batch);
                    } else {
                        self.trace_native_slice(object.addr(), word, children, &mut batch);
                    }
                    scanned += 1;
                }
                _ => unreachable!("major marker queues only object work"),
            }
            batch.publish_branches();
            if batch.local_child.is_none() {
                worker.complete_current();
            }
            if self.recovery_retry_generation.load(Ordering::Acquire) != retry_generation {
                // Preserve batching for successful graph work, but return to
                // resumed recovery pacing before consuming a Retry continuation.
                break;
            }
        }
        // Retain the original queue claim while following inline descendants.
        // Publish a bounded-drain tail before completing that claim.
        if let Some((child, _)) = batch.local_child.take() {
            self.queue
                .inject(crate::gc_mark_queue::MarkWork::object(
                    self.mark_epoch,
                    child,
                ))
                .expect("bounded continuation belongs to active drain");
        }
        worker.complete_current();
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
            chunk.owner_state.is_none() && !chunk.needs_index(),
            "epoch index requires retired, indexed TLABs"
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

/// Bumped under the heap lock whenever the concurrent cycle is installed or
/// cleared (`set_concurrent_cycle`), so a thread revalidates its cached cycle
/// with one atomic load instead of taking the heap lock.
pub(super) static ASSIST_CYCLE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The only writer of `GcState::concurrent_cycle`. A removed cycle's shard
/// buffers are sealed (any entries are dropped: every finishing path flushes
/// them first), so a thread still holding it can no longer publish into it.
pub(super) fn set_concurrent_cycle(state: &mut GcState, cycle: Option<Arc<ConcurrentCycle>>) {
    if let Some(previous) = state.concurrent_cycle.take() {
        previous.satb_shards.seal_without_drain();
    }
    state.concurrent_cycle = cycle;
    publish_collection_busy(state);
    ASSIST_CYCLE_GENERATION.fetch_add(1, Ordering::Release);
}

thread_local! {
    // Weak: a finished cycle's index is freed even if this thread never
    // allocates again.
    static ASSIST_CYCLE: std::cell::RefCell<Option<(u64, std::sync::Weak<ConcurrentCycle>)>> =
        const { std::cell::RefCell::new(None) };
}

/// The installed cycle and the generation it was read at; the heap lock is
/// taken only when the generation moved since this thread last looked.
pub(super) fn cached_cycle() -> Option<(u64, Arc<ConcurrentCycle>)> {
    let generation = ASSIST_CYCLE_GENERATION.load(Ordering::Acquire);
    // The cache holds a `Weak` (a destructor): during TLS teardown, where
    // runtime allocation and barriers still run, fall back to the locked
    // lookup instead of panicking across the C ABI.
    let cached = ASSIST_CYCLE
        .try_with(|cached| match &*cached.borrow() {
            Some((cached, cycle)) if *cached == generation => Some(cycle.upgrade()),
            _ => None,
        })
        .ok()
        .flatten();
    if let Some(cycle) = cached {
        return cycle.map(|cycle| (generation, cycle));
    }
    let (generation, cycle) = {
        let state = runtime().heap.lock().unwrap();
        (
            ASSIST_CYCLE_GENERATION.load(Ordering::Relaxed),
            state.concurrent_cycle.clone(),
        )
    };
    let weak = cycle
        .as_ref()
        .map_or_else(std::sync::Weak::new, Arc::downgrade);
    let _ = ASSIST_CYCLE.try_with(|cached| *cached.borrow_mut() = Some((generation, weak)));
    cycle.map(|cycle| (generation, cycle))
}

/// Bounded allocation assistance. Only call outside runtime container locks.
pub(super) fn assist_concurrent_mark(allocated: u64) {
    if GC_MARK_PHASE.load(Ordering::Acquire) != 1 {
        assist::reset();
        return;
    }
    if !safepoint::current_thread_is_registered() {
        assist::reset();
        return;
    }
    if let Some((generation, cycle)) = cached_cycle()
        && let Some(pacing) = &cycle.assist_pacing
    {
        if !cycle.closing.load(Ordering::Acquire) {
            let completed = completed_work(&cycle);
            let mut pending = !cycle.queue.snapshot().is_drained();
            let rate = pacing.charge(allocated, completed, pending);
            let mut due = assist::charge(cycle.assist_epoch, allocated, rate);
            // Repay the whole debt, not one batch per allocation: past the
            // runway the rate makes debt exceed any single batch, and
            // allocating on regardless would overshoot the planned goal.
            // A batch without progress (only retrying slices or work claimed
            // elsewhere) keeps its debt for the next allocation: a retrying
            // slice may wait on this very mutator.
            while due && pending {
                if assist_batch(&cycle) == 0 {
                    break;
                }
                due = assist::due(cycle.assist_epoch);
                pending = assist_open(&cycle) && !cycle.queue.snapshot().is_drained();
                if due && pending {
                    willow_gc_safepoint();
                }
            }
        } else {
            // Closing allocations still consume the runway.
            pacing.record(allocated);
        }
        // Past the runway, closing termination (assists closed) or queued work
        // means the mark is unfinished; allocating on would overshoot. Wait
        // like an allocation-budget waiter, helping while assists are open.
        // The collector never waits on its own cycle.
        if pacing.exhausted() && std::thread::current().id() != cycle.collector {
            let _stall = crate::gc_telemetry::observations::Span::new(
                crate::gc_telemetry::observations::Kind::AllocationStall,
            );
            // Phase 0 or a reinstalled cycle ends the wait: a later cycle's
            // phase 1 must not keep this thread parked on a finished one.
            // Only retrying work left (e.g. a lock or channel slice this thread
            // holds) cannot finish while it waits: without mark progress for
            // STALL_NO_PROGRESS the allocation proceeds instead of livelocking
            // or exhausting the collector's bounded recovery retries.
            let mut progress = (completed_work(&cycle), std::time::Instant::now());
            while GC_MARK_PHASE.load(Ordering::Acquire) != 0
                && ASSIST_CYCLE_GENERATION.load(Ordering::Acquire) == generation
            {
                let drained = cycle.queue.snapshot().is_drained();
                // A drained queue before `closing` means the collector is about
                // to terminate the mark: its current worker-pool run returns and
                // `finish` sets `closing`, which bounds this overshoot window.
                if !cycle.closing.load(Ordering::Acquire) && drained {
                    break;
                }
                if !drained && assist_open(&cycle) && !cycle.recovering.load(Ordering::Acquire) {
                    assist_batch(&cycle);
                }
                willow_gc_safepoint();
                std::thread::yield_now();
                let completed = completed_work(&cycle);
                let now = std::time::Instant::now();
                if completed != progress.0 {
                    progress = (completed, now);
                } else if now.duration_since(progress.1) >= STALL_NO_PROGRESS {
                    break;
                }
            }
        }
    }
    // All queue claims/readers and the assist CPU span are released before
    // entering a handshake. A stop must not wait for an owner parked here.
    willow_gc_safepoint();
}

/// Longer than the collector's 1 ms resumed recovery wait between native
/// retries, far below its bounded retry allowance.
const STALL_NO_PROGRESS: std::time::Duration = std::time::Duration::from_millis(2);

fn completed_work(cycle: &ConcurrentCycle) -> u64 {
    let work = cycle.work.lock().unwrap();
    work.marked_bytes
        .saturating_add(work.scanned_bytes)
        .saturating_add(work.descriptor_bytes)
}

fn assist_open(cycle: &ConcurrentCycle) -> bool {
    GC_MARK_PHASE.load(Ordering::Acquire) == 1 && !cycle.closing.load(Ordering::Acquire)
}

/// One bounded assist batch; empty queues never block or manufacture credit.
/// Legacy callbacks are indivisible and measured by native telemetry.
/// Returns the credited work.
fn assist_batch(cycle: &ConcurrentCycle) -> u64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_micros(250);
    let cpu = assist::CpuScope::new();
    let work = {
        let _assist = crate::gc_telemetry::measurement::Span::new(
            crate::gc_telemetry::measurement::Phase::Assist,
        );
        let work = cycle.drain_checked_until(8, Some(deadline));
        assist::credit(cycle.assist_epoch, work);
        work
    };
    let overhead = cpu.overhead();
    let mut total = cycle.work.lock().unwrap();
    if let Some(overhead) = overhead {
        if let Some(cpu) = total.cpu_ns.checked_add(overhead) {
            total.cpu_ns = cpu;
        } else {
            total.cpu_incomplete = true;
        }
        if std::thread::current().id() == cycle.collector {
            total.collector_mark_cpu_ns = total.collector_mark_cpu_ns.saturating_add(overhead);
        }
    } else {
        total.cpu_incomplete = true;
    }
    work
}

pub(super) fn mark_worklist(
    mut worklist: Vec<*mut u8>,
    completed: Option<(&epoch_index::EpochIndex, &HashSet<usize>)>,
) -> crate::gc_telemetry::MarkWork {
    let started = std::time::Instant::now();
    let mut work = crate::gc_telemetry::MarkWork::roots(worklist.len());
    let mut hooks = IdMap::default();
    let mut native_counts = crate::gc_telemetry::native_trace::Counts::default();
    let cpu_start = crate::gc_telemetry::workers::thread_cpu_ns();
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
        let hook = *hooks
            .entry(metadata.type_id)
            .or_insert_with(|| registry::lookup_trace_contract(metadata.type_id));
        if let Some((trace, mode)) = hook {
            let mut child_slots: Vec<*mut *mut u8> = Vec::new();
            // SAFETY: trace is the registered function for this type_id.
            native_counts.stopped(metadata.type_id, mode == TraceMode::StopOnly);
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
        work.frame(
            metadata.layout_id,
            metadata.type_id,
            metadata.payload_size,
            metadata.gc_ref_mask,
        );
        work.object(object.size(), scanned_slots);
    }
    work.mark_ns = crate::gc_telemetry::elapsed_ns(started);
    let cpu = cpu_start
        .zip(crate::gc_telemetry::workers::thread_cpu_ns())
        .and_then(|(a, b)| b.checked_sub(a));
    work.cpu_ns = cpu.unwrap_or(0);
    work.cpu_incomplete = cpu.is_none();
    crate::gc_telemetry::observations::graph_work(work);
    work
}
