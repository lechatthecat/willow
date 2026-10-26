# Generic variant context implementation audit

Date: 2026-10-21. Implementation prepared for review; ticket intentionally remains in progress. No staging, commit, push, Beads mirror export, or CI access.

## Behavior and acceptance

Generic function arguments retain their enum head as contextual information while payload type parameters are unresolved. `f(Some(4))`, `f(None, 4)`, and `f(Ok(4))` now infer payload types from constructors or other arguments. Qualified variants continue to work. Constructor expression metadata, including nested `None` and ternary/match arms, is completed after inference without checking payload expressions again.

Unresolved bare `Some`/`None` and unannotated standalone `Ok`/`Err` bindings explain that an expected Option/Result type is required, with annotation and qualified-constructor suggestions. Existing sibling-branch Result inference remains available. Generic structural mismatches identify the positional argument, parameter name, and expected/found types, retaining the specific underlying mismatch explanation.

Current source deliberately rejects bare generic type annotations such as `Option`; this change preserves that existing arity rule. Correct annotations use `Option<T>` or `Result<T, E>`. The already-working annotated closure return context is covered by regression tests; no lambda implementation change was needed.

## Implementation review

Reviewed generic call checking, contextual variant construction, incremental structural constraints, expression type recording, deferred lambda handling, source-symbol recording, and the compile/link/run path. Enum inference is compiler-owned; runtime ABI, runtime auto-build, and build scheduling are unchanged.

Partial constraints recognize a compiler-only, source-unspellable enum-hole marker. Concrete `Type::Void` is always a real constraint, including inside nested values and callable return types. Only absent constructor constraints produce the marker; ordinary expressions retain their concrete types. Completed generic bindings reject residual markers, and diagnostics render unresolved slots as `_`. Whole formal subtrees are skipped with a single forward signature traversal. Fixed and explicit arguments, lexical shadowing, reference argument paths, and normal unknown-name diagnostics retain their prior paths. Omitted void success payloads remain supported.

Selected payload templates are substituted without instantiating all variants. Finalization visits nested constructors and value-producing branch edges once and reuses checked payload types. Partial ternary and match merging uses a strict structural merge of marked holes; it cannot fall back to the older void-placeholder compatibility path. This fixes the shallow enum-compatibility predicate's inability to complete holes beneath two enum heads; a dedicated test examines the final expression type table.

## Test perspectives

The focused module covers: integer, string and boolean payloads; None before and after the binding argument; consistent repeated constraints; Ok payload inference; Err and Ok with two independently inferred parameters; explicit type arguments; qualified Some, None and Ok; nested Some, nested None and outer None; annotated closure return; qualified context-free Some; concrete annotation context; all four missing-context diagnostics and their help; argument mismatch details; unresolved None rejection; unrelated unknown names; lexical function shadowing; repeated-call scaling; mismatched Result payload constraints; missing payload arity; deep nesting; concrete nested expression metadata; and omitted void payloads.

Structural inference adds focused cases for skipping a whole nested formal subtree, preserving a mismatch in its sibling, rejecting void value arguments, and preserving callable return mismatches. Existing generic lambda, contextual value and type-arity tests guard affected paths.

The runnable source is `example/generic_variant_context.wi`. Its integration test checks exact output `7, 9, hello, true, 11, 13, 15, 16, 18, 19, true` (one item per line). A second integration test calls an imported generic function with Some and None and checks `21, 22`. A third integration test rejects the concrete-void nested-payload reproducer.

## Review repairs

Both review findings were reproduced by failing tests before the repairs. The initial review regression run accepted the invalid concrete-void call and rejected contextual branch shorthands.

P1: `Some(r)` with `r: Result<void, String>` no longer erases the incompatible concrete `void` constraint when a formal expects `Option<Result<i64, T>>`. Inference holes now have separate identity from `Void`. Negative tests cover direct, qualified, ternary and match payloads and concrete-void/i64 sibling branches. Positive tests preserve omitted `Ok()` for a known `Result<void, E>` context and complementary Ok/Err branches.

P2: partial enum context is routed through the existing ternary and match checking paths. Branches contribute marked constraints; arguments checked later can resolve holes left by all-None branches. The final metadata walk reaches branch wrappers, arms and nested constructors. Tests cover both requested expressions, all-None branches, nesting, complementary Result variants, unresolved-call diagnostics, and direct inspection that successful expression metadata has no residual markers.

The structural merge and marker scan use explicit worklists. Equal repeated bindings reuse the existing binding without allocation. No payload is rechecked and no argument-wide inference retry loop was introduced. The legacy placeholder representation outside partial generic enum argument checking is unchanged; this is not a whole-compiler type representation migration.

## Complexity and measurements

Independent dimensions are call count C, argument count A, constructor nesting D, enum variant count V, and aggregate stored/visited type-node size S. Payload expressions are checked once; inference and metadata completion add bounded passes, not a fixed-point retry across arguments. The Option/Result variant count is constant. For arbitrary user enums the existing vector lookup costs O(V) per constructor; unrelated variant payloads are no longer cloned or substituted.

The constraint matcher is O(S) plus repeated-binding equality comparisons on the corresponding actual subtrees. Context checking and completion are linear in the aggregate type representations they consume and store. On a depth-D chain, the owned expression type table itself stores all suffix types and therefore contains Theta(D^2) type nodes. The implementation does not claim O(D) total type-copy time or memory; avoiding that representation cost requires a separate interned-type representation change. Traversal count is linear in D, and no runtime scan or generated dispatch is added.

Deterministic tests measure the new contextual argument entry count:

| Calls C | Visits |
| ---: | ---: |
| 8 | 24 |
| 16 | 48 |
| 32 | 96 |
| 64 | 192 |

| Constructor depth D | Visits |
| ---: | ---: |
| 4 | 6 |
| 8 | 10 |
| 16 | 18 |
| 32 | 34 |

The repair adds a match fan-out measurement with N concrete Some arms and one None fallback:

| Concrete arms N | Context visits |
| ---: | ---: |
| 8 | 19 |
| 16 | 35 |
| 32 | 67 |
| 64 | 131 |

This is exactly 2N + 3: the match argument, each Some and payload, the None arm, and the ordinary fallback argument. Branch merging is linear in the aggregate arm type sizes, and finalization is a single traversal of value-producing edges. Each table is asserted by generated-source tests, not inferred from timing. Generated-code size and runtime complexity are inferred unchanged from reuse of existing enum lowering and generic specialization; runtime output was measured, but no machine-code-size or wall-time benchmark is claimed.

Reproduce deterministic inputs and counts using:

```sh
rtk proxy cargo test -p willow --lib generic_variant_tests::p25_repeated_context_visits_scale_linearly -- --nocapture
rtk proxy cargo test -p willow --lib generic_variant_tests::p28_deep_constructor_visits_match_depth -- --nocapture
rtk proxy cargo test -p willow --lib generic_variant_tests::review_match_fanout_is_linear -- --nocapture
```

The input generators are kept in `src/semantic/type_checker/generic_variant_tests.rs`; the runnable input is the example above. No generated binary or cache is included here.

## Validation

Focused checks: `cargo test -p willow --lib` with the individual filters `generic_variant_tests`, `generic_inference::tests`, `generic_lambda_tests`, `contextual_values_tests`, and `type_arity_tests`; integration filter `generic_variant_context_`. Results after review repairs: 38 generic-variant tests, 38 structural-inference tests, 26 generic-lambda tests, 5 contextual-value tests, 58 type-arity tests, and 3 integration tests passed (168 focused tests total). Formatting and workspace/all-targets Clippy passed. Cross-target Clippy passed for x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc, aarch64-apple-darwin, and x86_64-apple-darwin.

Native execution is Linux x86_64 only. Windows x86_64, macOS arm64 and macOS x86_64 native linking/execution are unverified. Cross-target Clippy is compile-time evidence, not native execution. Full local suites, the full example audit and CI polling are intentionally omitted under the session policy.
