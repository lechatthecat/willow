//! Fatal diagnostics for native stack exhaustion in generated code.
//!
//! The compiler emits page-by-page stack probes, including for large frames.
//! Protection must be established on every thread before entering generated
//! code. Signal handlers deliberately avoid Rust TLS, allocation, and locks.
//! On musl with an unlimited main-thread stack, no finite guard address can be
//! classified: stack probes remain enabled, but the OS handles exhaustion.

use std::cell::RefCell;

const DIAGNOSTIC: &[u8] = b"Willow runtime error: native stack overflow\n";

thread_local! {
    // TLS is used only during ordinary initialization and thread teardown.
    static PROTECTION: RefCell<Option<platform::Protection>> = const { RefCell::new(None) };
}

pub(crate) fn protect_current_thread() {
    PROTECTION.with(|slot| {
        let mut protection = slot.borrow_mut();
        if protection.is_none() {
            *protection = Some(platform::Protection::new().unwrap_or_else(|error| {
                panic!("cannot initialize Willow native stack protection: {error}")
            }));
        }
    });
}

/// Change the signal handler's guard range while entering a task-owned stack.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn replace_guard(lower: usize, upper: usize) -> (usize, usize) {
    protect_current_thread();
    PROTECTION.with(|slot| slot.borrow().as_ref().unwrap().replace_guard(lower, upper))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    use std::io;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    use super::DIAGNOSTIC;

    const CLAIMED: usize = usize::MAX;
    const ALT_STACK_BYTES: usize = 128 * 1024;

    /// Nodes are never freed while the process runs, making handler traversal
    /// independent of reclamation. Vacant nodes are reused under the setup
    /// mutex, so retained node count is bounded by peak simultaneous threads.
    struct ThreadSlot {
        key: AtomicUsize,
        lower: AtomicUsize,
        upper: AtomicUsize,
        next: *mut ThreadSlot,
    }

    impl ThreadSlot {
        fn claim(&self, key: usize, bounds: Bounds) -> bool {
            if self
                .key
                .compare_exchange(0, CLAIMED, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                return false;
            }
            self.lower.store(bounds.lower, Ordering::Relaxed);
            self.upper.store(bounds.upper, Ordering::Relaxed);
            self.key.store(key, Ordering::Release);
            true
        }
    }

    static SLOTS: AtomicPtr<ThreadSlot> = AtomicPtr::new(ptr::null_mut());
    static SETUP: Mutex<()> = Mutex::new(());
    static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();
    static PREVIOUS: AtomicPtr<Previous> = AtomicPtr::new(ptr::null_mut());

    struct Previous {
        segv: libc::sigaction,
        bus: libc::sigaction,
        segv_reset: AtomicBool,
        bus_reset: AtomicBool,
    }

    #[derive(Clone, Copy, Debug)]
    struct Bounds {
        lower: usize,
        upper: usize,
    }

    impl Bounds {
        fn around(base: usize, below: usize, above: usize) -> Result<Self, String> {
            let lower = base
                .checked_sub(below)
                .ok_or("stack guard address underflow")?;
            let upper = base
                .checked_add(above)
                .ok_or("stack guard address overflow")?;
            if lower >= upper {
                return Err("empty native stack guard range".into());
            }
            Ok(Self { lower, upper })
        }

        fn contains(self, address: usize) -> bool {
            address != 0 && address >= self.lower && address < self.upper
        }
    }

    fn thread_key() -> usize {
        // POSIX permits pthread_self in an asynchronous signal handler. Its
        // representation on these two targets is a nonzero integer/pointer.
        unsafe { libc::pthread_self() as usize }
    }

    fn page_size() -> Result<usize, String> {
        let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if size < 4096 {
            return Err("native page size is smaller than compiler stack probes".into());
        }
        Ok(size as usize)
    }

    fn system_error(operation: &str) -> String {
        format!("{operation}: {}", io::Error::last_os_error())
    }

    fn install_handlers() -> Result<(), String> {
        INITIALIZED
            .get_or_init(|| unsafe {
                let mut previous = Box::new(Previous {
                    segv: std::mem::zeroed(),
                    bus: std::mem::zeroed(),
                    segv_reset: AtomicBool::new(false),
                    bus_reset: AtomicBool::new(false),
                });
                if libc::sigaction(libc::SIGSEGV, ptr::null(), &mut previous.segv) != 0
                    || libc::sigaction(libc::SIGBUS, ptr::null(), &mut previous.bus) != 0
                {
                    return Err(system_error("reading previous fault handlers"));
                }
                // Publish immutable dispositions before either handler can run.
                let previous = Box::into_raw(previous);
                PREVIOUS.store(previous, Ordering::Release);
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = signal_handler as *const () as usize;
                action.sa_flags = libc::SA_SIGINFO
                    | libc::SA_ONSTACK
                    | ((*previous).segv.sa_flags & libc::SA_RESTART);
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(libc::SIGSEGV, &action, ptr::null_mut()) != 0 {
                    return Err(system_error("installing SIGSEGV handler"));
                }
                action.sa_flags = libc::SA_SIGINFO
                    | libc::SA_ONSTACK
                    | ((*previous).bus.sa_flags & libc::SA_RESTART);
                if libc::sigaction(libc::SIGBUS, &action, ptr::null_mut()) != 0 {
                    let error = system_error("installing SIGBUS handler");
                    libc::sigaction(libc::SIGSEGV, &(*previous).segv, ptr::null_mut());
                    return Err(error);
                }
                Ok(())
            })
            .clone()
    }

    unsafe extern "C" fn signal_handler(
        signal: libc::c_int,
        info: *mut libc::siginfo_t,
        context: *mut libc::c_void,
    ) {
        // All allocations and publication happen during setup. The handler
        // uses only native pointer-sized atomics and POSIX signal-safe calls.
        let address = unsafe { (*info).si_addr() as usize };
        if unsafe { (*info).si_code } > 0 && address != 0 {
            let key = thread_key();
            let mut node = SLOTS.load(Ordering::Acquire);
            while !node.is_null() {
                let slot = unsafe { &*node };
                if slot.key.load(Ordering::Acquire) == key {
                    let bounds = Bounds {
                        lower: slot.lower.load(Ordering::Relaxed),
                        upper: slot.upper.load(Ordering::Relaxed),
                    };
                    if bounds.contains(address) {
                        unsafe {
                            libc::write(
                                libc::STDERR_FILENO,
                                DIAGNOSTIC.as_ptr().cast(),
                                DIAGNOSTIC.len(),
                            );
                            libc::_exit(101);
                        }
                    }
                    break;
                }
                node = slot.next;
            }
        }

        let previous = PREVIOUS.load(Ordering::Acquire);
        if !previous.is_null() {
            let (action, reset) = unsafe {
                if signal == libc::SIGBUS {
                    (&(*previous).bus, &(*previous).bus_reset)
                } else {
                    (&(*previous).segv, &(*previous).segv_reset)
                }
            };
            let consumed =
                action.sa_flags & libc::SA_RESETHAND != 0 && reset.swap(true, Ordering::AcqRel);
            if action.sa_sigaction == libc::SIG_IGN && !consumed {
                return;
            }
            if action.sa_sigaction == libc::SIG_DFL || consumed {
                // Restore the fatal disposition and redeliver explicitly.
                // Darwin can report SEGV_ACCERR even for raise(SIGSEGV), so
                // si_code cannot tell us whether an instruction will repeat.
                let mut default: libc::sigaction = unsafe { std::mem::zeroed() };
                default.sa_sigaction = libc::SIG_DFL;
                unsafe { libc::sigemptyset(&mut default.sa_mask) };
                unsafe { libc::sigaction(signal, &default, ptr::null_mut()) };
                unsafe { libc::raise(signal) };
                return;
            }

            // A custom handler may recover. Invoke it with the original
            // siginfo/ucontext while retaining Willow's process disposition,
            // so subsequent overflows still reach us. Reproduce its blocking
            // mask and one-shot semantics without allocating or locking.
            let mut saved_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
            unsafe { libc::sigprocmask(libc::SIG_BLOCK, &action.sa_mask, &mut saved_mask) };
            if action.sa_flags & libc::SA_NODEFER != 0
                && unsafe { libc::sigismember(&action.sa_mask, signal) } == 0
            {
                let mut signal_mask: libc::sigset_t = unsafe { std::mem::zeroed() };
                unsafe {
                    libc::sigemptyset(&mut signal_mask);
                    libc::sigaddset(&mut signal_mask, signal);
                    libc::sigprocmask(libc::SIG_UNBLOCK, &signal_mask, ptr::null_mut());
                }
            }
            if action.sa_flags & libc::SA_SIGINFO != 0 {
                let handler: unsafe extern "C" fn(
                    libc::c_int,
                    *mut libc::siginfo_t,
                    *mut libc::c_void,
                ) = unsafe { std::mem::transmute(action.sa_sigaction) };
                unsafe { handler(signal, info, context) };
            } else {
                let handler: unsafe extern "C" fn(libc::c_int) =
                    unsafe { std::mem::transmute(action.sa_sigaction) };
                unsafe { handler(signal) };
            }
            unsafe { libc::sigprocmask(libc::SIG_SETMASK, &saved_mask, ptr::null_mut()) };
        }
    }

    fn register(bounds: Bounds) -> Result<*mut ThreadSlot, String> {
        let key = thread_key();
        if key == 0 || key == CLAIMED {
            return Err("unsupported native thread identifier".into());
        }
        let _setup = SETUP.lock().unwrap_or_else(|poison| poison.into_inner());
        let mut node = SLOTS.load(Ordering::Acquire);
        while !node.is_null() {
            let slot = unsafe { &*node };
            if slot.claim(key, bounds) {
                return Ok(node);
            }
            node = slot.next;
        }
        let node = Box::into_raw(Box::new(ThreadSlot {
            key: AtomicUsize::new(key),
            lower: AtomicUsize::new(bounds.lower),
            upper: AtomicUsize::new(bounds.upper),
            next: SLOTS.load(Ordering::Relaxed),
        }));
        SLOTS.store(node, Ordering::Release);
        Ok(node)
    }

    struct AlternateStack {
        mapping: *mut libc::c_void,
        mapping_size: usize,
        stack: *mut libc::c_void,
        previous: libc::stack_t,
    }

    impl AlternateStack {
        fn install(page: usize) -> Result<Option<Self>, String> {
            unsafe {
                let mut previous: libc::stack_t = std::mem::zeroed();
                if libc::sigaltstack(ptr::null(), &mut previous) != 0 {
                    return Err(system_error("reading alternate signal stack"));
                }
                if previous.ss_flags & libc::SS_DISABLE == 0 {
                    return Ok(None);
                }
                // Darwin validates the size even when disabling a stack.
                previous.ss_size = previous.ss_size.max(libc::MINSIGSTKSZ as usize);
                let needed = ALT_STACK_BYTES.max(libc::SIGSTKSZ as usize);
                let stack_size = needed.div_ceil(page) * page;
                let mapping_size = stack_size + page;
                let mapping = libc::mmap(
                    ptr::null_mut(),
                    mapping_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                    -1,
                    0,
                );
                if mapping == libc::MAP_FAILED {
                    return Err(system_error("allocating alternate signal stack"));
                }
                if libc::mprotect(mapping, page, libc::PROT_NONE) != 0 {
                    let error = system_error("protecting alternate signal stack guard");
                    libc::munmap(mapping, mapping_size);
                    return Err(error);
                }
                let stack = mapping.cast::<u8>().add(page).cast();
                let installed = libc::stack_t {
                    ss_sp: stack,
                    ss_flags: 0,
                    ss_size: stack_size,
                };
                if libc::sigaltstack(&installed, ptr::null_mut()) != 0 {
                    let error = system_error("installing alternate signal stack");
                    libc::munmap(mapping, mapping_size);
                    return Err(error);
                }
                Ok(Some(Self {
                    mapping,
                    mapping_size,
                    stack,
                    previous,
                }))
            }
        }
    }

    impl Drop for AlternateStack {
        fn drop(&mut self) {
            unsafe {
                let mut current: libc::stack_t = std::mem::zeroed();
                if libc::sigaltstack(ptr::null(), &mut current) != 0 {
                    // Prefer retaining memory to unmapping a live signal stack.
                    return;
                }
                if current.ss_sp == self.stack
                    && current.ss_flags & libc::SS_DISABLE == 0
                    && libc::sigaltstack(&self.previous, ptr::null_mut()) != 0
                {
                    return;
                }
                libc::munmap(self.mapping, self.mapping_size);
            }
        }
    }

    pub(super) struct Protection {
        slot: *mut ThreadSlot,
        _alternate_stack: Option<AlternateStack>,
    }

    impl Protection {
        pub(super) fn replace_guard(&self, lower: usize, upper: usize) -> (usize, usize) {
            let slot = unsafe { &*self.slot };
            let previous = (
                slot.lower.load(Ordering::Relaxed),
                slot.upper.load(Ordering::Relaxed),
            );
            slot.lower.store(lower, Ordering::Relaxed);
            slot.upper.store(upper, Ordering::Relaxed);
            previous
        }

        pub(super) fn new() -> Result<Self, String> {
            let page = page_size()?;
            let bounds = current_bounds(page)?;
            let alternate_stack = AlternateStack::install(page)?;
            install_handlers()?;
            let slot = register(bounds)?;
            Ok(Self {
                slot,
                _alternate_stack: alternate_stack,
            })
        }
    }

    impl Drop for Protection {
        fn drop(&mut self) {
            // Teardown occurs on this same thread; its handler cannot race a
            // concurrent unregister/reuse while reading matching bounds.
            unsafe { &*self.slot }.key.store(0, Ordering::Release);
        }
    }

    #[cfg(target_os = "linux")]
    fn current_bounds(page: usize) -> Result<Bounds, String> {
        unsafe {
            let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
            let result = libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr());
            if result != 0 {
                return Err(format!(
                    "pthread_getattr_np: {}",
                    io::Error::from_raw_os_error(result)
                ));
            }
            let mut attr = attr.assume_init();
            let mut base = ptr::null_mut();
            let mut size = 0;
            let mut guard = 0;
            let stack_result = libc::pthread_attr_getstack(&attr, &mut base, &mut size);
            let guard_result = libc::pthread_attr_getguardsize(&attr, &mut guard);
            libc::pthread_attr_destroy(&mut attr);
            if stack_result != 0 || guard_result != 0 {
                return Err("cannot read native thread stack bounds".into());
            }
            #[cfg(target_env = "musl")]
            if libc::syscall(libc::SYS_gettid) == libc::getpid() as libc::c_long {
                return musl_main_bounds(page);
            }
            let base = base as usize;
            let aligned = base.checked_add(page - 1).ok_or("stack address overflow")? / page * page;
            if guard == 0 {
                // The Linux main stack grows toward the kernel's rlimit guard.
                Bounds::around(aligned, page, 0)
            } else {
                // glibc before 2.27 counted guard bytes inside the allocation;
                // newer versions put them immediately below the usable stack.
                Bounds::around(aligned, guard.max(page), guard.max(page))
            }
        }
    }

    #[cfg(all(target_os = "linux", target_env = "musl"))]
    fn musl_main_bounds(page: usize) -> Result<Bounds, String> {
        // musl reports only currently mapped main-stack bytes. The mapping's
        // upper end and finite rlimit describe how far Linux may grow it.
        let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrlimit(libc::RLIMIT_STACK, &mut limit) } != 0 {
            return Err(system_error("reading the main stack limit"));
        }
        if limit.rlim_cur == libc::RLIM_INFINITY {
            // There is no fixed rlimit guard to recognize. Do not reject an
            // otherwise valid program, nor misclassify its unrelated faults.
            return Ok(Bounds { lower: 0, upper: 0 });
        }
        let marker = 0u8;
        let address = &marker as *const u8 as usize;
        let maps = std::fs::read_to_string("/proc/self/maps").map_err(|error| error.to_string())?;
        let top = maps
            .lines()
            .find_map(|line| {
                let range = line.split_whitespace().next()?;
                let (low, high) = range.split_once('-')?;
                let low = usize::from_str_radix(low, 16).ok()?;
                let high = usize::from_str_radix(high, 16).ok()?;
                (low <= address && address < high).then_some(high)
            })
            .ok_or("cannot locate the main stack mapping")?;
        let base = top
            .checked_sub(limit.rlim_cur as usize)
            .ok_or("invalid main stack limit")?;
        let aligned = base.checked_add(page - 1).ok_or("stack address overflow")? / page * page;
        Bounds::around(aligned, page, 0)
    }

    #[cfg(target_os = "macos")]
    fn current_bounds(page: usize) -> Result<Bounds, String> {
        unsafe {
            let thread = libc::pthread_self();
            let top = libc::pthread_get_stackaddr_np(thread) as usize;
            let size = libc::pthread_get_stacksize_np(thread);
            let base = top
                .checked_sub(size)
                .ok_or("invalid native thread stack bounds")?;
            // Rust-hosted main threads may have an installed guard within the
            // first stack page; pthread worker guards lie below the stack.
            Bounds::around(
                base,
                page,
                if libc::pthread_main_np() != 0 {
                    page
                } else {
                    0
                },
            )
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const CHILD: &str = "WILLOW_NATIVE_STACK_TEST_CHILD";

        fn subprocess(test: &str) -> std::process::Output {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!("stack_overflow::platform::tests::{test}"),
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, test)
                .output()
                .unwrap()
        }

        fn is_child(test: &str) -> bool {
            std::env::var(CHILD).is_ok_and(|value| value == test)
        }

        static CUSTOM_CALLS: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn custom_handler(_signal: libc::c_int) {
            CUSTOM_CALLS.fetch_add(1, Ordering::Relaxed);
        }

        #[test]
        fn protected_worker_overflow_uses_the_alternate_stack() {
            const TEST: &str = "protected_worker_overflow_uses_the_alternate_stack";
            if is_child(TEST) {
                #[inline(never)]
                fn recurse(depth: usize) -> usize {
                    let padding = [1u8; 8192];
                    std::hint::black_box(&padding);
                    if std::hint::black_box(depth) == 0 {
                        return 0;
                    }
                    recurse(depth - 1).wrapping_add(std::hint::black_box(padding[0]) as usize)
                }
                std::thread::Builder::new()
                    .stack_size(1024 * 1024)
                    .spawn(|| {
                        super::super::protect_current_thread();
                        std::hint::black_box(recurse(1_000_000));
                    })
                    .unwrap()
                    .join()
                    .unwrap();
                panic!("worker stack exhaustion was not fatal");
            }
            let output = subprocess(TEST);
            assert_eq!(output.status.code(), Some(101));
            assert!(
                output
                    .stderr
                    .windows(DIAGNOSTIC.len())
                    .any(|part| part == DIAGNOSTIC),
                "{:?}",
                output
            );
        }

        #[test]
        fn unrelated_synthetic_default_signal_remains_fatal() {
            const TEST: &str = "unrelated_synthetic_default_signal_remains_fatal";
            if is_child(TEST) {
                let no_core = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) }, 0);
                unsafe { libc::signal(libc::SIGSEGV, libc::SIG_DFL) };
                super::super::protect_current_thread();
                unsafe { libc::raise(libc::SIGSEGV) };
                panic!("default SIGSEGV disposition was swallowed");
            }
            use std::os::unix::process::ExitStatusExt;
            let output = subprocess(TEST);
            assert_eq!(output.status.signal(), Some(libc::SIGSEGV), "{output:?}");
            assert!(
                !output
                    .stderr
                    .windows(DIAGNOSTIC.len())
                    .any(|part| part == DIAGNOSTIC)
            );
        }

        #[test]
        fn recoverable_custom_signal_keeps_overflow_handler_installed() {
            const TEST: &str = "recoverable_custom_signal_keeps_overflow_handler_installed";
            if is_child(TEST) {
                unsafe {
                    libc::signal(libc::SIGSEGV, custom_handler as *const () as usize);
                }
                super::super::protect_current_thread();
                for _ in 0..2 {
                    assert_eq!(unsafe { libc::raise(libc::SIGSEGV) }, 0);
                }
                assert_eq!(CUSTOM_CALLS.load(Ordering::Relaxed), 2);
                let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                assert_eq!(
                    unsafe { libc::sigaction(libc::SIGSEGV, ptr::null(), &mut action) },
                    0
                );
                assert_eq!(action.sa_sigaction, signal_handler as *const () as usize);
                return;
            }
            let output = subprocess(TEST);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        #[test]
        fn thread_protection_is_idempotent_and_reuses_retired_slots() {
            const TEST: &str = "thread_protection_is_idempotent_and_reuses_retired_slots";
            if is_child(TEST) {
                super::super::protect_current_thread();
                let mut first = None;
                for _ in 0..8 {
                    let slot = std::thread::spawn(|| {
                        super::super::protect_current_thread();
                        let first = super::super::PROTECTION
                            .with(|protection| protection.borrow().as_ref().unwrap().slot as usize);
                        super::super::protect_current_thread();
                        let second = super::super::PROTECTION
                            .with(|protection| protection.borrow().as_ref().unwrap().slot as usize);
                        assert_eq!(first, second);
                        first
                    })
                    .join()
                    .unwrap();
                    assert_eq!(
                        unsafe { &*(slot as *const ThreadSlot) }
                            .key
                            .load(Ordering::Acquire),
                        0
                    );
                    assert_eq!(*first.get_or_insert(slot), slot);
                }
                return;
            }
            let output = subprocess(TEST);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        #[test]
        fn owned_alternate_stack_restores_disabled_state() {
            const TEST: &str = "owned_alternate_stack_restores_disabled_state";
            if is_child(TEST) {
                unsafe {
                    let mut original: libc::stack_t = std::mem::zeroed();
                    assert_eq!(libc::sigaltstack(ptr::null(), &mut original), 0);
                    let disabled = libc::stack_t {
                        ss_sp: ptr::null_mut(),
                        ss_flags: libc::SS_DISABLE,
                        ss_size: libc::MINSIGSTKSZ as usize,
                    };
                    assert_eq!(libc::sigaltstack(&disabled, ptr::null_mut()), 0);
                    let owned = AlternateStack::install(page_size().unwrap())
                        .unwrap()
                        .unwrap();
                    let mut active: libc::stack_t = std::mem::zeroed();
                    assert_eq!(libc::sigaltstack(ptr::null(), &mut active), 0);
                    assert_eq!(active.ss_sp, owned.stack);
                    assert!(
                        AlternateStack::install(page_size().unwrap())
                            .unwrap()
                            .is_none()
                    );
                    drop(owned);
                    assert_eq!(libc::sigaltstack(ptr::null(), &mut active), 0);
                    assert_ne!(active.ss_flags & libc::SS_DISABLE, 0);
                    original.ss_size = original.ss_size.max(libc::MINSIGSTKSZ as usize);
                    assert_eq!(libc::sigaltstack(&original, ptr::null_mut()), 0);
                }
                return;
            }
            let output = subprocess(TEST);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        #[test]
        fn guard_classification_has_exclusive_upper_bound() {
            let bounds = Bounds::around(0x10000, 0x1000, 0x1000).unwrap();
            assert!(!bounds.contains(0));
            assert!(!bounds.contains(0xefff));
            assert!(bounds.contains(0xf000));
            assert!(bounds.contains(0x10000));
            assert!(bounds.contains(0x10fff));
            assert!(!bounds.contains(0x11000));
        }

        #[test]
        fn guard_classification_rejects_address_wraparound() {
            assert!(Bounds::around(1, 2, 0).is_err());
            assert!(Bounds::around(usize::MAX, 0, 1).is_err());
            assert!(Bounds::around(1, 0, 0).is_err());
        }

        #[test]
        fn current_thread_guard_is_below_a_live_stack_local() {
            let marker = 0u8;
            let bounds = current_bounds(page_size().unwrap()).unwrap();
            assert!(bounds.upper < &marker as *const u8 as usize);
        }

        #[test]
        fn vacant_registry_slot_can_be_reused_with_new_bounds() {
            let bounds = Bounds::around(0x10000, 0x1000, 0).unwrap();
            let slot = ThreadSlot {
                key: AtomicUsize::new(0),
                lower: AtomicUsize::new(0),
                upper: AtomicUsize::new(0),
                next: ptr::null_mut(),
            };
            assert!(slot.claim(2, bounds));
            assert!(!slot.claim(3, bounds));
            assert_eq!(slot.key.load(Ordering::Acquire), 2);
            slot.key.store(0, Ordering::Release);
            let replacement = Bounds::around(0x20000, 0x2000, 0).unwrap();
            assert!(slot.claim(3, replacement));
            assert_eq!(slot.key.load(Ordering::Acquire), 3);
            assert_eq!(slot.lower.load(Ordering::Relaxed), replacement.lower);
            assert_eq!(slot.upper.load(Ordering::Relaxed), replacement.upper);
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::OnceLock;

    use super::DIAGNOSTIC;

    const STATUS_STACK_OVERFLOW: u32 = 0xc00000fd;

    // Only the initial, ABI-stable exception code is inspected.
    #[repr(C)]
    struct ExceptionRecord {
        code: u32,
    }

    #[repr(C)]
    struct ExceptionPointers {
        record: *const ExceptionRecord,
        context: *mut c_void,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetThreadStackGuarantee(bytes: *mut u32) -> i32;
        fn AddVectoredExceptionHandler(
            first: u32,
            handler: unsafe extern "system" fn(*mut ExceptionPointers) -> i32,
        ) -> *mut c_void;
        fn GetStdHandle(which: u32) -> *mut c_void;
        fn WriteFile(
            file: *mut c_void,
            buffer: *const c_void,
            bytes: u32,
            written: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn GetCurrentProcess() -> *mut c_void;
        fn TerminateProcess(process: *mut c_void, code: u32) -> i32;
        fn ExitProcess(code: u32) -> !;
    }

    unsafe extern "system" fn exception_handler(info: *mut ExceptionPointers) -> i32 {
        if unsafe { (*(*info).record).code } == STATUS_STACK_OVERFLOW {
            unsafe {
                let mut written = 0;
                WriteFile(
                    GetStdHandle(-12i32 as u32),
                    DIAGNOSTIC.as_ptr().cast(),
                    DIAGNOSTIC.len() as u32,
                    &mut written,
                    ptr::null_mut(),
                );
                TerminateProcess(GetCurrentProcess(), 101);
                ExitProcess(101);
            }
        }
        0 // EXCEPTION_CONTINUE_SEARCH: preserve unrelated exception handling.
    }

    pub(super) struct Protection;

    impl Protection {
        pub(super) fn new() -> Result<Self, String> {
            // A guarantee is thread-lifetime OS state. Windows only increases
            // an existing guarantee, so no previous reservation is destroyed.
            let mut guarantee = 64 * 1024;
            if unsafe { SetThreadStackGuarantee(&mut guarantee) } == 0 {
                return Err(format!(
                    "SetThreadStackGuarantee: {}",
                    std::io::Error::last_os_error()
                ));
            }
            static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();
            INSTALLED
                .get_or_init(|| {
                    if unsafe { AddVectoredExceptionHandler(1, exception_handler) }.is_null() {
                        Err(format!(
                            "AddVectoredExceptionHandler: {}",
                            std::io::Error::last_os_error()
                        ))
                    } else {
                        Ok(())
                    }
                })
                .clone()?;
            Ok(Self)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    pub(super) struct Protection;

    impl Protection {
        pub(super) fn new() -> Result<Self, String> {
            Err("native stack overflow diagnostics are unsupported on this target".into())
        }
    }
}
