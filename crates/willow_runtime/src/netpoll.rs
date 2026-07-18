use std::collections::{HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use crate::task::RuntimeTaskId;
use crate::trace::{GcTrace, GcVisitor};

/// Native readiness handle. Unix file descriptors fit in this word; on Windows
/// it carries a WinSock `SOCKET` without truncating the 64-bit handle.
type RawFd = i64;

pub const WILLOW_NETPOLL_READABLE: i32 = 1;
pub const WILLOW_NETPOLL_WRITABLE: i32 = 2;

#[cfg(not(target_os = "windows"))]
fn valid_native_handle(handle: RawFd) -> bool {
    handle >= 0 && i32::try_from(handle).is_ok()
}

#[cfg(target_os = "windows")]
fn valid_native_handle(handle: RawFd) -> bool {
    // WinSock SOCKET is an unsigned pointer-sized integer. A valid socket may
    // therefore have bit 63 set when carried through the ABI's i64 word; only
    // INVALID_SOCKET (all bits set) is reserved.
    handle != -1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoInterest {
    Readable,
    Writable,
    ReadWrite,
}

impl IoInterest {
    fn from_bits(bits: i32) -> Option<Self> {
        match bits & (WILLOW_NETPOLL_READABLE | WILLOW_NETPOLL_WRITABLE) {
            WILLOW_NETPOLL_READABLE => Some(Self::Readable),
            WILLOW_NETPOLL_WRITABLE => Some(Self::Writable),
            3 => Some(Self::ReadWrite),
            _ => None,
        }
    }

    fn overlaps(self, ready: Self) -> bool {
        matches!(
            (self, ready),
            (Self::ReadWrite, _)
                | (_, Self::ReadWrite)
                | (Self::Readable, Self::Readable)
                | (Self::Writable, Self::Writable)
        )
    }

    #[cfg(target_os = "linux")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoRegistration {
    pub fd: RawFd,
    pub token: usize,
    pub task_id: RuntimeTaskId,
    pub interest: IoInterest,
}

impl IoRegistration {
    pub fn new(fd: RawFd, task_id: RuntimeTaskId, interest: IoInterest) -> Self {
        Self {
            fd,
            token: fd as usize,
            task_id,
            interest,
        }
    }
}

/// Whether this build has a real netpoll backend. Linux uses epoll, Apple
/// platforms use kqueue, and Windows uses WinSock's readiness poll. Unsupported
/// targets fail registration instead of parking a task that can never wake.
#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
))]
const PLATFORM_POLL_SUPPORTED: bool = true;
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
)))]
const PLATFORM_POLL_SUPPORTED: bool = false;

#[derive(Debug)]
pub struct RuntimeNetPoll {
    registrations: Vec<IoRegistration>,
    ready_tokens: VecDeque<usize>,
    #[cfg(target_os = "linux")]
    epoll_fd: Option<i32>,
    #[cfg(target_os = "linux")]
    wake_fd: Option<i32>,
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    kqueue_fd: Option<i32>,
    #[cfg(target_os = "windows")]
    wake_receiver: Option<std::net::UdpSocket>,
    #[cfg(target_os = "windows")]
    wake_sender: Option<std::net::UdpSocket>,
}

impl Default for RuntimeNetPoll {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeNetPoll {
    pub fn new() -> Self {
        Self {
            registrations: Vec::new(),
            ready_tokens: VecDeque::new(),
            #[cfg(target_os = "linux")]
            epoll_fd: None,
            #[cfg(target_os = "linux")]
            wake_fd: None,
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            kqueue_fd: None,
            #[cfg(target_os = "windows")]
            wake_receiver: None,
            #[cfg(target_os = "windows")]
            wake_sender: None,
        }
    }

    pub fn init(&mut self) -> i32 {
        self.init_platform()
    }

    pub fn register(&mut self, registration: IoRegistration) {
        if !self.registrations.iter().any(|existing| {
            existing.fd == registration.fd
                && existing.task_id == registration.task_id
                && existing.interest == registration.interest
        }) {
            self.registrations.push(registration);
        }
    }

    pub fn reregister(&mut self, registration: IoRegistration) {
        self.registrations.retain(|existing| {
            !(existing.fd == registration.fd && existing.task_id == registration.task_id)
        });
        self.register(registration);
    }

    pub fn register_fd(&mut self, fd: RawFd, task_id: RuntimeTaskId, interest: IoInterest) -> i32 {
        if !valid_native_handle(fd) || task_id == 0 || !PLATFORM_POLL_SUPPORTED || self.init() != 0
        {
            return -1;
        }
        let before = self.registrations.len();
        self.register(IoRegistration::new(fd, task_id, interest));
        let added = self.registrations.len() > before;
        let rc = self.sync_platform_fd(fd);
        if rc != 0 && added {
            // The platform poller rejected the fd: roll back the registration we
            // just added so a failed `epoll_ctl` does not leave a phantom waiter
            // that keeps `has_waiters()` true forever and misleads the scheduler.
            self.registrations
                .retain(|r| !(r.fd == fd && r.task_id == task_id && r.interest == interest));
            let _ = self.sync_platform_fd(fd);
        }
        rc
    }

    pub fn reregister_fd(
        &mut self,
        fd: RawFd,
        task_id: RuntimeTaskId,
        interest: IoInterest,
    ) -> i32 {
        if !valid_native_handle(fd) || task_id == 0 || !PLATFORM_POLL_SUPPORTED || self.init() != 0
        {
            return -1;
        }
        self.reregister(IoRegistration::new(fd, task_id, interest));
        let rc = self.sync_platform_fd(fd);
        if rc != 0 {
            // Drop the registration on sync failure so no phantom waiter remains
            // (the prior registration for this fd/task was already replaced).
            self.registrations
                .retain(|r| !(r.fd == fd && r.task_id == task_id && r.interest == interest));
            let _ = self.sync_platform_fd(fd);
        }
        rc
    }

    pub fn deregister_fd(&mut self, fd: RawFd) -> i32 {
        self.registrations
            .retain(|registration| registration.fd != fd);
        self.sync_platform_fd(fd)
    }

    pub fn registrations(&self) -> &[IoRegistration] {
        &self.registrations
    }

    pub fn has_waiters(&self) -> bool {
        !self.registrations.is_empty()
    }

    /// Remove every registration owned by `task_id` (willow-vynv.1): a
    /// cancelled task must not linger as an I/O waiter, and its fds must not
    /// keep firing wakeups for a task that will never poll again.
    pub fn purge_task(&mut self, task_id: RuntimeTaskId) {
        let dead: HashSet<RawFd> = self
            .registrations
            .iter()
            .filter(|r| r.task_id == task_id)
            .map(|r| r.fd)
            .collect();
        for fd in dead {
            // Only drop the platform interest when NO other task shares the fd.
            if !self
                .registrations
                .iter()
                .any(|r| r.fd == fd && r.task_id != task_id)
            {
                self.deregister_fd(fd);
            }
        }
        self.registrations.retain(|r| r.task_id != task_id);
    }

    pub fn ready_tasks(&self, token: usize) -> Vec<RuntimeTaskId> {
        self.ready_tasks_for(token, None)
    }

    fn ready_tasks_for(
        &self,
        token: usize,
        ready_interest: Option<IoInterest>,
    ) -> Vec<RuntimeTaskId> {
        let mut seen = HashSet::new();
        let mut tasks = Vec::new();
        for registration in &self.registrations {
            if registration.token != token {
                continue;
            }
            if let Some(ready) = ready_interest
                && !registration.interest.overlaps(ready)
            {
                continue;
            }
            if seen.insert(registration.task_id) {
                tasks.push(registration.task_id);
            }
        }
        tasks
    }

    fn wake_token(&mut self, token: usize) -> i64 {
        let ready_count = self.ready_tasks(token).len() as i64;
        self.ready_tokens.push_back(token);
        self.poke_platform_waker();
        ready_count
    }

    fn drain_ready_tokens(&mut self) -> Vec<ReadyEvent> {
        self.ready_tokens
            .drain(..)
            .map(|token| ReadyEvent {
                token,
                interest: None,
            })
            .collect()
    }

    fn tasks_for_ready_events(&self, events: Vec<ReadyEvent>) -> Vec<RuntimeTaskId> {
        let mut seen = HashSet::new();
        let mut tasks = Vec::new();
        for event in events {
            for task_id in self.ready_tasks_for(event.token, event.interest) {
                if seen.insert(task_id) {
                    tasks.push(task_id);
                }
            }
        }
        tasks
    }

    #[cfg(test)]
    pub fn reset_for_test(&mut self) {
        self.registrations.clear();
        self.ready_tokens.clear();
        self.close_platform();
    }
}

impl Drop for RuntimeNetPoll {
    fn drop(&mut self) {
        self.close_platform();
    }
}

impl GcTrace for RuntimeNetPoll {
    fn trace(&self, _visitor: &mut GcVisitor) {
        // Netpoll waiters are task ids, not GC pointers. Parked task frames stay
        // GC-reachable through the scheduler's task table/runtime roots.
    }
}

#[derive(Debug, Clone, Copy)]
struct ReadyEvent {
    token: usize,
    interest: Option<IoInterest>,
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
))]
const WAKE_TOKEN: usize = usize::MAX;

#[cfg(target_os = "linux")]
impl RuntimeNetPoll {
    fn init_platform(&mut self) -> i32 {
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

    fn sync_platform_fd(&mut self, fd: RawFd) -> i32 {
        let Some(epoll_fd) = self.epoll_fd else {
            return 0;
        };
        let interest = self
            .registrations
            .iter()
            .filter(|registration| registration.fd == fd)
            .fold(None, |acc, registration| {
                match (acc, registration.interest) {
                    (None, interest) => Some(interest),
                    (Some(existing), interest) if existing == interest => Some(existing),
                    (Some(_), _) => Some(IoInterest::ReadWrite),
                }
            });
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

    fn poke_platform_waker(&self) {
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

    fn drain_platform_waker(&self) {
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

    fn close_platform(&mut self) {
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

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl RuntimeNetPoll {
    fn init_platform(&mut self) -> i32 {
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

    fn sync_platform_fd(&mut self, fd: RawFd) -> i32 {
        let Some(kqueue_fd) = self.kqueue_fd else {
            return 0;
        };
        let interest = merged_interest(&self.registrations, fd);
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

    fn poke_platform_waker(&self) {
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

    fn drain_platform_waker(&self) {}

    fn close_platform(&mut self) {
        if let Some(kqueue_fd) = self.kqueue_fd.take() {
            unsafe { libc::close(kqueue_fd) };
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
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

#[cfg(target_os = "windows")]
#[repr(C)]
struct WsaPollFd {
    fd: usize,
    events: i16,
    revents: i16,
}

#[cfg(target_os = "windows")]
const WSA_POLL_READ: i16 = 0x0100 | 0x0200;
#[cfg(target_os = "windows")]
const WSA_POLL_WRITE: i16 = 0x0010;
#[cfg(target_os = "windows")]
const WSA_POLL_ERROR: i16 = 0x0001 | 0x0002 | 0x0004;
#[cfg(target_os = "windows")]
const WSA_POLL_NVAL: i16 = 0x0004;

#[cfg(target_os = "windows")]
#[link(name = "ws2_32")]
unsafe extern "system" {
    fn WSAPoll(fd_array: *mut WsaPollFd, count: u32, timeout_ms: i32) -> i32;
}

#[cfg(target_os = "windows")]
impl RuntimeNetPoll {
    fn init_platform(&mut self) -> i32 {
        if self.wake_receiver.is_some() {
            return 0;
        }
        let receiver = match std::net::UdpSocket::bind(("127.0.0.1", 0)) {
            Ok(socket) => socket,
            Err(_) => return -1,
        };
        let address = match receiver.local_addr() {
            Ok(address) => address,
            Err(_) => return -1,
        };
        let sender = match std::net::UdpSocket::bind(("127.0.0.1", 0)) {
            Ok(socket) => socket,
            Err(_) => return -1,
        };
        if receiver.set_nonblocking(true).is_err()
            || sender.set_nonblocking(true).is_err()
            || sender.connect(address).is_err()
        {
            return -1;
        }
        self.wake_receiver = Some(receiver);
        self.wake_sender = Some(sender);
        0
    }

    fn sync_platform_fd(&mut self, fd: RawFd) -> i32 {
        if merged_interest(&self.registrations, fd).is_none() {
            return 0;
        }
        let mut probe = [WsaPollFd {
            fd: fd as usize,
            events: WSA_POLL_READ | WSA_POLL_WRITE,
            revents: 0,
        }];
        let rc = unsafe { WSAPoll(probe.as_mut_ptr(), 1, 0) };
        if rc < 0 || probe[0].revents & WSA_POLL_NVAL != 0 {
            -1
        } else {
            0
        }
    }

    fn poke_platform_waker(&self) {
        if let Some(sender) = &self.wake_sender {
            let _ = sender.send(&[1]);
        }
    }

    fn drain_platform_waker(&self) {
        let Some(receiver) = &self.wake_receiver else {
            return;
        };
        let mut bytes = [0_u8; 64];
        while receiver.recv(&mut bytes).is_ok() {}
    }

    fn close_platform(&mut self) {
        self.wake_sender = None;
        self.wake_receiver = None;
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
)))]
impl RuntimeNetPoll {
    fn init_platform(&mut self) -> i32 {
        -1
    }

    fn sync_platform_fd(&mut self, _fd: RawFd) -> i32 {
        -1
    }

    fn poke_platform_waker(&self) {}

    fn drain_platform_waker(&self) {}

    fn close_platform(&mut self) {}
}

fn merged_interest(registrations: &[IoRegistration], fd: RawFd) -> Option<IoInterest> {
    registrations
        .iter()
        .filter(|registration| registration.fd == fd)
        .fold(None, |acc, registration| {
            match (acc, registration.interest) {
                (None, interest) => Some(interest),
                (Some(existing), interest) if existing == interest => Some(existing),
                (Some(_), _) => Some(IoInterest::ReadWrite),
            }
        })
}

static GLOBAL_NETPOLL: LazyLock<Mutex<RuntimeNetPoll>> =
    LazyLock::new(|| Mutex::new(RuntimeNetPoll::new()));

fn with_global<R>(f: impl FnOnce(&mut RuntimeNetPoll) -> R) -> R {
    let mut guard = GLOBAL_NETPOLL.lock().expect("netpoll mutex poisoned");
    f(&mut guard)
}

pub(crate) fn has_waiters() -> bool {
    with_global(|poll| poll.has_waiters())
}

/// Purge every registration owned by a cancelled task (willow-vynv.1). Safe
/// to call with the scheduler lock held: the netpoll lock is only ever held
/// briefly (never across `epoll_wait`), and no path nests netpoll -> scheduler.
pub(crate) fn purge_task(task_id: RuntimeTaskId) {
    with_global(|poll| poll.purge_task(task_id));
}

pub(crate) fn wait_and_wake(timeout: Option<Duration>) -> usize {
    let tasks = wait_ready_tasks(timeout);
    let count = tasks.len();
    for task_id in tasks {
        crate::scheduler::willow_sched_wake(task_id);
    }
    if count > 0 {
        crate::gc::stress_collect("scheduler");
    }
    count
}

fn wait_ready_tasks(timeout: Option<Duration>) -> Vec<RuntimeTaskId> {
    let initial = with_global(|poll| {
        let events = poll.drain_ready_tokens();
        poll.tasks_for_ready_events(events)
    });
    if !initial.is_empty() {
        return initial;
    }

    #[cfg(target_os = "linux")]
    {
        let epoll_fd = with_global(|poll| {
            if !poll.has_waiters() || poll.init() != 0 {
                None
            } else {
                poll.epoll_fd
            }
        });
        let Some(epoll_fd) = epoll_fd else {
            return Vec::new();
        };
        let events = wait_platform_events(epoll_fd, timeout);
        with_global(|poll| {
            let mut ready = Vec::new();
            for event in events {
                if event.token == WAKE_TOKEN {
                    poll.drain_platform_waker();
                    ready.extend(poll.drain_ready_tokens());
                } else {
                    ready.push(event);
                }
            }
            poll.tasks_for_ready_events(ready)
        })
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let kqueue_fd = with_global(|poll| {
            if !poll.has_waiters() || poll.init() != 0 {
                None
            } else {
                poll.kqueue_fd
            }
        });
        let Some(kqueue_fd) = kqueue_fd else {
            return Vec::new();
        };
        let events = wait_apple_events(kqueue_fd, timeout);
        with_global(|poll| {
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

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::io::AsRawSocket;
        let snapshot = with_global(|poll| {
            if !poll.has_waiters() || poll.init() != 0 {
                None
            } else {
                Some((
                    poll.registrations.clone(),
                    poll.wake_receiver.as_ref()?.as_raw_socket() as usize,
                ))
            }
        });
        let Some((registrations, wake_socket)) = snapshot else {
            return Vec::new();
        };
        let events = wait_windows_events(&registrations, wake_socket, timeout);
        with_global(|poll| {
            let mut ready = Vec::new();
            for event in events {
                if event.token == WAKE_TOKEN {
                    poll.drain_platform_waker();
                    ready.extend(poll.drain_ready_tokens());
                } else {
                    ready.push(event);
                }
            }
            poll.tasks_for_ready_events(ready)
        })
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    )))]
    {
        if let Some(timeout) = timeout {
            if !timeout.is_zero() {
                std::thread::sleep(timeout);
            }
        }
        Vec::new()
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
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

#[cfg(target_os = "windows")]
fn wait_windows_events(
    registrations: &[IoRegistration],
    wake_socket: usize,
    timeout: Option<Duration>,
) -> Vec<ReadyEvent> {
    let mut unique = Vec::<(RawFd, IoInterest)>::new();
    for registration in registrations {
        if let Some((_, interest)) = unique.iter_mut().find(|(fd, _)| *fd == registration.fd) {
            if *interest != registration.interest {
                *interest = IoInterest::ReadWrite;
            }
        } else {
            unique.push((registration.fd, registration.interest));
        }
    }
    let mut sockets = Vec::with_capacity(unique.len() + 1);
    sockets.push(WsaPollFd {
        fd: wake_socket,
        events: WSA_POLL_READ,
        revents: 0,
    });
    sockets.extend(unique.iter().map(|(fd, interest)| WsaPollFd {
        fd: *fd as usize,
        events: match interest {
            IoInterest::Readable => WSA_POLL_READ,
            IoInterest::Writable => WSA_POLL_WRITE,
            IoInterest::ReadWrite => WSA_POLL_READ | WSA_POLL_WRITE,
        },
        revents: 0,
    }));
    let timeout_ms = match timeout {
        None => -1,
        Some(duration) => duration.as_millis().min(i32::MAX as u128) as i32,
    };
    let n = unsafe { WSAPoll(sockets.as_mut_ptr(), sockets.len() as u32, timeout_ms) };
    if n <= 0 {
        return Vec::new();
    }
    sockets
        .into_iter()
        .enumerate()
        .filter(|(_, socket)| socket.revents != 0)
        .map(|(index, socket)| {
            let token = if index == 0 {
                WAKE_TOKEN
            } else {
                unique[index - 1].0 as usize
            };
            let interest = if socket.revents & WSA_POLL_ERROR != 0 {
                Some(IoInterest::ReadWrite)
            } else {
                match (
                    socket.revents & WSA_POLL_READ != 0,
                    socket.revents & WSA_POLL_WRITE != 0,
                ) {
                    (true, true) => Some(IoInterest::ReadWrite),
                    (true, false) => Some(IoInterest::Readable),
                    (false, true) => Some(IoInterest::Writable),
                    (false, false) => None,
                }
            };
            ReadyEvent { token, interest }
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn wait_platform_events(epoll_fd: i32, timeout: Option<Duration>) -> Vec<ReadyEvent> {
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

#[unsafe(no_mangle)]
pub extern "C" fn willow_netpoll_init() -> i32 {
    with_global(|poll| poll.init())
}

/// Register the current cooperative task for fd readiness. `interest` is a
/// bitmask: 1 readable, 2 writable, 3 both. Returns 0 on success.
#[unsafe(no_mangle)]
pub extern "C" fn willow_netpoll_register(fd: i64, interest: i32) -> i32 {
    let Some(interest) = IoInterest::from_bits(interest) else {
        return -1;
    };
    let current = crate::scheduler::willow_sched_current_task();
    let result = with_global(|poll| poll.register_fd(fd, current, interest));
    crate::gc::stress_collect("scheduler");
    result
}

/// Replace the current task's registration for `fd`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_netpoll_reregister(fd: i64, interest: i32) -> i32 {
    let Some(interest) = IoInterest::from_bits(interest) else {
        return -1;
    };
    let current = crate::scheduler::willow_sched_current_task();
    let result = with_global(|poll| poll.reregister_fd(fd, current, interest));
    crate::gc::stress_collect("scheduler");
    result
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_netpoll_deregister(fd: i64) -> i32 {
    with_global(|poll| poll.deregister_fd(fd))
}

/// Wait for readiness and wake matching parked tasks. `timeout_ms < 0` waits
/// indefinitely; `0` polls; positive values bound the wait.
#[unsafe(no_mangle)]
pub extern "C" fn willow_netpoll_wait(timeout_ms: i64) -> i64 {
    let timeout = if timeout_ms < 0 {
        None
    } else {
        Some(Duration::from_millis(timeout_ms as u64))
    };
    wait_and_wake(timeout) as i64
}

/// Inject readiness for `token` (currently fd-as-token) and wake the platform
/// poller. The actual scheduler wake happens on the scheduler thread during
/// `willow_netpoll_wait` / idle scheduler polling.
#[unsafe(no_mangle)]
pub extern "C" fn willow_netpoll_wake(token: i64) -> i64 {
    if !valid_native_handle(token) {
        return -1;
    }
    with_global(|poll| poll.wake_token(token as usize))
}

#[cfg(test)]
pub fn reset_global_netpoll_for_test() {
    with_global(|poll| poll.reset_for_test());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(any(unix, windows))]
    use crate::async_frame::{async_frame_slot_offset, willow_async_frame_alloc};
    #[cfg(any(unix, windows))]
    use crate::gc::{reset_internal_for_test, runtime_test_guard};
    #[cfg(any(unix, windows))]
    use crate::scheduler::{
        reset_global_scheduler_for_test, willow_sched_run, willow_sched_spawn,
        willow_sched_task_state,
    };
    #[cfg(any(unix, windows))]
    use crate::task::{RUNTIME_POLL_PENDING, RUNTIME_POLL_READY};
    #[cfg(any(unix, windows))]
    use std::ffi::c_void;
    use std::sync::atomic::AtomicI32;
    #[cfg(any(unix, windows))]
    use std::sync::atomic::Ordering;

    static NETPOLL_TEST_LAST_REGISTER: AtomicI32 = AtomicI32::new(0);

    #[test]
    fn netpoll_maps_tokens_to_tasks() {
        let mut poll = RuntimeNetPoll::default();
        poll.register(IoRegistration::new(3, 9, IoInterest::Readable));
        assert_eq!(poll.ready_tasks(3), vec![9]);
    }

    #[test]
    fn netpoll_reregister_replaces_task_interest() {
        let mut poll = RuntimeNetPoll::default();
        poll.register(IoRegistration::new(3, 9, IoInterest::Readable));
        poll.reregister(IoRegistration::new(3, 9, IoInterest::Writable));
        assert_eq!(
            poll.registrations(),
            &[IoRegistration::new(3, 9, IoInterest::Writable)]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn netpoll_register_fd_rolls_back_when_sync_fails() {
        // A regular file is not epoll-pollable, so epoll_ctl fails with EPERM on
        // Linux (deterministic, and the held-open File prevents fd reuse). On
        // platforms without a netpoll backend, register_fd fails fast instead.
        // Either way the registration must NOT be left behind as a phantom waiter.
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(std::env::current_exe().unwrap())
            .or_else(|_| std::fs::File::open("Cargo.toml"))
            .expect("open a regular file for the test");
        let mut poll = RuntimeNetPoll::default();
        let rc = poll.register_fd(file.as_raw_fd().into(), 9, IoInterest::Readable);
        assert_eq!(rc, -1, "registering an un-pollable fd must fail");
        assert!(
            !poll.has_waiters(),
            "a failed registration must be rolled back, not left as a phantom waiter"
        );
        poll.reset_for_test();
    }

    #[test]
    fn netpoll_deregister_removes_fd_waiters() {
        let mut poll = RuntimeNetPoll::default();
        poll.register(IoRegistration::new(3, 9, IoInterest::Readable));
        poll.register(IoRegistration::new(4, 10, IoInterest::Readable));
        poll.deregister_fd(3);
        assert_eq!(poll.ready_tasks(3), Vec::<RuntimeTaskId>::new());
        assert_eq!(poll.ready_tasks(4), vec![10]);
    }

    #[cfg(unix)]
    unsafe extern "C" fn poll_netpoll_manual_wake(frame: *mut c_void) -> i32 {
        let state = unsafe { &mut *(frame as *mut i64) };
        let fd = unsafe { *((frame as *mut u8).add(async_frame_slot_offset(0)) as *const i32) };
        *state += 1;
        if *state >= 2 {
            willow_netpoll_deregister(fd.into());
            RUNTIME_POLL_READY
        } else {
            let result = willow_netpoll_register(fd.into(), WILLOW_NETPOLL_READABLE);
            NETPOLL_TEST_LAST_REGISTER.store(result, Ordering::SeqCst);
            if result != 0 {
                return RUNTIME_POLL_READY;
            }
            RUNTIME_POLL_PENDING
        }
    }

    #[cfg(unix)]
    #[test]
    fn netpoll_wake_resumes_parked_task() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        reset_global_netpoll_for_test();
        NETPOLL_TEST_LAST_REGISTER.store(i32::MIN, Ordering::SeqCst);

        let mut fds = [0_i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let read_fd = fds[0];
        let write_fd = fds[1];

        let frame = willow_async_frame_alloc(1, 0) as *mut u8;
        unsafe {
            *((frame.add(async_frame_slot_offset(0))) as *mut i32) = read_fd;
        }
        let id = willow_sched_spawn(poll_netpoll_manual_wake, frame as *mut c_void);
        let token = read_fd as i64;
        let waker = std::thread::spawn(move || {
            while NETPOLL_TEST_LAST_REGISTER.load(Ordering::SeqCst) == i32::MIN {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(willow_netpoll_wake(token), 1);
        });

        assert_eq!(willow_sched_run(), 1);
        assert_eq!(NETPOLL_TEST_LAST_REGISTER.load(Ordering::SeqCst), 0);
        assert_eq!(willow_sched_task_state(id), 3); // Completed
        waker.join().unwrap();

        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        reset_global_netpoll_for_test();
        reset_internal_for_test();
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
    unsafe extern "C" fn poll_pipe_readable(frame: *mut c_void) -> i32 {
        let state = unsafe { &mut *(frame as *mut i64) };
        let fd = unsafe { *((frame as *mut u8).add(async_frame_slot_offset(0)) as *const i32) };
        *state += 1;
        if *state >= 2 {
            willow_netpoll_deregister(fd.into());
            let mut byte = 0_u8;
            unsafe {
                libc::read(fd, (&mut byte as *mut u8).cast::<libc::c_void>(), 1);
            }
            RUNTIME_POLL_READY
        } else {
            let result = willow_netpoll_register(fd.into(), WILLOW_NETPOLL_READABLE);
            NETPOLL_TEST_LAST_REGISTER.store(result, Ordering::SeqCst);
            if result != 0 {
                return RUNTIME_POLL_READY;
            }
            RUNTIME_POLL_PENDING
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
    #[test]
    fn netpoll_unix_readiness_wakes_scheduler_idle_task() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        reset_global_netpoll_for_test();
        NETPOLL_TEST_LAST_REGISTER.store(i32::MIN, Ordering::SeqCst);

        let mut fds = [0_i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let read_fd = fds[0];
        let write_fd = fds[1];

        let frame = willow_async_frame_alloc(1, 0) as *mut u8;
        unsafe {
            *((frame.add(async_frame_slot_offset(0))) as *mut i32) = read_fd;
        }
        let id = willow_sched_spawn(poll_pipe_readable, frame as *mut c_void);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            let byte = [b'x'];
            unsafe {
                libc::write(write_fd, byte.as_ptr().cast::<libc::c_void>(), 1);
                libc::close(write_fd);
            }
        });

        assert_eq!(willow_sched_run(), 1);
        assert_eq!(NETPOLL_TEST_LAST_REGISTER.load(Ordering::SeqCst), 0);
        assert_eq!(willow_sched_task_state(id), 3); // Completed

        writer.join().unwrap();
        unsafe {
            libc::close(read_fd);
        }
        reset_global_netpoll_for_test();
        reset_internal_for_test();
    }

    #[cfg(windows)]
    unsafe extern "C" fn poll_socket_readable(frame: *mut c_void) -> i32 {
        let state = unsafe { &mut *(frame as *mut i64) };
        let socket = unsafe { *((frame as *mut u8).add(async_frame_slot_offset(0)) as *const i64) };
        *state += 1;
        if *state >= 2 {
            willow_netpoll_deregister(socket);
            RUNTIME_POLL_READY
        } else {
            let result = willow_netpoll_register(socket, WILLOW_NETPOLL_READABLE);
            NETPOLL_TEST_LAST_REGISTER.store(result, Ordering::SeqCst);
            if result != 0 {
                return RUNTIME_POLL_READY;
            }
            RUNTIME_POLL_PENDING
        }
    }

    #[cfg(windows)]
    #[test]
    fn netpoll_windows_socket_readiness_wakes_scheduler_idle_task() {
        use std::os::windows::io::AsRawSocket;

        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        reset_global_netpoll_for_test();
        NETPOLL_TEST_LAST_REGISTER.store(i32::MIN, Ordering::SeqCst);

        let receiver = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let sender = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        sender.connect(receiver.local_addr().unwrap()).unwrap();

        let frame = willow_async_frame_alloc(1, 0) as *mut u8;
        unsafe {
            *((frame.add(async_frame_slot_offset(0))) as *mut i64) =
                receiver.as_raw_socket() as i64;
        }
        let id = willow_sched_spawn(poll_socket_readable, frame as *mut c_void);
        let writer = std::thread::spawn(move || {
            while NETPOLL_TEST_LAST_REGISTER.load(Ordering::SeqCst) == i32::MIN {
                std::thread::sleep(Duration::from_millis(1));
            }
            sender.send(&[b'x']).unwrap();
        });

        assert_eq!(willow_sched_run(), 1);
        assert_eq!(NETPOLL_TEST_LAST_REGISTER.load(Ordering::SeqCst), 0);
        assert_eq!(willow_sched_task_state(id), 3);
        writer.join().unwrap();

        reset_global_netpoll_for_test();
        reset_internal_for_test();
    }

    // willow-vynv.1: a cancelled task's registrations are purged.
    #[test]
    fn netpoll_purge_task_removes_own_registrations() {
        let mut poll = RuntimeNetPoll::default();
        poll.register(IoRegistration::new(3, 9, IoInterest::Readable));
        poll.register(IoRegistration::new(4, 9, IoInterest::Writable));
        poll.register(IoRegistration::new(5, 10, IoInterest::Readable));
        poll.purge_task(9);
        assert_eq!(
            poll.registrations(),
            &[IoRegistration::new(5, 10, IoInterest::Readable)]
        );
    }

    #[test]
    fn netpoll_purge_task_keeps_fd_shared_with_live_task() {
        let mut poll = RuntimeNetPoll::default();
        poll.register(IoRegistration::new(7, 9, IoInterest::Readable));
        poll.register(IoRegistration::new(7, 10, IoInterest::Readable));
        poll.purge_task(9);
        assert_eq!(
            poll.registrations(),
            &[IoRegistration::new(7, 10, IoInterest::Readable)],
            "the live task's interest in the shared fd must survive"
        );
    }
}
