# M4 minor collection implementation audit — confirmation and three independent reviews passed

Current decision (2026-10-24): the user adopted the proposed first-survival
workload after viewing exploratory results and fixed the baseline and budgets.
See contract.md and confirmation-lock.json. Earlier unresolved-contract
statements below describe the historical pre-approval state, not a current
request for approval. Fresh five-pair confirmation includes GC CPU/byte.
Original evidence is retained; confirmation results are separate.

This report describes public runtime source and measurements from this working
tree. The baseline is `8af6c52ba8f6e95e7de14d84f12e9faf98d70995`.
The ticket remains open, with its original acceptance criteria unchanged.
No staging, commit, push, Beads mirror sync, or CI inspection was performed.

## Implementation and ownership

Both elected minor entry points create `SurvivorAdmission` before `with_stw`.
Admission holds the heap mutex and requests one fixed-size chunk per non-pinned
source chunk. Each admitted chunk owns aligned storage, start bits, concurrent
mark bits and enough header-offset capacity for minimum-sized headers. Empty
reserve chunks are absent from heap address indexes. Their storage is charged
to existing TLAB reservations before the stop, so subsequent native/region
reservations cannot spend the same hard-cap capacity.

A survivor destination consumes an owned reserve chunk in constant time;
transfer does not allocate storage or bitmap metadata. Accounting transfers
without incrementing reservation bytes again. Normal sweep owns consumed
chunks. `SurvivorAdmission` releases unused chunks after the stop, before the
collector election is released. It also cleans up on skip, deferral and unwind.
Admission and unused release are included in cycle elapsed time; their heap
mutex can stall allocating mutators outside STW. Moving work out of the stop
does not eliminate that work, and the throughput measurement includes it.
Reset handles any remaining empty reserve. Direct collector unit fixtures have
a test-only admission wrapper; production entry-point tests use the real guard.

Admission is deliberately finite, not a promise of unlimited copying space.
The hard cap, allocator refusal, packing loss, or concurrent refills can exhaust
it. The affected object is then promoted in place by the existing `pin_root`
path, counted as reserve/budget fallback, and queued for tracing. Its children
can still move and are rewritten. Explicit leases are processed first and keep
their own pin reason. The pre-existing no-pin diagnostic also rejects this
fallback. The region/native reservation cap retains its existing scope: it does
not count all allocator metadata or OS RSS.

Old-region promotion allocation and general collector scratch structures are
unchanged; this change pre-admits survivor storage and its per-chunk metadata,
not every possible allocation in a collector callback. The existing old-cycle
publication path still translates mark transports before freeing sources,
claims promoted/copied objects in the active epoch, publishes unscanned native
objects, and retains old descendants of inline-generated objects. No barrier,
allocation-color policy, lease rule, or root-slot mutation protocol was replaced.
Root slots remain exclusively rewritten while owners are stopped. Remembered
owners are re-added when they retain young children; old-cycle publication
remains in the same stopped transaction.

## Attribution

`gc_telemetry::pause::snapshot()` returns six exclusive component histograms
for each of four stop causes, plus the last sample and longest sample per cause.
The existing C ABI is unchanged. Stop-purpose generation is explicit in each
sample: young, old-cycle coordination, or mixed recovery.

Rendezvous, root enumeration, remembered owner/card handling, fused evacuation
(copy/promotion/forwarding/reference rewrite), metadata/finalization, and gate
release/wakeup partition the recorded request-to-release interval. Per-object
clock calls and locks are absent. Metadata is an explicit residual category,
not silently discarded time. Wakeup does not claim to measure the OS scheduling
latency of every resumed worker. V3 overlapping metrics keep their prior meaning.
Aborted samples are retained; saturated aggregates are flagged. Longest samples
preserve cross-component attribution rather than combining unrelated maxima.

## Complexity and responsibility audit

Let C be all source chunks, A admitted chunks (A <= non-pinned C), V reachable
young objects, B copied payload bytes, E visited reference slots, R published
roots, P lease roots, D remembered owners, and Q old-mark transport entries.
The new admission costs O(C + A) region/metadata operations outside STW, plus
O(A * chunk-size) storage initialization in the allocator. Reserve consumption
is O(1) per destination and unused release O(A) after STW. Header-offset capacity
is reserved once, avoiding growth during survivor copying. Retained vector
capacities follow the largest prior admission; peak extra storage is bounded
by the admitted source capacity. This trades earlier, potentially unused
reservation for removing survivor allocation from the stop.

The existing collector still performs expected O(R + P + D + V + E + Q + C)
index/graph work plus O(B) copying and applicable old-address/tree operations.
Source and destination address indexes, native callbacks, old-region allocation,
and old-epoch refresh retain their existing costs. No claim that all existing
collector work is strictly linear or allocation-free is made. New telemetry
uses constant-sized TLS state and 24 histograms, O(1) time per phase transition
and per stop, and O(1) retained memory. No per-object map, per-exit duplication,
compiler pass, generated code, build scheduling, or artifact caching was added.

Copying B live bytes and inspecting R roots/E edges has an unavoidable lower
bound; these measurements cannot establish a universal constant pause bound.
Deterministic tests count region allocation attempts, metadata visits, graph
bytes, slots, and root values. They include fragmented chunks, duplicate roots,
chains, fan-out, shared edges, repeated collections, leases, cap fallback and
old-cycle overlap. The scaling probe changes live bytes, root aliases, pins,
registered mutator peers, and remembered owner density independently. It does
not infer asymptotic behavior from wall-clock timing alone.

## Reproduction and scope

`python3 docs/audits/gc-minor-survivor-admission/measure.py` archives the public baseline into
a temporary directory without changing HEAD/index, refuses a baseline containing
private requirements, and injects the same opt-in probe into that baseline.
It builds the filtered release test executable without running a full suite.
Only `gc::minor::m4_probe::m4_pause_scaling_probe` executes. The final runtime
source and probe hashes are recorded in `measurements.json`.

The proposed medium fixture has a fixed 16 MiB old payload, 256 old owners,
16 densely packed young chunks (524160 live bytes), 256 aliases to one live
chain, no leases, no additional mutator peers, and 32 remembered owners. Each
sample validates the complete chain after copying. Twenty warmups are discarded;
2000 medium samples and 500 samples per independent variation are retained.
Percentiles use nearest rank; p99.9 for a 500-sample variant equals its maximum.
Worker counts describe registered safepoint-polling mutator peers, not parallel
evacuation workers. Fixture setup and workers' launch/join are outside stop time.
Throughput covers young allocation/root registration/minor collection, not just
copying. Peak RSS is the whole test process from `/usr/bin/time`; it includes
harness/code/allocator retention and is distinct from post-collection managed
reservations. A single paired process run is not a confidence interval.

At the time of exploratory measurement this fixture was newly proposed, not an identified pre-existing published
medium-heap release workload. Its measurements are useful implementation
evidence but cannot silently substitute for the ticket's agreed performance
gate or invent missing throughput/footprint tolerances. It measures first
survival; promotion/old-mark overlap correctness is covered by focused tests,
not claimed as a performance distribution from this fixture.

## Validation

Final commands and outcomes are retained in `validation.txt`, `fmt.txt`, four
`clippy-<target>.txt` files, and `example.txt`. Focused test filters are `m4_`,
`survivor_direct_root_and_budget_fallback_promote_in_place`,
`tenure_budget_failure_retains_survivor_chunk_as_pinned`,
`minor_tracing_work_scales_with_graph_and_roots`,
`minor_survivor_cost_and_page_index_scale`, `gc_telemetry::pause::tests`,
`gc_telemetry::stops::tests`, and `gc::minor::overlap_tests::`.
The ignored release probe is invoked separately, never by an ordinary suite.

The runnable linked-list example uses current-source `cargo run --bin willow`
with minor stress, barrier verification, and region verification enabled.
An old local `willowc` executable was incompatible with the current runtime;
the current Cargo binary is used for the recorded result.

Linux x86_64 native validation is local evidence only. Cross-target Clippy
checks Linux x86_64, Windows MSVC x86_64, macOS arm64 and macOS x86_64. Foreign
native execution/linking and GitHub CI are unverified. Full local suites and
the full example audit were intentionally not run.

## Recorded results — 2026-10-24

Environment: AMD Ryzen 7 7800X3D (8 cores, 16 logical CPUs), Linux x86_64,
glibc 2.43, rustc 1.96.0. No foreign native runs are implied. All results are
environment-specific. Final validation passed: 29 focused tests, fmt, all four
cross-target Clippy checks, and the single stressed linked-list example (`6`).
The full suite and full example audit remain delegated to CI, which was not read.

The initial paired matrix is preserved, including its anomalous large-live
case; no unfavorable sample was dropped. Throughput and RSS deltas below are
candidate versus baseline. Managed post-collection reservations were identical
for each paired fixture. RSS includes the complete test process.

| Variation | Candidate p99 ms | p99.9 ms | Max ms | Throughput delta | Peak RSS delta | Metadata visits | Root values |
|---|---:|---:|---:|---:|---:|---:|---:|
| medium_candidate | 0.5002 | 0.7007 | 0.7244 | +6.79% | +4.85% | 21840 | 256 |
| chunks_1 | 0.0418 | 0.0616 | 0.0616 | +0.30% | +3.94% | 1365 | 256 |
| chunks_64 | 3.3657 | 3.5106 | 3.5106 | -32.44% | +3.09% | 87360 | 256 |
| roots_1 | 0.4819 | 0.4947 | 0.4947 | +11.53% | +4.13% | 21840 | 1 |
| roots_4096 | 0.5327 | 0.6657 | 0.6657 | +0.99% | +3.20% | 21840 | 4096 |
| pins_64 | 0.5653 | 0.5796 | 0.5796 | +2.02% | +2.73% | 23205 | 320 |
| pins_1024 | 0.5460 | 0.5577 | 0.5577 | +2.62% | +3.60% | 23205 | 1280 |
| workers_1 | 0.5356 | 0.5798 | 0.5798 | +2.81% | +3.40% | 21840 | 256 |
| workers_4 | 0.6956 | 0.7198 | 0.7198 | +2.80% | +2.07% | 21840 | 256 |
| workers_8 | 0.5547 | 0.6637 | 0.6637 | -1.29% | +2.53% | 21840 | 256 |
| remembered_0 | 0.4647 | 0.4817 | 0.4817 | +28.02% | +3.40% | 21840 | 256 |
| remembered_256 | 0.5481 | 0.6986 | 0.6986 | +2.43% | +3.06% | 21840 | 256 |

The single large-live run showed a 32.44% throughput decline. Five additional
paired runs, reversing execution order on alternating trials, did not reproduce
it. This is evidence against a stable 32% implementation regression, not proof
of the transient's cause. The original samples remain in `samples.jsonl`.
Repeated measurements and raw samples are in `replicates.json` and
`replicates-samples.jsonl`. Reproduce with the same script plus
`--case chunks_64 --case medium_candidate --trials 5`.

| Repeated case | p99 range ms | p99.9 range ms | Worst max ms | Median throughput delta | Median peak RSS delta |
|---|---:|---:|---:|---:|---:|
| medium_candidate | 0.4860–0.5278 | 0.5921–0.6980 | 0.8494 | +2.37% | +3.78% |
| chunks_64 | 1.8870–1.9163 | 1.9160–2.1589 | 2.1589 | +2.62% | +3.71% |

Mean medium component attribution: rendezvous 0.51 µs (0.11%), roots 0.85 µs (0.18%), remembered 1.15 µs (0.24%), evacuation 463.86 µs (98.15%), metadata 5.82 µs (1.23%), wakeup 0.43 µs (0.09%).

Evacuation dominates this measured chain. The change removes survivor storage
and bitmap allocation from that interval while preserving the existing fused
copy/reference traversal. Parallel evacuation, card-epoch redesign and root-arena
rewrites were not added without evidence that their complexity is justified.
Operation counts remain proportional to graph size; raising roots from 1 to
4096 keeps metadata visits at 21840. Pins cause additional visits within retained
source chunks (23205 for both measured pin cases), not a roots-times-objects
rescan. Measured count evidence is narrower than a proof for arbitrary native
trace/drop callbacks or arbitrary concurrent mutation.

## Historical acceptance blocker — resolved by explicit workload adoption

Before the explicit user decision, no named medium-heap contract with numeric
throughput/footprint tolerances had been identified. The exploratory fixture
was therefore not treated as an accepted release gate, even though its p99 was
below 2 ms. This was a measurement-contract blocker, not missing foreign native
execution or CI. No substitute ticket was created or acceptance weakened.

The resumed-session integrity check retained all 40,000 exploratory sample rows,
checked each recorded component partition, and confirmed the then-current
runtime hash matched the exploratory reports. Those historical reports and the
original baseline directory remain intact. The user subsequently adopted the
fixture and budgets explicitly; the following confirmation supersedes that
blocker. Historical samples are not relabeled as post-contract measurements.

## Fixed-contract confirmation after user adoption

The prior contract blocker is resolved by explicit user adoption AFTER viewing
exploratory results. contract.md freezes definitions and gates before new runs;
confirmation-lock.json identifies the pre-run contract and probe. CPU clocks
wrap the full synchronous collection, including admission/cleanup, identically
in both builds. Original baseline/session and unfavorable samples remain intact.

confirmation-01/report.md reports five fresh independent process pairs and
all p99/p99.9/max, throughput, RSS and GC CPU/byte values, with descriptive
paired bootstrap intervals. All ten processes succeeded; no failed runs were
dropped. The fixed gates pass: median throughput ratio 1.005950, median
CPU/byte ratio 0.991460, worst-process peak footprint ratio 1.029982, and every
candidate p99 below 2 ms. These are environment-specific point-estimate gates;
confidence intervals and limited tail precision are reported, not suppressed.

The only runtime source change since original validation is test-only CPU
measurement in the opt-in probe. Four-target Clippy, fmt and focused M4,
component and overlap tests are revalidated in confirmation-validation.txt.
Independent-axis exploratory data remain applicable to unchanged production
collector code; no dimensions were replaced by the medium-only confirmation.

Final confirmation validation completed: fmt, four-target workspace/all-target
Clippy and 17 focused tests passed; the ignored probe was measured separately
in ten fresh release processes. The original additional focused test coverage
and stressed runnable example remain recorded. Contract/probe pre-run hashes
were checked unchanged after measurement.

## Additional independent reviews

Three user-authorized independent reviewers approved the final implementation
and evidence with no blocking findings. Each received only the path to the
local review scope, read the full relevant source/audit/evidence, and independently
recomputed the 20,000 fresh samples. The baseline injection and source/contract
hashes were checked. Two reviewers additionally reran the 17 focused tests;
the third inspected recorded validation. No source changes were requested.
Review records remain in the current local ticket-loop session directory.

All reviewers preserved the limitations: adopted workload after exploratory
results, Linux-only native measurements, descriptive CPU confidence interval
crossing 1.0, and pre-admission limited to survivor storage/per-chunk metadata.
The fixed point-estimate gates pass; this is not proof of universal improvement.
Per user instruction the ticket is still open and nothing is staged/published.
