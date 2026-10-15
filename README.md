# Willow
![Status](https://img.shields.io/badge/status-experimental-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

This project is not production ready.

A statically typed, garbage-collected native programming language with its own runtime, package manager, incremental compiler infrastructure, and AI-oriented semantic tooling.

Willow is an experimental programming language that compiles to native code through [Cranelift](https://cranelift.dev/).

## How to start

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

Runtime microbenchmarks measured on 2026-10-14 on an AMD Ryzen 7 7800X3D host (Willow commit `f918069`, clean working tree).
Values are wall-time medians in milliseconds across 5 trials; lower is better. The fastest result in each row is bold.
Willow used 8 workers. Comparison runtimes were Go 1.27.0 and OpenJDK 26.0.2 using virtual threads.

| Case | Willow (ms) | Go (ms) | Java (ms) |
| --- | ---: | ---: | ---: |
| idle_spawn | **66.816** | 116.184 | 92.215 |
| wake_fanout | 110.744 | **12.841** | 186.666 |
| yield_switch | 631.070 | 1,468.180 | **540.615** |
| ping_pong | 707.318 | **163.457** | 800.639 |
| gc_scheduler | 167.225 | **135.641** | 137.663 |
| channel_select_fan_in | 987.712 | **235.387** | 533.961 |
| spawn_join_tree | 146.586 | **17.212** | 99.525 |
| general_pow | 610.789 | 623.083 | **206.046** |
| fibonacci | 804.205 | 388.658 | **230.731** |
| array_sum | 65.634 | **50.003** | 86.632 |
| linked_list | 402.169 | **25.426** | 46.849 |
| object_churn | 194.742 | **56.194** | 71.798 |
| map_lookup_insert | 162.207 | **89.785** | 197.123 |
| virtual_dispatch | 180.149 | **41.047** | 58.262 |
| closure_call | 180.630 | 25.920 | **16.615** |
| string_build | 115.624 | **80.296** | 85.934 |

For `idle_spawn` with 100,000 parked tasks, RSS after parking was 76.57 MiB for Willow (765 B/task), 264.07 MiB for Go (2,741 B/task), and 172.04 MiB for Java (1,343 B/task).

Scheduler cases time marked phases. Other synchronous cases time the whole process, including JVM startup and JIT warm-up.
The Java `channel_select_fan_in` case uses one shared queue. CPU affinity/frequency were not pinned and background services were active.
The current Willow results for `fibonacci` and `virtual_dispatch` include confirmed performance regressions tracked in `willow-gq96`; they are shown here rather than excluded.
These are machine-local microbenchmark results, not general language rankings; workload and runtime behavior vary substantially by case.

---

## Examples
https://github.com/lechatthecat/willow/tree/main/example

---

### Download a prebuilt release

Prebuilt archives for Linux (x86_64), macOS (arm64, x86_64) and Windows (x86_64) are on the [latest release](https://github.com/lechatthecat/willow/releases/latest) page.

```bash
tar xzf willow-v0.0.1-linux-x86_64.tar.gz
./willow-v0.0.1-linux-x86_64/bin/willow
```

For convenience, add the archive's `bin` directory to your `PATH`. Keep `bin/` and `lib/` together: `willow` finds its runtime library in `../lib`. No Rust toolchain is needed, but Willow links programs with the system C toolchain: `cc` on Linux and macOS, or the MSVC Build Tools on Windows.

Release archives use the `packaged` Cargo feature: a missing bundled runtime reports its searched location and never falls back to the build machine's checkout or invokes Cargo. Restore the archive's `lib/`, or select a compatible runtime with `--runtime-lib` or `WILLOW_RUNTIME_LIB`. Source builds without this feature retain automatic runtime builds with Cargo.

Each release also includes a `SHA256SUMS` file containing the SHA-256 digest of every platform archive. Download it alongside your archive and verify the checksum before extracting.

On Linux, when only the downloaded platform archive is in the directory:

```bash
sha256sum --check --ignore-missing SHA256SUMS
```

On macOS, for example:

```bash
grep 'willow-v0.0.1-macos-arm64.tar.gz

### Build from source

A Rust toolchain is required to build Willow from source.

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


# License

[MIT License](LICENSE)
 SHA256SUMS | shasum -a 256 --check
```
