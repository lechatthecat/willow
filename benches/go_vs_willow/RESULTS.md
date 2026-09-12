# Results: Willow vs Go 1.27.0

Measured on 2026-09-12 at Willow commit `b277388b12d768de84222f1a6119e70d1bbab1a2`.
The working tree was clean before measurement.

- CPU: AMD Ryzen 7 7800X3D, 8 physical cores / 16 threads
- OS: Linux 7.0.0-31-generic, amd64
- Go: go1.27.0 linux/amd64
- Builds: freshly rebuilt release Willow compiler and release benchmark binaries;
  optimized Go binaries
- Concurrency: `WILLOW_WORKERS=8`, `GOMAXPROCS=8`
- Statistic: median of five fresh-process trials
- Build time excluded; CPU affinity and frequency not pinned, background services active

These are machine-specific microbenchmarks. See the
[combined measurement](MEASUREMENT_2026-09-12.md) and
[raw trial records](MEASUREMENT_2026-09-12.jsonl).

## Task footprint and spawn

RSS/task is `(RSS after all tasks parked - baseline RSS) / task count`.

| Parked tasks | Willow spawn | Go spawn | Willow RSS/task | Go RSS/task |
|---:|---:|---:|---:|---:|
| 100,000 | 144.147 ms | 119.952 ms | 796.79 B | 2743.75 B |

Willow used 75.99 MiB incremental RSS (79.02 MiB total), versus Go’s
261.66 MiB (264.09 MiB total): 71.0% less incremental RSS. Go completed
task creation 1.20x faster. Median phase CPU time was 770 ms for Willow and
280 ms for Go. Other task counts were not measured in this run.

## Scheduler and channel throughput

Timings measure marked phases. The harness runs five Willow trials followed by
five Go trials. CPU time is read from Linux process ticks with 10 ms resolution.

| Benchmark | Work | Willow wall / CPU | Go wall / CPU | Go wall advantage |
|---|---|---:|---:|---:|
| wake_fanout | 100k tasks, private channel/task | 210.036 / 910 ms | 13.670 / 90 ms | 15.37x |
| yield_switch | 100k tasks × 100 yields | 6720.312 / 59390 ms | 1542.974 / 9880 ms | 4.36x |
| ping_pong | 1M round trips | 3936.811 / 7970 ms | 165.628 / 180 ms | 23.77x |
| gc_scheduler | 10k tasks × 100 allocation/yield rounds | 5229.646 / 14710 ms | 141.206 / 1010 ms | 37.04x |

The wake case includes 100k sends to private capacity-1 channels and fan-in of
100k completion messages. It measures burst wake-to-completion, not broadcast
or per-task resume latency.

## GC plus scheduler

Each worker keeps an allocated object live across each yield, for one million
retained allocation/yield operations.

| Metric | Willow median | Go median |
|---|---:|---:|
| Allocation/yield operations per second | 191,218 | 7,081,851 |
| Minor / major GC cycles | 187 / 2 | n/a |
| Go GC cycles | n/a | 3 |
| Promoted / moved Willow objects | 997,736 / 997,736 | n/a |
| Go total allocated bytes | n/a | 22,465,120 |
| Go total / max GC pause | n/a | 251.571 / 103.374 µs |

GC counters are not directly equivalent between runtimes. Willow does not expose
GC pause duration or GC CPU percentage through its language ABI.

## Scope

This run covers the five scheduler cases above. It does not measure task resume
latency percentiles, HTTP/WebSocket, large connection counts, or shared-channel
contention. Reproduction commands are in the [combined report](MEASUREMENT_2026-09-12.md).
