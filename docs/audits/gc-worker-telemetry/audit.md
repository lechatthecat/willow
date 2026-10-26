# M3 implementation audit (gc-worker-telemetry)

Status: implementation and evidence prepared for review; ticket remains in progress.
No staging, commit, push, CI inspection or Beads mirror operation is authorized in
this implementation turn. The measurements below are local Linux evidence.

## Changes and responsibility boundaries

`gc_telemetry::m3` adds opt-in (`WILLOW_GC_PROFILE=1`) fixed-size nanosecond
histograms for SATB registry acquisition, private-buffer acquisition, stopped
seal slices, concurrent sweep heap acquisition and heap holds. A collision is an
actual `try_lock` WouldBlock, not an inferred slow acquisition. Acquisition
measurement ends before the statistics mutex is acquired. Observer overhead
still perturbs contention: profiled values must not be interpreted as an
uninstrumented latency guarantee. Histogram percentiles are power-of-two upper
bounds, not interpolated exact percentiles. Counts saturate; extreme saturated
histograms are not useful for inference. No trace history or unbounded thread map.

Worker profiles are per pool slot (0..63), accumulated at job boundaries. Objects,
reference bytes, duplicate discoveries, queue outstanding-depth samples, steal
attempts/successes, CPU validity/time, wall and between-job idle time are separate.
The graph path updates thread-local state once per batch; it does not take a
profiling mutex per object. Queue depth is sampled, not an exact high-water mark.
Duplicate discoveries are **not CAS failures**. Mark bitmap claims use atomic fetch-or, not CAS; rejected claims are included
in duplicate discoveries rather than mislabeled CAS failures. The current work-queue slot
allocator uses a mutex-protected free list; it does not use a CAS retry loop.
The profiler leaves the versioned C snapshot layouts unchanged.

`gc::opportunity` is an explicit experimental request API, not an automatically
enabled scheduler policy. It distinguishes emergency, required, opportunity and
background requests. Required/emergency requests bypass utilization entirely.
Optional work needs a current version-1 CPU window with both whole-process busy
and whole-system idle values, plus no queued Willow work. Queue emptiness alone
never establishes CPU idleness. A single CAS reserves at most one optional request
per second; losing callers defer, rather than spin. Existing coordinator request
coalescing/election recovery and hard allocation reservation enforcement remain
responsible for collection and safety. The scheduler returns an owned advisory
snapshot before any GC lock acquisition. There is no scheduler/GC lock nesting.
`CpuWindowV1` timestamps use the clock exposed by `scheduler::gc_metrics_v1`.

The optional prototype does not change OS thread priorities or preempt an active
cycle. Its four classes are request admission classes: once admitted, the existing
bounded GC work and pacing finish the cycle. The Linux runnable probe supplies
measured /proc windows; missing OS measurements fail closed. No automatic native
CPU sampler is installed on other operating systems. Process CPU includes every
thread, including assist. Existing GC accounting separately debits mark/assist
and sweep-assist CPU (`gc_telemetry.rs`, cycle completion).

Release rendezvous now uses the existing 120-second diagnostic bound by default,
matching debug behavior. The existing explicit timeout setting (including zero
for an explicitly requested indefinite wait) remains supported. Timeout reports
identify the missing mutator thread IDs and abort; the collector never reclaims
objects on an unproven root snapshot.

## Termination decision

The original standalone condition-variable timing did not measure collector
termination and is superseded. The new `m3_real_closure_last_work_latency_and_cpu`
fixture creates a real cycle and unpublished mark batch, enters closure while the
reader is active, then releases its final accounting and owner. The timestamp
immediately before final owner release is compared with closure return, including
the stopped recheck and sweep-plan preparation. Each process measures 100 samples
with four release offsets, and only closure thread CPU (no spawn/join CPU). Five
release processes are retained as `closure-process-*.txt`. This controlled tail
fixture measures the real `finish_with_wait` implementation through its shared
policy function, not a generic standalone wake primitive.

The integrated notification prototype is compiled in tests or explicitly with
`gc-notification-prototype`; release applications select it with
`WILLOW_GC_NOTIFY=1`. Ordinary builds have no notification fields/callbacks.
It retains packed generation/owner election, announces a waiter before validating
generation, and couples notification to the waiter mutex. SeqCst ordering ensures
that either the waiter sees publication or the publisher sees the waiter.
Prior publication, concurrent publication, spurious wake and timeout cases are
tested. Closure samples generation after its own drain and before checking idle
sources. Queue publications and final reader/accounting merges are bracketed by
existing owners; the stopped election remains authoritative after every wake.

CPU repayment is a separate uninterruptible wait using measured thread CPU (wall
time fallback). Only the idle remainder of the existing delay is interruptible.
Runnable queue/retry work retains the existing 1 ms minimum and recovery bounds;
no busy-spin was introduced. The prototype adds O(1) state per cycle, one waiter
check per owner transition, and a predicate mutex only with an enrolled waiter.
The 128-wave bound is unchanged. There is no scan of historical publications.
Notification mutex code never acquires heap, queue, registry or scheduler locks,
and a waiter releases the predicate lock before doing any collector work.

`review_measure.py` compares normal builds against the **enabled integrated**
prototype: allocation, chain, and 32-cycle graph applications, marker counts
1/4/16, five fresh processes per cell in alternating order. Results and bootstrap
confidence intervals are in `results.md`, raw rows in
`notification-comparison.json`. Real closure-tail latency improves, but the graph
four-marker median regresses beyond 3%, application benefits are inconsistent,
and CPU non-regression is not established. Therefore the prototype remains an
explicit experiment and is **not enabled in normal builds**. The original
uninstrumented HEAD/current comparison is not used to infer notification benefit.
GNU time application CPU has coarse 10 ms resolution; the closure CPU measurement
uses the high-resolution thread clock. Neither is extrapolated to other machines.

## Attributable request completion

The old mandatory checks could pass because of a second synchronous collection.
They now submit exactly one request per round and poll its receipt while calling
safepoints. Both assert asynchronous admission, so the synchronous compatibility
fallback cannot mask coordinator failure. The four allocating-mutator test retains
its hard-budget checks during the wait and the 10-second per-request deadline.
The Linux application probe uses the same receipt for both optional and required
requests; no independent `willow_gc_collect` is issued by that probe.

Under the existing coordinator state mutex, each accepted request receives a
checked monotonic sequence. The collector snapshots the selected sequence before
attempting collection, includes newly coalesced requests when retrying a lost
election, and release-publishes completion only after its own successful collect
and mutator unregistration. Requests arriving during its successful cycle remain
pending for the next cycle. Explicit collectors cannot update this completion
counter. Shutdown cancellation leaves unserved receipts incomplete. The counter
is held by an Arc belonging to that coordinator, so a later coordinator cannot
accidentally satisfy an old receipt. Polling is one acquire load and requires no
GC locks. Added cost is O(1) per request/completed cycle and O(1) shared storage per
coordinator plus O(1) per caller-held receipt; no per-request queue/history scan or
allocation on accepted asynchronous requests. Existing coordinator/GC lock order
and coalescing are preserved. The election-loss regression also waits for its
receipt and requires exactly two cycles with no later allocations/requests.

## Foreign-root receipt repair

Review found that the former boolean collection result meant both collection and
foreign-root skip. `CollectionAttempt` now distinguishes Completed, ElectionLost
and ForeignOwner; both early and definitive-handshake foreign checks return the
last outcome. Only the return after actual cycle completion can advance receipts.
The existing void collection ABI and explicit collector-join path are unchanged.

The coordinator retains its selected request on a foreign skip, unregisters before
waiting, and retries after a 10 ms condition-variable backoff. It holds no heap,
root, collector or safepoint guard while waiting. Shutdown interrupts the wait;
new requests and spurious wakes do not shorten the timeout predicate. Election
loss still uses the existing cooperative cycle-completion wait. Every retry
coalesces newly pending requests without scanning a request list. Legacy owners
are not obliged to safepoint and root release has no common notification edge,
so a timed retry avoids adding synchronization to every root mutation. This
trades up to one backoff interval (plus scheduling delay) after owner release
for bounded retry frequency. A permanently held foreign stack intentionally
prevents collection; no unsafe reclamation or completion is claimed.

The synchronous fallback preserves its native failure boundary, quiescent frame
root release and relocation-safe scope, but constructs a completed receipt only
for Completed. A foreign-root skip or lost election returns None, documented as
deferred/not admitted, so callers may retry. No independent collection is used
as evidence that the skipped request succeeded.

Regression coverage spans asynchronous/synchronous requests crossed with early/
handshake foreign-root checks. The handshake case bypasses only the early test
gate; a real legacy root owner is rejected by the actual stopped handshake.
Assertions check no successful collection, no completed receipt, correct skip
reason, then recovery after root release (same asynchronous receipt, explicitly
resubmitted synchronous request). Repeated spurious notifications verify an
attempt-count bound of elapsed milliseconds / 10 plus two boundary attempts.
A separate test shuts down while ownership remains foreign, then proves a later
explicit collection cannot complete the canceled receipt. Existing foreign-owner,
coordinator election, shutdown, allocation-progress and fatal-failure tests are
included in validate.py. The M3 saturation test still requires asynchronous
receipts and enforces the hard budget.

Added dispatch and backoff state are O(1). Over a foreign-owner interval D the
coordinator makes at most O(1 + D/10ms) attempts, independent of request count A;
coalescing still costs O(A) total and retains O(1) coordinator storage. The normal
early check is constant work. A rare definitive-handshake rejection retains the
existing snapshot preparation cost (regions/bitmap words); this repair does not
add a heap scan. No graph, generated-code or successful-cycle asymptotic change.
The final source manifest and validation logs cover this repair; the earlier
notification experiment is retained as historical evidence for the unchanged
notification implementation. `review_measure.py --opportunity-only` regenerates
the affected request-completion application evidence without rerunning unrelated
profiling/notification experiments.

## Barrier ledger and Q7

| Store path | Existing generated/runtime work | Decision |
| --- | --- | --- |
| Filtered inactive reference store | Phase atomic byte load; owner generation and remembered byte loads; tests/branch; store. OldValueSkips can add a value-generation load. | Preserve. |
| Active or generational slow reference store | Old reference atomic load, one barrier call, atomic store; deletion published before the store. | Preserve SATB. |
| Raw unfiltered reference store | Exactly one old atomic load, call, atomic store per reference word. Tested at 1/16/256 stores. | Linear generated size. |
| Interface pair | Object word follows reference protocol; vtable word is an ordinary scalar store. | No spurious vtable root. |
| SATB publication | Registry acquisition, private owner-buffer acquisition, bounded 256-entry publication by default; sealed paths fall back conservatively. | Keep existing owner buffers and 64 registry shards. |
| Initialization | Fresh storage starts zeroed; old-value deletion is absent, but insertion/generational obligations still apply. | No speculative initialization-barrier elision. |
| CAS | Current public atomics provide i64/bool CAS, not managed-reference CAS. No reference CAS store to instrument. | Do not invent a pointer-CAS language/API extension. |

`barrier-process-*.txt` contains five separate release processes for the existing
store ledger workload, including inactive stores and active capacities 1/256.
The focused Cranelift logs prove generated ordering/counts rather than equating
runtime nanoseconds with an instruction count. No SATB write is removed on timing
evidence. Scalar-CAS behavior is independent of managed reference barriers.

## Complexity and lock audit

Independent dimensions: graph objects V, reference edges E, queued work Q,
worker count W (bounded at 64), SATB publications S, registered mutators T,
regions R, sweep heap holds H, optional request count A.

Added profiling costs O(S + H + completed mark batches + steal attempts) time,
O(1) retained process storage (5 x 65 histogram buckets and 64 worker records),
and O(1) thread-local worker storage. Disabled profiling adds constant checks at
existing batch/lock boundaries and does not read clocks. No extra graph scan,
region pass, generated code, or per-object allocation is introduced. A queue
snapshot reads a fixed number of atomic counters. Existing worker stealing may
probe O(W) peers; no new peer scan is added. Scheduler advisory snapshots take
O(W) atomic reads of existing local queue lengths, once per optional request;
mandatory requests avoid that scan. A globally coherent O(1) queue summary would
require new hot-path synchronization, unjustified for this cold prototype.

SATB lock order remains heap -> registry -> private buffer -> queue for sealing;
producers release registry before private-buffer publication. Profiling locks
never acquire a GC/scheduler lock, preventing a reverse edge. Sweep observations
are recorded after the heap guard is dropped. The sweep cursor's existing
Unswept/Sweeping/Quarantined/Available behavior and exclusion of allocating into
swept/quarantined regions are unchanged. Relevant competing-allocator and
reclamation tests were rerun. Region-local locks, bitmap partitioning, TLS
redesign and adaptive worker-count changes were not justified/adopted.

Scaling evidence covers 4,096/32,768-object chains and fanout 4/16, all reachable
through a single root, with 32 full collections per process, a concurrent CPU-active registered mutator,
per-cycle durations, directly measured mutator CPU/wall and complete payload checks;
1/2/4/8/16 markers, five fresh processes per cell. SATB tests use 1/8/64/128 producers,
exactly 4096 deletions each, checking the captured total after seal. Existing
bounded-sweep operation-count tests cover fragmented/contended storage and
amortized heap holds. Feature/test notification state is O(1) per cycle. Optional
admission is O(A) except the bounded scheduler snapshot; there is no retry loop.

## Validation and limits

`validate.py` runs formatting, all four CI-target Clippy compilations in both
normal and notification-feature configurations, and only
named/filtered tests for the affected paths. It includes generation elections,
spurious pool wakes, late roots, late SATB, batched assist closure, worker panics,
deferred/retrying native callbacks, bounded recovery, a dead registered mutator,
hard-limit failures, sweep contention and generated reference-store proofs.
Feature-enabled closure, termination and native-retry filters also pass. The
coordinator election-loss test verifies the retained request receipt.
The saturated-mutator test runs four allocating/CPU-active mutators, eight
mandatory requests, rejects sixteen optional requests using a deliberately
saturated sample, checks completed cycles/progress and enforces a hard cap.
This deterministic injected sample is distinct from the measured Linux probe.

Linux x86_64 native execution is tested. Windows x86_64, macOS arm64 and macOS
x86_64 have cross-target Clippy coverage only; native link/run is unverified.
No full local test suite, complete example audit, or CI polling was performed.
The workloads are specified experimental fixtures, not a claim to have executed
an unspecified universal application standard set. No production optimization
is asserted to pass an application-benefit gate that these data do not establish.

Reproduce: `rtk proxy python3 docs/audits/gc-worker-telemetry/validate.py`, then
`rtk proxy python3 -B docs/audits/gc-worker-telemetry/final_measure.py` and
`rtk proxy python3 -B docs/audits/gc-worker-telemetry/review_measure.py`, followed by
`rtk proxy python3 -B docs/audits/gc-worker-telemetry/report.py`. Do not run other
heavy jobs while measuring. `exploratory/` retains measurements taken before the
final instrumentation audit; final reports use the top-level evidence files.

## Supervisor recovery verification

The interrupted loop stopped before review on a generated Python bytecode cache.
The recovered .gitignore change excludes only this audit directory's nested
`__pycache__` directories. The cache remains on disk; source scripts and raw
evidence remain reviewable. The requirements exclusion remains intact.

`recovery.json` records verification against the original session baseline.
At that recovery checkpoint every runtime/probe hash matched and only .gitignore
differed. This historical recovery record predates the subsequent review repairs;
the current manifest and rerun validation describe the repaired implementation. Formatting and diff whitespace checks were rerun successfully. The
actual loop snapshot/diff functions accept the complete current change set without
a binary error, excluding private paths and Python cache files. The Git index is
empty. No implementation-review result existed to resolve; no approval or ticket
closure is inferred from this infrastructure check.
