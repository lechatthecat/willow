# Semantic CLI symbol-kind validation audit

Date: 2026-10-26

## Behavior and ownership

The direct semantic CLI parser now rejects unknown `--kind` values before project
lookup or compiler construction. Errors identify the invalid value, enumerate all
15 symbol kinds, explain that kinds describe symbols, and state that the effects
command currently has no effect filter. The diagnostic suggests inspecting effects
without the kind filter. Existing human stderr and JSON/NDJSON invalid-arguments
rendering and exit code 2 are reused.

Reviewed `Options::parse`, `run`, `Options::execute`, `DirectSession` filtering,
and symbol construction in `src/ai/symbols.rs`. The accepted list includes lexical
kinds (module, import, binding, parameter, type-parameter) that overview deliberately
omits. Compiler query APIs, language semantics, generated code and runtime behavior
are unchanged. No new effect-filter feature is introduced.

## Validation

Before the fix, the new regression failed: an invalid kind reached project lookup
and returned exit code 1 instead of invalid-arguments/2. After the fix:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo test --test semantic_cli direct_kind_rejects_effect_names_and_unknown_values -- --exact`: passed; 168 invocations (7 commands, 4 invalid values, 3 formats, 2 option spellings), with the manifest absent to prove rejection precedes project discovery.
- `cargo test --bin willow direct_kind_accepts_every_symbol_kind -- --exact cli::direct::kind_tests::direct_kind_accepts_every_symbol_kind`: passed; all 15 kinds in both spellings.
- `cargo test --test semantic_cli direct_cli_formats_errors_discovery_and_legacy_compatibility -- --exact`: passed.
- `cargo test --test semantic_cli direct_queries_resolve_fields_functions_and_locations -- --exact`: passed.

The matrix covers effect names, arbitrary invalid names, case sensitivity, all
direct commands including the references alias and rename dry-run, human stderr,
JSON/NDJSON status/message/exit code, and both split and inline option values.
Existing tests cover duplicate flags, valid filters, unknown symbols and resolution.

## Complexity and limitations

Let A be argument bytes and P the project size. The new membership check makes at
most 15 string comparisons, against fixed literals of at most 14 bytes. This bound
is established from the code, not inferred from timings. It adds constant work and
storage on success; the failure message takes O(K) time/space for an invalid value
of K bytes, necessary to echo that value. Joining the fixed list happens only on
failure. Parsing remains O(A); invalid kinds perform no work dependent on P, as the
missing-manifest regression demonstrates. No repeated project scans, compiler
passes, generated-code expansion or runtime allocations are introduced. Growing
project-size benchmarks would not exercise the changed path and were omitted.

Linux validation only. Foreign native execution and CI were not run. Cross-target
Clippy is not required for this argument-validation-only change: no platform APIs,
filesystem behavior, cfg branches, compiler or runtime implementation changed.
Full test suites and the full runnable-example audit were intentionally omitted.
The literal vocabulary must be updated if new symbol kinds are introduced.

## Review correction validation

Corrected the invocation count to 7 × 4 × 3 × 2 = 168 after checking the
actual test loops. Re-ran the named invalid-kind regression: passed (1 test,
113 filtered out). `git diff --check` passed. This correction changes only
documentation; the implementation, test matrix and complexity assessment are
unchanged. Earlier successful fmt and Clippy results remain applicable.
