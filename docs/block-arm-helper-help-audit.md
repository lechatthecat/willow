# Block-arm helper guidance audit

## Change and responsibility

Both E0101 producers now advise moving the block arm logic into a helper
function and calling it as an expression arm. The parser rejects ordinary
expression tails and conditional value tails; the type checker catches surviving
nested match value tails through `block_arm_tail`. Both retain the existing
statement-discard suggestion. No grammar, typing, lowering, runtime, or build
scheduling behavior changes. Direct if-expression arms remain valid.

The regression first failed against the old text. The integration matrix includes
the reported binding followed by an if tail in both arm orders. The executable
example moves that complete computation into `conditional_value`, explicitly
returns the conditional result, and calls the helper from an expression arm.

## Explicit validation perspectives

The focused parser and integration cases cover:

1. Bare nested match value tail.
2. Binding followed by a match value tail.
3. Match expression arm containing an if value.
4. Bare if value tail.
5. Binding followed by an if value tail (new regression).
6. Nested if value tail.
7. Else-if chain.
8. Match value inside an if branch.
9. Parenthesized match value.
10. Parenthesized if value.
11. Nested block-arm match value.
12. Tuple binding before a match tail.
13. Function call as the nested match value.
14. If-let value tail.
15. Invalid arm before its value sibling.
16. Invalid arm after its value sibling.
17. E0101 retained and unrelated E1201 cascade absent.
18. Ordinary scalar, variable, call, and arithmetic tails.
19. Explicitly discarded values retain type mismatch diagnostics.
20. Empty and void block arms retain type mismatch diagnostics.
21. Enum payload reproduction.
22. Diagnostic primary span and context restoration.
23. Direct if-expression arm remains runnable.
24. New binding-plus-if helper workaround returns 11.
25. Existing statement, nested loop, and divergent-return examples still run.
26. Repeated and deep diagnostic inputs preserve linear token-read slopes.

## Validation results

All completed locally on x86_64 Linux:

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo test --lib parser::match_arm_diagnostic_tests`: 7 passed.
- `cargo test --test integration block_arm_control_`: 4 passed; includes
  compilation/execution of `example/block_arm_control_tails.wi`, exact output 1–11.
- `cargo test --test integration match_arm_actionable_diagnostics`: 1 passed.

Full workspace tests and the full example audit were intentionally omitted under
the local test policy. CI was not queried. macOS, Windows, and other architectures
were not executed. Cross-target Clippy is not required for this diagnostic-text
change: no cfg, platform API, filesystem, threading, or runtime implementation
was changed.

## Complexity and final implementation audit

Inspected both diagnostic producers, their existing block-tail guards, parser
context recovery, the nested match type-checking path, and all changed tests and
example code. The production diff replaces only two fixed strings: no extra
traversals, branching, generated code, or allocations are introduced. Each
emitted diagnostic still copies one constant-length help string. For D emitted
diagnostics, additional text storage and emission work remain O(D), with O(1)
work per diagnostic, the output-size lower bound. Input token count N and nesting
depth H do not acquire new factors; existing parsing/type-checking costs are
unchanged. Successful programs have identical generated-code size and runtime
cost by inspection of the text-only production diff.

Existing deterministic token-read tests passed for repeated, wide, deep, and
conditional shapes at sizes 8, 16, 32, and 64, asserting equal per-size slopes.
Those are measured parser operation assertions, not wall-clock complexity
claims. No new input-dependent algorithm needs an additional benchmark.
The two diagnostic sites intentionally retain the same literal wording rather
than introducing a new cross-layer abstraction for this small text correction.
No unresolved implementation issue was found within this scope.
