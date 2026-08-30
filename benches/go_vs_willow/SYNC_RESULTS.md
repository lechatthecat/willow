# Synchronous results: Willow vs Go 1.27.0

Measured on 2026-08-30 from Willow base commit `55ba426` with the synchronous
benchmark sources in this directory.

- CPU: AMD Ryzen 7 7800X3D, 8 physical cores / 16 threads
- OS: Linux 7.0.0-30-generic, amd64
- Go: go1.27.0 linux/amd64 (`/usr/local/go/bin/go`)
- Builds: Willow `--release` (Cranelift `opt_level=speed`); standard optimized
  `go build -trimpath`
- Environment: `WILLOW_WORKERS=8`, `GOMAXPROCS=8`
- Statistic: median of five fresh-process trials, with language launch order
  alternated each trial
- Timing: Python monotonic wall clock around GNU `time`; GNU `time` process user
  and system time; GNU `time` maximum resident set size

CPU affinity and frequency were not pinned, and background services were not
disabled. Build time is excluded. Wall time includes runtime process startup and
shutdown. These are local microbenchmarks, not universal language rankings.

## Results

The time cells are `wall / user / system`; RSS is the median peak resident set.
Every trial produced the same checked answer in both languages.

| Benchmark | Fixed work | Willow time | Go time | Relative wall time | Willow RSS | Go RSS |
|---|---:|---:|---:|---:|---:|---:|
| Leibniz with `pow` | 100,000,001 terms | 5.132 / 5.12 / 0.00 s | 6.726 / 6.72 / 0.00 s | Willow 1.31x faster | 2.75 MiB | 1.84 MiB |
| Leibniz, sign-reduced | 100,000,001 terms | 100.233 / 90 / 0 ms | 100.169 / 90 / 0 ms | effectively tied (0.06%) | 2.71 MiB | 1.96 MiB |
| Recursive Fibonacci | `fib(40)` | 549.407 / 540 / 0 ms | 384.019 / 380 / 0 ms | Go 1.43x faster | 2.76 MiB | 1.84 MiB |
| Dynamic array build + sum | 5,000,000 `i64` values | 263.131 / 220 / 40 ms | 49.151 / 40 / 30 ms | Go 5.35x faster | 131.09 MiB | 77.64 MiB |
| Linked-list build + sum | 1,000,000 nodes | 955.917 / 900 / 50 ms | 30.485 / 30 / 0 ms | Go 31.36x faster | 109.08 MiB | 18.59 MiB |

## Interpretation

The [Qiita-style](https://qiita.com/hanaata/items/c91788bcac2a40f1bb05)
`pow(-1, n)` case measures each language's exponentiation implementation as much
as it measures the loop. Willow's generated floating power helper wins this
specific workload. Once the alternating power is reduced to a sign flip, the
two generated loops are effectively tied on this host.

Go is faster in the recursive-call case, and substantially faster in the two
collection/allocation cases. The array case includes dynamic capacity growth in
both implementations. The linked-list case intentionally keeps every node live
through construction and then traverses it, so its result combines allocation,
GC bookkeeping, object representation, and field-access costs; it is not a pure
collector-pause benchmark.

The synchronous Willow binaries still initialize the Willow runtime and its
configured workers. That startup is included to follow the whole-program method
used by the referenced article. It has a larger proportional effect on the
short Go runs, especially the linked-list case.

## Reproduce

```sh
python3 benches/go_vs_willow/run_sync.py
```

To rerun selected already-built cases:

```sh
python3 benches/go_vs_willow/run_sync.py fibonacci array_sum --trials 5 --no-build
```
