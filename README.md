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

Runtime microbenchmarks measured on 2026-10-04 on an AMD Ryzen 7 7800X3D host.
Values are wall-time medians in milliseconds across 5 trials; lower is better.
Willow used 8 workers. Comparison runtimes were Go 1.27.0 and OpenJDK 26.0.2 using virtual threads.

| Case | Willow | Go | Java |
| --- | ---: | ---: | ---: |
| idle_spawn 100k | **106.0** | 119.4 | 91.7 |
| wake_fanout | 163.4 | **14.2** | 190.6 |
| yield_switch | **1297.8** | 1481.1 | 575.7 |
| ping_pong | 1710.5 | **167.2** | 783.6 |
| gc_scheduler | 1234.6 | **139.7** | 151.8 |
| channel_select_fan_in | 2579.4 | **222.5** | 536.9 |
| spawn_join_tree | 262.3 | **16.8** | 112.5 |
| fibonacci | 233.1 | 390.1 | **229.5** |
| linked_list | 545.3 | **26.1** | 45.8 |
| object_churn | 285.2 | **56.1** | 70.3 |
| map_lookup_insert | 168.1 | **86.2** | 201.6 |
| virtual_dispatch | 57.2 | **34.8** | 58.5 |

The Java `channel_select_fan_in` case uses one shared queue. These are machine-local
microbenchmark results, not general language rankings; workload and runtime behavior
vary substantially by case.

---

## Examples
https://github.com/lechatthecat/willow/tree/main/example

# License

[MIT License](LICENSE)
