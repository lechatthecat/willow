# Literal payload pattern diagnostics audit

Date: 2026-10-26

## Change and boundary

Variant payload literal patterns remain unsupported. The shared payload-binding
parser now rejects boolean, integer, negative numeric, float, string and nil
tokens before identifier parsing. E0102 names literal patterns and recommends a
binding plus match guard, including boolean and integer examples. It no longer
suggests renaming a literal. No AST, semantic, backend or runtime change.

Both qualified variant and unqualified constructor/downcast syntax use this
helper. Match expressions and if/while-let heads use the same pattern parser.
Top-level literals and tuple destructuring are unchanged. Nested constructor
patterns retain their existing separate diagnostic. A negative numeric label
points to the minus token; this is intentional rather than a full-expression span.

## Validation and perspectives

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo test --lib parser::pattern::tests -- --nocapture`: 3 passed.
- `cargo test --test integration literal_payload_ -- --nocapture`: 2 passed.
- The runnable `example/literal_payload_guards.wi` is included by the integration
  test and executed in debug and release modes: output `2`, `0`, `3`, `1`.

The diagnostic matrix contains 72 explicit cases: four constructor spellings
(qualified, unqualified, module-qualified, generic Value spelling), nine literal
forms (true, false, zero, positive integer, negative integer, positive float,
negative float, string, nil), and first/second payload positions. Each asserts
one E0102, literal-specific text, no reserved-keyword or rename advice, binding
and guard help, exact label start, and recovery preserving a following async
function. Existing nested-pattern coverage adds 20 cases. CLI checks exercise
six literal forms. Runtime cases cover boolean true/false and negative integer
match/fallthrough using the recommended guard workaround.

## Complexity and affected paths

The new predicate performs at most two current-token inspections and one indexed
lookahead, independent of literal length, constructor depth and payload arity.
It borrows tokens, never clones string contents, and allocates only fixed-size
error text when rejecting. `peek_kind_at` directly indexes the token vector.
Added cost is O(1) time/space per payload binding, O(B) across B bindings, which
already require inspection. No repeated scan, new recursion or code generation
is introduced. Existing binding storage remains O(B); generated code and runtime
cost are unchanged for valid programs.

The checked-in `literal_payload_diagnostic_linear_token_reads` test generates
16, 64 and 256 bindings before a negative literal. Measured parser token-read
counts are 134, 422 and 1574 (6B + 38 for these samples). It enforces a linear
bound of 20B + 200. This counts `peek_kind` calls, not every CPU operation;
constant indexed lookahead is established by source inspection. The generator
is the reproducible source input. No wall-clock inference is used.

## Limits

Validation ran on Linux. macOS and Windows native execution is unverified.
Cross-target Clippy is not required for this parser-only change: no cfg, platform
API, filesystem, thread or runtime implementation changed. Full tests and the
full runnable-example audit were intentionally omitted; CI was not queried.
Literal pattern support and nil/float/string top-level pattern semantics are
outside this diagnostic fix. No outstanding implementation blocker found.
