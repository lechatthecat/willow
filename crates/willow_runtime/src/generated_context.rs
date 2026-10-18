//! Stable generated-code state. Native task contexts move between workers by
//! ownership transfer; their addresses never change. No TLS address is cached
//! in generated code. Runtime-to-generated calls establish the pinned register.
use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

#[repr(C)]
pub(crate) struct Helper {
    bytes: *const u8,
    len: usize,
}

static NO_PANIC: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
pub(crate) struct Context {
    helper: AtomicPtr<Helper>,
    tlab: AtomicPtr<crate::gc::GcTlabState>,
    panic_depth: AtomicPtr<AtomicUsize>,
}

impl Context {
    pub(crate) const fn new() -> Self {
        Self {
            helper: AtomicPtr::new(std::ptr::null_mut()),
            tlab: AtomicPtr::new(std::ptr::null_mut()),
            panic_depth: AtomicPtr::new((&raw const NO_PANIC).cast_mut()),
        }
    }

    fn refresh_panic_depth(&self) {
        self.panic_depth.store(
            crate::panic_context::current_depth_ptr().unwrap_or((&raw const NO_PANIC).cast_mut()),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn refresh(&self) {
        self.refresh_panic_depth();
        self.tlab
            .store(crate::gc::willow_gc_current_tlab(), Ordering::Relaxed);
    }

    pub(crate) fn clear_helper(&self) {
        self.helper.store(std::ptr::null_mut(), Ordering::Relaxed);
    }

    pub(crate) fn helper(&self) -> Option<&'static [u8]> {
        let descriptor = self.helper.load(Ordering::Relaxed);
        if descriptor.is_null() {
            return None;
        }
        // SAFETY: generated code publishes only aligned, process-lifetime
        // immutable descriptors, in a single pointer-sized store.
        unsafe {
            Some(std::slice::from_raw_parts(
                (*descriptor).bytes,
                (*descriptor).len,
            ))
        }
    }
}

const _: () = {
    assert!(std::mem::size_of::<Helper>() == willow_abi::generated_context::HELPER_DESCRIPTOR_SIZE);
    assert!(
        std::mem::offset_of!(Helper, len) == willow_abi::generated_context::HELPER_LENGTH_OFFSET
    );
    assert!(
        std::mem::offset_of!(Context, helper)
            == willow_abi::generated_context::HELPER_OFFSET as usize
    );
    assert!(
        std::mem::offset_of!(Context, tlab) == willow_abi::generated_context::TLAB_OFFSET as usize
    );
    assert!(
        std::mem::offset_of!(Context, panic_depth)
            == willow_abi::generated_context::PANIC_DEPTH_OFFSET as usize
    );
    assert!(std::mem::size_of::<Context>() == willow_abi::generated_context::SIZE);
};

thread_local! {
    static ACTIVE: Cell<*const Context> = const { Cell::new(std::ptr::null()) };
    static THREAD: Context = const { Context::new() };
}

// Only the running owner reads this pointer. CURRENT_CONTEXT retains its Arc
// until replacement; refresh before dropping that Arc or entering/resuming code.
// Inactive contexts can contain stale pointers, but are refreshed before use.
pub(crate) fn refresh_panic_depth() {
    let active = ACTIVE.get();
    if active.is_null() {
        THREAD.with(Context::refresh_panic_depth);
    } else {
        unsafe { &*active }.refresh_panic_depth();
    }
}

pub(crate) fn replace(context: *const Context) -> *const Context {
    ACTIVE.replace(context)
}

fn current() -> *const Context {
    let context = ACTIVE.get();
    if !context.is_null() {
        return context;
    }
    THREAD.with(|context| {
        context.refresh();
        context as *const Context
    })
}

/// Test the exact published pointer, without borrowing panic TLS.
#[cfg(test)]
pub(crate) fn published_depth() -> usize {
    let read = |context: &Context| {
        let pointer = context.panic_depth.load(Ordering::Relaxed);
        unsafe { &*pointer }.load(Ordering::Acquire)
    };
    let active = ACTIVE.get();
    if active.is_null() {
        THREAD.with(read)
    } else {
        read(unsafe { &*active })
    }
}

/// Enter a pointer/void or pointer/i32 C-ABI callback.
///
/// # Safety
/// `entry` and `frame` must satisfy the callback's contract; it must not unwind.
#[inline]
pub(crate) unsafe extern "C" fn invoke(entry: *const (), frame: *mut c_void) -> i32 {
    unsafe { invoke_word(entry, frame as usize) as i32 }
}

/// Enter a generated scalar mapper without truncating either ABI word.
///
/// # Safety
/// The callback must satisfy its own contract and must not unwind.
#[inline]
pub(crate) unsafe fn invoke_i64(entry: unsafe extern "C" fn(i64) -> i64, value: i64) -> i64 {
    unsafe { invoke_word(entry as *const (), value as usize) as i64 }
}

/// Shared one-word C-ABI bridge for the supported 64-bit targets. Unused
/// argument/return registers are harmless for the void entry variants.
/// The compiler saves/restores the native caller's pinned register around asm.
/// Keep this out of line: native callers may freely repurpose that register.
///
/// # Safety
/// `entry` must accept the supplied ABI word (or no arguments), return at most
/// one ABI word, and never unwind. Float/aggregate callbacks are not supported.
#[inline(never)]
unsafe extern "C" fn invoke_word(entry: *const (), argument: usize) -> usize {
    let context = current();
    // Runtime callbacks must not leak a nested generated callee's name into
    // their generated caller. The pointer is stable even if the callback migrates.
    let helper = unsafe { &*context }.helper.load(Ordering::Relaxed);
    let result: usize;
    #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
    unsafe {
        std::arch::asm!("call rax",
            inlateout("rax") entry => result,
            in("rdi") argument, in("r15") context,
            clobber_abi("C"));
    }
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    unsafe {
        std::arch::asm!("sub rsp, 32", "call rax", "add rsp, 32",
            inlateout("rax") entry => result,
            in("rcx") argument, in("r15") context,
            clobber_abi("C"));
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("blr x9",
            in("x9") entry, inlateout("x0") argument => result,
            in("x21") context, clobber_abi("C"));
    }
    unsafe { &*context }.helper.store(helper, Ordering::Relaxed);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[unsafe(naked)]
    unsafe extern "C" fn check_register(_: *mut c_void) -> i32 {
        #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
        std::arch::naked_asm!("xor eax, eax", "cmp rdi, r15", "sete al", "ret");
        #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
        std::arch::naked_asm!("xor eax, eax", "cmp rcx, r15", "sete al", "ret");
        #[cfg(target_arch = "aarch64")]
        std::arch::naked_asm!("cmp x0, x21", "cset w0, eq", "ret");
    }

    #[unsafe(naked)]
    unsafe extern "C" fn check_preserved_register(entry: *const (), frame: *mut c_void) -> i32 {
        #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
        std::arch::naked_asm!(
            "push r15", "mov r15, 12345", "call {bridge}",
            "xor eax, eax", "cmp r15, 12345", "sete al", "pop r15", "ret",
            bridge = sym invoke);
        #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
        std::arch::naked_asm!(
            "push r15", "sub rsp, 32", "mov r15, 12345", "call {bridge}",
            "xor eax, eax", "cmp r15, 12345", "sete al", "add rsp, 32", "pop r15", "ret",
            bridge = sym invoke);
        #[cfg(target_arch = "aarch64")]
        std::arch::naked_asm!(
            "stp x21, x30, [sp, #-16]!", "mov x21, #12345", "bl {bridge}",
            "mov x9, #12345", "cmp x21, x9", "cset w0, eq", "ldp x21, x30, [sp], #16", "ret",
            bridge = sym invoke);
    }

    #[test]
    fn bridge_preserves_native_callers_register() {
        assert_eq!(
            unsafe {
                check_preserved_register(check_register as *const (), current().cast_mut().cast())
            },
            1
        );
    }

    #[test]
    fn bridge_installs_thread_and_nested_stable_contexts() {
        let thread = current();
        assert_eq!(
            unsafe { invoke(check_register as *const (), thread.cast_mut().cast()) },
            1
        );
        let context = Context::new();
        context.refresh();
        let old = replace(&context);
        assert!(old.is_null());
        assert_eq!(
            unsafe {
                invoke(
                    check_register as *const (),
                    (&raw const context).cast_mut().cast(),
                )
            },
            1
        );
        let nested = Context::new();
        nested.refresh();
        let outer = replace(&nested);
        assert_eq!(outer, &raw const context);
        assert_eq!(
            unsafe {
                invoke(
                    check_register as *const (),
                    (&raw const nested).cast_mut().cast(),
                )
            },
            1
        );
        assert_eq!(replace(outer), &raw const nested);
        assert_eq!(replace(old), &raw const context);
        assert_eq!(current(), thread);
    }

    #[unsafe(naked)]
    unsafe extern "C" fn clear_helper(_: *mut c_void) -> i32 {
        #[cfg(target_arch = "x86_64")]
        std::arch::naked_asm!("mov qword ptr [r15], 0", "mov eax, 7", "ret");
        #[cfg(target_arch = "aarch64")]
        std::arch::naked_asm!("str xzr, [x21]", "mov w0, #7", "ret");
    }

    #[test]
    fn runtime_callback_restores_callers_helper_descriptor() {
        let context = Context::new();
        let descriptor = Box::leak(Box::new(Helper {
            bytes: b"caller".as_ptr(),
            len: 6,
        }));
        context.helper.store(descriptor, Ordering::Relaxed);
        context.refresh();
        let old = replace(&context);
        assert_eq!(
            unsafe { invoke(clear_helper as *const (), std::ptr::null_mut()) },
            7
        );
        assert_eq!(context.helper(), Some(&b"caller"[..]));
        replace(old);
    }

    #[unsafe(naked)]
    unsafe extern "C" fn scalar_echo_and_clear_helper(_: i64) -> i64 {
        #[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
        std::arch::naked_asm!("mov qword ptr [r15], 0", "mov rax, rdi", "ret");
        #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
        std::arch::naked_asm!("mov qword ptr [r15], 0", "mov rax, rcx", "ret");
        #[cfg(target_arch = "aarch64")]
        std::arch::naked_asm!("str xzr, [x21]", "ret");
    }

    #[test]
    fn scalar_bridge_preserves_all_bits_and_restores_helper() {
        let context = Context::new();
        let descriptor = Box::leak(Box::new(Helper {
            bytes: b"scalar caller".as_ptr(),
            len: 13,
        }));
        context.helper.store(descriptor, Ordering::Relaxed);
        context.refresh();
        let old = replace(&context);
        for value in [i64::MIN, i64::MAX, -4294967297, 4294967297, -1, 0, 1] {
            assert_eq!(
                unsafe { invoke_i64(scalar_echo_and_clear_helper, value) },
                value
            );
            assert_eq!(context.helper(), Some(&b"scalar caller"[..]));
        }
        replace(old);
    }

    #[test]
    fn helper_descriptor_and_tlab_refresh_are_independent() {
        let context = Context::new();
        let descriptor = Box::leak(Box::new(Helper {
            bytes: b"helper".as_ptr(),
            len: 6,
        }));
        context.helper.store(descriptor, Ordering::Relaxed);
        context.refresh();
        assert_eq!(context.helper(), Some(&b"helper"[..]));
        assert_eq!(
            context.tlab.load(Ordering::Relaxed),
            crate::gc::willow_gc_current_tlab()
        );
        context.clear_helper();
        assert_eq!(context.helper(), None);
    }
}
