//! Opt-in measurements of the actual retry helpers; no release-build hooks.
use super::*;

#[derive(Clone)]
struct Distribution {
    bins: [u64; 256],
    retries: u64,
    max: usize,
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
    fn merge(&mut self, other: &Self) {
        for (a, b) in self.bins.iter_mut().zip(other.bins) {
            *a += b;
        }
        self.retries += other.retries;
        self.max = self.max.max(other.max);
    }
    #[cfg(target_os = "linux")]
    fn report(&self, label: &str) {
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
thread_local! {
    static STATS: RefCell<Option<[Distribution; 2]>> = const { RefCell::new(None) };
}
pub(super) struct RetryProbe {
    kind: usize,
    retries: usize,
}
impl RetryProbe {
    pub(super) fn new(kind: usize) -> Self {
        Self { kind, retries: 0 }
    }
    pub(super) fn retry(&mut self) {
        self.retries += 1;
    }
}
impl Drop for RetryProbe {
    fn drop(&mut self) {
        STATS.with_borrow_mut(|stats| {
            if let Some(stats) = stats {
                stats[self.kind].record(self.retries);
            }
        });
    }
}

#[test]
fn retry_measurement_counts_terminal_success_and_empty() {
    for count in [0, 1, 8, 64, 256, 4096] {
        for success in [false, true] {
            struct Script {
                left: Cell<usize>,
                success: bool,
                calls: Cell<usize>,
            }
            impl FnSteal<usize> for Script {
                fn steal_one(&self) -> Steal<usize> {
                    self.calls.set(self.calls.get() + 1);
                    let left = self.left.get();
                    if left != 0 {
                        self.left.set(left - 1);
                        Steal::Retry
                    } else if self.success {
                        Steal::Success(7)
                    } else {
                        Steal::Empty
                    }
                }
            }
            STATS.with_borrow_mut(|s| *s = Some(Default::default()));
            let script = Script {
                left: Cell::new(count),
                success,
                calls: Cell::new(0),
            };
            assert_eq!(steal_retry(&script), success.then_some(7));
            assert_eq!(script.calls.get(), count + 1);
            let stats = STATS.take().unwrap();
            assert_eq!(stats[0].retries, count as u64);
            assert_eq!(stats[0].max, count);
            assert_eq!(stats[0].bins[count.min(255)], 1);
            assert_eq!(stats[0].bins.iter().sum::<u64>(), 1);
        }
    }
    STATS.with_borrow_mut(|s| *s = Some(Default::default()));
    let inbox = Injector::new();
    let owner = Worker::new_fifo();
    assert_eq!(steal_retry_batch(&inbox, &owner), None);
    inbox.push(9);
    assert_eq!(steal_retry_batch(&inbox, &owner), Some(9));
    assert_eq!(STATS.take().unwrap()[1].bins[0], 2);
}

// Thread CPU excludes waiting, producer CPU, and verification. Linux native
// measurement only; the deterministic probe test above runs on every target.
#[cfg(target_os = "linux")]
fn thread_cpu_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes to a valid timespec; no shared pointer.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) },
        0
    );
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "opt-in retry/CPU measurement; run release, nocapture, one test thread"]
fn retry_contention_measurement() {
    // wake: repeated small publications; fan_in: simultaneous producers;
    // owner: thieves race an owner deque; inbox: owner batches race thieves.
    for repeat in 0..3 {
        for workers in [1, 4, 16] {
            for per_producer in [4096, 32_768, 262_144] {
                for mode in ["wake", "fan_in", "owner", "inbox"] {
                    let producers = if mode == "fan_in" { workers } else { 1 };
                    let total = producers * per_producer;
                    let queues = RunQueues::new(workers + 1);
                    let gate = std::sync::Barrier::new(workers + producers + 1);
                    let left = AtomicUsize::new(producers);
                    let (wall, results) = std::thread::scope(|scope| {
                        let mut consumers = Vec::new();
                        for index in 0..workers {
                            let (queues, gate, left) = (&queues, &gate, &left);
                            consumers.push(scope.spawn(move || {
                                let worker = if mode == "inbox" { index } else { index + 1 };
                                CURRENT_WORKER.set(worker);
                                SCHED_RUN_DEPTH.set(1);
                                let mut output = Vec::with_capacity(total.div_ceil(workers));
                                STATS.with_borrow_mut(|s| *s = Some(Default::default()));
                                gate.wait();
                                let start = thread_cpu_ns();
                                let deadline = Instant::now() + Duration::from_secs(30);
                                queues.with_owner(worker, || {
                                    loop {
                                        // Completion must be observed before the last empty probe.
                                        let done = left.load(Ordering::Acquire) == 0;
                                        if let Some(id) = queues.pop_for_worker(worker) {
                                            output.push(id);
                                        } else if done {
                                            break;
                                        } else {
                                            std::thread::yield_now();
                                        }
                                        assert!(Instant::now() < deadline, "measurement timed out");
                                    }
                                });
                                let cpu = thread_cpu_ns() - start;
                                SCHED_RUN_DEPTH.set(0);
                                (output, cpu, STATS.take().unwrap())
                            }));
                        }
                        for producer in 0..producers {
                            let (queues, gate, left) = (&queues, &gate, &left);
                            scope.spawn(move || {
                                let publish = || {
                                    gate.wait();
                                    for offset in (0..per_producer).step_by(8) {
                                        for n in offset..offset + 8 {
                                            let id = (producer * per_producer + n) as RuntimeTaskId;
                                            if mode == "owner" || mode == "inbox" {
                                                queues.push_local(0, id);
                                            } else {
                                                queues.push_global(id);
                                            }
                                        }
                                        if mode == "wake" {
                                            std::thread::yield_now();
                                        }
                                    }
                                    left.fetch_sub(1, Ordering::Release);
                                };
                                if mode == "owner" {
                                    queues.with_owner(0, publish);
                                } else {
                                    publish();
                                }
                            });
                        }
                        let start = Instant::now();
                        gate.wait();
                        let results = consumers
                            .into_iter()
                            .map(|h| h.join().unwrap())
                            .collect::<Vec<_>>();
                        (start.elapsed(), results)
                    });
                    let mut seen = vec![false; total];
                    let mut stats: [Distribution; 2] = Default::default();
                    let mut cpu = 0;
                    let mut counts = Vec::new();
                    for (output, worker_cpu, worker_stats) in results {
                        cpu += worker_cpu;
                        counts.push(output.len());
                        for (a, b) in stats.iter_mut().zip(worker_stats) {
                            a.merge(&b);
                        }
                        for id in output {
                            assert!((id as usize) < total);
                            assert!(
                                !std::mem::replace(&mut seen[id as usize], true),
                                "duplicate token"
                            );
                        }
                    }
                    assert!(seen.into_iter().all(|x| x), "lost token");
                    assert_eq!(queues.len(), 0);
                    println!(
                        "sample repeat={repeat} mode={mode} workers={workers} tokens={total} wall_ns={} worker_cpu_ns={cpu} tokens_per_cpu_s={:.0} per_worker={counts:?}",
                        wall.as_nanos(),
                        total as f64 * 1e9 / cpu as f64
                    );
                    stats[0].report("single");
                    stats[1].report("batch");
                }
            }
        }
    }
    // Queue publication-to-pop latency, not OS wake-to-poll latency.
    let queues = RunQueues::new(1);
    let mut latency = Vec::with_capacity(10_000);
    queues.with_owner(0, || {
        for id in 0..10_000 {
            let start = Instant::now();
            queues.push_global(id);
            assert_eq!(queues.pop_for_worker(0), Some(id));
            latency.push(start.elapsed().as_nanos());
        }
    });
    latency.sort_unstable();
    println!(
        "single_task_queue_latency_ns p50={} p99={} max={}",
        latency[4999], latency[9899], latency[9999]
    );
}
