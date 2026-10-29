# Willow

[![Status: experimental](https://img.shields.io/badge/status-experimental-orange)](https://github.com/lechatthecat/willow)
[![CI](https://github.com/lechatthecat/willow/actions/workflows/ci.yml/badge.svg)](https://github.com/lechatthecat/willow/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**A native programming language with compiler-enforced concurrency rules and semantic tooling for AI coding agents.**

Willow is a statically typed, garbage-collected language that compiles to native code using [Cranelift](https://cranelift.dev/). It includes its own async runtime, garbage collector, package manager, and incremental compiler infrastructure.

Its premise is simple: **when an AI agent changes a program, the compiler should do more than check syntax and types. It should reject certain unsafe behaviors and explain the program's semantics in a form the agent can use.**

**Status:** Experimental research/engineering project. **Not production-ready.** The guarantees below describe specific compiler-enforced properties, not a proof that arbitrary Willow programs are free of deadlocks, races, or other bugs.

## Start with a guarantee, not a token count

### E2604: no suspension or blocking wait while holding a Willow lock

Consider this Willow program:

```willow
fn receive(ch: Channel<i64>) -> i64 {
    return ch.recv();
}

async fn main() {
    let balance = Mutex::new(100);
    let ch = Channel<i64>::new();

    lock balance as mut value {
        value = value + receive(ch); // error[E2604]
    }
}
```

`receive` contains no `await`, and the critical section calls only an ordinary synchronous helper. But `receive` can wait on `Channel.recv()`. Willow follows the call and rejects it **at compile time** instead of allowing a task to wait while still holding the mutex.

The same rule rejects a direct `await`, `select`, waiting channel operation, or a call whose transitive effects include `MAY_BLOCK` or `MAY_SUSPEND`.

Move the waiting operation outside the critical section:

```willow
fn receive(ch: Channel<i64>) -> i64 {
    return ch.recv();
}

async fn main() {
    let balance = Mutex::new(100);
    let ch = Channel<i64>::with_capacity(1);
    ch.send(5);

    let delta = receive(ch);
    lock balance as mut value {
        value = value + delta; // accepted
    }
}
```

The compiler is enforcing a **semantic constraint**, not looking for the literal word `await`. That distinction matters when an AI agent edits a helper several calls away from the critical section.

The implementation and regression tests are in [`src/semantic/effects.rs`](src/semantic/effects.rs), [`src/compiler_db/effects.rs`](src/compiler_db/effects.rs), and [`tests/integration/concurrency/lock_restrictions.rs`](tests/integration/concurrency/lock_restrictions.rs). See the runnable [scheduler-aware lock example](example/scheduler_aware_lock.wi).

### The analysis follows the program, not just the local syntax

Willow computes effect summaries over the compiler's call graph using a monotone fixed point. Important cases include:

| Situation | What the compiler accounts for |
| --- | --- |
| Direct `await`, `select`, and channel operations | Local suspension/wait effects |
| Synchronous helper chains | Transitive `MAY_BLOCK` / `MAY_SUSPEND` |
| Virtual methods and interface dispatch | Reachable overrides, including a waiting override hidden behind a non-waiting base method |
| Function values and closures | Indirect calls, including calls reached through other helpers |
| Unknown or unavailable callees | Conservative bounds rather than silently assuming the call is safe |

Effects have **witnesses**: compiler evidence that can point to the operation responsible for a property. The propagation machinery is shared by multiple consumers, including E2604's lock check, recoverable-panic handling, and scheduler/preemption analysis.

This is deliberately conservative. In particular, the current lock check approximates indirect function-value targets by parameter count; it can reject a harmless indirect call if another compatible-arity function value may wait. A proof of *no waiting effect* requires sufficient information.

Calling an `async fn` creates a Task; it does not by itself suspend its caller. The analysis distinguishes task creation from actually awaiting it. This is another reason a plain text scan is not enough.

See [shared effect fixpoint](example/shared_effect_fixpoint.wi), [virtual dispatch example](example/shared_call_graph.wi), and [indirect calls example](example/lock_function_values.wi).

## Locks designed together with the compiler and scheduler

Willow does not expose an ordinary mutex guard whose destructor must release the lock. Instead, it gives the compiler ownership of a **lexical critical section**:

```willow
async fn deposit(balance: Mutex<i64>, amount: i64) {
    lock balance as mut value {
        value = value + amount;
    }
}
```

- **Task-aware acquisition:** a contended `Mutex<T>` or `RwLock<T>` acquisition parks the Willow task rather than blocking its scheduler worker.
- **Compiler-managed exit:** the lock is released across supported exits, including fallthrough, early `return`, `break`, `continue`, `?` propagation, and panic unwinding; cancellation also uses the compiler's cleanup path.
- **No wait inside:** E2604 rejects suspension/blocking waits within the critical section.
- **No nested acquisition in V1:** E2605 rejects nested lexical `lock` statements; Willow does not currently perform general lock-order analysis.
- **Read/write modes:** `lock read` and `lock write` provide lexical access to `RwLock<T>`.

These features belong together: the parser, effect checker, async lowering, cleanup logic, and runtime cooperate to enforce a restricted locking discipline. Willow uses a GC rather than a borrow checker or general destructor-based ownership system; the compiler-owned lock scope is part of its design.

**Scope of the claim:** E2604 applies to Willow's modeled scheduler-aware lock sections and known effects. It does **not** imply universal deadlock freedom, absence of all data races, or correct behavior from arbitrary native code. Blocking compatibility primitives and external/foreign calls have separate semantics and must not be mistaken for the lexical lock guarantee.

See [Mutex example](example/scheduler_aware_lock.wi), [RwLock example](example/scheduler_aware_rwlock.wi), and [concurrency regression tests](tests/integration/concurrency/).

## The same semantic knowledge is available to AI agents

Many languages already offer navigation, refactoring, or LSP/MCP integration. **That alone is not a reason to invent a new language.** Willow's more interesting combination is:

1. The compiler enforces program properties during normal checking.
2. The same semantic infrastructure exposes machine-readable explanations to an agent.
3. The agent can use those explanations to make a targeted edit and run the checks again.

The `willow` CLI offers:

| Command | Compiler-backed answer or action |
| --- | --- |
| `willow effects SELECTOR` | Effects such as suspension, blocking, allocation and locking, with evidence |
| `willow impact SELECTOR` | Definitions, implementations, and direct/indirect callers potentially affected |
| `willow refs SELECTOR` | Symbol-resolved references, not a text search for matching names |
| `willow type SELECTOR` / `willow symbol SELECTOR` | Checked types and declarations, including local symbols |
| `willow rename SELECTOR NEW_NAME` | Project-wide rename with collision checks; preview with `--dry-run --diff` |
| `willow overview` | Bounded project outline |

For example:

```sh
willow effects receive --source src/main.wi --explain --format json
willow impact account::deposit --format json
willow rename account::deposit credit --dry-run --diff
willow check src/main.wi --format ndjson
```

These illustrate the interface; use selectors and source paths from your own program. Semantic queries support JSON and bounded output, while machine-mode check/build provide structured diagnostics. `willow init` generates `AGENTS.md` and `CLAUDE.md` so coding agents can discover the tooling; `willow agent sync` can update Willow-managed instructions.

This is not the claim that an agent is always correct. Rather, **some mistakes can be turned into deterministic compiler failures with actionable evidence** instead of being left to prompts, tests, or code review alone.

See the [AI toolchain protocol](protocol/ai_toolchain/README.md) and [example effect explanations](example/effect_explanations.wi).

## How this relates to other approaches

- [gopls MCP](https://go.dev/gopls/features/mcp) exposes analysis of Go programs to assistants. Willow also exposes compiler semantics, but its central argument is **language-level enforcement of particular concurrency rules**, not merely a new interface to an existing analyzer.
- [Rust Clippy's `await_holding_lock` lint](https://rust-lang.github.io/rust-clippy/master/index.html#await_holding_lock) warns about holding certain non-async-aware guards across `await`. Willow instead rejects waiting effects inside its own lexical lock construct, including transitive calls. The two systems have different scopes; this is not a claim that Willow is generally safer than Rust.
- [Koka](https://github.com/koka-lang/koka) illustrates that effect systems are a well-established research direction. Willow is not claiming to have invented effect tracking. Its emphasis is **connecting concrete effect checks, the native runtime, and agent-readable evidence** in one toolchain.

## Measured AI-agent efficiency (a secondary benefit)

The semantic CLI also reduced work in one small controlled benchmark. Five tasks were run three times per mode on an approximately 1,300-line, eight-module Willow chess engine, with Claude Code (Sonnet 5.5), on 2026-10-22.

| Total across 15 runs per mode | Semantic CLI | Text tools only | Change |
| --- | ---: | ---: | ---: |
| Input tokens | 252,229 | 458,222 | -45% |
| Output tokens | 4,580 | 12,315 | -63% |
| Cost (USD) | $0.225 | $0.508 | -56% |
| Tool calls | 15 | 45 | -67% |

All answers were correct in both modes. One task (interface-signature impact) used *more* input tokens with the semantic CLI. This is evidence for the tested tasks only, **not** a demonstrated general improvement on large repositories. Token savings are useful, but they are not Willow's primary safety argument.

## Runtime performance: strengths and gaps

A separate, reproducible microbenchmark compared Willow, Go 1.27.0 and OpenJDK 26.0.2 (virtual threads) on an AMD Ryzen 7 7800X3D, with eight workers. The table shows the median of five trials on 2026-10-25 (commit `453fc6c`); **lower is better**.

| Case | Willow | Go | Java |
| --- | ---: | ---: | ---: |
| idle_spawn (100k tasks) | **55.7 ms** | 108.1 ms | 94.9 ms |
| yield_switch | **527.6 ms** | 1,405.3 ms | 541.9 ms |
| gc_scheduler | **84.9 ms** | 134.7 ms | 141.0 ms |
| ping_pong | 696.0 ms | **163.9 ms** | 782.3 ms |
| channel_select_fan_in | 955.2 ms | **237.1 ms** | 530.7 ms |
| virtual_dispatch | 69.4 ms | **41.4 ms** | 58.1 ms |

In the idle-spawn measurement, memory after parking 100,000 tasks was about **744 B/task** for Willow, compared with **2,743 B/task** for Go and **1,351 B/task** for Java. The [full report, methods, raw data, and reproducible harness](benches/go_vs_willow/MEASUREMENT_2026-10-25.md) include additional cases and limitations. These are workload-specific measurements, not a blanket claim of runtime superiority.

## Try Willow

Download a [prebuilt release](https://github.com/lechatthecat/willow/releases/latest) for Linux x86_64, macOS arm64/x86_64, or Windows x86_64. Keep the distributed `bin/` and `lib/` directories together. The packaged compiler needs a system C toolchain for linking, but does not require Rust. The release includes checksums.

```sh
willow --version
willow init hello
cd hello
willow run
```

The generated project includes `project.toml`, `src/main.wi`, `AGENTS.md`, and `CLAUDE.md`.

Or build from source (Rust 1.96+):

```sh
git clone https://github.com/lechatthecat/willow.git
cd willow
cargo build --release
./target/release/willow --version
```

Examples live in [`example/`](example/); [`RELEASE_README.md`](RELEASE_README.md) covers installation, library layout, checksums and toolchain requirements.

## Current boundaries

Willow is an experimental language and compiler, **not yet suitable for production deployment**. Its effect guarantees are scoped to the supported language operations and available compiler/runtime knowledge. In particular:

- E2604 prevents modeled blocking/suspending behavior in Willow lexical lock bodies; it does not prove arbitrary programs deadlock-free.
- Conservative dynamic/indirect-call analysis may reject code that is actually safe. Unavailable evidence is not treated as proof of purity.
- Nested lexical locks are rejected in V1 (E2605), rather than checked with a general lock-order proof.
- Native/foreign boundaries, compiler defects, and runtime defects are outside the guarantee of a static effect check.
- The runtime benchmarks and agent-token experiment measure limited, published workloads.

**The goal:** make semantic safety properties part of the compiler's ordinary contract, and make their evidence accessible to both humans and coding agents.

MIT licensed. See [LICENSE](LICENSE).
