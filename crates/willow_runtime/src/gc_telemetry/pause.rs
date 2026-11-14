//! Exclusive stop components. Unlike V3 overlapping spans these sum to the
//! request-to-release interval. Existing C snapshot layouts are unchanged.
use super::{GcLatencyV1, timestamp_ns};
use std::cell::Cell;
use std::sync::Mutex;

pub const COMPONENTS: usize = 6;
#[repr(usize)]
#[derive(Clone, Copy, Debug)]
pub enum Component {
    Rendezvous,
    Roots,
    Remembered,
    Evacuation,
    Metadata,
    Wakeup,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    pub sequence: u64,
    pub reason: u32,
    /// 0 mixed/full recovery, 1 young, 2 old-cycle coordination.
    pub generation: u32,
    pub aborted: bool,
    pub ns: [u64; COMPONENTS],
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Initial mark, remark, minor, recovery. Generation is determined by
    /// stop purpose, not by the age of every object visited within the stop.
    pub by_reason: [[GcLatencyV1; COMPONENTS]; 4],
    pub last: Sample,
    /// Keep component attribution for the longest complete interval per cause.
    pub longest: [Sample; 4],
    /// Saturated totals must not be used as acceptance evidence.
    pub saturated: bool,
}
pub(crate) struct PauseTelemetry {
    stats: Mutex<Option<Stats>>,
}
impl PauseTelemetry {
    pub(crate) const fn new() -> Self {
        Self {
            stats: Mutex::new(None),
        }
    }
    fn reset(&self) {
        if ACTIVE
            .get()
            .is_some_and(|active| std::ptr::eq(active.owner, self))
        {
            ACTIVE.set(None);
        }
        *self.stats.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
    fn snapshot(&self) -> Stats {
        self.stats
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .unwrap_or_default()
    }
}
fn default_owner() -> &'static PauseTelemetry {
    &super::telemetry().pause
}

#[derive(Clone, Copy)]
struct Active {
    // Identity only; never dereferenced. StopMeasurement borrows the owner.
    owner: *const PauseTelemetry,
    sample: Sample,
    component: Component,
    since: u64,
}
impl Active {
    fn switch(&mut self, next: Component, now: u64) -> Component {
        let previous = self.component;
        self.sample.ns[previous as usize] =
            self.sample.ns[previous as usize].saturating_add(now.saturating_sub(self.since));
        self.component = next;
        self.since = now;
        previous
    }
}
thread_local! {
    static ACTIVE: Cell<Option<Active>> = const { Cell::new(None) };
}

impl PauseTelemetry {
    pub(crate) fn begin(&self, sequence: u64, reason: u32, now: u64) {
        ACTIVE.set(Some(Active {
            owner: self,
            sample: Sample {
                sequence,
                reason,
                generation: match reason {
                    3 => 1,
                    4 => 0,
                    _ => 2,
                },
                ..Default::default()
            },
            component: Component::Rendezvous,
            since: now,
        }));
    }
}
fn switch(owner: *const PauseTelemetry, component: Component) {
    ACTIVE.with(|cell| {
        cell.set({
            let active = cell.get();
            active.map(|mut a| {
                if a.owner == owner {
                    a.switch(component, timestamp_ns());
                }
                a
            })
        })
    });
}
impl PauseTelemetry {
    pub(crate) fn switch(&self, component: Component) {
        switch(self, component);
    }

    pub(crate) fn finish(&self, now: u64, aborted: bool) {
        if !ACTIVE
            .get()
            .is_some_and(|active| std::ptr::eq(active.owner, self))
        {
            return;
        }
        if let Some(mut a) = ACTIVE.take() {
            a.switch(Component::Metadata, now);
            a.sample.aborted = aborted;
            let mut guard = self.stats.lock().unwrap_or_else(|p| p.into_inner());
            let stats = guard.get_or_insert_with(Stats::default);
            for (histogram, ns) in stats.by_reason[a.sample.reason as usize - 1]
                .iter_mut()
                .zip(a.sample.ns)
            {
                stats.saturated |=
                    histogram.count == u64::MAX || histogram.total_ns.checked_add(ns).is_none();
                histogram.record(ns);
            }
            let longest = &mut stats.longest[a.sample.reason as usize - 1];
            let total = |sample: &Sample| sample.ns.iter().copied().fold(0, u64::saturating_add);
            if longest.sequence == 0 || total(&a.sample) > total(longest) {
                *longest = a.sample;
            }
            stats.last = a.sample;
        }
    }
}
pub fn snapshot() -> Stats {
    default_owner().snapshot()
}
pub(crate) fn reset() {
    default_owner().reset();
}

/// Constant work per phase, no per-object clocks, allocations or shared locks.
/// The borrow keeps owner identity alive; the marker keeps TLS restoration on
/// the creating thread.
pub(crate) struct Span<'a>(
    Option<(&'a PauseTelemetry, Component)>,
    std::marker::PhantomData<std::rc::Rc<()>>,
);
impl<'a> Span<'a> {
    pub(crate) fn new(owner: &'a PauseTelemetry, component: Component) -> Self {
        let mut previous = None;
        ACTIVE.with(|cell| {
            cell.set({
                let active = cell.get();
                active.map(|mut a| {
                    if std::ptr::eq(a.owner, owner) {
                        previous = Some((owner, a.switch(component, timestamp_ns())));
                    }
                    a
                })
            })
        });
        Self(previous, std::marker::PhantomData)
    }
}
impl Drop for Span<'_> {
    fn drop(&mut self) {
        if let Some((owner, previous)) = self.0 {
            switch(owner, previous);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_pause_span_does_not_switch_another_owner() {
        let first = PauseTelemetry::new();
        let second = PauseTelemetry::new();
        first.begin(1, 3, timestamp_ns());
        let span = Span::new(&first, Component::Roots);
        second.begin(1, 3, timestamp_ns());
        drop(span);
        assert_eq!(
            ACTIVE.get().unwrap().component as usize,
            Component::Rendezvous as usize
        );
        second.finish(timestamp_ns(), false);
    }

    #[test]
    fn foreign_pause_finish_and_reset_preserve_active_owner() {
        let first = PauseTelemetry::new();
        let second = PauseTelemetry::new();
        first.begin(7, 3, 10);
        let foreign = Span::new(&second, Component::Roots);
        drop(foreign);
        second.switch(Component::Wakeup);
        assert_eq!(
            ACTIVE.get().unwrap().component as usize,
            Component::Rendezvous as usize
        );
        second.finish(20, true);
        second.reset();
        assert_eq!(second.snapshot().last.sequence, 0);
        first.finish(30, false);
        let sample = first.snapshot().last;
        assert_eq!(sample.sequence, 7);
        assert_eq!(sample.ns.iter().sum::<u64>(), 20);
        assert!(!sample.aborted);
        first.begin(8, 3, 40);
        first.reset();
        first.finish(50, false);
        assert_eq!(first.snapshot().last.sequence, 0);
    }

    #[test]
    fn pause_owners_keep_concurrent_same_sequence_components_and_reset_independent() {
        let first = PauseTelemetry::new();
        let second = PauseTelemetry::new();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [(&first, 1), (&second, 3)]
                .into_iter()
                .map(|(owner, duration)| {
                    let barrier = &barrier;
                    scope.spawn(crate::runtime::serving(move || {
                        for sequence in 1..=64 {
                            owner.begin(sequence, 3, sequence * 100);
                            barrier.wait();
                            owner.finish(sequence * 100 + duration, sequence % 2 == 0);
                            barrier.wait();
                            let stats = owner.snapshot();
                            assert_eq!(stats.last.sequence, sequence);
                            assert_eq!(stats.last.ns[0], duration);
                            assert_eq!(stats.by_reason[2][0].count, sequence);
                            assert_eq!(stats.by_reason[2][0].total_ns, sequence * duration);
                        }
                        assert!(ACTIVE.get().is_none());
                    }))
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        first.reset();
        assert_eq!(first.snapshot().by_reason[2][0].count, 0);
        assert_eq!(second.snapshot().by_reason[2][0].count, 64);
        assert_eq!(second.snapshot().by_reason[2][0].total_ns, 192);
    }

    #[test]
    fn exclusive_components_partition_time_without_double_counting() {
        let owner = PauseTelemetry::new();
        let mut a = Active {
            owner: &owner,
            sample: Sample::default(),
            component: Component::Rendezvous,
            since: 100,
        };
        a.switch(Component::Roots, 110);
        let parent = a.switch(Component::Remembered, 130);
        a.switch(parent, 135);
        a.switch(Component::Evacuation, 145);
        a.switch(Component::Metadata, 170);
        a.switch(Component::Wakeup, 190);
        a.switch(Component::Metadata, 200);
        assert_eq!(a.sample.ns, [10, 30, 5, 25, 20, 10]);
        assert_eq!(a.sample.ns.iter().sum::<u64>(), 100);
    }

    #[test]
    fn cause_generation_histograms_and_aborts_are_preserved() {
        let owner = PauseTelemetry::new();
        for reason in 1..=4 {
            owner.begin(u64::from(reason), reason, 10);
            owner.finish(30, reason == 4);
            let s = owner.snapshot();
            assert_eq!(
                s.last.generation,
                match reason {
                    3 => 1,
                    4 => 0,
                    _ => 2,
                }
            );
            assert_eq!(s.last.aborted, reason == 4);
            assert_eq!(s.last.ns, [20, 0, 0, 0, 0, 0]);
        }
        for cause in owner.snapshot().by_reason {
            assert!(cause.iter().all(|h| h.count == 1));
            assert_eq!(cause[0].total_ns, 20);
            assert_eq!(cause[0].max_ns, 20);
            assert_eq!(cause[0].buckets.iter().sum::<u64>(), 1);
        }
        owner.begin(5, 3, 10);
        owner.finish(20, false);
        assert_eq!(owner.snapshot().longest[2].sequence, 3);
        owner.stats.lock().unwrap().as_mut().unwrap().by_reason[2][0].total_ns = u64::MAX;
        owner.begin(6, 3, 10);
        owner.finish(40, false);
        assert!(owner.snapshot().saturated);
        assert_eq!(owner.snapshot().longest[2].sequence, 6);
        owner.reset();
        assert!(
            owner
                .snapshot()
                .by_reason
                .iter()
                .flatten()
                .all(|h| h.count == 0)
        );
    }
}
