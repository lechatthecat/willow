use super::{IoInterest, RuntimeNetPoll};

#[derive(Debug)]
#[repr(C)]
pub(super) struct WsaPollFd {
    pub(super) fd: usize,
    pub(super) events: i16,
    pub(super) revents: i16,
}

pub(super) const WSA_POLL_READ: i16 = 0x0100 | 0x0200;
pub(super) const WSA_POLL_WRITE: i16 = 0x0010;

pub(super) fn fill_windows_sockets(
    sockets: &mut Vec<WsaPollFd>,
    poll: &RuntimeNetPoll,
    wake_socket: usize,
) {
    sockets.clear();
    sockets.reserve(poll.interest_counts.len() + 1);
    sockets.push(WsaPollFd {
        fd: wake_socket,
        events: WSA_POLL_READ,
        revents: 0,
    });
    sockets.extend(poll.interest_counts.keys().map(|&fd| WsaPollFd {
        fd: fd as usize,
        events: match poll.merged_interest(fd).unwrap() {
            IoInterest::Readable => WSA_POLL_READ,
            IoInterest::Writable => WSA_POLL_WRITE,
            IoInterest::ReadWrite => WSA_POLL_READ | WSA_POLL_WRITE,
        },
        revents: 0,
    }));
}
