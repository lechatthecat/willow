# Inline String metadata audit — 2026-10-26

## Scope and API inventory

The existing O(1), non-allocating String metadata operation is `len()`.
`src/semantic/intrinsics.rs::resolve` exposes len, substring/slice, split,
contains, find, trim, repeat and starts_with; String.toString is already identity.
`check_collections.rs::check_index` accepts arrays and frozen arrays, not String.
There is no String byteAt/charAt/index intrinsic or runtime byte-access ABI to
inline. This change introduces no new indexing API or codepoint semantics.
Substring/slice remain allocating runtime operations, with their existing
UTF-8 boundary checks and recoverable range errors. Search/comparison operations
scan content and are not O(1) metadata reads. Scalar toString and concatenation
continue to allocate through the runtime.

## Implementation and call-path audit

String references point after the GC header to a payload containing an i64 byte
length at offset zero, UTF-8 data at offset eight, and one trailing NUL.
`willow_abi::string_layout` now owns these constants; both the runtime allocation,
copy/read helpers and compiler consume them. The shared GC header already lives
in willow_abi and is not accessed by the length operation. Layout constants enter
the ABI fingerprint, preventing stale compiler/runtime combinations from silently
using different layouts. Runtime ABI length remains available to native callers.

Cranelift branches on null before a single length load. Null returns zero.
There is no speculative null load, allocation, safepoint, UTF-8 scan, or runtime
call in this operation. The early intrinsic path bypasses transient operand-root
push/pop; general LIR lifetime roots and function/loop safepoints remain intact.
Receiver expressions still evaluate once, before the intrinsic. Rooted receivers
are reloaded through the existing LIR machinery after collection. No pointer is
borrowed across a new GC point. Runtime-symbol/effect metadata was updated to
match the emitted operation. Final inspection covered intrinsic resolution,
LIR emission, root handling, runtime String creation/copy/read paths, ABI hashing,
and emitted x86-64 instructions (null test/branch followed by one 64-bit load).

## Deterministic scaling and complexity

Let S be the number of source length sites, B the receiver byte length and R the
number of dynamic evaluations. Each site emits two blocks, one null comparison,
one conditional branch, one length load, one join and a scalar result. Emission
work, IR memory and added code are O(S); each evaluation is O(1) time/space
independent of B, and R evaluations cost O(R). Reading one stored word is the
necessary lower bound for a non-constant receiver. No per-receiver cache or
repeated content scan is introduced. Existing function prologues and loop polls
are outside the length operation and retained for GC/preemption correctness.

`scaling.py` builds distinct functions to prevent combining identical sites and
measures actual ELF symbol sizes with binutils. Release counts:

| Sites | Length runtime relocations | Total function bytes | Bytes/function |
| ---: | ---: | ---: | ---: |
| 1 | 0 | 399 | 399 |
| 8 | 0 | 3192 | 399 |
| 32 | 0 | 12768 | 399 |

These sizes include existing function machinery; they are not a claim of 399
bytes added per read. Integration relocation checks also cover debug builds.
Large receiver, repeated-loop receiver mutation, multiple helper functions and
collection-induced movement are covered below. No inheritance/module traversal
or input-dependent nested pass was introduced. Asymptotic conclusions follow
from the emission/call paths and deterministic counts, not benchmark timings.

## Release microbenchmark

Baseline source revision: `20bcc1d354829b5b52d5f428b945c39dfa5a4ded`.
Host: Linux x86-64, AMD Ryzen 7 7800X3D, rustc 1.96.0.
Both compiler and emitted program were built in release mode. `length.wi`
evaluates length 100,000,000 times on a nine-byte UTF-8 receiver, retaining loop
polls and a checksum of 900000000. One warmup, seven subprocess wall-clock samples
per binary; raw values are in before.json and after.json. Timing includes startup
and is a local microbenchmark, not an application-wide speedup claim.

Median before: 0.271848483 s. Median after: 0.106589539 s.
Ratio before/after: 2.550x; elapsed time reduction: 60.79%.
The baseline executable disassembly contains a call to `willow_string_len` in
the loop; the new object contains no such relocation. The runtime payload layout
itself did not change. Binaries/objects remain temporary and are not published.

Reproduce on each compiler revision (use distinct output paths):

```sh
cargo build --release -p willow --bin willow
WILLOW_KEEP_OBJECT=1 target/release/willow build docs/audits/string-metadata/length.wi --release -o /tmp/string-length
python3 docs/audits/string-metadata/measure.py /tmp/string-length LABEL
python3 docs/audits/string-metadata/scaling.py target/release/willow
```

## Test perspectives and results

New integration tests passed (4 tests, containing these explicit perspectives):

1. Empty literal length.
2. One ASCII byte.
3. Multiple ASCII bytes.
4. Two-byte UTF-8 character.
5. Three-byte UTF-8 character.
6. Four-byte UTF-8 character.
7. Combining character sequence counts bytes, not glyphs.
8. Embedded NUL counts as data.
9. Mixed-width UTF-8 passed through a function parameter.
10. Side-effecting receiver evaluated exactly once.
11. Concatenation result length.
12. Allocating scalar toString result length.
13. Trim result length.
14. Large repeated receiver (16,384 bytes).
15. First split element length.
16. Second split element length.
17. Reassigned receiver in a loop.
18. Defer body length read.
19. Substring on a two-byte boundary.
20. Substring on a three-byte boundary.
21. Substring on a four-byte boundary ending at len().
22. Empty substring at the end position.
23. Length after explicit collection.
24. Relocating GC stress.
25. Debug execution.
26. Release execution.
27. Negative substring index still panics.
28. Past-end substring still panics.
29. Reversed range still panics.
30. Start inside a two-byte character still panics.
31. End inside a three-byte character still panics.
32. End inside a four-byte character still panics.
33. Empty range past end still panics.
34. Zero length-call relocations at 1, 8, 32 sites, debug and release.

Commands completed successfully:

```sh
cargo test -p willow --test integration string_metadata -- --nocapture
cargo test -p willow_runtime string_length_counts_utf8_bytes_including_embedded_nul
cargo test -p willow_runtime string_methods_
cargo test -p willow_runtime string_payload_size_boundaries
cargo test -p willow_runtime concat_increasing_sizes_preserves_bytes_and_terminator
cargo test -p willow_runtime array_join_measured_linear_work
cargo test -p willow_abi abi_hash_covers_every_contract_constant
cargo test -p willow --test integration concat_fusion_empty_and_non_ascii_operands
cargo fmt --all -- --check
```

The runtime selections total seven tests; the ABI selection one, and the existing
concat integration selection one. They cover null ABI inputs, layout size overflow,
terminators, repeat/substring/join copy scaling and compatibility hashing.
`example/string_metadata.wi` is executed by the new integration test in debug,
release and relocation stress modes with exact output assertions.

`cargo clippy --workspace --all-targets --target TARGET -- -D warnings` passed for
x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc, aarch64-apple-darwin and
x86_64-apple-darwin. Native execution was Linux x86-64 only; Windows and both macOS
architectures were not executed. CI was not queried. Full local suites and the
full example audit were intentionally not run. Generated null-branch behavior
was inspected in machine code; runtime null semantics were executed in runtime
tests, but no language-level null String construction is exposed by this API.
