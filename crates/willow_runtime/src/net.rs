//! Scheduler-aware TCP runtime for `std::net`.
//!
//! Listener and stream handles are GC-managed objects with finalizers that
//! close their native sockets. `connect_async`, `accept_async`, `read_async`,
//! and `write_async` return ordinary Willow Tasks. Their poll functions make
//! one non-blocking socket attempt and register the current task with netpoll
//! on `WouldBlock`; no scheduler worker waits in an OS socket call.

use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
#[cfg(test)]
use std::sync::atomic::Ordering;

use socket2::{Domain, Protocol, Socket, Type};

use crate::gc::{GcObjectKind, willow_pop_roots, willow_push_root};
use crate::native_frame::{NativeFrameSpec, NativeTaskFrame};
use crate::string::willow_string_as_str;

use willow_abi::runtime_type_ids::NETWORK_HANDLE_TYPE_ID;
const NET_TASK_RESULT_SLOT: usize = 0;
const NET_TASK_ID_SLOT: usize = 1;
const NET_TASK_HANDLE_SLOT: usize = 2;
const NET_TASK_OPERATION_SLOT: usize = 3;
const MAX_READ_BYTES: usize = 16 * 1024 * 1024;

struct NetTaskFrame;

impl NativeFrameSpec for NetTaskFrame {
    const LAYOUT: willow_abi::NativeFrameLayout<'static> = willow_abi::NativeFrameLayout::new(&[
        willow_abi::SlotKind::GcRef,
        willow_abi::SlotKind::Word,
        willow_abi::SlotKind::GcRef,
        willow_abi::SlotKind::NativePtr,
    ]);
    const NAME: &'static str = "net operation";
}

unsafe fn net_frame(frame: *mut c_void) -> NativeTaskFrame<NetTaskFrame> {
    // SAFETY: The scheduler passes the frame allocated for this native callback and retains it
    // during the poll.
    unsafe { NativeTaskFrame::from_raw(frame) }
}

enum NetworkHandle {
    Listener(TcpListener),
    Stream(TcpStream),
}

unsafe fn drop_network_handle(payload: *mut u8) {
    // SAFETY: The collector calls the matching finalizer once before releasing the initialized,
    // word-aligned GC payload.
    unsafe { std::ptr::drop_in_place(payload.cast::<NetworkHandle>()) };
}

#[cfg(test)]
const NETWORK_HANDLE_DROP_COUNT: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::NetworkHandleDropCount);

const NETWORK_REGISTRATION: crate::gc::NativeGcRegistration =
    crate::gc::NativeGcRegistration::Network;
// SAFETY: The network handle constructor installs this ID only on its matching initialized
// representation. The callbacks use that same layout.
const NETWORK_GC_TYPES: &[crate::gc::NativeGcType] = &[unsafe {
    crate::gc::NativeGcType::new(NETWORK_HANDLE_TYPE_ID, None, Some(drop_network_handle))
}];

fn ensure_network_handle_registered() {
    // SAFETY: This is the complete descriptor set for the network handle registration group
    // and its constructors.
    unsafe { NETWORK_REGISTRATION.ensure(NETWORK_GC_TYPES) };
}

fn alloc_handle(handle: NetworkHandle) -> *mut u8 {
    ensure_network_handle_registered();
    const { assert!(align_of::<NetworkHandle>() <= align_of::<crate::gc::GcHeader>()) };
    // SAFETY: The constructor selects the matching payload layout and initializes its fields
    // before exposing it or reaching a GC point.
    let payload = unsafe {
        crate::gc::willow_alloc_with_layout(
            GcObjectKind::Class,
            NETWORK_HANDLE_TYPE_ID,
            std::mem::size_of::<NetworkHandle>() as i64,
            0,
        )
    };
    if !payload.is_null() {
        // SAFETY: The network constructor reserves word-aligned GC storage sized for
        // NetworkHandle. A non-null handle designates that payload.
        unsafe { payload.cast::<NetworkHandle>().write(handle) };
    }
    payload
}

unsafe fn handle<'a>(payload: *mut u8) -> Option<&'a NetworkHandle> {
    // SAFETY: The network constructor reserves word-aligned GC storage sized for NetworkHandle. A
    // non-null handle designates that payload.
    unsafe { payload.cast::<NetworkHandle>().as_ref() }
}

fn ok_handle(handle: NetworkHandle) -> *mut u8 {
    let mut handle = alloc_handle(handle);
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut handle) };
    let result = crate::fs::alloc_ok(handle as i64, true);
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(1) };
    result
}

fn ok_string(value: &str) -> *mut u8 {
    let mut string = crate::string::willow_string_from_str(value);
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut string) };
    let result = crate::fs::alloc_ok(string as i64, true);
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(1) };
    result
}

fn parse_address(address: *const u8) -> Result<(String, SocketAddr), *mut u8> {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
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
///
/// # Safety
/// A non-null address must be a current immutable WillowString through the address copy.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_net_bind(address: *const u8) -> *mut u8 {
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

/// # Safety
/// A non-null `listener` must be a live network handle allocated by this runtime.
/// Keep it reachable through the call.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_net_local_addr(listener: *mut u8) -> *mut u8 {
    // SAFETY: The network constructor reserves word-aligned GC storage sized for NetworkHandle. A
    // non-null handle designates that payload.
    let Some(NetworkHandle::Listener(listener)) = (unsafe { handle(listener) }) else {
        return crate::fs::alloc_io_err("net::local_addr: invalid TcpListener");
    };
    match listener.local_addr() {
        Ok(address) => ok_string(&address.to_string()),
        Err(error) => crate::fs::alloc_io_err(&format!("net::local_addr: {error}")),
    }
}

/// # Safety
/// A non-null `stream` must be a live network handle allocated by this runtime.
/// Keep it reachable through the call.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_net_peer_addr(stream: *mut u8) -> *mut u8 {
    // SAFETY: The network constructor reserves word-aligned GC storage sized for NetworkHandle. A
    // non-null handle designates that payload.
    let Some(NetworkHandle::Stream(stream)) = (unsafe { handle(stream) }) else {
        return crate::fs::alloc_io_err("net::peer_addr: invalid TcpStream");
    };
    match stream.peer_addr() {
        Ok(address) => ok_string(&address.to_string()),
        Err(error) => crate::fs::alloc_io_err(&format!("net::peer_addr: {error}")),
    }
}

/// # Safety
/// A non-null `stream` must be a live network handle allocated by this runtime.
/// Keep it reachable through the call.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_net_shutdown(stream: *mut u8) -> *mut u8 {
    // SAFETY: The network constructor reserves word-aligned GC storage sized for NetworkHandle. A
    // non-null handle designates that payload.
    let Some(NetworkHandle::Stream(stream)) = (unsafe { handle(stream) }) else {
        return crate::fs::alloc_io_err("net::shutdown: invalid TcpStream");
    };
    match stream.shutdown(Shutdown::Both) {
        Ok(()) => crate::fs::alloc_ok(0, false),
        // Darwin can report ENOTCONN after the peer has already closed or a
        // previous shutdown completed. The requested closed state is reached.
        Err(error) if error.kind() == std::io::ErrorKind::NotConnected => {
            crate::fs::alloc_ok(0, false)
        }
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
        binary: bool,
        bytes: Vec<u8>,
    },
    Write {
        bytes: Vec<u8>,
        offset: usize,
    },
    ImmediateError(String),
}

unsafe fn task_handle(frame: *mut c_void) -> *mut u8 {
    // SAFETY: The scheduler pairs this network callback with its live, initialized operation
    // frame.
    unsafe { net_frame(frame) }.load_gc(NET_TASK_HANDLE_SLOT)
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
        matches!(error.raw_os_error(), Some(10035..=10037))
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
    // SAFETY: The scheduler pairs this network callback with its live, initialized operation
    // frame.
    unsafe { net_frame(frame) }.store_gc(NET_TASK_HANDLE_SLOT, handle);
}

unsafe fn finish(frame: *mut c_void, result: *mut u8, fd: Option<i64>) -> i32 {
    if let Some(fd) = fd {
        crate::netpoll::deregister_current(fd);
    }
    // SAFETY: The scheduler keeps this frame live and serializes its poll and cancel
    // callbacks. Every non-null operation slot holds a Box::into_raw pointer. take_native
    // clears the slot, so only the callback that took it may reclaim the Box. When the
    // poll already owns the Box, finish sees a null slot and the poll drops it on return.
    unsafe {
        let frame = net_frame(frame);
        frame.store_gc(NET_TASK_RESULT_SLOT, result);
        let raw = frame.take_native::<NetOperation>(NET_TASK_OPERATION_SLOT);
        if !raw.is_null() {
            drop(Box::from_raw(raw));
        }
    }
    crate::task::RUNTIME_POLL_READY
}

unsafe fn finish_error(frame: *mut c_void, message: &str, fd: Option<i64>) -> i32 {
    let error = crate::fs::alloc_io_err(message);
    // SAFETY: The scheduler pairs this network callback with its live, initialized operation
    // frame.
    unsafe { finish(frame, error, fd) }
}

unsafe fn poll_connect(frame: *mut c_void, operation: &mut NetOperation) -> i32 {
    let NetOperation::Connect {
        address_text,
        address,
        socket,
        started,
    } = operation
    else {
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        return unsafe { finish_error(frame, "net::connect_async: invalid operation", None) };
    };

    let Some(active) = socket.as_ref() else {
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
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
                    // SAFETY: The scheduler pairs this network callback with its live, initialized
                    // operation frame.
                    Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
                };
            }
            Err(error) => {
                let message = format!("{address_text}: {error}");
                // SAFETY: The scheduler pairs this network callback with its live, initialized
                // operation frame.
                return unsafe { finish_error(frame, &message, Some(fd)) };
            }
        }
    } else {
        match active.take_error() {
            Ok(Some(error)) => {
                let message = format!("{address_text}: {error}");
                // SAFETY: The scheduler pairs this network callback with its live, initialized
                // operation frame.
                return unsafe { finish_error(frame, &message, Some(fd)) };
            }
            Err(error) => {
                let message = format!("{address_text}: {error}");
                // SAFETY: The scheduler pairs this network callback with its live, initialized
                // operation frame.
                return unsafe { finish_error(frame, &message, Some(fd)) };
            }
            Ok(None) => {
                if let Err(error) = active.peer_addr() {
                    if connect_in_progress(&error)
                        || error.kind() == std::io::ErrorKind::NotConnected
                    {
                        return match register(fd, crate::netpoll::WILLOW_NETPOLL_WRITABLE) {
                            Ok(()) => crate::task::RUNTIME_POLL_PENDING,
                            // SAFETY: The scheduler pairs this network callback with its live,
                            // initialized operation frame.
                            Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
                        };
                    }
                    let message = format!("{address_text}: {error}");
                    // SAFETY: The scheduler pairs this network callback with its live, initialized
                    // operation frame.
                    return unsafe { finish_error(frame, &message, Some(fd)) };
                }
            }
        }
    }

    let socket = socket.take().expect("connected socket checked above");
    let stream: TcpStream = socket.into();
    let handle = alloc_handle(NetworkHandle::Stream(stream));
    if handle.is_null() {
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        return unsafe { finish(frame, std::ptr::null_mut(), Some(fd)) };
    }
    // SAFETY: The scheduler pairs this network callback with its live, initialized operation
    // frame.
    unsafe { store_handle(frame, handle) };
    let result = crate::fs::alloc_ok(handle as i64, true);
    // SAFETY: The scheduler pairs this network callback with its live, initialized operation
    // frame.
    unsafe { finish(frame, result, Some(fd)) }
}

unsafe fn poll_accept(frame: *mut c_void) -> i32 {
    // SAFETY: The scheduler pairs this network callback with its live, initialized operation
    // frame.
    let handle_ptr = unsafe { task_handle(frame) };
    // SAFETY: The network constructor reserves word-aligned GC storage sized for NetworkHandle. A
    // non-null handle designates that payload.
    let Some(NetworkHandle::Listener(listener)) = (unsafe { handle(handle_ptr) }) else {
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        return unsafe { finish_error(frame, "net::accept_async: invalid TcpListener", None) };
    };
    let fd = raw_socket(listener);
    match listener.accept() {
        Ok((stream, _)) => {
            if let Err(error) = stream.set_nonblocking(true) {
                // SAFETY: The scheduler pairs this network callback with its live, initialized
                // operation frame.
                return unsafe {
                    finish_error(frame, &format!("net::accept_async: {error}"), Some(fd))
                };
            }
            let accepted = alloc_handle(NetworkHandle::Stream(stream));
            if accepted.is_null() {
                // SAFETY: The scheduler pairs this network callback with its live, initialized
                // operation frame.
                return unsafe { finish(frame, std::ptr::null_mut(), Some(fd)) };
            }
            let mut accepted_root = accepted;
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut accepted_root) };
            let result = crate::fs::alloc_ok(accepted_root as i64, true);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
            // SAFETY: The scheduler pairs this network callback with its live, initialized
            // operation frame.
            unsafe { finish(frame, result, Some(fd)) }
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            match register(fd, crate::netpoll::WILLOW_NETPOLL_READABLE) {
                Ok(()) => crate::task::RUNTIME_POLL_PENDING,
                // SAFETY: The scheduler pairs this network callback with its live, initialized
                // operation frame.
                Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
            }
        }
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        Err(error) => unsafe {
            finish_error(frame, &format!("net::accept_async: {error}"), Some(fd))
        },
    }
}

unsafe fn poll_read(
    frame: *mut c_void,
    max_bytes: usize,
    binary: bool,
    buffered: &mut Vec<u8>,
) -> i32 {
    // SAFETY: The scheduler pairs this network callback with its live, initialized operation
    // frame.
    let handle_ptr = unsafe { task_handle(frame) };
    // SAFETY: The network constructor reserves word-aligned GC storage sized for NetworkHandle. A
    // non-null handle designates that payload.
    let Some(NetworkHandle::Stream(stream)) = (unsafe { handle(handle_ptr) }) else {
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        return unsafe { finish_error(frame, "net::read_async: invalid TcpStream", None) };
    };
    let fd = raw_socket(stream);
    let remaining = max_bytes.saturating_sub(buffered.len());
    debug_assert!(remaining > 0, "incomplete UTF-8 is rejected before re-poll");
    let mut bytes = vec![0_u8; remaining];
    let mut stream_ref = stream;
    match stream_ref.read(&mut bytes) {
        Ok(read) => {
            if binary {
                let mut array = crate::array::from_native_bytes(&bytes[..read]);
                // SAFETY: The initialized slot stays at this address until its registration
                // is popped; collection may rewrite it there.
                unsafe { willow_push_root(&raw mut array) };
                let result = crate::fs::alloc_ok(array as i64, true);
                // SAFETY: The removed shadow-stack entries belong to this operation and their
                // slot storage is still live.
                unsafe { willow_pop_roots(1) };
                // SAFETY: The scheduler pairs this network callback with its live, initialized
                // operation frame.
                return unsafe { finish(frame, result, Some(fd)) };
            }
            buffered.extend_from_slice(&bytes[..read]);
            match std::str::from_utf8(buffered) {
                Ok(text) => {
                    let result = ok_string(text);
                    // SAFETY: The scheduler pairs this network callback with its live, initialized
                    // operation frame.
                    unsafe { finish(frame, result, Some(fd)) }
                }
                Err(error) if error.error_len().is_none() && buffered.len() < max_bytes => {
                    match register(fd, crate::netpoll::WILLOW_NETPOLL_READABLE) {
                        Ok(()) => crate::task::RUNTIME_POLL_PENDING,
                        // SAFETY: The scheduler pairs this network callback with its live,
                        // initialized operation frame.
                        Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
                    }
                }
                // SAFETY: The scheduler pairs this network callback with its live, initialized
                // operation frame.
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
                // SAFETY: The scheduler pairs this network callback with its live, initialized
                // operation frame.
                Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
            }
        }
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        Err(error) => unsafe {
            finish_error(frame, &format!("net::read_async: {error}"), Some(fd))
        },
    }
}

unsafe fn poll_write(frame: *mut c_void, bytes: &[u8], offset: &mut usize) -> i32 {
    // SAFETY: The scheduler pairs this network callback with its live, initialized operation
    // frame.
    let handle_ptr = unsafe { task_handle(frame) };
    // SAFETY: The network constructor reserves word-aligned GC storage sized for NetworkHandle. A
    // non-null handle designates that payload.
    let Some(NetworkHandle::Stream(stream)) = (unsafe { handle(handle_ptr) }) else {
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        return unsafe { finish_error(frame, "net::write_async: invalid TcpStream", None) };
    };
    let fd = raw_socket(stream);
    if bytes.is_empty() {
        let result = crate::fs::alloc_ok(0, false);
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        return unsafe { finish(frame, result, Some(fd)) };
    }
    let mut stream_ref = stream;
    match stream_ref.write(&bytes[*offset..]) {
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
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
                // SAFETY: The scheduler pairs this network callback with its live, initialized
                // operation frame.
                unsafe { finish(frame, result, Some(fd)) }
            } else {
                match register(fd, crate::netpoll::WILLOW_NETPOLL_WRITABLE) {
                    Ok(()) => crate::task::RUNTIME_POLL_PENDING,
                    // SAFETY: The scheduler pairs this network callback with its live, initialized
                    // operation frame.
                    Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            match register(fd, crate::netpoll::WILLOW_NETPOLL_WRITABLE) {
                Ok(()) => crate::task::RUNTIME_POLL_PENDING,
                // SAFETY: The scheduler pairs this network callback with its live, initialized
                // operation frame.
                Err(message) => unsafe { finish_error(frame, &message, Some(fd)) },
            }
        }
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        Err(error) => unsafe {
            finish_error(frame, &format!("net::write_async: {error}"), Some(fd))
        },
    }
}

#[willow_runtime_macros::ffi_boundary]
unsafe extern "C" fn poll_net_operation(frame: *mut c_void) -> i32 {
    // SAFETY: The scheduler pairs this network callback with its live, initialized operation
    // frame.
    let native_frame = unsafe { net_frame(frame) };
    let raw = native_frame.take_native::<NetOperation>(NET_TASK_OPERATION_SLOT);
    if raw.is_null() {
        return crate::task::RUNTIME_POLL_READY;
    }
    // Poll/cancel callbacks for a task are serialized by the scheduler. Take
    // ownership so finish() cannot free fields borrowed by the poll helpers.
    // SAFETY: Taking the frame slot transfers its Box::into_raw operation pointer to this poll
    // once.
    let mut operation = unsafe { Box::from_raw(raw) };
    let status = match operation.as_mut() {
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        NetOperation::Connect { .. } => unsafe { poll_connect(frame, &mut operation) },
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        NetOperation::Accept => unsafe { poll_accept(frame) },
        NetOperation::Read {
            max_bytes,
            binary,
            bytes,
            // SAFETY: The scheduler pairs this network callback with its live, initialized
            // operation frame.
        } => unsafe { poll_read(frame, *max_bytes, *binary, bytes) },
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        NetOperation::Write { bytes, offset } => unsafe { poll_write(frame, bytes, offset) },
        NetOperation::ImmediateError(message) => {
            let message = std::mem::take(message);
            // SAFETY: The scheduler pairs this network callback with its live, initialized
            // operation frame.
            unsafe { finish_error(frame, &message, None) }
        }
    };
    if status == crate::task::RUNTIME_POLL_PENDING {
        native_frame.store_native(NET_TASK_OPERATION_SLOT, Box::into_raw(operation));
    }
    status
}

#[willow_runtime_macros::ffi_boundary]
unsafe extern "C" fn cancel_net_operation(frame: *mut c_void) {
    // SAFETY: The scheduler keeps this frame live and serializes cancellation with polling.
    // Spawn and pending polls store the operation with Box::into_raw. take_native clears
    // the slot, giving cancellation sole ownership of a non-null pointer through socket
    // deregistration and the final Box::from_raw drop. The frame roots the socket handle.
    unsafe {
        let frame = net_frame(frame);
        let raw = frame.take_native::<NetOperation>(NET_TASK_OPERATION_SLOT);
        if !raw.is_null() {
            let task_id = frame.load_word(NET_TASK_ID_SLOT) as u64;
            let operation = &*raw;
            let fd = match operation {
                NetOperation::Connect { socket, .. } => socket.as_ref().map(raw_socket),
                NetOperation::Accept | NetOperation::Read { .. } | NetOperation::Write { .. } => {
                    match handle(frame.load_gc(NET_TASK_HANDLE_SLOT)) {
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
    let Some(frame) = NativeTaskFrame::<NetTaskFrame>::allocate() else {
        return std::ptr::null_mut();
    };
    frame.store_gc(NET_TASK_HANDLE_SLOT, handle);
    frame.store_native(NET_TASK_OPERATION_SLOT, Box::into_raw(Box::new(operation)));
    let raw = frame.as_raw();
    // SAFETY: The supplied callback matches this frame layout; the frame storage remains live
    // across scheduling and callback execution.
    unsafe {
        crate::scheduler::spawn_global_task_initialized(
            poll_net_operation,
            raw,
            Some(cancel_net_operation),
            |task_id| frame.store_word(NET_TASK_ID_SLOT, task_id as i64),
        )
    };
    raw
}

/// # Safety
/// A non-null address must be a current immutable WillowString through the address copy.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_net_connect_async(address: *const u8) -> *mut c_void {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
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

/// # Safety
/// A non-null `listener` must be a live network handle allocated by this runtime.
/// Keep it reachable through the call.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_net_accept_async(listener: *mut u8) -> *mut c_void {
    spawn_operation(listener, NetOperation::Accept)
}

/// # Safety
/// A non-null `stream` must be a live network handle allocated by this runtime.
/// Keep it reachable through the call.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_net_read_async(stream: *mut u8, max_bytes: i64) -> *mut c_void {
    let operation = match usize::try_from(max_bytes) {
        Ok(max_bytes) if (1..=MAX_READ_BYTES).contains(&max_bytes) => NetOperation::Read {
            max_bytes,
            binary: false,
            bytes: Vec::new(),
        },
        _ => NetOperation::ImmediateError(format!(
            "net::read_async: max_bytes must be between 1 and {MAX_READ_BYTES}"
        )),
    };
    spawn_operation(stream, operation)
}

/// # Safety
/// A non-null `stream` must be a live network handle allocated by this runtime.
/// Keep it reachable through the call. A non-null `contents` must be a current
/// immutable WillowString until its bytes have been copied.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_net_write_async(
    stream: *mut u8,
    contents: *const u8,
) -> *mut c_void {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let bytes = unsafe { willow_string_as_str(contents) }
        .as_bytes()
        .to_vec();
    spawn_operation(stream, NetOperation::Write { bytes, offset: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{isolated_runtime, reset_internal_for_test};
    use crate::scheduler::{reset_global_scheduler_for_test, willow_sched_run_until};
    use crate::string::willow_string_from_str;

    fn result_tag(result: *mut u8) -> i64 {
        // SAFETY: The returned Result stores its tag in the first word of a word-aligned GC
        // allocation.
        unsafe { *(result.cast::<i64>()) }
    }

    #[test]
    fn bind_rejects_hostname_without_blocking_dns() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        // SAFETY: The freshly allocated address string is copied on entry, before any GC point.
        let result = unsafe { willow_net_bind(willow_string_from_str("localhost:80")) };
        assert_eq!(result_tag(result), 1);
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
    }

    #[test]
    fn connect_error_is_delivered_through_task_result() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        reset_global_scheduler_for_test();
        // SAFETY: The freshly allocated address string is copied on entry, before any GC point.
        let task = unsafe { willow_net_connect_async(willow_string_from_str("invalid")) };
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        let id = unsafe { net_frame(task) }.load_word(NET_TASK_ID_SLOT) as u64;
        assert_eq!(willow_sched_run_until(id), 1);
        // SAFETY: The scheduler pairs this network callback with its live, initialized operation
        // frame.
        let result = unsafe { net_frame(task) }.load_gc(NET_TASK_RESULT_SLOT);
        assert_eq!(result_tag(result), 1);
        reset_global_scheduler_for_test();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
    }

    #[test]
    fn unreachable_listener_runs_socket_finalizer() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        NETWORK_HANDLE_DROP_COUNT.store(0, Ordering::SeqCst);

        // SAFETY: The freshly allocated address string is copied on entry, before any GC point.
        let mut result = unsafe { willow_net_bind(willow_string_from_str("127.0.0.1:0")) };
        assert_eq!(result_tag(result), 0);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut result) };
        crate::gc::willow_gc_collect();
        assert_eq!(NETWORK_HANDLE_DROP_COUNT.load(Ordering::SeqCst), 0);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        crate::gc::willow_gc_collect();
        assert_eq!(NETWORK_HANDLE_DROP_COUNT.load(Ordering::SeqCst), 1);

        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
    }
}

/// Read arbitrary bytes through the same readiness and cancellation path as text.
///
/// # Safety
/// A non-null `stream` must be a live network handle allocated by this runtime.
/// Keep it reachable through the call.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_net_read_bytes_async(
    stream: *mut u8,
    max_bytes: i64,
) -> *mut c_void {
    let operation = match usize::try_from(max_bytes) {
        Ok(max_bytes) if (1..=MAX_READ_BYTES).contains(&max_bytes) => NetOperation::Read {
            max_bytes,
            binary: true,
            bytes: Vec::new(),
        },
        _ => NetOperation::ImmediateError(format!(
            "net::read_bytes_async: max_bytes must be between 1 and {MAX_READ_BYTES}"
        )),
    };
    spawn_operation(stream, operation)
}

/// Snapshot packed bytes before handing ownership to the scheduler.
///
/// # Safety
/// A non-null `stream` must be a live network handle allocated by this runtime.
/// Keep it reachable through the call. A non-null `contents` must be a current packed
/// Array<u8> with stable length and contents through the synchronous copy.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_net_write_bytes_async(
    stream: *mut u8,
    contents: *mut u8,
) -> *mut c_void {
    // Snapshot before scheduler admission; no managed borrow crosses a safepoint.
    // SAFETY: The ABI supplies a live packed byte array. The copy completes before the frame can
    // suspend.
    let bytes = unsafe { crate::array::packed_bytes(contents) }.to_vec();
    spawn_operation(stream, NetOperation::Write { bytes, offset: 0 })
}
