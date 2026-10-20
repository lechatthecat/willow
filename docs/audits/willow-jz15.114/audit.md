# Imported Result error identity audit

Ticket: willow-jz15.114. Validation: 2026-10-21, Linux x86_64.

## Cause and fix

`check_try_propagate` compared source-spelled types with Rust equality.
An imported class `E1` and the provider signature's `a::E1` therefore
produced E1805 despite identifying the same declaration. Reuse the existing
`same_type_identity` structural comparator. Keep unresolved Void handling
and the existing Into conversion fallback. Do not use subtype compatibility:
a Child error must not silently become Base without the required conversion.

Before the production edit, the new matrix failed on class/brace/entry/no
header with E1805. Afterward all cases pass. No pre-existing changes were
present at the start of this session.

## Explicit perspectives

The matrix in `tests/integration/imported_error_identity.rs` executes these
12 arrangements separately for class and enum (24 perspectives total).
Every arrangement verifies Ok(3) becomes 4 and Err preserves the string `neg`.

| Import | Entry header | Caller |
| --- | --- | --- |
| brace | absent | entry |
| brace | absent | helper module |
| brace | present | entry |
| brace | present | helper module |
| single | absent | entry |
| single | absent | helper module |
| single | present | entry |
| single | present | helper module |
| renamed single | absent | entry |
| renamed single | absent | helper module |
| renamed single | present | entry |
| renamed single | present | helper module |

Additional tests reject separate same-named class and enum declarations with
E1805, reject Child-to-Base propagation, and run the shipped two-file example
at `example/imported_error_identity/main.wi` (output `3`, then `neg`).
Existing focused tests cover Into conversion, wrong/no Into target, chained
conversion, GC-managed payload conversion, Option and primitive mismatches.

## Implementation and complexity audit

Reviewed `check_try_propagate`, `same_type_identity`, `err_converts_via_into`,
HIR `LowerCtx::normalize_declared`, and LIR `lower_try_propagate`.
HIR already resolves class declaration aliases before LIR compares the source
and target error types. Thus the accepted alias case does not introduce a
spurious Into conversion. The native matrix verifies both emitted branches.
No runtime/ABI, scheduling, build caching or compiler auto-build change.

For Q propagation sites and S structural nodes in each compared error type,
the comparator visits each node pair once: O(Q*S) visits, with early mismatch
termination. Hash/name lookup also depends on identifier length and the
existing symbol lookup structure; this change adds no declaration or module
scan. The pending sibling slices use O(D) auxiliary memory for nesting depth D;
wide signatures use constant auxiliary space. No normalized type copies are
allocated. A simple named class error uses one visit. Exact structural equality
has an Omega(S) worst-case lower bound without an existing interned identity.
No new identity cache or broad normalization pass is justified for this fix.
The existing operand/return cloning and Into search remain unchanged.

Deterministic existing test `structural_identity_visits_each_node_once` passed.
It asserts the exact counts below, for N repeated calls over N-deep array types
and N-wide function signatures with aliased class leaves. These are measured
counter assertions, not timing-based estimates:

| N | Deep: N*(N+1) | Wide: N*(N+2) |
| --- | --- | --- |
| 8 | 72 | 80 |
| 32 | 1056 | 1088 |
| 128 | 16512 | 16640 |
| 512 | 262656 | 263168 |

Generated-code size and runtime work are inferred unchanged per propagation
site: the edit is only a frontend acceptance predicate; lowering still emits
one success/failure branch pair and its existing payload path. No generated
instruction count or performance claim is made. No avoidable repeated scans,
new allocation, or duplicated code generation were introduced.

## Validation

Run `bash docs/audits/willow-jz15.114/verify.sh` to reproduce focused checks.

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- Integration filter `imported_error_identity`: 4 tests passed (24 positive
  matrix arrangements, 3 negative cases, and the runnable example).
- Integration filter `try_convert_`: 14 tests passed.
- Integration filter `test_e1805`: 2 tests passed.
- Library filter `structural_identity_visits_each_node_once`: 1 test passed.
- `git diff --check`: passed.

Native execution was Linux x86_64 only. Windows and both macOS architectures
are unexecuted. Cross-target Clippy is not required by the platform-sensitive
change rule: this edit touches only semantic comparison, with no runtime,
cfg, OS API, filesystem, thread or socket implementation changes. Full local
test/example suites were intentionally omitted. CI was neither checked nor
polled. The ticket remains in progress pending independent review; nothing
was staged, committed or pushed.
