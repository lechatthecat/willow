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

This deliberately invalid program demonstrates the compile-time diagnostic:

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

### Exact rename: change a symbol, preserve unrelated names

Save this standalone program as `main.wi`:

```willow
fn deposit(amount: i64) -> i64 { return amount + 1; }
fn unrelated() -> i64 {
    let deposit = 7;
    return deposit;
}
fn main() {
    println(deposit(4)); // 5
    println(unrelated()); // 7
}
```

Preview the symbol-resolved edit, apply it, then run the program:

```sh
willow rename main::deposit credit --source main.wi --dry-run --diff
willow rename main::deposit credit --source main.wi
willow run main.wi
```

The declaration and call become `credit`; the unrelated local binding named
`deposit` stays unchanged. Exact rename uses resolved symbol identity rather
than spelling, checks collisions, and validates the candidate before applying
source changes. The guarantee is scoped to supported symbols in the loaded
project: it does not update text in strings, external consumers, or undiscovered
source trees. Ambiguous selectors, detected collisions, and candidates that fail validation
are rejected. See the [rename regression tests](tests/semantic_rename.rs).

### What an agent can trust

An agent can use these compiler results without independently reconstructing
the same analysis. It still needs tests and review for behavior outside their
stated scope.

| Result | Compiler-backed information | Unknowns or assumptions |
| --- | --- | --- |
| Accepted lexical lock body | No blocking/suspending effect under the compiler's modeled lock rules | Not a proof of global deadlock freedom or arbitrary native-code behavior |
| Successful exact rename | Resolved occurrences of the selected symbol are edited and the candidate passes validation | Limited to the loaded project and supported symbols; not a proof of application correctness |
| Effect `MAY` (`may` in JSON) | A source or known-callee witness supports the capability | It may not occur on every runtime path; uncertainty can coexist in `unknown_reasons` |
| Effect `UNKNOWN` (`unknown` in JSON) | A conservative capability is retained without a supporting witness | Indirect/unresolved calls, missing bodies, incomplete flow, or external interop require assumptions; this is not evidence that the effect occurs or is absent |
| Effect `NONE` (`none` in JSON) | Neither a witness nor an uncertainty reason remains for that capability in the modeled analysis | Scoped to available compiler knowledge, not a guarantee about unmodeled behavior |

The `willow.effects.v2` output keeps `witness` and `unknown_reasons` separate.
Missing evidence is not purity. Safety checks retain conservative capability
bounds even when the explanatory query reports `UNKNOWN`; a query does not
waive E2604. See the [effect-state example](example/effect_states.wi).

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

### Effect contracts: declare the property on any function

E2604 applies the waiting analysis to lock bodies. `@effects(...)` applies the
same solved effects to a declaration, so a function can promise the property to
its callers whether or not it runs under a lock:

| Contract | Forbids | Still allowed |
| --- | --- | --- |
| `no_block` | Blocking waits such as stdout writes or `BlockingCell` reads | `await` and other suspension |
| `no_suspend` | Suspension such as `await`, `select` and waiting channel operations | Blocking waits |
| `no_wait` | Both | Creating a Task without awaiting it |

```willow
fn helper() { println(1); }

@effects(no_wait)
fn guard() { helper(); }
```

```text
error[E2610]: effect contract `no_wait` violated by `guard`
1 | fn helper() { println(1); }
  |               ^^^^^^^ root operation `$output::println`
2 | @effects(no_wait)
  | ---------------- effect contract declared here
3 | fn guard() { helper(); }
  |              ------ `guard` calls `helper`
note: certainty: compiler-proven; call chain: guard -> helper -> $output::println
```

Contracts are checked fail-closed:

- **E2610**: a proven witness violates the contract, transitively through helpers, dispatch and imported modules.
- **E2611**: an override weakens an inherited contract. Overrides that declare nothing inherit the base contract and are checked against it.
- **E2612**: the property cannot be proven, for example when the function calls an arbitrary function-value parameter. Missing evidence is not treated as success.

See [effect contracts](example/effect_contracts.wi) and [contracts across interface dispatch](example/effect_contract_dispatch.wi).

### Purity and value contracts

Effect contracts describe waiting. Value-level properties use separate annotations:

- **`@pure`** requires a function to have no observable interaction outside its
  arguments and result. Output, field and index writes, heap reads,
  nondeterminism and unknown callees are rejected with E2620. Undeclared callees
  are analyzed, so they do not need their own `@pure`.
  See [pure functions](example/pure_functions.wi).
- **`requires` / `ensures`** clauses are checked at run time in both debug and
  release builds. A violation is a recoverable panic such as
  `contract violation: ensures`. See [runtime contracts](example/runtime_contracts.wi).
- **`@verified`** (which requires `@pure`) proves i64/bool contracts statically with Z3. Failures
  are reported as E2630 (counterexample), E2631 (unsupported construct) and
  E2632 (solver unknown or failure). An unproved contract fails the build. Setup
  is under [Try Willow](#try-willow).

## Locks designed together with the compiler and scheduler

Willow does not expose an ordinary mutex guard whose destructor must release the lock. Instead, it gives the compiler ownership of a **lexical critical section**:

```willow
async fn deposit(balance: Mutex<i64>, amount: i64) {
    lock balance as mut value {
        value = value + amount;
    }
}
async fn main() {
    let balance = Mutex::new(100);
    await deposit(balance, 5);
    lock balance as value { println(value); } // 105
}
```

- **Task-aware acquisition:** a contended `Mutex<T>` or `RwLock<T>` acquisition parks the Willow task rather than blocking its scheduler worker.
- **Compiler-managed exit:** the lock is released across supported exits, including fallthrough, early `return`, `break`, `continue`, `?` propagation, and panic unwinding; cancellation also uses the compiler's cleanup path.
- **No wait inside:** E2604 rejects suspension/blocking waits within the critical section.
- **Checked nesting:** E2605 rejects cyclic acquisition order, same-lock reacquisition (including read-read), and nesting whose identities cannot be proved safe. Distinct allocation classes with consistent ordering may nest.
- **Read/write modes:** `lock read` and `lock write` provide lexical access to `RwLock<T>`.

These features belong together: the parser, effect checker, async lowering, cleanup logic, and runtime cooperate to enforce a restricted locking discipline. Willow uses a GC rather than a borrow checker or general destructor-based ownership system; the compiler-owned lock scope is part of its design.

**Scope of the claim:** E2604 applies to Willow's modeled scheduler-aware lock sections and known effects. It does **not** imply universal deadlock freedom, absence of all data races, or correct behavior from arbitrary native code. Blocking compatibility primitives and external/foreign calls have separate semantics and must not be mistaken for the lexical lock guarantee.

See [Mutex example](example/scheduler_aware_lock.wi), [RwLock example](example/scheduler_aware_rwlock.wi), and [concurrency regression tests](tests/integration/concurrency/).

### Acquiring multiple locks

Use one statement when lock handles come from parameters, fields, or collections
and their relative order cannot be proved statically:

```willow
async fn transfer(from: Mutex<i64>, to: Mutex<i64>, amount: i64) {
    lock from as mut debit, to as mut credit {
        debit = debit - amount;
        credit = credit + amount;
    }
}
```

Targets are evaluated once, left to right, before any binding is introduced.
The runtime acquires them in internal identity order while keeping bindings in
source order. Duplicate handles are rejected, including two read acquisitions
of the same RwLock. Each member may specify its own mode, for example
`lock account as mut value, read rates as rate { ... }`.

A contended group releases its acquired prefix before parking and retries after
waking. Scope exits, panic and cancellation release acquired locks in reverse
order; writes already made are committed, not rolled back. This provides mutual
exclusion, not transactional rollback.

Ordinary nested locks require provably distinct identities and an acyclic
acquisition order. E2605 reports the acquisition sites involved in a cycle;
unknown aliases remain conservative. Awaiting an async helper inside a lock is
still E2604: compute awaited values before entering the critical section, or
acquire the required locks together. Merely creating an async task does not
execute its body on the caller's stack.

See [ordered locks](example/ordered_locks.wi) for a runnable example.

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

For scalar String builtins, `effects` separates normal-path and panic-path allocation evidence. For example, `String.byte_at` shows normal `may-allocate: none` and panic `may-allocate: may`; the aggregate bound still includes panic allocation, even for an in-range loop. JSON exposes these per-operation details in `effect_evidence[].paths`. Argument evaluation and code after `recover()` are outside that per-operation description. This display does not change compiler/GC effect bounds or `@effects` contracts.

These illustrate the interface; use selectors and source paths from your own program. Semantic queries support JSON and bounded output, while machine-mode check/build provide structured diagnostics. `willow init` generates `AGENTS.md` and `CLAUDE.md` so coding agents can discover the tooling; `willow agent sync` can update Willow-managed instructions.

This is not the claim that an agent is always correct. Rather, **some mistakes can be turned into deterministic compiler failures with actionable evidence** instead of being left to prompts, tests, or code review alone.

See the [AI toolchain protocol](protocol/ai_toolchain/README.md) and [example effect explanations](example/effect_explanations.wi).

### Compare scalar contracts across an edit

`willow verify-edit --base BASE --schema v2 --protected-base FULL_COMMIT --format json`
adds purity, `requires`/`ensures`, and proof-dependency comparisons. The default remains
v1. `contract_preserved` does not imply `implementation_verified` or behavioral
equivalence; behavioral equivalence is unsupported. Unsupported inputs and missing
proof evidence return exit 3; contract or approval-policy violations return exit 1.
The current comparison supports standalone top-level i64/bool contracts; unsupported
imports and contract calls produce incomplete evidence.

For failed edits, inspect `summary.verification.target` for proof obligations,
`counterexample_candidate`, `input_symbols` and `unknown_reasons`.
`summary.verification.source_path` scopes report locations and declaration IDs. Correct the implementation and rerun
`check`, `verify-edit` and focused tests; see [the repair example](example/contract_repair.wi).
A successful proof does not approve a changed contract: obtain independent review.

`--write-contracts-lock` writes normalized contract and semantic digests to
`contracts.lock`. Editing that file never grants approval. A trusted CI wrapper must
supply the protected commit and, when approval is needed, `--approval-evidence FILE
--approval-verifier EXTERNAL_EXECUTABLE`. Keep that verifier and its configuration
outside the editable checkout and enforce reviewers with CODEOWNERS/branch protection.
The verifier receives the evidence path and a JSON binding as its two arguments;
it must independently authenticate approval for the protected commit, target tree,
and contract digests, then return
`{"schema":"willow.contract-approval.v1","approved":true,"binding":<exact binding>}`.
Only that successful CI handoff yields `APPROVED_BY_POLICY`; `CI=true` alone is
insufficient. See [the runnable contract example](example/contract_comparison.wi).

## What needs language semantics, and what LSP/MCP can provide

Symbol navigation, checked types, references, and exact rename can be exposed
through LSP or MCP for any static language with suitable compiler tooling.
Willow's rename example demonstrates that useful tooling contract; it is not
unique to a new language. LSP and MCP are interfaces, not sources of semantic
guarantees themselves.

E2604 needs a definition of waiting effects, their propagation through calls,
and a language rule connecting those effects to lexical lock scopes. Enforcing
that rule also depends on compiler lowering, cleanup, and runtime behavior.
An LSP/MCP server can expose such a rule where a language or analyzer implements
it; adding the protocol alone does not establish it. Willow puts this specific
contract in normal compilation and exposes the same knowledge through its CLI.

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

For optional static contract checking, install Z3 4.15.4 (`python -m pip install
z3-solver==4.15.4.0`) and keep `z3` on PATH, or set `WILLOW_Z3` to its executable.
Run `willow run example/static_verification.wi` for a `@pure @verified` example.
For `while` proofs, add boolean `invariant` clauses and one i64 `variant` before
the body; see [`example/verified_loops.wi`](example/verified_loops.wi). The verifier
checks entry, preservation, and a nonnegative, strictly decreasing rank on
iterations. Annotations are proof-only. Loops without both annotations, `for`,
`break`, and `continue` remain unsupported in verified functions.
Unproved contracts fail the build. Verification JSON and replayable SMT files
are saved under `target/willow-verification` (override with `WILLOW_VERIFICATION_DIR`).

## Current boundaries

Willow is an experimental language and compiler, **not yet suitable for production deployment**. Its effect guarantees are scoped to the supported language operations and available compiler/runtime knowledge. In particular:

- E2604 prevents modeled blocking/suspending behavior in Willow lexical lock bodies; it does not prove arbitrary programs deadlock-free.
- Conservative dynamic/indirect-call analysis may reject code that is actually safe. Unavailable evidence is not treated as proof of purity.
- Lock-order analysis conservatively rejects uncertain identities and acquisition cycles (E2605); simultaneous acquisition handles runtime ordering of distinct objects in the same class.
- Native/foreign boundaries, compiler defects, and runtime defects are outside the guarantee of a static effect check.
- The runtime benchmarks and agent-token experiment measure limited, published workloads.

**The goal:** make semantic safety properties part of the compiler's ordinary contract, and make their evidence accessible to both humans and coding agents.

MIT licensed. See [LICENSE](LICENSE).
