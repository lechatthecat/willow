# Function-to-closure argument diagnostics

## Decision and scope

Keep the existing distinction between `fn` values and `closure` values. Changing
coercion would require language and lowering changes; a typed lambda already
provides an explicit, runnable adapter. Failed ordinary value arguments now show
that adapter. Generic constraint failures retain expected/found and parameter
context while their primary label points at the failing argument. Arity errors
still point at the call. No inference, compatibility, ABI or lowering changes.

The wrapper uses the actual function's parameter types and positional names,
with a collision escape when the function itself is named `arg0`, etc. This also
covers stored function values. Non-variable expressions receive general wrapping
advice rather than fabricated source text. A wrapper addresses the callable-kind
mismatch; incompatible parameter/result signatures may require further changes.
For a generic mismatch before bindings exist, expected types retain their type
parameter names; the found type and wrapper parameters are concrete.

## Implementation audit

Reviewed ordinary value argument checking, shared help construction, both early
and deferred-lambda generic constraint failures, first-error precedence, and the
successful generic instantiation path. Only the error path constructs help.
Failed calls still return the recovery type and do not instantiate generated
code. No runtime or platform-specific behavior changed. The runnable example
exercises existing closure lowering for ordinary and generic calls.

Let C be failing calls, P their total callable parameter count, and L the total
rendered type/name/text length. Added formatting takes O(P + L) time and O(P + L)
space across diagnostics; per-call temporary storage is proportional to its own
wrapper. Argument failure tracking adds one optional index, O(1) memory, and no
new argument scan or expression checking. There is no added multiplication by
instantiations, modules, exits or generated-code paths. Producing L characters
already requires Omega(L) work; the helper visits each parameter once for naming
and once for rendering, then joins the strings. The successful path adds no
wrapper allocation or generated/runtime work.

Deterministic increasing-size cases at C = 1, 8, 64, 256 assert exactly C errors,
C labels and C help entries. This measures diagnostic output counts, not compiler
wall-clock complexity; the traversal/space bounds above are source-audit results.
Reproduce with `cargo test --lib fn_closure_diagnostic -- --nocapture`.

## Validation

The focused regression module covers 20 explicit type/call combinations: i64,
f64, bool, String and Array<i64>, each with ordinary, inferred generic, explicit
generic and value-first generic calls. Each checks expected/found, exact argument
span, concrete wrapper text and successful typechecking after applying the help.
Additional cases cover zero/binary arity, name shadowing, stored fn values,
unchanged ordinary fn acceptance, unrelated generic conflicts, argument counts,
and increasing diagnostic counts.

Commands:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --lib fn_closure_diagnostic -- --nocapture`
- `cargo test --lib generic_lambda_tests -- --nocapture`
- `cargo run --bin willow -- run example/function_closure_wrappers.wi`

Linux native example output: `true`, `true`. Foreign native execution was not
performed. Cross-target Clippy is not applicable to this compiler-only diagnostic
change (no cfg, OS API, runtime, thread, filesystem or ABI changes). Full local
test suites and the full example audit were omitted by policy. CI was not queried.
