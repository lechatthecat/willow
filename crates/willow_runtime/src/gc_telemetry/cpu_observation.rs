//! Completed-cycle CPU input. Production keeps the measured value; injected
//! observations belong to a runtime and retain independent OS-thread scopes.

pub(crate) trait CpuObservation {
    fn completed<'a>(_owner: impl FnOnce() -> &'a Self, measured: Option<u64>) -> Option<u64>
    where
        Self: 'a,
    {
        measured
    }
}

pub(crate) struct MeasuredCpu;
impl MeasuredCpu {
    pub(crate) const fn new() -> Self {
        Self
    }
}
impl CpuObservation for MeasuredCpu {}

#[cfg(not(test))]
pub(crate) type RuntimeCpuObservation = MeasuredCpu;
#[cfg(test)]
pub(crate) type RuntimeCpuObservation = tests::Overrides;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::thread::{ThreadId, current};

    pub(crate) struct Overrides(Mutex<Option<HashMap<ThreadId, Option<u64>>>>);

    impl Overrides {
        pub(crate) const fn new() -> Self {
            Self(Mutex::new(None))
        }

        fn replace(&self, thread: ThreadId, value: Option<Option<u64>>) -> Option<Option<u64>> {
            let mut scopes = self.0.lock().unwrap();
            if let Some(value) = value {
                scopes
                    .get_or_insert_with(HashMap::new)
                    .insert(thread, value)
            } else {
                let old = scopes.as_mut().and_then(|scopes| scopes.remove(&thread));
                if scopes.as_ref().is_some_and(HashMap::is_empty) {
                    *scopes = None;
                }
                old
            }
        }

        pub(crate) fn with_override<T>(&self, cpu: Option<u64>, run: impl FnOnce() -> T) -> T {
            let thread = current().id();
            let _restore = Restore {
                owner: self,
                thread,
                previous: self.replace(thread, Some(cpu)),
            };
            run()
        }
    }

    struct Restore<'a> {
        owner: &'a Overrides,
        thread: ThreadId,
        previous: Option<Option<u64>>,
    }
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            self.owner.replace(self.thread, self.previous);
        }
    }

    impl CpuObservation for Overrides {
        fn completed<'a>(owner: impl FnOnce() -> &'a Self, measured: Option<u64>) -> Option<u64> {
            owner()
                .0
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|scopes| scopes.get(&current().id()).copied())
                .unwrap_or(measured)
        }
    }

    #[test]
    fn completed_cpu_overrides_are_runtime_and_thread_scoped() {
        let first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [None, Some(7)]
                .into_iter()
                .map(|cpu| {
                    let (first, second, barrier) = (&first, &second, &barrier);
                    scope.spawn(crate::runtime::serving(move || {
                        first.telemetry.completed_cpu.with_override(cpu, || {
                            barrier.wait();
                            let mut correct = true;
                            for _ in 0..64 {
                                correct &= Overrides::completed(
                                    || &first.telemetry.completed_cpu,
                                    Some(99),
                                ) == cpu;
                                correct &= Overrides::completed(
                                    || &second.telemetry.completed_cpu,
                                    Some(99),
                                ) == Some(99);
                                second.telemetry.completed_cpu.with_override(Some(11), || {
                                    correct &= Overrides::completed(
                                        || &second.telemetry.completed_cpu,
                                        None,
                                    ) == Some(11);
                                });
                            }
                            barrier.wait();
                            correct
                        })
                    }))
                })
                .collect();
            for handle in handles {
                assert!(handle.join().unwrap());
            }
        });
        assert!(first.telemetry.completed_cpu.0.lock().unwrap().is_none());
        assert!(second.telemetry.completed_cpu.0.lock().unwrap().is_none());
        assert!(std::sync::LazyLock::get(&first.gc).is_none());
        assert!(std::sync::LazyLock::get(&second.gc).is_none());
    }

    #[test]
    fn completed_cpu_nested_override_restores_on_unwind() {
        let owner = Overrides::new();
        owner.with_override(Some(7), || {
            let result = std::panic::catch_unwind(|| {
                owner.with_override(None, || {
                    assert_eq!(Overrides::completed(|| &owner, Some(99)), None);
                    panic!("exercise CPU scope restoration");
                })
            });
            assert!(result.is_err());
            assert_eq!(Overrides::completed(|| &owner, None), Some(7));
        });
        assert_eq!(Overrides::completed(|| &owner, Some(99)), Some(99));
        assert!(owner.0.lock().unwrap().is_none());
        let measured = MeasuredCpu::new();
        assert_eq!(
            MeasuredCpu::completed(|| panic!("measured input resolved owner"), Some(99)),
            Some(99)
        );
        assert_eq!(MeasuredCpu::completed(|| &measured, None), None);
    }
}
