# Pacer implementation audit — 2026-10-24

Ticket: willow-6fv.5.8.3. Implementation/review handoff only; ticket remains open.
This report describes the source and locally measured behavior, not private specifications.

## Implementation and policy

`gc/pacer.rs` separates integer input/decision calculations from sampling and
publication. The existing immediate-rise allocation estimator and immediate-fall
mark estimator remain; improvements/decay use a 3:1 EWMA. Goals change by at most
2x per completed cycle, except that actual live bytes are always a lower bound.
Allocation samples retain the current goal, raising it only to cover live bytes.
The trigger retains proportional allocation progress (at least a quarter of the
live set, or 256 KiB) to avoid repeatedly scanning an unchanged heap. A predicted
late start obeys that progress floor; pressure and failed reservation retain their
existing immediate collection paths. No memory-limit clamping moved into Pacer.

Inputs include retained/occupied bytes, cumulative allocation demand, previous
root/graph work, barrier-call growth charged as eight-byte slots, previous pause
maximum, previous precollection occupancy, mark bytes per CPU second, available
CPU capacity and configured marker count. Barrier growth is a conservative work
estimate, not a count of new reachable objects. The cycle group uses the existing
V1 telemetry fields. The CPU producer uses measured complete-cycle CPU with nested
minor and collector mark overlap removed; the fixed eight-top-level-interval window is
separate from the preserved C snapshot layouts. Eight subsequent valid top-level intervals
remove a missing/old CPU sample. RSS and committed validity remain explicitly
unavailable to this cheap sampling path; they are not silently interpreted as zero.

The background target is 250,000 ppm of available CPUs. Full workers plus at most
one fractional worker are selected by index, capped by the configured pool. Each
bounded drain repays measured CPU against elapsed wall time. Missing clocks charge
elapsed wall time. Waiting holds no heap/queue lock and polls safepoints in at most
100 us requested sleep slices; OS scheduling is not a hard latency guarantee.
No opportunistic work is admitted, and its CPU counter stays explicitly zero.

CPU fractions above 250,000/500,000 ppm have distinct reason bits; normal excess
also increases the desired goal. **This is not a global CPU cap.** Root coordination,
collector closure, sweep and mutator assistance retain their own scheduling. Every
runtime publication explicitly carries `UNCONTROLLED_PHASES` (256), as permitted
by the ticket's enforce-or-explicit-fallback acceptance criterion. Missing memory
observations have bit 512. Non-background CPU cannot be represented as free work.
No new global stop or all-mutator allocation gate was introduced by pacing.

Decisions also expose expected work, residual assist demand in work-bytes per
allocation-byte scaled by 1,000,000, goal/trigger/runway, worker count and fractional
duty, goal/8 overshoot budget, immediate-start status and reason flags. Assist
execution is unchanged: its separate controller still uses the existing fixed
execution ratio. The decision's residual demand is a prediction, not proof that
mutator assist execution has been reduced. Pressure/failure collection and GC
correctness remain owned by their existing call paths.

`WILLOW_GC_PACER=0` keeps the fixed completed-cycle goal `max(1 MiB, 2*live)`, a
trigger at that goal and unrestricted configured background workers. Existing
soft/hard memory triggers remain effective. The disabled decision reports the
existing one-work-byte-per-allocation-byte assist policy for comparison.

`gc_telemetry::pacing::snapshot()` publishes a coherent Rust version-1 diagnostic
group. V1–V4 C layouts are unchanged. CPU/window updates, worker charges and
controller decisions share one short mutex; no acquisition below this mutex
re-enters the heap. Sampling under the heap mutex uses a cycle-only telemetry
snapshot, avoiding recursive heap sampling. Worker job setup takes the heap mutex
before publishing to the pool; workers never take the pool mutex while holding
the heap mutex. Collection and failure paths were inspected for these lock orders.

## Test perspectives

Focused tests cover these distinct perspectives:

1. Formula and CPU capacity scaling at 1, 2, 5 and 64 CPUs.
2. Configured worker capacity lower than the normal target; zero workers.
3. One fractional worker and no optional idle borrowing.
4. Measured CPU repayment and credit for existing wall-time delays.
5. Missing worker clocks use conservative wall charging.
6. Saturated repayment arithmetic.
7. Normal CPU boundary, boundary+1, hard boundary and boundary+1.
8. Missing CPU and zero observation window.
9. Eight-cycle removal of stale/missing CPU history and step replacement.
10. CPU-window sum saturation is invalid, never a small wrapped value.
11. Allocation spikes react immediately.
12. Allocation decay and mark improvements converge over 20 samples.
13. Mark slowdowns react immediately.
14. Zero-work/zero-CPU mark samples discard the rate.
15. Equal/regressing clocks and allocation-counter regression discard rates.
16. Stale windows discard old mark rates.
17. Allocation burst subdivision does not change the sampling estimate.
18. Collection pauses do not dilute mutator allocation demand.
19. Active goals do not shrink; retained bytes remain a lower bound.
20. Fixed-current-goal outputs agree on runway, trigger, assist and overshoot.
21. Root, barrier and pause growth increase predicted runway.
22. Pressure, failure, goal crossing and predicted-late decisions.
23. Independent extreme live/work/allocation/CPU dimensions preserve bounds.
24. Full-width arithmetic retains the denominator's precision.
25. Proportional progress under saturated runway avoids repeated full scans.
26. Concurrent decision publication cannot tear related fields.
27. Disabled pacing retains soft-memory collection and fixed thresholds.
28. Native occupancy/allocation accounting feeds pacing.
29. Worker shutdown, spurious wakeups, timeout and fatal-worker policy.
30. Multi-mutator collection and live buffers through closure/pending sweep.

## Complexity audit

Let A be allocation samples, C completed cycles, B bounded background drains,
P configured workers, and H the live graph's objects/slots. The new pure controller
has a fixed number of saturating arithmetic operations and no heap allocation,
recursion, graph walk or input-sized loop: O(1) time/storage per decision. The
CPU ring retains exactly eight top-level intervals and examines at most eight starts on
completion, O(1) storage and time independent of C. Publication is one fixed-size
copy. Sampling adds O(A), CPU history O(C), and worker accounting O(B) work.
Existing worker wakeup remains O(P) per job and graph tracing remains dependent
on H; no new O(H*P), rescanning or generated-code expansion was introduced.

Waiting polls proportional to the requested debt divided by the 100 us sleep
slice; this is a deliberate responsiveness/time tradeoff, not a graph-dependent
scan. Queue consumers publish before waiting; debt repayment retains termination
ownership until that worker acknowledges completion. CPU-clock/syscall and mutex
costs are bounded per sample/drain, not per graph edge. Controller instructions
are runtime code only; compiler analysis, generated code and build scheduling are
unchanged. The deterministic loop bound (eight history entries, no input loop in
`decide`) establishes scaling; timings alone are not used as asymptotic evidence.
The benchmark varies pure-decision repetitions 10k/100k/1m and allocation sizes
64/512/4096 bytes. Existing graph topology algorithms are unchanged.

## Validation and measurement limits

Local checks: fmt; workspace/all-target Clippy for Linux x86-64, Windows MSVC
x86-64, macOS arm64 and macOS x86-64; 22 matching `pacer` tests; one CPU-window
test; four worker termination tests; multi-mutator deadlock and live-buffer
closure/sweep regression tests. The reference measurement is a separately,
explicitly invoked ignored test, three repetitions in release mode.

Native execution was on Linux x86-64 only. Other OS/architecture execution and
CI are unverified; no CI was checked. Full workspace tests and the full example
audit were not run. These omissions follow the requested local validation scope.

`measure.py` reproduces the measurements and records the exact runtime source
hashes. `measurements.json` contains only public numerical evidence, tool version
and platform identity. Throughput includes measurement work; occupancy is sampled
once per 64 allocations and overshoot is therefore a sampled lower bound, not a
proven peak upper bound. Pause is the observed maximum, not p99. CPU ratios require
dividing GC CPU by elapsed wall time and available CPU count. Actual assist counts
are zero for this single-mutator workload, so only predicted demand is compared.
These are reference-host targets/observations, not new acceptance gates.

## Reference-host results (medians of three runs)

| Payload bytes | Pacer | Allocations/s | GC CPU / host capacity | Sampled overshoot bytes | Maximum pause us | Predicted assist ppm |
|---:|:---:|---:|---:|---:|---:|---:|
| 64 | off | 3,674,994 | 2.52% | 0 | 65.07 | 1000000 |
| 64 | on | 4,494,352 | 2.68% | 0 | 53.14 | 10849 |
| 512 | off | 3,807,143 | 2.98% | 15872 | 57.68 | 1000000 |
| 512 | on | 2,792,917 | 2.18% | 7424 | 60.27 | 69938 |
| 4096 | off | 824,069 | 3.65% | 4096 | 61.88 | 1000000 |
| 4096 | on | 403,252 | 1.78% | 0 | 67.16 | 1001946 |

The pure model measured approximately 42–48 ns/decision at 100k/1m repetitions.
Pacing lowered sampled overshoot in the 512/4096-byte cases and predicted assist
demand in the 64/512-byte cases. It did not universally improve throughput or
assist demand: 512/4096-byte throughput was lower with pacing, and the 4096-byte
predicted assist demand slightly exceeded the fixed ratio. These costs remain
visible in the report; the measurements do not establish a universal improvement.
All samples reported complete GC CPU measurements. No statistical significance
or end-to-end concurrent-assist reduction is claimed.

## Runnable example

Built the current `willow` compiler and `willow_runtime`, then ran
`target/debug/willow run example/gc_old_cycle.wi --runtime-lib target/debug/libwillow_runtime.a`
with `WILLOW_GC_PACER=1` and `0`. Both printed `499500` three times and exited
successfully. An initial attempt using the obsolete local `willowc` binary failed
to link its removed ABI symbol; rebuilding/using the current `willow` executable
resolved the artifact mismatch. No compiler or ABI source change was needed.

## Review repair: nested minor CPU retention

The previous completion ring could evict a nested minor before the enclosing
major completed. The major CPU producer subtracts all nested minor CPU, so adding
only the remaining ring entries undercounted CPU while retaining the major's full
wall interval. An early missing sample could likewise be forgotten prematurely.

The ring now receives non-overlapping top-level intervals. Major begin opens a
constant-size accumulator; each nested minor contributes its exclusive CPU and
validity to that accumulator, without inserting or evicting a ring entry. Major
completion adds exclusive major CPU and publishes the complete group as one
entry. Standalone minors remain individual entries. The lifetime CPU counters
still add each exclusive contribution exactly once. During an active major the
snapshot marks the previous completed window invalid instead of claiming that
partial major accounting is complete. Aborted nested cycles invalidate their
whole group; an aborted major closes one invalid entry and releases the accumulator.
Overflow in grouped CPU also invalidates the entry. These validity flags reach
`PacerInputs` as unavailable CPU and produce `CPU_MISSING`.

Regression evidence: nine nested minors with six CPU units each plus one exclusive
major CPU unit over 100 wall units now publish **55**, not 43, and produce both
`CPU_NORMAL` and `CPU_HARD` on one CPU. Missing samples at the first or ninth
minor produce `CPU_MISSING` instead of a valid low utilization. Independent state
tests exercise 9, 64 and 1024 nested minors, early missing samples, aborted minor
and major intervals, overflow, and eviction after eight complete top-level
intervals. They assert zero ring insertions during nested completion and one
insertion for the enclosing group. No list of nested cycles or retrospective
scan is introduced: each nested completion adds O(1) work, storage stays O(1),
and one group insertion examines at most eight retained starts.

Revalidation for this repair: 23 `pacer`-matching tests and three pacing-window
tests; the named aborted-cycle, nested-minor phase/epoch, and native-frame/marker
minor-overlap tests. Formatting and all four cross-target Clippy checks were
rerun. Release measurements were regenerated against the repaired source hashes.
The runnable example results above belong to the initial implementation; this
repair was validated through the affected runtime/telemetry tests. Native execution
remains Linux x86-64 only; no CI, foreign native run or full local suite was used.

## Review repair: completed CPU feedback before next-goal selection

The previous runtime order selected the next goal while the current major's CPU
interval was still active. That necessarily supplied `CPU_MISSING`; allocation
samples could not compensate because their goals are fixed. The collector now
finishes the cycle's CPU/telemetry publication before reacquiring the heap mutex
and selecting the next goal. Allocation-sampling resumption, hard-pressure rearm,
soft-memory completion and collection-progress publication follow that decision.
Current retained/work samples are still captured at the end of collection.

`finish_concurrent` remains outside the heap lock because it can perform trace
I/O. Telemetry and pacing guards are released before heap reacquisition. The
collector election remains held throughout; its release (and therefore the
quiescent milestone) still follows all policy updates. This does not add a global
stop or reorder reclamation. After the unlocked publication interval the runtime
refreshes TLAB byte accounting so memory policy sees allocations made meanwhile.

The runtime regression was run before the reorder and failed with reason bits
`2` (`CPU_MISSING`) instead of `0` for a valid zero-CPU sample. After the fix it
passes 30 real collection cycles: ten each with a zero, high, or unavailable CPU
measurement. A scoped thread-local test hook replaces only the completed measured
CPU value; it does not call the controller directly, bypass collection, or change
completion order. High measurements produce `CPU_NORMAL | CPU_HARD` and an actual
heap threshold of 2 MiB rather than 1 MiB. Zero measurements clear those flags and
retain 1 MiB; missing measurements retain `CPU_MISSING` and the conservative 1 MiB
goal. Each scenario exceeds the eight-interval history length. The hook is absent
from non-test builds and restores its previous value on unwinding.

Complexity remains O(1) for the decision and bounded CPU history. The reorder adds
one heap-lock acquisition and a fresh byte-accounting pass after the unlocked
telemetry boundary. This is O(T + L) for T retained TLABs and L registered old LABs,
independent of object graph size; it does not repeat graph tracing. The earlier pass supplies the completed
heap sample, while the later pass accounts for allocations made during publication
and cannot safely reuse that earlier sample. Overall collection accounting stays
O(T + L), with two distinct sampling instants. No new allocation, nested graph loop,
compiler pass or generated-code change was introduced by this repair.

Validation for the completion-order repair: fmt passed; workspace/all-target Clippy
passed for x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc,
aarch64-apple-darwin and x86_64-apple-darwin. All 32 focused tests passed:
24 matching `pacer`, three pacing-window tests, and the five named live-buffer
closure/sweep, nested-minor phase/epoch, multi-mutator deadlock, aborted-cycle and
major-milestone ordering tests. The pre-fix failure and post-fix success of the
new runtime regression establish that it detects the reported ordering defect.
The release benchmark ran three times; all 27 rows and the table above were
refreshed and their source hashes checked. Native execution was Linux x86-64 only.
No CI, full suite, full example audit or foreign native execution was performed.
