//! Canonical Rust bridge ABI. Aggregates use pointer transport and explicit tags.
pub const WILLOW_RUST_BRIDGE_ABI_REVISION: u32 = 1;

/// Bridge metadata flag, separate from the runtime effect lattice.
pub const FOREIGN_CALL: u8 = 1 << 7;

/// Pointer-only C transport; never a Rust enum or Rust allocation layout.
/// Scalar bits use `low`; scalar pairs use explicit tag (`low`) and payload
/// (`high`). Reference words are opaque to user Rust adapters.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WillowBridgeValue {
    pub low: u64,
    pub high: u64,
}

/// Borrowed UTF-8/bytes. The owner must outlive the synchronous call.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WillowSliceU8 {
    pub ptr: *const u8,
    pub len: usize,
}
