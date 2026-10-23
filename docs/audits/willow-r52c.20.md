# willow-r52c.20: default-body sharing evaluation

Date: 2026-10-24. Source baseline: `8f68d34`. This is an evidence-only change:
production compiler behavior and representation are unchanged. The ticket remains
in progress for review, as requested. The earlier archive named in the ticket
was absent; these are fresh measurements of current source, not recovered work.

## Decision

Use the acceptance criterion's **no production change** alternative. Retained AST
copies and receiver-specialized LIR are real linear-in-fan-out costs; this report
does not claim they are optimal or that sharing cannot help. A shared instantiated
body is promising, but the measurements do not establish an end-to-end compiler
memory win from that representation. The existing canonical typed artifact already
eliminates the presumed repeated typecheck for non-generic defaults. Changing only
AST ownership would not eliminate duplicate LIR, while changing executable body
identity also needs receiver substitution, dispatch and artifact ownership work.
Do not implement source-BodyId-based executable deduplication on this evidence.

This decision accepts the measured remaining cost rather than disguising it as a
fix. No memory or generated-code reduction is claimed. A future implementation
would need a measured benefit in both dimensions, per the ticket's changed-code
acceptance alternative. No follow-up issue was created.

## Reproduction and measurements

Run from the repository root:

```sh
rtk proxy cargo test -p willow --lib default_body_clone_measurements -- --nocapture
rtk proxy bash docs/audits/willow-s71l-generic-foreign-defaults-counts.sh target/debug/willow
rtk proxy target/debug/willow run example/default_body_fanout.wi
```

Build `target/debug/willow` first with `rtk cargo build -p willow --bin willow` if
needed. The integration-test runs in this session built the current compiler.
The generator and assertions are in `src/compiler_db/body.rs`'s test module;
the pre-existing generic generator is the script above. Raw outputs are adjacent
in `willow-r52c.20-measurements.txt` and `willow-r52c.20-generic-measurements.txt`.
These inputs are independently constructed public test programs.

The new harness crosses receiver counts C = 1, 16, 64, body statements
B = 1, 16, 64, and constant vs interface-call returns (18 explicit perspectives).
It checks every copy's distinct semantic BodyId, canonical typed-record equality,
exact AST traversal count, total actual typechecks, and zero typed computations
on repeated demand. It lowers through HIR to final LIR and compares serialized
functions, preserving spans, locals, instructions, calls and types.

| C | B | Return | Retained copied AST events | Actual typechecks | Default typed records | LIR bodies | LIR JSON bytes |
|---:|---:|---|---:|---:|---:|---:|---:|
| 1 | 64 | constant | 327 | 3 | 1 | 1 | 20,428 |
| 16 | 64 | constant | 5,232 | 18 | 1 | 16 | 326,866 |
| 64 | 64 | constant | 20,928 | 66 | 1 | 64 | 1,307,554 |
| 64 | 64 | self call | 21,120 | 66 | 1 | 64 | 1,380,666 |

The C+2 checks are C explicitly implemented `seed` methods, one main, and one
canonical default. Repeating queries adds zero computations. AST events count
enter/exit events, not heap allocations or unique AST nodes. Copied event totals
exclude the source body and transient snapshots; they measure retained clone
volume, a lower bound on total cloning. JSON size is a representation-size proxy,
not heap RSS, artifact-store bytes, machine code size or peak compiler memory.

Erasing only the exported function name leaves C distinct LIR shapes (0% duplicate
fraction). Additionally renaming exact receiver-name string values to `C0` leaves
one shape in every case: duplicate fraction (C-1)/C, or 98.4375% at C=64.
This is a restricted structural isomorphism metric, not proof that machine-code
bodies may be merged. The transformation may rename symbol owners as well as
receiver types. Receiver-specific dispatch bindings must survive an instantiated
view; identical shapes alone cannot erase those bindings. Width one has no pair
and hence no duplicate. The dynamic execution example uses different `seed`
results precisely to exercise those bindings, and prints the expected `792`.

The existing generic generator exercises two concrete type arguments, two receiver
modules, defaults containing calls/lambdas, and interface dispatch. For N=1,2,4,8,
16,32 receivers per module it reports 4N default LIR bodies and 2N lambda bodies.
At N=32: 128 default bodies, 64 lambda bodies, 203 typed-body query computations,
261 LIR-body computations, three checked units. Each generated program executes
and its output is asserted. `typed_body` here counts query evaluations, including
aliases, **not actual typechecker invocations**; do not compare it directly with
the new harness's actual typecheck counter. This complements, rather than extends,
the narrow non-generic typed-record assertion.

## Implementation audit and complexity

Reviewed `IfaceComposition::compose/methods`, default injection, `BodyIndex`
source/owner identity, `BodyQueries::typed_record`, HIR receiver handling and
`LirQueries::evaluate/lower_foreign`.

* Composition memoizes borrowed method summaries; `methods()` clones the selected
  declarations. Snapshot/default maps and final class injection still own cloned
  bodies. For C independent recipients, M defaults and B body size, retained
  default syntax costs O(CMB); snapshot costs add their own interface/declaration
  dimensions. Counts above establish the C and B product independently, not by
  wall-clock inference. These figures do not measure every snapshot clone.
* Semantic owner identity is separate from source identity. Typed aliases reuse
  a canonical artifact; copying syntax does not imply C repeated typechecks.
  Generic instances and foreign provider scope are additional independent keys.
* LIR inventory and artifact storage are per semantic body. Lowering walks copied
  bodies and retains receiver-specific symbols/types. Current code/data volume
  for these fixtures is O(CMB), even when a symbolic template would be O(MB+CM).
  O(MB+CM) is a feasible representation target, not a proven lower bound on full
  compilation: dispatch, inheritance, closures and substitutions add dimensions.
* A `DefaultMethodBody` plus instantiated view could retain source syntax once,
  with O(CM) owner/substitution views. Mutable body-ID assignment, normalization,
  generic substitution, nested lambda identities and receiver-dependent HIR/LIR
  would need to consume that view without materializing O(CMB) copies downstream.
  Sharing AST alone is therefore insufficient for the ticket's paired memory/LIR
  reduction requirement. Sharing compiled functions by source ID is unsound;
  retaining receiver-specific wrappers or bindings is necessary.
* No production complexity was introduced. The measurement harness indexes LIR
  functions by FunctionId once, avoiding a per-receiver scan. Its traversal and
  serialization cost is linear in measured syntax/LIR size; hash-set shape storage
  is proportional to that size. The receiver renamer walks the small JSON tree
  once. The fixture is intentionally bounded and does not claim arbitrary-depth
  JSON robustness. Build scheduling/runtime auto-build remain untouched.

The measured duplication is acknowledged existing representation cost, not an
introduced regression. No unrelated architecture changes or new tickets are
needed for this evidence-only disposition.

## Validation

Passed on x86_64 Linux:

* `cargo fmt --all -- --check` and workspace/all-targets Clippy with `-D warnings`.
* `default_body_clone_measurements`: one test, 18 scaling/payload perspectives.
* Integration filter `default_method_`: 10 tests, including default execution,
  override priority, interface dispatch, default-to-default calls, required-method
  diagnostics and missing-self diagnostics.
* Individually selected `default_inheritance_11_generic_interface_default`,
  `default_inheritance_14_ambiguous_defaults_are_still_rejected`,
  `default_inheritance_02_the_default_reaches_the_subclass_override`,
  `super_coercion_17_diamond_shared_root`, and
  `fan_in_receivers_share_one_provider`: five passed.
* Unit tests `self_signature_default_is_typed_once_for_every_receiver` and
  `repeated_diamonds_have_linear_summary_work`: two passed.
* Six generic scaling programs and the sixteen-receiver example executed with
  asserted/checked expected results. This supplies more than twenty explicit
  perspectives without running the full integration or example suites.

Full local suites and the full example audit were omitted by policy. CI was not
checked. Windows and both macOS architectures were not executed or cross-checked;
no platform conditional, OS API, runtime or production code changed, so additional
cross-target Clippy was not applicable to this test/documentation-only change.

Limitations: no allocator instrumentation or RSS measurement, no whole-project
workload distribution, no generic LIR-isomorphism assertion, and no experimental
shared-view implementation. The evidence quantifies retained copies and potential
symbolic sharing; it does not prove whole-compiler savings or universal equivalence.
