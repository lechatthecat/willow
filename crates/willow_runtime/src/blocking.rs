//! Bounded blocking-work pool used to isolate file I/O and conservative foreign
//! calls from scheduler workers (willow-0a6k.5).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};

type BlockingWork = Box<dyn FnOnce() + Send + 'static>;

pub const DEFAULT_BLOCKING_THREADS: usize = 4;

/// Foreign-call classification. Unknown declarations are conservative and use
/// the blocking pool until explicitly audited as non-blocking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignCallClass {
    NonBlocking,
    Blocking,
    Unknown,
}

impl ForeignCallClass {
    pub fn must_isolate(self) -> bool {
        !matches!(self, Self::NonBlocking)
    }
}

struct BlockingPool {
    sender: mpsc::Sender<BlockingWork>,
}

impl BlockingPool {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel::<BlockingWork>();
        let receiver = Arc::new(Mutex::new(receiver));
        let threads = std::env::var("WILLOW_BLOCKING_THREADS")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .filter(|count| *count > 0)
            .unwrap_or(DEFAULT_BLOCKING_THREADS);
        for index in 0..threads {
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name(format!("willow-blocking-{index}"))
                .spawn(move || {
                    loop {
                        let work = {
                            let receiver = receiver
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            receiver.recv()
                        };
                        let Ok(work) = work else { break };
                        ACTIVE_JOBS.fetch_add(1, Ordering::AcqRel);
                        work();
                        ACTIVE_JOBS.fetch_sub(1, Ordering::AcqRel);
                        COMPLETED_JOBS.fetch_add(1, Ordering::AcqRel);
                    }
                })
                .expect("failed to start Willow blocking worker");
        }
        Self { sender }
    }

    fn submit(&self, work: BlockingWork) -> bool {
        self.sender.send(work).is_ok()
    }
}

static BLOCKING_POOL: LazyLock<BlockingPool> = LazyLock::new(BlockingPool::new);
static ACTIVE_JOBS: AtomicUsize = AtomicUsize::new(0);
static COMPLETED_JOBS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn submit(work: impl FnOnce() + Send + 'static) -> bool {
    BLOCKING_POOL.submit(Box::new(work))
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_active_jobs() -> i64 {
    ACTIVE_JOBS.load(Ordering::Acquire) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_completed_jobs() -> i64 {
    COMPLETED_JOBS.load(Ordering::Acquire) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_pool_executes_submitted_work() {
        let (sender, receiver) = mpsc::channel();
        assert!(submit(move || sender.send(42).unwrap()));
        assert_eq!(
            receiver.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(42)
        );
    }

    #[test]
    fn unknown_foreign_calls_are_conservatively_blocking() {
        assert!(!ForeignCallClass::NonBlocking.must_isolate());
        assert!(ForeignCallClass::Blocking.must_isolate());
        assert!(ForeignCallClass::Unknown.must_isolate());
    }
}
