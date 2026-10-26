# Array literal allocation implementation audit — 2026-10-21

## Change and acceptance

Constant-length literals use `willow_array_new_literal` (`ALLOC`, two I64 arguments,
pointer result). `Array::new()` lowers to an empty literal and now uses the same
allocation-only effect summary. The existing `willow_array_new` remains
`PANIC_ALLOC`; negative and overflowing dynamic lengths retain recoverable language
panics. Both entries share `allocate_array`, including TLAB hit/miss, reference
registration, old-generation fallback, and temporary rooting.

The literal entry rejects invalid lengths fatally before allocation. Its validated
range ensures the buffer header plus elements fits the existing signed byte-size
calculation. Managed allocation exhaustion already terminates through
`failure::resource_exhausted`; the entry also refuses to return a null allocation.
This uses the existing non-recoverable resource-exhaustion exit policy rather than
introducing a different OOM signal. Rust unwinding remains contained by the same
FFI boundary macro. No recoverable panic snapshot is needed around this entry.

## Measured evidence

`count_calls.py` creates temporary source/binaries, builds release programs, and
uses gdb breakpoints to count executed ABI entries. `call-counts.txt` records 1,
8, 32, and 128 literals: exactly that many literal-allocation calls, with zero old
allocation, panic-depth, or scalar-store calls. These are allocation-related call
counts, not a claim that function prologues or safepoints have no runtime calls.

Object/codegen event tests additionally prove that both debug and release builds
emit one allocation call per empty/scalar/constructor site and no panic snapshots
attributable to allocation. Scalar stores retain cold bounds-fault calls and their
snapshots in the object; those paths are not executed by valid initialization.
This distinction matters because the earlier panic-depth optimization already inlined those reads:
zero `willow_panic_depth` relocations alone would not prove removal of the bracket.

Increasing source-size tests cover conditional empty literals (1, 8, 32, 128
branch pairs, giving 2, 16, 64, 256 allocation relocations) and repeated frozen
constructors (1, 8, 32, 128, giving one allocation/copy per constructor). These
measure emitted call counts, not wall-clock performance or total binary size.

## Implementation and complexity audit

Inspected literal lowering in `ir/lower.rs`, `ir/lowered.rs` and
`ir/lowered/value/lower.rs`; allocation emission in `flat_objects.rs`; metadata
selection in `emit_stmt.rs`; LIR/root treatment in `root_effect.rs`; static effects
in `semantic/intrinsics.rs`; AI effect evidence in `ai/mod.rs`; allocation and
failure paths in runtime `array.rs`, `gc.rs`, and `gc/runtime_tlab.rs`.

Independent dimensions are literal sites S, total element expressions E, runtime
literal evaluations R, and element slots initialized per evaluation N. This change
adds O(1) compiler work and generated instructions per allocation site, without
new analysis passes, scans, or per-element metadata. Existing initialization remains
O(E) emitted work across sites. Allocation bookkeeping stays O(1) on TLAB hits;
zeroing/initialization and storage remain O(N), which is required by the current
materialized array representation. Across evaluations this is O(sum N), plus
existing collector work depending on the live heap and roots. No claim is made
that collection is constant-time. Existing reference tracing also remains O(N).

The runtime helper is shared, so branch/exit counts do not duplicate the allocation
algorithm. There is no nested new scan across sites, callers, or element types.
The ABI `MAY_ALLOCATE` bit remains set; collection/root reload obligations are not
weakened when `MAY_PANIC` disappears. Relocation tests exercise this with nested
reference arrays. Build scheduling, runtime auto-build, and link caching are
unchanged. No additional avoidable scaling or separately scoped defect was found.

## Explicit test perspectives

1. Empty literal emits one allocation-only call.
2. Nonempty i64 literal emits one allocation-only call.
3. Empty `Array::new()` shares that ABI.
4. Debug codegen has no allocation panic bracket.
5. Release codegen has no allocation panic bracket.
6. Cold scalar-store panic brackets remain present.
7. Executed scalar stores use the inline normal path.
8. Repeated runtime evaluations scale 1/8/32/128 calls exactly.
9. Conditional allocation sites scale 2/16/64/256 calls exactly.
10. Frozen constructor allocation/copy sites scale linearly.
11. CLI effects retain `MAY_ALLOCATE` for empty/scalar/constructor forms.
12. CLI effects omit `MAY_PANIC` for those forms.
13. Panicking child expressions still contribute `MAY_PANIC`.
14. Child panic skips remaining initialization and reaches recover.
15. A literal allocated during an active panic preserves recovery.
16. f64 and bool elements retain correct values.
17. Allocating string elements remain rooted.
18. Nested array elements survive minor collection.
19. Object elements survive collection during unwind.
20. Unregistered runtime threads allocate literals in the old generation.
21. Registered TLAB owners allocate adjacent handle/buffer objects.
22. Empty/reference/64-element TLAB arrays are zero-initialized.
23. Oversized TLAB buffers fall back to old allocation.
24. Relocating allocation misses rewrite caller roots and nested edges.
25. Dynamic -1 and i64::MIN lengths remain recoverable.
26. Dynamic i64::MAX capacity overflow remains recoverable.
27. Dynamic valid lengths 0/1/64/1000 retain their result and panic depth.
28. Invalid literal ABI lengths cannot return to generated code.
29. Literal allocation exhaustion exits without returning a neutral pointer.
30. ABI function type, schema kinds, and allocation/panic classifications agree.
31. Existing optional/interface array element coercion tests still pass.
32. Existing async array-element await test still passes.
33. Runnable example passes normal debug/release and minor/relocate/alloc stress.

## Validation

Passed:

- `cargo fmt --all -- --check`.
- `cargo clippy --workspace --all-targets -- -D warnings`.
- The same Clippy command with `--target` for x86_64-unknown-linux-gnu,
  x86_64-pc-windows-msvc, aarch64-apple-darwin, x86_64-apple-darwin.
- `cargo test -p willow --test integration array_literal_ -- --nocapture`:
  9 passed, including the example under five configurations.
- `cargo test -p willow_runtime --lib array_literal -- --nocapture`: 4 passed.
- `cargo test -p willow_runtime --lib array_dynamic_lengths_preserve_recoverable_panics`:
  1 passed.
- `cargo test -p willow_runtime --lib payload_free_collection_and_lock_rows_stay_i64`:
  1 passed.
- `cargo test -p willow --test semantic_cli array_literal_effect_queries_match_allocation_only_abi`:
  1 passed.
- `cargo test -p willow --lib backend::abi:: -- --nocapture`: 37 passed.
- `cargo test -p willow --test integration contextual_empty_array_allocation_sites_scale_linearly`:
  1 passed, exact counts 2/16/64/256.

The touched frozen-constructor scaling test exposed a pre-existing stale expectation:
non-escaping maps already use `willow_map_new_local` from `emit_collections.rs`.
The assertion now sums the two map constructor symbols and still requires exactly
one allocation per site; no compiler map behavior changed. The focused rerun passed
(1 test), including 1/8/32/128 allocation/copy sites for arrays and maps.

The focused `array_fast_paths_bounds_recover_and_defer` regression also passed
(1 test, debug/release negative and out-of-range reads/stores).

Native execution is Linux x86_64 only. macOS arm64, macOS x86_64, and Windows
x86_64 native link/run were not performed. No CI results were requested or checked.
Full local suites and full runnable-example audit were omitted under project policy.

Reproduce the focused regression with `cargo test -p willow --test integration
array_literal_ -- --nocapture`; the example is
`example/array_literal_allocation.wi`. Run `python3
docs/audits/array-literal-allocation/count_calls.py` after building the compiler for gdb counts.

Ticket remains in progress for independent review; no staging, commit, push, or
closure is part of this implementation turn.
