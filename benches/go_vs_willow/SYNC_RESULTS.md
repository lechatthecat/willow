# Synchronous results: Willow vs Go 1.27.0

Measured on 2026-09-12 at Willow commit `b277388b12d768de84222f1a6119e70d1bbab1a2`.
The working tree was clean before the original measurement.

Fibonacci was remeasured on 2026-09-12 21:32 JST using candidate snapshot
`70fb2d6582a729ea584ba357639fa599b9e79ab5`. The current working-tree source
matches that candidate. Release compilers/binaries were rebuilt. Five fresh-process
trials per variant used eight workers and rotated baseline/current/Go launch order.
The Fibonacci rows below use this run; other rows retain the original measurements.
See the [matched impact comparison](RECURSION_IMPACT_2026-09-12.md) and
[all 55 new trial records](RECURSION_IMPACT_2026-09-12.jsonl).
All four required native platforms passed [CI for this candidate](https://github.com/lechatthecat/willow/actions/runs/34693081610).

Willow measured 304.813 ms versus Go 406.079 ms: Willow is 1.33x faster here.
The source still uses the same naive recursive recurrence and input. Small pure
recursive bodies are now expanded by up to three levels within a fixed code-size
budget, in addition to the earlier poll/cancellation optimizations. GC/preemption
and native-task cancellation remain enabled. The separate matched comparison of
the other four synchronous cases found changes from -2.9% to +0.15% in median wall time.
Earlier [TRE-only](FIBONACCI_2026-09-12_RERUN.jsonl) and
[poll-only](FIBONACCI_2026-09-12_OPTIMIZED.jsonl) records are preserved.

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

The harness alternates language launch order and validates every output. All 50
synchronous trials passed. Wall time includes process startup and shutdown;
GNU `time` supplies user/system CPU time and peak RSS. Benchmark bodies are
synchronous, with the same eight-worker runtime limit as the scheduler suite.

## Results

Time cells are `wall / user / system` in milliseconds. RSS is median peak RSS.

| Benchmark | Fixed work | Willow time (ms) | Go time (ms) | Faster | Willow RSS | Go RSS |
|---|---|---:|---:|---|---:|---:|
| leibniz_pow | 100,000,001 terms | 5112.359 / 5110 / 0 | 6546.172 / 6550 / 0 | Willow 1.28x | 2.40 MiB | 1.87 MiB |
| leibniz_reduced | 100,000,001 terms | 109.289 / 100 / 0 | 99.482 / 90 / 0 | Go 1.10x | 2.28 MiB | 1.87 MiB |
| fibonacci | fib(40) | 304.813 / 300 / 0 | 406.079 / 400 / 0 | Willow 1.33x | 2.28 MiB | 1.87 MiB |
| array_sum | 5,000,000 i64 values | 311.738 / 260 / 40 | 51.095 / 50 / 30 | Go 6.10x | 130.77 MiB | 77.48 MiB |
| linked_list | 1,000,000 nodes | 1310.592 / 1250 / 60 | 25.463 / 20 / 0 | Go 51.47x | 116.86 MiB | 18.25 MiB |

## Interpretation

Willow is 1.28x faster in the exponentiation-based Leibniz case. This includes
the cost of each language’s power implementation. With exponentiation replaced
by a sign flip, Go is 1.10x faster. Willow is 1.33x faster for recursive Fibonacci.
Go is 6.10x faster for dynamic-array construction and traversal, and 51.47x
for linked-list construction and traversal.

The collection cases combine allocation, object representation, GC bookkeeping
and access costs; they do not isolate collector pause time. Whole-process timing
also includes Willow runtime initialization and its configured workers.

## Reproduce

```sh
python3 benches/go_vs_willow/run_sync.py leibniz_pow leibniz_reduced fibonacci array_sum linked_list --trials 5
```

Fibonacci-only rerun after rebuilding the compiler and benchmark binaries:

```sh
python3 benches/go_vs_willow/run_sync.py fibonacci --trials 5 --workers 8 --no-build
```
