//! Scheduler-aware TCP runtime for `std::net` (willow-2s3.1).
//!
//! Listener and stream handles are GC-managed objects with finalizers that
//! close their native sockets. `connect_async`, `accept_async`, `read_async`,
//! and `write_async` return ordinary Willow Tasks. Their poll functions make
//! one non-blocking socket attempt and register the current task with netpoll
//! on `WouldBlock`; no scheduler worker waits in an OS socket call.

use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use socket2::{Domain, Protocol, Socket, Type};

use crate::gc::{
    GcObjectKind, GcStoreDestination, willow_gc_write_barrier, willow_pop_roots, willow_push_root,
};
use crate::string::willow_string_as_str;

const NETWORK_HANDLE_TYPE_ID: u32 = 0x4E45_5401;
const NET_TASK_RESULT_SLOT: usize = 0;
const NET_TASK_ID_SLOT: usize = 1;
const NET_TASK_HANDLE_SLOT: usize = 2;
const NET_TASK_OPERATION_SLOT: usize = 3;
const MAX_READ_BYTES: usize = 16 * 1024 * 1024;

enum NetworkHandle {
    Listener(TcpListener),
    Stream(TcpStream),
}

unsafe fn drop_network_handle(payload: *mut u8) {
    unsafe { std::ptr::drop_in_place(payload.cast::<NetworkHandle>()) };
    #[cfg(test)]
    NETWORK_HANDLE_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
}

#[cfg(test)]
static NETWORK_HANDLE_DROP_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

static NETWORK_REGISTERED_GENERATION: AtomicU64 = AtomicU64::new(0);
static NETWORK_REGISTRATION_LOCK: Mutex<()> = Mutex::new(());

fn ensure_network_handle_registered() {
    let generation = crate::gc::registry_generation();
    if NETWORK_REGISTERED_GENERATION.load(Ordering::Acquire) == generation {
        return;
    }
    let _registration = NETWORK_REGISTRATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = crate::gc::registry_generation();
    if NETWORK_REGISTERED_GENERATION.load(Ordering::Acquire) == generation {
        return;
    }
    crate::gc::willow_register_drop(NETWORK_HANDLE_TYPE_ID, drop_network_handle);
    NETWORK_REGISTERED_GENERATION.store(generation, Ordering::Release);
}

fn alloc_handle(handle: NetworkHandle) -> *mut u8 {
    ensure_network_handle_registered();
    let payload = crate::gc::willow_alloc_with_layout(
        GcObjectKind::Class,
        NETWORK_HANDLE_TYPE_ID,
        std::mem::size_of::<NetworkHandle>() as i64,
        0,
    );
    if !payload.is_null() {
        unsafe { payload.cast::<NetworkHandle>().write(handle) };
    }
    payload
}

unsafe fn handle<'a>(payload: *mut u8) -> Option<&'a NetworkHandle> {
    unsafe { payload.cast::<NetworkHandle>().as_ref() }
}

fn ok_handle(handle: NetworkHandle) -> *mut u8 {
    let mut handle = alloc_handle(handle);
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    willow_push_root(&mut handle as *mut *mut u8);
    let result = crate::fs::alloc_ok(handle as i64, true);
    willow_pop_roots(1);
    result
}

fn ok_string(value: &str) -> *mut u8 {
    let mut string = crate::string::willow_string_from_str(value);
    willow_push_root(&mut string as *mut *mut u8);
    let result = crate::fs::alloc_ok(string as i64, true);
    willow_pop_roots(1);
    result
}

fn parse_address(address: *const u8) -> Result<(String, SocketAddr), *mut u8> {
    let address = unsafe { willow_string_as_str(address) }.to_string();
    match address.parse::<SocketAddr>() {
        Ok(parsed) => Ok((address, parsed)),
        Err(error) => Err(crate::fs::alloc_io_err(&format!(
            "{address}: invalid numeric socket address: {error}"
        ))),
    }
}

/// Bind a non-blocking TCP listener. Address resolution is deliberately not
/// hidden in this operation: v1 accepts numeric `IP:port` strings, avoiding a
/// DNS lookup on a scheduler worker.
#[unsafe(no_mangle)]
pub extern "C" fn willow_net_bind(address: *const u8) -> *mut u8 {
    let (address, parsed) = match parse_address(address) {
        Ok(value) => value,
        Err(error) => return error,
    };
    match TcpListener::bind(parsed).and_then(|listener| {
        listener.set_nonblocking(true)?;
        Ok(listener)
    }) {
        Ok(listener) => ok_handle(NetworkHandle::Listener(listener)),
        Err(error) => crate::fs::alloc_io_err(&format!("{address}: {error}")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_net_local_addr(listener: *mut u8) -> *mut u8 {
    let Some(NetworkHandle::Listener(listener)) = (unsafe { handle(listener) }) else {
        return crate::fs::alloc_io_err("net::local_addr: invalid TcpListener");
    };
    match listener.local_addr() {
        Ok(address) => ok_string(&address.to_string()),
        Err(error) => crate::fs::alloc_io_err(&format!("net::local_addr: {error}")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_net_peer_addr(stream: *mut u8) -> *mut u8 {
    let Some(NetworkHandle::Stream(stream)) = (unsafe { handle(stream) }) else {
        return crate::fs::alloc_io_err("net::peer_addr: invalid TcpStream");
    };
    match stream.peer_addr() {
        Ok(address) => ok_string(&address.to_string()),
        Err(error) => crate::fs::alloc_io_err(&format!("net::peer_addr: {error}")),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_net_shutdown(stream: *mut u8) -> *mut u8 {
    let Some(NetworkHandle::Stream(stream)) = (unsafe { handle(stream) }) else {
        return crate::fs::alloc_io_err("net::shutdown: invalid TcpStream");
    };
    match stream.shutdown(Shutdown::Both) {
        Ok(()) => crate::fs::alloc_ok(0, false),
        Err(error) => crate::fs::alloc_io_err(&format!("net::shutdown: {error}")),
    }
}

enum NetOperation {
    Connect {
        address_text: String,
        address: SocketAddr,
        socket: Option<Socket>,
        started: bool,
    },
    Accept,
    Read {
        max_bytes: usize,
        bytes: Vec<u8>,
    },
    Write {
        bytes: Vec<u8>,
        offset: usize,
    },
    ImmediateError(String),
}

unsafe fn frame_slot<T>(frame: *mut c_void, slot: usize) -> *mut T {
    unsafe {
        (frame as *mut u8)
            .add(crate::async_frame::async_frame_slot_offset(slot))
            .cast()
    }
}

unsafe fn operation(frame: *mut c_void) -> Option<&'static mut NetOperation> {
    let raw = unsafe { *frame_slot::<*mut NetOperation>(frame, NET_TASK_OPERATION_SLOT) };
    unsafe { raw.as_mut() }
}

unsafe fn task_handle(frame: *mut c_void) -> *mut u8 {
    unsafe { *frame_slot::<*mut u8>(frame, NET_TASK_HANDLE_SLOT) }
}

#[cfg(unix)]
fn raw_socket<T: std::os::fd::AsRawFd>(socket: &T) -> i64 {
    i64::from(socket.as_raw_fd())
}

#[cfg(windows)]
fn raw_socket<T: std::os::windows::io::AsRawSocket>(socket: &T) -> i64 {
    socket.as_raw_socket() as i64
}

fn connect_in_progress(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock
        || error.kind() == std::io::ErrorKind::Interrupted
    {
        return true;
    }
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::EINPROGRESS)
            || error.raw_os_error() == Some(libc::EALREADY)
    }
    #[cfg(windows)]
    {
        // WSAEINPROGRESS / WSAEALREADY / WSAEWOULDBLOCK.
        matches!(error.raw_os_error(), Some(10036 | 10037 | 10035))
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

fn register(fd: i64, interest: i32) -> Result<(), String> {
    let result = crate::netpoll::willow_netpoll_register(fd, interest);
    if result == 0 {
        Ok(())
    } else {
        Err("netpoll registration failed".to_string())
    }
}

unsafe fn store_handle(frame: *mut c_void, handle: *mut u8) {
    unsafe {
        willow_gc_write_barrier(
            frame.cast::<u8>(),
            handle,
            GcStoreDestination::AsyncFrameSlot as i64,
        );
        *frame_slot::<*mut u8>(frame, NET_TASK_HANDLE_SLOT) = handle;
    }
}

unsafe fn finish(frame: *mut c_void, result: *mut u8, fd: Option<i64>) -> i32 {
    if let Some(fd) = fd {
        crate::netpoll::deregister_current(fd);
    }
    unsafe {
        willow_gc_write_barrier(
            frame.cast::<u8>(),
            result,
            GcStoreDestination::AsyncFrameSlot as i64,
        );
        *frame_slot::<*mut u8>(frame, NET_TASK_RESULT_SLOT) = result;
        let operation_slot = frame_slot::<*mut NetOperation>(frame, NET_TASK_OPERATION_SLOT);
        let raw = *operation_slot;
        *operation_slot = std::ptr::null_mut();
        if !raw.is_null() {
            drop(Box::from_raw(raw));
        }
    }
    crate::task::RUNTIME_POLL_READY
}

unsafe fn finish_error(frame: *mut c_void, message: &str, fd: Option<i64>) -> i32 {
    let error = crate::fs::alloc_io_err(message);
    unsafe { finish(frame, error, fd) }
}

unsafe fn poll_connect(frame: *mut c_void) -> i32 {
    let Some(NetOperation::Connect {
        address_text,
        address,
        socket,
        started,
    }) = (unsafe { operation(frame) })
    else {
        return unsafe { finish_error(frame, "net::connect_async: invalid operation", None) };
    };

    let Some(active) = socket.as_ref() else {
        return unsafe { finish_error(frame, "net::connect_async: missing socket", None) };
    };
    let fd = raw_socket(active);
    if !*started {
        *started = true;
        match active.connect(&(*address).into()) {
            Ok(()) => {}
            Err(error) if connect_in_progress(&error) => {
                return match register(fd, crate::netpoll::WILLOW_NETPOLL_WRITABLE) {
                    Ok(()) => crate::task::RUNTIME_POLL_PENDING,
                    Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
                };
            }
            Err(error) => {
                let message = format!("{address_text}: {error}");
                return unsafe { finish_error(frame, &message, Some(fd)) };
            }
        }
    } else {
        match active.take_error() {
            Ok(Some(error)) => {
                let message = format!("{address_text}: {error}");
                return unsafe { finish_error(frame, &message, Some(fd)) };
            }
            Err(error) => {
                let message = format!("{address_text}: {error}");
                return unsafe { finish_error(frame, &message, Some(fd)) };
            }
            Ok(None) => {
                if let Err(error) = active.peer_addr() {
                    if connect_in_progress(&error)
                        || error.kind() == std::io::ErrorKind::NotConnected
                    {
                        return match register(fd, crate::netpoll::WILLOW_NETPOLL_WRITABLE) {
                            Ok(()) => crate::task::RUNTIME_POLL_PENDING,
                            Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
                        };
                    }
                    let message = format!("{address_text}: {error}");
                    return unsafe { finish_error(frame, &message, Some(fd)) };
                }
            }
        }
    }

    let socket = socket.take().expect("connected socket checked above");
    let stream: TcpStream = socket.into();
    let handle = alloc_handle(NetworkHandle::Stream(stream));
    if handle.is_null() {
        return unsafe { finish(frame, std::ptr::null_mut(), Some(fd)) };
    }
    unsafe { store_handle(frame, handle) };
    let result = crate::fs::alloc_ok(handle as i64, true);
    unsafe { finish(frame, result, Some(fd)) }
}

unsafe fn poll_accept(frame: *mut c_void) -> i32 {
    let handle_ptr = unsafe { task_handle(frame) };
    let Some(NetworkHandle::Listener(listener)) = (unsafe { handle(handle_ptr) }) else {
        return unsafe { finish_error(frame, "net::accept_async: invalid TcpListener", None) };
    };
    let fd = raw_socket(listener);
    match listener.accept() {
        Ok((stream, _)) => {
            if let Err(error) = stream.set_nonblocking(true) {
                return unsafe {
                    finish_error(frame, &format!("net::accept_async: {error}"), Some(fd))
                };
            }
            let accepted = alloc_handle(NetworkHandle::Stream(stream));
            if accepted.is_null() {
                return unsafe { finish(frame, std::ptr::null_mut(), Some(fd)) };
            }
            let mut accepted_root = accepted;
            willow_push_root(&mut accepted_root as *mut *mut u8);
            let result = crate::fs::alloc_ok(accepted_root as i64, true);
            willow_pop_roots(1);
            unsafe { finish(frame, result, Some(fd)) }
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            match register(fd, crate::netpoll::WILLOW_NETPOLL_READABLE) {
                Ok(()) => crate::task::RUNTIME_POLL_PENDING,
                Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
            }
        }
        Err(error) => unsafe {
            finish_error(frame, &format!("net::accept_async: {error}"), Some(fd))
        },
    }
}

unsafe fn poll_read(frame: *mut c_void, max_bytes: usize, buffered: &mut Vec<u8>) -> i32 {
    let handle_ptr = unsafe { task_handle(frame) };
    let Some(NetworkHandle::Stream(stream)) = (unsafe { handle(handle_ptr) }) else {
        return unsafe { finish_error(frame, "net::read_async: invalid TcpStream", None) };
    };
    let fd = raw_socket(stream);
    let remaining = max_bytes.saturating_sub(buffered.len());
    debug_assert!(remaining > 0, "incomplete UTF-8 is rejected before re-poll");
    let mut bytes = vec![0_u8; remaining];
    let mut stream_ref = stream;
    match stream_ref.read(&mut bytes) {
        Ok(read) => {
            buffered.extend_from_slice(&bytes[..read]);
            match std::str::from_utf8(buffered) {
                Ok(text) => {
                    let result = ok_string(text);
                    unsafe { finish(frame, result, Some(fd)) }
                }
                Err(error) if error.error_len().is_none() && buffered.len() < max_bytes => {
                    match register(fd, crate::netpoll::WILLOW_NETPOLL_READABLE) {
                        Ok(()) => crate::task::RUNTIME_POLL_PENDING,
                        Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
                    }
                }
                Err(error) => unsafe {
                    finish_error(
                        frame,
                        &format!("net::read_async: response is not UTF-8: {error}"),
                        Some(fd),
                    )
                },
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            match register(fd, crate::netpoll::WILLOW_NETPOLL_READABLE) {
                Ok(()) => crate::task::RUNTIME_POLL_PENDING,
                Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
            }
        }
        Err(error) => unsafe {
            finish_error(frame, &format!("net::read_async: {error}"), Some(fd))
        },
    }
}

unsafe fn poll_write(frame: *mut c_void, bytes: &[u8], offset: &mut usize) -> i32 {
    let handle_ptr = unsafe { task_handle(frame) };
    let Some(NetworkHandle::Stream(stream)) = (unsafe { handle(handle_ptr) }) else {
        return unsafe { finish_error(frame, "net::write_async: invalid TcpStream", None) };
    };
    let fd = raw_socket(stream);
    if bytes.is_empty() {
        let result = crate::fs::alloc_ok(0, false);
        return unsafe { finish(frame, result, Some(fd)) };
    }
    let mut stream_ref = stream;
    match stream_ref.write(&bytes[*offset..]) {
        Ok(0) => unsafe {
            finish_error(
                frame,
                "net::write_async: socket closed before all bytes were written",
                Some(fd),
            )
        },
        Ok(written) => {
            *offset += written;
            if *offset == bytes.len() {
                let result = crate::fs::alloc_ok(0, false);
                unsafe { finish(frame, result, Some(fd)) }
            } else {
                match register(fd, crate::netpoll::WILLOW_NETPOLL_WRITABLE) {
                    Ok(()) => crate::task::RUNTIME_POLL_PENDING,
                    Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            match register(fd, crate::netpoll::WILLOW_NETPOLL_WRITABLE) {
                Ok(()) => crate::task::RUNTIME_POLL_PENDING,
                Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
            }
        }
        Err(error) => unsafe {
            finish_error(frame, &format!("net::write_async: {error}"), Some(fd))
        },
    }
}

unsafe extern "C" fn poll_net_operation(frame: *mut c_void) -> i32 {
    let Some(operation) = (unsafe { operation(frame) }) else {
        return crate::task::RUNTIME_POLL_READY;
    };
    match operation {
        NetOperation::Connect { .. } => unsafe { poll_connect(frame) },
        NetOperation::Accept => unsafe { poll_accept(frame) },
        NetOperation::Read { max_bytes, bytes } => unsafe { poll_read(frame, *max_bytes, bytes) },
        NetOperation::Write { bytes, offset } => unsafe { poll_write(frame, bytes, offset) },
        NetOperation::ImmediateError(message) => {
            let message = std::mem::take(message);
            unsafe { finish_error(frame, &message, None) }
        }
    }
}

unsafe extern "C" fn cancel_net_operation(frame: *mut c_void) {
    unsafe {
        let slot = frame_slot::<*mut NetOperation>(frame, NET_TASK_OPERATION_SLOT);
        let raw = *slot;
        *slot = std::ptr::null_mut();
        if !raw.is_null() {
            let task_id = *frame_slot::<u64>(frame, NET_TASK_ID_SLOT);
            let operation = &*raw;
            let fd = match operation {
                NetOperation::Connect { socket, .. } => socket.as_ref().map(raw_socket),
                NetOperation::Accept | NetOperation::Read { .. } | NetOperation::Write { .. } => {
                    match handle(task_handle(frame)) {
                        Some(NetworkHandle::Listener(listener)) => Some(raw_socket(listener)),
                        Some(NetworkHandle::Stream(stream)) => Some(raw_socket(stream)),
                        None => None,
                    }
                }
                NetOperation::ImmediateError(_) => None,
            };
            if let Some(fd) = fd {
                crate::netpoll::deregister_task(fd, task_id);
            }
            drop(Box::from_raw(raw));
        }
    }
}

fn spawn_operation(handle: *mut u8, operation: NetOperation) -> *mut c_void {
    // Slot 0 is Result<_, IoError>; slot 2 keeps the listener/stream alive
    // through readiness waits and cancellation cleanup.
    let frame = crate::async_frame::willow_async_frame_alloc(4, 0b0101);
    if frame.is_null() {
        return frame;
    }
    unsafe {
        *((frame as *mut u8)
            .add(crate::async_frame::ASYNC_FRAME_SLOT_COUNT_OFFSET)
            .cast::<i64>()) = 4;
        store_handle(frame, handle);
        *frame_slot::<*mut NetOperation>(frame, NET_TASK_OPERATION_SLOT) =
            Box::into_raw(Box::new(operation));
    }
    crate::scheduler::spawn_global_task_initialized(
        poll_net_operation,
        frame,
        Some(cancel_net_operation),
        |task_id| unsafe { *frame_slot::<u64>(frame, NET_TASK_ID_SLOT) = task_id },
    );
    frame
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_net_connect_async(address: *const u8) -> *mut c_void {
    let address_text = unsafe { willow_string_as_str(address) }.to_string();
    let parsed = match address_text.parse::<SocketAddr>() {
        Ok(address) => address,
        Err(error) => {
            return spawn_operation(
                std::ptr::null_mut(),
                NetOperation::ImmediateError(format!(
                    "{address_text}: invalid numeric socket address: {error}"
                )),
            );
        }
    };
    let domain = if parsed.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = match Socket::new(domain, Type::STREAM, Some(Protocol::TCP)).and_then(|socket| {
        socket.set_nonblocking(true)?;
        Ok(socket)
    }) {
        Ok(socket) => Some(socket),
        Err(error) => {
            return spawn_operation(
                std::ptr::null_mut(),
                NetOperation::ImmediateError(format!("{address_text}: {error}")),
            );
        }
    };
    spawn_operation(
        std::ptr::null_mut(),
        NetOperation::Connect {
            address_text,
            address: parsed,
            socket,
            started: false,
        },
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_net_accept_async(listener: *mut u8) -> *mut c_void {
    spawn_operation(listener, NetOperation::Accept)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_net_read_async(stream: *mut u8, max_bytes: i64) -> *mut c_void {
    let operation = match usize::try_from(max_bytes) {
        Ok(max_bytes) if (1..=MAX_READ_BYTES).contains(&max_bytes) => NetOperation::Read {
            max_bytes,
            bytes: Vec::new(),
        },
        _ => NetOperation::ImmediateError(format!(
            "net::read_async: max_bytes must be between 1 and {MAX_READ_BYTES}"
        )),
    };
    spawn_operation(stream, operation)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_net_write_async(stream: *mut u8, contents: *const u8) -> *mut c_void {
    let bytes = unsafe { willow_string_as_str(contents) }
        .as_bytes()
        .to_vec();
    spawn_operation(stream, NetOperation::Write { bytes, offset: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{reset_internal_for_test, runtime_test_guard};
    use crate::scheduler::{reset_global_scheduler_for_test, willow_sched_run_until};
    use crate::string::willow_string_from_str;

    fn result_tag(result: *mut u8) -> i64 {
        unsafe { *(result.cast::<i64>()) }
    }

    #[test]
    fn bind_rejects_hostname_without_blocking_dns() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let result = willow_net_bind(willow_string_from_str("localhost:80"));
        assert_eq!(result_tag(result), 1);
        reset_internal_for_test();
    }

    #[test]
    fn connect_error_is_delivered_through_task_result() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let task = willow_net_connect_async(willow_string_from_str("invalid"));
        let id = unsafe { *frame_slot::<u64>(task, NET_TASK_ID_SLOT) };
        assert_eq!(willow_sched_run_until(id), 1);
        let result = unsafe { *frame_slot::<*mut u8>(task, NET_TASK_RESULT_SLOT) };
        assert_eq!(result_tag(result), 1);
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn unreachable_listener_runs_socket_finalizer() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        NETWORK_HANDLE_DROP_COUNT.store(0, Ordering::SeqCst);

        let mut result = willow_net_bind(willow_string_from_str("127.0.0.1:0"));
        assert_eq!(result_tag(result), 0);
        willow_push_root(&mut result as *mut *mut u8);
        crate::gc::willow_gc_collect();
        assert_eq!(NETWORK_HANDLE_DROP_COUNT.load(Ordering::SeqCst), 0);
        willow_pop_roots(1);
        crate::gc::willow_gc_collect();
        assert_eq!(NETWORK_HANDLE_DROP_COUNT.load(Ordering::SeqCst), 1);

        reset_internal_for_test();
    }
}
