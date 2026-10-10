# Willow
![Status](https://img.shields.io/badge/status-experimental-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

This project is not production ready.

A statically typed, garbage-collected native programming language with its own runtime, package manager, incremental compiler infrastructure, and AI-oriented semantic tooling.

Willow is an experimental programming language that compiles to native code through [Cranelift](https://cranelift.dev/).

## How to start

A Rust toolchain is currently required to build Willow from source.

```bash
git clone https://github.com/lechatthecat/willow.git
cd willow
cargo build --release
```

The user-facing tool is:

```bash
./target/release/willow
```

For convenience, add `target/release` to your `PATH`.

### Create a project

```bash
willow init hello
cd hello
willow run
```

`willow init` creates:

```text
hello/
├── project.toml
├── .gitignore
├── AGENTS.md
├── CLAUDE.md
└── src/
    └── main.wi
```

Both agent files contain the current `willow agent instructions` guidance for
semantic queries, safe renaming, and check/build validation. Existing agent files
are left untouched. Use `willow agent sync` to review updates to Willow-managed
sections later.

Willow includes AI-oriented semantic tooling for references, types, impact analysis, and safe structured refactoring.
AI agents can query compiler-resolved program structure directly instead of reconstructing it from text search alone.

## Performance snapshot

Runtime microbenchmarks measured on 2026-10-10 on an AMD Ryzen 7 7800X3D host (Willow commit `578b16f`, clean working tree).
Values are wall-time medians in milliseconds across 5 trials; lower is better. The fastest result in each row is bold.
Willow used 8 workers. Comparison runtimes were Go 1.27.0 and OpenJDK 26.0.2 using virtual threads.

| Case | Willow (ms) | Go (ms) | Java (ms) |
| --- | ---: | ---: | ---: |
| idle_spawn | **77.786** | 113.369 | 88.782 |
| wake_fanout | 145.554 | **13.122** | 184.807 |
| yield_switch | 968.008 | 1507.208 | **567.020** |
| ping_pong | 1585.254 | **166.887** | 838.658 |
| gc_scheduler | 672.301 | **137.182** | 147.349 |
| channel_select_fan_in | 2334.562 | **241.098** | 544.313 |
| spawn_join_tree | 169.744 | **17.401** | 104.035 |
| general_pow | 719.857 | 628.464 | **206.669** |
| leibniz_reduced | **111.384** | 114.248 | 271.756 |
| fibonacci | 242.333 | 390.820 | **228.183** |
| array_sum | 66.493 | **52.286** | 87.602 |
| linked_list | 339.943 | **29.670** | 46.332 |
| array_build_only | 54.534 | **42.808** | 56.720 |
| array_read_sum_only | 3.146 | **1.346** | 4.731 |
| array_write_only | 4.807 | **3.161** | 7.097 |
| array_grow_only | 28.196 | **12.385** | 29.737 |
| object_churn | 170.887 | **55.450** | 71.699 |
| map_lookup_insert | 132.431 | **88.026** | 208.081 |
| virtual_dispatch | 55.577 | **34.684** | 58.267 |

The `leibniz_pow` case is intentionally excluded from this comparison.
Scheduler cases time marked phases; `*_only` synchronous cases also time marked phases. Other synchronous cases time the whole process, including JVM startup and JIT warm-up.
The Java `channel_select_fan_in` case uses one shared queue. CPU affinity/frequency were not pinned and background services were active.
The nominal `leibniz_reduced` lead over Go is within observed session-to-session variation and should not be treated as a confirmed speed advantage.
These are machine-local microbenchmark results, not general language rankings; workload and runtime behavior vary substantially by case.

---

## Examples
https://github.com/lechatthecat/willow/tree/main/example

# License

[MIT License](LICENSE)
