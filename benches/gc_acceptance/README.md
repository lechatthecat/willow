# GC acceptance harness (schema 1)

Build the opt-in fault-capable probe and run the focused checks:

```sh
rtk cargo build -p willow_runtime --release --example gc_acceptance --features gc-acceptance-faults
rtk proxy python3 -B -m unittest discover -s benches/gc_acceptance -p test_suite.py
rtk proxy python3 -B -m unittest discover -s benches/gc_acceptance -p test_native.py
rtk proxy python3 benches/gc_acceptance/faults.py --binary target/release/examples/gc_acceptance --output /tmp/gc-faults
rtk proxy python3 benches/gc_acceptance/suite.py --binary target/release/examples/gc_acceptance --output /tmp/gc-pr --profile pr --power unavailable --ram-bytes 0 --build-flags release --fault-evidence /tmp/gc-faults/summary.json
```

The active contract is R1/R2: classified STW is measured, not forbidden.
Recovery counts remain visible and their CPU, pause, wait and footprint costs
remain gated. The fault diagnostic alone freezes a zero-recovery cap to prove
injected recovery is detected. No throughput, CPU, footprint, pause or scaling
budget is relaxed.

Every output directory must be new unless `--resume` is specified. Resume requires
identical frozen source, binary, manifest and host metadata. It reuses saved trials;
invalid or interrupted outcomes are retained and never rerun in their place.
Raw NDJSON/binary events are produced and validated, then compacted after saving
the result: SHA-256/byte counts, endpoint samples, configuration, distributions
and failures remain; regenerable raw dumps are deleted to bound local storage. `--profile nightly` selects the larger matrix;
`--case ID` may be repeated for focused investigations. Selected cases and PR runs
cannot certify the full release matrix. Missing workload families, control modes,
fixed-root scaling, root/worker/hard-cap axes, soft-limit states or measured
million-task inputs are reported explicitly and prevent release approval. Duplicate
case IDs are rejected before a capture can be reused. Exit 0 means complete selected measurements;
read `summary.json` for the separate acceptance decision. Full release evaluation
requires same-binary runtime fault evidence, complete nightly coverage, differential
checksums, performance gates and measured elasticity. No retry replaces a failure.

Inputs, public-source/binary hashes, revision, compiler, configuration and declared
host information are frozen before the first process. Zero RAM and `unavailable`
power indicate unavailable metadata. Each variant has five independent processes.
The five stopped-reference processes run first; reference distributions and absolute
limits are then frozen before candidates. Fixed ratio budgets are never relaxed
using candidate results. Limits are specific to this reference host and workload;
they are not portable latency guarantees. The reference is the existing stopped
collector with concurrent marking disabled, using the same revision and binary.
The other variants exercise concurrent collection, Pacer disabled and Assist disabled.

The matrix includes fixed-root linked/array/fan-out/cyclic graphs, churn, mutation
bursts, variable-sized allocations, real scheduler tasks/cancellation and bounded
channels, retained young strings and old-to-young array references, scoped FFI loans,
indexed catalog data, registered allocating peers, hard reservation caps, soft
limits and overloaded single-marker configurations. Nightly task/cancellation
cases execute millions of distinct tasks; graph scaling spans 64 MiB–1 GiB with
four roots. Root-count cases independently vary roots. Every batch checks graph
IDs/edges and a deterministic workload result. Churn allocates at least four times
the measured retained bytes per batch; the nightly input retains 1 MiB and requests
65536 short allocations. A burst starts at MarkComplete and independently counts
allocation and mutation batches that finish before SweepComplete. Missing overlap
fails coverage. The burst adds one registered collector-control thread. Pin cases additionally check stable
addresses across collections. Run summaries distinguish requested coverage from
what actually happened (including observed overlapping minors, Assist, fallback,
copying, allocation during sweep, channel operations during GC, and measured footprint/limit ratios). Merely naming an overload does not prove
Assist is necessary or beneficial. The overload probe also records reservation
peak through the next MarkComplete milestone. The predeclared phase limit
must hold in every assisted trial, up to the memory-limit allowed overshoot
`max(5% of limit, 16 MiB)`, and every unassisted trial's median window must exceed
the footprint ratio (10%) above that limit, to satisfy that optional evidence route. The nightly overload keeps 512 MiB live with one
background marker while sixteen allocators each request 32 MiB of 4 KiB objects
under a 640 MiB soft limit, so allocation demand outruns background marking. Whole-process peak/RSS comparisons still
apply separately; mark-window benefit does not excuse later heap growth.

Pause elasticity reports initial, remark and other observed reasons separately,
as well as the maximum individual pause and the combined old-cycle pause. Required
initial/remark evidence must be present at all three sizes in every concurrent
trial. A larger constant phase cannot hide a heap-proportional smaller phase.

Application latency measures allocation/collection/verification batches, not HTTP
request latency. Operations count transient allocations, visited graph nodes and
requested workload operations. A zero workload checksum still counts executed
operations; `test_native.py` checks one-task and one-cancellation inputs with
checksum zero, plus their two-operation nonzero-checksum neighbors. Snapshot/flush gaps are outside batch latency;
these are instrumented workload metrics. Startup and warmup remain in raw evidence
and undergo transport validation. Counter deltas and batch distributions exclude
warmup. Pauses run from stop request through coordination/gate release, not OS
rescheduling of every thread. Reasons 1/2/3/4 mean initial/remark/minor/recovery.
Combined old-cycle pauses sum contained non-minor stops. Mark throughput uses
reported marked bytes/time; sweep visit throughput uses completed actual major
sweep visits/time, separately from reclamation rate. Sweep wall time includes
waiting and drop/release work. Local stall distributions overlap and must not be
summed. Per-mutator MMU unions that mutator's Assist and global stops, with exact
1/10/100 ms sliding windows; short lifetimes yield null.

Managed committed/reserved uses the allocator reservation convention, not native
OS commit. Its peak is maintained at reservation growth, including between samples.
Peak RSS/working set comes from the OS process high-water counter. Linux uses
VmHWM instead of inherited pre-exec getrusage history and retains the largest
reported value to accommodate kernel RSS counter approximation. Both include
startup and warmup. Live/quarantine/fragmentation/steady RSS remain boundary samples;
overshoot is explicitly sampled. Wasted bytes count cumulative unused nursery
chunk tails at retirement, not total dead objects or simultaneous fragmentation.
Copied bytes include survivor and copied-to-old bytes; pin-in-place is not copying.
Missing RSS, CPU, events, endpoints or checksums invalidates a measurement.

Soft-limit cases first measure five unlimited same-input concurrent processes.
The median steady maximum of managed reservation and RSS sets each limit, before
any limited variant runs. The achieved ratios are checked independently of labels.

The probe drains V4 events on an unregistered observer while collections run.
The opt-in build assigns drain ownership to this observer while it runs, so
collection completion does not also format V4 JSON. Fixed-width, little-endian
records go to a buffered binary spool; after exit,
`merge_trace` streams a sequence merge with the native JSON sink into canonical
NDJSON. It rejects truncation, invalid tags and per-sink reordering. It never sorts
away corrupted order or discards loss checkpoints. Native JSON, binary spool and
merged JSON are compacted only after analysis and outcome/identity preservation.
This avoids collection-time JSON formatting for
spooled records. `WILLOW_GC_TRACE_CAPACITY=524288` is predeclared; default runtime capacity remains
4096. Capacity is strictly bounded, and invalid configuration or overflow remains
an error in evidence. At 136 bytes/event, the exclusive ring plus one drained batch
uses at most 136 MiB of event payload storage, plus a 1 MiB spool buffer (allocator
overhead excluded). Without the opt-in feature, mixed native/observer drains can
retain two batches, for a conservative 205 MiB total bound.
Stop-event history is separately predeclared at 16384 entries (default 64).
Its pending payload is bounded at 1.25 MiB, with up to another 1.25 MiB per
active cycle emitter; allocator overhead is excluded. Invalid capacity and lost
stop events invalidate the run. The observer report states these bounds separately
from the V4 bound and measures actual process RSS.
Producer-side event work never performs sink I/O. Drain/serialization CPU and
flush count are recorded on stderr. The observer comparison script under the
implementation audit runs five disabled/aggregate/stream processes per mode;
its measured overhead must accompany interpretation of instrumented results. The
comparison predeclares one application worker and one marker and preserves host,
power, source, binary and per-process invocation metadata.

Faults require the explicit `gc-acceptance-faults` Cargo feature. They exercise real
stop/wait/heap-lock protocols and actual capacity overflow, not edited JSON.
`fallback` enters the recovery-stop protocol; it does not manufacture a failed
mark algorithm. `mark_wait` holds collector election and exercises a real
explicit CycleWait rather than wrapping a sleep in a global stop. Fault diagnostics use an explicitly frozen 10 ms wait threshold
so the injected 40/50 ms stalls are practical deterministic tests. Normal suite
wait budgets are separately declared. The fault runner checks the actual targeted release decisions, exact initial-stop,
CycleWait and HeapLock event kinds, and independent runtime loss flags/drop counts.
Binary and evaluator-source identity must remain unchanged throughout diagnostics.
Failed trial summaries and raw identities are retained; they are never retried.

Measurement reports are local artifacts under `docs/audits/gc-acceptance/`.
Linux execution never establishes native Windows/macOS results; unexecuted
platforms remain unverified. No CI polling is needed to interpret local evidence.
