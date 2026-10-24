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
static STATS: Mutex<Option<Stats>> = Mutex::new(None);

#[derive(Clone, Copy)]
struct Active {
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

pub(crate) fn begin(sequence: u64, reason: u32, now: u64) {
    ACTIVE.set(Some(Active {
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
pub(crate) fn switch(component: Component) {
    ACTIVE.with(|cell| {
        cell.set({
            let active = cell.get();
            active.map(|mut a| {
                a.switch(component, timestamp_ns());
                a
            })
        })
    });
}
pub(crate) fn finish(now: u64, aborted: bool) {
    if let Some(mut a) = ACTIVE.take() {
        a.switch(Component::Metadata, now);
        a.sample.aborted = aborted;
        let mut guard = STATS.lock().unwrap_or_else(|p| p.into_inner());
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
pub fn snapshot() -> Stats {
    STATS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .unwrap_or_default()
}
pub(crate) fn reset() {
    *STATS.lock().unwrap_or_else(|p| p.into_inner()) = None;
    ACTIVE.set(None);
}

/// Constant work per phase, no per-object clocks, allocations or shared locks.
pub(crate) struct Span(Option<Component>);
impl Span {
    pub(crate) fn new(component: Component) -> Self {
        let mut previous = None;
        ACTIVE.with(|cell| {
            cell.set({
                let active = cell.get();
                active.map(|mut a| {
                    previous = Some(a.switch(component, timestamp_ns()));
                    a
                })
            })
        });
        Self(previous)
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if let Some(previous) = self.0 {
            switch(previous);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_components_partition_time_without_double_counting() {
        let mut a = Active {
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
        let _guard = crate::gc::runtime_test_guard();
        reset();
        for reason in 1..=4 {
            begin(u64::from(reason), reason, 10);
            finish(30, reason == 4);
            let s = snapshot();
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
        for cause in snapshot().by_reason {
            assert!(cause.iter().all(|h| h.count == 1));
            assert_eq!(cause[0].total_ns, 20);
            assert_eq!(cause[0].max_ns, 20);
            assert_eq!(cause[0].buckets.iter().sum::<u64>(), 1);
        }
        begin(5, 3, 10);
        finish(20, false);
        assert_eq!(snapshot().longest[2].sequence, 3);
        STATS.lock().unwrap().as_mut().unwrap().by_reason[2][0].total_ns = u64::MAX;
        begin(6, 3, 10);
        finish(40, false);
        assert!(snapshot().saturated);
        assert_eq!(snapshot().longest[2].sequence, 6);
        reset();
        assert!(snapshot().by_reason.iter().flatten().all(|h| h.count == 0));
    }
}
