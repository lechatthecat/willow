# Builtin method diagnostics audit

Date: 2026-10-26

## Behavior and acceptance coverage

Unknown methods on String, i64, f64 and bool now identify both the requested
method and receiver type. Available-method lists provide discovery even when
there is no plausible spelling correction. Array and Map retain their existing
lists, now including previously omitted join/toString entries where applicable,
and use the same message format. Array.join requires String elements; a listed
method is not a promise that all element types or argument lists are valid.

Direct method, function and static-call receivers receive a secondary label at
the receiver call with its checked return type. For example, calling unwrap_or
on String.find's result identifies i64 and retains the sentinel-value guidance.
The parser's method-call span identifies the method token rather than the entire
expression; the label deliberately uses that existing source span. Stored local
receivers show their type but do not gain speculative call-origin labels: this
change does not add inter-statement provenance analysis.

No overload resolution, argument validation, runtime ABI or lowering changes.
The shared resolver now receives the MethodCallExpr instead of three separate
fields. Both regular and super dispatch callers preserve their dispatch flag.
Error recovery, Option handling and class/interface lookup remain on their
existing paths. The fallback also gives a specific missing-method message for
non-class types such as void.

## Implementation and complexity review

Reviewed both resolver callers, primitive method recognition against the
intrinsic resolver, Array/Map dispatch, the secondary-label helper, and error
recovery paths. The continuation macro compiles the revised resolver with the
same checked receiver; the helper never invokes expression checking or walks
receiver children. Successful calls return through the same earlier branches,
so no new generated program code or runtime work is introduced.

Independent dimensions are invalid call count N, requested-name bytes M, and
rendered receiver-type bytes T (including nested generic element types). New
work is O(M + T) per diagnostic and O(sum(M + T)) overall, with equivalent stored
output space. Primitive candidate inventories are fixed strings, so lookup and
list selection are O(1). Origin detection inspects one AST variant and one span;
it adds no factor for call-chain depth, exits, classes or module count. No symbol
scan, edit-distance search, rechecking, or global diagnostic traversal is added.
Writing method/type text is a necessary output-size lower bound. The type may
be formatted a constant number of times; it is not recomputed for every candidate.

Deterministic output-count test results for repeated invalid find-result calls:

| Invalid calls | Diagnostics | Labels | Help entries |
| ---: | ---: | ---: | ---: |
| 1 | 1 | 2 | 2 |
| 8 | 8 | 16 | 16 |
| 64 | 64 | 128 | 128 |
| 256 | 256 | 512 | 512 |

These are measured structural counts, not wall-clock asymptotic estimates.
The no-recheck/O(1) origin bound is inferred from the helper implementation.
The test source generator is checked in at
`src/semantic/type_checker/builtin_method_diagnostic_tests.rs`.

## Reproduction and validation

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --lib builtin_method_diagnostic`: 3 tests; 24 family/name cases,
  six direct call origins, and four increasing-size diagnostic-count cases.
- `cargo test --lib string_find_`: 4 tests, retaining sentinel guidance.
- `cargo test --lib taskwait_07_result_is_not_a_method_on_plain_values`: 1 test.
- `cargo test --lib diagnostic_recovery_`: 26 tests.
- `cargo test --lib super_call_`: 7 tests for the other resolver caller.
- `cargo test --test integration unknown_method_is_error`: 2 compiler diagnostics.
- `cargo run -- run example/builtin_method_surface.wi`: Linux compile/link/run;
  output lines: `a`, `5`, `1.5`, `true`, `3`, `1`, `1`.

Validation is on Linux x86_64. macOS arm64, macOS x86_64 and Windows execution
are unverified. No platform APIs, cfg paths or runtime code changed, so additional
cross-target Clippy is not applicable. Full suites and full example audit were
not run; CI was not checked. Available-method lists are intentionally explicit
and must be maintained when the builtin surface changes.
