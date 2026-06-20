# Compiler benchmark harness

Run all compiler benchmarks with:

```shell
cargo bench --bench compiler_bench
```

Set `WILLOW_BENCH_ITERATIONS` to control repetitions (default: 3). The harness
prints CSV columns for compilation time, executable size, and execution time:

```text
case,iteration,compile_ms,artifact_bytes,run_ms
```

The cases cover recursive Fibonacci, a 1,000-function source file, a 25-module
project, an allocation/collection-heavy GC pause workload, and 32 cooperative
async tasks. Redirect stdout to retain a baseline for comparison; compiler and
linker progress is written to stderr.
