# Sync interface diagnostic batching audit

Date: 2026-10-26

## Cause and change

The template-checking phase previously built a separate field-write summary
from each hydrated source file. A mutable Array field was rejected there,
while a scalar field written from another file was only rejected by the final
whole-program check. An error in the template phase stopped that final check.
The regression test reproduced exactly one E2406 before the change and two
after it, without fixing either invalid implementation.

The template phase now resolves one build-wide summary from existing body
artifact facts and shares it by Rc with all module checkers and the entry
checker. It keeps the final post-specialization summary: generated declarations
and bodies still require that later view. No diagnostic suppression, continuation
past invalid IR, marker-rule relaxation or code-generation changes were added.

## Implementation and call-path audit

Reviewed run_frontend_revision, typecheck_phase_with_generics,
TypeChecker::check_program_items, check_class_marker_contract,
UnitArtifacts::field_writes and FieldWrites::resolve_programs/Resolver::resolve.
The summary is set before any template checker runs, including the checker
that later handles pending generic instances. Entry and module paths share the
same immutable summary. The final compiler database continues to supply its own
post-expansion summary, and programs without templates retain their old path.
Cached body facts are resolved against all source declarations/import identities;
the new call does not hydrate bodies or repeat AST collection per checker.
Static fields, construction-only writes and unknown-receiver conservatism retain
the existing resolver semantics. Rejection stays in the compiler before backend
execution. Runtime ABI, target behavior and runtime auto-build are unchanged.

## Complexity and measurements

Let U be source units, B body slots, D declaration/import inventory, W recorded
write facts, and R the existing resolver's name-resolution work (including
qualified spelling lengths). The added pre-check summary costs O(B + D + W + R)
expected time with hash indexes and O(D + W) temporary inventory plus name
storage. Inheritance field resolution visits the forest once with scoped field
indexes; it does not multiply writes by ancestor depth. Sharing with U checkers
costs O(U) Rc increments, not U copies or U whole-program scans. The retained
summary stores at most one entry per reassigned field or unresolved field name.
The prior local body scans are bypassed because field_writes is already set.
The final summary is still needed after specialization; eliminating it would
risk missing generated facts. No new generated instructions or runtime work
are introduced. Invalid programs produce O(E) diagnostic output for E violations,
which necessarily requires Omega(E) output work.

Measured deterministic assertions in existing focused tests:

- Deep inheritance with 16, 64, 256 and 4096 writers: exactly 16, 64, 256 and
  4096 field resolutions, and one reassigned root field.
- 100 equivalent method writes: one resolution after deduplication.
- Marker diamonds of size 16, 64, 256: 35, 131, 515 marker visits.
- Repeated contract roots at those sizes: 65, 257, 1025 marker visits.
- The adjacent reproduce.py generates cross-module pairs and calls check once:
  16/64/256 pairs produce exactly 32/128/512 E2406 diagnostics and no other errors.

These are operation/output counts, not wall-clock asymptotic claims. The
resolver and marker algorithms are reused unchanged. The end-to-end count probe
measures completeness and output growth, not all compiler operations or peak RSS.

## Validation

Passed on x86_64 Linux:

- cargo fmt --all -- --check
- cargo clippy --workspace --all-targets -- -D warnings
- cargo test --test integration test_interface_marker_contract -- --nocapture:
  6 tests; includes a 20-case check matrix (template/non-template path,
  entry/separate writer, assignment/compound/alias/lambda/conditional write),
  entry-checker coverage, build-path reproduction, prior Send rejection and
  the runnable example.
- cargo test --lib interface_marker_contract -- --nocapture: 6 tests, including
  the existing 48-case marker matrix, cycles, generic payloads and scaling.
- cargo test --lib semantic::field_writes::tests::deep_chain_resolution_is_linear -- --nocapture:
  1 test.
- cargo test --lib semantic::field_writes::tests::repeated_writes_resolve_once -- --nocapture:
  1 test.
- Focused field-reassignment integration validation: 29 tests covering
  reassignment, inheritance, cross-module/generic writes, static fields,
  constructor escapes, atomics, locks and runnable examples.
- python3 docs/audits/interface-sync-diagnostic-batching/reproduce.py
- git diff --check

The new example/interface_sync_contracts.wi builds and runs, printing 3 and 7
on separate lines. It demonstrates two valid Sync implementations with frozen
collection storage and mutable atomic state, plus generic/interface conversion.

No full local suites or full example audit were run. No CI was checked.
macOS and Windows native execution are unverified. Cross-target Clippy was not
required for this target-independent checker orchestration change; no cfg,
platform API, filesystem operation, threading or runtime implementation changed.

Review correction: replaced an identifier-bearing test command with the
descriptive validation entry above. The exact command is retained only in local
runner notes. Rechecked added diff lines and new public artifacts for internal
identifiers, including underscore-encoded forms; no matches remain. This
correction changes documentation only; the implementation and test evidence above
are unchanged. Formatting and diff checks were repeated for this correction.
