# Willow
![Status](https://img.shields.io/badge/status-experimental-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

This project is not production ready.

A statically typed, garbage-collected native programming language built for AI coding agents.

Willow compiles to native code through [Cranelift](https://cranelift.dev/), with its own runtime, package manager and incremental compiler infrastructure. Its compiler answers agents' questions about a program directly: references, types, effects, impact and safe renames. Agents do not have to reconstruct these from text search.

## Why Willow: fewer tokens for AI agents

The `willow` CLI exposes compiler-resolved program structure as commands:

| Command | Answers |
| --- | --- |
| `willow refs SELECTOR` | every compiler-resolved reference to a symbol, even when other types have fields with the same name |
| `willow rename SELECTOR NEW_NAME` | a project-wide rename with collision checks (`--dry-run --diff` previews it) |
| `willow impact SELECTOR` | definitions and implementations plus direct and indirect callers affected by a signature change |
| `willow effects SELECTOR` | whether a function may suspend, take a lock or allocate, with evidence locations |
| `willow type SELECTOR` / `willow symbol SELECTOR` | inferred types and declarations, including locals (`module::function::local`) |
| `willow overview` | a bounded outline of the project |

Each command accepts `--format json`. The query commands also accept `--max-chars N` to bound their output; `rename` accepts it only with `--dry-run`. `willow init` writes `AGENTS.md` and `CLAUDE.md` describing them to agents.

### Measured token savings

We ran the same five coding-agent tasks twice. In "With semantic CLI" the agent could use the commands above. In "Text tools only" the semantic commands were blocked, and the agent had grep, file reads and edits.

The tasks were run on an 8-module, ~1.3k-line Willow chess engine with interfaces, payload enums, async tasks, channels, Mutex, and same-named fields across classes. Each task was run 3 times per mode, with Claude Code (`claude -p`, model `claude-sonnet-5-5`) on Willow 0.1.5 (release tag `v0.1.5`, commit `52555ac`), on 2026-10-22.

Values are per-task medians. Input tokens include cache reads and writes.

| Task | Input tokens: semantic CLI | Input tokens: text tools | Cost (USD): semantic CLI | Cost (USD): text tools | Tool calls: semantic CLI | Tool calls: text tools |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Five renames (field, enum variant, interface method, local, field) | **16,578** | 39,316 | **0.012** | 0.047 | **1** | 4 |
| Functions to edit when an interface method gains a parameter | 16,292 | **14,436** | **0.014** | 0.016 | 1 | 1 |
| All references to one of three same-named fields | **16,313** | 23,641 | **0.013** | 0.028 | **1** | 3 |
| Five suspension / lock / allocation questions | **18,905** | 42,901 | **0.024** | 0.053 | **1** | 4 |
| Inferred types of two locals | **15,995** | 22,137 | **0.010** | 0.021 | **1** | 3 |

All 30 runs, totalled by mode:

| Total over 15 runs | Semantic CLI | Text tools only | Change |
| --- | ---: | ---: | ---: |
| Input tokens | 252,229 | 458,222 | −45% |
| Output tokens | 4,580 | 12,315 | −63% |
| Cost (USD) | 0.225 | 0.508 | −56% |
| Tool calls | 15 | 45 | −67% |

Every answer was correct in both modes. Every rename tree matched the expected result exactly. Three rename runs (one semantic-CLI, two text-only) did fail the program-output check. The Willow-built program intermittently exited with SIGSEGV, independent of the edits. This open Willow runtime bug is tracked as `willow-p0gb`.

The semantic-CLI system prompt is about 3.6 KB longer, and its cost is included above. With so small a project, text tools already do well whenever a name is unique: in the impact task, plain grep needed fewer input tokens. The advantage should grow with project size and name collisions, but this run does not measure that.

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

## Performance snapshot

These runtime microbenchmarks were measured on 2026-10-21 on an AMD Ryzen 7 7800X3D host, using the code released as Willow 0.1.5. That run was taken at commit `68937d8` plus the uncommitted array-literal fast path. That change was later committed as `52555ac`, the commit that the `v0.1.5` release tag points to.
Values are wall-time medians in milliseconds across 5 trials; lower is better. The fastest result in each row is bold.
Willow used 8 workers. Comparison runtimes were Go 1.27.0 and OpenJDK 26.0.2 using virtual threads.

| Case | Willow (ms) | Go (ms) | Java (ms) |
| --- | ---: | ---: | ---: |
| idle_spawn | **76.847** | 114.706 | 86.052 |
| wake_fanout | 72.090 | **13.334** | 203.489 |
| yield_switch | 1,127.807 | 1,472.703 | **553.509** |
| ping_pong | 729.448 | **168.098** | 803.912 |
| gc_scheduler | 141.597 | **136.290** | 143.513 |
| channel_select_fan_in | 1,025.584 | **233.291** | 540.071 |
| spawn_join_tree | 211.162 | **16.508** | 96.946 |
| general_pow | 576.120 | 596.288 | **198.036** |
| fibonacci | 234.369 | 388.397 | **229.820** |
| array_sum | 53.486 | **51.556** | 83.574 |
| linked_list | 32.183 | **28.383** | 44.931 |
| object_churn | 66.643 | **54.926** | 69.774 |
| map_lookup_insert | 88.194 | **84.500** | 199.472 |
| virtual_dispatch | 69.848 | **40.614** | 58.301 |
| closure_call | 58.177 | 26.167 | **16.521** |
| string_build | **64.596** | 79.601 | 83.775 |

For `idle_spawn` with 100,000 parked tasks, RSS after parking was 73.69 MiB for Willow (734 B/task), 263.56 MiB for Go (2,737 B/task), and 172.77 MiB for Java (1,349 B/task).

---

## Examples
https://github.com/lechatthecat/willow/tree/main/example

---

### Download a prebuilt release

Prebuilt archives for Linux (x86_64), macOS (arm64, x86_64) and Windows (x86_64) are on the [latest release](https://github.com/lechatthecat/willow/releases/latest) page.

```bash
tar xzf willow-v0.1.6-linux-x86_64.tar.gz
./willow-v0.1.6-linux-x86_64/bin/willow
```

Run `willow --version` (also `-V` or `version`) to report the package version and build commit. See the [bundled installation guide](RELEASE_README.md) for shell and Windows PATH setup.

For convenience, add the archive's `bin` directory to your `PATH`. Keep `bin/` and `lib/` together: `willow` finds its runtime library in `../lib`. No Rust toolchain is needed, but Willow links programs with the system C toolchain: `cc` on Linux and macOS, or the MSVC Build Tools on Windows.

Release archives use the `packaged` Cargo feature: a missing bundled runtime reports its searched location and never falls back to the build machine's checkout or invokes Cargo. Restore the archive's `lib/`, or select a compatible runtime with `--runtime-lib` or `WILLOW_RUNTIME_LIB`. Source builds without this feature retain automatic runtime builds with Cargo.

Each release also includes a `SHA256SUMS` file containing the SHA-256 digest of every platform archive. Download it alongside your archive and verify the checksum before extracting.

On Linux, when only the downloaded platform archive is in the directory:

```bash
sha256sum --check --ignore-missing SHA256SUMS
```

On macOS, for example:

```bash
grep 'willow-v0.1.6-macos-arm64.tar.gz' SHA256SUMS | shasum -a 256 --check
```

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

# License

[MIT License](LICENSE)

