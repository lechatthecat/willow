# Linear string assembly audit

Date: 2026-10-26

## Implementation and affected paths

`Array<String>.join(separator)` resolves through the existing array method checker
and intrinsic catalog to one `willow_array_join` ABI call. The ABI declares panic
and allocation effects, so existing root placement and panic propagation apply.
Other element types, incorrect argument counts, non-string separators and explicit
reference arguments are rejected. No new language syntax or builder lifecycle is
needed. Frozen arrays are outside this API's scope.

The runtime first sums lengths using checked multiplication/addition, including
separator bytes only between elements, and validates header/terminator capacity
against the signed allocator size domain. It roots the array and separator across
the single output allocation. The array's reference buffer traces its elements;
the second pass reloads elements from the rooted array after possible collection.
No borrowed managed bytes survive allocation. Element access and copying do not
allocate or safepoint. The runtime FFI guard covers the operation. Length and
terminator are initialized, empty copies are skipped, and UTF-8 remains valid
because only complete existing strings are copied. Embedded NUL is content.
Inputs remain logically unchanged and the result owns separate storage.

Audited callees: `willow_array_len`, `willow_array_get`, `expose_pair_storage`,
`GcRootHandle`, `willow_alloc_with_layout`, `ws_as_bytes`, `string_payload_size`,
`copy_bytes`, and the standard intrinsic lowering/runtime-effect path.
`willow_array_get` conservatively marks reference slots exposed, a constant-time
operation that has no pair-box transfer consequence for String elements.

## Complexity and evidence

Independent dimensions are element count n, total element bytes S, separator
length d, and source call-site count k. Output length B is S + max(n-1,0)*d.
Runtime work is two element passes plus B copied bytes: O(n+B). Output memory is
O(B), auxiliary memory O(1) (two root handles); GC tracing/collection costs are
excluded and depend on the live heap. Reading n lengths and writing B result
bytes is the required lower bound for this independent-output implementation.
There are no nested scans, repeated prefix copies, per-element output allocations,
or temporary element vectors. Repeated calls incur their own required output
work; callers should accumulate pieces and join once.

The runtime test instruments actual element accesses and copy operations. The
checked-in measurements cover n=0,1,8,64,512,4096,20000 with empty, six-byte
Unicode/NUL, and 23-byte fields, separately allocated to exercise fragmented
storage. Every case records exactly 2n accesses and B copied bytes. For six-byte
fields and a five-byte separator: n=512 gives 1024 accesses/5627 copied bytes;
n=4096 gives 8192/45051; n=20000 gives 40000/219995. These are deterministic
operation measurements, not timing-based asymptotic estimates.

Generated relocation counts for k=1,8,32 and literal sizes 1,1024 confirm exactly
k join calls. Compiler dispatch adds O(1) work and code per call site; input
literal storage still scales with source bytes. No compiler loop is unrolled
according to the runtime number of elements.

## Validation

- Four integration tests filtered by `array_join`: passed. Twenty explicit
  semantic expressions cover empty arrays/separators/elements, singletons,
  separator placement, order, Unicode scalars/combining marks, long separators,
  literal separator content and split/join round trips. Additional perspectives
  cover invalid types/arities/reference modes, dynamic function arguments,
  async suspension, explicit GC, result independence after mutation, a 20,000
  element runnable example, debug/release/GC-stress execution, and codegen counts.
- `array_join_measured_linear_work`: passed (21 measured input shapes), also
  asserting embedded NUL preservation and the trailing terminator.
- `semantic::intrinsics`: 54 passed after updating the explicit catalog count
  and adding join to checker-agreement coverage.
- `backend::abi::tests`: 14 passed.
- Formatting and host workspace/all-target Clippy passed.
- `backend::abi::alloc_effects_tests`: 23 passed, including fresh-object and
  allocation-column consistency checks.
- Workspace/all-target Clippy with warnings denied passed for all four targets:
  `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
  `aarch64-apple-darwin`, `x86_64-apple-darwin`.

Run `bash docs/audits/array-join/verify.sh` for the focused reproducer. Test source
is in `tests/integration/parser/string_methods.rs` and
`crates/willow_runtime/src/string/methods.rs`; the runnable source is
`example/array_join.wi`. Raw operation counts are in `measurements.txt`.

Native execution was performed on Linux x86_64 only. Windows x86_64 and macOS
arm64/x86_64 native execution remain unverified. No CI was inspected and no full
local test suite or full example audit was run. Exhaustion-size allocation was
not attempted; overflow prevention was audited at each arithmetic boundary.
