# Constructor inference diagnostics audit

Date: 2026-10-26

Unconstrained `Ok`/`Err` and `Option::None` constructors now report E1803/E1801 at the constructor, name the missing parameter, and suggest a type annotation. This applies at ordinary expression inference boundaries, including match scrutinees, discarded expressions, arrays and nested payloads. The failed constructor returns the existing recovery type, preventing the missing payload from becoming a misleading String/void concatenation error. Actual void operands retain a concatenation error but no longer receive `.toString()` advice.

The implementation reuses the existing constructor diagnostic, expression finalization, name-resolution map, and recovery handling. Annotated and generic contextual checks still supply their expected types before finalizing constructors. Method receivers retain their existing inference behavior: a method such as `and_then` can supply the omitted type, and methods that do not observe the omitted side keep their prior behavior. This change does not introduce a new whole-program inference pass or alter that method inference policy. Explicitly supplied `void` and void-valued payloads are not mislabeled as missing type arguments; existing value-type validation still applies.

## Reproduction and verification

Run from the repository root:

```sh
rtk cargo run --bin willow -- check docs/audits/constructor-inference-repro.wi
rtk cargo run --bin willow -- run example/constructor_type_annotations.wi
rtk cargo test --lib constructor_inference -- --nocapture
rtk cargo test --lib generic_variant_tests -- --nocapture
rtk cargo test --test integration typecheck::variant_diagnostics -- --nocapture
rtk cargo test --test integration typecheck::option_context -- --nocapture
rtk cargo test --test integration runtime::display -- --nocapture
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets -- -D warnings
```

The negative reproducer must fail with E1803, identify error type `E`, and suggest `Result<i64, String>`. It must not report a concatenation error or suggest `.toString()`. The runnable example prints `7`, `true`, and `fail missing` on separate lines.

Focused validation passed on Linux x86_64: 7 constructor-inference unit tests (table-driven cases), 38 generic-variant unit tests, 23 variant-diagnostic integration tests, 20 Option-context integration tests, and 5 display integration tests. The runnable example, formatting and workspace/all-targets Clippy passed. Full workspace tests and the full example audit were intentionally not run. No CI was queried. macOS arm64, macOS x86_64 and Windows native execution are unverified. Cross-target Clippy is not applicable to this compiler-only change: no cfg branches, platform APIs, runtime, filesystem, thread or stack implementation changed.

## Explicit test perspectives

The new unit tests cover these independent cases; the integration tests verify rendered diagnostics and executable contextual construction:

1. Bare `Ok` missing error type.
2. Bare `Err` missing success type.
3. Qualified `Result::Ok` missing error type.
4. Qualified `Result::Err` missing success type.
5. Qualified `Option::None` missing element type.
6. Bare `None` missing element type.
7. Unannotated binding boundary.
8. Discarded-expression boundary.
9. Match-scrutinee boundary.
10. Constructor source label location.
11. Annotation help for missing parameters.
12. No cascade from an invalid match payload into String addition.
13. Annotated shorthand Result success and error constructors.
14. Annotated qualified Result success and error constructors.
15. Annotated shorthand and qualified None.
16. Some payload inference without an annotation.
17. Expected type from a function parameter.
18. Expected type propagated through a ternary.
19. Expected type propagated through match arms.
20. A function named `Ok` retains ordinary name resolution.
21. Void on the left of String addition.
22. Void on the right of String addition.
23. Numeric String addition retains its conversion help.
24. Explicit void type arguments are not called inference holes.
25. A void-valued Some payload is not called an inference hole.
26. Nested constructor in an array.
27. Nested constructor in another enum payload.
28. Contextless constructors in both ternary branches.
29. Contextless constructors in both match arms.
30. Wrong constructor arity does not add a missing-type cascade.
31. Unknown payload name does not add a missing-type cascade.
32. Repeated equivalent constructors produce exactly one check and diagnostic each.
33. Existing method-chain inference (`Result::Ok(10).and_then(add_five)`) still compiles and runs.
34. Generic function inference receives evidence from other arguments and expected results (existing focused generic-variant cases).

## Complexity and implementation audit

Independent dimensions are visited expression count N, constructor count C, payload type depth D, and emitted diagnostics K. The added ordinary-expression gate is constant work per existing visit. Builtin detection compares fixed names and fixed-size Option/Result argument slices. Bare Result detection reuses the node-ID resolution map instead of repeating lexical lookups. The diagnostic code does not recursively traverse payload types, search sibling expressions, clone syntax, recheck arguments, or add a later AST pass. Its added work is O(N + K), with O(K) diagnostic storage and O(1) temporary state per check. Reading each visited expression and producing each diagnostic impose the corresponding lower bounds. Existing parser/checker recursion and user-enum checks are unchanged; this is not a claim about the complexity of the entire compiler.

A test-only counter measures actual diagnostic candidate checks at the constructor boundary. `constructor_inference_diagnostic_counts_scale` generates independent repeated match scrutinees, with both success and error arms, at increasing sizes:

| Constructors | Measured candidate checks | Measured E1803 diagnostics |
| ---: | ---: | ---: |
| 1 | 1 | 1 |
| 8 | 8 | 8 |
| 64 | 64 | 64 |
| 256 | 256 | 256 |

These are deterministic assertions, not timing-derived complexity estimates. Nested arrays, enum payloads, ternaries and match arms separately check propagation and fan-out without a repeated diagnostic pass. The test source and generator live in `src/semantic/type_checker/constructor_inference_tests.rs`; the standalone failing input is adjacent to this report.

Audited call paths: ordinary expression checking and finalization; annotated binding checking; contextual enum construction and generic-argument inference; method receiver checking; invalid match-pattern recovery; binary String addition; user-defined enum binding diagnostics. Malformed constructors now keep their original argument/name error without a second inference diagnostic. The final implementation uses existing compiler ownership boundaries and changes no build scheduling, linking or caching.

Invalid programs stop before code generation. No generated-code templates, runtime ABI, allocation, cleanup or dispatch paths change. Runtime/generated-code cost for valid programs is therefore unchanged by inspection; native execution of the annotated example and existing chain tests corroborates behavior on Linux only. There is no foreign-native or full-suite evidence in this audit.
