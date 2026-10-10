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
Values are wall-time medians in milliseconds across 5 trials; lower is better.
Willow used 8 workers. Comparison runtimes were Go 1.27.0 and OpenJDK 26.0.2 using virtual threads.

| Case | Willow (ms) | Go (ms) | Java (ms) |
| --- | ---: | ---: | ---: |
| idle_spawn 100k | **77.8** | 113.4 | 88.8 |
| wake_fanout | 145.6 | **13.1** | 184.8 |
| yield_switch | 968.0 | 1507.2 | **567.0** |
| ping_pong | 1585.3 | **166.9** | 838.7 |
| gc_scheduler | 672.3 | **137.2** | 147.3 |
| channel_select_fan_in | 2334.6 | **241.1** | 544.3 |
| spawn_join_tree | 169.7 | **17.4** | 104.0 |
| fibonacci | 242.3 | 390.8 | **228.2** |
| linked_list | 339.9 | **29.7** | 46.3 |
| object_churn | 170.9 | **55.5** | 71.7 |
| map_lookup_insert | 132.4 | **88.0** | 208.1 |
| virtual_dispatch | 55.6 | **34.7** | 58.3 |

Scheduler cases time marked phases; synchronous cases shown here time the whole process, including JVM startup and warm-up.
The Java `channel_select_fan_in` case uses one shared queue. CPU affinity/frequency were not pinned and background services were active.
These are machine-local microbenchmark results, not general language rankings; workload and runtime behavior vary substantially by case.

---

## Examples
https://github.com/lechatthecat/willow/tree/main/example

# License

[MIT License](LICENSE)
