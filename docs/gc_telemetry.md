# GC telemetry V1

The runtime exports `willow_gc_stats_snapshot_v1(WillowGcStatsV1 *out)` and
`willow_runtime::gc_telemetry::snapshot()`. These measure the current minor and
major stop-the-world collectors and provide the observation window needed by
future pacing work. They do not change collection policy.

The C declaration is in
[`willow_gc_stats.h`](../crates/willow_runtime/include/willow_gc_stats.h).
The C API writes exactly 2,112 bytes on supported 64-bit targets, returns `0` on
success, and returns `-1` for a null pointer. The caller provides aligned,
writable storage for the complete V1 structure. Its first two fields are
`uint32_t version` and `uint32_t struct_size`. V1 will not grow; an incompatible
or extended layout needs a new versioned symbol. The compiler ABI registry
records `(Ptr) -> I32`, with no GC allocation or safepoint effect.

## Reading measurements

| Group | Meaning |
|---|---|
| `counters` | Cumulative logical allocations/bytes, freed objects, allocator-released backing bytes, TLAB refills and allocations, promotion and barrier work. Promotion copies are not new logical allocations. |
| `heap` | Current occupied storage, young occupied storage, allocator reservation/commitment, regions, remembered objects/cards, and current collection triggers. |
| `minor_cycles`, `major_cycles` | Completed cycles, consistent with their pause histogram counts. |
| `marked_bytes` | Cumulative header plus payload bytes of objects visited, once per object per traversal. A minor cycle includes remembered old objects it scans. |
| `scanned_bytes` | Reference slots inspected, including null references, multiplied by pointer width. Custom trace callbacks contribute their non-null slot addresses. |
| `root_scan_bytes` | Non-null root addresses submitted to tracing, multiplied by pointer width. Duplicate roots count here but do not double-count object traversal. This is not the size of all allocated root-stack slots. |
| `mark_ns` | Elapsed traversal time. Minor traversal includes pinning, scanning and copying, but excludes nursery inventory construction and sweeping. This is elapsed time, not CPU time. |
| `pauses`, `minor_pauses`, `major_pauses` | Count, cumulative nanoseconds, maximum, and 65 logarithmic buckets. Bucket 0 is zero; bucket b > 0 is `[2^(b-1), 2^b)` ns. |
| `last_cycle` | Complete most recent cycle: identity, kind, timestamps, latency, work, occupied bytes before/after and their reclaimed difference. |
| `last_major_cycle` | Most recent whole-heap liveness measurement. A minor cycle leaves this record intact. |
| `phase`, `epoch` | Coherent cycle state: phase 0 = idle, 1 = minor STW, 2 = major STW. Epoch advances for elected collections; skipped collection requests do not create cycles. |
| `reset_generation` | Advances on runtime reset. Cumulative counters reset together. `GcRates::between` rejects windows across resets. |
| `process_resident_bytes`, `resident_valid` | Best-effort process RSS, including memory outside the GC. Only the C snapshot samples the OS. |
| `trace_errors` | Process-lifetime failed trace opens/writes/flushes. A failed sink is disabled after one failure. |

Occupied bytes include unreachable objects until a collection discovers them.
`last_major_cycle.heap_after_bytes` is measured live storage at the end of that
major collection. A minor cycle's retained storage includes old objects that the
minor collector did not collect. Those quantities are not interchangeable.

Reservations include GC region/TLAB backing storage; they exclude Rust container
buffers, allocator overhead and non-GC memory. The current allocator obtains all
backing storage with `alloc_zeroed`, so allocator commitment equals reservation.
Neither is process RSS. Released bytes mean returned to the allocator, not a
promise that the OS has reclaimed those pages.

A pause spans the elected collection, including safepoint handshake and root
preparation, through world resumption. Trace I/O is outside this interval. This
is the maximum affected-mutator interval, not just the interval when every
mutator is simultaneously parked.

Heap fields are captured together under the existing heap mutex; cycle fields
are captured together under a separate telemetry mutex. The two groups may
straddle a collection. Their own invariants hold, but callers must not compare
`heap.occupied_bytes` with `last_cycle.heap_after_bytes` as if they were sampled
at one instant. No heap lock is held while taking the telemetry lock or doing
OS/trace I/O.

Counters use saturating accumulation. Existing TLAB counters are merged on
snapshot, refill, retirement and unregister, exactly once per observed delta.
The generated allocation fast path gains no telemetry lock, global atomic or
runtime call. Traversal work accumulates locally and publishes once per cycle.

`GcRates::between(&before, &after)` calculates allocation bytes/second, mark
bytes/second over the observation window, and mark bytes/second of traversal.
It rejects zero/reversed windows, unknown versions and reset/decreasing-counter
windows. `snapshot()` omits OS RSS sampling so a future controller can poll it
without filesystem or platform calls. Concurrent marking, assist debt/latency,
CPU accounting and memory-limit policy require their own implementation before
measurements for those features can be defined.

## Cycle trace

Set `WILLOW_GC_TRACE=stderr` or `WILLOW_GC_TRACE=/path/to/events.ndjson` before
starting the process. An unset or empty value disables tracing. Files append;
use a separate file per process/run, since epochs are process-local.

Each completed cycle writes a `gc_start`/`gc_end` NDJSON pair sharing `epoch` and
`kind` (1 minor, 2 major). Their timestamps describe collection time. Both lines
are emitted together after world resumption and after releasing collector
election. Concurrent collectors can publish pairs out of epoch order; correlate
by epoch and sort by timestamps. Buffered writes are flushed per cycle. Failure
to open, write or flush disables the sink and increments `trace_errors`; GC
continues. A crashed process can leave a partial last line/pair.

```json
{"version":1,"event":"gc_start","epoch":1,"kind":2,"timestamp_ns":100,"heap_bytes":96}
{"version":1,"event":"gc_end","epoch":1,"kind":2,"timestamp_ns":150,"pause_ns":50,"mark_ns":20,"marked_bytes":48,"scanned_bytes":8,"root_scan_bytes":8,"heap_bytes":48,"reclaimed_bytes":48}
```

## Running the examples and checks

```sh
cargo test -p willow_runtime --lib telemetry
WILLOW_GC_TRACE=/tmp/willow-gc.ndjson cargo run -p willow_runtime --release --example gc_telemetry
cargo run -p willow_runtime --release --example gc_allocation_bench -- 1000000
cargo run -p willow_runtime --release --example gc_allocation_bench -- 1000000 alloc
```

`gc_allocation_bench` defaults to mixed live chains and short-lived garbage, with
explicit collections per batch. `alloc` measures unrooted allocation with the
normal automatic collection trigger. The same source builds against the runtime
before telemetry, for alternating baseline/candidate comparisons.

Linux C ABI smoke test (the runtime supplies `main`):

```sh
cargo build -p willow_runtime --release
cc -std=c11 -Wall -Wextra -Werror -I crates/willow_runtime/include \
  crates/willow_runtime/examples/gc_telemetry_abi.c \
  target/release/libwillow_runtime.a -lpthread -ldl -lm -o /tmp/gc-abi
/tmp/gc-abi
WILLOW_GC_TRACE=/dev/full /tmp/gc-abi
```

The latter must finish with `errors=1`. Unit coverage includes fixed ABI layout
and output canaries, histogram boundaries and saturation, graph/root work,
TLAB delta merging/unregister, promotion accounting, minor/major separation,
reset/rate windows, trace failures, and 100,000 snapshots concurrent with 32
registered allocating mutators, eight snapshot readers and collection.

## Local baseline comparison

On Linux x86_64 / Ryzen 7 7800X3D, with release runtime and baseline `05f30cc`,
five interleaved measured samples after warmup gave:

| Workload (1,000,448 allocations) | Baseline median | Telemetry, trace off | Buffered file trace |
|---|---:|---:|---:|
| Automatic collection, unrooted allocations | 380.62 ms | 381.03 ms (+0.11%) | 388.50 ms (+2.07%) |
| Live chains, explicit collection per batch | 1155.26 ms | 1168.21 ms (+1.12%) | 1198.14 ms (+3.71%) |

The Rust snapshot measured p50 120 ns / p99 180 ns with an empty heap. Cost grows
with registered TLAB states and region count. These are small, single-host
samples, not a cross-platform performance gate. In particular, the file-trace
samples do not establish the proposal's 2% overhead target. The raw samples and
method are in [`baseline_comparison.json`](../benches/gc_telemetry/baseline_comparison.json).
