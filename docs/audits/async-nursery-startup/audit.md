# Async nursery startup implementation audit

Date: 2026-10-21. Baseline: `a2f023c42d7d1b9db38d4369c18c2250f0470eb5`.
Implementation is ready for review; this report does not authorize ticket closure.

## Cause and change

Scheduler entry widened the cold nursery from 256 KiB to 4 MiB per configured
worker, capped at 32 MiB. Worker capacity does not imply concurrent allocation:
one async main calling a synchronous allocator received that entire runway.
Low-survival feedback eventually shrank it, but peak RSS already reflected the
large first nursery. The major cycle and minor deferral could add further overlap.
A baseline GC trace observed the first minor at 34,022,200 occupied bytes.
As a diagnostic intervention, setting `WILLOW_GC_NURSERY_BYTES=262144` reduced
10M-iteration async RSS from 43,232 to 8,968 KiB before editing runtime policy.
These diagnostic samples are distinct from the three-repeat table below.

`gc::nursery::scheduler_start` now preserves the existing nursery threshold.
The major-cycle startup budget is unchanged and its helper is renamed to make
that distinction explicit. Environment override and hard-limit initialization
remain in `Policy::initial`; later survivor feedback still uses `Policy::next`.
No allocation, root, relocation, generated-code, runtime ABI, or scheduler
ownership logic is changed. No speculative worker-scaled young storage is needed.

The original ticket's 110–423 MB measurements were not reproduced exactly on
this newer baseline, which already includes pending-minor and recovery fixes.
There was still a reproducible 39–80 MB async peak versus approximately 3.4 MB
sync. This session fixes that remaining startup-policy difference; it does not
attribute earlier fixes to this change.

## Measurement

Linux x86_64, 16 available CPUs, rustc 1.96.0. Release compiler and runtime.
Both archives were built from the same tree/compiler, before and after the
runtime edit. Each row is the median of three fresh processes; raw samples are
in `before.jsonl` and `after.jsonl`. `/usr/bin/time` reports peak RSS in KiB.
GC overrides are removed from the measurement environment. Default workers are
host-dependent; explicit 1/4-worker rows make that dimension reproducible.
Timing is descriptive, not an asymptotic proof or a controlled throughput claim.

| Iterations | Workers | Shape | Sync after KiB | Async before KiB | Async after KiB | Async seconds before → after |
|---:|---|---|---:|---:|---:|---|
| 0 | default | call | 3324 | 3336 | 3436 | 0 → 0 |
| 1,000,000 | default | call | 3436 | 38980 | 6128 | 0.04 → 0.04 |
| 10,000,000 | default | call | 3332 | 44384 | 8940 | 0.38 → 0.37 |
| 40,000,000 | default | call | 3440 | 44960 | 9200 | 1.48 → 1.48 |
| 120,000,000 | default | call | 3440 | 44728 | 9200 | 4.46 → 4.43 |
| 10,000,000 | 1 | call | 3440 | 12492 | 3568 | 0.2 → 0.2 |
| 10,000,000 | 4 | call | 3440 | 22400 | 4488 | 0.21 → 0.2 |
| 40,000,000 | default | inline | 3448 | 80288 | 9168 | 2.29 → 2.18 |

Every candidate sample passes `async RSS < 2 * minimum paired sync RSS + 4096 KiB`.
The fixed 4 MiB allowance includes runtime task/worker/collector overhead for
this workload, not an allowance proportional to iterations. It is an empirical
regression bound, not a universal memory guarantee for arbitrary task counts.
The maximum candidate async sample is 9,296 KiB. The zero-iteration probe does
not exercise lazy GC/worker state and therefore is not the full runtime overhead.

Every run validates both nodes in all 64 retained slots using the exact checksum
`128*N - 4096` (zero for the empty probe). Each nonempty program creates exactly
`2*N + 64` Node instances at source level, including the initialization ring.
The test records actual minor collection counts as well. For default workers,
sync counts at N=1M/10M/40M/120M are 305/3052/12209/36629; candidate async medians
are 304/3048/12195/36583. These operation counts demonstrate collection progress
scaling with allocation, while the bounded live graph and RSS plateau remain
independent of N. They do not infer complexity from elapsed seconds alone.

## Reproduction

Save the baseline runtime archive **before** applying the change (or build it
in a separate baseline checkout). Keep binaries and archives outside docs.
No Git history mutation is needed. From the project root:

```sh
rtk cargo build --release --bin willow
rtk cargo build --release -p willow_runtime
rtk proxy cp target/release/libwillow_runtime.a /tmp/nursery-startup-before-runtime.a
# Apply the runtime change after preserving the baseline archive.
rtk proxy python3 docs/audits/async-nursery-startup/measure.py --runtime /tmp/nursery-startup-before-runtime.a --output /tmp/nursery-startup-before.jsonl --label before
rtk cargo build --release -p willow_runtime
rtk proxy python3 docs/audits/async-nursery-startup/measure.py --runtime target/release/libwillow_runtime.a --output /tmp/nursery-startup-after.jsonl --label after
```

`measure.py` creates temporary sources from `example/async_allocation_churn.wi`,
links the selected runtime explicitly, checks outputs, and deletes build outputs.
The standalone example uses only 100,000 iterations. Its checksum is 12,795,904;
its collection count is deliberately not a portable exact expectation.

## Implementation and complexity audit

Reviewed scheduler drive → GC startup → policy initialization, generated TLAB
slow allocation → threshold check → minor collection → survivor feedback, and
major pacing. Startup is guarded once and skips warmed/active collectors.
The edit removes a threshold assignment; startup remains O(1) time/space and
adds no per-allocation work, generated instructions, scans, or allocations.
Policy decisions remain O(1). For A allocated bytes and fixed nursery budget B,
low-survival workloads request O(A/B) minors; each minor processes its young
heap, reachable graph, roots, and existing registry work. The latter costs are
unchanged and not claimed to be O(1). Root/task count R, live graph size L,
concurrent allocators W, and total allocation A are independent dimensions.

For this fixed 64-slot graph, live edges are bounded and measured collections
grow proportionally to A. Changing the starting B changes a constant startup
cost, not generated-code size or asymptotic allocation work. High-survival
feedback can double B from 256 KiB to the 32 MiB cap in seven cycles. Tests cover
that feedback and empty-cycle/dead-band behavior rather than adding a new
worker-pressure estimator or another scan. Pending-minor debt remains bounded
by its captured budget; the existing focused deferral tests cover concurrent
refills, ownership waits, hard caps, saturation, and release of pinned roots.
The measurement validator indexes paired sync minima once, O(samples) time and
O(cases) additional space, rather than rescanning samples for each async run.

Tradeoff: a cold workload with many simultaneously allocating tasks can now
perform earlier minor stops. High-survival workloads still adapt upward, and
major pacing stays unchanged. This patch does not claim a many-task throughput
benchmark. Increasing task counts/large live graphs are outside the fixed-live-set
RSS acceptance measurement; focused collector tests protect their correctness.

## Validation perspectives and results

The 36-case new policy regression crosses six worker capacities (0,1,2,8,64,max),
three nursery overrides (default,8 KiB,4 MiB), and two hard limits (none,4 KiB).
Relevant distinct perspectives also include: (1) compact default startup,
(2) one worker, (3) several workers, (4) saturated worker capacity,
(5) explicit small nursery, (6) explicit large nursery, (7) hard limit below floor,
(8) startup only once, (9) warm minor history, (10) warm major history,
(11) active concurrent cycle, (12) survival growth, (13) survival shrink,
(14) dead band, (15) empty cycles, (16) override persistence,
(17) allocation stress, (18) retained references after relocation,
(19) deferred unsafe roots, (20) pending-owner refill limits,
(21) major/minor overlap, (22) async frame/container roots,
(23) inline async allocation, (24) synchronous callee allocation,
(25) constant live set across increasing total allocation, and (26) empty startup.

Passed:

- `cargo fmt --all -- --check` and `git diff --check`.
- `cargo test -p willow_runtime --lib gc::nursery:: -- --nocapture`: 8 passed.
- `cargo test -p willow_runtime --lib gc::minor::deferral_tests:: -- --nocapture`: 10 passed.
- `cargo test -p willow_runtime --lib gc::minor::overlap_tests::overlap_ -- --nocapture`: 9 passed.
- `cargo clippy --workspace --all-targets --target TARGET -- -D warnings` for
  `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
  `aarch64-apple-darwin`, and `x86_64-apple-darwin`.
- Both measurement sets: 48 processes each, exact checksums; all 24 candidate
  async samples satisfy the RSS bound. Each set has 16 separately built cases.
- Standalone release example: checksum 12,795,904.
- Reduced 1,000-iteration example under `WILLOW_GC_STRESS=relocate`,
  `WILLOW_GC_VERIFY_BARRIER=1`, `WILLOW_GC_VERIFY_REGIONS=1`: checksum 123,904,
  2,066 minor collections. Generate it by replacing `churn(100000, keep);`
  with `churn(1000, keep);` in the example, then build/run with those variables.
  The initial 100,000-iteration relocation/region-verification run was deliberately
  stopped after about two minutes without a result; only the reduced stress
  case is claimed as passed. Ordinary 100,000-iteration execution passed.

All commands were invoked via `rtk`/`rtk proxy`.

No local full-suite run or full example audit; CI was not queried. Native run/RSS
validation is Linux x86_64 only. Windows and both macOS architectures are not
natively executed; cross-target Clippy checks compile/lints only.
