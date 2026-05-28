use std::time::{Duration, Instant};

use crate::runtime::trace::{GcTrace, GcVisitor};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_timer_is_ready_immediately() {
        assert!(RuntimeTimer::after_millis(0).is_ready());
    }
}
