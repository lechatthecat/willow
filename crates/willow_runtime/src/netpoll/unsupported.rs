use super::{IoInterest, RawFd, RuntimeTaskId};
use std::time::Duration;

#[derive(Debug, Default)]
pub(super) struct Backend {}

pub(super) const PLATFORM_POLL_SUPPORTED: bool = false;

pub(super) fn valid_native_handle(handle: RawFd) -> bool {
    handle >= 0 && i32::try_from(handle).is_ok()
}

impl Backend {
    pub(super) fn init_platform(&mut self) -> i32 {
        -1
    }

    pub(super) fn sync_platform_fd(&mut self, _fd: RawFd, _interest: Option<IoInterest>) -> i32 {
        -1
    }

    pub(super) fn poke_platform_waker(&self) {}

    pub(super) fn close_platform(&mut self) {}
}

pub(super) fn wait_ready_tasks(
    _service: &super::NetpollService,
    timeout: Option<Duration>,
) -> Vec<RuntimeTaskId> {
    if let Some(timeout) = timeout {
        if !timeout.is_zero() {
            std::thread::sleep(timeout);
        }
    }
    Vec::new()
}
