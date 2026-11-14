//! Task-table-owned lock evidence; production calls compile away.
pub(super) trait MutexObservation {
    fn acquired(&self) {}
}
#[cfg(not(test))]
#[derive(Debug, Default)]
pub(super) struct RuntimeMutexObservation {
    _disabled: (),
}
#[cfg(not(test))]
impl MutexObservation for RuntimeMutexObservation {}
#[cfg(test)]
pub(super) use tests::Observation as RuntimeMutexObservation;

#[cfg(test)]
mod tests {
    use super::MutexObservation;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::thread::{ThreadId, current};

    #[derive(Debug, Default)]
    pub(crate) struct Observation(Mutex<HashMap<ThreadId, usize>>);
    impl Observation {
        pub(crate) fn current(&self) -> usize {
            self.0
                .lock()
                .unwrap()
                .get(&current().id())
                .copied()
                .unwrap_or(0)
        }
    }
    impl MutexObservation for Observation {
        fn acquired(&self) {
            *self.0.lock().unwrap().entry(current().id()).or_default() += 1;
        }
    }
}
