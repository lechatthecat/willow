# Rust value records

Run `willow run --offline` in this directory. The expected output is 22 `true` lines.

`src/native.rs` defines `pub struct Packet` with public owned fields. Willow imports
that struct and its functions directly. `new Packet(...)` constructs a Willow
value; adapters copy fields across the boundary without sharing Rust layout.
Public record fields support scalars, `String`, supported `Vec` elements, nested
records, and composed `Option`/`Result`. Recursive and generic records are rejected.
Opaque handles may be returned inside records; pass them separately by reference
when calling Rust instead of consuming them through a record input.

Owned Rust strings, vectors and records are snapshots. Mutating or dropping the
native copy cannot change Willow input values. Borrowed `&str` and `&[u8]` parameters
are supported for functions; async adapters retain owned storage across awaits.
Results are copied into rooted Willow storage. Record identity and sharing are not
transported. Each distinct record has shared decode and encode helpers, and query
schema 2 reports its ordered field definitions.
