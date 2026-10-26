# Monotonic clock implementation audit

Date: 2026-10-26. Scope: the new public `std::time::now_ns() -> i64` API.

## Implementation and call paths

The std schema supplies the signature and import registry. Type checking,
module-alias normalization and HIR namespace resolution recognize `time` using
the existing builtin-module paths. Intrinsic dispatch selects one shared ABI
row, `willow_time_now_ns`, with no parameters and a signed 64-bit result.
The runtime declaration is pinned to that row by a compile-time function-pointer
assignment and a schema/effect assertion. No runtime implementation lives in
the compiler. Existing runtime automatic build and artifact scheduling are intact.

The exported function uses the existing timer LazyLock epoch and Rust Instant.
It converts elapsed Duration to nanoseconds and clamps before narrowing to i64.
No clock state is stored per task or per call, no GC roots or cleanup are needed,
and no scheduler registration is required. FFI panic containment follows the
runtime's existing macro. ABI NONE denotes absence of GC allocation/suspension,
not purity: emitted calls remain observable, including unused reads in release.

Module aliases pass through normalization as well as HIR; an initially missing
normalization registration was found by execution tests and fixed. Existing std
item imports validate item names but do not register bare callable functions.
This API follows that existing behavior and documents qualified calls. Local
user modules named `time` retain precedence, verified by a two-file program.

## Complexity and measurements

Independent dimensions are C static clock expressions, R executed reads, and T
calling threads/tasks. The additional schema/namespace entry has fixed size;
normal compiler traversal does constant additional work per relevant expression
or import. Clock-specific generated code is O(C), with one runtime call per
expression, sharing one implementation and one epoch. Runtime work is O(R)
and retained clock state is O(1), independent of T. Lazy initialization happens
once and may synchronize concurrent first readers; ordinary reads do not add
per-thread storage. OS clock latency and scheduling are not bounded here.

Measured object relocation counts in debug and release for C=1,16,256 were
exactly 1,16,256. This confirms no duplicated calls and no optimizer removal of
unused reads. At R=1,16,256,4096, native allocation deltas were all zero and
consecutive readings never decreased. Measurements are deterministic call and
allocation counts, not timing-based complexity estimates. O(C) emitted calls
and O(R) observations are necessary to preserve each requested observation;
constant shared state avoids per-call or per-task growth. General compiler
complexity and OS clock internals were not benchmarked or claimed optimal.

Reproducible generators and assertions live in
`tests/integration/monotonic_clock.rs` and runtime `timer.rs`; run the adjacent
`reproduce.sh` from the repository root. Raw focused outputs are `integration.txt`
and `runtime.txt`.

## Explicit test perspectives

1. Unqualified builtin module access (`time::now_ns`).
2. Explicit module import.
3. Module alias.
4. Known item import resolves with qualified invocation.
5. Aliased item import resolves with qualified invocation.
6. Return from a synchronous helper.
7. Passing a reading to another function.
8. Consecutive reads do not decrease.
9. Subtraction produces nonnegative elapsed time.
10. Repeated reads within a loop.
11. Reading in a branch expression.
12. Discarded reads remain executable.
13. Signed i64 annotation and value copying.
14. A user function named now_ns remains callable.
15. A reading survives an await and measures the sleep duration.
16. GC stress preserves readings and a live string across await.
17. Debug generated-call counts at increasing source sizes.
18. Release generated-call counts at increasing source sizes.
19. Runnable example output and minimum elapsed interval.
20. Extra arguments rejected without internal error.
21. Incorrect result type rejected without internal error.
22. Unknown imported item rejected without internal error.
23. Unknown namespace function rejected without internal error.
24. User-defined time module wins over builtin namespace.
25. Zero Duration conversion.
26. One-nanosecond conversion.
27. Seconds-to-nanoseconds conversion.
28. Exact i64 maximum remains representable.
29. Maximum plus one saturates.
30. Duration maximum saturates.
31. Cross-thread readings share an epoch and preserve ordered observations.
32. Increasing repeated runtime reads allocate no storage.
33. C ABI signature and nonallocating effects agree.
34. Std schema surface and registry include the new module.

## Validation and limitations

Passed: fmt check; all-target workspace Clippy with warnings denied for
x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc, aarch64-apple-darwin and
x86_64-apple-darwin. Focused Linux runtime tests: 4 passed. Focused Linux
integration tests: 6 passed (including the runnable example and generated-code
counts). Std schema basic tests: 2 passed; module surface test: 1 passed;
registry-all-entries test: 1 passed. Final diff whitespace check passed.

Native execution was Linux x86_64 only. Windows and both macOS architectures
were compile/lint checked, not linked or executed. CI was not queried. Full
workspace/integration suites and the full example audit were intentionally not
run. Nanosecond resolution, suspend behavior, cross-process comparability and
advancing after saturation are not promised; see the public API document.
