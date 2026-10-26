# String.find sentinel API and diagnostics

Decision (2026-10-26): retain `String.find(needle: String) -> i64`. The first
matching byte offset is returned, `-1` means absent, and an empty needle matches
at zero. This preserves the existing documented API, offset arithmetic, sentinel
comparisons, compiler intrinsic type, and runtime ABI. `Map.get` continues to
return an Option. Consistency with Map alone does not justify breaking existing
String callers. The example reference documents the distinction and links a
runnable safe-offset example.

## Implementation audit

Inspected `check_receiver_method_call` and its Option/Result dispatch,
`resolve_method`, Option-pattern validation (including desugared if/while let),
and `check_try_propagate`. Successful operations and existing error codes/labels
are unchanged. Integer misuse of the eight supported Option method names,
Option patterns, and `?` receives sentinel guidance. The more specific direct
Channel.recv pattern guidance takes precedence.

Advice is deliberately conditional: “if this i64 came from String.find”. This
covers stored values and aliases without pretending to know their provenance.
Other integers misused in these Option contexts also receive the conditional
tip. Other receiver types and unrelated method names do not. This is a bounded
diagnostic usability tradeoff, not value-flow analysis. Ordinary type annotation
mismatches retain their existing expected/actual type diagnostics.

Reviewed `Intrinsic::StringFind` resolution, Cranelift's StringFind lowering,
`willow_string_find`, and its use by contains. None changed. No new generated
code, allocations in successful programs, ABI conversions, or runtime search
passes are introduced. Search and byte-offset semantics remain the runtime's
existing Rust string search behavior; no new runtime complexity claim is made.

Let U be invalid Option uses and A be match arms. Each changed diagnostic path
does a fixed-size type/name check and appends at most one constant-length help.
Added work and diagnostic storage are O(U), including O(A) when every arm is
invalid. There are no AST rescans, alias-chain traversals, new indexes, or
cross-caller multiplication. Reporting U separate errors already requires
Omega(U) output. Valid generated-code size and runtime cost are unchanged.

The deterministic `string_find_guidance_scales_with_invalid_uses` test measures
exactly 1, 8, and 64 diagnostics and help entries for that many stored-result
misuses. This verifies output counts, not wall-clock scaling; the constant work
per site is established by inspection of the changed branches.

## Reproduction and test perspectives

Focused commands (no full-suite run):

```sh
cargo test -p willow --lib string_find_
cargo test -p willow --lib option_pattern_
cargo test -p willow --test integration string_find_sentinel_example
cargo test -p willow --test integration string_find_sentinel_diagnostic_is_rendered
cargo test -p willow --test integration string_methods_utf8_and_composition
cargo test -p willow --test integration lir_diff_53_map_get_yields_the_maps_own_option
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Explicit new diagnostic perspectives:

1. Direct find result with unwrap.
2. Stored result with unwrap.
3. Direct result with expect.
4. Stored result with expect.
5. Direct result with unwrap_or.
6. Stored result with unwrap_or.
7. Direct result with is_some.
8. Stored result with is_some.
9. Direct result with is_none.
10. Stored result with is_none.
11. Direct result with map.
12. Stored result with map.
13. Direct result with and_then.
14. Stored result with and_then.
15. Direct result with or_else.
16. Stored result with or_else.
17. Direct result with Some/None match.
18. Stored result with Some/None match.
19. Direct result with if let Some.
20. Stored result with if let Some.
21. Direct result with while let Option::Some.
22. Stored result with while let Option::Some.
23. Direct result with Option::None match.
24. Stored result with Option::None match.
25. Direct result with question-mark propagation.
26. Stored result with question-mark propagation.

Additional checks cover unrelated method/receiver errors, honestly conditional
advice for a plain integer, repeated invalid uses, existing Channel guidance,
and CLI-rendered sentinel help. Runtime cases cover found and missing needles,
first match, oversized needles, empty needles/strings, UTF-8 byte offsets, safe
slicing, contains, and unchanged Map.get Option results. The new runnable input
is `example/string_find.wi`; integration tests execute it in debug, release,
and GC-stress modes.

## Validation results

All six focused test commands passed on x86_64 Linux: four new semantic tests,
four existing Option-pattern tests, and four integration tests (12 Rust tests
in total). The example and UTF-8/composition tests each execute debug, release,
and GC-stress variants. The rendered diagnostic and Map.get checks also passed.
Workspace Clippy passed with warnings denied. Formatting passed after formatting
the final added CLI test. `git diff --check` passed.

Foreign native execution and CI are not performed. The change has no platform-specific APIs, cfg paths,
or runtime modifications; cross-target Clippy is not applicable to this
diagnostic-only implementation. Full suites and the full example audit are
intentionally omitted under the local test policy.
