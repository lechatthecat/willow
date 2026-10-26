# Checked semantic boundary

Date: 2026-10-24. Baseline: b9c2f9d. Implementation and audit only; ticket remains
in progress for review. No staging, commit, push, or CI inspection was performed.

## Boundary and required facts

`CheckedUnit` remains the owned, serialized unit result. Its `tables()` now
returns `SemanticFacts`, an immutable borrowed bundle with eight required
references: symbols (including enums), expression types, enum-variant
resolutions, pattern resolutions, block exit facts, normalized annotations,
static targets, and lambda captures. CompilerSession's normal and foreign-default
lowering routes and LirQueries require this type. It has no Default
constructor; per-node completeness is checked while lowering. Declaration-only queries and explicit standalone lowering tests
use `PartialCheckerTables`; they cannot be passed to the production entry points.
A private adapter shares existing lowering operations without copying maps.

Required per-node entries are expression types (including nested operator
nodes), lambda captures (an empty vector is valid), static targets (including
unchanged spelling), and match-block exit facts. Missing entries produce E0890
with the missing fact category. Ordinary E0800 lowering gaps retain their existing
behavior; the new category distinguishes missing facts without matching message
strings. LirQueries propagates the invariant error before
storing executable bodies, even for unused functions; it does not reduce it to
an optional log message or a later missing-body error. Both ordinary and foreign
lowering use the same diagnostic guard.

Enum reinterpretations and pattern reinterpretations remain sparse: no entry
means the parsed expression/pattern was not reinterpreted. Normalized annotation
entries are also sparse memoized rewrites; identity spellings need no rewrite.
These are optional *entries*, not optional production tables. Symbols own enum
declarations, avoiding a separately supplied enum map that could disagree.

Checked types are transcribed onto HIR expressions. Static target maps now hold
interned `TypeId` values, not one owned class-name string per call. HIR already
uses semantic type/target IDs and an owned resolution snapshot; the backend
consumes lowered IR and does not reconstruct missing bodies from AST. Contextual
enum checking, generic enum argument checking, and the zero-payload Result success
return all record the same required static-target fact. Derived wire version 2
invalidates older derived artifacts that had sparse static targets. Typed body
serialization and replay preserve the target IDs through the existing interner.

## Identity and reuse audit

Reviewed parser session allocation, `FreshIds`/`substitute_item`, contextual body
identity registration, and body normalization/replay. Fresh parses have disjoint
expression identities; old facts fail explicitly when passed to the edited
parse. A new regression verifies that generic substitutions allocate disjoint
expression, pattern, and block identities across the template and three
instances. Existing session-identity and injected-default tests also pass.

Unchanged immutable syntax can retain its source ExprId: copied interface
defaults deliberately share canonical checked syntax, while BodyIndex gives
receivers and nested lambdas separate contextual BodyIds. This is reuse of the
same checked source, not assigning its fact to changed syntax. Normalization
preserves those source identities and extends expression facts for newly
introduced ANF temporaries. Generic substitution uses fresh IDs for changed
syntax. Tests cover nested lambdas, defaults, static initializers, generic body
edits, and warm/cold comparison. The new cross-module test combines enum payloads,
a generic function and a static method, then changes and repeats the method body.

## Complexity and memory

Let E be lowered expression occurrences, T the sum of their type sizes, C the
number of capture records, S static-expression sites, U distinct static targets,
and D maximum operator depth. Creating SemanticFacts and its adapter is O(1)
time and space, with no map clone. Required-fact checks use bounded hash lookups
per occurrence (expected O(E)); copying and normalizing authoritative type
payloads adds O(T), not a subtree walk per ancestor. Operator traversal keeps
O(D) explicit work storage. Capture declaration indexing remains once per
lowering view, expected O(C) time/space, shared across its functions and lambdas.
Foreign receiver lowering still emits one body per receiver; the added checks
scale with the actual emitted occurrences, not all source facts per receiver.

Static target entries retain O(S) fixed-size IDs and share target names in the
existing interner (O(U plus unique name bytes)), rather than introducing O(S ×
name-length) retained string payloads. Parsing/resolving each written target
still reads its spelling. The lowering helper's transient spelling conversion
is required by the existing source-name environment; it does not retain a new
per-call string in the semantic table. No new runtime operations or cleanup
paths are generated; output size is proportional to emitted HIR as before.

Deterministic measurements from `checked-semantic-boundary-measurements.txt`:

| Functions | Expression facts | Required type reads | Entry capacity | Entry storage bytes | HIR functions |
|---:|---:|---:|---:|---:|---:|
| 16 | 48 | 48 | 56 | 3136 | 16 |
| 64 | 192 | 192 | 224 | 12544 | 64 |
| 256 | 768 | 768 | 896 | 50176 | 256 |

The borrowed SemanticFacts bundle measures 64 bytes on x86_64 Linux at every
size. Scalar facts have no nested heap payload. Entry storage is measured as
map capacity × size_of(key/value), excluding hash control bytes, allocator
rounding, symbols, and interner storage; these figures are not whole-process
RSS or total compiler heap. The private adapter and its capture index are
additional to the 64-byte bundle. This change does not claim an overall heap
reduction over the baseline.

For operator depths 16/64/256, expression facts are 33/129/513 and required type
reads are 33/129/513. For 16/64/256 repeated static calls, exactly one target
identity is retained; target entry capacities are 28/112/448 and entry storage
448/1792/7168 bytes. Thus measured lookups, fact counts and generated-function
counts grow linearly for deep chains, function fan-out and repeated targets.
Each expression type is read once by the required-fact check, including the
operator root. Timings are incidental and are not used as asymptotic proof.

## Validation

The reproducer is `docs/audits/checked-semantic-boundary-reproduce.sh`; it runs focused tests
and the runnable `example/semantic_facts/main.wi` (expected output `42`).

The 22 explicit behavior perspectives exercise integer, float, boolean, string,
variable, nested arithmetic, negation, logical not, comparison, logical binary,
array/index, local binding, if/return, while/assignment, array iteration, range
iteration, free call, lambda, enum construction, enum match, class construction/
field access and static call. Each also checks rejection with expression facts
removed. Additional tests delete individual nested-operator entries, capture
facts, static targets and match exits; test stale facts after reparse; and check
propagation at the compiler boundary.

Focused validation also includes the existing 83 `ir::lower::tests::p*` cases,
`module_checker_tables::every_resolution_kind_answers_together`,
`module_checker_tables::an_async_module_body_matches_its_own_enum`,
`revision_generic_instances_bind_template_syntax`,
`revision_lambda_edit_in_generic_instantiation_keeps_effect_bindings`,
`revision_preserves_defaults_constructors_statics_and_nested_lambdas`,
`normalized_injected_defaults_reuse_canonical_typed_artifact`,
`normalized_body_queries_preserve_nested_lambda_await_ids_and_checked_types`,
`session_identity_matrix`,
`instantiated_defaults_and_nested_lambdas_keep_contextual_identity`, and
`derived_typed_wire_ownership_schema_and_green_hits`.

Final validation passed: `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, and the same Clippy
command with each of `--target x86_64-unknown-linux-gnu`,
`--target x86_64-pc-windows-msvc`, `--target aarch64-apple-darwin`, and
`--target x86_64-apple-darwin`. All 14 `semantic_facts_` regression/measurement
tests passed. Native tests and example execution run only on Linux x86_64. Windows, macOS arm64 and macOS x86_64 native execution were not performed.
No full workspace tests, whole integration suite, full example audit or CI
polling was performed. No runtime, ABI, linker, or build scheduling changes.


## Review repair: reference-place facts

The reviewer regression was reproduced before the repair:
`ir::lowered::value::lower::tests::array_reference_owner_is_captured_before_later_argument_cfg`
failed at `src/ir/lowered/value/lower.rs:1943` with E0890, "missing required
semantic fact: expression type". `reference_place_info` directly resolved
variable and index places, bypassing `check_expr`, but only the field branch
stored the resulting expression type.

The repair adds the resolved variable type and successful index element type to
`expr_types` at that existing checker boundary. It does not call `check_expr`
again, re-evaluate index expressions, alter borrow/mutability/alias validation,
or relax strict lowering. The existing field path and invalid-place diagnostics
are unchanged. Both ordinary reference checking and generic reference inference
use this helper; the generic path still reuses the checked place rather than
rechecking its index/call subtree. Body result serialization already carries
these expression entries, so no new storage representation or runtime ABI is
introduced by the repair.

Added 24 focused perspectives: shared/mutable mode × local variable/reference
parameter/field/array-index place × i64/bool/String. Each asserts the exact
checker type, lowers HIR and executable source IR, then deletes that one fact
and confirms E0890 still rejects the incomplete input. The existing regression
also proves array-owner capture remains before later argument control flow.

Review-round focused validation passed: all 14 `semantic_facts_` tests,
the named array-owner regression, 73 `unit_reference_` cases, and
`nested_reference_inference_checks_each_generic_call_once`. The new native
`example/semantic_facts/references.wi` prints `1`, `2`, `9`, `9`, checking shared
reads and mutable writes through local and array-element references. The
previous section's other integration/83-case lowering validations were performed
in the original implementation round, rather than rerunning those unaffected
cases for this repair. The reproducer now includes the reference checks/example.

For R reference-place visits with type payload sizes t_i, the added work is one
expected-O(1) map insertion and one O(t_i) type clone per visit. Additional
storage is O(R + sum(t_i)) for distinct reference expressions. There is no extra
AST walk, caller-wide scan, nested re-inference, or generated instruction. This
is the required per-expression fact storage; no new repeated computation was
introduced. With R=32/128/512 references, measured total expression facts are
99/387/1539 (exactly 3R+3), map entry capacities are 112/448/1792, and entry
storage is 6272/25088/100352 bytes under the same capacity-based measurement
limits described above. These cases repeat variable/index references, so they
also verify fact storage grows with source occurrences rather than repeated
whole-function scans. Types and addresses in generated IR remain the previously
resolved values; the array-owner regression and native output provide the
runtime/order evidence.
