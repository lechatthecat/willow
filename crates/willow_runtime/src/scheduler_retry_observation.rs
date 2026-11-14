//! Queue-owned retry sampling; disabled entirely in production builds.
pub(super) trait RetrySample {
    fn retry(&mut self) {}
}
pub(super) trait RetryObservation {
    type Sample<'a>: RetrySample
    where
        Self: 'a;
    fn begin(&self, kind: usize) -> Self::Sample<'_>;
}
#[cfg(not(test))]
#[derive(Debug, Default, Clone)]
pub(super) struct RuntimeRetryObservation {
    _disabled: (),
}
#[cfg(not(test))]
pub(super) struct NoRetrySample;
#[cfg(not(test))]
impl RetrySample for NoRetrySample {}
#[cfg(not(test))]
impl RetryObservation for RuntimeRetryObservation {
    type Sample<'a> = NoRetrySample;
    fn begin(&self, _kind: usize) -> NoRetrySample {
        NoRetrySample
    }
}
#[cfg(all(test, target_os = "linux"))]
pub(super) use tests::Distribution;
#[cfg(test)]
pub(super) use tests::Observation as RuntimeRetryObservation;
#[cfg(test)]
mod tests {
    use super::{RetryObservation, RetrySample};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::thread::{ThreadId, current};
    #[derive(Clone, Debug)]
    pub(crate) struct Distribution {
        pub(crate) bins: [u64; 256],
        pub(crate) retries: u64,
        pub(crate) max: usize,
    }
    impl Default for Distribution {
        fn default() -> Self {
            Self {
                bins: [0; 256],
                retries: 0,
                max: 0,
            }
        }
    }
    impl Distribution {
        fn record(&mut self, retries: usize) {
            self.bins[retries.min(255)] += 1;
            self.retries += retries as u64;
            self.max = self.max.max(retries);
        }
        #[cfg(target_os = "linux")]
        pub(crate) fn merge(&mut self, other: &Self) {
            for (a, b) in self.bins.iter_mut().zip(other.bins) {
                *a += b;
            }
            self.retries += other.retries;
            self.max = self.max.max(other.max);
        }
        #[cfg(target_os = "linux")]
        pub(crate) fn report(&self, label: &str) {
            let calls: u64 = self.bins.iter().sum();
            let rank = (calls * 99).div_ceil(100);
            let mut sum = 0;
            let p99 = self
                .bins
                .iter()
                .position(|n| {
                    sum += n;
                    sum >= rank
                })
                .unwrap();
            println!(
                "retry kind={label} calls={calls} retries={} p99={p99} max={} overflow_ge255={}",
                self.retries, self.max, self.bins[255]
            );
        }
    }

    #[derive(Debug, Default, Clone)]
    pub(crate) struct Observation(Arc<Mutex<HashMap<ThreadId, [Distribution; 2]>>>);
    impl Observation {
        pub(crate) fn enable_current(&self) {
            self.0
                .lock()
                .unwrap()
                .insert(current().id(), Default::default());
        }
        pub(crate) fn take_current(&self) -> [Distribution; 2] {
            self.0.lock().unwrap().remove(&current().id()).unwrap()
        }
    }
    pub(crate) struct Sample<'a> {
        owner: &'a Observation,
        kind: usize,
        retries: usize,
    }
    impl RetrySample for Sample<'_> {
        fn retry(&mut self) {
            self.retries += 1;
        }
    }
    impl Drop for Sample<'_> {
        fn drop(&mut self) {
            if let Some(stats) = self.owner.0.lock().unwrap().get_mut(&current().id()) {
                stats[self.kind].record(self.retries);
            }
        }
    }
    impl RetryObservation for Observation {
        type Sample<'a> = Sample<'a>;
        fn begin(&self, kind: usize) -> Sample<'_> {
            Sample {
                owner: self,
                kind,
                retries: 0,
            }
        }
    }
}
