# willow-8hq4.33 — inline Array allocation audit (2026-10-21)

## Implementation

Constant array literals whose buffer fits the 4096-byte young-object limit now
reserve handle and buffer with one generated TLAB bump. On supported 64-bit
targets this includes lengths 0 through 509. The check accepts exact fits and
rejects null cursors, unsigned addition overflow and insufficient remaining space.
Larger literals retain their existing allocation-only runtime call. Dynamic
runtime lengths and their recoverable validation are unchanged.

Both objects use the existing ABI descriptor fingerprint, object kind, type ID,
size and reference mask. Header emission is shared with ordinary objects. Handle
fields and the buffer tracing length are initialized before both start bits are
published. The chunk is fresh zeroed storage; no element zeroing loop is added.
There is no call, suspension or GC point between reservation and publication.
Young objects need no insertion barrier at construction. Subsequent element
stores still use existing filtered barriers, including active concurrent marking.
The miss path calls `willow_array_new_literal`, preserving willow-8hq4.30's
allocation-only/no-language-panic contract and caller root reload obligations.

`willow_gc_init` registers native reference-buffer tracing before generated code
can allocate its first reference array. Runtime calls keep the existing lazy,
generation-aware registration for runtime-only tests/paths. A new regression
initializes twice, bypasses all array allocation entries, and verifies that minor
GC relocates both the reference buffer and its child each time. Registration does
not allocate managed objects. Its snapshot/slice hooks remain the existing hooks.

## Measured evidence

Baseline: `11a4d5906d727ed3d6eb63dba44af0acf4d36dd5`, clean working tree before
this change. `measure.py` builds `probe.wi` in release mode, then measures nine
processes with 4,000,000 three-element literal allocations and reads each. Timing
is between stdout markers, excluding startup; it includes collection, function
entry and element access. This is not an isolated bump-instruction latency.

| Metric | Before | After |
|---|---:|---:|
| Median ns/evaluation | 15.2522 | 12.2986 |
| `literal` native function bytes | 839 | 1076 |
| Whole executable text bytes | 1,096,152 | 1,096,496 |

The observed median improves 19.4%, at a cost of 237 bytes in this small function.
Raw samples are in `before.txt` and `after.txt`. This single Linux host/workload
measurement is not a universal performance guarantee. The measured gain supports
retaining the bounded fast path.

`verify_codegen.py` uses gdb breakpoints on allocation ABI entries AND every direct
call instruction in the generated `literal` function. For lengths 0/3/509,
normal and exact-fit allocations execute **zero allocation/initialization runtime
calls**. A one-word shortage executes one literal fallback. Length 510 executes
one fallback. All cases validate results after minor GC. Boundary workloads use
real generated allocations to fill the chunk; no allocator state is patched.

There are two existing function-entry calls (`willow_sync_poll_counter` and
`willow_gc_stop_flag`). They are explicitly reported separately; this work does
not claim an entire allocating function has no runtime calls. Neither old array
allocation, panic-depth nor array-set entries execute in these probes. Gdb
inspection is Linux x86_64-specific and uses the documented r15 context ABI.

Increasing empty-literal sites in a single function gives:

| Sites | Native `batch` bytes | Cold allocation call sites |
|---:|---:|---:|
| 1 | 757 | 1 |
| 8 | 3664 | 8 |
| 32 | 14010 | 32 |
| 128 | 55173 | 128 |

`codegen.txt` records the exact counts. Native bytes include root setup, length
reads, register spills and fault paths; they are not just allocator bytes.

### `array_grow_only` side effect (eager hook registration)

The 2026-10-21 Go/Java benchmark showed `array_grow_only` rising from 26.1 ms
(clean HEAD) to ~32.6 ms with this tree. The cause is not the inline literal
path itself: turning the fast path off for length 0 changed nothing (~31.7 ms).
Removing only the `ensure_trace_registered()` call from `willow_gc_init` restored
26.6 ms. These are seven-trial phase-marker medians.

Measured with `perf stat` and `strace -c` on release binaries:

| Variant | task-clock | page faults | `brk` calls |
|---|---:|---:|---:|
| Hooks registered at `willow_gc_init` | 39.6 ms | 4,785 | 252 |
| Registration removed | 33.9 ms | 1,105 | 14 |
| Registration removed, `grow_after_ref_registration.wi` | — | — | 252 |
| Both variants, glibc trim/mmap thresholds raised | 33.3 ms | 667 | — |

The GC logs show the same cycle count and sizes for both variants. The perf
page-fault call graph is `willow_array_push → alloc_buffer →
allocate_old_region_object_for_move → OldRegion::new → memset`. Large-region
storage released through `dealloc` is trimmed back to the OS by glibc and then
refaulted. Whether glibc trims it depends on unrelated heap layout above it.

`grow_after_ref_registration.wi` registers the same hooks lazily through one
`Array<String>`, which is the HEAD behaviour. It reproduces the churn without the
eager call, so the churn predates this change. Eager registration is required
for correctness: an inline reference buffer can exist before any runtime array
call. It stays in place. Deciding how much freed region storage to retain is
tracked in `willow-8hq4.47`.

## Complexity and responsibility audit

Reviewed `flat_objects.rs`, `gc_codegen.rs`, runtime `array.rs`, GC init/registry,
`gc/runtime_tlab.rs`, TLAB start-map publication and the existing LIR allocation
root/reload contract. Independent dimensions: allocation sites S, total literal
elements E, evaluations R, and live heap/root sizes for collection.

New compiler work is O(1) per site plus descriptor interning through the existing
hash map (expected O(1) per descriptor); no new passes or repeated whole-function
scans. New generated allocation instructions and descriptors are O(S), with at
most two fixed-size descriptors per site before interning. Existing element
initialization remains O(E), necessary for materialized array contents. Fast-path
allocation is O(1) per evaluation, consumes O(N) payload space, and uses fixed
scratch registers; zeroing is paid on chunk creation. GC/refill/tracing costs
remain dependent on the existing live heap, roots and element references. This
is not a claim that a collecting allocation is constant-time.

The two start-map updates use the existing owner-only protocol and occur in
sequence, including when both starts share a bitmap word. Header extraction does
not alter ordinary-object code generation. No extra work scales with exits,
inheritance depth, branches or element types. Increasing-site native counts above
provide deterministic evidence, rather than inferring scaling from elapsed time.
Nested references, repeated calls, chunk exhaustion and two size thresholds are
covered by focused tests.

The allocation algorithm remains shared in the runtime fallback. Each site has
one cold call-and-rejoin block, matching existing object allocation. A single
in-function returning slow block would need continuation IDs, result routing and
merged live-root state for distinct sites, unlike shared nonreturning fault
blocks. It would retain O(S) continuation edges and add dispatch/spill work to
save only a small ABI call sequence per site. This change keeps bounded per-site
blocks rather than introducing that extra control-flow mechanism. The size
tradeoff is measured above and explicitly left for review; no per-site runtime
allocation algorithm or per-element slow path is duplicated.

Compiler lowering owns this optimization; runtime policy/GC stays in the runtime.
Build scheduling, final artifact caching and willowc runtime auto-build are
unchanged. No independently scoped bug was found. No private specification text
was used in this report or its source reproducers.

## Explicit tested perspectives

1. Empty literal normal hit (gdb).
2. Three-element scalar normal hit (gdb).
3. Maximum young buffer, length 509 (gdb and integration).
4. First oversized buffer, length 510 (gdb and integration).
5. Larger 1024-element literal (integration).
6. Empty literal exact fit (gdb).
7. Three-element exact fit (gdb).
8. Maximum young buffer exact fit (gdb).
9. Empty literal one-word shortage (gdb).
10. Three-element one-word shortage (gdb).
11. Maximum young buffer one-word shortage (gdb).
12. Cold first allocation followed by warm hit (gdb).
13. Result/element correctness after minor collection (all boundary probes).
14. Debug size-boundary code generation and execution (new integration test).
15. Release size-boundary code generation and execution (new integration test).
16. GC initialization registers ref tracing before any array ABI call (new runtime test).
17. Reinitialization restores that tracing contract (same test, two resets).
18. Nested array references survive collection (existing allocation example).
19. Allocating string elements survive collection (same example).
20. Class references survive collection during active panic (same example).
21. Float and boolean elements (same example).
22. Empty `Array::new()` remains allocation-only (existing effect test).
23. Child panic skips remaining initialization (existing panic regression).
24. Allocation during unwind retains the active panic (existing example).
25. Runtime unregistered threads retain old allocation (runtime literal tests).
26. Miss path rewrites rooted caller values (runtime literal tests).
27. Relocate/minor/alloc stress retains nested edges (existing example tests).
28. Reference array bounded concurrent marking slices (focused concurrent test).
29. Shrink between concurrent slices preserves deleted references (focused test).
30. Optional reference elements survive minor collection (focused niche test).
31. Site growth 1/8/32/128 stays bounded (native code/count script).
32. Shared ordinary-object header/start-map ABI tests (10 focused codegen tests).
33. Dynamic negative/overflow lengths retain recoverable panics (focused runtime test).

## Validation

Passed on Linux x86_64:

- `cargo fmt --all -- --check`.
- `cargo clippy --workspace --all-targets -- -D warnings`.
- Cross-target Clippy with the same flags for x86_64-unknown-linux-gnu,
  x86_64-pc-windows-msvc, aarch64-apple-darwin, x86_64-apple-darwin.
- `cargo test -p willow --test integration array_literal_ -- --nocapture`:
  existing 9 cases passed before adding the size-boundary test.
- `cargo test -p willow --test integration array_literal_tlab_size_boundary_preserves_elements -- --nocapture`:
  1 passed, six lengths in debug/release.
- `cargo test -p willow_runtime --lib array_literal -- --nocapture`: existing 4 passed.
- `cargo test -p willow_runtime --lib array_literal_inline_trace_is_ready_after_every_gc_init -- --nocapture`:
  1 passed.
- `cargo test -p willow --lib backend::cranelift::gc_codegen::tests:: -- --nocapture`:
  10 passed.
- Focused runtime tests `native_reference_arrays_use_bounded_continuations_in_the_real_engine`
  and `array_shrink_between_slices_preserves_unscanned_deleted_references`: 1 each passed.
- Runtime `array_dynamic_lengths_preserve_recoverable_panics`: 1 passed.
- Integration `option_niche_18_option_values_survive_array_storage_and_minor_gc`: 1 passed.
- `python3 docs/audits/willow-8hq4.33/verify_codegen.py`: all assertions passed.
- `python3 docs/audits/willow-8hq4.33/measure.py after`: nine samples and checksum passed.

Runnable examples: `example/array_literal_allocation.wi` (debug/release and three
GC stress modes, via the focused test) and source-only release benchmark
`docs/audits/willow-8hq4.33/probe.wi`. Existing examples are reused rather than
adding a duplicate program.

Native link/run was Linux x86_64 only. Windows x86_64, macOS arm64 and macOS
x86_64 native execution is unverified; their Clippy checks passed. No CI was
queried. Full local suites and full example audit were intentionally omitted.
Ticket closed after review (2026-10-21); changes remain uncommitted for user review.
