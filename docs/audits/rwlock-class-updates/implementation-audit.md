# RwLock class updates implementation audit

Date: 2026-10-21. Scope: retain the class-field Sync rule and make the RwLock
failure actionable. Decision: [documented decision](decision.md).

## Implementation and affected paths

The sole production-code edit is the static help text selected in
`TypeChecker::marker_holds_with_reason` for RwLock/BlockingRwCell failure.
Both async argument capture and retained async-frame diagnostics use this
reason traversal. No predicates, traversal, allocations, field summary, IR,
lock lowering or runtime instructions were added or changed. Existing
field-declaration labels and failure notes are preserved.

Inspected `named_marker_holds`, `marker_failure_with_field`, wrapper marker
checks, `field_writes.rs`, and `check_lock_stmt`. The latter defines a binding
of the protected type and scopes its name, without invalidating references
copied out of it. The new executable example checks that an escaped read
reference retains value 1 after whole-value replacement publishes value 2.
Mutex field mutation independently produces 2. A negative test confirms that
whole-value replacement cannot hide another method's field writes.

Initial example validation used the same field name in both classes and was
conservatively rejected by the existing unresolved-receiver field-name rule.
The final example uses different names, consistent with that documented rule;
no change to field-write inference is claimed.

## Complexity audit

The changed operation selects a static string in an existing failure arm:
O(1) incremental selection time and memory per visited failing wrapper, with
no new scan across fields, methods, modules, exits, or inheritance. Diagnostic
rendering gains a fixed number of bytes per emitted diagnostic, hence O(D)
additional output for D diagnostics, the necessary lower bound for displaying
that guidance D times. Accepted programs have identical generated code and
runtime work by source inspection (not a binary comparison).

The existing deterministic reason-walk tests exercise depths/fan-outs 16, 64,
256 and four repeated requests, asserting visits of depth + 2 and
4 * (fanout + 2), respectively. They pass unchanged. These counts validate the
existing traversal, not a claim that all compiler passes are globally linear.
The text edit introduces no input-dependent traversal; no runtime speedup or
whole-compiler complexity improvement is claimed.

## Focused perspectives

The 38-test field-reassignment group covers these explicit perspectives:

1. Ordinary shared class mutation is rejected.
2. The field declaration is labeled.
3. Construction-only fields remain Sync.
4. Method compound assignments count as writes.
5. Free-function field writes count.
6. Inherited field writes count.
7. Reference arguments count as writes.
8. Nested object writes propagate non-Sync.
9. Constructor lambda writes count.
10. Non-writing constructor lambdas remain accepted.
11. Atomic, lock and frozen fields remain accepted.
12. Reassigning a Mutex field is rejected.
13. RwLock of a mutable class is rejected.
14. Mutex of a Send mutable class is accepted.
15. Explicit Sync interface contracts remain enforced.
16. Generic class writes remain covered.
17. Static fields do not alter instance Sync.
18. Resolved same-named fields remain distinct.
19. Unknown receivers retain conservative behavior.
20. Synchronous calls retain existing behavior.
21. Cross-module writes are recognized.
22. Imported immutable classes run.
23. Existing concurrent Mutex counter gives the exact total.
24. Exclusive binding simple and compound field writes name both remedies.
25. Replacement does not conceal unrelated field mutation.
26. Read references can outlive the read section.
27. Replacement preserves an old snapshot and publishes the new value.
28. The new Mutex alternative runs and produces the expected value.

## Validation

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- Cross-target Clippy with `--workspace --all-targets --target TARGET -- -D warnings`: passed for
  x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc, aarch64-apple-darwin,
  and x86_64-apple-darwin.
- `cargo test --test integration concurrency::field_reassignment_sync -- --nocapture`:
  38 passed, 5993 filtered out; includes the new runnable example.
- `cargo test --lib semantic::type_checker::send_sync::tests -- --nocapture`:
  24 passed, 5016 filtered out, including deterministic reason-walk counts.

Native execution was Linux x86_64 only. Full local suites and the full example
audit were intentionally omitted. CI was not queried. Foreign native execution
is unverified and is not required by the current ticket-loop policy.

Ticket remains open for review as explicitly requested. No staging, commit,
push, or Beads mirror export is part of this implementation turn.
