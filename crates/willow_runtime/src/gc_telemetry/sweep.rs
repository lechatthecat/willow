//! Completed major sweep work; includes foreground and allocation-assisted work.
//! Counts visits (including live objects); time includes waits, drops and release.
use std::sync::Mutex;
#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    pub objects: u64,
    pub bytes: u64,
    pub wall_ns: u64,
    pub reclaimed_bytes: u64,
}
pub(crate) struct SweepTelemetry {
    stats: Mutex<Stats>,
}
impl SweepTelemetry {
    pub(crate) const fn new() -> Self {
        Self {
            stats: Mutex::new(Stats {
                objects: 0,
                bytes: 0,
                wall_ns: 0,
                reclaimed_bytes: 0,
            }),
        }
    }
    pub(crate) fn snapshot(&self) -> Stats {
        *self.stats.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub(crate) fn reset(&self) {
        *self.stats.lock().unwrap_or_else(|p| p.into_inner()) = Stats::default();
    }
    pub(crate) fn completed(&self, objects: u64, bytes: u64, wall_ns: u64, reclaimed_bytes: u64) {
        let mut s = self.stats.lock().unwrap_or_else(|p| p.into_inner());
        s.objects = s.objects.saturating_add(objects);
        s.bytes = s.bytes.saturating_add(bytes);
        s.wall_ns = s.wall_ns.saturating_add(wall_ns);
        s.reclaimed_bytes = s.reclaimed_bytes.saturating_add(reclaimed_bytes);
    }
}
fn default_owner() -> &'static SweepTelemetry {
    &super::telemetry().sweep
}
pub fn snapshot() -> Stats {
    default_owner().snapshot()
}
pub(crate) fn reset() {
    default_owner().reset();
}
pub(crate) fn completed(objects: u64, bytes: u64, wall_ns: u64, reclaimed_bytes: u64) {
    default_owner().completed(objects, bytes, wall_ns, reclaimed_bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sweep_owners_keep_parallel_publications_reset_and_saturation_independent() {
        let first = SweepTelemetry::new();
        let second = SweepTelemetry::new();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [(&first, 1), (&second, 3)]
                .into_iter()
                .map(|(owner, stride)| {
                    let barrier = &barrier;
                    scope.spawn(crate::runtime::serving(move || {
                        for round in 1..=64 {
                            owner.completed(stride, 2 * stride, 3 * stride, 4 * stride);
                            barrier.wait();
                            let s = owner.snapshot();
                            assert_eq!(
                                (s.objects, s.bytes, s.wall_ns, s.reclaimed_bytes),
                                (
                                    round * stride,
                                    round * 2 * stride,
                                    round * 3 * stride,
                                    round * 4 * stride
                                )
                            );
                            barrier.wait();
                        }
                    }))
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
        });
        first.reset();
        assert_eq!(first.snapshot().objects, 0);
        assert_eq!(second.snapshot().objects, 192);
        first.completed(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        first.completed(1, 1, 1, 1);
        let s = first.snapshot();
        assert_eq!(
            (s.objects, s.bytes, s.wall_ns, s.reclaimed_bytes),
            (u64::MAX, u64::MAX, u64::MAX, u64::MAX)
        );
        assert_eq!(second.snapshot().reclaimed_bytes, 768);
    }
}
