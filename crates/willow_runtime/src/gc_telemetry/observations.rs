//! Opt-in V4 interval transport. The mutex never nests a GC/coordination lock
//! acquisition or performs I/O. Fixed retained history bounds memory under stalls.
use super::{GcLatencyV1, timestamp_ns};
use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicU64, Ordering},
};

pub const KINDS: usize = 9;
pub const CAPACITY: usize = 4096;
/// Opt-in upper bound, including acceptance runs. Producers never block on I/O.
pub const MAX_CAPACITY: usize = 524_288;
#[cfg(test)]
fn configured_capacity(value: Option<&str>) -> Option<usize> {
    super::bounded_capacity(value, CAPACITY, MAX_CAPACITY)
}
/// Preallocated event storage, excluding the bounded drained batch held by a sink.
pub fn transport_capacity() -> usize {
    let s = STATE.lock().unwrap_or_else(|p| p.into_inner());
    if s.discard { 0 } else { s.capacity }
}
pub const ENABLED: u64 = 1;
pub const LOST: u64 = 2;
pub const SATURATED: u64 = 4;
pub const INVALID: u64 = 8;
/// `WILLOW_GC_TRACE_CAPACITY` was unusable; the default capacity is kept.
pub const CONFIG_INVALID: u64 = 16;
/// These are overlapping intervals, never additive components of wall time.
#[repr(u64)]
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Handshake,
    Assist,
    AllocationStall,
    BarrierSlow,
    HeapLock,
    Roots,
    Evacuation,
    Resume,
    Collector,
    /// Event-only extension: explicit GC/lifecycle joins, distinct from allocation
    /// stalls. V4's nine histogram slots stay ABI-compatible.
    CycleWait,
    /// Complete collector cycle elapsed time; overlaps stopped/concurrent work.
    Cycle,
    /// Coordination mutex acquisition, including registration and cycle joins.
    CoordLock,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Work {
    pub objects: u64,
    pub slots: u64,
    pub bytes: u64,
    pub metadata_bytes: u64,
    pub budget_checks: u64,
    pub yields: u64,
    pub restarts: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub flags: u64,
    pub sequence: u64,
    pub dropped: u64,
    pub active_intervals: u64,
    pub registered: u64,
    pub unregistered: u64,
    pub progress: u64,
    pub intervals: [GcLatencyV1; KINDS],
    /// Initial, remark, minor, recovery, completed major, completed minor.
    pub work: [Work; 6],
}
/// event: 1 begin, 2 end, 3 register, 4 unregister, 6 work. Safepoint progress
/// (5) is a checkpoint counter only: per-poll events would flood the history.
/// Operation 7 records a completed concurrent graph batch.
/// identity pairs begin/end; thread is an opaque process-lifetime identity.
#[derive(Clone, Copy, Debug, Default)]
pub struct Event {
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub epoch: u64,
    pub thread: u64,
    /// 0 application mutator, 1 background GC marker. Not part of the C ABI.
    pub role: u64,
    pub event: u64,
    pub kind: u64,
    pub identity: u64,
    pub aborted: u64,
    pub progress: u64,
    pub work: Work,
}
/// `discard` keeps aggregate stats but retains no per-event history when no V4
/// trace sink will ever drain it, so measurement-only runs never report loss.
struct State {
    stats: Stats,
    events: VecDeque<Event>,
    discard: bool,
    capacity: usize,
}
impl Default for State {
    fn default() -> Self {
        Self {
            stats: Stats::default(),
            events: VecDeque::new(),
            discard: false,
            capacity: CAPACITY,
        }
    }
}
fn add(value: &mut u64, n: u64, flags: &mut u64) {
    if let Some(v) = value.checked_add(n) {
        *value = v;
    } else {
        *value = u64::MAX;
        *flags |= SATURATED;
    }
}
impl State {
    fn event(&mut self, mut e: Event) -> u64 {
        if matches!(e.event, 6 | 7)
            && [
                e.work.objects,
                e.work.slots,
                e.work.bytes,
                e.work.metadata_bytes,
                e.work.budget_checks,
                e.work.yields,
                e.work.restarts,
            ]
            .contains(&u64::MAX)
        {
            self.stats.flags |= SATURATED;
        }
        add(&mut self.stats.sequence, 1, &mut self.stats.flags);
        e.sequence = self.stats.sequence;
        if self.discard {
            return e.sequence;
        }
        if self.events.len() == self.capacity {
            self.events.pop_front();
            add(&mut self.stats.dropped, 1, &mut self.stats.flags);
            self.stats.flags |= LOST;
        }
        self.events.push_back(e);
        e.sequence
    }
    fn begin(&mut self, e: Event) -> u64 {
        add(&mut self.stats.active_intervals, 1, &mut self.stats.flags);
        self.event(e)
    }
    fn end(&mut self, e: Event, start: u64) {
        if self.stats.active_intervals == 0 || e.timestamp_ns < start || e.aborted != 0 {
            self.stats.flags |= INVALID;
        }
        self.stats.active_intervals = self.stats.active_intervals.saturating_sub(1);
        if let Some(h) = self.stats.intervals.get_mut(e.kind as usize) {
            let ns = e.timestamp_ns.saturating_sub(start);
            if h.count == u64::MAX || h.total_ns.checked_add(ns).is_none() {
                self.stats.flags |= SATURATED;
            }
            h.record(ns);
        }
        self.event(e);
    }
    fn work(&mut self, e: Event) {
        let w = &mut self.stats.work[e.kind as usize];
        for (a, b) in [
            (&mut w.objects, e.work.objects),
            (&mut w.slots, e.work.slots),
            (&mut w.bytes, e.work.bytes),
            (&mut w.metadata_bytes, e.work.metadata_bytes),
            (&mut w.budget_checks, e.work.budget_checks),
            (&mut w.yields, e.work.yields),
            (&mut w.restarts, e.work.restarts),
        ] {
            add(a, b, &mut self.stats.flags);
        }
        self.event(e);
    }
}
// Initialized before the mutex is first taken, so opening the sink is not I/O
// under the observation lock.
static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    let history = super::observation_history();
    let configured = super::env_capacity("WILLOW_GC_TRACE_CAPACITY", CAPACITY, MAX_CAPACITY);
    let capacity = configured.unwrap_or(CAPACITY);
    Mutex::new(State {
        stats: Stats {
            flags: if enabled() {
                ENABLED
                    | if configured.is_none() {
                        CONFIG_INVALID
                    } else {
                        0
                    }
            } else {
                0
            },
            ..Default::default()
        },
        events: if history {
            VecDeque::with_capacity(capacity)
        } else {
            VecDeque::new()
        },
        discard: !history,
        capacity,
    })
});
/// Safepoint polls are counted without the observation mutex.
static PROGRESS: AtomicU64 = AtomicU64::new(0);
static EPOCH: AtomicU64 = AtomicU64::new(0);
static THREAD: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static ROLE: Cell<u64> = const { Cell::new(0) };
    static ID: Cell<u64> = const { Cell::new(0) };
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
    static LOCAL_PROGRESS: Cell<u64> = const { Cell::new(0) };
    static STOP_PHASE: Cell<Option<usize>> = const { Cell::new(None) };
    static PENDING_PROGRESS: Cell<u64> = const { Cell::new(0) };
}
pub fn enabled() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| {
        std::env::var("WILLOW_GC_MEASURE").as_deref() == Ok("1")
            || std::env::var("WILLOW_GC_TRACE_VERSION").as_deref() == Ok("4")
    });
    *ON
}
fn thread() -> u64 {
    ID.with(|id| {
        if id.get() == 0 {
            // Saturation is explicit; do not reuse a thread identity.
            let next =
                THREAD.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1));
            match next {
                Ok(n) => id.set(n),
                Err(_) => {
                    STATE.lock().unwrap_or_else(|p| p.into_inner()).stats.flags |= SATURATED;
                    id.set(u64::MAX);
                }
            }
        }
        id.get()
    })
}
fn event(event: u64, kind: u64) -> Event {
    Event {
        event,
        kind,
        timestamp_ns: timestamp_ns(),
        epoch: EPOCH.load(Ordering::Relaxed),
        thread: thread(),
        role: ROLE.get(),
        progress: LOCAL_PROGRESS.get(),
        ..Default::default()
    }
}
pub(crate) fn epoch(epoch: u64) -> u64 {
    EPOCH.swap(epoch, Ordering::Relaxed)
}
fn checkpoint(s: &State) -> Stats {
    let mut stats = s.stats;
    stats.progress = PROGRESS.load(Ordering::Relaxed);
    if stats.progress == u64::MAX {
        stats.flags |= SATURATED;
    }
    stats
}
pub fn snapshot() -> Stats {
    checkpoint(&STATE.lock().unwrap_or_else(|p| p.into_inner()))
}
/// Drain and checkpoint atomically. A consumer must retain the previous sequence
/// and reject loss, gaps, active intervals at finalization and truncated output.
pub fn take_events() -> (Vec<Event>, Stats) {
    let mut s = STATE.lock().unwrap_or_else(|p| p.into_inner());
    (s.events.drain(..).collect(), checkpoint(&s))
}
/// Set before a marker thread registers; retain through its final unregister.
/// Application threads doing Assist or elected collection keep role 0.
pub(crate) struct MarkerRole(u64);
impl MarkerRole {
    pub(crate) fn new() -> Self {
        Self(ROLE.replace(1))
    }
}
impl Drop for MarkerRole {
    fn drop(&mut self) {
        ROLE.set(self.0);
    }
}
pub(crate) fn register() {
    if !enabled() || REGISTERED.replace(true) {
        return;
    }
    let e = event(3, 0);
    let mut s = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let State { stats, .. } = &mut *s;
    add(&mut stats.registered, 1, &mut stats.flags);
    s.event(e);
}
pub(crate) fn unregister() {
    if !enabled() || !REGISTERED.replace(false) {
        return;
    }
    flush_progress();
    let e = event(4, 0);
    let mut s = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let State { stats, .. } = &mut *s;
    add(&mut stats.unregistered, 1, &mut stats.flags);
    s.event(e);
}
pub(crate) fn progress() {
    if !enabled() || !REGISTERED.get() {
        return;
    }
    LOCAL_PROGRESS.set(LOCAL_PROGRESS.get().saturating_add(1));
    let pending = PENDING_PROGRESS.get() + 1;
    PENDING_PROGRESS.set(pending);
    if pending == 256 {
        flush_progress();
    }
}
fn accumulate_progress(counter: &AtomicU64, increment: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        Some(n.saturating_add(increment))
    });
}
fn flush_progress() {
    let pending = PENDING_PROGRESS.replace(0);
    if pending != 0 {
        accumulate_progress(&PROGRESS, pending);
    }
}

pub(crate) fn work(kind: usize, work: Work) {
    if !enabled() {
        return;
    }
    let mut e = event(6, kind as u64);
    e.work = work;
    STATE.lock().unwrap_or_else(|p| p.into_inner()).work(e);
}
/// Collector-local attribution; worker threads never inherit another thread's
/// stopped phase. Restoration is unwind-safe and supports nested scopes.
pub(crate) struct StopPhase(Option<usize>);
impl StopPhase {
    pub(crate) fn new(reason: usize) -> Self {
        Self(STOP_PHASE.replace(Some(reason)))
    }
}
impl Drop for StopPhase {
    fn drop(&mut self) {
        STOP_PHASE.set(self.0);
    }
}
pub(crate) fn graph_work(mark: super::MarkWork) {
    if !enabled() {
        return;
    }
    let w = Work {
        objects: mark.objects,
        slots: mark.scanned_bytes / 8,
        bytes: mark.marked_bytes,
        metadata_bytes: mark.descriptor_bytes,
        ..Default::default()
    };
    if let Some(phase) = STOP_PHASE.get() {
        work(phase, w);
    } else {
        // Completed concurrent batch event. V4 cumulative completed-cycle groups
        // remain unchanged; consumers can sum these separately from stopped work.
        let mut e = event(7, 0);
        e.work = w;
        STATE.lock().unwrap_or_else(|p| p.into_inner()).event(e);
    }
}
/// RAII interval endpoints survive Rust unwinding; no trace I/O on Drop.
pub struct Span(Option<Event>);
impl Span {
    pub(crate) fn abort(&mut self) {
        if let Some(event) = &mut self.0 {
            event.aborted = 1;
        }
    }
    pub fn new(kind: Kind) -> Self {
        if !enabled() {
            return Self(None);
        }
        flush_progress();
        let mut e = event(1, kind as u64);
        e.identity = STATE.lock().unwrap_or_else(|p| p.into_inner()).begin(e);
        Self(Some(e))
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if let Some(mut e) = self.0 {
            let start = e.timestamp_ns;
            e.event = 2;
            e.timestamp_ns = timestamp_ns();
            e.aborted |= u64::from(std::thread::panicking());
            STATE
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .end(e, start);
        }
    }
}
/// Instrument blocking GC mutex acquisition only. Deref preserves existing
/// try-lock diagnostics and reset poison recovery without recursive telemetry.
pub(crate) struct GcMutex<T>(Mutex<T>, Kind);
impl<T> GcMutex<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Mutex::new(value), Kind::HeapLock)
    }
    pub(crate) fn coordination(value: T) -> Self {
        Self(Mutex::new(value), Kind::CoordLock)
    }
    pub(crate) fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, T>> {
        let _span = Span::new(self.1);
        self.0.lock()
    }
}
impl<T> std::ops::Deref for GcMutex<T> {
    type Target = Mutex<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub(super) fn write(
    writer: &mut dyn std::io::Write,
    events: &[Event],
    s: Stats,
) -> std::io::Result<()> {
    for e in events {
        writeln!(
            writer,
            "{{\"version\":4,\"event\":\"gc_observation\",\"sequence\":{},\"timestamp_ns\":{},\"epoch\":{},\"thread\":{},\"role\":{},\"op\":{},\"kind\":{},\"identity\":{},\"aborted\":{},\"progress\":{},\"objects\":{},\"slots\":{},\"bytes\":{},\"metadata_bytes\":{},\"budget_checks\":{},\"yields\":{},\"restarts\":{}}}",
            e.sequence,
            e.timestamp_ns,
            e.epoch,
            e.thread,
            e.role,
            e.event,
            e.kind,
            e.identity,
            e.aborted,
            e.progress,
            e.work.objects,
            e.work.slots,
            e.work.bytes,
            e.work.metadata_bytes,
            e.work.budget_checks,
            e.work.yields,
            e.work.restarts
        )?;
    }
    writeln!(
        writer,
        "{{\"version\":4,\"event\":\"gc_observation_checkpoint\",\"sequence\":{},\"flags\":{},\"dropped\":{},\"active\":{},\"registered\":{},\"unregistered\":{},\"progress\":{}}}",
        s.sequence,
        s.flags,
        s.dropped,
        s.active_intervals,
        s.registered,
        s.unregistered,
        s.progress
    )
}

#[cfg(test)]
mod tests;
