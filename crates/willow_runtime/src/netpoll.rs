use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

// Statically selected internal interface: Backend owns native resources and
// provides init/sync/poke/close; wait_ready_tasks releases the global lock while
// blocking. Registration indexes and the exported ABI stay in this module.
#[cfg(target_os = "linux")]
#[path = "netpoll/linux.rs"]
mod backend;
#[cfg(any(target_os = "macos", target_os = "ios"))]
#[path = "netpoll/apple.rs"]
mod backend;
#[cfg(target_os = "windows")]
#[path = "netpoll/windows.rs"]
mod backend;
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows"
)))]
#[path = "netpoll/unsupported.rs"]
mod backend;
#[cfg(any(target_os = "windows", test))]
mod windows_snapshot;
use backend::{PLATFORM_POLL_SUPPORTED, valid_native_handle};

use crate::task::RuntimeTaskId;
use crate::trace::{GcTrace, GcVisitor};

/// Native readiness handle. Unix file descriptors fit in this word; on Windows
/// it carries a WinSock `SOCKET` without truncating the 64-bit handle.
type RawFd = i64;

pub const WILLOW_NETPOLL_READABLE: i32 = 1;
pub const WILLOW_NETPOLL_WRITABLE: i32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

type RegistrationKey = (RawFd, RuntimeTaskId, IoInterest);

/// Where a live registration sits: its `registrations` index (kept dense
/// with `swap_remove`) and the monotonic sequence assigned when it was
/// registered, which keeps per-token wake order first-registered-first.
#[derive(Debug, Clone, Copy)]
struct RegistrationSlot {
    position: usize,
    sequence: u64,
}

#[derive(Debug)]
pub struct RuntimeNetPoll {
    registrations: Vec<IoRegistration>,
    slots: HashMap<RegistrationKey, RegistrationSlot>,
    next_sequence: u64,
    /// Waiters per token in registration order, so readiness wakes tasks
    /// sharing one descriptor FIFO instead of in hash order.
    by_token: HashMap<usize, BTreeMap<u64, RegistrationKey>>,
    by_task: HashMap<RuntimeTaskId, HashSet<RegistrationKey>>,
    by_fd: HashMap<RawFd, HashSet<RegistrationKey>>,
    interest_counts: HashMap<RawFd, [usize; 2]>,
    #[cfg(test)]
    ready_visits: std::cell::Cell<usize>,
    ready_tokens: VecDeque<usize>,
    backend: backend::Backend,
}

fn remove_index_key<K: std::hash::Hash + Eq>(
    index: &mut HashMap<K, HashSet<RegistrationKey>>,
    bucket: K,
    key: RegistrationKey,
) {
    if let Some(keys) = index.get_mut(&bucket) {
        keys.remove(&key);
        if keys.len() <= keys.capacity() / 4 {
            keys.shrink_to_fit();
        }
        if keys.is_empty() {
            index.remove(&bucket);
        }
    }
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
            slots: HashMap::new(),
            next_sequence: 0,
            by_token: HashMap::new(),
            by_task: HashMap::new(),
            by_fd: HashMap::new(),
            interest_counts: HashMap::new(),
            #[cfg(test)]
            ready_visits: std::cell::Cell::new(0),
            ready_tokens: VecDeque::new(),
            backend: backend::Backend::default(),
        }
    }

    pub fn init(&mut self) -> i32 {
        self.backend.init_platform()
    }

    pub fn register(&mut self, registration: IoRegistration) {
        let key = (registration.fd, registration.task_id, registration.interest);
        if self.slots.contains_key(&key) {
            return;
        }
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.slots.insert(
            key,
            RegistrationSlot {
                position: self.registrations.len(),
                sequence,
            },
        );
        self.by_token
            .entry(registration.token)
            .or_default()
            .insert(sequence, key);
        self.by_task
            .entry(registration.task_id)
            .or_default()
            .insert(key);
        self.by_fd.entry(registration.fd).or_default().insert(key);
        let counts = self.interest_counts.entry(registration.fd).or_default();
        counts[0] += usize::from(registration.interest.overlaps(IoInterest::Readable));
        counts[1] += usize::from(registration.interest.overlaps(IoInterest::Writable));
        self.registrations.push(registration);
    }

    fn remove_registration(&mut self, key: RegistrationKey) {
        let Some(slot) = self.slots.remove(&key) else {
            return;
        };
        let removed = self.registrations.swap_remove(slot.position);
        if let Some(moved) = self.registrations.get(slot.position) {
            self.slots
                .get_mut(&(moved.fd, moved.task_id, moved.interest))
                .expect("moved registration is indexed")
                .position = slot.position;
        }
        if let Some(waiters) = self.by_token.get_mut(&removed.token) {
            waiters.remove(&slot.sequence);
            if waiters.is_empty() {
                self.by_token.remove(&removed.token);
            }
        }
        remove_index_key(&mut self.by_task, removed.task_id, key);
        remove_index_key(&mut self.by_fd, removed.fd, key);
        let counts = self.interest_counts.get_mut(&removed.fd).unwrap();
        counts[0] -= usize::from(removed.interest.overlaps(IoInterest::Readable));
        counts[1] -= usize::from(removed.interest.overlaps(IoInterest::Writable));
        if *counts == [0, 0] {
            self.interest_counts.remove(&removed.fd);
            // HashMap iteration visits capacity, so keep Windows snapshots
            // proportional to live descriptors after cancellation churn.
            if self.interest_counts.len() <= self.interest_counts.capacity() / 4 {
                self.interest_counts.shrink_to_fit();
            }
        }
    }

    fn remove_task_fd(&mut self, fd: RawFd, task_id: RuntimeTaskId) {
        for interest in [
            IoInterest::Readable,
            IoInterest::Writable,
            IoInterest::ReadWrite,
        ] {
            self.remove_registration((fd, task_id, interest));
        }
    }

    fn merged_interest(&self, fd: RawFd) -> Option<IoInterest> {
        self.interest_counts
            .get(&fd)
            .map(|counts| match (counts[0] > 0, counts[1] > 0) {
                (true, true) => IoInterest::ReadWrite,
                (true, false) => IoInterest::Readable,
                (false, true) => IoInterest::Writable,
                (false, false) => unreachable!("empty interest entry"),
            })
    }

    pub fn reregister(&mut self, registration: IoRegistration) {
        self.remove_task_fd(registration.fd, registration.task_id);
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
            self.remove_registration((fd, task_id, interest));
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
            self.remove_registration((fd, task_id, interest));
            let _ = self.sync_platform_fd(fd);
        }
        rc
    }

    fn sync_platform_fd(&mut self, fd: RawFd) -> i32 {
        let interest = self.merged_interest(fd);
        self.backend.sync_platform_fd(fd, interest)
    }

    pub fn deregister_fd(&mut self, fd: RawFd) -> i32 {
        let keys: Vec<_> = self.by_fd.get(&fd).into_iter().flatten().copied().collect();
        for key in keys {
            self.remove_registration(key);
        }
        self.sync_platform_fd(fd)
    }

    /// Remove only one task's registrations for `fd`. Socket operations may
    /// have independent read and write waiters on the same native handle, so a
    /// completed operation must not deregister the other tasks.
    pub fn deregister_task_fd(&mut self, fd: RawFd, task_id: RuntimeTaskId) -> i32 {
        self.remove_task_fd(fd, task_id);
        self.sync_platform_fd(fd)
    }

    /// Current waiters, in unspecified order.
    pub fn registrations(&self) -> &[IoRegistration] {
        &self.registrations
    }

    pub fn has_waiters(&self) -> bool {
        !self.registrations.is_empty()
    }

    /// Remove every registration owned by `task_id`: a
    /// cancelled task must not linger as an I/O waiter, and its fds must not
    /// keep firing wakeups for a task that will never poll again.
    pub fn purge_task(&mut self, task_id: RuntimeTaskId) {
        let keys: Vec<_> = self
            .by_task
            .get(&task_id)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        let affected: HashSet<_> = keys.iter().map(|key| key.0).collect();
        for key in keys {
            self.remove_registration(key);
        }
        // Rebuild the kernel interest from the registrations that survived.
        // This is required even when another task still shares the fd: the
        // cancelled task may have been the only READABLE (or WRITABLE)
        // subscriber. Leaving the old merged READ|WRITE mask installed makes
        // a level-triggered backend repeatedly report an event for which no
        // live waiter exists.
        for fd in affected {
            let _ = self.sync_platform_fd(fd);
        }
    }

    pub fn ready_tasks(&self, token: usize) -> Vec<RuntimeTaskId> {
        self.ready_tasks_for(token, None)
    }

    fn ready_tasks_for(
        &self,
        token: usize,
        ready_interest: Option<IoInterest>,
    ) -> Vec<RuntimeTaskId> {
        self.ready_tasks_before(token, ready_interest, u64::MAX)
    }

    fn ready_tasks_before(
        &self,
        token: usize,
        ready_interest: Option<IoInterest>,
        sequence_limit: u64,
    ) -> Vec<RuntimeTaskId> {
        let mut seen = HashSet::new();
        let mut tasks = Vec::new();
        // Registration order is the wake order: a task that waited longer on
        // a shared token is woken before one that registered after it.
        for key in self
            .by_token
            .get(&token)
            .into_iter()
            .flat_map(|waiters| waiters.range(..sequence_limit).map(|(_, key)| key))
        {
            #[cfg(test)]
            self.ready_visits.set(self.ready_visits.get() + 1);
            let registration = &self.registrations[self.slots[key].position];
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
        self.backend.poke_platform_waker();
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
        self.slots.clear();
        self.by_token.clear();
        self.by_task.clear();
        self.by_fd.clear();
        self.interest_counts.clear();
        self.ready_tokens.clear();
        self.backend.close_platform();
    }
}

impl Drop for RuntimeNetPoll {
    fn drop(&mut self) {
        self.backend.close_platform();
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

static GLOBAL_NETPOLL: LazyLock<Mutex<RuntimeNetPoll>> =
    LazyLock::new(|| Mutex::new(RuntimeNetPoll::new()));

/// Mirrors `GLOBAL_NETPOLL.has_waiters()`, written under its lock, so idle
/// workers and the run loop ask without taking that lock.
static HAS_WAITERS: AtomicBool = AtomicBool::new(false);

fn with_global<R>(f: impl FnOnce(&mut RuntimeNetPoll) -> R) -> R {
    let mut guard = GLOBAL_NETPOLL.lock().expect("netpoll mutex poisoned");
    let result = f(&mut guard);
    HAS_WAITERS.store(guard.has_waiters(), Ordering::Release);
    result
}

pub(crate) fn has_waiters() -> bool {
    HAS_WAITERS.load(Ordering::Acquire)
}

/// Purge every registration owned by a cancelled task. Safe
/// to call with the scheduler lock held: the netpoll lock is only ever held
/// briefly (never across `epoll_wait`), and no path nests netpoll -> scheduler.
pub(crate) fn purge_task(task_id: RuntimeTaskId) {
    with_global(|poll| poll.purge_task(task_id));
}

/// Remove the running task's registration without disturbing other operations
/// that share the socket. Used by Willow-facing async network Tasks when an I/O
/// attempt completes after a readiness wake.
pub(crate) fn deregister_current(fd: RawFd) -> i32 {
    let current = crate::scheduler::willow_sched_current_task();
    if current == 0 {
        return -1;
    }
    deregister_task(fd, current)
}

/// Remove a known task's registration. Cancellation uses this before dropping
/// an in-progress connect socket so its native descriptor cannot be reused
/// while a stale poll registration still names the old task.
pub(crate) fn deregister_task(fd: RawFd, task_id: RuntimeTaskId) -> i32 {
    with_global(|poll| poll.deregister_task_fd(fd, task_id))
}

pub(crate) fn wait_and_wake(timeout: Option<Duration>) -> usize {
    let tasks = wait_ready_tasks(timeout);
    let count = tasks.len();
    for task_id in tasks {
        crate::observability::record(
            crate::observability::RuntimeEventKind::NetpollWake,
            None,
            task_id,
            1,
        );
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

    backend::wait_ready_tasks(timeout)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_netpoll_init() -> i32 {
    with_global(|poll| poll.init())
}

/// Register the current cooperative task for fd readiness. `interest` is a
/// bitmask: 1 readable, 2 writable, 3 both. Returns 0 on success.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_netpoll_register(fd: i64, interest: i32) -> i32 {
    let interest_bits = interest;
    let Some(interest) = IoInterest::from_bits(interest) else {
        return -1;
    };
    let current = crate::scheduler::willow_sched_current_task();
    let result = with_global(|poll| poll.register_fd(fd, current, interest));
    if result == 0 {
        crate::scheduler::note_netpoll_registration(current);
        crate::observability::record(
            crate::observability::RuntimeEventKind::NetpollWait,
            None,
            current,
            i64::from(interest_bits),
        );
    }
    crate::gc::stress_collect("scheduler");
    result
}

/// Replace the current task's registration for `fd`.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_netpoll_reregister(fd: i64, interest: i32) -> i32 {
    let interest_bits = interest;
    let Some(interest) = IoInterest::from_bits(interest) else {
        return -1;
    };
    let current = crate::scheduler::willow_sched_current_task();
    let result = with_global(|poll| poll.reregister_fd(fd, current, interest));
    if result == 0 {
        crate::scheduler::note_netpoll_registration(current);
        crate::observability::record(
            crate::observability::RuntimeEventKind::NetpollWait,
            None,
            current,
            i64::from(interest_bits),
        );
    }
    crate::gc::stress_collect("scheduler");
    result
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_netpoll_deregister(fd: i64) -> i32 {
    with_global(|poll| poll.deregister_fd(fd))
}

/// Wait for readiness and wake matching parked tasks. `timeout_ms < 0` waits
/// indefinitely; `0` polls; positive values bound the wait.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
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
#[willow_runtime_macros::ffi_boundary]
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
    #[cfg(target_os = "linux")]
    use super::backend::wait_platform_events;
    #[cfg(target_os = "windows")]
    use super::backend::wait_windows_events;
    use super::windows_snapshot::{WSA_POLL_READ, WSA_POLL_WRITE, fill_windows_sockets};
    #[test]
    fn windows_snapshot_reuses_storage_and_tracks_live_interests() {
        let mut poll = RuntimeNetPoll::new();
        let mut sockets = Vec::new();
        for count in [1, 1_000, 10_000, 20_000] {
            for fd in 1..=count {
                poll.register(IoRegistration::new(fd, fd as u64, IoInterest::Readable));
            }
            fill_windows_sockets(&mut sockets, &poll, 0);
            assert_eq!(sockets.len(), count as usize + 1);
            let pointer = sockets.as_ptr();
            let capacity = sockets.capacity();
            for _ in 0..4 {
                for socket in &mut sockets {
                    socket.revents = 123;
                }
                fill_windows_sockets(&mut sockets, &poll, 0);
                assert_eq!(sockets.as_ptr(), pointer);
                assert_eq!(sockets.capacity(), capacity);
                assert!(sockets.iter().all(|socket| socket.revents == 0));
            }
        }
        // Fragment the registration table, change interests, then shrink it.
        for fd in 1..=20_000 {
            if fd != 7 {
                poll.remove_task_fd(fd, fd as u64);
            }
        }
        poll.reregister(IoRegistration::new(7, 7, IoInterest::Writable));
        fill_windows_sockets(&mut sockets, &poll, 42);
        assert_eq!(sockets.len(), 2);
        assert_eq!(sockets[0].fd, 42);
        assert_eq!(sockets[1].events, WSA_POLL_WRITE);
        poll.register(IoRegistration::new(7, 8, IoInterest::Readable));
        fill_windows_sockets(&mut sockets, &poll, 42);
        assert_eq!(sockets[1].events, WSA_POLL_READ | WSA_POLL_WRITE);
    }

    #[test]
    fn windows_snapshot_rejects_replacement_and_cancelled_waiters() {
        for interest in [
            IoInterest::Readable,
            IoInterest::Writable,
            IoInterest::ReadWrite,
        ] {
            let mut poll = RuntimeNetPoll::new();
            poll.register(IoRegistration::new(7, 1, interest));
            poll.register(IoRegistration::new(7, 2, interest));
            let limit = poll.next_sequence;
            assert_eq!(
                poll.ready_tasks_before(7, Some(interest), limit),
                vec![1, 2]
            );
            poll.remove_task_fd(7, 1);
            poll.register(IoRegistration::new(7, 1, interest));
            poll.register(IoRegistration::new(7, 3, interest));
            assert_eq!(poll.ready_tasks_before(7, Some(interest), limit), vec![2]);
            poll.remove_task_fd(7, 2);
            // Includes disconnect/error (ReadWrite), cancel and same-task fd reuse.
            assert!(
                poll.ready_tasks_before(7, Some(IoInterest::ReadWrite), limit)
                    .is_empty()
            );
            assert_eq!(
                poll.ready_tasks_before(7, None, poll.next_sequence),
                vec![1, 3]
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_snapshot_native_read_write_disconnect() {
        use std::io::{Read, Write};
        use std::os::windows::io::AsRawSocket;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let wake = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let fd = server.as_raw_socket() as i64;
        let mut poll = RuntimeNetPoll::new();
        let mut sockets = Vec::new();
        poll.register(IoRegistration::new(fd, 1, IoInterest::Readable));
        poll.register(IoRegistration::new(fd, 2, IoInterest::Writable));
        fill_windows_sockets(&mut sockets, &poll, wake.as_raw_socket() as usize);
        let events = wait_windows_events(&mut sockets, Some(Duration::from_secs(1)));
        assert_eq!(poll.tasks_for_ready_events(events), vec![2]);
        poll.remove_task_fd(fd, 2);
        client.write_all(&[1]).unwrap();
        fill_windows_sockets(&mut sockets, &poll, wake.as_raw_socket() as usize);
        let limit = poll.next_sequence;
        let events = wait_windows_events(&mut sockets, Some(Duration::from_secs(1)));
        assert_eq!(events.len(), 1);
        assert_eq!(
            poll.ready_tasks_before(events[0].token, events[0].interest, limit),
            vec![1]
        );
        poll.purge_task(1);
        poll.register(IoRegistration::new(fd, 3, IoInterest::Readable));
        assert!(
            poll.ready_tasks_before(events[0].token, events[0].interest, limit)
                .is_empty()
        );
        server.read_exact(&mut [0]).unwrap();
        drop(client);
        fill_windows_sockets(&mut sockets, &poll, wake.as_raw_socket() as usize);
        let events = wait_windows_events(&mut sockets, Some(Duration::from_secs(1)));
        assert_eq!(poll.tasks_for_ready_events(events), vec![3]);
    }

    // One outstanding datagram at a time. The clock and poll/receive operation
    // are injected so delayed delivery and deadlines are tested deterministically.
    fn measurement_receive_pending(
        deadline: Duration,
        mut elapsed: impl FnMut() -> Duration,
        mut poll_receive: impl FnMut() -> std::io::Result<bool>,
    ) -> std::io::Result<(usize, Duration)> {
        let mut polls = 0;
        loop {
            if elapsed() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "WSAPoll measurement datagram delivery deadline exceeded",
                ));
            }
            polls += 1;
            match poll_receive() {
                Ok(true) => return Ok((polls, elapsed())),
                Ok(false) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
    }

    #[test]
    fn windows_measurement_delayed_receipt_counts_every_poll() {
        let mut steps = [
            Ok(false),
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
            Ok(false),
            Ok(true),
        ]
        .into_iter();
        let mut time = 0;
        let (polls, latency) = measurement_receive_pending(
            Duration::from_millis(10),
            || {
                time += 1;
                Duration::from_millis(time)
            },
            || steps.next().unwrap(),
        )
        .unwrap();
        assert_eq!(polls, 4);
        assert_eq!(latency, Duration::from_millis(5));
        assert!(steps.next().is_none());
    }

    #[test]
    fn windows_measurement_missing_receipt_has_bounded_deadline() {
        let mut time = 0;
        let mut polls = 0;
        let error = measurement_receive_pending(
            Duration::from_millis(4),
            || {
                time += 1;
                Duration::from_millis(time)
            },
            || {
                polls += 1;
                Ok(false)
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(polls, 3);
        let error = measurement_receive_pending(
            Duration::from_secs(1),
            || Duration::ZERO,
            || Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    }

    /// Run on Windows with --ignored --nocapture. No network peers or external services.
    #[cfg(windows)]
    #[test]
    #[ignore = "manual Windows WSAPoll scale measurement"]
    fn windows_wsapoll_scale_measurement() {
        use std::os::windows::io::AsRawSocket;
        use std::time::Instant;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn GetProcessTimes(
                process: *mut std::ffi::c_void,
                creation: *mut u64,
                exit: *mut u64,
                kernel: *mut u64,
                user: *mut u64,
            ) -> i32;
        }
        fn cpu_ns() -> u64 {
            let (mut creation, mut exit, mut kernel, mut user) = (0, 0, 0, 0);
            assert_ne!(
                unsafe {
                    GetProcessTimes(
                        GetCurrentProcess(),
                        &mut creation,
                        &mut exit,
                        &mut kernel,
                        &mut user,
                    )
                },
                0
            );
            (kernel + user) * 100
        }
        for count in [1, 1_000, 10_000, 10_001] {
            let receivers: Vec<_> = (0..count)
                .map(|_| {
                    let socket = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
                    socket.set_nonblocking(true).unwrap();
                    socket
                })
                .collect();
            let sender = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
            let wake = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
            let mut poll = RuntimeNetPoll::new();
            for (index, socket) in receivers.iter().enumerate() {
                poll.register(IoRegistration::new(
                    socket.as_raw_socket() as i64,
                    index as u64 + 1,
                    IoInterest::Readable,
                ));
            }
            for every in [1, 10, 100] {
                for reuse in [false, true] {
                    let mut sockets = Vec::new();
                    fill_windows_sockets(&mut sockets, &poll, wake.as_raw_socket() as usize);
                    let mut latencies = Vec::new();
                    let mut wakes = 0;
                    let cpu = cpu_ns();
                    let start = Instant::now();
                    let mut polls = 0;
                    for iteration in 0..1_000 {
                        let pending = if iteration % every == 0 {
                            let sent = Instant::now();
                            sender
                                .send_to(&[1], receivers[0].local_addr().unwrap())
                                .unwrap();
                            Some(sent)
                        } else {
                            None
                        };
                        let mut poll_receive = || -> std::io::Result<bool> {
                            if !reuse {
                                sockets = Vec::new();
                            }
                            fill_windows_sockets(
                                &mut sockets,
                                &poll,
                                wake.as_raw_socket() as usize,
                            );
                            let events = wait_windows_events(&mut sockets, Some(Duration::ZERO));
                            wakes += events.len();
                            if pending.is_some()
                                && events.iter().any(|event| {
                                    event.token == receivers[0].as_raw_socket() as usize
                                })
                            {
                                let mut byte = [0];
                                let received = receivers[0].recv(&mut byte)?;
                                assert_eq!(received, 1);
                                Ok(true)
                            } else {
                                Ok(false)
                            }
                        };
                        if let Some(sent) = pending {
                            let (attempts, latency) = measurement_receive_pending(
                                Duration::from_secs(5),
                                || sent.elapsed(),
                                &mut poll_receive,
                            )
                            .expect("pending measurement datagram must arrive within five seconds");
                            polls += attempts;
                            latencies.push(latency.as_nanos());
                        } else {
                            poll_receive().unwrap();
                            polls += 1;
                        }
                    }
                    let elapsed = start.elapsed();
                    let cpu = cpu_ns() - cpu;
                    latencies.sort_unstable();
                    println!(
                        "sockets={count} wake_every={every} reuse={reuse} polls={polls} polls_per_s={} wakes={wakes} latency_p50_ns={} latency_p99_ns={} cpu_ns={cpu}",
                        polls as f64 / elapsed.as_secs_f64(),
                        latencies[latencies.len() / 2],
                        latencies[(latencies.len() * 99 / 100).min(latencies.len() - 1)]
                    );
                }
            }
        }
    }

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

    fn assert_indexes(poll: &RuntimeNetPoll) {
        assert_eq!(poll.slots.len(), poll.registrations.len());
        assert_eq!(
            poll.by_token.values().map(BTreeMap::len).sum::<usize>(),
            poll.registrations.len()
        );
        assert_eq!(
            poll.by_task.values().map(HashSet::len).sum::<usize>(),
            poll.registrations.len()
        );
        assert_eq!(
            poll.by_fd.values().map(HashSet::len).sum::<usize>(),
            poll.registrations.len()
        );
        for (position, registration) in poll.registrations.iter().enumerate() {
            let key = (registration.fd, registration.task_id, registration.interest);
            let slot = poll.slots[&key];
            assert_eq!(slot.position, position);
            assert!(slot.sequence < poll.next_sequence);
            assert_eq!(poll.by_token[&registration.token][&slot.sequence], key);
            assert!(poll.by_task[&registration.task_id].contains(&key));
            assert!(poll.by_fd[&registration.fd].contains(&key));
        }
        for (&fd, counts) in &poll.interest_counts {
            let mut expected = [0, 0];
            for registration in poll.registrations.iter().filter(|r| r.fd == fd) {
                expected[0] += usize::from(registration.interest.overlaps(IoInterest::Readable));
                expected[1] += usize::from(registration.interest.overlaps(IoInterest::Writable));
            }
            assert_eq!(*counts, expected);
        }
    }

    #[test]
    fn netpoll_index_scaling_and_mutations() {
        for n in [64, 128, 256, 512] {
            let mut poll = RuntimeNetPoll::new();
            for fd in 0..n {
                poll.register(IoRegistration::new(
                    fd,
                    fd as RuntimeTaskId + 1,
                    IoInterest::Readable,
                ));
            }
            // These are exactly the candidate buckets consumed by readiness;
            // unrelated registrations contribute no candidate visits.
            assert_eq!(poll.ready_tasks(0), vec![1]);
            assert_eq!(poll.ready_visits.replace(0), 1);
            let mut visits = 0;
            for token in 0..n as usize {
                visits += poll.by_token[&token].len();
                assert_eq!(poll.ready_tasks(token), vec![token as RuntimeTaskId + 1]);
            }
            assert_eq!(visits, n as usize);
            assert_eq!(poll.ready_visits.get(), n as usize);
            for task in 1..=n as RuntimeTaskId {
                assert_eq!(poll.by_task[&task].len(), 1);
                poll.purge_task(task);
            }
            assert_indexes(&poll);
            assert!(poll.slots.is_empty());
            assert!(poll.by_fd.is_empty());
            assert!(poll.by_token.is_empty());
            assert!(poll.by_task.is_empty());
            assert!(poll.interest_counts.is_empty());
        }
    }

    #[test]
    fn netpoll_shared_bucket_shrinks_after_cancellation() {
        let mut poll = RuntimeNetPoll::new();
        for task in 1..=512 {
            poll.register(IoRegistration::new(8, task, IoInterest::Readable));
        }
        for task in 1..512 {
            poll.purge_task(task);
        }
        assert_eq!(poll.ready_tasks(8), vec![512]);
        assert_eq!(poll.ready_visits.get(), 1);
        assert_eq!(poll.by_token[&8].len(), 1);
        assert!(poll.by_fd[&8].capacity() <= 4);
        assert_indexes(&poll);
    }

    #[test]
    fn netpoll_wakes_shared_token_waiters_in_registration_order() {
        let mut poll = RuntimeNetPoll::new();
        for task in [5, 3, 9, 1] {
            poll.register(IoRegistration::new(8, task, IoInterest::Readable));
        }
        // Tasks sharing a token wake first-registered-first, not in hash order.
        assert_eq!(poll.ready_tasks(8), vec![5, 3, 9, 1]);
        assert_eq!(poll.ready_visits.replace(0), 4);
        poll.purge_task(3);
        assert_eq!(poll.ready_tasks(8), vec![5, 9, 1]);
        // Re-registering moves a waiter to the back of the line.
        poll.register(IoRegistration::new(8, 3, IoInterest::Readable));
        poll.reregister(IoRegistration::new(8, 5, IoInterest::Writable));
        assert_eq!(poll.ready_tasks(8), vec![9, 1, 3, 5]);
        assert_eq!(
            poll.ready_tasks_for(8, Some(IoInterest::Readable)),
            vec![9, 1, 3]
        );
        assert_eq!(
            poll.tasks_for_ready_events(vec![
                ReadyEvent {
                    token: 8,
                    interest: Some(IoInterest::Writable),
                },
                ReadyEvent {
                    token: 8,
                    interest: None,
                },
            ]),
            vec![5, 9, 1, 3]
        );
        // A second poller running the same sequence sees the same order.
        let mut twin = RuntimeNetPoll::new();
        for task in [5, 3, 9, 1] {
            twin.register(IoRegistration::new(8, task, IoInterest::Readable));
        }
        twin.purge_task(3);
        twin.register(IoRegistration::new(8, 3, IoInterest::Readable));
        twin.reregister(IoRegistration::new(8, 5, IoInterest::Writable));
        assert_eq!(twin.ready_tasks(8), poll.ready_tasks(8));
        assert_indexes(&poll);
        assert_indexes(&twin);
    }

    #[test]
    fn netpoll_indexes_preserve_filtering_dedup_and_reuse() {
        let mut poll = RuntimeNetPoll::new();
        for interest in [
            IoInterest::Readable,
            IoInterest::Writable,
            IoInterest::ReadWrite,
        ] {
            poll.register(IoRegistration {
                fd: 8,
                token: 100,
                task_id: 1,
                interest,
            });
        }
        poll.register(IoRegistration {
            fd: 9,
            token: 100,
            task_id: 2,
            interest: IoInterest::Writable,
        });
        assert_eq!(
            poll.ready_tasks_for(100, Some(IoInterest::Readable)),
            vec![1]
        );
        assert_eq!(
            poll.ready_tasks_for(100, Some(IoInterest::Writable)),
            vec![1, 2]
        );
        assert_indexes(&poll);
        poll.reregister(IoRegistration::new(8, 1, IoInterest::Readable));
        assert_eq!(poll.ready_tasks(100), vec![2]);
        assert_eq!(poll.merged_interest(8), Some(IoInterest::Readable));
        assert_indexes(&poll);
        poll.deregister_fd(8);
        poll.register(IoRegistration::new(8, 3, IoInterest::Writable));
        assert_eq!(poll.ready_tasks(8), vec![3]);
        assert_indexes(&poll);
        poll.purge_task(2);
        assert!(poll.ready_tasks(100).is_empty());
        assert_indexes(&poll);
        let events = vec![
            ReadyEvent {
                token: 8,
                interest: None
            };
            3
        ];
        assert_eq!(poll.tasks_for_ready_events(events), vec![3]);
        poll.reset_for_test();
        assert_indexes(&poll);
        assert!(poll.interest_counts.is_empty());
    }

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
        assert_indexes(&poll);
        assert_eq!(
            poll.reregister_fd(file.as_raw_fd().into(), 9, IoInterest::Writable),
            -1
        );
        assert_indexes(&poll);
        assert!(!poll.has_waiters());
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

    #[test]
    fn netpoll_task_deregister_preserves_other_waiters_on_shared_fd() {
        let mut poll = RuntimeNetPoll::default();
        poll.register(IoRegistration::new(3, 9, IoInterest::Readable));
        poll.register(IoRegistration::new(3, 10, IoInterest::Writable));
        poll.deregister_task_fd(3, 9);
        assert_eq!(poll.ready_tasks(3), vec![10]);
        assert_eq!(
            poll.registrations(),
            &[IoRegistration::new(3, 10, IoInterest::Writable)]
        );
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
        assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
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
        assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped

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
            sender.send(b"x").unwrap();
        });

        assert_eq!(willow_sched_run(), 1);
        assert_eq!(NETPOLL_TEST_LAST_REGISTER.load(Ordering::SeqCst), 0);
        assert_eq!(willow_sched_task_state(id), -1);
        writer.join().unwrap();

        reset_global_netpoll_for_test();
        reset_internal_for_test();
    }

    // a cancelled task's registrations are purged.
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

    #[cfg(target_os = "linux")]
    #[test]
    fn netpoll_purge_task_resyncs_shared_fd_platform_interest() {
        use std::io::Write;
        use std::os::fd::AsRawFd;

        let (socket, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        let fd = i64::from(socket.as_raw_fd());

        let mut poll = RuntimeNetPoll::default();
        assert_eq!(poll.register_fd(fd, 9, IoInterest::Readable), 0);
        assert_eq!(poll.register_fd(fd, 10, IoInterest::Writable), 0);
        peer.write_all(b"ready").unwrap();

        poll.purge_task(9);
        assert_eq!(
            poll.registrations(),
            &[IoRegistration::new(fd, 10, IoInterest::Writable)]
        );

        let events = wait_platform_events(
            poll.backend
                .epoll_fd
                .expect("epoll initialized by register_fd"),
            Some(Duration::from_millis(20)),
        );
        let socket_events: Vec<_> = events
            .into_iter()
            .filter(|event| event.token == fd as usize)
            .collect();
        assert!(
            !socket_events.is_empty(),
            "the live writer stays registered"
        );
        assert!(
            socket_events
                .iter()
                .all(|event| event.interest == Some(IoInterest::Writable)),
            "the cancelled reader interest must be removed from epoll: {socket_events:?}"
        );
    }
}
