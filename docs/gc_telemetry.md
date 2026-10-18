# GC telemetry contract (M0, version 3)

This document describes public source behavior at baseline c1888af and this M0
change. V1 and V2 retain their sizes and symbols. Request version 3 through
`willow_gc_stats_size` / `willow_gc_stats_snapshot`; short buffers are untouched,
null/unknown requests fail, and unaligned output is supported. The public C
layout is `crates/willow_runtime/include/willow_gc_stats.h`. The reproduction
harness compares native C sizes and offsets with Rust, including frame fields.

## Cycle boundaries and requests

`Cycle::begin` assigns a strictly increasing process-lifetime identity; resets
never reuse it. `MarkComplete` means the graph closure is established (minor
tracing includes evacuation), `SweepComplete` means sweep/drop/finalization work
has completed, and `Quiescent` means this cycle has relinquished its ownership.
An outer cycle publishes quiescence under coordination at election release. A
nested minor publishes it after resuming the world, restoring its parent. New
reclaimed old spans can remain quarantined until the next mark activation;
SweepComplete does not imply that every freed byte is immediately reusable.
These events are unrelated to a task's separate retirement/quiescence epoch.

Use `gc_telemetry::milestones::request_gc_for_test()` for an opt-in synchronous
explicit request. It returns its own completed ticket, or None if it did not
complete its own cycle (for example a foreign root owner or collector reentry).
It does not impose a timeout on collection. `wait_gc_for_test(ticket, stage,
timeout)` is the bounded observer wait: condition-variable predicate rechecks,
a total elapsed-time budget, and explicit Timeout/Aborted/Expired results.
Registered mutators cannot block in this observer API. Observers must not hold
managed roots. The retained history is 64 records including active parents;
older inactive tickets expire. A parent survives more than 64 nested minors.

The existing `gc_collect()` is unchanged: scheduler quiescent roots are released,
a relocation-safe scope is entered, and `cycle_wait::collect_explicit` cooperates
with an existing collector, requests its recovery when necessary, then attempts
a full collection. Collector reentry does not wait on itself. Foreign legacy
root owners can prevent collection. Live lexical roots remain live; this call
is not a promise that a just-completed task or local object has been reclaimed.

`WILLOW_GC_TRACE` enables JSON lines. Version 1 remains the default;
`WILLOW_GC_TRACE_VERSION=2` adds stop events; version 3 additionally emits distinct
MarkComplete/SweepComplete/Quiescent records with the cycle identity and event
time, plus a verification-counter sample. Event times precede delayed trace I/O.
Trace sink failure disables that sink and increments trace_errors. Bounded event
history loss is explicit; trace counters can sample a later cycle, identified by
counter_epoch and reset_generation. No heap addresses are serialized.

## Units and consistency

All durations/timestamps are monotonic nanoseconds, memory is bytes, counts are
unsigned dimensionless quantities. Cumulative telemetry saturates where stated
in the source; frame/survivor operational counters retain their existing atomic
or heap-owned arithmetic. No snapshot is a globally simultaneous sample.

| Group / fields | Interpretation | Consistency |
|---|---|---|
| V1 timestamp, epoch, phase, reset_generation; last_cycle / last_major_cycle | Cycle start/end, wall mark duration, retained-byte samples; major phase includes concurrent work | Coherent telemetry mutex group; completed cycles eventual |
| V1 allocation_count/allocation_bytes, freed_objects/released_bytes, TLAB and promotion counters | Logical allocation and allocator reservation release; physical copies are not logical allocations | Heap-guarded reconciliation; exact at safepoint, concurrent updates can follow |
| V1 occupied/young/reserved/committed bytes | Occupied includes garbage. Committed is allocator reservation convention, not OS commit/RSS | Heap-guarded; exact at safepoint |
| V1 last_major_cycle.heap_after_bytes; V3 last_major_retained_bytes | Last major retention sample, not instantaneous live bytes | Eventual completed-major sample |
| V2 stops.by_reason, last_event, active_sequence, flags | Initial/remark/minor/recovery reason, request→park and parked→gate release, aborted protocol and event loss | Coherent stop group; completed events eventual; active request visible |
| V3 root_scan_ns/samples | Root enumeration and seed traversal; minor root evacuation overlaps fused evacuation timing | Coherent accumulated spans; exact at safepoint after publication |
| V3 evacuation_ns/samples | Fused minor tracing/copying traversal, not a separate sum of copy and trace costs | Coherent completed spans; overlaps root phase |
| V3 resume_ns/samples | Callback return→stop gate clear, notification and coordination unlock; excludes OS scheduling latency after notification | Coherent completed spans |
| V3 mark_cpu_ns / mark_cpu_missing_cycles | CPU measured on executing mark threads, including fallback and minor tracing; missing CPU is explicit | Completed-cycle accumulation; not wall time |
| V3 assist_cpu_ns / missing_samples / samples | Allocating mutator mark-assist interval CPU, including wrapper overhead; overlaps mark CPU, never add it to mark CPU | Completed-assist accumulation; can lead completed-cycle totals |
| V3 memory.managed/external/old_reserved/young_reserved | Heap metadata occupancy and reservation; external is retained native requested-capacity estimate | Heap-guarded metadata plus independent atomic external sample; approximate during mutation |
| V3 memory.pinned_bytes | Live bytes in pinned chunks; explicit pin-owner details also available in survivor/pin snapshots | Heap metadata gauge; exact at safepoint |
| V3 memory.fragmentation_bytes | Used minus live across extents, including unusable holes | Heap metadata gauge; approximate during sweep |
| V3 memory.pending_sweep_bytes | Live-byte metadata in pending regions/chunks; candidate occupancy, not predicted garbage; partial chunk metadata is approximate | Heap metadata gauge, zero after completion |
| V3 measurements.reclaimed_bytes | Sum of completed cycles' logical freed bytes | Eventual cumulative total; neither pending candidates nor allocator reuse |
| V3 memory.quarantined_bytes | Old free spans plus tails not yet available for reuse | Heap metadata gauge; separate from native/frame quarantine |
| V3 memory.reusable_bytes | Shared regular-old free spans plus tails, excluding pending/quarantined extents, large-object tails and private LAB capacity | Heap metadata gauge; exact at safepoint |
| V3 survivors.* | Copies, promotions, consecutive deferral debt/retry watermark/blockers, pin fallback/reasons/leases, survivor reservation/occupancy | Heap-owned counters; snapshot metadata, exact at safepoint; standalone survivor_snapshot reconciles current debt |
| V3 frames.* | Allocated/reclaimed/quarantined frame bytes and running/rooted/retired/terminal counts, lifecycle event counters | Atomic approximate; exact only at a rendezvous |
| V3 worker_threads/busy/idle | Marker pool thread counts; busy spans a job, idle=max(threads−busy,0), not CPU utilization | Independently atomic approximate |
| V3 barrier_variants[13], satb_deletions, valid | Processed destination-specific stores and logical deletions while marking | Atomic approximate; enabled in tests or WILLOW_GC_VERIFY_BARRIER; null/no-op exemptions remain |

Memory is not process RSS: map/channel/async-lock retained native capacities are
charged externally, while stacks, thread infrastructure, transient allocations
and allocator overhead are not represented as a complete process-memory cap.
Native committed bytes have no exact portable estimate. Do not sum nested V1
and V2 groups or mark+assist CPU. Snapshot cost is existing LAB reconciliation
plus O(regions+chunks); free-hole bytes are cached, avoiding per-hole rescans.

## Failure records

GC invariant/fatal/FFI failures emit bounded JSON to stderr. Runtime tests install
a chaining panic hook, so panics also carry diagnostics. `failure_record` exposes
an opt-in occupied-memory threshold assertion and reproduction seed/case tags.
Reservation admission failures report the actual old+young+external charge.
Threshold decisions and category emission share one reconciled sample.

The fixed buffer is 8192 bytes including newline; strings are escaped and capped
at 256 Unicode characters. Overflow emits a small valid fallback JSON record.
Fields include kind/reason, timestamp/epoch/phase, OS/arch, seed/case, actual/limit
bytes, managed/external/reserved/last-major-live categories, roots, stop outcome,
deferral/pin and coordination counters, and frame lifecycle counts. Unavailable
try-lock samples are null, never fabricated zero. Groups may describe adjacent
moments; failure heap sampling deliberately does not merge LABs. Sampling has no
unbounded traversal or wait for a GC lock; stderr I/O can block as ordinary I/O.
The reporter has a thread-local recursion guard. Assertions should use these
records, rather than only a boolean comparison or a sleep before sampling.

### Captured threshold samples and recovered marker failures

Memory regression checks retain their strict original bounds. The test-only
`assert_sample_below` takes the sample already used by the assertion, so reporting
under the heap mutex does not reacquire it. `managed_memory_threshold` measures
managed occupied bytes; `reserved_memory_threshold` measures old plus young
reservation bytes and retains categories from the peak reservation sample.
`rss_memory_threshold` reports process VmHWM converted from KiB to bytes; its GC
categories are a contemporaneous accounting sample, not an attribution of the
historical process RSS peak. These kinds use exclusive `limit_bytes`; the existing
`memory_threshold` occupied-memory helper retains its inclusive limit semantics.

`ConcurrentCycle::drain_recovering` emits `marker_panic` with the caught reason
before returning the fallback value. This production path does not require the
test panic hook. It uses the same bounded formatter and nonblocking metadata
try-locks; unavailable accounting stays null. Recovery, failed-cycle suppression,
and authoritative stopped tracing retain their existing behavior. Stderr I/O may
block, as for other failure records.
