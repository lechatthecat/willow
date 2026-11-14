//! Queue-owned notification evidence and publication injection.
pub(super) trait QueueObservation {
    fn idle_notification(&self) {}
    fn owner_notification(&self) {}
    fn before_global_publish(&self) {}
}

#[cfg(not(test))]
#[derive(Debug, Default)]
pub(super) struct RuntimeQueueObservation {
    _disabled: (),
}
#[cfg(not(test))]
impl QueueObservation for RuntimeQueueObservation {}
#[cfg(test)]
pub(super) type RuntimeQueueObservation = tests::Observation;

#[cfg(test)]
mod tests {
    use super::QueueObservation;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Debug, Default)]
    pub(crate) struct Observation {
        pub(crate) idle_notifications: AtomicUsize,
        pub(crate) owner_notifications: AtomicUsize,
        pub(crate) pause_global_publish: AtomicBool,
        pub(crate) global_publish_gate: Mutex<()>,
    }
    impl QueueObservation for Observation {
        fn idle_notification(&self) {
            self.idle_notifications.fetch_add(1, Ordering::Relaxed);
        }
        fn owner_notification(&self) {
            self.owner_notifications.fetch_add(1, Ordering::Relaxed);
        }
        fn before_global_publish(&self) {
            if self.pause_global_publish.load(Ordering::Acquire) {
                drop(
                    self.global_publish_gate
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                );
            }
        }
    }

    #[test]
    fn queue_observers_keep_notification_counts_independent() {
        let first = Observation::default();
        let second = Observation::default();
        std::thread::scope(|scope| {
            for (owner, count) in [(&first, 32), (&second, 96)] {
                scope.spawn(crate::runtime::serving(move || {
                    for _ in 0..count {
                        owner.idle_notification();
                        owner.owner_notification();
                        owner.before_global_publish();
                    }
                }));
            }
        });
        assert_eq!(first.idle_notifications.load(Ordering::Relaxed), 32);
        assert_eq!(second.idle_notifications.load(Ordering::Relaxed), 96);
        assert_eq!(first.owner_notifications.load(Ordering::Relaxed), 32);
        assert_eq!(second.owner_notifications.load(Ordering::Relaxed), 96);
        second.owner_notification();
        assert_eq!(second.owner_notifications.load(Ordering::Relaxed), 97);
    }
}
