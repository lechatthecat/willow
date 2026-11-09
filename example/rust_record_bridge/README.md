# Rust value records

Run `willow run --offline` in this directory. The adapter uses the local dependency shared with `rust_value_bridge`.

Declare a record as a `class` inside `extern rust`, before functions or other records that use it. Record and field names that Rust forbids even as raw identifiers (`crate`, `self`, `Self`, `super`, `_`) are rejected with a diagnostic. Other Rust keywords may use their generated raw spelling. Only public instance fields are accepted; constructors, methods, inheritance, generic records, recursive records and static fields are rejected. Fields may use any supported boundary type, including earlier records, opaque handles, Option and Result.

The generated adapter exposes `crate::records::Packet<T0, ...>` with one Rust type parameter per field. `crate::record_inputs::Packet<'a>` and `crate::record_outputs::Packet` are convenient aliases for its borrowed input and owned output forms. Record names share the adapter namespace; conflicting field definitions under the same name are diagnosed. These are ordinary Rust structs: no Rust struct layout is shared with Willow, and no `repr` attribute is required.

Boundary conversions:

| Willow | Rust input | Rust output |
| --- | --- | --- |
| `String` | `&str` | `String` |
| `Array<i64>` (legacy bytes, values 0–255) | `&[u8]` | `Vec<u8>` |
| `Array<String>` | `Vec<&str>` | `Vec<String>` |
| `Array<f64>` | `Vec<f64>` | `Vec<f64>` |
| `Array<bool>` | `Vec<bool>` | `Vec<bool>` |
| declared record | generated input alias | generated output alias |

Option and Result compose with these types on either side of a call. Other array element types are rejected. Existing byte-array semantics and signatures are unchanged.

Inputs are snapshots: arrays and records are converted into Rust values; input string contents are borrowed and pinned for the synchronous call. Input byte elements are converted once into a frame-owned buffer. Native adapters must not retain borrowed data beyond the call. Outputs are copied directly into rooted Willow storage once; adapters decide whether to clone input data when constructing an owned result. Record identity and sharing are not transported. GC tracing covers reference fields, including records wider than 64 storage words.

The query retains schema 1 and adds `Array`/`Record` signature variants and a `records` definition table per bridge. Record definitions enter the ABI fingerprint. Each distinct record has one generated decode helper and one encode helper, shared across functions and nested uses.
