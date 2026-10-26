# Generic class annotations on lambda parameters

Date: 2026-10-21. Implementation review evidence; ticket remains in progress.

## Cause and fix

`check_lambda_with_context_inner` validated the source `Generic` type before
normalization instantiated its class. `check_type_argument_count` consequently
looked for the template in the concrete symbol table and emitted E0350. Type
arguments were not lost by parsing. Normalize first, then validate the resulting
type, matching named function parameters and lambda return annotations.

The regression test failed before the fix with `cannot find type Wrap` for its
first `Wrap<A>` expression lambda. All twenty cases pass after the fix.

## Acceptance perspectives and execution

The check API matrix explicitly crosses five shapes (`Wrap<A>`, `Wrap<i64>`,
`Wrap<Wrap<A>>`, `Wrap<String>`, `Wrap<bool>`) with four contexts: standalone
expression, standalone block, fn argument/expression, capturing closure
argument/block. These are twenty independently checked programs. Additional
negative checks cover omitted arguments, excessive arguments, unknown nested
type and unknown plain type. Integration tests execute qualified imported class
annotations and reject private imported classes. The existing enum alias lambda
parameter regression is also run.

`example/generic_lambda_annotations.wi` executes expression/block lambdas,
fn context and a capturing closure with nested arguments; expected stdout is
`1`, `2`, `3`, `14` on separate lines. It has a focused integration test and an
entry in the existing example catalog.

## Implementation and complexity audit

Reviewed the common lambda path, parameter scope insertion, returned Fn/Closure
signature, normalization and generic instantiation, nominal validation, and
named-function parameter ordering. Captures and inferred parameters are
unchanged. Normalization still enforces generic arity, argument validity and
privacy; the negative tests verify those boundaries. Runtime execution verifies
that the concrete signature reaches lowering and native code for both callable
forms. No runtime ABI or platform-specific implementation changes.

The production change only reorders the two existing operations; it adds no
traversal, allocation, specialization, body check or generated dispatch. Validation
now visits the normalized nominal identity rather than traversing the source
generic arguments again. Let L be annotation occurrences, D nested generic
nodes per annotation, and U unique concrete instantiations. The existing
normalizer still makes L*D instantiation requests for repeated depth-D chains,
while the registry specializes U classes and interns each template once.
Template body size and argument serialization/key sizes remain independent
cost dimensions of the existing instantiator. These counts do not establish
that the entire compiler has linear time: normalization-map key cloning and
logical-type reconstruction retain their existing costs. This patch adds no
multiplication across callers, exits, modules or runtime paths. Reading each
explicit annotation requires work proportional to its syntax size; sharing
concrete instantiations avoids repeated body generation.

The deterministic unit test generates `Wrap<...Wrap<i64>...>` at depth D and L
independent `let fN = |x: TYPE| x;` declarations. Measured counts:

| L | D | Requests | Specializations | Template copies |
|---:|---:|---:|---:|---:|
| 1 | 1 | 1 | 1 | 1 |
| 1 | 4 | 4 | 4 | 1 |
| 1 | 8 | 8 | 8 | 1 |
| 8 | 1 | 8 | 1 | 1 |
| 8 | 4 | 32 | 4 | 1 |
| 8 | 8 | 64 | 8 | 1 |
| 32 | 1 | 32 | 1 | 1 |
| 32 | 4 | 128 | 4 | 1 |
| 32 | 8 | 256 | 8 | 1 |

The checked-in generator/assertions are in
`src/semantic/type_checker/generics.rs`, test
`generic_lambda_annotations_reuse_class_specializations`. Counts measure checker
requests and reuse, not backend instruction counts or wall-clock complexity.
Generated-code effects are inferred from unchanged lowering and specialization
reuse, with native correctness checked by the focused runtime tests.

## Reproduction and validation

Run from the repository root:

```sh
rtk cargo test --test check_api generic_lambda_annotations -- --nocapture
rtk cargo test --lib generic_lambda_annotations_reuse_class_specializations -- --nocapture
rtk cargo test --test integration generic_lambda_annotations -- --nocapture
rtk cargo test --test integration alias_12_lambda_parameter_annotation -- --nocapture
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets -- -D warnings
```

Linux x86_64: focused check tests (2), count test (1), integration tests (3),
enum alias test (1), formatting and workspace/all-targets Clippy passed. No full local suites or full example
audit run. CI was not queried. macOS arm64/x86_64 and Windows native execution
are unverified. Cross-target Clippy is not applicable to this semantic-only
change (no runtime, OS APIs, cfg, threading or filesystem implementation edits).
