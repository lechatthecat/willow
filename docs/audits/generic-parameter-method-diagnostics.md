# Generic parameter method diagnostics audit

Date: 2026-10-26

## Behavior and implementation

An opaque declaration parameter now reports `type parameter T has no method ...`
(with backticks in rendered diagnostics), rather than looking for a class named T.
Call receivers retain a secondary label describing the declared return type.
The original call span, including its file identity, is indexed in a hash set.
Concrete specialization checking reuses that rejection before intrinsic or class
method dispatch. It returns the existing error type, which suppresses dependent
member/return errors, while still checking argument expressions for independent
errors. Distinct calls and errors at other locations remain visible.

The parameter-name index is rebuilt once per template and is confined to the
abstract checker. Rejection spans are moved into the owning unit checker before
its pending specializations are checked. No global diagnostic deduplication rule
was weakened. Review covered template validation, instance work-queue ownership,
method dispatch order, error-type propagation, source diagnostic aggregation,
and imported function/class ownership.

The valid example exposed a separate but directly affected path: nominal alias
creation renamed prelude Option/Result types, hiding their intrinsic methods on
rechecked concrete signatures. These two builtin identities now remain stable;
nested user nominal types still pass through the existing alias table. Other
nominal aliases and collection import policy are unchanged. Local/imported
Option and Result identity-function calls compile and run. Existing nominal
alias coverage verifies user classes, inheritance, interfaces and enums, with
local/imported definitions and explicit collection.

## Test perspectives and measured counts

The unit matrix covers five receivers (parameter, Array.pop result, generic
function result, indexed element, parenthesized parameter) and four methods
(unwrap, missing, toString, len): 20 explicit rejection perspectives. Additional
cases cover independent argument errors, chained recovery, distinct parameters,
unrelated caller errors, and generic function/class bodies used or unused in
local or imported files. Called bodies have i64, bool and Option specializations.
The Option case intentionally rejects a method unavailable in the declaration
although that method would exist on a concrete argument.

Deterministic tests observed:

| Input | Sizes | Output |
| --- | --- | --- |
| Distinct rejected chained calls | 1, 8, 64, 256 | exactly 1, 8, 64, 256 diagnostics |
| Concrete specializations of one rejected body | 1, 8, 64 | exactly 1, 8, 64 checked instances; one rejection span and one diagnostic at every size |

The tests construct their source inputs in the checked-in test files, so the
counts are reproducible without generated files. No timing-based complexity
claim is made.

## Complexity and responsibility

Let P be parameter names in a template, M its rejected source calls, and V the
method-call visits across all concrete specializations. Building the parameter
index costs expected O(P) time and O(P) space (plus string bytes). Each visited
call performs bounded hash operations, independent of P and previous diagnostics.
The span set costs O(M) space; merging it once costs expected O(M). Additional
work is expected O(P + M + V), excluding already-required expression checking
and identifier hashing bytes. For S instances of a body with M calls, V can be
O(S*M); the existing body specialization pipeline still visits each instance.
This change adds no per-instance scan/copy of all rejection spans and stores
only source-site diagnostics. The source output lower bound is O(M).

Receiver expressions and argument expressions are not re-walked for origin
labels. New indexes do not affect valid generated-code size or runtime cost.
Invalid programs stop before code generation. Runtime ABI, target-specific
code, final artifact caching, and runtime auto-build are unchanged.

## Validation

Commands (all targeted test filters):

```sh
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo test -p willow --lib builtin_method_diagnostic -- --nocapture
rtk cargo test -p willow --test integration generic_parameter_method_diagnostics -- --nocapture
rtk cargo test -p willow --test integration nominal_aliases_cover_twenty_scope_storage_and_gc_cases -- --nocapture
```

Results: formatting and Clippy pass; 6 unit tests, 4 focused integration tests,
and the existing 20-case nominal-alias integration test pass. The runnable
`example/generic_parameter_methods.wi` is exercised by the integration filter
and produces `6`, `hello`, `9` on separate lines.

Linux x86_64 executed locally. macOS arm64, macOS x86_64 and Windows x86_64 were
not executed. No platform-specific code or runtime implementation changed, so
cross-target Clippy was not applicable. Full test suites and the full example
audit were not run locally; CI was not queried.

Limitations: this is a targeted method-rejection rule, not a general rewrite of
all generic-body diagnostic precedence. Existing collection import checks still
apply when a provider specializes a generic signature with Array; the valid
builtin identity controls here concern prelude Option/Result. Hash bounds are
expected bounds; no whole-compiler complexity improvement is asserted.
