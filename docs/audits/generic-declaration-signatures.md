# Generic declaration signature audit

Date: 2026-10-26

## Change and boundary

Direct `symbol` and `type` queries now display declaration binders, for example
`fn<T>(T, i64, closure(T) -> T) -> T`, `fn<A, B>() -> A`, and `main::Pair<A, B>`.
The existing source declaration metadata supplies function/class parameters.
Class methods share their owner's parameters through an `Arc<[String]>`.
Both symbol serialization and declared-type lookup use one presentation helper.
Exact `Type` wire trees, identities, type checking, monomorphization, and code
generation are unchanged. Receiver-instantiated call types and ordinary callable
values still use the existing type renderer, without invented declaration binders.

Current language boundary verified in `src/parser/decl.rs` and `src/parser/ast.rs`:
function/class type parameters are identifier lists, with no explicit bound AST;
methods accept `(` immediately after the method name and have no independent type
parameter list. Thus there are no declared bounds to display. Generic methods
here mean methods of generic classes. This change does not introduce new syntax
or turn inferred body requirements into declared bounds. Existing live metadata
is not persisted in snapshots; no persisted schema change is introduced.

## Verification perspectives

`generic_declaration_signatures` checks all four columns independently for each
row (32 explicit assertions, plus successful command status):

| Declaration | symbol JSON | type JSON | symbol human | type human |
| --- | --- | --- | --- | --- |
| One-parameter function | exact signature | exact signature | signature present | signature present |
| Two-parameter function | exact signature | exact signature | signature present | signature present |
| Generic closure argument | exact nested type | exact nested type | signature present | signature present |
| One-parameter class | canonical name + binder | canonical name + binder | signature present | signature present |
| Two-parameter class | ordered binders | ordered binders | signature present | signature present |
| One-parameter class method | owner binder | owner binder | signature present | signature present |
| Two-parameter class method | ordered owner binders | ordered owner binders | signature present | signature present |
| Nongeneric function | unchanged | unchanged | unchanged | unchanged |

Additional focused regressions cover imported declarations, aliases after rename,
uninstantiated bodies, source positions, closure calls, concrete receiver types,
canonical names, nested tuples, deep type rendering, exact wire-tree preservation,
and sync methods retaining their async flag as false.

## Complexity and implementation audit

Independent dimensions: parameter-name bytes P, member count M, type nodes N,
rendered output bytes L, and number of queries Q. Metadata capture adds O(P + M)
time/storage per generic class. Each method owns one shared pointer rather than
copying P bytes; original class metadata retains its existing own binder vector.
No source/project scan is added to query rendering. Rendering visits each type
node once, joins binder names once, and inserts the binder with one linear string
shift: O(N + P + L) time and O(N + P + L) temporary/output space, including the
existing iterative work stack. Q queries cost the sum of their output costs;
printing every method necessarily emits M copies of the owner binder. There are
no new compilation passes, generated functions, runtime allocations in compiled
Willow programs, or changes to generated-code size/runtime complexity.

Deterministic measurements (asserted tests, not wall-clock inference):

| Parameters / type arguments | Type visits |
| --- | --- |
| 1 | 3 |
| 2 | 4 |
| 16 | 18 |
| 128 | 130 |

For classes with (parameters, members) = (1,1), (8,8), (64,64), the metadata test
asserts exactly M method records and pointer equality of every method binder.
Each shared binder contains exactly P parameters, and method-local vectors are
empty. This adversarial fan-out proves the added storage does not multiply P*M.
Existing deep/wide renderer tests also pass. The linear output-writing lower
bound is unavoidable; no claim is made about unrelated pre-existing compiler
passes or full-project performance.

## Reproduction and results

Run from the repository root (all passed on x86_64 Linux):

```sh
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo test -p willow --test semantic_cli generic_declaration_signatures -- --exact
rtk cargo test -p willow --test semantic_cli generic_source_queries -- --exact
rtk cargo test -p willow --test semantic_cli generic_closure_value_calls_keep_queries_available -- --exact
rtk cargo test -p willow --test semantic_cli callee_type_cli_reports_callable_types -- --exact
rtk cargo test -p willow --test semantic_cli uninstantiated_generic_body_refs_and_rename -- --exact
rtk cargo test -p willow --lib ai::display::tests
rtk cargo test -p willow --lib ai::symbols::tests::generic_method_binders_share_owner_storage -- --exact
rtk proxy target/debug/willow run example/generic_signature_queries.wi
```

Runnable example output: `7`, `8`, `9`, `10`, `11`, `4` on separate lines.
The source fixture is checked in under `example/` and reused by the CLI test.
Initial affected tests exposed old expectations for omitted binders; those exact
expectations were updated, without relaxing assertions to accept both forms.

Full local suites and the full example audit were intentionally omitted. CI was
not queried. macOS arm64, macOS x86_64, and Windows were not executed or cross-linted:
this change does not touch runtime, platform cfgs, OS APIs, or filesystem behavior.
Foreign native execution is not a completion prerequisite. Validation is Linux
only; no assertion of foreign-platform execution success is made.
