//! Stable generated-code state. Native task contexts move between workers by
//! ownership transfer; their addresses never change. No TLS address is cached
//! in generated code. Runtime-to-generated calls establish the pinned register.
use std::cell::{Cell, RefCell, RefMut};
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Storage belongs to the execution context, never to a worker TLS address.
/// Only the running owner mutates it. Collectors consume parked snapshots.
#[repr(C)]
pub(crate) struct ShadowRoots {
    base: Cell<*mut *mut *mut u8>,
    top: Cell<usize>,
    limit: Cell<usize>,
    storage: RefCell<Vec<*mut *mut u8>>,
}

impl ShadowRoots {
    const fn new() -> Self {
        Self {
            base: Cell::new(std::ptr::null_mut()),
            top: Cell::new(0),
            limit: Cell::new(0),
            storage: RefCell::new(Vec::new()),
        }
    }
    #[cfg(test)]
    pub(crate) fn set_depth_for_test(&self, depth: usize) {
        self.top.set(depth);
    }

    pub(crate) fn borrow(&self) -> RootBorrow<'_> {
        self.borrow_mut()
    }
    pub(crate) fn borrow_mut(&self) -> RootBorrow<'_> {
        let mut storage = self.storage.borrow_mut();
        // Generated pushes initialize each slot before publishing top.
        assert!(self.top.get() <= storage.capacity());
        unsafe {
            storage.set_len(self.top.get());
        }
        RootBorrow {
            roots: self,
            storage,
        }
    }
}

pub(crate) struct RootBorrow<'a> {
    roots: &'a ShadowRoots,
    storage: RefMut<'a, Vec<*mut *mut u8>>,
}
impl std::ops::Deref for RootBorrow<'_> {
    type Target = Vec<*mut *mut u8>;
    fn deref(&self) -> &Self::Target {
        &self.storage
    }
}
impl std::ops::DerefMut for RootBorrow<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.storage
    }
}
impl Drop for RootBorrow<'_> {
    fn drop(&mut self) {
        self.roots.base.set(self.storage.as_mut_ptr());
        self.roots.top.set(self.storage.len());
        self.roots
            .limit
            .set(if crate::gc::generated_roots_admitted() {
                self.storage.capacity()
            } else {
                0
            });
    }
}

pub(crate) fn with_roots<R>(f: impl FnOnce(&ShadowRoots) -> R) -> R {
    f(&unsafe { &*active() }.roots)
}
pub(crate) fn depth() -> usize {
    let context = unsafe { &*active() };
    context.parent_depth.get() + context.roots.top.get()
}
pub(crate) fn parent_depth() -> usize {
    unsafe { &*active() }.parent_depth.get()
}
pub(crate) fn has_active_bridge() -> bool {
    let context = unsafe { &*active() };
    context.bridge_depth.get() != 0 || context.parent_bridge_active.get()
}

pub(crate) fn visit_roots(mut visit: impl FnMut(&[*mut *mut u8])) {
    let mut context = active();
    while !context.is_null() {
        let current = unsafe { &*context };
        visit(&current.roots.borrow());
        context = current.parent.get();
    }
}

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
    sync_counter: Cell<*mut i32>,
    gc_stop: Cell<*const u8>,
    pub(crate) roots: ShadowRoots,
    parent: Cell<*const Context>,
    parent_depth: Cell<usize>,
    parent_bridge_active: Cell<bool>,
    bridge_depth: Cell<usize>,
}

impl Context {
    pub(crate) const fn new() -> Self {
        Self {
            helper: AtomicPtr::new(std::ptr::null_mut()),
            tlab: AtomicPtr::new(std::ptr::null_mut()),
            panic_depth: AtomicPtr::new((&raw const NO_PANIC).cast_mut()),
            sync_counter: Cell::new(std::ptr::null_mut()),
            gc_stop: Cell::new(std::ptr::null()),
            roots: ShadowRoots::new(),
            parent: Cell::new(std::ptr::null()),
            parent_depth: Cell::new(0),
            parent_bridge_active: Cell::new(false),
            bridge_depth: Cell::new(0),
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
        // Refresh at native entry/resume, after the task context is installed.
        // The pointees are task-owned (or null) and process-owned respectively.
        self.sync_counter
            .set(crate::preempt::willow_sync_poll_counter());
        self.gc_stop.set(crate::gc::willow_gc_stop_flag());
        if !crate::gc::generated_roots_admitted() {
            self.roots.limit.set(0);
        }
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
    assert!(
        std::mem::offset_of!(Context, sync_counter)
            == willow_abi::generated_context::SYNC_COUNTER_OFFSET as usize
    );
    assert!(
        std::mem::offset_of!(Context, gc_stop)
            == willow_abi::generated_context::GC_STOP_OFFSET as usize
    );
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
    assert!(
        std::mem::offset_of!(Context, roots)
            == willow_abi::generated_context::ROOT_BASE_OFFSET as usize
    );
    assert!(
        std::mem::offset_of!(Context, roots) + std::mem::offset_of!(ShadowRoots, top)
            == willow_abi::generated_context::ROOT_TOP_OFFSET as usize
    );
    assert!(
        std::mem::offset_of!(Context, roots) + std::mem::offset_of!(ShadowRoots, limit)
            == willow_abi::generated_context::ROOT_LIMIT_OFFSET as usize
    );
    assert!(
        std::mem::offset_of!(Context, roots) + std::mem::offset_of!(ShadowRoots, storage)
            == willow_abi::generated_context::SIZE
    );
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
    let previous = active();
    if !context.is_null() && context != previous {
        let target = unsafe { &*context };
        // Returning to an ancestor must not introduce a cycle.
        if unsafe { &*previous }.parent.get() != context {
            target.parent.set(previous);
            target.parent_depth.set(depth());
            target.parent_bridge_active.set(has_active_bridge());
        }
    }
    ACTIVE.replace(context)
}

pub(crate) fn active() -> *const Context {
    let context = ACTIVE.get();
    if !context.is_null() {
        return context;
    }
    THREAD.with(|context| context as *const Context)
}

/// Read exactly the pointers that generated gate loads consume.
#[cfg(test)]
pub(crate) fn published_sync_gates() -> (*mut i32, *const u8) {
    let context = unsafe { &*active() };
    (context.sync_counter.get(), context.gc_stop.get())
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
#[cfg(test)]
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
    let bridge = ScalarBridge::enter();
    unsafe { call_word(entry, argument, bridge.context) }
}

/// One native poll may call many scalar mappers. Keep bridge ownership for
/// that poll instead of registering/leaving generated execution per element.
/// This must not outlive its running execution context or cross a poll return.
pub(crate) struct ScalarBridge {
    context: *const Context,
    helper: *mut Helper,
}

impl ScalarBridge {
    pub(crate) fn enter() -> Self {
        let context = active();
        let state = unsafe { &*context };
        state.refresh();
        state.bridge_depth.set(state.bridge_depth.get() + 1);
        Self {
            context,
            helper: state.helper.load(Ordering::Relaxed),
        }
    }

    /// The mapper must obey the scalar generated C ABI and never unwind.
    #[inline]
    pub(crate) unsafe fn call(&self, entry: unsafe extern "C" fn(i64) -> i64, value: i64) -> i64 {
        unsafe { call_word(entry as *const (), value as usize, self.context) as i64 }
    }

    #[inline]
    pub(crate) fn panicked(&self) -> bool {
        let depth = unsafe { &*self.context }
            .panic_depth
            .load(Ordering::Relaxed);
        unsafe { &*depth }.load(Ordering::Relaxed) != 0
    }
}

impl Drop for ScalarBridge {
    fn drop(&mut self) {
        // A nested generated callee must not leak its name into the caller.
        let state = unsafe { &*self.context };
        state.helper.store(self.helper, Ordering::Relaxed);
        state.bridge_depth.set(state.bridge_depth.get() - 1);
        crate::gc::leave_generated_roots();
    }
}

#[inline]
unsafe fn call_word(entry: *const (), argument: usize, context: *const Context) -> usize {
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
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_gates_thread_nested_and_restored_contexts() {
        let _guard = crate::gc::runtime_test_guard();
        let outer = active();
        unsafe { &*outer }.refresh();
        let expected = (
            crate::preempt::willow_sync_poll_counter(),
            crate::gc::willow_gc_stop_flag(),
        );
        assert!(expected.0.is_null());
        assert!(!expected.1.is_null());
        assert_eq!(published_sync_gates(), expected);
        let inner = Context::new();
        let previous = replace(&inner);
        inner.refresh();
        assert_eq!(published_sync_gates(), expected);
        replace(previous);
        assert_eq!(active(), outer);
        assert_eq!(published_sync_gates(), expected);
    }

    // Exact store/bump protocol emitted by the compiler, without a Rust Vec borrow.
    unsafe fn inline_push(roots: &ShadowRoots, slot: *mut *mut u8) {
        let top = roots.top.get();
        assert!(top < roots.limit.get());
        unsafe {
            roots.base.get().add(top).write(slot);
        }
        roots.top.set(top + 1);
    }

    #[test]
    fn shadow_roots_inline_and_runtime_mutations_share_storage() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        for count in [1, 16, 256, 4096] {
            let mut slots = vec![std::ptr::null_mut(); count + 1];
            crate::gc::willow_reserve_roots(count);
            with_roots(|roots| {
                for slot in &mut slots[..count] {
                    unsafe {
                        inline_push(roots, slot);
                    }
                }
            });
            crate::gc::willow_push_root(&mut slots[count]);
            assert_eq!(depth(), count + 1);
            with_roots(|roots| {
                assert_eq!(roots.borrow()[count], &raw mut slots[count]);
                roots.top.set(1);
            });
            crate::gc::willow_pop_root();
            assert_eq!(depth(), 0);
        }
        crate::gc::reset_internal_for_test();
    }

    #[test]
    fn shadow_roots_nested_contexts_preserve_parent_slots() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        let mut outer_slot = std::ptr::null_mut();
        crate::gc::willow_push_root(&mut outer_slot);
        let context = Context::new();
        let outer = replace(&context);
        assert_eq!(depth(), 1);
        let mut child_slot = std::ptr::null_mut();
        crate::gc::willow_push_root(&mut child_slot);
        assert_eq!(depth(), 2);
        let mut slots = Vec::new();
        visit_roots(|roots| slots.extend_from_slice(roots));
        assert_eq!(slots, [&raw mut child_slot, &raw mut outer_slot]);
        crate::gc::willow_pop_root();
        replace(outer);
        assert_eq!(depth(), 1);
        crate::gc::willow_pop_root();
        assert_eq!(depth(), 0);
        crate::gc::reset_internal_for_test();
    }

    unsafe extern "C" fn unregister_then_push(_: *mut c_void) -> i32 {
        crate::gc::willow_reserve_roots(8);
        crate::gc::willow_gc_unregister_mutator();
        assert!(crate::gc::generated_roots_admitted());
        let mut slot = std::ptr::null_mut();
        with_roots(|roots| unsafe {
            inline_push(roots, &mut slot);
        });
        assert_eq!(depth(), 1);
        with_roots(|roots| roots.top.set(0));
        1
    }

    #[test]
    fn shadow_roots_unregister_empty_bridge_keeps_admission() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::gc::willow_gc_register_mutator();
        assert_eq!(
            unsafe { invoke(unregister_then_push as *const (), std::ptr::null_mut()) },
            1
        );
        assert!(!crate::gc::generated_roots_admitted());
        assert_eq!(depth(), 0);
        crate::gc::reset_internal_for_test();
    }

    #[test]
    fn shadow_roots_refresh_requires_new_legacy_admission() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::gc::willow_reserve_roots(8);
        crate::gc::leave_generated_roots();
        unsafe { &*active() }.refresh();
        with_roots(|roots| assert_eq!(roots.limit.get(), 0));
        // An empty collector snapshot must not accidentally grant admission.
        visit_roots(|_| {});
        with_roots(|roots| assert_eq!(roots.limit.get(), 0));
        crate::gc::willow_reserve_roots(8);
        assert!(crate::gc::generated_roots_admitted());
        crate::gc::leave_generated_roots();
        crate::gc::reset_internal_for_test();
    }

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
                check_preserved_register(check_register as *const (), active().cast_mut().cast())
            },
            1
        );
    }

    #[test]
    fn bridge_installs_thread_and_nested_stable_contexts() {
        let thread = active();
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
        assert_eq!(active(), thread);
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
        let initial_depth = context.bridge_depth.get();
        {
            let bridge = ScalarBridge::enter();
            assert_eq!(context.bridge_depth.get(), initial_depth + 1);
            for value in [i64::MIN, i64::MAX, -4294967297, 4294967297, -1, 0, 1] {
                assert_eq!(
                    unsafe { bridge.call(scalar_echo_and_clear_helper, value) },
                    value
                );
                assert!(!bridge.panicked());
                assert_eq!(context.bridge_depth.get(), initial_depth + 1);
            }
            // Read the current context depth pointer, including replacements
            // by nested panic contexts; do not cache its old pointee.
            let depth = AtomicUsize::new(1);
            let previous = context
                .panic_depth
                .swap((&raw const depth).cast_mut(), Ordering::Relaxed);
            assert!(bridge.panicked());
            depth.store(0, Ordering::Relaxed);
            assert!(!bridge.panicked());
            context.panic_depth.store(previous, Ordering::Relaxed);
        }
        assert_eq!(context.bridge_depth.get(), initial_depth);
        assert_eq!(context.helper(), Some(&b"scalar caller"[..]));
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
