//! Private compiler/runtime debug traversal descriptors, independent of the
//! Rust bridge wire ABI. Word zero is the root node offset; zero means absent.
//! Nodes contain only offsets into this immutable, process-lifetime array.
pub const HANDLE: u64 = 1;
pub const NULLABLE: u64 = 2;
pub const TAGGED: u64 = 3;
pub const ARRAY: u64 = 4;
pub const RECORD: u64 = 5;
