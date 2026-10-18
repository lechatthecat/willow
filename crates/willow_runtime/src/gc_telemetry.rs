//! Versioned measurements for the current collector and its global stops.
//!
//! Allocation counters reuse the collector's TLAB accounting. Collection work
//! accumulates locally and is published once per cycle; no per-object atomic or
//! telemetry lock is added to allocation or graph traversal. Heap gauges and
//! cycle statistics are individually coherent groups, not one global instant.
//! Trace I/O happens only after the world resumes and collector locks are gone.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

pub mod failure_record;
pub mod measurement;
pub mod milestones;
pub mod native_trace;
pub mod observations;
pub mod stops;
pub mod workers;
pub use milestones::{GcStage, GcTicket, WaitError, wait_gc_for_test};

/// V3 extends the existing snapshot protocol without growing V1 or V2.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WillowGcStatsV3 {
    pub version: u32,
    pub struct_size: u32,
    pub timestamp_ns: u64,
    pub baseline: WillowGcStatsV2,
    pub milestones: milestones::CycleMilestones,
    pub memory: measurement::GcMemoryV3,
    pub survivors: GcSurvivorStats,
    pub frames: crate::frame_lifetime::FrameLifetimeSnapshot,
    pub measurements: measurement::MeasurementsV3,
    pub barrier_variants: [u64; 13],
    pub satb_deletions: u64,
    pub barrier_variants_valid: u64,
}
pub use stops::WillowGcStatsV2;

/// V4 preserves all earlier layouts and adds opt-in interval/work observations.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WillowGcStatsV4 {
    pub version: u32,
    pub struct_size: u32,
    pub timestamp_ns: u64,
    pub baseline: WillowGcStatsV3,
    pub observations: observations::Stats,
}

pub const GC_STATS_VERSION: u32 = 1;
pub const HISTOGRAM_BUCKETS: usize = 65;

/// Bucket 0 contains zero; bucket b > 0 contains [2^(b-1), 2^b) ns.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcLatencyV1 {
    pub count: u64,
    pub total_ns: u64,
    pub max_ns: u64,
    pub buckets: [u64; HISTOGRAM_BUCKETS],
}

impl Default for GcLatencyV1 {
    fn default() -> Self {
        Self {
            count: 0,
            total_ns: 0,
            max_ns: 0,
            buckets: [0; HISTOGRAM_BUCKETS],
        }
    }
}

impl GcLatencyV1 {
    fn record(&mut self, ns: u64) {
        let bucket = (u64::BITS - ns.leading_zeros()) as usize;
        self.count = self.count.saturating_add(1);
        self.total_ns = self.total_ns.saturating_add(ns);
        self.max_ns = self.max_ns.max(ns);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
    }
}

/// Logical bytes include object headers and exclude copies made by promotion.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GcCountersV1 {
    pub allocation_count: u64,
    pub allocation_bytes: u64,
    pub freed_objects: u64,
    pub released_bytes: u64,
    pub tlab_fast_allocations: u64,
    pub tlab_slow_allocations: u64,
    pub tlab_refills: u64,
    /// Objects reaching old generation, including promotion in place.
    pub promoted_objects: u64,
    pub promoted_bytes: u64,
    pub moved_objects: u64,
    /// Processed barriers; excludes null/null stores and null deletions when
    /// SATB marking is inactive, both of which require no barrier work.
    pub barrier_calls: u64,
    pub barrier_hits: u64,
}

/// Survivor aging measurements, separate from the stable V1 snapshot layout.
/// Copies are physical movement, not logical allocations. Promotions count only
/// objects reaching old generation; pinned promotions do not copy payloads.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct GcSurvivorStats {
    pub survivor_copies: u64,
    pub survivor_bytes: u64,
    pub tenured_objects: u64,
    pub tenured_bytes: u64,
    pub pinned_promotions: u64,
    /// Minor collections skipped because a thread parked inside runtime code
    /// published a young referent it may still hold as a raw copy.
    pub deferred_minor_collections: u64,
    pub minor_deferral_age_bytes: u64,
    pub minor_retry_target_bytes: u64,
    pub minor_blockers: u64,
    pub pin_fallback_count: u64,
    pub pinned_bytes: u64,
    /// Runtime stack, remembered owner, old/foreign, memory budget, scoped loan.
    pub pin_reasons: [u64; 5],
    pub active_pins: u64,
    pub active_pin_bytes: u64,
    pub released_pins: u64,
    pub survivor_space_reserved: u64,
    pub survivor_space_live: u64,
}

/// An in-place promoted allocation owns its diagnostic record until reclamation.
/// Addresses are identities only, never roots or dereferenceable handles.
#[derive(Clone, Debug)]
pub struct GcPinRecord {
    pub owner_allocation: usize,
    pub reason: &'static str,
    pub minor_epoch: u64,
    pub major_epoch: u64,
    pub bytes: u64,
}

pub fn pin_snapshot() -> Vec<GcPinRecord> {
    crate::gc::pin_snapshot()
}

pub fn survivor_snapshot() -> GcSurvivorStats {
    crate::gc::survivor_snapshot()
}

/// Managed storage only. Native requested estimates are exposed separately by
/// `native_memory_snapshot()` and must not be reported as managed savings or OS commit.
/// Managed committed bytes retain the allocator reservation convention (not RSS).
/// occupied includes unreachable objects until collection, NOT measured liveness.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GcHeapV1 {
    pub occupied_bytes: u64,
    pub young_occupied_bytes: u64,
    pub reserved_bytes: u64,
    pub committed_bytes: u64,
    pub old_reserved_bytes: u64,
    /// Combined bump storage (nursery, pinned, survivor); survivor_snapshot
    /// separately exposes the survivor subset without changing this V1 layout.
    pub nursery_reserved_bytes: u64,
    pub old_regions: u64,
    pub remembered_objects: u64,
    pub dirty_cards: u64,
    pub major_trigger_bytes: u64,
    pub minor_trigger_bytes: u64,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CycleKind {
    Minor = 1,
    Major = 2,
}

/// A completed cycle. A minor cycle's retained bytes include uncollected old
/// garbage; only a major cycle measures whole-heap liveness. Work includes
/// scanned reference slots (including nulls), and marked header+payload bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GcCycleV1 {
    pub epoch: u64,
    pub kind: u32,
    pub reserved: u32,
    pub start_ns: u64,
    pub end_ns: u64,
    /// Aggregate global stops for major; full cycle interval for minor.
    pub pause_ns: u64,
    pub mark_ns: u64,
    pub marked_bytes: u64,
    pub scanned_bytes: u64,
    pub root_scan_bytes: u64,
    pub heap_before_bytes: u64,
    pub heap_after_bytes: u64,
    pub reclaimed_bytes: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WillowGcStatsV1 {
    pub version: u32,
    pub struct_size: u32,
    pub timestamp_ns: u64,
    /// 0 = idle, 1 = minor cycle, 2 = major cycle (including concurrent work).
    pub phase: u32,
    /// Whether process_resident_bytes was available. RSS is process-wide.
    pub resident_valid: u32,
    pub epoch: u64,
    pub reset_generation: u64,
    pub process_resident_bytes: u64,
    pub counters: GcCountersV1,
    pub heap: GcHeapV1,
    pub minor_cycles: u64,
    pub major_cycles: u64,
    pub marked_bytes: u64,
    pub scanned_bytes: u64,
    pub root_scan_bytes: u64,
    pub mark_ns: u64,
    pub pauses: GcLatencyV1,
    pub minor_pauses: GcLatencyV1,
    pub major_pauses: GcLatencyV1,
    pub last_cycle: GcCycleV1,
    pub last_major_cycle: GcCycleV1,
    pub trace_errors: u64,
}

/// Rates over an explicit observation window. No hidden smoothing or wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct GcRates {
    pub allocation_bytes_per_second: f64,
    pub mark_bytes_per_second: f64,
    pub mark_bytes_per_mark_second: f64,
}

impl GcRates {
    pub fn between(before: &WillowGcStatsV1, after: &WillowGcStatsV1) -> Option<Self> {
        if before.version != GC_STATS_VERSION
            || after.version != GC_STATS_VERSION
            || before.reset_generation != after.reset_generation
        {
            return None;
        }
        let ns = after.timestamp_ns.checked_sub(before.timestamp_ns)?;
        if ns == 0 {
            return None;
        }
        let allocated = after
            .counters
            .allocation_bytes
            .checked_sub(before.counters.allocation_bytes)?;
        let marked = after.marked_bytes.checked_sub(before.marked_bytes)?;
        let mark_ns = after.mark_ns.checked_sub(before.mark_ns)?;
        Some(Self {
            allocation_bytes_per_second: allocated as f64 * 1e9 / ns as f64,
            mark_bytes_per_second: marked as f64 * 1e9 / ns as f64,
            mark_bytes_per_mark_second: if mark_ns == 0 {
                0.0
            } else {
                marked as f64 * 1e9 / mark_ns as f64
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MarkWork {
    pub objects: u64,
    pub marked_bytes: u64,
    pub scanned_bytes: u64,
    pub root_scan_bytes: u64,
    pub mark_ns: u64,
    /// Internal controller sample; frozen public telemetry layouts do not grow.
    pub cpu_ns: u64,
    pub cpu_incomplete: bool,
    pub descriptor_bytes: u64,
    pub frame_scans: u64,
    pub remembered_owners: u64,
    pub collector_mark_cpu_ns: u64,
    pub sweep_assist_cpu_ns: u64,
    pub sweep_cpu_incomplete: bool,
}

pub(crate) fn profiling() -> bool {
    static ENABLED: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("WILLOW_GC_PROFILE").is_some());
    *ENABLED
}

impl MarkWork {
    pub(crate) fn frame(&mut self, layout: u64, ty: u32, bytes: usize, mask: u64) {
        if profiling()
            && ty == 0
            && layout != 0
            && layout
                == willow_abi::gc_layout_id(
                    willow_abi::GcObjectKind::AsyncFrame,
                    bytes as i64,
                    0,
                    mask,
                )
        {
            self.frame_scans += 1;
        }
    }

    pub(crate) fn roots(count: usize) -> Self {
        Self {
            root_scan_bytes: (count as u64).saturating_mul(std::mem::size_of::<usize>() as u64),
            ..Self::default()
        }
    }
    pub(crate) fn object(&mut self, bytes: usize, slots: usize) {
        self.objects = self.objects.saturating_add(1);
        self.marked_bytes = self.marked_bytes.saturating_add(bytes as u64);
        self.scanned_bytes = self
            .scanned_bytes
            .saturating_add((slots as u64).saturating_mul(std::mem::size_of::<usize>() as u64));
    }
}

#[derive(Default)]
struct GcTelemetry {
    stats: WillowGcStatsV1,
    minor_cpu_ns: u64,
}
static TELEMETRY: LazyLock<Mutex<GcTelemetry>> =
    LazyLock::new(|| Mutex::new(GcTelemetry::default()));
static START: LazyLock<Instant> = LazyLock::new(Instant::now);
static TRACE_ERRORS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn timestamp_ns() -> u64 {
    elapsed_ns(*START)
}
pub(crate) fn elapsed_ns(start: Instant) -> u64 {
    start.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

pub(crate) struct Cycle {
    kind: CycleKind,
    epoch: u64,
    start_ns: u64,
    completed: bool,
    previous_phase: u32,
    cpu_start: Option<u64>,
    minor_cpu_before: u64,
}

impl Drop for Cycle {
    fn drop(&mut self) {
        if !self.completed {
            // An aborted collection has no completed work to publish. Keep its
            // epoch consumed while allowing the next elected collector to start.
            let mut telemetry = TELEMETRY.lock().unwrap_or_else(|p| p.into_inner());
            telemetry.stats.phase = self.previous_phase;
            milestones::abort(self.epoch);
        }
    }
}

impl Cycle {
    /// Called only by the elected collector, before requesting safepoints.
    pub(crate) fn begin(kind: CycleKind) -> Self {
        let start_ns = timestamp_ns();
        let mut telemetry = TELEMETRY.lock().unwrap_or_else(|p| p.into_inner());
        let minor_cpu_before = telemetry.minor_cpu_ns;
        let stats = &mut telemetry.stats;
        let previous_phase = stats.phase;
        debug_assert!(
            previous_phase == 0
                || (previous_phase == CycleKind::Major as u32 && kind == CycleKind::Minor)
        );
        stats.epoch = stats
            .epoch
            .checked_add(1)
            .expect("GC cycle identity exhausted");
        stats.phase = kind as u32;
        milestones::begin(stats.epoch);
        observations::epoch(stats.epoch);
        Self {
            kind,
            epoch: stats.epoch,
            start_ns,
            completed: false,
            previous_phase,
            cpu_start: profiling().then(workers::thread_cpu_ns).flatten(),
            minor_cpu_before,
        }
    }
    /// Called after resuming mutators; returns the trace record for publication
    /// after releasing collector election. `before/after` were sampled under STW.
    pub(crate) fn finish(self, before: u64, after: u64, work: MarkWork) -> GcCycleV1 {
        self.finish_metrics(before, after, work, None, None)
    }
    /// Concurrent cycles measure only global stops as pauses;
    /// allocations between them mean reclaimed bytes cannot be before - after.
    pub(crate) fn finish_concurrent(
        self,
        before: u64,
        after: u64,
        work: MarkWork,
        pause_ns: u64,
        reclaimed: u64,
    ) -> GcCycleV1 {
        self.finish_metrics(before, after, work, Some(pause_ns), Some(reclaimed))
    }
    fn finish_metrics(
        mut self,
        before: u64,
        after: u64,
        work: MarkWork,
        pause_ns: Option<u64>,
        reclaimed: Option<u64>,
    ) -> GcCycleV1 {
        let end_ns = timestamp_ns();
        observations::work(
            if self.kind == CycleKind::Major { 4 } else { 5 },
            observations::Work {
                objects: work.objects,
                slots: work.scanned_bytes / std::mem::size_of::<usize>() as u64,
                bytes: work.marked_bytes,
                metadata_bytes: work.descriptor_bytes,
                ..Default::default()
            },
        );
        let cycle = GcCycleV1 {
            epoch: self.epoch,
            kind: self.kind as u32,
            start_ns: self.start_ns,
            end_ns,
            pause_ns: pause_ns.unwrap_or_else(|| end_ns.saturating_sub(self.start_ns)),
            mark_ns: work.mark_ns,
            marked_bytes: work.marked_bytes,
            scanned_bytes: work.scanned_bytes,
            root_scan_bytes: work.root_scan_bytes,
            heap_before_bytes: before,
            heap_after_bytes: after,
            reclaimed_bytes: reclaimed.unwrap_or_else(|| before.saturating_sub(after)),
            reserved: 0,
        };
        let cpu = self
            .cpu_start
            .and_then(|start| workers::thread_cpu_ns()?.checked_sub(start));
        let cpu = {
            let mut telemetry = TELEMETRY.lock().unwrap_or_else(|p| p.into_inner());
            let cpu = cpu.map(|cpu| {
                if self.kind == CycleKind::Minor {
                    telemetry.minor_cpu_ns = telemetry.minor_cpu_ns.saturating_add(cpu);
                    cpu
                } else {
                    // Nested minors run on this collector. Mark batches from
                    // this thread are already in its CPU interval; other
                    // workers/assists contribute their independently timed CPU.
                    cpu.saturating_sub(telemetry.minor_cpu_ns.saturating_sub(self.minor_cpu_before))
                        .saturating_add(work.cpu_ns.saturating_sub(work.collector_mark_cpu_ns))
                        .saturating_add(work.sweep_assist_cpu_ns)
                }
            });
            let cpu = cpu.filter(|_| {
                self.kind == CycleKind::Minor || !(work.cpu_incomplete || work.sweep_cpu_incomplete)
            });
            telemetry.complete(cycle);
            telemetry.stats.phase = self.previous_phase;
            cpu
        };
        if profiling()
            && let Some(trace) = TRACE.as_ref()
        {
            let mut trace = trace.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(writer) = trace.writer.as_mut() {
                let result = writeln!(
                    writer,
                    "{{\"event\":\"gc_work\",\"epoch\":{},\"kind\":{},\"root_slots\":{},\"remembered_owners\":{},\"frame_scans\":{},\"mark_cpu_ns\":{},\"cpu_incomplete\":{},\"gc_cpu_ns\":{}}}",
                    cycle.epoch,
                    cycle.kind,
                    work.root_scan_bytes / std::mem::size_of::<usize>() as u64,
                    work.remembered_owners,
                    work.frame_scans,
                    work.cpu_ns,
                    cpu.is_none(),
                    cpu.map_or_else(|| "null".to_string(), |cpu| cpu.to_string())
                );
                if result.is_err() {
                    trace_error();
                    trace.writer = None;
                }
            }
        }
        if self.previous_phase != 0 {
            milestones::quiescent();
        }
        self.completed = true;
        measurement::completed(work, cycle.reclaimed_bytes);
        cycle
    }
}
impl GcTelemetry {
    fn complete(&mut self, cycle: GcCycleV1) {
        let s = &mut self.stats;
        s.phase = 0;
        s.epoch = s.epoch.max(cycle.epoch);
        s.pauses.record(cycle.pause_ns);
        if cycle.kind == CycleKind::Major as u32 {
            s.major_cycles = s.major_cycles.saturating_add(1);
            s.major_pauses.record(cycle.pause_ns);
            s.last_major_cycle = cycle;
        } else {
            s.minor_cycles = s.minor_cycles.saturating_add(1);
            s.minor_pauses.record(cycle.pause_ns);
        }
        s.marked_bytes = s.marked_bytes.saturating_add(cycle.marked_bytes);
        s.scanned_bytes = s.scanned_bytes.saturating_add(cycle.scanned_bytes);
        s.root_scan_bytes = s.root_scan_bytes.saturating_add(cycle.root_scan_bytes);
        s.mark_ns = s.mark_ns.saturating_add(cycle.mark_ns);
        s.last_cycle = cycle;
    }
}

/// Snapshot without RSS sampling, suitable for control loops and frequent polls.
/// The heap group is sampled before the cycle group; a concurrent collection
/// can complete between those groups. Within each group the fields agree.
pub fn snapshot() -> WillowGcStatsV1 {
    let (counters, heap) = crate::gc::telemetry_heap_snapshot();
    let mut stats = TELEMETRY.lock().unwrap_or_else(|p| p.into_inner()).stats;
    stats.version = GC_STATS_VERSION;
    stats.struct_size = std::mem::size_of::<WillowGcStatsV1>() as u32;
    stats.timestamp_ns = timestamp_ns();
    stats.counters = counters;
    stats.heap = heap;
    stats.trace_errors = TRACE_ERRORS.load(Ordering::Relaxed);
    stats
}

/// Write exactly V1's fixed layout. Returns 0, or -1 for null. The caller must
/// provide aligned, writable storage for a full WillowGcStatsV1. Future layouts
/// use a new symbol; V1 never grows. RSS is best-effort and outside GC locks.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_stats_snapshot_v1(out: *mut WillowGcStatsV1) -> i32 {
    if out.is_null() {
        return -1;
    }
    let stats = snapshot_with_resident();
    // SAFETY: the caller supplies one writable, aligned V1 object.
    unsafe {
        out.write(stats);
    }
    0
}

fn snapshot_with_resident() -> WillowGcStatsV1 {
    let mut stats = snapshot();
    if let Some(resident) = resident_bytes() {
        stats.process_resident_bytes = resident;
        stats.resident_valid = 1;
    }
    stats
}

/// Required output bytes, or -1 for an unsupported version.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_stats_size(version: i64) -> i64 {
    match version {
        1 => std::mem::size_of::<WillowGcStatsV1>() as i64,
        2 => std::mem::size_of::<WillowGcStatsV2>() as i64,
        3 => std::mem::size_of::<WillowGcStatsV3>() as i64,
        4 => std::mem::size_of::<WillowGcStatsV4>() as i64,
        _ => -1,
    }
}

/// Returns bytes written, 0 for insufficient length, or -1 for an unknown
/// version or null output. Errors never write. On success the caller supplies
/// writable storage for the required size; alignment is not required.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_stats_snapshot(version: i64, out: *mut u8, out_len: i64) -> i64 {
    let size = willow_gc_stats_size(version);
    if size < 0 {
        return -1;
    }
    if out_len < size {
        return 0;
    }
    if out.is_null() {
        return -1;
    }
    // SAFETY: the caller supplies writable storage of at least `size` bytes.
    if version == 1 {
        unsafe {
            out.cast::<WillowGcStatsV1>()
                .write_unaligned(snapshot_with_resident())
        };
    } else {
        let stats = WillowGcStatsV2 {
            version: 2,
            struct_size: std::mem::size_of::<WillowGcStatsV2>() as u32,
            timestamp_ns: timestamp_ns(),
            baseline: snapshot_with_resident(),
            stops: stops::snapshot_stops(),
            workers: workers::snapshot(),
        };
        if version == 2 {
            unsafe { out.cast::<WillowGcStatsV2>().write_unaligned(stats) };
        } else {
            let (memory, survivors) = crate::gc::telemetry_memory_snapshot();
            let (barrier_variants, satb_deletions, valid) = crate::gc::barrier_variant_counts();
            let stats = WillowGcStatsV3 {
                version: 3,
                struct_size: std::mem::size_of::<WillowGcStatsV3>() as u32,
                timestamp_ns: timestamp_ns(),
                baseline: stats,
                milestones: milestones::snapshot(),
                memory,
                survivors,
                frames: crate::frame_lifetime::snapshot(),
                measurements: measurement::snapshot(),
                barrier_variants,
                satb_deletions,
                barrier_variants_valid: u64::from(valid),
            };
            if version == 3 {
                unsafe { out.cast::<WillowGcStatsV3>().write_unaligned(stats) };
            } else {
                let stats = WillowGcStatsV4 {
                    version: 4,
                    struct_size: size as u32,
                    timestamp_ns: timestamp_ns(),
                    baseline: stats,
                    observations: observations::snapshot(),
                };
                unsafe { out.cast::<WillowGcStatsV4>().write_unaligned(stats) };
            }
        }
    }
    size
}

struct TraceSink {
    writer: Option<Box<dyn Write + Send>>,
}
impl TraceSink {
    fn from_env() -> Self {
        let Some(path) = std::env::var_os("WILLOW_GC_TRACE").filter(|p| !p.is_empty()) else {
            return Self { writer: None };
        };
        if path == "stderr" {
            return Self {
                writer: Some(Box::new(io::BufWriter::new(io::stderr()))),
            };
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(file) => Self {
                writer: Some(Box::new(io::BufWriter::new(file))),
            },
            Err(_) => {
                trace_error();
                Self { writer: None }
            }
        }
    }
    fn cycle(&mut self, c: GcCycleV1) {
        if let Some(writer) = self.writer.as_mut()
            && write_cycle(writer, c)
                .and_then(|()| {
                    if !*TRACE_MILESTONES {
                        return Ok(());
                    }
                    if let Some(record) = milestones::record(c.epoch) {
                        write_milestones(writer, record)?;
                    } else {
                        if c.epoch != 0 {
                            trace_error(); // Bounded history expired before trace publication.
                        }
                    }
                    write_verification(writer, c.epoch)
                })
                .and_then(|()| writer.flush())
                .is_err()
        {
            trace_error();
            // A broken sink must not repeatedly penalize collection.
            self.writer = None;
        }
    }
    fn stops(&mut self, events: &[stops::StopEventV2]) {
        if let Some(writer) = self.writer.as_mut()
            && events
                .iter()
                .try_for_each(|event| write_stop(writer, event))
                .is_err()
        {
            trace_error();
            self.writer = None;
        }
    }
}

fn write_milestones(writer: &mut dyn Write, record: milestones::CycleMilestones) -> io::Result<()> {
    for (stage, name, timestamp) in [
        (1, "MarkComplete", record.mark_complete_ns),
        (2, "SweepComplete", record.sweep_complete_ns),
        (3, "Quiescent", record.quiescent_ns),
    ] {
        if record.stage >= stage {
            writeln!(
                writer,
                "{{\"version\":3,\"event\":\"{name}\",\"epoch\":{},\"timestamp_ns\":{timestamp}}}",
                record.epoch
            )?;
        }
    }
    Ok(())
}

fn write_verification(writer: &mut dyn Write, event_epoch: u64) -> io::Result<()> {
    let (counts, deletions, valid) = crate::gc::barrier_variant_counts();
    let stats = measurement::snapshot();
    let telemetry = TELEMETRY.lock().unwrap_or_else(|p| p.into_inner());
    let (counter_epoch, generation) = (telemetry.stats.epoch, telemetry.stats.reset_generation);
    drop(telemetry);
    writeln!(
        writer,
        "{{\"version\":3,\"event\":\"gc_verification\",\"event_epoch\":{event_epoch},\"counter_epoch\":{counter_epoch},\"reset_generation\":{generation},\"timestamp_ns\":{},\"consistency\":\"atomic_approximate\",\"barrier_variants_valid\":{valid},\"barrier_variants\":{counts:?},\"satb_deletions\":{deletions},\"mark_cpu_ns\":{},\"mark_cpu_missing_cycles\":{},\"assist_cpu_ns\":{},\"assist_cpu_missing_samples\":{},\"reclaimed_bytes\":{},\"worker_threads\":{},\"worker_busy\":{},\"worker_idle\":{}}}",
        timestamp_ns(),
        stats.mark_cpu_ns,
        stats.mark_cpu_missing_cycles,
        stats.assist_cpu_ns,
        stats.assist_cpu_missing_samples,
        stats.reclaimed_bytes,
        stats.worker_threads,
        stats.worker_busy,
        stats.worker_idle
    )
}

fn write_stop(writer: &mut dyn Write, e: &stops::StopEventV2) -> io::Result<()> {
    writeln!(
        writer,
        "{{\"version\":2,\"event\":\"gc_stop_request\",\"sequence\":{},\"reset_generation\":{},\"reason\":{},\"timestamp_ns\":{}}}",
        e.sequence, e.reset_generation, e.reason, e.requested_ns
    )?;
    writeln!(
        writer,
        "{{\"version\":2,\"event\":\"gc_stop_release\",\"sequence\":{},\"reset_generation\":{},\"reason\":{},\"timestamp_ns\":{},\"stopped_ns\":{},\"outcome\":{},\"metadata_objects\":{},\"metadata_bytes\":{},\"root_values\":{},\"swept_objects\":{}}}",
        e.sequence,
        e.reset_generation,
        e.reason,
        e.released_ns,
        e.stopped_ns,
        e.outcome,
        e.work.metadata_objects,
        e.work.metadata_bytes,
        e.work.root_values,
        e.work.swept_objects
    )
}
fn trace_error() {
    let _ = TRACE_ERRORS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        Some(n.saturating_add(1))
    });
}
fn write_cycle(writer: &mut dyn Write, c: GcCycleV1) -> io::Result<()> {
    // Emit the pair together after world resumption. Times describe the event,
    // not the delayed I/O; the common epoch joins each start with its end.
    writeln!(
        writer,
        "{{\"version\":1,\"event\":\"gc_start\",\"epoch\":{},\"kind\":{},\"timestamp_ns\":{},\"heap_bytes\":{}}}",
        c.epoch, c.kind, c.start_ns, c.heap_before_bytes
    )?;
    writeln!(
        writer,
        "{{\"version\":1,\"event\":\"gc_end\",\"epoch\":{},\"kind\":{},\"timestamp_ns\":{},\"pause_ns\":{},\"mark_ns\":{},\"marked_bytes\":{},\"scanned_bytes\":{},\"root_scan_bytes\":{},\"heap_bytes\":{},\"reclaimed_bytes\":{}}}",
        c.epoch,
        c.kind,
        c.end_ns,
        c.pause_ns,
        c.mark_ns,
        c.marked_bytes,
        c.scanned_bytes,
        c.root_scan_bytes,
        c.heap_after_bytes,
        c.reclaimed_bytes
    )
}
static TRACE: LazyLock<Option<Mutex<TraceSink>>> = LazyLock::new(|| {
    let sink = TraceSink::from_env();
    sink.writer.as_ref()?;
    Some(Mutex::new(sink))
});
static TRACE_STOPS: LazyLock<bool> = LazyLock::new(|| {
    matches!(
        std::env::var("WILLOW_GC_TRACE_VERSION").as_deref(),
        Ok("2" | "3" | "4")
    )
});
static TRACE_MILESTONES: LazyLock<bool> = LazyLock::new(|| {
    matches!(
        std::env::var("WILLOW_GC_TRACE_VERSION").as_deref(),
        Ok("3" | "4")
    )
});
/// V4 observation lines go only to version-4 streams; V1-V3 consumers never
/// see them even when `WILLOW_GC_MEASURE=1` enables the aggregate snapshot.
static TRACE_OBSERVATIONS: LazyLock<bool> =
    LazyLock::new(|| std::env::var("WILLOW_GC_TRACE_VERSION").as_deref() == Ok("4"));
/// Whether a V4 sink exists to drain per-event observation history.
fn observation_history() -> bool {
    *TRACE_OBSERVATIONS && TRACE.is_some()
}
/// Flush V4 observations after releasing GC locks. Collections flush per cycle;
/// runtime exit flushes the final checkpoint, including for workloads that
/// perform no collection. Broken sinks remain nonfatal.
pub fn flush_observations() {
    if !observations::enabled() || !*TRACE_OBSERVATIONS {
        return;
    }
    // Serialize drain with output so concurrent flushes cannot reorder batches.
    if let Some(sink) = &*TRACE {
        let mut sink = sink.lock().unwrap_or_else(|p| p.into_inner());
        let (events, stats) = observations::take_events();
        if let Some(writer) = sink.writer.as_mut()
            && observations::write(writer, &events, stats)
                .and_then(|()| writer.flush())
                .is_err()
        {
            trace_error();
            sink.writer = None;
        }
    }
}
pub(crate) fn emit_cycle(cycle: GcCycleV1) {
    flush_observations();
    // Always drain: disabled tracing must neither retain an unbounded history
    // nor report artificial event loss after 64 successful stops.
    let events = if TRACE.is_some() && *TRACE_STOPS {
        stops::take_events()
    } else {
        stops::discard_events();
        Vec::new()
    };
    if let Some(sink) = &*TRACE {
        let mut sink = sink.lock().unwrap_or_else(|p| p.into_inner());
        if *TRACE_STOPS {
            sink.stops(&events);
        }
        sink.cycle(cycle);
    }
}

#[cfg(target_os = "linux")]
fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    // SAFETY: sysconf takes a constant selector and no pointers.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (page_size > 0)
        .then_some(pages.checked_mul(page_size as u64))
        .flatten()
}

#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc exposes this stable Darwin ABI; avoid a dependency for one call.
fn resident_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::mach_task_basic_info>::zeroed();
    let mut count = (std::mem::size_of::<libc::mach_task_basic_info>()
        / std::mem::size_of::<libc::natural_t>())
        as libc::mach_msg_type_number_t;
    // SAFETY: task_info receives the required count and a buffer of that size.
    let result = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            info.as_mut_ptr().cast(),
            &mut count,
        )
    };
    if result != libc::KERN_SUCCESS {
        return None;
    }
    Some(unsafe { info.assume_init() }.resident_size as u64)
}

#[cfg(target_os = "windows")]
fn resident_bytes() -> Option<u64> {
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        faults: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_nonpaged: usize,
        quota_nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
    }
    let size = std::mem::size_of::<ProcessMemoryCounters>() as u32;
    let mut counters = ProcessMemoryCounters {
        cb: size,
        faults: 0,
        peak_working_set: 0,
        working_set: 0,
        quota_peak_paged: 0,
        quota_paged: 0,
        quota_peak_nonpaged: 0,
        quota_nonpaged: 0,
        pagefile: 0,
        peak_pagefile: 0,
    };
    // SAFETY: -1 is GetCurrentProcess's documented pseudo-handle; counters has
    // the exact PROCESS_MEMORY_COUNTERS layout and initialized size.
    let ok = unsafe { GetProcessMemoryInfo(-1isize as *mut _, &mut counters, size) };
    (ok != 0).then_some(counters.working_set as u64)
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn resident_bytes() -> Option<u64> {
    None
}

pub(crate) fn reset_for_test() {
    let mut telemetry = TELEMETRY.lock().unwrap_or_else(|p| p.into_inner());
    let epoch = telemetry.stats.epoch;
    let generation = telemetry.stats.reset_generation.saturating_add(1);
    *telemetry = GcTelemetry::default();
    telemetry.stats.epoch = epoch;
    telemetry.stats.reset_generation = generation;
    stops::reset_for_test();
    native_trace::reset();
    measurement::reset();
}

#[cfg(test)]
#[path = "gc_telemetry_tests.rs"]
mod tests;

/// Retained native owner capacity. Hash tables use conservative high-water
/// estimates; this independently sampled gauge preserves the V1 C ABI layout.
pub fn external_bytes() -> u64 {
    crate::gc::external_bytes() as u64
}

/// Native requested storage accounting, in bytes. Hash tables use conservative
/// capacity high-water estimates, so even `requested_bytes` is not an allocator
/// layout measurement. Includes channel boxes/buffers/wait tables, map storage
/// and owned keys, and boxed async lock states/wait queues (including non-GC locks).
/// Excludes allocator overhead, transient reallocation overlap, stacks, runtime
/// metadata and unrelated native allocations. Sampling is not a process snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeMemorySnapshot {
    pub requested_bytes: u64,
    /// False: std HashMap does not expose its actual requested allocation layout.
    pub requested_is_exact: bool,
    /// None: no compatible allocator/OS commit provider. Never substitute RSS
    /// or the requested estimate, even when requested_bytes is zero.
    pub committed_bytes: Option<u64>,
    /// False until all process allocations and compatible commit are covered.
    pub process_limit_coverage_complete: bool,
}

pub fn native_memory_snapshot() -> NativeMemorySnapshot {
    NativeMemorySnapshot {
        requested_bytes: external_bytes(),
        requested_is_exact: false,
        committed_bytes: None,
        process_limit_coverage_complete: false,
    }
}

/// Bounded panic-path sample; never block while a collector owns telemetry.
pub(crate) fn diagnostic_cycle() -> Option<WillowGcStatsV1> {
    Some(TELEMETRY.try_lock().ok()?.stats)
}
