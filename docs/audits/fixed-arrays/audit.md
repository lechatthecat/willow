# Source-defined and fixed-size array implementation audit

Date: 2026-10-26

## Current implementation

`std/collections.wi` now declares the generic Array type, its constructor,
len/push/pop/freeze methods, and the element-dependent join/toString methods.
Parsing `Array<T>` produces an ordinary generic application. The collections
export derives its arity from the source declaration; bundled source enters the
normal module, generic checking, specialization, method resolution and lowering
pipeline. Source-selected method owners carry the existing GC array handle ABI.
Public method signatures are no longer supplied by the intrinsic table.

The compiler assigns storage metadata and a private declaration-file capability
to the bundled Array declaration. The remaining array operations are allocation,
indexing, bounds checks, length, storage resizing, and snapshot copying. String
concatenation of parts and debug quoting are separate low-level string operations.
These operations do not choose public Array method availability or implement its
public iteration/rendering algorithms. The source implements push/pop, separators,
brackets and element traversal. Snapshot copying retains the existing frozen
handle representation. User code cannot invoke the private ABI methods.

Finite method constraints use `where T: i64 | f64 | bool | String` (or a subset).
Every declared alternative is checked, even when unused. Unsupported concrete
methods are removed before ordinary signature registration and code generation.
Unconstrained generic code retains its existing abstract checks; no bundled body
is exempt. The new constructor authority test changes the source signature and
proves both explicit and unresolved calls follow its result and arity.

Import access is checked on written source type applications, before raw storage
erasure. Literals and inferred/imported array values remain usable without an
Array type import. Item imports, aliases, module imports and fully qualified
spellings keep their existing behavior. Shared storage dispatch uses nominal
declaration identity plus specialization identity, so local/aliased interface
spellings agree without conflating different generic element classes.

The earlier fixed-size implementation remains: `[T; N]` retains N through type
checking, rejects resizing and mismatched lengths, then erases the checked size
to the same runtime storage ABI. Neither full native execution on other platforms
nor full local suites are claimed.

## Current complexity audit

Independent dimensions: source methods M, constrained alternatives A (at most
four distinct scalar types), method-body expression counts B_i, type nodes V,
unique concrete element specializations U, consumer callsites C, array length N,
and rendered output bytes S.

Constraint checking copies and substitutes only the selected method, not every
class body for every method. The added deterministic unit test varies methods and
body size independently over 1, 8, 32 with two alternatives. Expression visits are
exactly `2 * M * (B + 1)`; raw counts are in `constraint-counts.txt`. The compiler
must inspect each declared alternative/body for sound checking. The bound for
this added traversal is O(sum A_i * B_i), with space bounded by the selected body;
existing expression/type-check costs are additional, not measured by this counter.

The module dependency index locates bundled collections once. Consumers use an
indexed lookup, and duplicate immediate signature registration was removed.
Specialization caching shares source bodies by concrete key; each consumer still
registers the required available signature metadata. The array API has a fixed
small number of source methods; its template availability scan is O(M) per call.
Type/storage dispatch-key construction visits V nodes once with constant-time
nominal lookups. Generic snapshots retain distinct monomorphization identities.

Generated-code tests vary constructor/freeze callsites over 1, 8, 32, 128:
source calls equal C while shared array allocation/copy relocations stay at 5/2.
Join tests vary callsites over 1, 8, 32 and input text size over 1, 1024 bytes:
source join calls equal C and shared concatenation relocations remain 2.
`source-dispatch-counts.txt` records these counts. They prove sharing in these
shapes, not a universal whole-compiler complexity bound. Fixed declared lengths
1, 32, 1024, 1048576 produce byte-identical normalized entry LIR. Historical
pre-migration and intermediate source-dispatch counts are retained in
`measurements.txt`; the final length-forwarding result is 463 bytes / 11 lines
for all four lengths.

Two bounded source-body optimizations retain the earlier scalar fast paths.
The loader recognizes an exact length-return body and an exact three-statement
append body, without testing public method names. Both proofs inspect a fixed
shape once per source method before body offloading, O(M) time and O(M) metadata.
Changed, effectful, async or differently typed append bodies are not expanded.
Statement-position append calls become receiver/value temporaries, a length
read, a resize and an indexed store; operand evaluation precedes the length
read. Deterministic HIR tests at 1, 8, 32 calls measure exactly 5*C statements.
Expansion costs O(C) fixed operations, with normal type-copy costs; it does not
clone arbitrary method bodies or recursively inline calls. Non-statement
positions retain normal shared source dispatch. Source rendering helpers retain
exactly three module loops across 1, 8, 32 consumer calls. The unchanged scalar
read/write loop assertions pass, including call-free hot paths and exact
load/store counts. This is a bounded code-size/dispatch-cost tradeoff, not a
claim of universal compiler optimality. Body planning borrows ordinary method
declarations and boxes only synthesized constructor bodies; the larger source
metadata no longer inflates every body-plan entry or requires per-method
allocation.

Runtime length/index operations stay O(1). Push has O(1) amortized growth through
existing geometric allocation; pop is O(1). Snapshot copying is O(N). Source
join/render assemble parts and concatenate once, O(N + S) time and O(N + S)
intermediate/output memory, avoiding repeated prefix concatenation. Scalar render
allocates one converted string per element; string render retains debug escaping.
Pair growth/pop reuse existing box ownership, captured-buffer and write-barrier
paths. Resize is O(abs(new_len-old_len)) plus any growth copy; source push/pop
change length by one. These runtime bounds are derived from code, not timing.

## Additional explicit test perspectives

The 37 fixed-array perspectives in the historical section remain covered. The
migration adds focused checks for:

1. All four scalar constraint alternatives, debug/release/GC stress.
2. String-only constrained operations and ordinary generic getters.
3. Rejected unused invalid alternative bodies.
4. Unknown constraint parameters, duplicate/nonconcrete alternatives.
5. Constraints rejected on nongeneric classes.
6. Concrete unavailable methods and undeclared element operations.
7. Imported constrained methods, positive and negative domains.
8. Nested literal-only arrays without imports.
9. Private ABI access rejection for length, resize, copy, concat and quoting.
10. Source constructor arity/result authority under a changed declaration.
11. Constructor aliases, explicit element arguments and contextual inference.
12. Unresolved empty semantic checks and one diagnostic for unresolved freeze.
13. Existing missing-import diagnostic codes on locals, parameters and main args.
14. Fully qualified and module-qualified collection types/constructors.
15. Distinct generic nominal element specializations.
16. Interface arrays across growth and moving GC.
17. Borrowed pair slots across growth, pop, later arguments and suspension.
18. Frozen snapshot independence and contextual frozen inference paths.
19. Existing scalar fast paths and root behavior across async frames.
20. Join separators/escaping, invalid arguments, generated-call sharing.
21. Constraint traversal counts with independently increasing body/method counts.
22. Existing unknown-method names, suggestions and receiver-call-origin labels.
23. Runnable fixed-array and source-array/constrained-method examples.
24. Receiver and argument evaluation once and in order, including arguments
    that mutate the receiver and allocate under moving GC.
25. Source-body-derived length forwarding, including a renamed method and
    changed bodies that must retain ordinary source dispatch.
26. Existing read/write loop layout, call, load/store and async safepoint checks.
27. Generic lambda inference across source Array storage normalization.
28. Append source-shape authority under renamed and modified declarations,
    fixed expansion counts, and shared helper-loop counts.

## Current validation and limitations

Validation completed for this implementation:

- `cargo fmt --all -- --check`: PASS.
- `cargo clippy --workspace --all-targets -- -D warnings`: PASS.
- The same workspace/all-target Clippy check with `--target` passed for
  `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
  `aarch64-apple-darwin`, and `x86_64-apple-darwin`.
- `cargo test -p willow --lib array_ -- --nocapture`: 87 PASS. This focused
  filter includes original loop-performance, GC-root, async safepoint, intrinsic
  authority, constructor, import/type and backend eligibility checks; 3305 other
  compiler tests were excluded.
- Named compiler tests `source_append_expansion_has_constant_operations_per_call`
  and `constrained_method_body_work_scales_with_selected_bodies`: both PASS.
- `cargo test --test integration FILTER -- --nocapture`: `constrained_methods`
  8 PASS; `fixed_arrays` 9 PASS; `array_join_` 4 PASS; `freeze_inference_` 2 PASS.
- Named integration tests `pair_51_reference_capture_keeps_original_buffer_after_resize`,
  `pair_54_reference_survives_later_argument_pop`,
  `pair_55_reference_survives_later_argument_pop_and_suspend`,
  `pair_56_pop_push_reuses_borrowed_slot`, and
  `array_fast_paths_scalar_words_growth_and_reference_parameter`: all PASS.
  `contextual_interface_arrays_example` selected debug/release checks: 2 PASS.
- `python3 docs/audits/fixed-arrays/measure.py` and
  `python3 docs/audits/fixed-arrays/source_migration_probe.py`: PASS; all six
  positive/negative probes matched expectations. Count evidence is stored beside
  this audit. Source and fixed-array runnable examples passed through the focused
  integration tests.
- `git diff --check`: PASS. No internal identifiers were found in added lines or
  new public artifacts; the private-directory ignore rule remains effective.

Intermediate failures were repaired rather than skipped: source authority and
inference issues, obsolete direct-lowering test fixtures, call-free append loop
regressions, and method-payload sizing reported by Clippy. Original performance
assertions were retained. Historical validation below remains separately labeled.

Native execution is Linux x86_64 only. Windows x86_64 MSVC and both macOS targets
receive local Clippy checks only. Full local test suites, the full example audit,
foreign native link/run and CI polling are intentionally omitted. An unresolved
empty array can have element-independent source signatures checked, but native
Array<Void> storage remains unsupported as before; concrete contexts infer the
element and dispatch compiled source methods normally.

Independent review found and prompted repairs to constructor authority, inferred
import checks, repeated registration, source-method diagnostics, and test-only
HIR eligibility. Subsequent review found no blocking defect in the corrected
migration, constrained checking, inference normalization, source-body fast paths,
evaluation order, GC behavior and complexity. The method-plan representation fix
was independently reviewed as well. Prior evidence is retained below. Closure
and publication are outside this implementation handoff.

---

# Historical fixed-array phase (superseded status, evidence preserved)

The following is the original pre-migration audit. Its incompletion/design-choice
statements describe that earlier phase; the explicit constrained-method decision
and implementation above supersede them.

# Fixed-size array implementation audit

Date: 2026-10-26

## Scope and completion boundary

This change implements the fixed-size array part of the planned array work.
**The broader Array-to-standard-library migration is not complete.** Dynamic
`Array<T>` remains a compiler-known type: its parser conversion, method checking,
and intrinsic selection have not been replaced by a generic `.wi` declaration.
This report is not approval of that broader migration or a claim that all its
acceptance conditions have been met.

## Representation and behavior

`[T; N]` retains its element type and non-negative literal length in the source
and checked type trees. Equality, hashing, serialization, generic inference and
normalization preserve the length. A contextual literal must contain exactly N
elements; fixed and dynamic arrays are not implicitly interchangeable. Indexed
mutation is permitted; `push` and `pop` are rejected. Storage is GC-managed with
reference semantics, not an inline value copied on assignment. The syntax does
not implement constant expressions, const generics, repeated-element literals,
or dynamic-to-fixed conversion.

HIR lowering erases the checked length constraint and reuses existing array
allocation/index/bounds/GC operations. Declaration, expression and imported enum
payload paths are covered. Backend representation comparisons ignore only the
array length/resize distinction after checking; they retain element and nominal
identity. Fixed-array class fields remain GC references. No runtime ABI, root
layout, allocation routine, build scheduling or cache policy was changed.

## Explicit test perspectives

The focused integration tests cover:

1. Integer indexing and assignment.
2. Length queries.
3. Parameters with fixed lengths.
4. Fixed-length return values.
5. Generic identity with an annotated array.
6. Zero-length literals.
7. Boolean elements.
8. Floating-point elements.
9. Nested arrays.
10. Iteration.
11. Class fields containing string arrays.
12. Nested arrays inside class fields.
13. GC survival of reference elements.
14. Contextual ternaries.
15. Too few elements.
16. Too many elements, including zero-length expectations.
17. Empty literal versus nonzero length.
18. Rejection of push and pop.
19. Different declared lengths remain distinct.
20. Element-type mismatches.
21. Negative, floating and named lengths are rejected.
22. Assignment and index type errors.
23. Nested length mismatches.
24. Fixed/dynamic type separation and existing dynamic import gating.
25. Higher-order function arguments and closures.
26. Generic fixed-array literal inference.
27. Enum payloads and Option payloads.
28. Dynamic arrays containing fixed arrays.
29. Async-frame locals, GC stress, freeze, join and display.
30. Bounds panic recovery for reads and writes.
31. Imported enum payloads and iteration after matching.
32. Async reference signatures (no claim of mutable cross-task sharing).
33. Generic ternary and match literal inference.
34. Partial enum inference holes in both element orders.
35. Empty fixed-array inference from another argument.
36. Concrete void is not treated as an inference hole.
37. Runnable example registration.

The value and async-local cases run in debug, release and GC-stress modes.
The remaining focused cases use their explicitly selected modes; they are not
an exhaustive combination of every feature with every execution mode.

## Complexity and evidence

Independent dimensions are type-node count V, array element count N, number of
expressions E and serialized type size. Parsing adds one frame/node per bracket
constructor. Type mapping, storage erasure, equality, hashing and serialization
visit each type node a constant number of times: O(V) time and O(V) worst-case
work/output space. Storage erasure is fused into the existing type-copy traversal,
without a second traversal or a replacement allocation per fixed node. Across E
independent copied types the bound is the sum of their node counts, rather than
E times the maximum declared array length.

Generic literal checking examines each element once and merges only unresolved
inference constraints. Its bound is O(sum of checked element-type sizes), plus
existing expression checking. Concrete equal elements do not allocate a merged
type. Constructing N literal elements still requires O(N) writes and O(N) storage;
reading, writing and querying length retain existing O(1) array operations. No
code is generated proportional to a declared length without literal elements.
These runtime bounds are source-derived, not inferred from wall-clock timings.

`fixed_array_type_tree_scaling_and_identity` checks depths 1, 32, 128 and 512:
serialized node counts and erased storage node counts are exactly depth + 1,
with clone, substitution, equality and wire round-trips. `measure.py` generates
length-only functions for N = 1, 32, 1024 and 1048576. After removing source spans,
the complete LIR is byte-identical: 457 bytes / 11 lines each. Raw counts are in
`measurements.txt`; generated sources and artifacts live in a temporary directory.
These checks establish type/output size bounds, not whole-compiler operation
counts. No claim is made that all pre-existing compiler passes are optimal.

## Validation

Final results: formatting passed; all 9 fixed-array integration tests passed;
the named type-tree unit test and both named dynamic-array regression tests
passed. All four cross-target Clippy commands passed with warnings denied.
The measurement script reproduced the recorded counts. The final intrinsic
resize guard was followed by another passing run of all 9 fixed-array tests,
the dynamic fast-path regression, formatting and all four Clippy targets.

Focused commands:

- `cargo fmt --all -- --check`
- `cargo test --test integration fixed_arrays -- --nocapture`
- `cargo test --lib fixed_array_type_tree_scaling_and_identity`
- `cargo test --test integration test_array_i64_literal_len_and_index`
- `cargo test --test integration array_fast_paths_scalar_words_growth_and_reference_parameter`
- `python3 docs/audits/fixed-arrays/measure.py`
- Workspace/all-target Clippy with `-D warnings` for Linux x86_64,
  Windows x86_64 MSVC, macOS arm64 and macOS x86_64.

Native execution is Linux x86_64 only. Foreign native link/run tests, full local
test suites, the full example audit and CI checking are intentionally omitted.

## Source migration investigation

A subsequent review correctly identified that the source-defined dynamic Array
migration is still absent. No acceptance criteria were changed and no source
stub was added to disguise compiler ownership.

`source_migration_probe.py` checks three small public-source examples against the
current compiler. An ordinary generic class with a field/getter passes. A generic
method calling `T.toString()` fails with E0201, even when only the concrete i64
class is instantiated. A String-specific generic method using concatenation
fails with E0202, even when instantiated with String. Diagnostics are recorded
in `source-migration-results.txt`.

The mechanism is declaration-time generic checking in
`validate_generic_templates`, plus the explicit opaque-parameter method rejection
in `check_receiver_method_call`. Existing Array checking grants `.join()` only
to String elements, and `.toString()` only to supported scalar/string elements.
Moving those contracts into ordinary generic source bodies therefore needs a
way to express and check conditional methods, or another deliberate language/API
representation. Simply disabling abstract checking for standard-library code
would create a compiler privilege beyond the requested ABI-only boundary; keeping
those public methods hardcoded would leave the review finding unresolved.

These probes establish the current source-expression limitation; they do not
prove that all source-based designs are impossible. The implementation choice
between declaration-checked constrained methods and concrete-type extension
methods changes the language surface and remains unresolved. No generic-checking
policy was relaxed. The migration is not complete and must not be approved on
the strength of the fixed-array tests.
