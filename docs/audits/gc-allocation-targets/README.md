# GC allocation throughput audit

Date: 2026-10-26. Native host: Linux x86_64, AMD Ryzen 7 7800X3D.
Baseline: `22778abdc7f3d816922b0d1a12c8086a4e66434c`, initially clean.
Implementation is uncommitted and awaits independent review. No CI queried.

## Acceptance and provenance

The previous reviewed delivery measured 99.615 ns/node, list/Go 4.659,
scheduler/churn 1.329, scheduler/Java 3.785, interface setup 197.643 ms,
and chain spread 2.286%. Thus its intermediate 150 ns/node, list ratio,
Java ratio, interface, and stability limits passed; the original 60 ns/node
and 1.3 scheduler/churn limits remained unmet. Those historical results are
not measurements of today's source. The saved reconstructed workloads were
reused, not replaced with smaller workloads. The current general benchmark
entry point had since changed; `assets/harness.js` preserves the allocation
comparison and `assets/probe.js chain` preserves the independent chain probe.
Only their repository-root resolution and public comments were adapted.

All comparisons use the same CPU affinity 0–7 and eight workers for all
languages. Five fresh-process trials per case/language, checked checksums,
no GC configuration overrides. Go 1.27.0; OpenJDK 26.0.2. Timing observes
newline markers at the parent process, so includes pipe delivery jitter.
Builds and validation do not overlap timed trials. No discarded outliers.

| Metric | Current baseline | Final | Original limit |
|---|---:|---:|---:|
| 3M reachable-node build | 140.297 ns/node | 59.456 ns/node | ≤60 (also ≤150 intermediate) |
| linked list / Go, whole workload | 5.653 | 2.744 | ≤5 |
| scheduler / churn, per allocation | 0.972 | 1.020 | ≤1.3 |
| scheduler / Java | 1.891 | 1.953 | ≤5 |
| 1M interface setup | 268.990 ms | 183.522 ms | ≤200 |
| Independent 3M chain build | 140.225 ns/node | 59.287 ns/node | ≤60 |
| Independent 3M chain spread, (max−min)/median | 14.122% | 2.046% | <10% |

Final chain medians at 300K/1M/3M nodes: 46.462/23.901/59.287 ns/node.
These nonmonotonic costs reflect collection thresholds, not constant per-node
runtime across all sizes. The final 3M chain has 3 minor / 1 major collections,
1,921,374 moved objects and 1,685,502 promotions, versus baseline 4 minor /
4 major, 3,599,505 moves and 1,914,003 promotions. The 60 ns result has a small
margin; this is host/workload evidence, not a portable performance guarantee.
The final scheduler trial range includes 145.803 ms versus median 85.597 ms
(77.44% spread). Its median ratios pass; no scheduler variance claim is made.

## Causes and changes

1. A promotion burst could overtake the previous goal. Inter-cycle growth was
   clamped to twice that stale goal, then only raised to live itself. A completed
   scan therefore bought zero allocation progress. Baseline logs show adjacent
   approximately 67 MB major scans with zero bytes reclaimed. The goal now has
   the same minimum progress floor as the trigger, before memory-limit clamping.
2. Major occupancy excludes nursery bytes, but the major traverses nursery
   references to find old edges. The former floor used only old live bytes and
   failed to amortize that traversal. The new floor is
   `live + max(4 * expected_work, live / 4, 256 KiB)`, with saturating arithmetic.
   Work includes the existing root/barrier accounting. This intentionally budgets
   one byte of repeated trace work per four bytes of allocation progress. It is
   not a CPU duty guarantee. Fixed-goal allocation samples retain their goal;
   explicit pressure/failure, soft limits, and hard reservation control still win.
3. Two consecutive ≥87.5% survival observations now select direct tenuring,
   rather than three. <75% survival resets immediately; the intervening band and
   empty samples preserve the state. This avoids a large intermediate survivor
   population during rapid nursery growth. Destination generation changes;
   roots, forwarding, descriptors, remembered edges, admission and reclamation
   retain their existing correctness paths.
4. A synchronous mark drain resolves its collector role once instead of querying
   the OS-thread identity at every object; stop, closure, deadline, and recovery
   flags are still sampled at their original boundaries. No worker-registration
   or queue-publication changes. Checked epoch/chunk/type-cache helpers are
   marked inline, keeping their full bounds and cache-key checks.

Intermediate evidence is retained: runway-only build 101.203 ns/node;
two-observation tenuring 60.936; one-work progress interface 227.226 ms;
two-work progress interface 231.890; four-work progress 186.129. These are
investigation checkpoints, not the final source or clean causal A/B estimates.
`final-metadata.json` records final changed-source hashes, the full source-tree
fingerprint, asset hashes and affinity. `results.json` derives all final limits.

## Complexity and implementation audit

Independent dimensions: objects N, reference slots E, roots R, regions C,
trace work W, and allocation progress A. Pacer/tenuring decisions remain O(1)
time and memory. No compiler or generated-code/ABI changes; runtime inlining
adds only a fixed number of call-site copies, independent of program size.
No new graph traversal, allocation, lock, or per-object metadata was introduced.
For stable work W, an unconstrained completed cycle buys at least 4W progress:
repeated-cycle work is O(W + A), rather than immediate repeated O(W) scans at
nearly zero progress. This bound excludes startup estimation, changing graphs,
explicit collections, pressure/limits, fixed-goal samples and saturation.
It does not assert that wall-clock GC cost is linear solely from timings.

The existing graph scan must inspect reachable objects/references, an Ω(N+E)
lower bound without extra reachability knowledge. Two-observation learning
reduces a constant number of copies, not this bound. Caches retain checked
numeric indices; no raw reader survives a callback/safepoint. Reader ownership,
minor remapping, batch-tail publication, and stop/closure ordering were reviewed.
No runtime-to-compiler or build-cache responsibilities moved.

Deterministic focused evidence (debug unless specified):

- Pacing scaling: independent old-live and trace-work sizes 1/16/256 MiB;
  progress ≥4W, no immediate restart, and soft-cap/pressure override asserted.
  Promotion-burst tests cover 1/16/256/4096 MiB, three previous-goal shapes and
  cold/slow/fast rates. Existing extreme/saturation/fixed/CPU tests still pass.
- Survivor shapes: 32/128/512/2048 objects across 1/4/16/64 chunks, exactly N
  moves and no index capacity growth while rebuilding the source page index.
- Promotion indexing: 1/1000/100000 promotions touch 1/1/10 candidate regions.
- Fully forwarded sources: 32/512/8192 objects require exactly 2N debug metadata
  visits (the extra validation pass); no newly introduced per-object lookup loop.
- Chain: 100/1000/10000 nodes use 2/16/157 epoch readers, bounded by batches;
  fanout 1000/10000/100000 preserves shared edges and empties outstanding work.
- Fragmented epoch views: 200/2000/20000 references require 1/10/100 page queries;
  stale/invalid hints, holes, rebuilt starts, and implicit-black new extents checked.
- Tenuring test checks all chain values after every round at 16/1000/10000 nodes,
  direct-promotion copy counts, region metadata, and recovery to young collection
  after a dead allocation wave. Overlap, memory-cap, assist-deadline/tail,
  remark-boundary and panic/unwind tests exercise the affected call paths.

## Memory tradeoff and limits

The new unlimited goal can be larger than twice the prior goal or twice old live
when a previously underestimated trace is expensive. This is intentional and
must not be described as a memory optimization. It exchanges earlier collection
for allocation progress. It does not immediately reserve that amount. Soft caps
can reduce the floor to the cap (live still wins), and hard admission remains
independent. Long-lived objects that die just after two high-survival waves may
now remain old until a major cycle; low survival resets the policy.

Separate final-source Linux measurements (not acceptance timings) record maximum
RSS 196,156 KiB for linked list and 235,488 KiB for interface setup. There is no
matched baseline RSS measurement, so no claim about an RSS improvement/regression
percentage. `memory.json` records commands/checksums; final instrumented GC logs
are diagnostic only. The existing hard-cap reservation/recovery test and all
focused soft-controller tests pass. Work-floor arithmetic saturates instead of
wrapping at extreme sizes. Controller state size remains constant; possible
heap allowance is O(live + W), with the above explicit memory tradeoff.

## Validation and reproduction

`validation.jsonl`: 29 successful commands, 90 passing test executions including
repeated debug/release perspectives; one optional reference-host measurement
remains ignored. fmt and host Clippy pass. Cross-target Clippy passes for Linux
x86_64, Windows x86_64 MSVC, macOS arm64, and macOS x86_64. Native execution was
Linux x86_64 only; macOS/Windows native behavior and CI are unverified.

`examples.jsonl`: four existing runnable GC examples, debug and release, each in
normal, allocation-stress and minor-stress modes (24 successful executions).
No local full workspace/crate/integration suite or complete example audit ran.

From repository root:

```sh
python3 docs/audits/gc-allocation-targets/validate.py
python3 docs/audits/gc-allocation-targets/reproduce.py final
python3 docs/audits/gc-allocation-targets/analyze.py
python3 docs/audits/gc-allocation-targets/examples.py
```

Scripts run checks/benchmarks serially. Select a supported CPU affinity using
`--cpus` if 0–7 is unavailable; report that change instead of equating hosts.
Raw baseline, all intermediate trials, final 60 comparative trials and 15 scaling
trials are retained. Only text/source evidence belongs here; binaries, profiler
binary data and caches stay under target. No private specifications or internal
tracking identifiers are included. No acceptance criterion was weakened.
