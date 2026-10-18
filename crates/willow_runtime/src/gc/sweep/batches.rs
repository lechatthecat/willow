//! One persistent cursor shared by the elected collector and allocation misses.
//! The cursor lock is never waited on by allocators. Each heap-lock visit makes
//! one liveness decision; hooks, index construction and storage release happen
//! after that lock has been dropped. The collector owns completion and shutdown.
use super::*;
use crate::gc_telemetry::workers;
use std::sync::TryLockError;
use std::time::Instant;

const BACKGROUND: Limits = Limits {
    objects: 256,
    bytes: 1024 * 1024,
    ns: 1_000_000,
};
const ALLOCATION: Limits = Limits {
    objects: 32,
    bytes: 64 * 1024,
    ns: 100_000,
};
// Ordinary steps scan one object or start-map word, so the batch deadline is
// sampled once per stride. Hooks, releases and index work force a sample.
const DEADLINE_STRIDE: u32 = 16;

#[derive(Clone, Copy)]
struct Limits {
    objects: u64,
    bytes: u64,
    ns: u64,
}

#[derive(Default, Debug, Clone)]
struct Report {
    objects: u64,
    bytes: u64,
    batches: u64,
    assists: u64,
    yields: u64,
    overshoots: u64,
    work_ns: u64,
    setup_ns: u64,
    sweep_wall_ns: u64,
    partial_batches: u64,
    heap_holds: u64,
    // Allocation-index descents; tests bound them by heap holds.
    #[cfg(test)]
    index_reads: u64,
    heap_hold_ns: u64,
    heap_wait_ns: u64,
    heap_max_hold_ns: u64,
    heap_max_wait_ns: u64,
    max_batch_ns: u64,
    assist_max_ns: u64,
    hook_ns: u64,
    release_ns: u64,
    assist_objects: u64,
    assist_bytes: u64,
    reclaimed_bytes: u64,
    metadata_items: u64,
    swept_objects: u64,
    assist_cpu_ns: u64,
    assist_cpu_incomplete: bool,
}

struct Cursor {
    plan: SweepPlan,
    old: usize,
    offset: usize,
    live_end: usize,
    region_reclaimed: bool,
    gaps: Vec<RegionFreeSpan>,
    building: Option<(usize, free_spans::Builder)>,
    started_region: bool,
    chunks: usize,
    chunk_started: bool,
    chunk_live: bool,
    chunk_old: bool,
    build_headers: bool,
    headers: Option<Vec<u16>>,
    dead_chunks: Vec<bool>,
    finalize_old: usize,
    finalize_chunks: usize,
    done: bool,
    // Heap wait/hold timings feed only logs, profiles and tests. Ordinary runs
    // skip their four clock reads per step; the batch deadline stays timed.
    timed_holds: bool,
    // Set by a step whose cost is not bounded by one object or map word.
    deadline_due: bool,
    // Allocation entries at `offset..`, read with one map descent per heap
    // hold, nearest last. Only this cursor mutates the quarantined region's
    // index, and only behind `offset`; the hold's end discards the snapshot.
    ahead: Vec<(usize, usize)>,
    report: Report,
}

static ACTIVE_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ACTIVE: Mutex<Option<Arc<Mutex<Cursor>>>> = Mutex::new(None);
#[cfg(test)]
static LAST: Mutex<Option<Report>> = Mutex::new(None);

fn set_active(job: Option<Arc<Mutex<Cursor>>>) {
    let present = job.is_some();
    *ACTIVE.lock().unwrap() = job;
    ACTIVE_FLAG.store(present, Ordering::Release);
}

pub(super) fn assist() {
    if !ACTIVE_FLAG.load(Ordering::Acquire) {
        return;
    }
    let job = match ACTIVE.try_lock() {
        Ok(active) => active.clone(),
        Err(_) => return,
    };
    if let Some(job) = job {
        match job.try_lock() {
            Ok(mut cursor) => {
                cursor.batch(ALLOCATION, true);
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Poisoned(_)) => panic!("sweep cursor poisoned"),
        }
    }
}

pub(super) fn execute(plan: SweepPlan) -> usize {
    let began = Instant::now();
    let job = Arc::new(Mutex::new(Cursor::new(plan)));
    job.lock().unwrap().report.setup_ns = began.elapsed().as_nanos() as u64;
    assert!(ACTIVE.lock().unwrap().is_none());
    set_active(Some(job.clone()));
    loop {
        let (done, completed) = {
            let mut cursor = job.lock().unwrap();
            let before = cursor.old;
            cursor.batch(BACKGROUND, false);
            (cursor.done, (cursor.old > before).then(|| cursor.old - 1))
        };
        if done {
            break;
        }
        // No heap/cursor guard survives a yield or test pause.
        if let Some(index) = completed {
            between_regions(index);
        } else {
            std::thread::yield_now();
        }
    }
    set_active(None);
    let mut cursor = job.lock().unwrap();
    cursor.report.sweep_wall_ns = began.elapsed().as_nanos() as u64;
    if let Some(marking) = &cursor.plan.marking {
        {
            let mut work = marking.work.lock().unwrap();
            work.sweep_assist_cpu_ns = cursor.report.assist_cpu_ns;
            work.sweep_cpu_incomplete = cursor.report.assist_cpu_incomplete;
        }
        marking
            .phase
            .lock()
            .unwrap()
            .transition(old_cycle::Phase::Idle);
    }
    let freed = cursor.report.reclaimed_bytes as usize;
    crate::frame_lifetime::record_sweep(
        crate::frame_lifetime::FrameEventKind::SweepCompleted,
        0,
        cursor.plan.epoch,
        freed as u64,
    );
    #[cfg(test)]
    {
        *LAST.lock().unwrap() = Some(cursor.report.clone());
    }
    if std::env::var("WILLOW_GC_LOG").is_ok() {
        eprintln!(
            "[gc] sweep background_limits=256/1048576/1000000 assist_limits=32/65536/100000 {:?}",
            cursor.report
        );
    }
    freed
}

fn elapsed_ns(start: Option<Instant>) -> u64 {
    start.map_or(0, |start| start.elapsed().as_nanos() as u64)
}

impl Cursor {
    fn new(plan: SweepPlan) -> Self {
        Self {
            finalize_old: plan.old,
            finalize_chunks: plan.chunks,
            dead_chunks: Vec::with_capacity(plan.chunks),
            plan,
            old: 0,
            offset: 0,
            live_end: 0,
            region_reclaimed: false,
            gaps: Vec::with_capacity(GC_OLD_REGION_SIZE / GC_HEADER_SIZE),
            building: None,
            started_region: false,
            chunks: 0,
            chunk_started: false,
            chunk_live: false,
            chunk_old: false,
            build_headers: false,
            headers: None,
            done: false,
            timed_holds: cfg!(test)
                || crate::gc_telemetry::profiling()
                || std::env::var_os("WILLOW_GC_LOG").is_some(),
            deadline_due: false,
            ahead: Vec::with_capacity(DEADLINE_STRIDE as usize),
            report: Report::default(),
        }
    }

    fn hold_clock(&self) -> Option<Instant> {
        self.timed_holds.then(Instant::now)
    }

    fn batch(&mut self, limits: Limits, assist: bool) {
        self.batch_with_clock(limits, assist, crate::gc_telemetry::timestamp_ns);
    }

    fn batch_with_clock(&mut self, limits: Limits, assist: bool, mut now: impl FnMut() -> u64) {
        if self.done {
            return;
        }
        assert!(limits.objects > 0 && limits.bytes > 0 && limits.ns > 0);
        let external_assist = assist
            && crate::gc_telemetry::profiling()
            && self
                .plan
                .marking
                .as_ref()
                .is_some_and(|marking| marking.collector != std::thread::current().id());
        let cpu_start = external_assist.then(workers::thread_cpu_ns).flatten();
        let began = now();
        let before_objects = self.report.objects;
        let before_bytes = self.report.bytes;
        let before_region = self.old;
        self.report.batches += 1;
        self.report.assists += u64::from(assist);
        self.deadline_due = false;
        let mut unsampled = 0;
        loop {
            if self.report.objects - before_objects >= limits.objects
                || self.report.bytes - before_bytes >= limits.bytes
            {
                break;
            }
            unsampled += 1;
            if unsampled == DEADLINE_STRIDE || std::mem::take(&mut self.deadline_due) {
                unsampled = 0;
                if now().saturating_sub(began) >= limits.ns {
                    break;
                }
            }
            if runtime().stop_requested.load(Ordering::Acquire) {
                break;
            }
            // Steps up to the next deadline sample may share one heap hold.
            let run = DEADLINE_STRIDE - unsampled;
            let (progress, steps) = self.step(assist, limits, before_objects, before_bytes, run);
            unsampled += steps - 1;
            if !progress {
                break;
            }
            if self.done || self.old != before_region {
                break;
            }
        }
        let elapsed = now().saturating_sub(began);
        self.report.work_ns = self.report.work_ns.saturating_add(elapsed);
        if elapsed > limits.ns || self.report.bytes - before_bytes > limits.bytes {
            self.report.overshoots += 1;
        }
        self.report.max_batch_ns = self.report.max_batch_ns.max(elapsed);
        if assist {
            self.report.assist_max_ns = self.report.assist_max_ns.max(elapsed);
            self.report.assist_objects += self.report.objects - before_objects;
            self.report.assist_bytes += self.report.bytes - before_bytes;
        }
        if external_assist {
            if let Some(cpu) =
                cpu_start.and_then(|start| workers::thread_cpu_ns()?.checked_sub(start))
            {
                self.report.assist_cpu_ns = self.report.assist_cpu_ns.saturating_add(cpu);
            } else {
                self.report.assist_cpu_incomplete = true;
            }
        }
        if !self.done {
            self.report.yields += 1;
            self.report.partial_batches += u64::from(self.offset != 0 || self.building.is_some());
        }
    }

    /// Runs one step, or up to `run` consecutive heap-locked Progress steps.
    /// Returns whether the batch may continue and how many steps ran.
    fn step(
        &mut self,
        assist: bool,
        limits: Limits,
        before_objects: u64,
        before_bytes: u64,
        run: u32,
    ) -> (bool, u32) {
        let remaining = limits.bytes - (self.report.bytes - before_bytes);
        if let Some((index, builder)) = &mut self.building {
            const METADATA_BYTES: u64 =
                (std::mem::size_of::<RegionFreeSpan>() + 3 * std::mem::size_of::<usize>()) as u64;
            if assist && remaining < METADATA_BYTES {
                return (false, 1);
            }
            self.report.objects += 1;
            self.report.metadata_items += 1;
            self.report.bytes += METADATA_BYTES;
            if !builder.step() {
                return (true, 1);
            }
            let index = *index;
            // Only a completed index is published; a rejected lock attempt
            // retains the completed builder for the next batch.
            let wait = self.hold_clock();
            let mut state = if assist {
                match runtime().heap.try_lock() {
                    Ok(state) => state,
                    Err(TryLockError::WouldBlock) => return (true, 1),
                    Err(TryLockError::Poisoned(_)) => panic!("GC heap poisoned"),
                }
            } else {
                runtime().heap.lock().unwrap()
            };
            let waited = elapsed_ns(wait);
            self.report.heap_wait_ns += waited;
            self.report.heap_max_wait_ns = self.report.heap_max_wait_ns.max(waited);
            let held = self.hold_clock();
            let (_, builder) = self.building.take().unwrap();
            let region = &mut state.old_regions[index];
            let (spans, source) = builder.finish();
            region.free_spans = spans;
            region.largest_free_span = region.free_spans.largest();
            region.used = self.live_end;
            region.sweep_pending = false;
            region.sweep_state.phase = region::SweepPhase::Swept;
            if self.region_reclaimed {
                region.reusable_after_epoch = region
                    .sweep_state
                    .epoch
                    .checked_add(1)
                    .expect("reuse epoch exhausted");
            }
            self.old += 1;
            self.offset = 0;
            self.live_end = 0;
            self.region_reclaimed = false;
            self.started_region = false;
            self.report.heap_holds += 1;
            let held_ns = elapsed_ns(held);
            self.report.heap_hold_ns += held_ns;
            self.report.heap_max_hold_ns = self.report.heap_max_hold_ns.max(held_ns);
            drop(state);
            let mut source = source;
            source.clear();
            self.gaps = source;
            return (true, 1);
        }
        let wait = self.hold_clock();
        let mut state = if assist {
            match runtime().heap.try_lock() {
                Ok(state) => state,
                Err(TryLockError::WouldBlock) => return (false, 1),
                Err(TryLockError::Poisoned(_)) => panic!("GC heap poisoned"),
            }
        } else {
            runtime().heap.lock().unwrap()
        };
        let waited_ns = elapsed_ns(wait);
        self.report.heap_wait_ns = self.report.heap_wait_ns.saturating_add(waited_ns);
        self.report.heap_max_wait_ns = self.report.heap_max_wait_ns.max(waited_ns);
        let held = self.hold_clock();
        let region = self.old;
        let mut steps = 0;
        self.ahead.clear();
        // Cheap Progress steps share the hold; every per-step batch limit is
        // rechecked, and a stride bounds the hold like the deadline sample.
        let action = loop {
            let remaining = limits.bytes - (self.report.bytes - before_bytes);
            let action = if self.old < self.plan.old {
                self.old_step(&mut state, assist, remaining, run - steps)
            } else if self.chunks < self.plan.chunks {
                self.chunk_step(&mut state, assist, remaining)
            } else if assist {
                Action::Yield // Finalization, hooks and release are collector-only.
            } else {
                self.finish_step(&mut state)
            };
            steps += 1;
            if !matches!(action, Action::Progress)
                || steps == run
                || self.done
                || self.old != region
                || self.report.objects - before_objects >= limits.objects
                || self.report.bytes - before_bytes >= limits.bytes
                || runtime().stop_requested.load(Ordering::Acquire)
            {
                break action;
            }
        };
        self.report.heap_holds += 1;
        let held_ns = elapsed_ns(held);
        self.report.heap_hold_ns = self.report.heap_hold_ns.saturating_add(held_ns);
        self.report.heap_max_hold_ns = self.report.heap_max_hold_ns.max(held_ns);
        drop(state);
        self.deadline_due = !matches!(action, Action::Yield | Action::Progress);
        let progress = match action {
            Action::Yield => false,
            Action::Progress => true,
            Action::Drop(hook, payload) => {
                // Only background batches admit hooks. The live index no longer
                // names the object, but its quarantined extent still owns it.
                let began = Instant::now();
                unsafe { run_drop_hook(hook, payload) };
                self.report.hook_ns += began.elapsed().as_nanos() as u64;
                crate::frame_lifetime::record_sweep(
                    crate::frame_lifetime::FrameEventKind::DropCompleted,
                    payload as usize,
                    self.plan.epoch,
                    0,
                );
                true
            }
            Action::Build(index, obsolete) => {
                drop(obsolete);
                self.building = Some((
                    index,
                    free_spans::Builder::new(std::mem::take(&mut self.gaps)),
                ));
                true
            }
            Action::ReserveHeaders(capacity) => {
                self.headers = Some(Vec::with_capacity(capacity));
                true
            }
            Action::ReleaseOld(region) => {
                let began = Instant::now();
                release_pause();
                drop(region);
                self.report.release_ns += began.elapsed().as_nanos() as u64;
                true
            }
            Action::ReleaseChunk(chunk) => {
                let began = Instant::now();
                release_pause();
                let layout =
                    Layout::from_size_align(chunk.capacity, std::mem::align_of::<GcHeader>())
                        .unwrap();
                unsafe { dealloc(chunk.base, layout) };
                self.report.release_ns += began.elapsed().as_nanos() as u64;
                true
            }
        };
        (progress, steps)
    }

    fn old_step(&mut self, state: &mut GcState, assist: bool, remaining: u64, want: u32) -> Action {
        let index = self.old;
        let region = &mut state.old_regions[index];
        if !self.started_region {
            // Normal remark retired all LABs before publishing quarantine. No
            // allocator may refill these extents while the cursor owns them.
            assert_eq!(region.active_labs, 0);
            assert!(region.sweep_quarantined);
            assert_eq!(
                region.sweep_state.epoch,
                self.plan
                    .marking
                    .as_ref()
                    .map_or(self.plan.epoch, |cycle| cycle.assist_epoch)
            );
            assert_eq!(region.sweep_state.phase, region::SweepPhase::Unswept);
            region.sweep_state.phase = region::SweepPhase::Sweeping;
            self.started_region = true;
        }
        if self.ahead.is_empty() {
            #[cfg(test)]
            {
                self.report.index_reads += 1;
            }
            let entries = region.allocations.range(self.offset..);
            self.ahead.extend(
                entries
                    .take(want as usize)
                    .map(|(&offset, &span)| (offset, span)),
            );
            self.ahead.reverse();
        }
        let Some(&(offset, span)) = self.ahead.last() else {
            if assist {
                return Action::Yield;
            }
            // Rebuild once, outside the heap lock. Regular region metadata has
            // a fixed region-size ceiling; dedicated large regions have one slot.
            self.report.objects += 1;
            return Action::Build(index, std::mem::take(&mut region.free_spans));
        };
        let object = HeapObject::from_raw(unsafe { region.base.add(offset) }.cast()).unwrap();
        let size = object.size();
        let live = object.marked()
            || self.plan.marking.as_ref().is_some_and(|marking| {
                marking
                    .objects
                    .retains_allocated(object.payload().as_ptr() as usize)
            });
        let hook = if live {
            None
        } else {
            match sweep_drop(object.type_id()) {
                Ok(hook) => hook,
                Err(()) => return Action::Yield,
            }
        };
        // Never make an allocating mutator execute a potentially blocking hook
        // or admit one indivisible object larger than its remaining byte budget.
        let gap_bytes = if live && offset > self.live_end {
            std::mem::size_of::<RegionFreeSpan>() as u64
        } else {
            0
        };
        let work_bytes = size as u64 + gap_bytes;
        if assist && (hook.is_some() || work_bytes > remaining) {
            return Action::Yield;
        }
        let reclaim_permit = if live {
            None
        } else {
            match object.try_reclaim_permit() {
                Some(permit) => Some(permit),
                None => return Action::Yield,
            }
        };
        self.report.objects += 1;
        self.report.bytes += work_bytes;
        self.report.swept_objects += 1;
        self.ahead.pop();
        self.offset = offset + span;
        if live {
            if offset > self.live_end {
                self.gaps.push(RegionFreeSpan {
                    offset: self.live_end,
                    size: offset - self.live_end,
                });
            }
            self.live_end = offset + span;
            object.clear_mark();
            Action::Progress
        } else {
            self.region_reclaimed = true;
            let payload = object.payload().as_ptr();
            if hook.is_some() {
                crate::frame_lifetime::pending_drop(payload as usize, self.plan.epoch);
            }
            note_dead_frame(object, self.plan.epoch);
            region.allocations.remove(&offset);
            region.mark_bitmap.unmark(offset);
            region.live_bytes = region.live_bytes.saturating_sub(size);
            reclaim(state, object, size, reclaim_permit.unwrap());
            self.report.reclaimed_bytes += size as u64;
            hook.map_or(Action::Progress, |hook| Action::Drop(hook, payload))
        }
    }

    fn chunk_step(&mut self, state: &mut GcState, assist: bool, remaining: u64) -> Action {
        let index = self.chunks;
        let chunk = &mut state.tlab_chunks[index];
        if chunk.owner_state.is_some() {
            self.dead_chunks.push(false);
            self.chunks += 1;
            self.report.objects += 1;
            return Action::Progress;
        }
        if !self.chunk_started {
            self.chunk_started = true;
            self.build_headers = chunk.needs_index();
            self.chunk_live = false;
            self.chunk_old = false;
            if chunk.kind == RegionKind::Survivor {
                state.survivor_stats.survivor_space_live -= chunk.live_bytes as u64;
            }
            chunk.live_bytes = 0;
        }
        if self.build_headers && self.headers.is_none() {
            if assist {
                return Action::Yield;
            }
            self.report.objects += 1;
            self.report.metadata_items += 1;
            return Action::ReserveHeaders(chunk.used / GC_HEADER_SIZE);
        }
        if self.offset == chunk.used {
            if self.build_headers {
                // Partial indexes stay private: readers keep using atomic start
                // bits until the complete physical-header index is published.
                chunk.header_offsets = self.headers.take().unwrap();
            }
            if chunk.kind == RegionKind::Survivor {
                if self.chunk_old {
                    state.survivor_stats.survivor_space_reserved -= chunk.capacity as u64;
                } else {
                    state.survivor_stats.survivor_space_live += chunk.live_bytes as u64;
                }
            }
            if self.chunk_old {
                chunk.kind = RegionKind::Pinned;
            }
            self.dead_chunks.push(!self.chunk_live);
            self.chunks += 1;
            self.offset = 0;
            self.chunk_started = false;
            self.report.objects += 1;
            return Action::Progress;
        }
        let scan_start = self.offset;
        // Once the physical-header index exists, old reclaimed headers need
        // no further visits. Scan at most one atomic start-map word per step.
        let bitmap_bytes = if self.build_headers { 0 } else { 8 };
        if !self.build_headers {
            if assist && remaining < bitmap_bytes {
                return Action::Yield;
            }
            let granule = self.offset / GC_REGION_MARK_GRANULE;
            self.report.metadata_items += 1;
            if let Some(next) = chunk.mark_bitmap.bits.first_in_word(granule) {
                self.offset = next * GC_REGION_MARK_GRANULE;
            } else {
                self.offset = ((granule / 64 + 1) * 64 * GC_REGION_MARK_GRANULE).min(chunk.used);
                self.report.objects += 1;
                self.report.bytes += bitmap_bytes;
                return Action::Progress;
            }
        }
        let offset = self.offset;
        let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast()).unwrap();
        let size = object.size();
        assert!(
            size >= GC_HEADER_SIZE
                && size.is_multiple_of(std::mem::align_of::<GcHeader>())
                && size <= chunk.used - offset,
            "corrupt retired TLAB header"
        );
        let live = object.allocated()
            && (object.marked()
                || self.plan.marking.as_ref().is_some_and(|marking| {
                    marking
                        .objects
                        .retains_allocated(object.payload().as_ptr() as usize)
                }));
        let hook = if !object.allocated() || live {
            None
        } else {
            match sweep_drop(object.type_id()) {
                Ok(hook) => hook,
                Err(()) => return Action::Yield,
            }
        };
        let work_bytes = size as u64
            + bitmap_bytes
            + if self.build_headers {
                std::mem::size_of::<u16>() as u64
            } else {
                0
            };
        if assist && (hook.is_some() || work_bytes > remaining) {
            return Action::Yield;
        }
        let reclaim_permit = if live || !object.allocated() {
            None
        } else {
            match object.try_reclaim_permit() {
                Some(permit) => Some(permit),
                None => {
                    self.offset = scan_start;
                    return Action::Yield;
                }
            }
        };
        if self.build_headers {
            self.headers
                .as_mut()
                .unwrap()
                .push(u16::try_from(offset).expect("TLAB offset"));
        }
        self.offset += size;
        self.report.objects += 1;
        self.report.bytes += work_bytes;
        self.report.swept_objects += u64::from(object.allocated());
        if live {
            object.clear_mark();
            self.chunk_live = true;
            self.chunk_old |= object.generation() == GC_GENERATION_OLD;
            chunk.live_bytes += size;
            Action::Progress
        } else if object.allocated() {
            let payload = object.payload().as_ptr();
            if hook.is_some() {
                crate::frame_lifetime::pending_drop(payload as usize, self.plan.epoch);
            }
            note_dead_frame(object, self.plan.epoch);
            chunk.mark_bitmap.unmark(offset);
            reclaim(state, object, size, reclaim_permit.unwrap());
            self.report.reclaimed_bytes += size as u64;
            hook.map_or(Action::Progress, |hook| Action::Drop(hook, payload))
        } else {
            Action::Progress
        }
    }

    fn finish_step(&mut self, state: &mut GcState) -> Action {
        self.report.objects += 1;
        // Descending original positions remain stable across swap_remove. A new
        // extent moved from the tail keeps its base-keyed buffer ownership.
        if self.finalize_old != 0 {
            self.finalize_old -= 1;
            let index = self.finalize_old;
            let region = &mut state.old_regions[index];
            assert_eq!(region.sweep_state.phase, region::SweepPhase::Swept);
            if region.allocations.is_empty() {
                region.sweep_state.phase = region::SweepPhase::Released;
                let region = state.old_regions.swap_remove(index);
                state.old_addresses.remove(region.start());
                if index < state.old_regions.len() {
                    let moved = &state.old_regions[index];
                    let old_index = state.old_regions.len();
                    state
                        .old_region_candidates
                        .remove(&(moved.available_span(), old_index));
                    state.old_addresses.move_index(moved.start(), index);
                    index_old_region(state, index);
                }
                state.old_reserved_bytes -= region.capacity;
                state.released_bytes += region.capacity as u64;
                state.old_regions_released += 1;
                return Action::ReleaseOld(region);
            }
            // Existing safe holes need not acquire another epoch of quarantine
            // when this sweep reclaimed no new object in the extent.
            if region.reusable_after_epoch <= region.sweep_state.epoch {
                region.sweep_quarantined = false;
                index_old_region(state, index);
            }
            return Action::Progress;
        }
        if self.finalize_chunks != 0 {
            self.finalize_chunks -= 1;
            let index = self.finalize_chunks;
            if self.dead_chunks[index] {
                let chunk = state.tlab_chunks.swap_remove(index);
                assert!(chunk.owner_state.is_none());
                if chunk.kind == RegionKind::Survivor {
                    state.survivor_stats.survivor_space_reserved -= chunk.capacity as u64;
                }
                state.tlab_addresses.remove(chunk.base as usize);
                if index < state.tlab_chunks.len() {
                    state
                        .tlab_addresses
                        .move_index(state.tlab_chunks[index].base as usize, index);
                }
                state.tlab_reserved_bytes -= chunk.capacity;
                state.released_bytes += chunk.capacity as u64;
                return Action::ReleaseChunk(chunk);
            }
            return Action::Progress;
        }
        state.sweeping = None;
        publish_collection_busy(state);
        self.done = true;
        Action::Progress
    }
}

/// Registry updates may allocate/resize. Never wait for them under the heap
/// mutex; a rejected lookup keeps this object's cursor and ownership intact.
fn sweep_drop(type_id: u32) -> Result<Option<DropFn>, ()> {
    match drop_registry().try_lock() {
        Ok(registry) => Ok(registry.get(&type_id).copied()),
        Err(TryLockError::WouldBlock) => Err(()),
        Err(TryLockError::Poisoned(_)) => panic!("drop registry poisoned"),
    }
}

fn reclaim(state: &mut GcState, object: HeapObject, size: usize, permit: raw_heap::ReclaimPermit) {
    let payload = object.payload().as_ptr() as usize;
    if state.remembered_set.remove(&payload) {
        let card = payload / GC_CARD_SIZE;
        if !state.remembered_set.contains_card(card) {
            state.dirty_cards.remove(&card);
        }
    }
    if object.generation() == GC_GENERATION_YOUNG {
        state.young_allocated_bytes = state.young_allocated_bytes.saturating_sub(size);
    }
    object.reclaim_with_permit(permit);
    state.allocated_bytes = state.allocated_bytes.saturating_sub(size);
    state.total_frees = state.total_frees.saturating_add(1);
}

fn release_pause() {
    #[cfg(test)]
    if let Some(hook) = *RELEASE_HOOK.lock().unwrap() {
        hook();
    }
}
#[cfg(test)]
static RELEASE_HOOK: Mutex<Option<fn()>> = Mutex::new(None);

enum Action {
    Yield,
    Progress,
    Drop(DropFn, *mut u8),
    Build(usize, FreeSpans),
    ReserveHeaders(usize),
    ReleaseOld(OldRegion),
    ReleaseChunk(BumpChunk),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    fn make_cursor() -> Cursor {
        let plan = prepare(&mut runtime().heap.lock().unwrap(), None);
        Cursor::new(plan)
    }

    fn finish(cursor: &mut Cursor) {
        for _ in 0..100_000 {
            if cursor.done {
                return;
            }
            cursor.batch(BACKGROUND, false);
        }
        panic!("sweep cursor did not finish");
    }

    #[test]
    fn bounded_sweep_one_object_batches_are_lossless_and_keep_live_start_bits() {
        let _guard = runtime_test_guard();
        for objects in [1usize, 2, 17, 513, 8193] {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
            let mut live = Vec::new();
            for index in 0..objects {
                let pointer = willow_alloc(8);
                if index % 2 == 0 {
                    HeapObject::from_payload(GcPayload::from_raw(pointer).unwrap()).begin_trace();
                    live.push(pointer as usize);
                }
            }
            let mut cursor = make_cursor();
            let limits = Limits {
                objects: 1,
                bytes: 64,
                ns: 100,
            };
            let mut decisions = 0;
            while cursor.old < cursor.plan.old {
                let before = cursor.report.objects;
                let before_offset = cursor.offset;
                cursor.batch_with_clock(limits, true, || 0);
                let work = cursor.report.objects - before;
                assert!(work <= 1);
                assert_eq!(cursor.report.overshoots, 0);
                decisions += work;
                if cursor.offset == before_offset {
                    // Building the final free index is collector-only work.
                    cursor.batch_with_clock(limits, false, || 0);
                }
            }
            finish(&mut cursor);
            assert_eq!(
                willow_gc_allocated_bytes() as usize,
                live.len() * (GC_HEADER_SIZE + 8)
            );
            let state = runtime().heap.lock().unwrap();
            verify_old_region_metadata(&state).unwrap();
            for address in live {
                assert!(find_old_region_object(&state, address, false).is_some());
            }
            assert!(
                state
                    .old_regions
                    .iter()
                    .all(|r| r.sweep_state.phase == region::SweepPhase::Swept)
            );
            if objects > 1 {
                assert!(
                    state.old_region_candidates.is_empty(),
                    "reclaimed spans wait for next epoch"
                );
            }
            println!(
                "sweep_count objects={objects} assist_decisions={decisions} visits={} holds={} reclaimed={}",
                cursor.report.objects, cursor.report.heap_holds, cursor.report.reclaimed_bytes
            );
            assert!(cursor.report.objects <= 5 * objects as u64 + 3 * cursor.plan.old as u64 + 1);
            drop(state);
        }
        reset_internal_for_test();
    }

    #[test]
    fn bounded_sweep_fake_deadlines_preserve_the_unprocessed_suffix() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        for _ in 0..65 {
            willow_alloc(8);
        }
        let mut cursor = make_cursor();
        let mut tick = 0;
        cursor.batch_with_clock(
            Limits {
                objects: 64,
                bytes: 4096,
                ns: 3,
            },
            true,
            || {
                let now = tick;
                tick += 1;
                now
            },
        );
        // The start read is tick 0; strided samples read ticks 1, 2 and 3,
        // and the third stops before its own step.
        let swept = 3 * DEADLINE_STRIDE as usize - 1;
        assert_eq!(cursor.report.objects, swept as u64);
        assert_eq!(cursor.offset, swept * (GC_HEADER_SIZE + 8));
        assert_eq!(cursor.report.yields, 1);
        finish(&mut cursor);
        assert_eq!(
            cursor.report.reclaimed_bytes,
            65 * (GC_HEADER_SIZE + 8) as u64
        );
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_internal_for_test();
    }

    #[test]
    fn bounded_sweep_giant_objects_never_run_in_allocation_assistance() {
        let _guard = runtime_test_guard();
        for size in [ALLOCATION.bytes as usize + 8, 1 << 20, 1 << 24] {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
            willow_alloc(size as i64);
            let mut cursor = make_cursor();
            cursor.batch(ALLOCATION, true);
            assert_eq!(cursor.report.objects, 0);
            assert_eq!(cursor.report.bytes, 0);
            assert_eq!(cursor.offset, 0);
            finish(&mut cursor);
            assert_eq!(
                cursor.report.reclaimed_bytes as usize,
                size + GC_HEADER_SIZE
            );
            assert_eq!(willow_gc_allocated_bytes(), 0);
        }
        reset_internal_for_test();
    }

    static COUNTED_HOOKS: AtomicUsize = AtomicUsize::new(0);

    unsafe fn counted_hook(_: *mut u8) {
        COUNTED_HOOKS.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn bounded_sweep_hooks_force_deadline_samples_between_strides() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        COUNTED_HOOKS.store(0, Ordering::Relaxed);
        willow_register_drop(99003, counted_hook);
        let hooked = 4 * DEADLINE_STRIDE as usize;
        for _ in 0..hooked {
            willow_gc_alloc_layout(0, 99003, 8, 0);
        }
        let mut cursor = make_cursor();
        let mut clock = 0;
        let limits = Limits {
            objects: u64::MAX,
            bytes: u64::MAX,
            ns: 3,
        };
        cursor.batch_with_clock(limits, false, || {
            clock += 1;
            clock - 1
        });
        // Without forced samples the stride alone would admit at least
        // 3 * DEADLINE_STRIDE - 1 steps; each hook samples the next step.
        let hooks = COUNTED_HOOKS.load(Ordering::Relaxed);
        assert!((1..=3).contains(&hooks), "hooks={hooks}");
        assert!(cursor.report.objects < DEADLINE_STRIDE as u64);
        assert_eq!(cursor.report.yields, 1);
        finish(&mut cursor);
        assert_eq!(COUNTED_HOOKS.load(Ordering::Relaxed), hooked);
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_internal_for_test();
    }

    #[test]
    fn bounded_sweep_amortizes_heap_holds_and_index_reads_over_strides() {
        let _guard = runtime_test_guard();
        for objects in [513usize, 8193, 40_000] {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
            let mut live = Vec::new();
            for index in 0..objects {
                let pointer = willow_alloc(8);
                if index % 3 == 0 {
                    HeapObject::from_payload(GcPayload::from_raw(pointer).unwrap()).begin_trace();
                    live.push(pointer as usize);
                }
            }
            let mut cursor = make_cursor();
            let limits = Limits {
                objects: u64::MAX,
                bytes: u64::MAX,
                ns: u64::MAX,
            };
            while cursor.old < cursor.plan.old {
                cursor.batch_with_clock(limits, false, || 0);
            }
            let regions = cursor.plan.old as u64;
            let report = cursor.report.clone();
            println!(
                "sweep_holds objects={objects} regions={regions} holds={} index_reads={}",
                report.heap_holds, report.index_reads
            );
            // One hold per stride plus each region's start, end and rebuild.
            let strides = (objects as u64).div_ceil(DEADLINE_STRIDE as u64);
            assert!(report.heap_holds <= strides + 3 * regions, "{report:?}");
            assert!(report.index_reads <= report.heap_holds, "{report:?}");
            finish(&mut cursor);
            assert_eq!(
                willow_gc_allocated_bytes() as usize,
                live.len() * (GC_HEADER_SIZE + 8)
            );
            let state = runtime().heap.lock().unwrap();
            verify_old_region_metadata(&state).unwrap();
            for address in live {
                assert!(find_old_region_object(&state, address, false).is_some());
            }
            drop(state);
        }
        reset_internal_for_test();
    }

    static HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);
    static HOOK_ENTERED: AtomicBool = AtomicBool::new(false);
    static ALLOCATED_DURING_HOOK: AtomicBool = AtomicBool::new(false);

    unsafe fn delayed_hook(_: *mut u8) {
        assert!(runtime().heap.try_lock().is_ok(), "hook holds heap mutex");
        HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
        HOOK_ENTERED.store(true, Ordering::Release);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !ALLOCATED_DURING_HOOK.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "allocator stalled behind hook");
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    #[test]
    fn bounded_sweep_delayed_hook_is_once_only_outside_heap_and_never_assisted() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        HOOK_CALLS.store(0, Ordering::Relaxed);
        HOOK_ENTERED.store(false, Ordering::Relaxed);
        ALLOCATED_DURING_HOOK.store(false, Ordering::Relaxed);
        willow_register_drop(99001, delayed_hook);
        willow_gc_alloc_layout(0, 99001, 8, 0);
        let cursor = Arc::new(Mutex::new(make_cursor()));
        set_active(Some(cursor.clone()));
        cursor.lock().unwrap().batch(ALLOCATION, true);
        assert_eq!(HOOK_CALLS.load(Ordering::Relaxed), 0);
        assert_eq!(cursor.lock().unwrap().report.objects, 0);
        let worker = std::thread::spawn(|| {
            let _mutator = MutatorRegistration::new();
            while !HOOK_ENTERED.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            let pointer = willow_alloc(8);
            unsafe {
                *pointer.cast::<u64>() = 77;
            }
            ALLOCATED_DURING_HOOK.store(true, Ordering::Release);
            pointer as usize
        });
        let mut guard = cursor.lock().unwrap();
        finish(&mut guard);
        assert_eq!(HOOK_CALLS.load(Ordering::Relaxed), 1);
        assert!(guard.report.overshoots > 0, "slow hook must be charged");
        drop(guard);
        set_active(None);
        let pointer = worker.join().unwrap();
        assert_eq!(unsafe { *(pointer as *const u64) }, 77);
        reset_internal_for_test();
    }

    unsafe fn panicking_hook(_: *mut u8) {
        eprintln!("SWEEP_PANIC_HOOK_ENTERED");
        assert!(
            runtime().heap.try_lock().is_ok(),
            "panic hook held heap mutex"
        );
        panic!("expected sweep hook panic");
    }

    #[test]
    fn bounded_sweep_panicking_hooks_preserve_fatal_policy() {
        use failure_policy_tests::{GC_FATAL_CASE, assert_fatal_child};
        if let Ok(shape) = std::env::var(GC_FATAL_CASE) {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
            willow_register_drop(99002, panicking_hook);
            let mut tls = tlab_state_for_test();
            for _ in 0..7 {
                match shape.as_str() {
                    "large" => {
                        willow_gc_alloc_layout(0, 99002, (GC_LARGE_OBJECT_THRESHOLD + 8) as i64, 0);
                    }
                    "tlab" => {
                        willow_gc_alloc_slow(&mut tls, 0, 99002, 8, 0);
                    }
                    _ => {
                        willow_gc_alloc_layout(0, 99002, 8, 0);
                    }
                }
            }
            retire_all_tlabs_unindexed(&mut runtime().heap.lock().unwrap());
            let mut cursor = make_cursor();
            finish(&mut cursor);
            eprintln!("SWEEP_PANIC_RETURNED");
            return;
        }
        for shape in ["old", "large", "tlab"] {
            let stderr = assert_fatal_child(
                "gc::sweep::batches::tests::bounded_sweep_panicking_hooks_preserve_fatal_policy",
                shape,
                "runtime fatal: Rust panic in GC native destructor",
            );
            assert_eq!(stderr.matches("SWEEP_PANIC_HOOK_ENTERED").count(), 1);
            assert!(!stderr.contains("SWEEP_PANIC_RETURNED"));
            assert!(stderr.contains("expected sweep hook panic"));
        }
    }

    #[test]
    fn bounded_sweep_try_lock_and_stop_requests_never_consume_work() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        willow_alloc(8);
        let job = Arc::new(Mutex::new(make_cursor()));
        set_active(Some(job.clone()));
        {
            let _guard = job.lock().unwrap();
            assist();
        }
        assert_eq!(job.lock().unwrap().report.objects, 0);
        {
            let _heap = runtime().heap.lock().unwrap();
            assist();
        }
        assert_eq!(job.lock().unwrap().report.objects, 0);
        {
            let _registry = drop_registry().lock().unwrap();
            assist();
        }
        assert_eq!(job.lock().unwrap().report.objects, 0);
        runtime().stop_requested.store(true, Ordering::Release);
        assist();
        assert_eq!(job.lock().unwrap().report.objects, 0);
        runtime().stop_requested.store(false, Ordering::Release);
        finish(&mut job.lock().unwrap());
        set_active(None);
        reset_internal_for_test();
    }

    #[test]
    fn bounded_sweep_epoch_quarantine_releases_only_after_next_activation() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let pointer = willow_alloc(8);
        let object = HeapObject::from_payload(GcPayload::from_raw(pointer).unwrap());
        object.begin_trace();
        willow_alloc(8);
        let mut cursor = make_cursor();
        let epoch = cursor.plan.epoch;
        finish(&mut cursor);
        release_quarantine(epoch);
        assert!(runtime().heap.lock().unwrap().old_regions[0].sweep_quarantined);
        release_quarantine(epoch + 1);
        assert!(!runtime().heap.lock().unwrap().old_regions[0].sweep_quarantined);
        assert_eq!(
            runtime().heap.lock().unwrap().old_region_candidates.len(),
            1
        );
        reset_internal_for_test();
    }

    fn delayed_release() {
        assert!(
            runtime().heap.try_lock().is_ok(),
            "release holds heap mutex"
        );
        HOOK_ENTERED.store(true, Ordering::Release);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !ALLOCATED_DURING_HOOK.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "allocator stalled behind release"
            );
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    #[test]
    fn bounded_sweep_delayed_release_keeps_allocators_running_and_is_charged() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        willow_alloc((GC_LARGE_OBJECT_THRESHOLD + 8) as i64);
        HOOK_ENTERED.store(false, Ordering::Relaxed);
        ALLOCATED_DURING_HOOK.store(false, Ordering::Relaxed);
        let job = Arc::new(Mutex::new(make_cursor()));
        set_active(Some(job.clone()));
        *RELEASE_HOOK.lock().unwrap() = Some(delayed_release);
        let worker = std::thread::spawn(|| {
            let _mutator = MutatorRegistration::new();
            while !HOOK_ENTERED.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            let pointer = willow_alloc(8);
            ALLOCATED_DURING_HOOK.store(true, Ordering::Release);
            pointer as usize
        });
        finish(&mut job.lock().unwrap());
        assert!(job.lock().unwrap().report.release_ns >= 2_000_000);
        assert!(job.lock().unwrap().report.overshoots > 0);
        *RELEASE_HOOK.lock().unwrap() = None;
        set_active(None);
        let pointer = worker.join().unwrap();
        assert!(find_old_region_object(&runtime().heap.lock().unwrap(), pointer, false).is_some());
        reset_internal_for_test();
    }

    #[test]
    fn bounded_sweep_next_epoch_keeps_previously_safe_holes_reusable() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let live = willow_alloc(8);
        let dead = willow_alloc(8);
        let survivor = HeapObject::from_payload(GcPayload::from_raw(live).unwrap());
        survivor.begin_trace();
        let mut first = make_cursor();
        finish(&mut first);
        assert!(
            runtime()
                .heap
                .lock()
                .unwrap()
                .old_region_candidates
                .is_empty()
        );
        release_quarantine(first.plan.epoch + 1);
        let reused = willow_alloc(8);
        // A dead trailing object becomes bump tail after coalescing.
        assert_eq!(dead, reused);
        survivor.begin_trace();
        HeapObject::from_payload(GcPayload::from_raw(reused).unwrap()).begin_trace();
        let mut second = make_cursor();
        finish(&mut second);
        assert!(!runtime().heap.lock().unwrap().old_regions[0].sweep_quarantined);
        assert_eq!(
            runtime().heap.lock().unwrap().old_region_candidates.len(),
            1
        );
        reset_internal_for_test();
    }

    static SCALE_START: AtomicBool = AtomicBool::new(false);
    static SCALE_DONE: AtomicBool = AtomicBool::new(false);
    // One background batch can complete several regions and reports only the
    // last one, so the first pause need not carry index 0.
    fn scale_between_regions(_index: usize) {
        SCALE_START.store(true, Ordering::Release);
    }

    #[test]
    fn bounded_sweep_fixed_root_scaling_reports_stalls_and_reclamation() {
        let _guard = runtime_test_guard();
        for mib in [64usize, 256, 1024] {
            reset_internal_for_test();
            let mut root = willow_alloc(8);
            willow_push_root(&mut root);
            {
                let mut state = runtime().heap.lock().unwrap();
                state.threshold_bytes = usize::MAX;
                state.memory_limit_bytes = None;
                for _ in 0..mib {
                    allocate_old_region_object_locked(
                        &mut state,
                        0,
                        0,
                        (1 << 20) - GC_HEADER_SIZE,
                        0,
                        true,
                    )
                    .unwrap();
                }
            }
            SCALE_START.store(false, Ordering::Relaxed);
            SCALE_DONE.store(false, Ordering::Relaxed);
            let ready = Arc::new(AtomicBool::new(false));
            let worker = {
                let ready = ready.clone();
                std::thread::spawn(move || {
                    let _mutator = MutatorRegistration::new();
                    ready.store(true, Ordering::Release);
                    while !SCALE_START.load(Ordering::Acquire) {
                        willow_gc_safepoint();
                        std::thread::yield_now();
                    }
                    let began = Instant::now();
                    let mut stalls = Vec::new();
                    let mut overlapped = 0usize;
                    for _ in 0..2048 {
                        overlapped += usize::from(ACTIVE_FLAG.load(Ordering::Acquire));
                        let started = Instant::now();
                        willow_alloc(8);
                        stalls.push(started.elapsed().as_nanos() as u64);
                    }
                    let elapsed = began.elapsed().as_nanos() as u64;
                    SCALE_DONE.store(true, Ordering::Release);
                    stalls.sort_unstable();
                    (stalls, elapsed, overlapped)
                })
            };
            while !ready.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            *SWEEP_TEST_HOOK.lock().unwrap() = Some(scale_between_regions);
            old_cycle::take_stop_diagnostics();
            let began = Instant::now();
            willow_gc_collect();
            let elapsed = began.elapsed().as_nanos() as u64;
            *SWEEP_TEST_HOOK.lock().unwrap() = None;
            // A sweep that finishes inside its first batch never pauses;
            // release the mutator instead of waiting for it forever.
            SCALE_START.store(true, Ordering::Release);
            let (stalls, mutator_ns, overlapped) = worker.join().unwrap();
            let report = LAST
                .lock()
                .unwrap()
                .clone()
                .expect("normal concurrent sweep");
            assert_eq!(report.reclaimed_bytes, (mib << 20) as u64);
            let stops = crate::gc_telemetry::stops::snapshot_stops();
            assert_eq!(stops.by_reason[0].work.swept_objects, 0);
            assert_eq!(stops.by_reason[1].work.swept_objects, 0);
            assert_eq!(
                stops.by_reason[3].completed, 0,
                "fixture must complete without Recovery"
            );
            for (phase, stop) in stops.by_reason.into_iter().enumerate() {
                println!(
                    "sweep_scale mib={mib} phase={phase} stops={} stopped_ns={} metadata_objects={} metadata_bytes={} roots={} swept_objects={}",
                    stop.completed,
                    stop.stopped.total_ns,
                    stop.work.metadata_objects,
                    stop.work.metadata_bytes,
                    stop.work.root_values,
                    stop.work.swept_objects
                );
            }
            for diagnostic in old_cycle::take_stop_diagnostics() {
                println!("sweep_scale mib={mib} stop_diagnostic {diagnostic:?}");
            }
            println!(
                "sweep_scale mib={mib} cycle_ns={elapsed} mutator_ns={mutator_ns} samples={} overlap_allocations={overlapped} stall_p50_ns={} stall_p99_ns={} stall_p999_ns={} stall_max_ns={} {:?}",
                stalls.len(),
                stalls[stalls.len() / 2],
                stalls[stalls.len() * 99 / 100],
                stalls[stalls.len() * 999 / 1000],
                stalls.last().unwrap(),
                report
            );
            // Background batches hold the heap at most once per object plus
            // twice per MiB of partial progress. Each allocation assist adds
            // one bounded hold; slower runners overlap more allocations with
            // the sweep, so assists vary with the machine, not with heap size.
            assert!(
                report.heap_holds <= report.objects + 2 * (mib + 1) as u64 + report.assists,
                "{report:?}"
            );
            willow_pop_root();
        }
        reset_internal_for_test();
    }

    struct LabsEnabled;
    impl LabsEnabled {
        fn new() -> Self {
            old_lab::OLD_LABS_FOR_TEST.store(true, Ordering::Release);
            Self
        }
    }
    impl Drop for LabsEnabled {
        fn drop(&mut self) {
            old_lab::OLD_LABS_FOR_TEST.store(false, Ordering::Release);
        }
    }

    #[test]
    fn bounded_sweep_release_reindexes_active_new_buffers_by_stable_base() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        willow_alloc((GC_LARGE_OBJECT_THRESHOLD + 8) as i64);
        let live = willow_alloc(8);
        HeapObject::from_payload(GcPayload::from_raw(live).unwrap()).begin_trace();
        let mut cursor = make_cursor();
        let _labs = LabsEnabled::new();
        let during = willow_alloc(8);
        let base = {
            let state = runtime().heap.lock().unwrap();
            assert_eq!(state.old_regions.len(), 3);
            state.old_regions[2].start()
        };
        finish(&mut cursor);
        {
            let mut state = runtime().heap.lock().unwrap();
            assert_eq!(state.old_addresses.exact(base), Some(0));
            old_lab::flush_all(&mut state);
            verify_old_region_metadata(&state).unwrap();
            assert!(
                state.old_regions[0]
                    .allocations
                    .contains_key(&(during as usize - GC_HEADER_SIZE - base))
            );
        }
        let later = willow_alloc(8);
        assert!(later as usize > during as usize);
        old_lab::retire_all(&mut runtime().heap.lock().unwrap());
        reset_internal_for_test();
    }

    #[test]
    fn bounded_sweep_competing_assists_share_ownership_without_double_reclamation() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        for _ in 0..2048 {
            willow_alloc(8);
        }
        let job = Arc::new(Mutex::new(make_cursor()));
        set_active(Some(job.clone()));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..256 {
                        assist();
                    }
                });
            }
        });
        let mut cursor = job.lock().unwrap();
        finish(&mut cursor);
        assert_eq!(
            cursor.report.reclaimed_bytes,
            2048 * (GC_HEADER_SIZE + 8) as u64
        );
        assert_eq!(runtime().heap.lock().unwrap().total_frees, 2048);
        assert!(cursor.report.assist_objects > 0);
        drop(cursor);
        set_active(None);
        reset_internal_for_test();
    }

    #[test]
    fn bounded_sweep_competing_allocators_keep_forward_progress() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        for _ in 0..4096 {
            willow_alloc(8);
        }
        let job = Arc::new(Mutex::new(make_cursor()));
        set_active(Some(job.clone()));
        let stalls = std::thread::scope(|scope| {
            let mut threads = Vec::new();
            for _ in 0..8 {
                threads.push(scope.spawn(|| {
                    let _mutator = MutatorRegistration::new();
                    let mut stalls = Vec::new();
                    for _ in 0..128 {
                        let began = Instant::now();
                        let pointer = willow_alloc(8);
                        unsafe {
                            *pointer.cast::<u64>() = 101;
                        }
                        stalls.push(began.elapsed().as_nanos() as u64);
                    }
                    stalls
                }));
            }
            threads
                .into_iter()
                .flat_map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        let mut cursor = job.lock().unwrap();
        finish(&mut cursor);
        assert_eq!(
            cursor.report.reclaimed_bytes,
            4096 * (GC_HEADER_SIZE + 8) as u64
        );
        assert!(cursor.report.assist_objects > 0);
        assert_eq!(
            willow_gc_allocated_bytes(),
            1024 * (GC_HEADER_SIZE + 8) as i64
        );
        let mut stalls = stalls;
        stalls.sort_unstable();
        println!(
            "sweep_competing allocators=8 allocations=1024 p50_ns={} p99_ns={} p999_ns={} max_ns={} {:?}",
            stalls[512], stalls[1013], stalls[1022], stalls[1023], cursor.report
        );
        drop(cursor);
        set_active(None);
        reset_internal_for_test();
    }

    #[test]
    fn bounded_sweep_partial_tlab_index_keeps_unprocessed_live_objects_visible() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        let mut tls = tlab_state_for_test();
        let first = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
        HeapObject::from_payload(GcPayload::from_raw(first).unwrap()).begin_trace();
        let mut last = first;
        for _ in 0..5 {
            let cursor = tls.cursor.load(Ordering::Acquire);
            let object =
                initialize_object_at(cursor as *mut u8, GC_HEADER_SIZE + 8, 0, 0, 0).unwrap();
            publish_tlab_start_for_test(&tls, cursor as *mut u8);
            tls.cursor
                .store(cursor + GC_HEADER_SIZE + 8, Ordering::Release);
            last = object.payload().as_ptr();
        }
        HeapObject::from_payload(GcPayload::from_raw(last).unwrap()).begin_trace();
        {
            let mut state = runtime().heap.lock().unwrap();
            sync_tlab_bytes(&mut state);
            retire_all_tlabs_unindexed(&mut state);
        }
        let mut cursor = make_cursor();
        let limits = Limits {
            objects: 1,
            bytes: 64,
            ns: 100,
        };
        cursor.batch_with_clock(limits, false, || 0); // Reserve outside heap lock.
        cursor.batch_with_clock(limits, true, || 0); // One header; private prefix.
        assert_eq!(cursor.headers.as_ref().unwrap().len(), 1);
        {
            let state = runtime().heap.lock().unwrap();
            let chunk = &state.tlab_chunks[0];
            assert!(chunk.header_offsets.is_empty());
            assert!(chunk.needs_index());
            assert!(object_in_retired_chunk(chunk, last as usize, false).is_some());
            assert!(object_in_retired_chunk(chunk, last as usize + 1, true).is_some());
            validate_payload_pointer_locked(&state, last).unwrap();
        }
        finish(&mut cursor);
        let state = runtime().heap.lock().unwrap();
        assert_eq!(state.tlab_chunks[0].header_offsets.len(), 6);
        assert_eq!(state.tlab_chunks[0].live_bytes, 2 * (GC_HEADER_SIZE + 8));
        assert!(object_in_retired_chunk(&state.tlab_chunks[0], last as usize, false).is_some());
        drop(state);
        assert_eq!(willow_gc_allocated_bytes(), 2 * (GC_HEADER_SIZE + 8) as i64);
        reset_internal_for_test();
    }

    #[test]
    fn bounded_sweep_fragmented_tlabs_skip_already_reclaimed_physical_headers() {
        let _guard = runtime_test_guard();
        for objects in [128usize, 512, GC_TLAB_CHUNK_SIZE / (GC_HEADER_SIZE + 8)] {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
            let mut tls = tlab_state_for_test();
            let live = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
            let survivor = HeapObject::from_payload(GcPayload::from_raw(live).unwrap());
            survivor.begin_trace();
            for _ in 1..objects {
                let at = tls.cursor.load(Ordering::Acquire);
                assert!(at + GC_HEADER_SIZE + 8 <= tls.limit.load(Ordering::Acquire));
                initialize_object_at(at as *mut u8, GC_HEADER_SIZE + 8, 0, 0, 0).unwrap();
                publish_tlab_start_for_test(&tls, at as *mut u8);
                tls.cursor.store(at + GC_HEADER_SIZE + 8, Ordering::Release);
            }
            {
                let mut state = runtime().heap.lock().unwrap();
                sync_tlab_bytes(&mut state);
                retire_all_tlabs_unindexed(&mut state);
            }
            let mut first = make_cursor();
            finish(&mut first);
            assert_eq!(first.report.swept_objects, objects as u64);
            survivor.begin_trace();
            let mut second = make_cursor();
            finish(&mut second);
            let words = (objects * (GC_HEADER_SIZE + 8)).div_ceil(64 * GC_REGION_MARK_GRANULE);
            assert_eq!(second.report.swept_objects, 1);
            assert!(second.report.objects <= words as u64 + 5);
            println!(
                "sweep_fragmented physical_headers={objects} first_objects={} second_objects={} second_work_items={} bitmap_reads={} word_bound={words}",
                first.report.swept_objects,
                second.report.swept_objects,
                second.report.objects,
                second.report.metadata_items
            );
            assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
            reset_internal_for_test();
        }
    }

    static SHUTDOWN_PAUSED: AtomicBool = AtomicBool::new(false);
    static SHUTDOWN_RESUME: AtomicBool = AtomicBool::new(false);
    fn pause_for_shutdown(index: usize) {
        if index != 0 {
            return;
        }
        assert!(!runtime().stop_requested.load(Ordering::Acquire));
        SHUTDOWN_PAUSED.store(true, Ordering::Release);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !SHUTDOWN_RESUME.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "shutdown sweep did not resume");
            std::thread::yield_now();
        }
    }

    #[test]
    fn bounded_sweep_coordinator_shutdown_waits_for_owned_cleanup() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        willow_alloc((GC_LARGE_OBJECT_THRESHOLD + 8) as i64);
        let mut root = willow_alloc(8);
        let mutator = MutatorRegistration::new();
        willow_push_root(&mut root);
        SHUTDOWN_PAUSED.store(false, Ordering::Relaxed);
        SHUTDOWN_RESUME.store(false, Ordering::Relaxed);
        *SWEEP_TEST_HOOK.lock().unwrap() = Some(pause_for_shutdown);
        assert!(coordinator::request());
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !SHUTDOWN_PAUSED.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "background sweep did not start");
            willow_gc_safepoint();
            std::thread::yield_now();
        }
        assert!(ACTIVE_FLAG.load(Ordering::Acquire));
        let releaser = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(2));
            SHUTDOWN_RESUME.store(true, Ordering::Release);
        });
        coordinator::shutdown();
        releaser.join().unwrap();
        *SWEEP_TEST_HOOK.lock().unwrap() = None;
        assert!(!ACTIVE_FLAG.load(Ordering::Acquire));
        assert!(runtime().heap.lock().unwrap().sweeping.is_none());
        assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
        assert!(runtime().heap.lock().unwrap().released_bytes > 0);
        willow_pop_root();
        drop(mutator);
        reset_internal_for_test();
    }

    #[test]
    fn bounded_sweep_allocate_reclaim_waves_preserve_root_and_accounting() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        let mut root = willow_alloc(8);
        willow_push_root(&mut root);
        for wave in 0..20 {
            runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
            for _ in 0..512 {
                willow_alloc(8);
            }
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
            let state = runtime().heap.lock().unwrap();
            assert_eq!(state.total_frees, (wave + 1) * 512);
            verify_old_region_metadata(&state).unwrap();
            assert!(state.old_reserved_bytes <= 2 * GC_OLD_REGION_SIZE);
        }
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_internal_for_test();
    }

    #[test]
    fn bounded_sweep_descriptor_writer_contention_preserves_work_and_references() {
        let _guard = runtime_test_guard();
        for tlab in [false, true] {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
            let mut tls = tlab_state_for_test();
            let pointer = if tlab {
                willow_gc_alloc_slow(&mut tls, 887766, 99300, 8, 0)
            } else {
                willow_gc_alloc_layout(887766, 99300, 8, 0)
            };
            let object = HeapObject::from_payload(GcPayload::from_raw(pointer).unwrap());
            let key = willow_abi::GcLayoutDescriptor {
                layout_id: 887766,
                type_id: 99300,
                gc_ref_mask: 0,
                size: (GC_HEADER_SIZE + 8) as u64,
            };
            assert_eq!(layouts::references(key), 1);
            if tlab {
                retire_all_tlabs_unindexed(&mut runtime().heap.lock().unwrap());
            }
            let mut cursor = make_cursor();
            if tlab {
                cursor.batch_with_clock(
                    Limits {
                        objects: 1,
                        bytes: 64,
                        ns: 100,
                    },
                    false,
                    || 0,
                );
                assert!(cursor.headers.is_some());
            }
            let before_objects = cursor.report.objects;
            let before_bytes = cursor.report.bytes;
            let before_cursor = cursor.offset;
            let before_frees = runtime().heap.lock().unwrap().total_frees;
            layouts::with_exclusive_registry(|| {
                for _ in 0..64 {
                    cursor.batch_with_clock(ALLOCATION, true, || 0);
                    // The collector uses the same nonblocking admission, too.
                    cursor.batch_with_clock(BACKGROUND, false, || 0);
                    assert_eq!(cursor.offset, before_cursor);
                    assert_eq!(cursor.report.objects, before_objects);
                    assert_eq!(cursor.report.bytes, before_bytes);
                    assert_eq!(cursor.report.reclaimed_bytes, 0);
                    assert!(object.allocated());
                    let state = runtime()
                        .heap
                        .try_lock()
                        .expect("descriptor contention held heap lock");
                    assert_eq!(state.total_frees, before_frees);
                    validate_payload_pointer_locked(&state, pointer).unwrap();
                    if tlab {
                        assert!(state.tlab_chunks[0].mark_bitmap.is_marked(0));
                        assert!(state.tlab_chunks[0].header_offsets.is_empty());
                        assert!(cursor.headers.as_ref().unwrap().is_empty());
                    } else {
                        assert!(state.old_regions[0].allocations.contains_key(&0));
                        assert!(state.old_regions[0].mark_bitmap.is_marked(0));
                    }
                }
            });
            assert_eq!(layouts::references(key), 1);
            cursor.batch_with_clock(ALLOCATION, true, || 0);
            assert_eq!(cursor.report.reclaimed_bytes, (GC_HEADER_SIZE + 8) as u64);
            assert_eq!(layouts::references(key), 0);
            finish(&mut cursor);
            finish(&mut cursor);
            assert_eq!(runtime().heap.lock().unwrap().total_frees, before_frees + 1);
            assert_eq!(willow_gc_allocated_bytes(), 0);
            println!(
                "descriptor_contention tlab={tlab} rejected_batches=128 object_work=0 retained_references=1 eventual_releases=1"
            );
            reset_internal_for_test(); // TLS remains alive through reset.
        }
    }

    unsafe fn verify_owned_payload_before_release(payload: *mut u8) {
        assert_eq!(unsafe { *payload.cast::<u64>() }, 0xAABBCCDD);
        let state = runtime().heap.try_lock().expect("hook held heap mutex");
        let address = payload as usize;
        let owned_old = state
            .old_addresses
            .candidate(address - GC_HEADER_SIZE)
            .is_some_and(|index| state.old_regions[index].contains(address));
        assert!(
            owned_old || find_tlab_chunk(&state, address).is_some(),
            "payload extent released before its hook"
        );
        HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn bounded_sweep_hook_runs_once_for_dead_payload_before_storage_release() {
        let _guard = runtime_test_guard();
        for tlab in [false, true] {
            reset_internal_for_test();
            HOOK_CALLS.store(0, Ordering::Relaxed);
            runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
            willow_register_drop(99301, verify_owned_payload_before_release);
            let mut tls = tlab_state_for_test();
            let mut allocate = || {
                if tlab {
                    willow_gc_alloc_slow(&mut tls, 887767, 99301, 8, 0)
                } else {
                    willow_gc_alloc_layout(887767, 99301, 8, 0)
                }
            };
            let dead = allocate();
            let live = allocate();
            unsafe {
                *dead.cast::<u64>() = 0xAABBCCDD;
                *live.cast::<u64>() = 0xAABBCCDD;
            }
            HeapObject::from_payload(GcPayload::from_raw(live).unwrap()).begin_trace();
            if tlab {
                retire_all_tlabs_unindexed(&mut runtime().heap.lock().unwrap());
            }
            let mut cursor = make_cursor();
            finish(&mut cursor);
            finish(&mut cursor);
            assert_eq!(
                HOOK_CALLS.load(Ordering::Relaxed),
                1,
                "live payload hook must not run"
            );
            assert_eq!(unsafe { *live.cast::<u64>() }, 0xAABBCCDD);
            assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
            let mut second = make_cursor();
            finish(&mut second);
            assert_eq!(HOOK_CALLS.load(Ordering::Relaxed), 2);
            assert_eq!(willow_gc_allocated_bytes(), 0);
            reset_internal_for_test();
        }
    }
}
