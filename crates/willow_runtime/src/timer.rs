use std::time::{Duration, Instant};

use crate::trace::{GcTrace, GcVisitor};

#[derive(Debug, Clone)]
pub struct RuntimeTimer {
    deadline: Instant,
}

impl RuntimeTimer {
    pub fn after_millis(ms: i64) -> Self {
        let millis = ms.max(0) as u64;
        Self {
            deadline: Instant::now() + Duration::from_millis(millis),
        }
    }

    pub fn is_ready(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

impl GcTrace for RuntimeTimer {
    fn trace(&self, _visitor: &mut GcVisitor) {}
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_runtime_sleep(ms: i64) -> u8 {
    if ms > 0 {
        std::thread::sleep(Duration::from_millis(ms as u64));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_timer_is_ready_immediately() {
        assert!(RuntimeTimer::after_millis(0).is_ready());
    }

    #[test]
    fn timer_unit_01_negative_sleep_returns_ready_without_panic() {
        assert_eq!(willow_runtime_sleep(-1), 0);
    }

    #[test]
    fn timer_unit_02_zero_sleep_returns_ready_without_panic() {
        assert_eq!(willow_runtime_sleep(0), 0);
    }
}
