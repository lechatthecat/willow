use super::ReadyEvent;
use super::{IoInterest, RawFd, RuntimeTaskId};
use std::time::Duration;

#[derive(Debug, Default)]
pub(super) struct Backend {
    kqueue_fd: Option<i32>,
}

pub(super) const PLATFORM_POLL_SUPPORTED: bool = true;

const WAKE_TOKEN: usize = usize::MAX;

pub(super) fn valid_native_handle(handle: RawFd) -> bool {
    handle >= 0 && i32::try_from(handle).is_ok()
}

impl Backend {
    pub(super) fn init_platform(&mut self) -> i32 {
        if self.kqueue_fd.is_some() {
            return 0;
        }
        let kqueue_fd = unsafe { libc::kqueue() };
        if kqueue_fd < 0 {
            return -1;
        }
        let wake = apple_kevent(
            WAKE_TOKEN,
            libc::EVFILT_USER,
            libc::EV_ADD | libc::EV_CLEAR,
            0,
        );
        let added = unsafe {
            libc::kevent(
                kqueue_fd,
                &wake,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if added != 0 {
            unsafe { libc::close(kqueue_fd) };
            return -1;
        }
        self.kqueue_fd = Some(kqueue_fd);
        0
    }

    pub(super) fn sync_platform_fd(&mut self, fd: RawFd, interest: Option<IoInterest>) -> i32 {
        let Some(kqueue_fd) = self.kqueue_fd else {
            return 0;
        };
        let mut failed = false;
        for (filter, wanted) in [
            (
                libc::EVFILT_READ,
                interest.is_some_and(|i| i.overlaps(IoInterest::Readable)),
            ),
            (
                libc::EVFILT_WRITE,
                interest.is_some_and(|i| i.overlaps(IoInterest::Writable)),
            ),
        ] {
            let flags = if wanted {
                // Keep descriptor readiness level-triggered, matching epoll's
                // default and WSAPoll. EV_CLEAR would make this edge-triggered
                // and could lose a wake if a task only partially drains an fd.
                libc::EV_ADD
            } else {
                libc::EV_DELETE
            };
            let change = apple_kevent(fd as usize, filter, flags, 0);
            let rc = unsafe {
                libc::kevent(
                    kqueue_fd,
                    &change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if rc != 0 {
                let error = std::io::Error::last_os_error().raw_os_error();
                // Deleting a filter that was never installed is harmless.
                if wanted || error != Some(libc::ENOENT) {
                    failed = true;
                }
            }
        }
        if failed { -1 } else { 0 }
    }

    pub(super) fn poke_platform_waker(&self) {
        let Some(kqueue_fd) = self.kqueue_fd else {
            return;
        };
        let trigger = apple_kevent(WAKE_TOKEN, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER);
        unsafe {
            libc::kevent(
                kqueue_fd,
                &trigger,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            );
        }
    }

    pub(super) fn close_platform(&mut self) {
        if let Some(kqueue_fd) = self.kqueue_fd.take() {
            unsafe { libc::close(kqueue_fd) };
        }
    }
}

fn apple_kevent(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    }
}

pub(super) fn wait_ready_tasks(
    service: &super::NetpollService,
    timeout: Option<Duration>,
) -> Vec<RuntimeTaskId> {
    let kqueue_fd = service.with(|poll| {
        if !poll.has_waiters() || poll.init() != 0 {
            None
        } else {
            poll.backend.kqueue_fd
        }
    });
    let Some(kqueue_fd) = kqueue_fd else {
        return Vec::new();
    };
    let events = wait_apple_events(kqueue_fd, timeout);
    service.with(|poll| {
        let mut ready = Vec::new();
        for event in events {
            if event.token == WAKE_TOKEN {
                ready.extend(poll.drain_ready_tokens());
            } else {
                ready.push(event);
            }
        }
        poll.tasks_for_ready_events(ready)
    })
}

fn wait_apple_events(kqueue_fd: i32, timeout: Option<Duration>) -> Vec<ReadyEvent> {
    let timeout_value = timeout.map(|duration| libc::timespec {
        tv_sec: duration.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    });
    let timeout_ptr = timeout_value
        .as_ref()
        .map_or(std::ptr::null(), |value| value as *const libc::timespec);
    let mut events: Vec<libc::kevent> = (0..64).map(|_| apple_kevent(0, 0, 0, 0)).collect();
    let n = unsafe {
        libc::kevent(
            kqueue_fd,
            std::ptr::null(),
            0,
            events.as_mut_ptr(),
            events.len() as i32,
            timeout_ptr,
        )
    };
    if n <= 0 {
        return Vec::new();
    }
    events
        .into_iter()
        .take(n as usize)
        .map(|event| {
            let interest = if event.flags & (libc::EV_EOF | libc::EV_ERROR) != 0 {
                Some(IoInterest::ReadWrite)
            } else if event.filter == libc::EVFILT_READ {
                Some(IoInterest::Readable)
            } else if event.filter == libc::EVFILT_WRITE {
                Some(IoInterest::Writable)
            } else {
                None
            };
            ReadyEvent {
                token: event.ident,
                interest,
            }
        })
        .collect()
}
