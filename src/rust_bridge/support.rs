// Included into generated bridge crates, not compiled as a compiler module.
use willow_bridge_abi::{WillowBridgeValue as BridgeValue, WillowSliceU8};
unsafe extern "C" {
    fn willow_rust_bridge_frame_new() -> *mut u8;
    fn willow_rust_bridge_frame_drop(frame: *mut u8);
    fn willow_rust_bridge_root(frame: *mut u8, object: u64);
    fn willow_rust_bridge_string_data(object: u64, out: *mut WillowSliceU8);
    fn willow_rust_bridge_bytes_len(object: u64) -> usize;
    fn willow_rust_bridge_bytes_copy(object: u64, out: *mut u8, len: usize);
    fn willow_rust_bridge_buffer(frame: *mut u8, bytes: *const u8, len: usize, string: u32) -> u64;
    fn willow_rust_bridge_tag(object: u64) -> u64;
    fn willow_rust_bridge_payload(object: u64, pair: u32, out: *mut BridgeValue);
    fn willow_rust_bridge_enum(
        frame: *mut u8,
        tag: u64,
        value: *const BridgeValue,
        pair: u32,
        reference: u32,
    ) -> u64;
}
struct BridgeFrame {
    raw: *mut u8,
    handles: std::cell::RefCell<Vec<handles::Lease>>,
    // Each inner allocation remains stable when the outer vector grows.
    bytes: std::cell::RefCell<Vec<Vec<u8>>>,
}
impl BridgeFrame {
    fn new() -> Self {
        Self {
            raw: unsafe { willow_rust_bridge_frame_new() },
            bytes: Default::default(),
            handles: Default::default(),
        }
    }
    fn handle<T: std::any::Any + Send + Sync>(&self, id: u64, name: std::any::TypeId) -> &T {
        let lease = handles::get(id, name).unwrap_or_else(|error| std::panic::panic_any((error, id)));
        let value = lease.downcast_ref::<T>().expect("opaque adapter type invariant") as *const T;
        self.handles.borrow_mut().push(lease);
        // The Arc keeps the boxed object stable until this frame is dropped,
        // even if another call closes the ID. No mutable references escape.
        unsafe { &*value }
    }
    fn release_handles(&mut self) {
        // Release native borrows before allocating Willow output objects. A
        // user destructor may itself call into the collector.
        let leases = std::mem::take(self.handles.get_mut());
        drop(leases);
    }
    fn root(&self, object: u64) {
        unsafe { willow_rust_bridge_root(self.raw, object) }
    }
    fn string(&self, object: u64) -> &str {
        let mut slice = WillowSliceU8 {
            ptr: std::ptr::null(),
            len: 0,
        };
        unsafe {
            willow_rust_bridge_string_data(object, &mut slice);
            std::str::from_utf8(std::slice::from_raw_parts(slice.ptr, slice.len))
                .expect("Willow UTF-8 invariant")
        }
    }
    fn bytes(&self, object: u64) -> &[u8] {
        let len = unsafe { willow_rust_bridge_bytes_len(object) };
        let mut bytes = vec![0; len];
        unsafe { willow_rust_bridge_bytes_copy(object, bytes.as_mut_ptr(), len) };
        let ptr = bytes.as_ptr();
        self.bytes.borrow_mut().push(bytes);
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
    fn output_string(&self, value: &str) -> BridgeValue {
        self.buffer(value.as_bytes(), true)
    }
    fn output_bytes(&self, value: &[u8]) -> BridgeValue {
        self.buffer(value, false)
    }
    fn buffer(&self, value: &[u8], string: bool) -> BridgeValue {
        BridgeValue {
            low: unsafe {
                willow_rust_bridge_buffer(self.raw, value.as_ptr(), value.len(), u32::from(string))
            },
            high: 0,
        }
    }
    fn tag(&self, value: BridgeValue) -> u64 {
        unsafe { willow_rust_bridge_tag(value.low) }
    }
    fn payload(&self, value: BridgeValue, pair: bool) -> BridgeValue {
        let mut output = BridgeValue::default();
        unsafe { willow_rust_bridge_payload(value.low, u32::from(pair), &mut output) };
        output
    }
    fn enum_value(&self, tag: u64, value: BridgeValue, pair: bool, reference: bool) -> BridgeValue {
        BridgeValue {
            low: unsafe {
                willow_rust_bridge_enum(
                    self.raw,
                    tag,
                    &value,
                    u32::from(pair),
                    u32::from(reference),
                )
            },
            high: 0,
        }
    }
}
impl Drop for BridgeFrame {
    fn drop(&mut self) {
        // User destructors run while outputs remain rooted and inside the
        // generated catch_unwind boundary.
        self.handles.get_mut().clear();
        unsafe { willow_rust_bridge_frame_drop(self.raw) }
    }
}
