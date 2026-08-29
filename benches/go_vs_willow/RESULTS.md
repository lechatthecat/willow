# Results: Willow vs Go 1.27.0

Measured on 2026-08-29 at Willow commit `c660b36`.

- CPU: AMD Ryzen 7 7800X3D, 8 physical cores / 16 threads
- OS: Linux 7.0.0-30-generic, amd64
- Go: go1.27.0 linux/amd64
- Concurrency: `WILLOW_WORKERS=8`, `GOMAXPROCS=8`
- Builds: Willow and Go benchmark binaries compiled with release settings
- Statistic: median of 3 fresh-process trials

These are local microbenchmark results, not universal language rankings. CPU
frequency was not pinned and background services were not disabled. CPU time
comes from Linux process ticks and has 10 ms resolution.

## Task footprint and spawn

RSS/task is `(RSS after all tasks parked - baseline RSS) / task count`.

| Parked tasks | Willow spawn | Go spawn | Willow RSS/task | Go RSS/task |
|---:|---:|---:|---:|---:|
| 1,000 | 1.105 ms | 1.126 ms | 827 B | 2,978 B |
| 10,000 | 13.386 ms | 11.585 ms | 753 B | 2,743 B |
| 100,000 | 143.581 ms | 122.439 ms | 713 B | 2,743 B |
| 1,000,000 | 1.808 s | 1.093 s | 928 B | 2,735 B |

At 100k, Willow's median RSS delta was 68.0 MiB and its total ready RSS was
71.2 MiB. Go's median delta was 261.6 MiB and total ready RSS was 264.1 MiB.
Willow therefore used 74.0% less incremental RSS (3.85x as many parked tasks
per byte), while Go's spawn time was 14.7% lower. Approximate process CPU time
for the spawn phase was 630 ms for Willow and 320 ms for Go.

At 1M, Willow used 885.1 MiB incremental RSS versus Go's 2,608.1 MiB, a 66.1%
reduction. Go's spawn time was 39.6% lower. Willow's per-task RSS rises between
100k and 1M, so the footprint is not a single constant across scales.

## Scheduler and channel throughput

| Benchmark | Work | Willow wall / CPU | Go wall / CPU | Go wall advantage |
|---|---:|---:|---:|---:|
| Burst wake-to-completion | 100k tasks, private channel/task | 195.350 ms / 830 ms | 13.394 ms / 90 ms | 14.6x |
| Cooperative yield | 100k tasks × 100 yields | 6.718 s / 52.34 s (1.49M/s) | 1.710 s / 10.45 s (5.85M/s) | 3.93x |
| Channel ping-pong | 1M round trips | 3.370 s / 3.46 s (297k/s) | 166.972 ms / 180 ms (5.99M/s) | 20.2x |

The burst-wake case uses one private capacity-1 channel per worker in both
languages. It includes 100k sends and fan-in of 100k completion messages. It is
not a zero-work broadcast and does not report per-task p50/p99 resume latency.

## GC plus scheduler

The workload starts 10k tasks; each allocates and retains one small object across
each of 100 yields, for one million retained allocations/yields total.

| Metric | Willow median | Go median |
|---|---:|---:|
| Total wall / CPU time | 5.603 s / 10.65 s | 138.900 ms / 990 ms |
| Allocation/yield operations per second | 178k/s | 7.20M/s |
| Minor / major GC cycles | 187 / 5 | n/a |
| Go GC cycles | n/a | 4 |
| Promoted/moved Willow objects | 996,584 / 996,584 | n/a |
| Go total allocated bytes | n/a | 22,458,016 |
| Go median total / max GC pause | n/a | 215 µs / 75 µs |

Go completed this particular allocation-and-yield workload 40.3x faster. The GC
telemetry rows are not direct cross-runtime equivalents: Willow's
`gc_allocated_bytes` describes its managed heap state, whereas Go's
`TotalAlloc` is cumulative allocation. Willow currently exposes collection and
movement counts to Willow programs, but not GC pause duration or GC CPU percent,
so those two values cannot yet be reported fairly for Willow.

## Shared-channel contention finding

An initial version parked every Willow task on one shared channel and then sent
N messages. Willow intentionally drains/wakes every registered receiver on each
send (`channel.rs` tests require this), while Go wakes one receiver. The workload
therefore creates repeated empty wakeups in Willow and is not an equivalent
comparison.

It is still a useful Willow scalability finding: one trial took 8.69 ms at 1k,
41.0 s at 10k, and exceeded the 180 s timeout at 100k. Applications should avoid
using repeated sends to one channel as a large waiter fan-out until this runtime
behavior is optimized or a true broadcast primitive is available.

## Not measured yet

- p50/p95/p99 task resume latency: Willow has no public monotonic high-resolution
  clock in the language ABI, so the current harness measures whole phases only.
- HTTP and WebSocket: Willow exposes asynchronous TCP, but no standard HTTP or
  WebSocket protocol layer. A fair test needs a shared protocol implementation
  or an explicitly scoped raw-TCP benchmark.
- 100k loopback connections: local ephemeral-port and file-descriptor limits
  must be recorded or adjusted before treating the result as a runtime limit.
- GC CPU percentage and Willow maximum pause: not exposed by current telemetry.
