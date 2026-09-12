# Go vs Willow measurement — 2026-09-12

- Commit: `b277388b12d768de84222f1a6119e70d1bbab1a2` (clean working tree before measurement).
- AMD Ryzen 7 7800X3D, 8 cores / 16 threads; Linux 7.0.0-31-generic.
- Go 1.27.0; freshly rebuilt release Willow compiler and benchmark binaries.
- Five fresh-process trials per language and case; medians below. `WILLOW_WORKERS=8`, `GOMAXPROCS=8`.
- Build time excluded. CPU affinity/frequency not pinned; background services remained active.
- Scheduler timing measures marked phases; synchronous timing includes startup/shutdown. Scheduler runs Willow then Go; synchronous launch order alternates.
- All 100 runs completed successfully; all 50 synchronous outputs passed harness validation.

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

## Wall time

Ratio is Willow time / Go time; greater than 1 means Go is faster.

| Case | Work | Willow (ms) | Go (ms) | Ratio |
|---|---|---:|---:|---:|
| idle_spawn | 100,000 tasks | 144.147 | 119.952 | 1.20× |
| wake_fanout | 100,000 tasks | 210.036 | 13.670 | 15.37× |
| yield_switch | 100,000 tasks × 100 yields | 6720.312 | 1542.974 | 4.36× |
| ping_pong | 1,000,000 round trips | 3936.811 | 165.628 | 23.77× |
| gc_scheduler | 10,000 tasks × 100 rounds | 5229.646 | 141.206 | 37.04× |
| leibniz_pow | 100,000,001 terms | 5112.359 | 6546.172 | 0.78× |
| leibniz_reduced | 100,000,001 terms | 109.289 | 99.482 | 1.10× |
| fibonacci | fib(40) | 304.813 | 406.079 | 0.75× |
| array_sum | 5,000,000 values | 311.738 | 51.095 | 6.10× |
| linked_list | 1,000,000 nodes | 1310.592 | 25.463 | 51.47× |

## Memory

| Case / metric | Willow | Go |
|---|---:|---:|
| 100k parked tasks: incremental RSS (MiB) | 75.99 | 261.66 |
| 100k parked tasks: total RSS (MiB) | 79.02 | 264.09 |
| 100k parked tasks: incremental bytes/task | 796.79 | 2743.75 |
| leibniz_pow: peak RSS (MiB) | 2.40 | 1.87 |
| leibniz_reduced: peak RSS (MiB) | 2.28 | 1.87 |
| fibonacci: peak RSS (MiB) | 2.28 | 1.87 |
| array_sum: peak RSS (MiB) | 130.77 | 77.48 |
| linked_list: peak RSS (MiB) | 116.86 | 18.25 |

## Raw measurements

[Original 100 trial records](MEASUREMENT_2026-09-12.jsonl) include elapsed time, CPU metrics, memory where available, and GC telemetry. Scheduler CPU time has 10 ms resolution. GC telemetry differs between runtimes and is not directly equivalent.

Commands:

```sh
cargo build --release --bin willowc
python3 benches/go_vs_willow/run.py idle_spawn 100000 --trials 5
python3 benches/go_vs_willow/run.py wake_fanout 100000 --trials 5 --no-build
python3 benches/go_vs_willow/run.py yield_switch 100000 100 --trials 5 --no-build
python3 benches/go_vs_willow/run.py ping_pong 1000000 --trials 5 --no-build
python3 benches/go_vs_willow/run.py gc_scheduler 10000 100 --trials 5 --no-build
python3 benches/go_vs_willow/run_sync.py leibniz_pow leibniz_reduced fibonacci array_sum linked_list --trials 5
```
