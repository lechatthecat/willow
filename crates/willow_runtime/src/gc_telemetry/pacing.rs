//! Rust diagnostic view, version 1; existing C snapshots retain their layouts.
//! Reasons are bit flags: cold=1, CPU missing=2, normal exceeded=4, hard
//! exceeded=8, predicted late=16, pressure=32, allocation failure=64, fixed=128,
//! uncontrolled non-background phases=256, missing memory observations=512.
//! Duty is not a process-wide CPU cap.
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, Default)]
pub struct Decision {
    pub goal_bytes: u64,
    pub trigger_bytes: u64,
    pub runway_bytes: u64,
    pub expected_work_bytes: u64,
    pub dedicated_workers: u64,
    pub fractional_duty_ppm: u64,
    pub assist_work_per_byte_ppm: u64,
    pub overshoot_budget_bytes: u64,
    pub reasons: u64,
    pub start_now: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub version: u32,
    pub publications: u64,
    pub decision: Decision,
    pub background_cpu_ns: u64,
    pub gc_cpu_ns: u64,
    pub cpu_missing_cycles: u64,
    pub window_start_ns: u64,
    pub window_end_ns: u64,
    pub window_cpu_ns: u64,
    pub window_cpu_missing: u64,
    pub duty_wait_ns: u64,
    pub missing_cpu_samples: u64,
    /// No opportunistic work is admitted by this controller.
    pub opportunistic_cpu_ns: u64,
}

#[derive(Clone, Copy, Default)]
struct Interval {
    start: u64,
    cpu: Option<u64>,
}
#[derive(Default)]
struct State {
    snapshot: Snapshot,
    window: [Interval; 8],
    cursor: usize,
    count: usize,
    cpu: u128,
    missing: u64,
    enclosing: Option<Interval>,
}
impl State {
    fn begin_major(&mut self, start: u64) {
        assert!(self.enclosing.is_none(), "overlapping major CPU intervals");
        self.enclosing = Some(Interval {
            start,
            cpu: Some(0),
        });
    }

    fn cycle(&mut self, start: u64, end: u64, cpu: Option<u64>, nested: bool) {
        let s = &mut self.snapshot;
        s.gc_cpu_ns = s.gc_cpu_ns.saturating_add(cpu.unwrap_or(0));
        s.cpu_missing_cycles = s
            .cpu_missing_cycles
            .saturating_add(u64::from(cpu.is_none()));
        if nested {
            let enclosing = self
                .enclosing
                .as_mut()
                .expect("nested CPU interval needs major");
            enclosing.cpu = enclosing.cpu.zip(cpu).and_then(|(a, b)| a.checked_add(b));
        } else if let Some(enclosing) = self.enclosing.take() {
            let cpu = enclosing.cpu.zip(cpu).and_then(|(a, b)| a.checked_add(b));
            self.insert(enclosing.start, end, cpu);
        } else {
            self.insert(start, end, cpu);
        }
    }

    fn abort(&mut self, start: u64, end: u64, nested: bool) {
        // Lost CPU must invalidate the entire enclosing interval, even if the
        // aborted child would otherwise have fallen out of the eight-entry ring.
        self.cycle(start, end, None, nested);
    }

    fn view(&self) -> Snapshot {
        let mut snapshot = self.snapshot;
        snapshot.version = 1;
        // No partial major interval is a valid completed CPU window. Keep the
        // previous completed window, explicitly invalid until the major ends.
        snapshot.window_cpu_missing += u64::from(self.enclosing.is_some());
        snapshot
    }

    fn insert(&mut self, start: u64, end: u64, cpu: Option<u64>) {
        let s = &mut self.snapshot;
        s.version = 1;
        let previous = self.window[self.cursor];
        if self.count == self.window.len() {
            self.cpu -= u128::from(previous.cpu.unwrap_or(0));
            self.missing -= u64::from(previous.cpu.is_none());
        } else {
            self.count += 1;
        }
        // Include inter-cycle mutator time. Each ring entry now covers a whole
        // top-level interval, including all nested CPU and its validity.
        let interval = Interval {
            start: if s.window_end_ns == 0 {
                start
            } else {
                s.window_end_ns.min(start)
            },
            cpu,
        };
        self.window[self.cursor] = interval;
        self.cursor = (self.cursor + 1) % self.window.len();
        self.cpu += u128::from(cpu.unwrap_or(0));
        self.missing += u64::from(cpu.is_none());
        s.window_start_ns = self.window[..self.count]
            .iter()
            .map(|sample| sample.start)
            .min()
            .unwrap_or(start);
        s.window_end_ns = end;
        s.window_cpu_ns = self.cpu.min(u128::from(u64::MAX)) as u64;
        s.window_cpu_missing = self.missing + u64::from(self.cpu > u128::from(u64::MAX));
    }
}
static STATS: Mutex<Option<State>> = Mutex::new(None);

pub fn snapshot() -> Snapshot {
    STATS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map_or(
            Snapshot {
                version: 1,
                ..Snapshot::default()
            },
            State::view,
        )
}
pub(crate) fn publish(decision: Decision) {
    let mut guard = STATS.lock().unwrap_or_else(|p| p.into_inner());
    let stats = &mut guard.get_or_insert_with(State::default).snapshot;
    stats.version = 1;
    stats.publications = stats.publications.saturating_add(1);
    stats.decision = decision;
}
pub(crate) fn duty(cpu: Option<u64>, wait: u64) {
    let mut guard = STATS.lock().unwrap_or_else(|p| p.into_inner());
    let stats = &mut guard.get_or_insert_with(State::default).snapshot;
    stats.version = 1;
    stats.background_cpu_ns = stats.background_cpu_ns.saturating_add(cpu.unwrap_or(0));
    stats.duty_wait_ns = stats.duty_wait_ns.saturating_add(wait);
    stats.missing_cpu_samples = stats
        .missing_cpu_samples
        .saturating_add(u64::from(cpu.is_none()));
}
pub(crate) fn begin_major(start: u64) {
    STATS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(State::default)
        .begin_major(start);
}
pub(crate) fn abort(start: u64, end: u64, nested: bool) {
    STATS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(State::default)
        .abort(start, end, nested);
}
pub(crate) fn cycle(start: u64, end: u64, cpu: Option<u64>, nested: bool) {
    STATS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(State::default)
        .cycle(start, end, cpu, nested);
}
pub(crate) fn reset() {
    *STATS.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_window_forgets_missing_and_step_history_in_eight_cycles() {
        let mut s = State::default();
        s.cycle(1, 10, None, false);
        for i in 1..=8 {
            s.cycle(i * 100, i * 100 + 10, Some(20), false);
        }
        assert_eq!(s.snapshot.window_cpu_ns, 160);
        assert_eq!(s.snapshot.window_cpu_missing, 0);
        assert_eq!(s.snapshot.window_start_ns, 10);
        for i in 9..=16 {
            s.cycle(i * 100, i * 100 + 10, Some(40), false);
        }
        assert_eq!(s.snapshot.window_cpu_ns, 320);
        assert_eq!(s.snapshot.window_start_ns, 810);
        for i in 17..=24 {
            s.cycle(i * 100, i * 100 + 10, Some(u64::MAX), false);
        }
        assert_eq!(s.snapshot.window_cpu_ns, u64::MAX);
        assert_eq!(s.snapshot.window_cpu_missing, 1);
    }
    #[test]
    fn nested_cpu_and_validity_are_retained_as_one_interval() {
        for children in [9, 64, 1024] {
            for missing in [None, Some(0), Some(8)] {
                let mut s = State::default();
                s.begin_major(100);
                for i in 0..children {
                    s.cycle(
                        100 + i * 10,
                        106 + i * 10,
                        (missing != Some(i)).then_some(6),
                        true,
                    );
                    assert_eq!(s.count, 0, "children never evict enclosing accounting");
                    assert_eq!(s.view().window_cpu_missing, 1);
                }
                s.cycle(100, 100 + (children + 1) * 10, Some(1), false);
                assert_eq!(s.count, 1);
                assert_eq!(s.view().window_start_ns, 100);
                assert_eq!(s.view().window_cpu_missing, u64::from(missing.is_some()));
                assert_eq!(
                    s.view().window_cpu_ns,
                    if missing.is_none() {
                        children * 6 + 1
                    } else {
                        0
                    }
                );
                for i in 0..7 {
                    s.cycle(20_000 + i * 10, 20_006 + i * 10, Some(1), false);
                }
                assert_eq!(s.view().window_cpu_missing, u64::from(missing.is_some()));
                s.cycle(30_000, 30_006, Some(1), false);
                assert_eq!(s.view().window_cpu_missing, 0);
                assert_eq!(s.view().window_cpu_ns, 8);
            }
        }
    }

    #[test]
    fn aborted_and_overflowing_nested_intervals_remain_invalid() {
        let mut s = State::default();
        s.begin_major(100);
        s.abort(101, 102, true);
        for i in 0..9 {
            s.cycle(110 + i * 8, 116 + i * 8, Some(6), true);
        }
        s.cycle(100, 200, Some(1), false);
        assert_eq!(s.view().window_cpu_missing, 1);
        s.begin_major(300);
        s.cycle(301, 310, Some(6), true);
        s.abort(300, 400, false);
        assert!(s.enclosing.is_none());
        assert_eq!(s.view().window_cpu_missing, 2);
        s.begin_major(500);
        s.cycle(501, 510, Some(u64::MAX), true);
        s.cycle(511, 520, Some(1), true);
        s.cycle(500, 600, Some(1), false);
        assert_eq!(s.view().window_cpu_missing, 3);
    }
}
