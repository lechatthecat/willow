# Native Rust modules

Run `willow run example/rust_modules` from the repository root, or `willow run .`
from this directory. Expected output:

```text
true
true
Willow
```

`import native;` loads `src/native.rs`. `import math::value as answer;` imports
one function. No Willow declarations, `[rust]` section, or Cargo dependency are
needed. Rust dependencies can still be declared in `[rust-dependencies]`; legacy
`extern rust` and `[rust] bridge` declarations can coexist with native modules.
All reachable Rust modules are compiled into one project bridge crate and share
its lock and fingerprint. `willow check` derives native-only interfaces without
starting Cargo; `willow query rust-bridge --project-dir .` reports those signatures.
A native build still requires Cargo and rustc. Checking does not validate Rust
function bodies or resolve Rust dependency types.

Only unrestricted `pub fn`, `pub async fn`, and `pub struct` form the interface.
Use private visibility or `pub(crate)` for implementation details. Methods and
macro expansion are not imported as Willow functions. Generics, explicit
lifetimes, `impl Trait`, unsupported types, and conditional interface attributes
produce diagnostics at their Rust source location. A Willow module and a Rust
module with the same name is an error. Exported names must also be valid Willow identifiers.

Canonical standard-library type paths such as `::std::string::String` and
`::core::primitive::i64` are recognized. A local alias, private type, or custom
import that shadows a builtin is diagnosed when exposed in a public signature.
With unknown wildcard imports, use explicit canonical standard paths or imports;
checking does not guess what a Rust dependency exports.

Supported mappings:

| Rust signature | Willow type |
| --- | --- |
| `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`, `char`, `f64`, `bool` | Same scalar |
| `()` return | `void` |
| `String`, input `&str` | `String` |
| `Vec<u8>`, input `&[u8]` | `Array<u8>` |
| `Vec<scalar>`, `Vec<String>` | `Array<scalar>`, `Array<String>` |
| `Option<T>`, `Result<T, E>` | Corresponding Willow generic type |
| Named public struct with public supported fields, passed by value | Record class |
| `Box<T>` return and `&T` input for a public struct | Opaque handle |

A struct used through `Box<T>`/`&T` is opaque: its private representation remains
Rust-owned. A generated `T_close(handle)` explicitly releases it; automatic
handle cleanup also uses the existing bridge lifetime rules. The same struct
cannot also be passed as a value record. Records may contain nested records and
return owned opaque handles, including inside Option/Result. A record input
cannot consume an owned handle; pass the handle as a separate `&T` argument.

Owned String/byte-vector arguments and record fields copy borrowed Willow data
when Rust needs ownership. Borrowed arguments reuse the existing bridge views.
Scalar arrays move the existing bridge vector without another conversion pass.
Async borrowing follows the same Send/Sync restrictions as `extern rust`.
Synchronous Rust calls, including explicit close, cannot run inside an async
Willow task without the existing nonblocking proof. Use native async functions
for task code; their adapters own arguments until the future finishes.
Every derived function retains conservative MAY_BLOCK effects; there is no
native-module nonblocking assertion in this phase.
