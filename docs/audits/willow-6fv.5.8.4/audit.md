# Mutator assist implementation audit — 2026-10-24

Ticket willow-6fv.5.8.4 remains in progress for review. No staging, commit,
publication, CI query, or mirror sync was performed. Initial working tree was clean.
This report describes public implementation and experiments, not private specifications.

## Current-session implementation versus inherited behavior

The inherited implementation already had signed saturating Q16 thread debt,
unique cycle epochs, TLAB refill accounting, work credit from the actual consumer,
250 microsecond/eight-item drains, chunked bitmap/native containers, native callback
telemetry, and assist CPU measurement. This session connects execution to the
Pacer's residual assist prediction and recomputes the ratio from remaining work
and remaining allocation runway at each slow allocation/refill. Completed work
includes background and collector work, but never becomes another thread's credit.
Historical underestimation retains one work byte per allocation byte while queue
ownership remains outstanding. A zero residual background prediction can rise as
runway is consumed. PPM-to-Q16 conversion rounds upward with full-width arithmetic.

Only registered mutators execute allocation assists. The fast TLAB path is unchanged:
no new global atomic or generated instructions per fast allocation. Cycle-local
allocation consumption uses one saturating atomic update per slow charge. Completed
work is read under the existing short work-accounting mutex, which is released
before queue acquisition. Empty queues skip drains even when private debt remains;
no credits are invented and there is no wait-for-credit protocol.

Noncollector drains now check the stop/closing gates between items and inside SATB
batches. A partial batch is republished before its original claim is completed.
Inline tails and native continuations retain the existing publication protocol.
Allocation assists release all readers/claims and their CPU span before entering
the safepoint handshake. The collector remains able to finish stopped work.
Unregister clears TLS debt; inactive-phase allocation clears it; a new cycle tag
invalidates dormant debt/credit lazily. Shutdown/cancellation retain the existing
owner retirement protocol; this patch introduces no parked assist waiters.

## Budget, CPU and fallback boundaries

The approved scheduling slice remains 250 microseconds, eight outer items.
An owned SATB item contains at most 32 objects; it may exceed the object count by
31 but checks the deadline and stop gates between objects. At least one item may
run after entry descheduling, preserving progress. Native unchunkable callbacks
remain indivisible and separately recorded by native trace telemetry. Bitmap,
array, map, channel and scope/frame continuation implementations are reused;
focused tests cover their adverse shapes. Wall time is not a hard realtime bound:
OS scheduling, lock acquisition and an indivisible callback can exceed the slice.

MarkBatch CPU from assists is included in cycle mark CPU and complete-cycle GC
CPU, excluding collector overlap, then enters Pacer's completed CPU budget window.
The existing Assist span separately measures wrapper-inclusive CPU; these two
counters overlap and must not be summed. This session adds a thread-local CPU
scope which subtracts the nested mark CPU from the wrapper interval and adds
only the overhead to cycle CPU. Collector overlap is updated consistently;
missing clocks, reversed clocks and overflow invalidate CPU accounting rather
than reporting zero-cost work. Scope cleanup restores TLS even during unwinding.
The Pacer still explicitly reports uncontrolled phases;
this change does not assert a process-wide hard CPU cap.

WILLOW_GC_ASSIST=0 removes the cycle's assist plan. Background/collector marking,
reservation limits, pressure collection and explicit baseline stopped recovery
remain active. Predictive runway is not a hard reservation cap. A descheduled owner
can hold the last outstanding item while peers consume more runway; an attempted
one-in-flight-allocation overshoot assertion was invalid for that model. The
isolated controller benchmark reports this overshoot, while a separate real
allocation test asserts the actual hard reservation bound with 1/5/16 mutators,
with assistance both enabled and disabled. No global allocation barrier was added.
This is the allowed baseline controller slice, not final pauseless-mode proof.

## Focused validation and test perspectives

Reproduce focused validation with `rtk proxy python3 docs/audits/willow-6fv.5.8.4/verify.py`.
The checked-in output records 37 passing test executions (some filters overlap),
not a whole-crate or integration-suite run. Perspectives include:

1. Fractional Q16 charge accumulation and refill batching.
2. Signed positive debt and negative credit saturation.
3. Zero work cannot manufacture credit.
4. Old epoch credit cannot repay new epoch debt.
5. Independent 1/5/16-thread private credit.
6. Remaining-work decrease after completed scans.
7. Runway consumption and exhaustion.
8. Saturating concurrent allocation consumption.
9. Pacer background prediction and PPM conversion.
10. Underestimated work with outstanding queue ownership.
11. Empty queues return without waiting or fabricated work.
12. Shared queue credit sums to exactly the traced work.
13. Expired deadlines publish SATB tails before return.
14. Remark/stop gates preserve owned SATB tails.
15. In-flight assist and remark rendezvous.
16. Bitmap growth with exact slot/descriptor counts.
17. Native reference-array slicing and shrink races.
18. Map growth with finite cursor and retained deleted values.
19. Channel pop/append snapshot bounds.
20. Parallel SATB phase flips and registration/unregister publication.
21. Marker panic recovery and consumer retirement.
22. Cancellation and shutdown with retained allocations.
23. Repeated closure restarts and empty assists.
24. Deep chain and fan-out continuation accounting.
25. Real allocation progress, debt, work fairness and latency at 1/5/16 threads.
26. Real reservation pressure and disabled-assist safety at 1/5/16 threads.
27. Existing CPU budget thresholds and missing-clock behavior.
28. Assist CPU aggregation, missing samples, overflow and unwind restoration.

`cargo fmt --all -- --check` passed. Workspace/all-target Clippy passed for x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc,
aarch64-apple-darwin and x86_64-apple-darwin. Native execution was Linux x86_64 only; macOS and Windows native
link/run behavior is unverified. Full local suites and example audit were omitted
by policy; CI was not queried.

The fresh runtime was linked by `target/debug/willow run example/gc_old_cycle.wi
--runtime-lib target/debug/libwillow_runtime.a`, both normally and with
WILLOW_GC_ASSIST=0. Both runs printed 499500 three times.

## Measurements

Run `rtk proxy python3 docs/audits/willow-6fv.5.8.4/measure.py` to reproduce
`measurement.txt`. Each thread-count case uses a fresh process so histograms do
not mix windows. Debug Linux measurements are host observations, not release
performance promises. Histograms report percentile bucket upper bounds.

| Mutators | Assist p50 upper ns | p99 upper ns | Max ns | Total assist CPU ns | Runway overshoot bytes |
|---|---:|---:|---:|---:|---:|
| 1 | 16384 | 32768 | 45024 | 4030208 | 0 |
| 5 | 16384 | 65536 | 58639 | 5814773 | 84112 |
| 16 | 32768 | 131072 | 93344 | 6448110 | 97632 |

All cases trace exactly 8192 objects / 131072 work bytes with no background
consumer. Raw rows report each thread's work share, current/peak Q16 debt,
allocation CPU, p50/p99/max allocation latency and aggregate elapsed time, so
cumulative overhead and fairness are visible rather than hidden by small slices.
Total allocation throughput is allocations divided by elapsed_ns, with 1024
allocations per thread. This measures registered-thread fairness, not Task fairness.

The real-pressure workload uses 4096-byte allocations and a 16777216-byte hard
reservation limit. Peak reservation was 4456448/16777216/16777216 bytes for
1/5/16 threads, with both assist modes. No reservation exceeded the configured
cap; all threads and collection work completed. The inherited Pacer allocation
benchmark additionally reports soft-goal overshoot, throughput and total GC CPU
for 64/512/4096-byte allocations. That synchronous workload had zero assist samples
and is fallback evidence, not evidence of assist speedup.

## Complexity and ownership audit

Let A be slow allocations/refills, P participating threads, W graph work (objects,
reference slots and descriptor words), and B bounded drains. New pacing arithmetic
and snapshots have fixed size and no input-dependent scans: O(A) uncontended work,
O(1) storage per cycle and O(P) existing TLS debt. The saturating CAS can retry
under contention; it is lock-free, not wait-free, and no constant worst-case
latency is claimed. The shared accounting mutex likewise depends on scheduling.
No heap scan or mutator-registry walk is introduced by a pacing update.

Queue scanning remains O(W) useful work plus existing queue scheduling, and O(B)
bounded publication/CPU-accounting overhead. SATB tails copy at most 31 entries;
repeated expired slices can copy a fixed 32-element batch quadratically in 32,
which is a fixed bound, not quadratic in graph size. Bitmap and container cursors
do not rescan prefixes. Tests deterministically count credited bytes, queued work,
bitmap slots, chain continuations and fan-out claims at increasing input sizes;
wall time alone is not used to infer complexity. A claimed item is completed only
after its successors/tail are published, so closure cannot observe a false zero.
No per-Task map, new graph index, duplicated compiler analysis, compiler code-size
change, linker scheduling change or artifact caching was introduced.

The existing TLAB accounting path scans registered records under its existing
heap sync; that cost predates this patch and is not repeated by the new controller.
Existing native retry policy and legacy unchunkable callback costs remain visible
in their own telemetry. No new backlog item was created.
