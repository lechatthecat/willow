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
pub const ENABLED: u64 = 1;
pub const LOST: u64 = 2;
pub const SATURATED: u64 = 4;
pub const INVALID: u64 = 8;
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
/// identity pairs begin/end; thread is an opaque process-lifetime identity.
#[derive(Clone, Copy, Debug, Default)]
pub struct Event {
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub epoch: u64,
    pub thread: u64,
    pub event: u64,
    pub kind: u64,
    pub identity: u64,
    pub aborted: u64,
    pub work: Work,
}
/// `discard` keeps aggregate stats but retains no per-event history when no V4
/// trace sink will ever drain it, so measurement-only runs never report loss.
#[derive(Default)]
struct State {
    stats: Stats,
    events: VecDeque<Event>,
    discard: bool,
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
        add(&mut self.stats.sequence, 1, &mut self.stats.flags);
        e.sequence = self.stats.sequence;
        if self.discard {
            return e.sequence;
        }
        if self.events.len() == CAPACITY {
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
        if self.stats.active_intervals == 0 || e.timestamp_ns < start {
            self.stats.flags |= INVALID;
        }
        self.stats.active_intervals = self.stats.active_intervals.saturating_sub(1);
        let h = &mut self.stats.intervals[e.kind as usize];
        let ns = e.timestamp_ns.saturating_sub(start);
        if h.count == u64::MAX || h.total_ns.checked_add(ns).is_none() {
            self.stats.flags |= SATURATED;
        }
        h.record(ns);
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
    Mutex::new(State {
        stats: Stats {
            flags: if enabled() { ENABLED } else { 0 },
            ..Default::default()
        },
        events: if history {
            VecDeque::with_capacity(CAPACITY)
        } else {
            VecDeque::new()
        },
        discard: !history,
    })
});
/// Safepoint polls are counted without the observation mutex.
static PROGRESS: AtomicU64 = AtomicU64::new(0);
static EPOCH: AtomicU64 = AtomicU64::new(0);
static THREAD: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static ID: Cell<u64> = const { Cell::new(0) };
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
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
        ..Default::default()
    }
}
pub(crate) fn epoch(epoch: u64) {
    EPOCH.store(epoch, Ordering::Relaxed);
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
    // Saturating without a CAS loop on the poll path; u64::MAX flags saturation.
    if PROGRESS.load(Ordering::Relaxed) != u64::MAX {
        PROGRESS.fetch_add(1, Ordering::Relaxed);
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
/// RAII interval endpoints survive Rust unwinding; no trace I/O on Drop.
pub struct Span(Option<Event>);
impl Span {
    pub fn new(kind: Kind) -> Self {
        if !enabled() {
            return Self(None);
        }
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
            e.aborted = u64::from(std::thread::panicking());
            STATE
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .end(e, start);
        }
    }
}
/// Instrument the blocking heap acquisition only. Deref preserves existing
/// try-lock diagnostics and reset poison recovery without recursive telemetry.
pub(crate) struct HeapMutex<T>(Mutex<T>);
impl<T> HeapMutex<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }
    pub(crate) fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, T>> {
        let _span = Span::new(Kind::HeapLock);
        self.0.lock()
    }
}
impl<T> std::ops::Deref for HeapMutex<T> {
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
            "{{\"version\":4,\"event\":\"gc_observation\",\"sequence\":{},\"timestamp_ns\":{},\"epoch\":{},\"thread\":{},\"op\":{},\"kind\":{},\"identity\":{},\"aborted\":{},\"objects\":{},\"slots\":{},\"bytes\":{},\"metadata_bytes\":{},\"budget_checks\":{},\"yields\":{},\"restarts\":{}}}",
            e.sequence,
            e.timestamp_ns,
            e.epoch,
            e.thread,
            e.event,
            e.kind,
            e.identity,
            e.aborted,
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
