//! Epoch-local ownership and generation election. An election requests remark;
//! only a stopped recheck may end the queue epoch. The packed CAS prevents an
//! owner from becoming invisible between generation validation and election.
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(any(test, feature = "gc-notification-prototype"))]
use std::sync::{Condvar, Mutex, atomic::AtomicUsize};

const OWNERS: u64 = (1 << 20) - 1;
const REQUESTED: u64 = 1 << 20;
const GENERATION: u64 = 1 << 21;

#[derive(Default)]
pub(super) struct Termination(
    AtomicU64,
    #[cfg(any(test, feature = "gc-notification-prototype"))] Mutex<()>,
    #[cfg(any(test, feature = "gc-notification-prototype"))] Condvar,
    #[cfg(any(test, feature = "gc-notification-prototype"))] AtomicUsize,
);

pub(super) struct Owner<'a>(&'a Termination);

impl Termination {
    /// Own a root job, publication, worker or assist before touching its source.
    /// New ownership invalidates a candidate, restarting concurrent marking.
    pub(super) fn enter(&self) -> Owner<'_> {
        self.0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                assert!(state & OWNERS != OWNERS, "mark owner count exhausted");
                Some(Self::advance(state) + 1)
            })
            .expect("ownership update is infallible");
        #[cfg(any(test, feature = "gc-notification-prototype"))]
        self.notify();
        Owner(self)
    }

    // SeqCst publication/waiter handshake: either the waiter sees the new
    // generation or the publisher sees the waiter and takes its predicate lock.
    // The hot path never takes this mutex when no closure waiter exists.
    #[cfg(any(test, feature = "gc-notification-prototype"))]
    fn notify(&self) {
        if self.3.load(Ordering::SeqCst) != 0 {
            let _guard = self.1.lock().unwrap();
            self.2.notify_all();
        }
    }

    #[cfg(any(test, feature = "gc-notification-prototype"))]
    pub(super) fn wait_for_change(&self, sample: u64, timeout: std::time::Duration) {
        let guard = self.1.lock().unwrap();
        self.3.fetch_add(1, Ordering::SeqCst);
        let (_guard, _) = self
            .2
            .wait_timeout_while(guard, timeout, |_| self.sample() == sample)
            .unwrap();
        self.3.fetch_sub(1, Ordering::SeqCst);
    }

    fn advance(state: u64) -> u64 {
        (state & !REQUESTED)
            .checked_add(GENERATION)
            .expect("mark publication generation exhausted")
    }

    /// Sample before inspecting sources; CAS only the very same generation.
    pub(super) fn sample(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    pub(super) fn request(&self, sample: u64, empty: bool) -> bool {
        empty
            && sample & (OWNERS | REQUESTED) == 0
            && self
                .0
                .compare_exchange(
                    sample,
                    sample | REQUESTED,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
    }

    pub(super) fn requested(&self) -> bool {
        self.sample() & (OWNERS | REQUESTED) == REQUESTED
    }

    pub(super) fn restart(&self) {
        crate::gc_telemetry::observations::work(
            1,
            crate::gc_telemetry::observations::Work {
                restarts: 1,
                ..Default::default()
            },
        );
        self.0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                Some(Self::advance(state))
            })
            .expect("restart update is infallible");
        #[cfg(any(test, feature = "gc-notification-prototype"))]
        self.notify();
    }
}

impl Drop for Owner<'_> {
    fn drop(&mut self) {
        self.0
            .0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                assert!(state & OWNERS != 0, "mark owner count underflow");
                Some(Termination::advance(state) - 1)
            })
            .expect("owner completion is infallible");
        #[cfg(any(test, feature = "gc-notification-prototype"))]
        self.0.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn termination_generation_rejects_last_owner_and_completed_publication_races() {
        let protocol = Termination::default();
        let sample = protocol.sample();
        let root = protocol.enter();
        let assist = protocol.enter();
        assert!(!protocol.request(sample, true));
        assert!(!protocol.request(protocol.sample(), true));
        drop(root);
        assert!(!protocol.request(protocol.sample(), true));
        drop(assist);
        assert!(!protocol.request(sample, true));
        assert!(protocol.request(protocol.sample(), true));
        let publication = protocol.enter();
        assert!(!protocol.requested());
        drop(publication);
        assert!(protocol.request(protocol.sample(), true));
        protocol.restart();
        assert!(!protocol.requested());
    }

    #[test]
    fn termination_candidate_publication_race_is_deterministic() {
        for publication_first in [false, true] {
            for _ in 0..128 {
                let protocol = std::sync::Arc::new(Termination::default());
                let sampled = protocol.sample();
                let (begin_tx, begin_rx) = std::sync::mpsc::channel();
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let worker = {
                    let protocol = protocol.clone();
                    std::thread::spawn(move || {
                        begin_rx.recv().unwrap();
                        let owner = protocol.enter();
                        assert!(!protocol.requested());
                        drop(owner);
                        done_tx.send(()).unwrap();
                    })
                };
                if publication_first {
                    begin_tx.send(()).unwrap();
                    done_rx.recv().unwrap();
                    assert!(!protocol.request(sampled, true));
                } else {
                    assert!(protocol.request(sampled, true));
                    begin_tx.send(()).unwrap();
                    done_rx.recv().unwrap();
                    assert!(!protocol.requested());
                }
                worker.join().unwrap();
                assert!(protocol.request(protocol.sample(), true));
            }
        }
    }

    // Exhaust every legal length-seven history of two independent owners,
    // samples, empty/nonempty elections and restart. Compare against a plain
    // reference state, retaining the sampled generation across transitions.
    #[test]
    fn termination_transition_model_exhausts_owner_election_and_restart_histories() {
        #[derive(Clone, Copy, Default)]
        struct Model {
            owners: usize,
            generation: u64,
            requested: bool,
            sample: u64,
        }
        fn visit(model: Model, depth: usize, histories: &mut usize) {
            *histories += 1;
            if depth == 0 {
                return;
            }
            for action in 0..6 {
                if (action == 0 && model.owners == 2) || (action == 1 && model.owners == 0) {
                    continue;
                }
                let packed = model.generation * GENERATION
                    + model.owners as u64
                    + if model.requested { REQUESTED } else { 0 };
                let protocol = Termination(
                    AtomicU64::new(packed),
                    Mutex::new(()),
                    Condvar::new(),
                    AtomicUsize::new(0),
                );
                let mut next = model;
                match action {
                    0 => {
                        std::mem::forget(protocol.enter());
                        next.owners += 1;
                        next.generation += 1;
                        next.requested = false;
                    }
                    1 => {
                        drop(Owner(&protocol));
                        next.owners -= 1;
                        next.generation += 1;
                        next.requested = false;
                    }
                    2 => next.sample = protocol.sample(),
                    3 | 4 => {
                        let empty = action == 3;
                        let expected = empty
                            && model.owners == 0
                            && !model.requested
                            && model.sample == packed;
                        assert_eq!(protocol.request(model.sample, empty), expected);
                        next.requested |= expected;
                    }
                    5 => {
                        protocol.restart();
                        next.generation += 1;
                        next.requested = false;
                    }
                    _ => unreachable!(),
                }
                assert_eq!(protocol.sample() & OWNERS, next.owners as u64);
                assert_eq!(protocol.sample() / GENERATION, next.generation);
                assert_eq!(protocol.requested(), next.requested && next.owners == 0);
                visit(next, depth - 1, histories);
            }
        }
        let mut histories = 0;
        visit(Model::default(), 7, &mut histories);
        assert!(histories > 100_000);
        println!("termination model histories={histories} depth=7 owners=2");
    }

    #[test]
    fn termination_checked_limits_do_not_wrap_or_underflow() {
        for packed in [OWNERS, !OWNERS & !REQUESTED] {
            let protocol = Termination(
                AtomicU64::new(packed),
                Mutex::new(()),
                Condvar::new(),
                AtomicUsize::new(0),
            );
            assert!(std::panic::catch_unwind(|| protocol.enter()).is_err());
            assert_eq!(protocol.sample(), packed);
        }
        let protocol = Termination::default();
        assert!(std::panic::catch_unwind(|| drop(Owner(&protocol))).is_err());
        assert_eq!(protocol.sample(), 0);
    }
}

#[cfg(test)]
mod scaling_tests {
    use super::*;

    #[test]
    fn termination_generation_counts_scale_with_publications_and_nested_owners() {
        for count in [1, 16, 257, 4096] {
            let protocol = Termination::default();
            // Model a root job producing N publications: one root ownership
            // pair, one pair per publication, no rescan of previous roots.
            let root = protocol.enter();
            for _ in 0..count {
                let publication = protocol.enter();
                drop(publication);
            }
            drop(root);
            assert_eq!(protocol.sample() / GENERATION, 2 * (count + 1));
            assert_eq!(protocol.sample() & OWNERS, 0);
            assert!(protocol.request(protocol.sample(), true));
            println!(
                "publications={count} generation_updates={} candidate_cas=1",
                2 * (count + 1)
            );
        }
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;
    use std::time::{Duration, Instant};
    #[test]
    fn m3_termination_notification_before_wait_during_wait_and_spurious() {
        let t = std::sync::Arc::new(Termination::default());
        let old = t.sample();
        drop(t.enter());
        t.wait_for_change(old, Duration::from_secs(10)); // Must not lose prior publication.
        let sample = t.sample();
        let other = t.clone();
        let waiter =
            std::thread::spawn(move || other.wait_for_change(sample, Duration::from_secs(10)));
        let deadline = Instant::now() + Duration::from_secs(2);
        while t.3.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        {
            let _guard = t.1.lock().unwrap();
            t.2.notify_all();
        }
        assert_eq!(t.sample(), sample); // Spurious wake cannot manufacture work completion.
        drop(t.enter());
        waiter.join().unwrap();
        assert_eq!(t.3.load(Ordering::SeqCst), 0);
        assert!(t.request(t.sample(), true));
    }
    #[test]
    fn m3_termination_notification_timeout_preserves_owner_and_election() {
        let t = Termination::default();
        let owner = t.enter();
        t.wait_for_change(t.sample(), Duration::from_millis(1));
        assert!(!t.request(t.sample(), true));
        drop(owner);
        assert!(t.request(t.sample(), true));
    }
}
