# willow-23fv implementation audit — 2026-10-21

## Scope and provenance

Base: `628ad365ce447586927ad5ab0d7d8c0edf2be44e`, initially clean working tree.
Host: AMD Ryzen 7 7800X3D, Linux x86_64; release builds, 8 workers.
The before measurements describe that clean base. After measurements describe
the uncommitted runtime changes in this ticket. The compiler and each of the
three benchmark binaries were rebuilt in this session. No CI was queried.
The ticket remains in progress for review; nothing was staged or committed.

## Cause and change

An automatic minor request that loses collector election queues `minor::PENDING`.
The previous refill loop treated absence of a root-deferral epoch as grounds to
wait for the whole elected cycle, even when there was ample bounded allocation
runway. It then unconditionally requested another minor after waking, including
when the owner had already serviced the request. Multiple waiters could cause
unnecessary collections and promotion.

The refill decision now:

* Stops waiting if minor work is no longer due.
* Captures the existing monotonic young-allocation clock and nursery budget on
  the first queued request, before granting more small-object allocation.
* Grants unassessed runway only without a hard reservation cap, with at least
  one complete TLAB chunk of budget, and with an unsaturated clock.
* Preserves that single epoch through subsequent pending requests, root scans,
  changes to the nursery threshold, and major reclamation.
* Joins the owner when debt is exhausted (or no runway can be granted), then
  rechecks current allocation pressure before retrying collection.

The intermediate recheck-only experiment reduced the median from 344.144 to
152.688 ms but still failed the target. Adding bounded pending-request runway
restored the approximately 100 ms mode. Both intermediate samples are retained
in `measurements.json` and are not used as final acceptance evidence.

## Measurement results

Every trial and GC counter is retained in `measurements.json`. Five trials per
batch, unchanged `gc_scheduler 10000 100`, `--workers 8 --languages willow`.

| Workload | Comparison median ms | Final median ms | Delta |
| --- | ---: | ---: | ---: |
| gc_scheduler | 344.144 (same-session before) | 104.722 (spread rerun) | -69.57% |
| object_churn | 99.107 (2026-10-19 baseline) | 98.242 | -0.87% |
| array_literal_churn | 456.315 (2026-10-19 baseline) | 466.187 | +2.16% |

Same-session before churn medians were 99.357 and 468.401 ms. The acceptance
comparison above uses the ticket's original `MEASUREMENT_2026-10-19.jsonl`;
its selected Willow rows are preserved unchanged in the evidence file.

**Spread and limitation:** the first final-code batch was
93.815 / 95.627 / **178.353** / 92.656 / 96.038 ms. Its median passed, but its
maximum failed the 150 ms criterion. That slow trial ran 1 minor / 2 majors;
the other four ran 0 minors / 1 major. It is not discarded or attributed to OS
noise without evidence. Following the benchmark procedure's >15% spread rule,
the immediate next five-trial benchmark run after local checks was
104.722 / 100.885 / 105.939 / 105.125 / 102.220 ms: median 104.722, maximum
105.939 ms, satisfying both requested thresholds. The measured improvement is
repeatable across both medians, but a universal tail-latency bound is not proven;
review should retain visibility of the earlier slow trial.

| Counter | Before (five trials) | Final spread rerun (five trials) |
| --- | --- | --- |
| minor collections | 11, 12, 12, 11, 5 | 0, 0, 0, 0, 0 |
| major collections | 2, 2, 2, 2, 2 | 1, 1, 1, 1, 1 |
| promoted objects | 693611, 642670, 672575, 731100, 605251 | 0, 0, 0, 0, 0 |
| moved objects | 1131922, 1023673, 1046377, 1109545, 1210496 | 0, 0, 0, 0, 0 |
| total allocation bytes | 55360000 each | 55360000 each |

All benchmark processes exited successfully; the historical unexplained exit 1
did not recur. CPU measurements remain whole-process, not worker-only CPU.

## Correctness and affected call paths

Audited generated-TLAB entry `willow_gc_alloc_slow` and runtime-owned TLAB entry
through `tlab_alloc_slow`, minor election/`PENDING`, owner `service_pending`
boundaries before remark and after sweep, `should_defer_minor`, TLAB byte merging,
and `cycle_wait` election/publication/safepoint participation.

The heap guard is explicitly dropped before entering the coordination wait.
No relocation is added during sweep: existing owner serialization and
`collect_stopped` assertions remain. The initial pending epoch and later
root-deferral epoch share the same captured budget; root scans cannot restart
its clock. Saturation still forces fallback. Hard reservation enforcement,
large-object routing, scoped-loan handling, and collector reentrancy are unchanged.
Only small, non-allocation-stress refills use the new runway policy.

The bound is still one captured nursery budget plus concurrent active TLAB
runway, not a strict byte-by-byte stop: bump allocation happens between refill
checks. Neither occupancy reductions nor threshold growth replenish the budget.
The existing tiny-budget first-request-during-sweep test is unchanged and passes;
it still forbids any refill before publication. The new held-sweep test permits
exactly one 32768-byte budget, observes the next refill sleeping, then verifies
one owner minor, in-place promotion of the parked root, retained data, and no
redundant waiter collection.

## Complexity audit

Independent dimensions: refill attempts F, registered TLABs T, waiting mutators
W, and roots/objects/edges processed by a collection H. Chunk size C is a fixed
runtime constant. The new decision helper has no loop, allocation, root scan,
or object lookup: O(1) time and O(1) extra space per invocation, using existing
state fields. Compiler analysis, emitted allocation code, and ABI are unchanged;
there is no input-dependent generated-code growth.

The existing byte-accounting scan is O(T) per allocation-policy check, hence
O(F*T) across refills. Post-wake revalidation adds an O(T) accounting check per
woken waiter; it avoids unconditional collection work for already-satisfied
requests. This is not a claim that all runtime allocation accounting is O(1).
The scope does not redesign the existing per-TLAB cursor accounting protocol.

For the owner-serviced fan-out case, deterministic tests with W = 1, 4, 16, 64
measure **one** minor collection and W successful refills in every case. Thus
the waiter count no longer multiplies that completed collection's H work. The
predicate's constant work is established by source inspection; collection counts
are measured, not inferred from wall time. Existing overlap tests also exercise
growing graphs, remembered edges, relocation, and marker ownership. Condition
variable waits retain safepoint participation and do not introduce busy polling.
Concurrent fresh pressure may legitimately request another collection; the
one-collection count applies to the measured unchanged-pressure fixture.

## Validation and perspectives

40 unique focused runtime tests passed; one existing opt-in waiter CPU benchmark
was left ignored. Filters: `minor_deferral` (11), `minor::overlap` (10),
`cycle_wait::tests` (10), `nursery::` (7), and the two named TLAB threshold and
large-object tests (1 each). Expected caught-panic diagnostics in saturation and
unwind tests are not test failures. The prior minor-deferral tests were not edited.

Explicit perspectives exercised by those filters:

| Perspective | Evidence |
| --- | --- |
| Fixed root-deferral budget across threshold growth | existing clock-boundary test |
| Young clock around 32-bit boundaries | existing clock-boundary test |
| Saturation and monotonicity invariant | existing saturation test |
| Major reclamation cannot erase debt | existing reclamation test |
| Old allocation does not charge young debt | existing reclamation test |
| Parked-root pin fallback and data retention | existing reclamation and new held-sweep tests |
| Pin release after root release | existing reclamation test |
| Parallel fast/slow TLAB accounting, counted once | existing 1/4/8-worker test |
| Hard cap forces fallback and clears debt | existing hard-cap test |
| Exhausted refill joins collector | existing exhausted-refill test |
| Tiny unassessed first request blocks during sweep | unchanged first-request test |
| Pending budget below/equal/above one chunk | new boundary matrix |
| Pending request under hard cap | new boundary matrix |
| Pending request at saturated clock | new boundary matrix |
| No queued request cannot manufacture an epoch | new boundary test |
| Completed request does not wait on cleared epoch | new boundary test |
| Pending debt survives occupancy/threshold changes | new boundary matrix |
| Real held sweep grants finite runway then blocks | new held-sweep test |
| Owner-serviced request avoids second minor | new held-sweep test |
| Waiter fan-out 1/4/16/64 shares one owner collection | new fan-out test |
| Minor/major overlap preserves rewritten graph edges | overlap tests |
| SATB roots and post-snapshot allocation survive | overlap tests |
| Stop participation, spurious wakes, generation rollover | cycle-wait tests |
| Completion and unwind wake waiters | cycle-wait tests |
| Native collector reentrancy and explicit collection | cycle-wait tests |
| Nursery feedback and stress/override policy | nursery tests |
| Large allocations stay on old-region path | named TLAB large-object test |
| Generated-code/runtime integration with changing mutators | runnable example |

`example/gc_mutator_registration.wi` built in release mode and ran with 8 workers;
output was exactly `ledger` followed by `288`. The three benchmark programs also
exercise compiler-generated TLABs and the actual scheduler/runtime integration.

`cargo fmt --all -- --check` and workspace/all-target Clippy with `-D warnings`
passed for x86_64 Linux, x86_64 Windows MSVC, aarch64 macOS, and x86_64 macOS.
Native tests/execution were performed only on Linux x86_64. Windows and both
macOS native execution results are **unverified**. No full local test suite or
full example audit was run; no CI result was checked.

## Reproduction

From the repository root, `bash docs/audits/willow-23fv/verify.sh checks` runs
the focused validation; `bash docs/audits/willow-23fv/verify.sh benchmarks`
rebuilds the three selected workloads using the existing local
`benches/go_vs_willow` suite, executes the exact five-trial harness commands,
and asserts the original median/maximum/churn thresholds. Run measurements
without concurrent builds. The wrapper does not hide failed trials or retry
automatically. Raw local logs are ignored; the reviewed measurement JSON,
audit, and reproduction script are explicitly allowlisted for later review.
