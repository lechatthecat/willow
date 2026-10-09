//! Canonical scalar Rust bridge ABI. Scalars require no aggregate layout.
pub const WILLOW_RUST_BRIDGE_ABI_REVISION: u32 = 1;

/// Bridge metadata flag, separate from the runtime effect lattice.
pub const FOREIGN_CALL: u8 = 1 << 7;
