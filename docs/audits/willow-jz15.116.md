# willow-jz15.116 implementation audit

The lambda checker now installs the callable's expected return type before either
expression or block checking, and moves the outer return-inference stack aside.
Both values are restored after the body. A lambda without a contextual or explicit
return type starts with `void`, so `?` cannot borrow an unrelated enclosing return
type. This does not infer an unannotated lambda return signature from `?`.

Existing contextual constructor checking already supplies missing Ok/Err type
arguments for annotated bindings and call arguments. This change preserves that
path; it fixes expression-body propagation and isolates nested inferred returns.
The original block-body and constructor examples are retained as regression cases.

## Implementation and affected paths

Reviewed check_lambda_expecting / check_lambda_for_callable, body-query dispatch,
check_lambda_with_context_inner, check_expr_expecting, check_try_propagate and
return-statement checking. Explicit annotations still take precedence. Return
payload checking inside a typed nested lambda no longer bypasses validation just
because its parent is inferring returns. The saved stack is moved, not copied.
No early return intervenes between saving and restoring these local fields.
No runtime, ABI, backend, platform conditional, or build scheduling code changed.
Execution tests exercise the existing lowering and native code paths for closure
and plain function values, captures, successful results, and error early returns.

## Test perspectives

The focused integration module has 24 explicit cases:

1. Annotated closure binding, Ok expression.
2. Annotated closure binding, Err expression.
3. Annotated closure binding, expression-body `?`.
4. Annotated closure binding, block-body `?`.
5. Closure parameter context, Ok expression.
6. Closure parameter context, Err expression.
7. Closure parameter context, expression-body `?`.
8. Closure parameter context, block-body `?`.
9. Plain fn binding, Ok expression.
10. Plain fn binding, Err expression.
11. Plain fn binding, expression-body `?`.
12. Plain fn binding, block-body `?`.
13. Explicit lambda return annotation.
14. Runtime error early return.
15. Qualified Result constructors.
16. Typed inner lambda within an inferred outer lambda.
17. Distinct inner/outer error types and restoration.
18. Captured environment value with propagation.
19. Option propagation at the same callable boundary.
20. Reject incompatible propagated error types.
21. Reject an incompatible Ok payload.
22. Reject borrowing an outer Result return type in an untyped lambda.
23. Reject an incompatible nested payload within inferred outer returns.
24. Array parameter and user-defined success/error payload types.

## Complexity and reproducibility

Let L be lambda count, D nesting depth, and R the sum of contextual return-type
representation sizes. Installing return context costs O(R) total cloning and O(L)
constant-sized stack moves; live saved state follows nesting depth and type sizes.
The old implementation cloned the enclosing return type even when unnecessary.
No new AST walk, body recheck, symbol scan, or generated instruction is added.
Return checking still visits existing return expressions once. Runtime complexity
and generated-code complexity are unchanged; newly accepted early returns use the
existing Result propagation lowering. This is a bound for the changed work, not
a claim that every existing capture/type analysis is globally linear.

The deterministic unit test `result_lambda_context_records_scale_linearly`
generates sibling and nested callable shapes at L = 4, 16, 64. Each has exactly
L + 2 body computations (6, 18, 66), L lambda records, and zero inline rechecks.
These are asserted operation counts, not wall-clock inferences. The source
construction is in src/semantic/type_checker/body.rs; its test-only repeated
string construction is not compiler work. The unit fixture explicitly registers
the standard prelude, as the standalone checker does not load it automatically.

Reproduce with `bash docs/audits/willow-jz15.116-check.sh`. The runnable source is
example/lambda_result_context.wi; expected stdout is `42`, `true`, `true`, `true`,
`7`, each on its own line. No binaries or caches belong in this report.

## Validation scope

Linux native focused integration tests, deterministic body-count test, runnable
example, formatting, and workspace/all-target Clippy are required here. macOS
arm64, macOS x86_64 and Windows native execution are unverified. Cross-target
Clippy is not required by this compiler-only change (no platform APIs, cfg,
runtime, or filesystem implementation change). Full local suites and the full
example audit were intentionally omitted. CI was not queried.

Final local results (2026-10-21): fmt and Clippy passed. The `lambda_result_`
filter passed 26 cases while the new Array fixture initially failed for a missing
collection import; after adding that import, its exact named rerun passed. Thus
all 24 new perspectives and three existing matching cases passed. The scaling
unit test passed all six shapes; the example produced the expected five lines.
The earlier scaling-fixture failures were missing prelude registration, corrected
before the passing run. No implementation failure remains.
