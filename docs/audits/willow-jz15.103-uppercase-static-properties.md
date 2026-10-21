# willow-jz15.103: uppercase static property resolution

## Implementation audit (2026-10-21)

Implemented the readable-property alternative in the original acceptance criteria.
Bare uppercase members on known classes use the existing static-property resolver
and lower to `HirExprKind::StaticField`. Explicit parentheses still select methods;
enum variants and module constants keep their existing interpretation. No declaration
restriction or acceptance-criteria change was made.

The parser already uses `parse_control_head` for for/if/while/match. No expression
parser change was necessary. Its assignment path now also accepts a bare uppercase
member as a static-property target; mutability and visibility remain checker-owned.

Audited paths:

- Checker resolution reuses private/protected checks, member-use recording and
  static-initializer dependency tracking. Classification checks the receiver class,
  not its property hierarchy, avoiding a second hierarchy scan.
- Call/effect recording excludes these value reads, including module constants;
  they must not produce fabricated method-call edges. Forward initialization and
  initialization-cycle tests exercise the resulting dependency graph.
- HIR lowering recognizes both local class tables and checker symbols for imported
  classes. The latter matters because the local HIR class map alone omits imported
  module classes. The existing field path preserves checker-recorded types and
  canonical receiver names. Runtime and ABI code are unchanged.
- Explicit `C::VALUE()` remains a call even when `C::VALUE` is a property. Tests
  exercise both a same-named real method and a missing method.
- Assignment transfers existing name strings to the existing assignment node;
  explicit calls are not assignable. Reads and writes share existing storage.

## Validation

Passed on x86_64 Linux:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --test integration uppercase_static -- --nocapture`: 7 tests,
  covering more than 26 explicit perspectives listed in the test source.
- Eight neighboring named tests: `test_static_prop_01_i64`,
  `test_static_prop_05_read_in_static_method`,
  `test_static_prop_14_forward_reference_ordered`,
  `test_static_prop_17_assign_to_immutable_static_rejected`,
  `test_static_s4_17_protected_static_accessible_in_subclass`,
  `dependency_09_inherited_static`, `dependency_16_cross_class_cycle`,
  `c30_frozen_static_array_replacement_runs`.
- `git diff --check`.

`example/uppercase_static_properties.wi` runs in debug and release under
`WILLOW_GC_STRESS=1` via the focused test. Expected output:

```text
tables
10
6
10
16
once
sixteen
```

The focused tests also execute module-qualified reads/indexes/iteration, module
aliases, item imports, inherited and private internal reads, module-qualified
if/while and mutation, enum matching, and module constants. Negative tests verify
private/protected access, immutable assignment, missing members and explicit calls.

No full local suite or full example audit was run. CI was not inspected. macOS,
Windows and other architectures were not executed. Cross-target Clippy was not
required for this compiler-only change: no cfg, platform API, runtime, filesystem,
thread or socket implementation was changed. Foreign native execution is not a
closure prerequisite under the current user policy.

## Complexity and reproducible evidence

Let R be member expressions, D inheritance depth, and L qualified-name length.
New classification uses a bounded number of symbol-map lookups per expression,
with name hashing/resolution proportional to L (expected hash-map cost). There is
no scan over all classes, all properties, all expressions or callers. Storage added
per expression is constant apart from transient names of length L; no new cache,
per-module matrix or runtime allocation was added.

Existing hierarchy lookup remains O(D) for an inherited property (plus name/type
copy costs); the new classifier does not repeat that walk. Thus this change adds
O(R * L) classification work, while inherited resolution retains its existing
O(R * D) worst-case shape. This is a statement about the changed paths, not a claim
that the entire compiler has optimal complexity. Existing class-table construction
and inheritance copying are outside this change. Runtime reads remain one static
storage load and code size remains linear in source reads.

The deterministic generated-HIR test measured these counts, not timings:

| Source reads | Emitted property reads |
| ---: | ---: |
| 1 | 1 |
| 8 | 8 |
| 32 | 32 |
| 128 | 128 |

Reproduce with:

```sh
rtk cargo test --test integration uppercase_static_linear_hir_reads -- --nocapture
```

Its reproducible source family is:

```python
for count in [1, 8, 32, 128]:
    source = ('class C { pub static VALUE: i64 = 7; } fn main() { '
              + 'println(C::VALUE);' * count + ' }')
    # Build with `willow build <source.wi> --emit-hir`.
    # Assert stdout.count('println(C::VALUE: i64)') == count.
```

Repeated equivalent accesses therefore produce exactly one property expression
each, rather than duplicated initialization or call expressions. Inheritance,
module fan-out through aliases, control-flow heads and forward/cyclic initializer
edges are covered by the functional tests. Deep-chain timing and foreign execution
were not measured; the inheritance complexity above is inferred from the existing
single-chain resolver, not from wall-clock timings.
