use std::sync::{Condvar, Mutex};

#[derive(Debug, Default)]
pub struct RuntimeStopTheWorld {
    paused: Mutex<bool>,
    changed: Condvar,
}

impl RuntimeStopTheWorld {
    pub fn request_pause(&self) {
        let mut paused = self.paused.lock().expect("pause mutex poisoned");
        *paused = true;
        self.changed.notify_all();
    }

    pub fn resume(&self) {
        let mut paused = self.paused.lock().expect("pause mutex poisoned");
        *paused = false;
        self.changed.notify_all();
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.lock().expect("pause mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_the_world_pause_flag_round_trips() {
        let sync = RuntimeStopTheWorld::default();
        sync.request_pause();
        assert!(sync.is_paused());
        sync.resume();
        assert!(!sync.is_paused());
    }
}
