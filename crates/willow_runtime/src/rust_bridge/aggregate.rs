//! Synchronous aggregate transport. Frames own stable collector-rewritable slots.
use crate::{array, gc, string};
use willow_abi::ffi::{WillowBridgeValue, WillowSliceU8};
use willow_abi::{EnumVariantLayout, SlotKind};

#[cfg(test)]
std::thread_local! {
    static INPUT_COPIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static OUTPUT_COPIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Default)]
struct Frame {
    // Box storage must remain stable when the vector grows.
    #[allow(clippy::vec_box)]
    roots: Vec<Box<*mut u8>>,
}

impl Drop for Frame {
    fn drop(&mut self) {
        for slot in &mut self.roots {
            gc::willow_gc_remove_runtime_root_slot(&mut **slot);
        }
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_frame_new() -> *mut u8 {
    Box::into_raw(Box::<Frame>::default()).cast()
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_frame_drop(frame: *mut u8) {
    unsafe { drop(Box::from_raw(frame.cast::<Frame>())) }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_root(frame: *mut u8, object: u64) {
    if object == 0 {
        return;
    }
    let frame = unsafe { &mut *frame.cast::<Frame>() };
    let mut slot = Box::new(object as *mut u8);
    gc::willow_gc_add_runtime_root_slot(&mut *slot);
    frame.roots.push(slot);
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_string_data(object: u64, out: *mut WillowSliceU8) {
    let object = object as *const u8;
    let len = string::willow_string_len(object) as usize;
    // Runtime-created String storage is old-generation and never moves.
    let ptr = if object.is_null() {
        std::ptr::NonNull::<u8>::dangling().as_ptr()
    } else {
        unsafe { object.add(8) }
    };
    unsafe {
        *out = WillowSliceU8 { ptr, len };
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_bytes_len(object: u64) -> usize {
    array::willow_array_len(object as *mut u8) as usize
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_bytes_copy(object: u64, out: *mut u8, len: usize) {
    if len != willow_rust_bridge_bytes_len(object) {
        crate::failure::fatal_invariant("rust_bridge_bytes_length");
    }
    for index in 0..len {
        let value = array::willow_array_get(object as *mut u8, index as i64);
        let Ok(byte) = u8::try_from(value) else {
            crate::failure::fatal_invariant("rust_bridge_byte_out_of_range");
        };
        unsafe {
            *out.add(index) = byte;
        }
        #[cfg(test)]
        INPUT_COPIES.set(INPUT_COPIES.get() + 1);
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_buffer(
    frame: *mut u8,
    bytes: *const u8,
    len: usize,
    is_string: u32,
) -> u64 {
    let Ok(length) = i64::try_from(len) else {
        crate::failure::fatal_invariant("rust_bridge_buffer_length");
    };
    let object = if is_string != 0 {
        string::willow_string_alloc(bytes, length)
    } else {
        array::willow_array_new(length, 0)
    };
    willow_rust_bridge_root(frame, object as u64);
    if is_string == 0 {
        for index in 0..len {
            array::willow_array_set(object, index as i64, unsafe { *bytes.add(index) } as i64);
            #[cfg(test)]
            OUTPUT_COPIES.set(OUTPUT_COPIES.get() + 1);
        }
    }
    object as u64
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_tag(object: u64) -> u64 {
    unsafe { *(object as *const u64) }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_payload(object: u64, pair: u32, out: *mut WillowBridgeValue) {
    let layout = EnumVariantLayout::new(0, &[]);
    let payload = unsafe {
        (object as *const u8)
            .add(layout.payload_byte_offset(8) as usize)
            .cast::<u64>()
    };
    unsafe {
        *out = WillowBridgeValue {
            low: *payload,
            high: if pair != 0 { *payload.add(1) } else { 0 },
        };
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_enum(
    frame: *mut u8,
    tag: u64,
    value: *const WillowBridgeValue,
    pair: u32,
    reference: u32,
) -> u64 {
    let value = unsafe { *value };
    let kind = match (pair != 0, reference != 0) {
        (false, false) => SlotKind::Word,
        (false, true) => SlotKind::GcRef,
        (true, false) => SlotKind::ScalarPair,
        (true, true) => SlotKind::InterfacePair,
    };
    let slots = [kind];
    let layout = EnumVariantLayout::new(tag as u32, &slots);
    let mut referent = value.low as *mut u8;
    if reference != 0 {
        gc::willow_gc_add_runtime_root_slot(&mut referent);
    }
    let object = gc::willow_alloc_with_layout(
        gc::GcObjectKind::Enum,
        0,
        layout.payload_bytes(8) as i64,
        layout.gc_ref_mask(),
    );
    if reference != 0 {
        gc::willow_gc_remove_runtime_root_slot(&mut referent);
    }
    willow_rust_bridge_root(frame, object as u64);
    unsafe {
        *object.cast::<u64>() = tag;
        let payload = object
            .add(layout.payload_byte_offset(8) as usize)
            .cast::<u64>();
        if reference != 0 {
            gc::willow_gc_write_barrier(
                object,
                std::ptr::null_mut(),
                referent,
                willow_abi::GcStoreDestination::EnumPayload as i64,
            );
            gc::store_gc_reference(payload.cast(), referent);
        } else {
            *payload = value.low;
        }
        if pair != 0 {
            *payload.add(1) = value.high;
        }
    }
    object as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_roundtrip_roots_and_linear_buffers() {
        const KEY: &str = "WILLOW_BRIDGE_AGGREGATE_CHILD";
        if std::env::var_os(KEY).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "rust_bridge::aggregate::tests::aggregate_roundtrip_roots_and_linear_buffers",
                    "--nocapture",
                ])
                .env(KEY, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let before = gc::runtime_root_count();
        let frame = willow_rust_bridge_frame_new();
        for len in [0, 1, 16, 256, 4096, 65536] {
            INPUT_COPIES.set(0);
            OUTPUT_COPIES.set(0);
            let bytes: Vec<u8> = (0..len).map(|index| index as u8).collect();
            let object = willow_rust_bridge_buffer(frame, bytes.as_ptr(), len, 0);
            assert_eq!(willow_rust_bridge_bytes_len(object), len);
            let mut output = vec![0; len];
            willow_rust_bridge_bytes_copy(object, output.as_mut_ptr(), len);
            assert_eq!(output, bytes);
            assert_eq!(INPUT_COPIES.get(), len);
            assert_eq!(OUTPUT_COPIES.get(), len);
            eprintln!(
                "bytes length={len} input_copy={} output_copy={}",
                INPUT_COPIES.get(),
                OUTPUT_COPIES.get()
            );
        }
        for repeats in [0, 1, 16, 256, 4096] {
            let text = "é🙂".repeat(repeats);
            string::ALLOCATION_COPY_BYTES.set(0);
            let object = willow_rust_bridge_buffer(frame, text.as_ptr(), text.len(), 1);
            assert_eq!(string::ALLOCATION_COPY_BYTES.get(), text.len());
            let mut slice = WillowSliceU8 {
                ptr: std::ptr::null(),
                len: 0,
            };
            willow_rust_bridge_string_data(object, &mut slice);
            assert_eq!(slice.len, text.len());
            assert_eq!(
                string::ALLOCATION_COPY_BYTES.get(),
                text.len(),
                "borrow copies zero bytes"
            );
            eprintln!(
                "string bytes={} output_copy={} input_copy=0",
                text.len(),
                string::ALLOCATION_COPY_BYTES.get()
            );
        }
        let text = "héllo 世界\0";
        let object = willow_rust_bridge_buffer(frame, text.as_ptr(), text.len(), 1);
        let collections = gc::willow_gc_major_collections();
        gc::willow_gc_collect();
        assert!(gc::willow_gc_major_collections() > collections);
        let mut slice = WillowSliceU8 {
            ptr: std::ptr::null(),
            len: 0,
        };
        willow_rust_bridge_string_data(object, &mut slice);
        assert_eq!(
            unsafe { std::slice::from_raw_parts(slice.ptr, slice.len) },
            text.as_bytes()
        );
        for pair in [0, 1] {
            let value = WillowBridgeValue { low: 17, high: 29 };
            let object = willow_rust_bridge_enum(frame, 1, &value, pair, 0);
            let mut output = WillowBridgeValue::default();
            willow_rust_bridge_payload(object, pair, &mut output);
            assert_eq!(willow_rust_bridge_tag(object), 1);
            assert_eq!(output.low, 17);
            assert_eq!(output.high, if pair == 0 { 0 } else { 29 });
        }
        willow_rust_bridge_frame_drop(frame);
        assert_eq!(gc::runtime_root_count(), before);
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_panic_message(bytes: *const u8, len: usize) -> ! {
    let message = if len == 0 {
        "".into()
    } else {
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(bytes, len) })
    };
    crate::failure::fatal_invariant(&format!("RustPanic: rust_bridge_panic: {message}"));
}
