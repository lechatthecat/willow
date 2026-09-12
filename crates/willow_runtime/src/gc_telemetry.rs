//! Versioned measurements for the current stop-the-world collector.
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
    pub promoted_objects: u64,
    pub promoted_bytes: u64,
    pub moved_objects: u64,
    pub barrier_calls: u64,
    pub barrier_hits: u64,
}

/// GC-owned storage, excluding Rust container buffers and allocator overhead.
/// The current allocator commits every reservation, so committed == reserved.
/// occupied includes unreachable objects until collection, NOT measured liveness.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GcHeapV1 {
    pub occupied_bytes: u64,
    pub young_occupied_bytes: u64,
    pub reserved_bytes: u64,
    pub committed_bytes: u64,
    pub old_reserved_bytes: u64,
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
    /// From collector election to world resumption, including safepoint wait.
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
    /// 0 = idle, 1 = minor STW cycle, 2 = major STW cycle.
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
    pub marked_bytes: u64,
    pub scanned_bytes: u64,
    pub root_scan_bytes: u64,
    pub mark_ns: u64,
}

impl MarkWork {
    pub(crate) fn roots(count: usize) -> Self {
        Self {
            root_scan_bytes: (count as u64).saturating_mul(std::mem::size_of::<usize>() as u64),
            ..Self::default()
        }
    }
    pub(crate) fn object(&mut self, bytes: usize, slots: usize) {
        self.marked_bytes = self.marked_bytes.saturating_add(bytes as u64);
        self.scanned_bytes = self
            .scanned_bytes
            .saturating_add((slots as u64).saturating_mul(std::mem::size_of::<usize>() as u64));
    }
}

#[derive(Default)]
struct GcTelemetry {
    stats: WillowGcStatsV1,
}
static TELEMETRY: LazyLock<Mutex<GcTelemetry>> =
    LazyLock::new(|| Mutex::new(GcTelemetry::default()));
static START: LazyLock<Instant> = LazyLock::new(Instant::now);
static TRACE_ERRORS: AtomicU64 = AtomicU64::new(0);

fn timestamp_ns() -> u64 {
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
}

impl Drop for Cycle {
    fn drop(&mut self) {
        if !self.completed {
            // An aborted collection has no completed work to publish. Keep its
            // epoch consumed while allowing the next elected collector to start.
            let mut telemetry = TELEMETRY.lock().unwrap_or_else(|p| p.into_inner());
            telemetry.stats.phase = 0;
        }
    }
}

impl Cycle {
    /// Called only by the elected collector, before requesting safepoints.
    pub(crate) fn begin(kind: CycleKind) -> Self {
        let start_ns = timestamp_ns();
        let mut telemetry = TELEMETRY.lock().unwrap_or_else(|p| p.into_inner());
        let stats = &mut telemetry.stats;
        debug_assert_eq!(stats.phase, 0);
        stats.epoch = stats.epoch.saturating_add(1);
        stats.phase = kind as u32;
        Self {
            kind,
            epoch: stats.epoch,
            start_ns,
            completed: false,
        }
    }
    /// Called after resuming mutators; returns the trace record for publication
    /// after releasing collector election. `before/after` were sampled under STW.
    pub(crate) fn finish(self, before: u64, after: u64, work: MarkWork) -> GcCycleV1 {
        self.finish_metrics(before, after, work, None, None)
    }
    /// Concurrent cycles measure only the initial and remark stops as pauses;
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
        TELEMETRY
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .complete(cycle);
        self.completed = true;
        cycle
    }
}
impl GcTelemetry {
    fn complete(&mut self, cycle: GcCycleV1) {
        let s = &mut self.stats;
        s.phase = 0;
        s.epoch = cycle.epoch;
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
pub extern "C" fn willow_gc_stats_size(version: i64) -> i64 {
    if version == i64::from(GC_STATS_VERSION) {
        std::mem::size_of::<WillowGcStatsV1>() as i64
    } else {
        -1
    }
}

/// Returns bytes written, 0 for insufficient length, or -1 for an unknown
/// version or null output. Errors never write. On success the caller supplies
/// writable storage for the required size; alignment is not required.
#[unsafe(no_mangle)]
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
    let stats = snapshot_with_resident();
    // SAFETY: the caller supplies writable storage of at least `size` bytes.
    unsafe { out.cast::<WillowGcStatsV1>().write_unaligned(stats) };
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
                .and_then(|()| writer.flush())
                .is_err()
        {
            trace_error();
            // A broken sink must not repeatedly penalize collection.
            self.writer = None;
        }
    }
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
pub(crate) fn emit_cycle(cycle: GcCycleV1) {
    if let Some(sink) = &*TRACE {
        sink.lock().unwrap_or_else(|p| p.into_inner()).cycle(cycle);
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
}

#[cfg(test)]
#[path = "gc_telemetry_tests.rs"]
mod tests;
