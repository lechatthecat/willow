use super::ReadyEvent;
use super::{IoInterest, RawFd, RuntimeTaskId};
use std::time::Duration;

#[derive(Debug, Default)]
pub(super) struct Backend {
    pub(super) epoll_fd: Option<i32>,
    wake_fd: Option<i32>,
}

pub(super) const PLATFORM_POLL_SUPPORTED: bool = true;

const WAKE_TOKEN: usize = usize::MAX;

impl IoInterest {
    fn epoll_events(self) -> u32 {
        let mut events = 0;
        if self.overlaps(Self::Readable) {
            events |= libc::EPOLLIN as u32;
        }
        if self.overlaps(Self::Writable) {
            events |= libc::EPOLLOUT as u32;
        }
        events
    }
}

pub(super) fn valid_native_handle(handle: RawFd) -> bool {
    handle >= 0 && i32::try_from(handle).is_ok()
}

impl Backend {
    pub(super) fn init_platform(&mut self) -> i32 {
        if self.epoll_fd.is_some() {
            return 0;
        }
        let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll_fd < 0 {
            return -1;
        }
        let wake_fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if wake_fd < 0 {
            unsafe {
                libc::close(epoll_fd);
            }
            return -1;
        }
        let mut event = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: WAKE_TOKEN as u64,
        };
        let added = unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, wake_fd, &mut event) };
        if added != 0 {
            unsafe {
                libc::close(wake_fd);
                libc::close(epoll_fd);
            }
            return -1;
        }
        self.epoll_fd = Some(epoll_fd);
        self.wake_fd = Some(wake_fd);
        0
    }

    pub(super) fn sync_platform_fd(&mut self, fd: RawFd, interest: Option<IoInterest>) -> i32 {
        let Some(epoll_fd) = self.epoll_fd else {
            return 0;
        };
        let Some(interest) = interest else {
            let mut event = libc::epoll_event { events: 0, u64: 0 };
            unsafe {
                libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_DEL, fd as i32, &mut event);
            }
            return 0;
        };
        let mut event = libc::epoll_event {
            events: interest.epoll_events(),
            u64: fd as u64,
        };
        let fd = fd as i32;
        let add = unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut event) };
        if add == 0 {
            return 0;
        }
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
            let modify = unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_MOD, fd, &mut event) };
            if modify == 0 {
                return 0;
            }
        }
        -1
    }

    pub(super) fn poke_platform_waker(&self) {
        let Some(wake_fd) = self.wake_fd else {
            return;
        };
        let value = 1_u64;
        unsafe {
            libc::write(
                wake_fd,
                (&value as *const u64).cast::<libc::c_void>(),
                std::mem::size_of::<u64>(),
            );
        }
    }

    pub(super) fn drain_platform_waker(&self) {
        let Some(wake_fd) = self.wake_fd else {
            return;
        };
        loop {
            let mut value = 0_u64;
            let read = unsafe {
                libc::read(
                    wake_fd,
                    (&mut value as *mut u64).cast::<libc::c_void>(),
                    std::mem::size_of::<u64>(),
                )
            };
            if read != std::mem::size_of::<u64>() as isize {
                break;
            }
        }
    }

    pub(super) fn close_platform(&mut self) {
        if let Some(wake_fd) = self.wake_fd.take() {
            unsafe {
                libc::close(wake_fd);
            }
        }
        if let Some(epoll_fd) = self.epoll_fd.take() {
            unsafe {
                libc::close(epoll_fd);
            }
        }
    }
}

pub(super) fn wait_ready_tasks(
    service: &super::NetpollService,
    timeout: Option<Duration>,
) -> Vec<RuntimeTaskId> {
    let epoll_fd = service.with(|poll| {
        if !poll.has_waiters() || poll.init() != 0 {
            None
        } else {
            poll.backend.epoll_fd
        }
    });
    let Some(epoll_fd) = epoll_fd else {
        return Vec::new();
    };
    let events = wait_platform_events(epoll_fd, timeout);
    service.with(|poll| {
        let mut ready = Vec::new();
        for event in events {
            if event.token == WAKE_TOKEN {
                poll.backend.drain_platform_waker();
                ready.extend(poll.drain_ready_tokens());
            } else {
                ready.push(event);
            }
        }
        poll.tasks_for_ready_events(ready)
    })
}

pub(super) fn wait_platform_events(epoll_fd: i32, timeout: Option<Duration>) -> Vec<ReadyEvent> {
    let timeout_ms = match timeout {
        None => -1,
        Some(duration) => duration.as_millis().min(i32::MAX as u128) as i32,
    };
    let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 64];
    let n = unsafe {
        libc::epoll_wait(
            epoll_fd,
            events.as_mut_ptr(),
            events.len() as i32,
            timeout_ms,
        )
    };
    if n <= 0 {
        return Vec::new();
    }
    events
        .into_iter()
        .take(n as usize)
        .map(|event| {
            let readable = event.events & (libc::EPOLLIN as u32) != 0;
            let writable = event.events & (libc::EPOLLOUT as u32) != 0;
            let closed_or_error = event.events & ((libc::EPOLLHUP | libc::EPOLLERR) as u32) != 0;
            let interest = match (readable, writable) {
                _ if closed_or_error => Some(IoInterest::ReadWrite),
                (true, true) => Some(IoInterest::ReadWrite),
                (true, false) => Some(IoInterest::Readable),
                (false, true) => Some(IoInterest::Writable),
                (false, false) => None,
            };
            ReadyEvent {
                token: event.u64 as usize,
                interest,
            }
        })
        .collect()
}
