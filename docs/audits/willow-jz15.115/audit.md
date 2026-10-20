# willow-jz15.115 implementation audit

Expression-bodied lambdas returning contextual Result constructors now receive
an expected return type during body checking. The change is confined to
`check_lambda_with_context_inner`; it reuses `check_expr_expecting` rather than
repairing types in lowering or adding another compiler pass.

## Evidence and affected paths

Before the fix, `willow check example/lambda_result_expression.wi` succeeded,
while `willow run` failed with E0800: function `$lambda.0` has invalid lowered IR,
a return has an incompatible operand or signature. After the fix, check succeeds
and run prints `2` then `failed` with successful exit.

The lambda's explicit return annotation takes precedence over contextual return
type as before. Expected types now reach constructor payload checking and the
expression-type table before lowering. Unannotated expressions retain ordinary
inference. Block bodies and subsequent return compatibility checks are unchanged.
`check_expr_expecting` records the resulting type and existing variant helpers
record resolution and validate payloads. Ternary and match branches use their
existing contextual paths. Cranelift `compile_lambda` synthesizes a return from
the expression using this checked callable signature; inspecting emitted LIR
confirms Result<i64, String> on both allocations and lambda return signatures.
No runtime, ABI, platform conditional, scheduling, or cache changes were made.

## Explicit regression perspectives

The matrix checks, builds and executes all 16 combinations of:

- `fn` and `closure` callable annotations;
- explicit and inferred parameter types;
- unqualified Ok, unqualified Err, qualified Result::Ok, qualified Result::Err.

Six further check/build/run cases cover captured values, explicit lambda return
annotations, ternary results, match results, reassignment, and the existing
block-body return path. Four negative cases use each constructor spelling with
the wrong payload type: check and build must both reject with E0201, never E0800.
These are 26 explicit perspectives. Existing lambda-inference tests additionally
exercise Option combinators, Result and_then, function arguments, uncontextual
inference and explicit block return annotations.

## Complexity and generated-code measurement

Let L be the number of lambdas, N the total expression nodes in their bodies,
B the number of result branches per lambda, and T the structural size of the
expected type. The fix adds one expected-type dispatch per expression lambda;
it replaces the ordinary body check, without checking the body twice. Existing
contextual recursion visits children once and may clone/compare type structures,
so the added contextual work is O(N*T) in the worst case, O(N) for fixed-size
Result<i64, String>. Existing symbol lookup and capture analysis costs remain;
this is not a claim that the entire compiler is linear. Persistent expression
type storage remains O(N*T); traversal stack follows expression/type depth.
No all-lambda scan, cross-caller pass, or runtime allocation is added by the fix.

`rtk proxy python3 docs/audits/willow-jz15.115/count_lir.py` generates independent
lambdas and deep ternary chains, then asserts deterministic LIR counts:

| Lambdas | Branches each | Lambda definitions | Enum allocations | Value returns |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 1 | 1 | 1 | 1 |
| 8 | 1 | 8 | 8 | 8 |
| 32 | 1 | 32 | 32 | 32 |
| 1 | 8 | 1 | 8 | 1 |
| 1 | 32 | 1 | 32 | 1 |
| 8 | 8 | 8 | 64 | 8 |

These measured counts show O(L*B) constructor sites for L*B source branches,
with one function and one shared return per lambda, not repeated body emission
per branch. At runtime only the chosen constructor branch executes. Full
compiler time and memory bounds above are source-based analysis, not timing
inferences; the script does not instrument checker operations or peak memory.
Distinct source branches must be inspected for correctness; the change adds no
avoidable repeated scans or code duplication. Existing contextual machinery is
the narrowest responsibility boundary and avoids a separate normalization pass.

## Validation

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- Focused integration filter `codegen::lambda_inference::`: 13 tests passed, 5962 filtered out
  (includes the 26 new perspectives).
- Runnable example check/run: passed, stdout `2\nfailed\n`.
- Deterministic LIR-count script: all six sizes/shapes passed.

The first negative test incorrectly expected E0200; the compiler correctly
reported E0201 for a constructor argument mismatch. The assertion was corrected.
Validation is native x86_64 Linux only. macOS arm64/x86_64 and Windows native
execution are unverified. Cross-target Clippy is not applicable to this pure
semantic-checker change (no runtime, OS API, cfg, filesystem, stack, thread, or
socket behavior changes). Full test suites and the full example audit were
intentionally omitted under local test policy. CI was not queried.

Ticket remains in progress for review; no closure, staging, commit or push.
