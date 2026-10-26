# Class-private diagnostic scope audit

Date: 2026-10-26.

## Implementation and decision

Retain class-scoped private access; see
[the decision](../class-private-scope.md). All four semantic E0501
emission sites now explain that same-module callers outside the declaring class
lack permission. Existing remedies and primary/secondary labels remain intact.
The instance method path also handles inherited method lookup. Static method,
field read/write, and implicit memberwise construction paths are covered.
Module-private free functions retain their original help: class scope does not
apply to those declarations. Access predicates, ownership lookup, protected/public
handling, IR, runtime ABI, and build scheduling are unchanged.

## Focused validation

Reproduce from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --lib class_private_scope_help
cargo test --lib memberwise_owner_
cargo test --lib super_method_lookup_errors_do_not_cascade
cargo test --test integration class_private_scope_example_runs
```

The diagnostic matrix has 20 explicit perspectives: single file/module context
× free function/other class caller × field read, field write, instance method,
static method, and implicit memberwise constructor. Every case asserts exactly
one E0501, the scope explanation, an actionable remedy, and both labels.
The existing four memberwise-owner tests cover owner access, generic owners,
external callers, inherited visibility, and field-chain visit counts.
The super-call test covers inherited private method rejection and neighboring
lookup/type failures. The runnable example prints `true` using a public factory
and accessor around a private field and private method.

## Complexity and boundaries

Source inspection: only four help string literals change in production code.
No traversal, lookup, branch, helper invocation, or allocation site is added.
Let D be the number of emitted E0501 diagnostics and L their pre-existing total
formatted help length (including variable names/types). Formatting and retained
help storage remain O(L + D); each diagnostic adds the same fixed prefix. This
is the output-size lower bound for emitting the explanation per diagnostic.
The scaling regression generates 1, 8, and 64 repeated inaccessible static calls
and asserts exactly that many diagnostics, each with exactly one identical help.
These are deterministic output counts, not timing-based complexity claims.
Existing owner/hierarchy lookup complexity is unchanged. No new multiplication
across modules, classes, inheritance chains, or compiler passes is introduced.
Generated code size and runtime time/memory are unaffected: successful programs
never execute these diagnostic branches. This is inferred from the unchanged
semantic predicates and IR paths, not from a machine-code comparison.

All listed checks passed: 2 diagnostic tests, 4 memberwise-owner tests,
1 super-call test, and 1 integration test; fmt and workspace/all-targets Clippy
reported no issues.

## Platform scope

Local host validation is Linux x86_64. No cfg, OS API, runtime, thread, stack,
or filesystem implementation changed; cross-target Clippy is not applicable.
macOS and Windows native execution was not performed. Full suites and the full
example audit were intentionally omitted; CI was not queried.
