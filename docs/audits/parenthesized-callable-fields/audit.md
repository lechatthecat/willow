# Parenthesized callable fields implementation audit

Date: 2026-10-26

## Behavior and implementation

`(receiver.field)(args)` reads the field and calls its value. This includes
`self`, generic class substitutions, inherited fields, closure values and
function pointers. `receiver.field(args)` retains method dispatch and its
existing callable-field diagnostic. Parenthesizing a method name alone no
longer selects a method: this syntax requires a field. A local callable such
as `(callback)(args)` retains its existing behavior.

The previous parser explicitly converted a parenthesized FieldAccess into a
MethodCall. The correction retains the FieldAccess and moves it into a
single-arm binding match, whose expression body calls the binding. This uses
existing scoped binding, type checking, closure ABI and indirect-call lowering;
no runtime or platform code changes are needed. The internal binding uses a
name containing `$`, which cannot be a source identifier. Nested bindings
are scoped and lowering gives shadowed locals distinct identities.

Audited paths: parser postfix construction and expression ownership;
check_match_expr/check_match_body and callable resolution; indexed semantic
scopes; lower_match/lower_call_expr and indexed lowering scopes; LIR match
binding and indirect calls. The receiver is evaluated once before arguments,
and the bound closure remains rooted across argument evaluation. The mutation
test verifies that replacing the field in an argument still calls the original
closure. Captured values, nested calls and heap-valued String results execute
successfully. Existing return-value and void match paths are reused.

## Explicit test perspectives

The focused runtime tests cover: (1) plain closure field, (2) repeated
parentheses, (3) self field, (4) generic receiver, (5) generic self,
(6) nested receiver fields, (7) zero arguments, (8) two arguments,
(9) bool result, (10) String result, (11) arithmetic composition,
(12) nested callable arguments, (13) trailing comma, (14) normal method
call preservation, (15) parenthesized local preservation, (16) receiver
side effects exactly once, (17) receiver-before-argument order,
(18) indexed receiver, (19) array literal composition, (20) conditional
composition, (21) sibling calls, (22) generic String field,
(23) generic String self, (24) callable snapshot before field mutation,
(25) fn pointer field, and (26) void closure return.

Semantic tests additionally cover private self fields, inherited fields,
plain/generic fn signatures, rejecting non-callable fields, too few/many
arguments, wrong argument types, inaccessible fields, absent fields, and
parenthesized method-only names. Existing direct callable-field diagnostics
remain tested. Parser perspectives cover postfix field/index/try composition
and reference-argument syntax; these parser-only cases are not claims of
runtime verification of those combinations.

## Complexity and deterministic evidence

Let N be callable-field suffixes and A the total argument expression size.
Each new suffix moves its receiver rather than cloning the growing prefix,
and adds one match, one binding and one named call. Construction adds O(N)
time and AST memory beyond O(A) argument parsing. The parser count test uses
receiver chains of 16, 64, 256 and 1024 suffixes and finds exactly N calls,
N arguments and one original receiver leaf.

Semantic and lowering scopes use a per-name ownership index, so the new
binding scopes do not introduce depth-proportional name lookup scans. Each
one-arm match checks/lowers its scrutinee once and its call once. For fixed
callable signatures, incremental traversal work and storage are O(N + A).
Type substitution and argument checking retain their existing dependence on
signature size and generic instantiations; this is not a claim that the
whole compiler is linear for arbitrary types.

Reproduce code-generation measurements after building the compiler:

```
rtk cargo build --bin willow
rtk proxy python3 docs/audits/parenthesized-callable-fields/count_codegen.py
rtk cargo test --lib closure_field_postfix_linear_ast_counts -- --nocapture
```

| Shape | Calls | Field loads | Indirect calls | LIR instructions |
| --- | ---: | ---: | ---: | ---: |
| Siblings | 16 | 16 | 16 | 202 |
| Siblings | 64 | 64 | 64 | 778 |
| Siblings | 256 | 256 | 256 | 3082 |
| Nested arguments | 16 | 16 | 16 | 217 |
| Nested arguments | 64 | 64 | 64 | 841 |
| Nested arguments | 256 | 256 | 256 | 3337 |

These are measured counts, not timing extrapolations. Counts follow 12N+10
and 13N+9 respectively. Existing binding-match lowering adds constant-size
control flow and root management per call; it allocates no new closure or
receiver object for invocation. Runtime work is one field load and one
indirect call plus argument/body work, meeting the required Omega(N) lower
bound for N observable calls. Peak temporary storage depends on expression
nesting. Reusing match lowering trades constant control-flow overhead for a
small parser-only implementation; no new dispatch algorithm, repeated
receiver scan, duplicated receiver code or build scheduling is introduced.

## Validation and limits

Passed on Linux x86_64:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --lib closure_field -- --nocapture` (6 tests)
- `cargo test --lib parenthesized_callable_field -- --nocapture` (1 test)
- `cargo test --test integration test_parenthesized_callable_field -- --nocapture` (2 tests)
- `cargo test --test integration test_closure_field_binding_example -- --nocapture` (1 test)
- `cargo test --test integration test_call_result_binding_example -- --nocapture` (1 test)
- deterministic LIR count script and `git diff --check`.

Validation commands used the RTK wrapper. The runnable example is
`example/closure_field_binding.wi`, exercised through its focused integration
test. Full suites and the full example audit were omitted under local test
policy. No CI was inspected. macOS arm64/x86_64 and Windows native execution
are unverified. Cross-target Clippy was not required for this parser-only
production change (no cfg, runtime, OS, filesystem, thread or socket changes).
