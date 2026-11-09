//! Native task shell. Rust futures and their owned values stay in the bridge;
//! only generated, nonblocking completion conversion runs on a Willow worker.
use std::ffi::c_void;

use crate::native_frame::{NativeFrameSpec, NativeTaskFrame};
use willow_abi::{NativeFrameLayout, SlotKind, ffi::WillowBridgeValue};

type Start = unsafe extern "C" fn(*mut u8, u64);
type Finish = unsafe extern "C" fn(*mut u8, *mut WillowBridgeValue) -> i32;
type Cancel = unsafe extern "C" fn(*mut u8) -> i32;

struct Operation {
    context: *mut u8,
    finish: Finish,
    cancel: Cancel,
}

struct ValueFrame;
struct ReferenceFrame;
impl NativeFrameSpec for ValueFrame {
    const LAYOUT: NativeFrameLayout<'static> = NativeFrameLayout::new(&[
        SlotKind::Word,
        SlotKind::Word,
        SlotKind::Word,
        SlotKind::NativePtr,
    ]);
    const NAME: &'static str = "Rust async value";
}
impl NativeFrameSpec for ReferenceFrame {
    const LAYOUT: NativeFrameLayout<'static> = NativeFrameLayout::new(&[
        SlotKind::GcRef,
        SlotKind::Word,
        SlotKind::Word,
        SlotKind::NativePtr,
    ]);
    const NAME: &'static str = "Rust async reference";
}

#[willow_runtime_macros::ffi_boundary]
unsafe extern "C" fn poll<S: NativeFrameSpec>(raw: *mut c_void) -> i32 {
    let frame = unsafe { NativeTaskFrame::<S>::from_raw(raw) };
    let operation = frame.load_native::<Operation>(3);
    let mut value = WillowBridgeValue::default();
    // The callback uses try_lock: a future polling on the bridge executor
    // must never block a scheduler worker. Completion carries its own wake.
    let ready = unsafe { ((*operation).finish)((*operation).context, &mut value) };
    if ready == 0 {
        return crate::task::RUNTIME_POLL_BLOCKED_SYSCALL;
    }
    // finish consumes the bridge context on every terminal outcome.
    unsafe { drop(Box::from_raw(frame.take_native::<Operation>(3))) };
    if ready != 1 {
        crate::failure::fatal_invariant("RustPanic: async Rust future panicked");
    }
    if S::LAYOUT.slots.slots[0] == SlotKind::GcRef {
        frame.store_gc(0, value.low as *mut u8);
    } else {
        frame.store_word(0, value.low as i64);
    }
    // Inline-pair task ABI reserves slot 1 for the task ID.
    frame.store_word(2, value.high as i64);
    crate::task::RUNTIME_POLL_READY
}

#[willow_runtime_macros::ffi_boundary]
unsafe extern "C" fn cancel<S: NativeFrameSpec>(raw: *mut c_void) {
    let frame = unsafe { NativeTaskFrame::<S>::from_raw(raw) };
    let operation = frame.take_native::<Operation>(3);
    if !operation.is_null() {
        let operation = unsafe { Box::from_raw(operation) };
        // The frame's native slot is cleared before suspension. Only owned
        // native state survives; moving GC may rewrite every managed root.
        let _relocation = crate::gc::RelocationSafeScope::enter();
        while unsafe { (operation.cancel)(operation.context) } == 0 {
            suspend_cancel();
        }
    }
}

// Native cancellation callbacks already run on the scheduler's cleanup stack.
// Its existing BlockedSyscall wake protocol delays terminal publication until
// this callback returns, without introducing a new task lifecycle or polling.
fn suspend_cancel() {
    #[cfg(any(
        all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
    ))]
    if crate::native_stack::suspend_with_result(crate::task::RUNTIME_POLL_BLOCKED_SYSCALL) {
        return;
    }
    crate::failure::fatal_invariant("Rust async cancellation requires a native cleanup stack");
}

fn spawn<S: NativeFrameSpec>(operation: Operation, start: Start) -> *mut c_void {
    let Some(frame) = NativeTaskFrame::<S>::allocate() else {
        crate::failure::fatal_invariant("Rust async frame allocation failed");
    };
    let context = operation.context;
    frame.store_native(3, Box::into_raw(Box::new(operation)));
    crate::scheduler::spawn_global_task_initialized(
        poll::<S>,
        frame.as_raw(),
        Some(cancel::<S>),
        |id| {
            frame.store_word(1, id as i64);
            unsafe { start(context, id) };
        },
    );
    frame.as_raw()
}

/// Generated bridge callbacks own `context`. `start` publishes executor work;
/// `finish` transfers owned output, and `cancel` queues destruction on that
/// executor. All three callbacks contain Rust panics and never block.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_rust_bridge_async_spawn(
    context: *mut u8,
    start: Start,
    finish: Finish,
    cancel: Cancel,
    reference: u32,
) -> *mut c_void {
    let operation = Operation {
        context,
        finish,
        cancel,
    };
    if reference != 0 {
        spawn::<ReferenceFrame>(operation, start)
    } else {
        spawn::<ValueFrame>(operation, start)
    }
}
