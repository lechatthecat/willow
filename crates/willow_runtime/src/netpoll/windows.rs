use super::windows_snapshot::{WSA_POLL_READ, WSA_POLL_WRITE, WsaPollFd, fill_windows_sockets};
use super::{IoInterest, RawFd, RuntimeTaskId};
use super::{ReadyEvent, with_global};
use std::collections::HashSet;
use std::os::windows::io::AsRawSocket;
use std::time::Duration;

#[derive(Debug, Default)]
pub(super) struct Backend {
    wake_receiver: Option<std::sync::Arc<std::net::UdpSocket>>,
    wake_sender: Option<std::net::UdpSocket>,
    poll_sockets: Vec<WsaPollFd>,
}

pub(super) const PLATFORM_POLL_SUPPORTED: bool = true;

const WAKE_TOKEN: usize = usize::MAX;

pub(super) fn valid_native_handle(handle: RawFd) -> bool {
    // WinSock SOCKET is an unsigned pointer-sized integer. A valid socket may
    // therefore have bit 63 set when carried through the ABI's i64 word; only
    // INVALID_SOCKET (all bits set) is reserved.
    handle != -1
}

const WSA_POLL_ERROR: i16 = 0x0001 | 0x0002 | 0x0004;
const WSA_POLL_NVAL: i16 = 0x0004;

#[link(name = "ws2_32")]
unsafe extern "system" {
    fn WSAPoll(fd_array: *mut WsaPollFd, count: u32, timeout_ms: i32) -> i32;
}

impl Backend {
    pub(super) fn init_platform(&mut self) -> i32 {
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
        self.wake_receiver = Some(std::sync::Arc::new(receiver));
        self.wake_sender = Some(sender);
        0
    }

    pub(super) fn sync_platform_fd(&mut self, fd: RawFd, interest: Option<IoInterest>) -> i32 {
        if interest.is_none() {
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

    pub(super) fn poke_platform_waker(&self) {
        if let Some(sender) = &self.wake_sender {
            let _ = sender.send(&[1]);
        }
    }

    pub(super) fn drain_platform_waker(&self) {
        let Some(receiver) = &self.wake_receiver else {
            return;
        };
        let mut bytes = [0_u8; 64];
        while receiver.recv(&mut bytes).is_ok() {}
    }

    pub(super) fn close_platform(&mut self) {
        self.wake_sender = None;
        self.wake_receiver = None;
    }
}

pub(super) fn wait_ready_tasks(timeout: Option<Duration>) -> Vec<RuntimeTaskId> {
    let snapshot = with_global(|poll| {
        if !poll.has_waiters() || poll.init() != 0 {
            None
        } else {
            // Each in-flight wait owns its buffer; concurrent waiters never alias it.
            let wake = std::sync::Arc::clone(poll.backend.wake_receiver.as_ref()?);
            let mut sockets = std::mem::take(&mut poll.backend.poll_sockets);
            fill_windows_sockets(&mut sockets, poll, wake.as_raw_socket() as usize);
            Some((sockets, wake, poll.next_sequence))
        }
    });
    let Some((mut sockets, _wake, sequence_limit)) = snapshot else {
        return Vec::new();
    };
    let events = wait_windows_events(&mut sockets, timeout);
    with_global(|poll| {
        let mut tasks = Vec::new();
        let mut seen = HashSet::new();
        for event in events {
            let ready = if event.token == WAKE_TOKEN {
                poll.backend.drain_platform_waker();
                let queued = poll.drain_ready_tokens();
                poll.tasks_for_ready_events(queued)
            } else {
                // A descriptor can be cancelled, closed and reused while WSAPoll
                // runs. Only registrations present in this snapshot may wake.
                poll.ready_tasks_before(event.token, event.interest, sequence_limit)
            };
            tasks.extend(ready.into_iter().filter(|task| seen.insert(*task)));
        }
        sockets.clear();
        if sockets.capacity() > poll.backend.poll_sockets.capacity() {
            poll.backend.poll_sockets = sockets;
        }
        tasks
    })
}

pub(super) fn wait_windows_events(
    sockets: &mut [WsaPollFd],
    timeout: Option<Duration>,
) -> Vec<ReadyEvent> {
    let timeout_ms = match timeout {
        None => -1,
        Some(duration) => duration.as_millis().min(i32::MAX as u128) as i32,
    };
    let n = unsafe { WSAPoll(sockets.as_mut_ptr(), sockets.len() as u32, timeout_ms) };
    if n <= 0 {
        return Vec::new();
    }
    sockets
        .iter()
        .enumerate()
        .filter(|(_, socket)| socket.revents != 0)
        .map(|(index, socket)| {
            let token = if index == 0 { WAKE_TOKEN } else { socket.fd };
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
