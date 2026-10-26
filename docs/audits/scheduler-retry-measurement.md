# NON-09 retry measurement and implementation audit

Date: 2026-10-24. Source baseline: `e4fd8b17fdb26f5d5d278bc2ce07b496caf58dbc`.
Host: Linux x86_64, Ryzen 7 7800X3D, 8 cores / 16 logical CPUs;
Rust 1.96.0. Native foreign-platform runs were not performed.

## Decision and scope

Retain the production retry policy. This is the ticket's evidence-backed
no-production-change option, pending independent review; the ticket remains open.
The change adds opt-in measurements and a deterministic instrumentation test.
It does not introduce a retry cap, yielding policy, or different queue selection.

Both retry helpers are still unbounded under adversarial contention. Finite
measurements cannot prove lock-free per-worker starvation freedom. A cap that
returns `None` would conflate contention with an empty queue at callers including
`LocalRunQueue::{pop,steal}`, global refill, and maintenance drains. A new policy
requires evidence of benefit and a separate correctness argument at those paths.

## Method

Run `bash docs/audits/scheduler-retry-measurement-reproduce.sh` from the repository root.
The checked-in raw output accompanies this report. The measurement is an ignored,
release-mode unit test using the real `RunQueues` and real retry helpers, not a
copy of the queue algorithm. No shared per-retry atomics or timed-loop logging
are introduced. Per-thread histograms are merged only after joining workers.

The matrix has three repetitions, 1/4/16 consumers, and 4,096/32,768/262,144 tokens
per producer, in four modes (108 samples):

- `wake`: one producer publishes small groups of eight, yielding between groups.
- `fan_in`: 1/4/16 simultaneous producers publish into the global injector.
- `owner`: thieves drain a queue concurrently populated through its owner deque.
- `inbox`: a local owner batches its inbox while other consumers steal from it.

These are queue-level wake/fan-in models, not an end-to-end channel application.
The reproduction also runs an existing real-scheduler failed-probe/publication
race test. The single-task measurement is queue publication-to-pop latency;
it must not be described as sleeping-worker wake-to-poll latency.

Each helper invocation contributes one retry-chain sample, including zero-retry
and empty outcomes. `single` covers both `Injector::steal` and `Stealer::steal`;
`batch` covers `steal_batch_and_pop`. p99 uses nearest rank. Bin 255 is an overflow
bucket, explicitly reported; max and total retries remain exact. A row with zero
calls has no distribution (its printed p99/max placeholders are zero).

Worker CPU uses Linux `CLOCK_THREAD_CPUTIME_ID` around the consumer loop.
Efficiency is completed tokens / summed consumer CPU seconds; producer CPU,
verification and thread startup are excluded. Wall time includes the barrier and
joins. Per-worker completion counts expose imbalance but are not a starvation
proof. Instrumentation, clock/deadline checks and verification-buffer writes
perturb timings. These are diagnostic observations, not production throughput
claims or comparisons against a hypothetical backoff implementation.

## Results

All 108 samples passed exact-once verification: **26,910,720 tokens**, zero
missing or duplicate IDs, zero final queue lengths. No histogram overflow was
observed. The table takes maxima of each sample's p99/max (not a pooled p99),
and the median efficiency across nine samples per row (three sizes × three
repetitions). For `owner`, batch calls are absent; printed zero is a placeholder.

| Mode | Workers | Worst sample single p99 / max | Worst sample batch p99 / max | Median tokens / worker CPU second |
|---|---:|---:|---:|---:|
| wake | 1 | 0 / 0 | 0 / 0 | 9,982,956 |
| fan_in | 1 | 0 / 0 | 0 / 0 | 21,459,782 |
| owner | 1 | 0 / 1 | 0 / 0 | 13,492,858 |
| inbox | 1 | 0 / 0 | 0 / 0 | 17,854,625 |
| wake | 4 | 5 / 17 | 0 / 0 | 1,787,371 |
| fan_in | 4 | 6 / 23 | 0 / 0 | 2,468,659 |
| owner | 4 | 6 / 17 | 0 / 0 | 2,213,814 |
| inbox | 4 | 3 / 11 | 3 / 14 | 2,535,447 |
| wake | 16 | 13 / 31 | 0 / 0 | 369,665 |
| fan_in | 16 | 19 / 76 | 0 / 0 | 676,374 |
| owner | 16 | 12 / 38 | 0 / 0 | 410,300 |
| inbox | 16 | 25 / 100 | 24 / 40 | 527,772 |

The single-task queue probe recorded p50 **40 ns**, p99 **110 ns**, max **180 ns**.
This is a diagnostic baseline, not an A/B latency comparison. Non-test code has
no behavioral change, so no new low-load regression is introduced by this patch.

Contention clearly lowers worker efficiency as consumer count rises; the data
does not attribute all of that loss to retry spinning (queue atomics, cache-line
sharing, unsuccessful queue scans, scheduling and instrumentation also cost CPU).
The 16-worker fan-in additionally has 16 producers on a 16-logical-CPU machine.
Observed retry chains remained short (single max 100, batch max 40), and the
runs completed without lost work. These measurements do not establish a
pathological long-spin case or demonstrate a throughput benefit from a cap.
Consequently the conservative outcome is to retain the policy, with evidence
for revisiting it if a representative workload shows long-tail spinning. This
is not a claim that backoff could never improve performance, that fairness is
bounded, or that production workloads share this distribution.

Five narrowly selected tests passed: the deterministic retry instrumentation
check, the ignored contention measurement, the real-scheduler publication race,
concurrent publisher/stealer accounting, and concurrent global-batch token
uniqueness. The raw output records each exact test name and result.


## Correctness and implementation audit

All measurement publications have unique IDs. After workers join, a linear
bitmap pass rejects out-of-range IDs and double processing, checks every ID was
seen, and checks queue length is zero. The producer-completion acquire load
precedes the empty probe, avoiding a final-publication/termination race.
Consumers retain their owner guard while draining private refill batches.

The deterministic test forces 0/1/8/64/256/4096 retries before both success and
empty. It asserts exactly K+1 underlying steal calls and exactly K recorded
retries, including overflow-bin accounting. Batch success and empty paths are
also checked. The production helpers retain success/empty return semantics and
all existing queue-length decrements and ownership boundaries.

Independent dimensions are total tokens N, workers W, producers P, helper calls H,
and retries R. Added measurement work is O(H+R) during execution and O(N+256W)
for exact-once verification and histogram merge. Storage is O(N+256W+P): initial
per-consumer buffers reserve ceil(N/W), grow amortized, and hold N IDs in total;
there is no N-sized buffer per consumer. Low-load quantiles separately sort
10,000 timestamps in O(L log L), outside the timed samples. Exact retry counts
require observing each retry; exact-ID verification requires inspecting each
published token. The test does not add repeated scans of all outputs.

Existing scheduler cost remains input- and contention-dependent: unsuccessful
victim traversal can inspect O(W) queues per pop and each helper can perform K+1
steal attempts. There is no finite worst-case retry bound, nor a claim that the
existing scheduler has minimal total complexity. The deterministic increasing-K
test validates linear instrumentation operation counts; the N/W measurement
matrix supplies increasing workloads without inferring complexity from timing.

Every probe construction, increment and the measurement module is `cfg(test)`.
Non-test compilation removes all added state/operations; the remaining helper
bodies differ only by braces around the same `spin_loop()` statement. Thus no
production latency, code-size or memory change is introduced. This conclusion
is source/cfg reasoning, not a binary-identity measurement. Compiler analysis,
build scheduling and runtime auto-build responsibilities are unchanged.

## Validation

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- Explicit cross-target workspace/all-target Clippy: passed for
  `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
  `aarch64-apple-darwin`, and `x86_64-apple-darwin`.
- Focused Linux tests and release measurement: see raw output and reproduction.
- Windows and both macOS targets: type/lint checked only; native execution
  unverified. CI was not queried. Full workspace tests and the full example
  audit were deliberately not run. No compiler/language behavior changes.
