# Memberwise owner visibility

Date: 2026-10-21. Implementation ready for review; ticket deliberately remains open.

## Cause and change

`implicit_constructor_field_infos` supplied `ClassInfo::name` as the field owner,
while method/constructor receiver scopes and ordinary field lookup use the lookup
key. In a module the former can be `m::Secret` while the latter is `Secret`.
The pre-fix regression failed with E0501 in the module/static/non-generic case,
including the misleading suggestion to provide the factory that already existed.

Preserve the lookup key when collecting memberwise fields, matching
`lookup_field_in_hierarchy`. This changes only the owner metadata consumed by
visibility diagnostics. Field order, field types, specialization, explicit init
selection, and emitted construction remain unchanged. No suffix matching or
module-wide private permission is introduced.

## Explicit test perspectives

The unit tests in `subclass_constructor_tests.rs` cover these independent cases:

| Cases | Context | Expected |
| --- | --- | --- |
| 1–4 | Single file: non-generic/static, non-generic/instance, generic/static, generic/instance | Own private construction accepted, no diagnostic or help |
| 5–8 | Module: the same four combinations | Accepted, no diagnostic or help |
| 9–12 | Single file: non-generic/free caller, non-generic/other class, generic/free caller, generic/other class | Exactly E0501 and actionable factory help |
| 13–16 | Module: the same four outside callers | Exactly E0501 |
| 17–20 | Single file and module: own public field, own protected field | Accepted |
| 21–26 | Single file and module: inherited private, protected, public field | Private rejected; protected/public accepted |
| 27–30 | Single file and module: missing argument, wrong argument type inside owner | Only E0845 or E0201; no spurious privacy diagnostic |

Six integration tests additionally execute the runnable multi-file example,
single-file equivalent, aliased-module calls, and manifest-based project
check/build/run; check external construction via direct imports and aliases for
both generic and non-generic classes; and reject a different class with the same
short name. Executed generic specializations include i64 and String, with static
factory and instance copy methods. Example output: `1`, `2`, `kept`.

Generic construction uses the supported explicit `new Gen<T>(x)` syntax.
An exploratory omitted-type-argument probe (`new Gen(x)`) gets E0201 in both
single-file and module mode under the existing generic arity rule; generic
argument inference is not changed by this privacy fix.

## Implementation audit

Reviewed both callers: `check_new` and `check_super_init`. Both consume the same
field list and visibility check. The latter retains ancestor ownership, so a
subclass cannot initialize inherited private state. Reviewed receiver ownership
in `check_method_body` / `check_constructor_body`, normal hierarchy field lookup,
and private/protected predicates. The cross-module same-name negative test guards
against accidentally granting ownership by short name.

Let C be constructor occurrences, D ancestor depth, F visited fields, and L name
length. Existing collection is expected O(C*(D+F)) table operations, plus string
hashing/copying costs; its temporary storage is O(D+F) entries. Private visibility
adds one string equality per private field (O(L) worst case). Protected visibility
retains its existing ancestor walk. No caller/pass/branch loop, table lookup,
allocation or ancestor traversal is added. Moving the already owned lookup key
instead of cloning the canonical name removes one string clone per visited class.
No new analysis cache or code-generation path is needed for this metadata fix.
This is a statement about the change, not a claim that all pre-existing hierarchy
analysis is globally optimal.

Deterministic scaling test `memberwise_owner_repeated_calls_field_walk_counts`
uses F=1,8,32 and C=1,8,32 in both single-file and module modes (18 combinations).
Measured class-chain visits are exactly C in every combination: 1, 8, 32 for each
field count. The source-level field loop therefore still processes C*F entries;
that field-entry count is inferred from the loop, not a separate counter.
The existing focused constructor tests exercise ancestor depths 1,8,32 and call
counts 1,8,32 with exact C*(D+1) visits, plus deep-chain/fan-out diagnostic counts.
Neither timings nor binary size are used to infer complexity.

Code generation and runtime are unchanged: accepted construction still lowers
through the existing constructor path; executable tests validate field values
and String lifetime through factory/copy/get calls. The metadata change introduces
zero generated instructions or runtime allocations (source inspection inference).

## Validation

Passed on x86_64 Linux:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --lib memberwise_owner -- --nocapture`: 4 tests, including the matrices and counts.
- `cargo test --lib subclass_constructor_ -- --nocapture`: 13 focused tests (includes the four above).
- `cargo test --test integration memberwise_owner -- --nocapture`: 6 tests.
- `cargo test --test integration implicit_memberwise -- --nocapture`: 4 existing tests.
- `cargo test --test integration test_new_ctor_21_implicit_inherited_memberwise_constructor`: 1 existing test.
- `git diff --check`

An initial exact-name invocation selected zero tests; the corrected filtered
invocation above executed and passed the inherited memberwise test.

No platform cfg, native API, runtime, filesystem, thread or socket implementation
changed, so foreign-target Clippy is not applicable to this compiler-only change.
macOS arm64, macOS x86_64 and Windows native execution are unverified. No CI was
queried. Full workspace tests and full example audit were intentionally not run.

Reproduce with `bash docs/audits/memberwise-owner-checks.sh`; test source generators
and exact assertions are checked in with the unit/integration tests. No private
specification contents are used in this report or its fixtures.
