//! Linux x86-64 backend: static ELF, bubblewrap namespaces, read-only snapshots,
//! and an allowlist seccomp filter. There is no host filesystem or socket grant.
use super::*;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use willow_abi::plugin::HEADER_BYTES;
pub(super) struct Native {
    child: Child,
    transport: UnixStream,
    limit: usize,
    pub callbacks: VecDeque<Vec<u8>>,
}
fn snapshot(bytes: &[u8]) -> Result<File, Error> {
    let fd = unsafe { libc::memfd_create(c"willow-plugin".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(Error::IsolationUnavailable);
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)
        .map_err(|_| Error::IsolationUnavailable)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|_| Error::IsolationUnavailable)?;
    Ok(file)
}
fn filter() -> Vec<u8> {
    // Architecture is checked before syscall numbers; x32 numbers are not allowed.
    let mut instructions: Vec<(u16, u8, u8, u32)> = vec![
        (0x20, 0, 0, 4),
        (0x15, 1, 0, 0xc000003e),
        (0x06, 0, 0, 0x80000000),
        (0x20, 0, 0, 0),
    ];
    for syscall in [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_close,
        libc::SYS_fstat,
        libc::SYS_newfstatat,
        libc::SYS_stat,
        libc::SYS_lstat,
        libc::SYS_statx,
        libc::SYS_lseek,
        libc::SYS_pread64,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_munmap,
        libc::SYS_brk,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_open,
        libc::SYS_openat,
        libc::SYS_readlink,
        libc::SYS_readlinkat,
        libc::SYS_access,
        libc::SYS_faccessat,
        libc::SYS_getrandom,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_getuid,
        libc::SYS_geteuid,
        libc::SYS_getgid,
        libc::SYS_getegid,
        libc::SYS_uname,
        libc::SYS_arch_prctl,
        libc::SYS_set_tid_address,
        libc::SYS_set_robust_list,
        libc::SYS_rseq,
        libc::SYS_futex,
        libc::SYS_clock_gettime,
        libc::SYS_nanosleep,
        libc::SYS_clock_nanosleep,
        libc::SYS_sched_yield,
        libc::SYS_getrlimit,
        libc::SYS_prlimit64,
        libc::SYS_wait4,
        libc::SYS_waitid,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_execve,
    ] {
        instructions.push((0x15, 0, 1, syscall as u32));
        instructions.push((0x06, 0, 0, 0x7fff0000));
    }
    instructions.push((0x06, 0, 0, 0x00050000 | libc::EPERM as u32));
    let mut bytes = Vec::with_capacity(instructions.len() * 8);
    for (code, jt, jf, k) in instructions {
        bytes.extend_from_slice(&code.to_ne_bytes());
        bytes.push(jt);
        bytes.push(jf);
        bytes.extend_from_slice(&k.to_ne_bytes());
    }
    bytes
}
fn nonblocking(fd: i32) -> Result<(), Error> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(Error::IsolationUnavailable);
    }
    Ok(())
}
fn wait(fd: i32, events: i16, cancel: &Cancellation, deadline: Instant) -> Result<(), Error> {
    cancel.check(deadline)?;
    let mut pfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut pfd, 1, 2) };
    if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
        return Err(Error::Crash);
    }
    Ok(())
}
impl Native {
    #[cfg(test)]
    pub(super) fn wait_for_exit(&mut self) {
        self.child.wait().unwrap();
    }
    pub fn start(
        manifest: &Manifest,
        artifact: &[u8],
        resources: Resources,
    ) -> Result<Self, Error> {
        if !artifact.starts_with(b"\x7fELF\x02\x01") {
            return Err(Error::ProtocolMismatch);
        }
        let mut files = vec![snapshot(artifact)?, snapshot(&filter())?];
        let mut command = Command::new("/usr/bin/bwrap");
        command
            .env_clear()
            .args([
                "--unshare-user",
                "--unshare-pid",
                "--unshare-net",
                "--unshare-ipc",
                "--unshare-uts",
                "--disable-userns",
                "--assert-userns-disabled",
                "--die-with-parent",
                "--new-session",
                "--cap-drop",
                "ALL",
                "--clearenv",
                "--perms",
                "0500",
                "--ro-bind-data",
            ])
            .arg(files[0].as_raw_fd().to_string())
            .arg("/plugin")
            .arg("--seccomp")
            .arg(files[1].as_raw_fd().to_string());
        for (id, bytes) in resources {
            let file = snapshot(&bytes)?;
            command
                .args(["--perms", "0400", "--ro-bind-data"])
                .arg(file.as_raw_fd().to_string())
                .arg(format!("/resources/{id}"));
            files.push(file);
        }
        let (transport, guest) = UnixStream::pair().map_err(|_| Error::IsolationUnavailable)?;
        let guest_input = guest.try_clone().map_err(|_| Error::IsolationUnavailable)?;
        command
            .args(["--remount-ro", "/", "--chdir", "/", "/plugin"])
            .stdin(Stdio::from(OwnedFd::from(guest_input)))
            .stdout(Stdio::from(OwnedFd::from(guest)))
            .stderr(Stdio::null());
        let fds: Vec<_> = files.iter().map(AsRawFd::as_raw_fd).collect();
        let memory = manifest.budgets.memory_bytes as libc::rlim_t;
        let cpu = (u64::from(manifest.budgets.deadline_ms).div_ceil(1000) + 1) as libc::rlim_t;
        unsafe {
            command.pre_exec(move || {
                // Do not trust bwrap to close descriptors inherited from the
                // embedding application. Set CLOEXEC in the child only, then
                // explicitly reopen inheritance for the snapshot/filter FDs.
                if libc::syscall(
                    libc::SYS_close_range,
                    3u32,
                    u32::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                ) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                for fd in &fds {
                    if libc::fcntl(*fd, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                for (resource, limit) in [
                    (libc::RLIMIT_AS, memory),
                    (libc::RLIMIT_CPU, cpu),
                    (libc::RLIMIT_CORE, 0),
                ] {
                    let limit = libc::rlimit {
                        rlim_cur: limit,
                        rlim_max: limit,
                    };
                    if libc::setrlimit(resource, &limit) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(|_| Error::IsolationUnavailable)?;
        drop(command);
        let mut native = Self {
            child,
            transport,
            limit: manifest.budgets.payload_bytes,
            callbacks: VecDeque::new(),
        };
        nonblocking(native.transport.as_raw_fd())?;
        // The hello is read only after bwrap successfully installs every policy.
        let cancel = Cancellation(Arc::new(AtomicU8::new(0)));
        let deadline = Instant::now() + Duration::from_millis(manifest.budgets.deadline_ms.into());
        let mut bytes = [0; HEADER_BYTES];
        native
            .read(&mut bytes, &cancel, deadline)
            .map_err(|_| Error::IsolationUnavailable)?;
        let hello = Header::decode(&bytes, 0)?;
        if hello
            != (Header {
                tag: Tag::Response,
                method: 0,
                generation: 0,
                request: 0,
                resource: 0,
                length: 0,
            })
        {
            return Err(Error::ProtocolMismatch);
        }
        Ok(native)
    }
    fn read(
        &mut self,
        mut bytes: &mut [u8],
        cancel: &Cancellation,
        deadline: Instant,
    ) -> Result<(), Error> {
        while !bytes.is_empty() {
            cancel.check(deadline)?;
            match self.transport.read(bytes) {
                Ok(0) => return Err(Error::Crash),
                Ok(n) => {
                    bytes = &mut bytes[n..];
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    wait(self.transport.as_raw_fd(), libc::POLLIN, cancel, deadline)?
                }
                Err(_) => return Err(Error::Crash),
            }
        }
        Ok(())
    }
    fn write(
        &mut self,
        mut bytes: &[u8],
        cancel: &Cancellation,
        deadline: Instant,
    ) -> Result<(), Error> {
        while !bytes.is_empty() {
            cancel.check(deadline)?;
            // The runtime has a C entrypoint and cannot assume Rust's normal
            // SIGPIPE setup. Suppress the signal on this operation only.
            let written = unsafe {
                libc::send(
                    self.transport.as_raw_fd(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            let result = if written < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(written as usize)
            };
            match result {
                Ok(0) => return Err(Error::Crash),
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    wait(self.transport.as_raw_fd(), libc::POLLOUT, cancel, deadline)?
                }
                Err(_) => return Err(Error::Crash),
            }
        }
        Ok(())
    }
    pub fn call(
        &mut self,
        header: Header,
        input: &[u8],
        cancel: &Cancellation,
        deadline: Instant,
    ) -> Result<Vec<u8>, Error> {
        self.write(&header.encode(), cancel, deadline)?;
        self.write(input, cancel, deadline)?;
        let mut callback_bytes = 0;
        loop {
            let mut bytes = [0; HEADER_BYTES];
            self.read(&mut bytes, cancel, deadline)?;
            let response = Header::decode(&bytes, self.limit - callback_bytes)?;
            if !matches!(response.tag, Tag::Response | Tag::Error | Tag::Callback)
                || response.method != header.method
                || response.request != header.request
                || response.generation != header.generation
                || response.resource != header.resource
            {
                return Err(Error::ProtocolMismatch);
            }
            if response.tag == Tag::Error && response.length != 1 {
                return Err(Error::ProtocolMismatch);
            }
            if response.tag == Tag::Callback && self.callbacks.len() == 16 {
                return Err(Error::ResourceLimit);
            }
            // All untrusted metadata is validated before this allocation.
            let mut payload = vec![0; response.length];
            self.read(&mut payload, cancel, deadline)?;
            match response.tag {
                Tag::Callback => {
                    callback_bytes += payload.len();
                    self.callbacks.push_back(payload);
                }
                Tag::Error => {
                    return Err(match payload[0] {
                        1 => Error::Trap,
                        2 => Error::ResourceLimit,
                        3 => Error::DeniedCapability,
                        _ => Error::ProtocolMismatch,
                    });
                }
                _ => return Ok(payload),
            }
        }
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        // bwrap's death signal and PID namespace tear down the sandbox, including
        // its init. The seccomp policy disallows creation of guest descendants.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
