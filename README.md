# Willow

[![Status: experimental](https://img.shields.io/badge/status-experimental-orange)](https://github.com/lechatthecat/willow)
[![Release](https://img.shields.io/github/v/release/lechatthecat/willow)](https://github.com/lechatthecat/willow/releases/latest)
[![CI](https://github.com/lechatthecat/willow/actions/workflows/ci.yml/badge.svg)](https://github.com/lechatthecat/willow/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**An experimental native, garbage-collected language with compiler-checked concurrency rules and opt-in contracts. Not production-ready.**

Willow is statically typed and compiles through [Cranelift](https://cranelift.dev/).
It combines enums, pattern matching and classes with lightweight tasks, channels
and lexical locks. Its semantic CLI exposes the compiler's evidence to people
and coding agents.

## Concurrency checks: what the compiler guarantees

Ordinary `willow check` and `willow build` enforce the following rules. These
are guarantees within the compiler's modeled Willow semantics and the stated
assumptions, not proofs about arbitrary native code.

| Check | Guarantee for accepted code | Main limit or assumption |
| --- | --- | --- |
| [E2402 / E2408](#e2402-async-calls-capture-only-shareable-or-exclusively-lent-values) | Async captures are shareable or exclusively lent; transferred values cannot be used through old aliases. Non-`Sync` immutable statics are rejected in task-reachable code. | Conservative reachability and ownership rules can reject safe code; native shared memory is not analyzed. |
| [E2604](#e2604-a-lock-body-never-waits) | A lexical `lock` body contains no operation classified as waiting, including through calls; missing effect evidence is rejected. | Console output is allowed and can stall. Reviewed `nonblocking` foreign declarations are trusted. Computation time is unbounded. |
| [E2605](#e2605-nested-locks-follow-one-acyclic-order) | Nested lexical locks follow one acyclic acquisition order without reacquiring a held lock. | Locks the analysis cannot distinguish cannot be nested; use a [group lock](#acquiring-multiple-locks), whose acquisition is not fair. |
| [`@effects(...)`](#effect-contracts-declare-the-property-on-any-function) | An annotated function excludes the declared effects within the effect model, including transitive calls; unknown effects are rejected. | Native declarations are reviewed assumptions. `no_alloc` covers managed language allocation, not all native allocation or runtime bookkeeping. |

**Main limits:** these checks do not prove global deadlock freedom or the absence
of all data races. Tasks can still wait forever through channels and `await`
outside locks. Native adapters, compiler/runtime defects and unmodeled behavior
remain outside the guarantees. Conservative analysis can reject safe programs.
The [details below](#details-and-evidence) link to public examples, tests and
implementation evidence.

<a id="e2607-a-bug-finder-for-some-permanent-waits"></a>

**Bug finder, not a guarantee:** [E2607](src/semantic/permanent_wait.rs)
reports some permanent waits in a small straight-line subset. Unsupported code
can leave the whole program unanalyzed. The absence of E2607 provides **no
guarantee of progress**. See the [positive and unsupported cases](src/semantic/permanent_wait/tests.rs).

**Runtime guarantee:** an unfinished cooperative `async main` at global
quiescence fails with a channel/await wait witness and exit status 1. This detects
an observed whole-program stop, not partial deadlocks while other work or wake
sources remain. See the [precise scope and exclusions](#runtime-channelawait-deadlock-detection).

## Runtime channel/await deadlock detection

Willow uses **runtime global-quiescence detection** for channel/await deadlocks.
It does not require a new language subset or a static proof that every accepted
program terminates. E2607 remains a separate, incomplete compile-time bug finder.

**Guarantee for the covered execution.** When an unfinished `async main` and
its tasks are suspended on cooperative channel operations, `await`, or `select`,
and no runnable task, executing poll, in-flight scheduler claim, timer, registered
I/O waiter or outstanding blocking operation remains, the entry driver reports
`runtime deadlock` on stderr with a wait witness and terminates with exit status
1. It cannot treat that global stop as successful completion of main. This
covers empty receives, full bounded sends, channel cycles and await chains or
cycles once the whole execution reaches that state. Branches, loops and imported
helpers do not by themselves exclude a runtime observation.

The witness starts at main and follows registered await targets. It lists task
IDs, available names and spawn sites, and channel registrations (`RecvWait`,
`SendWait`, or ownership/handoff roles) with process-local channel identifiers.
For example, a main awaiting a receiver can produce:

```text
runtime deadlock: task 1 (main) blocked on await; no runnable task or wake source remains
witness (registered waits; select entries are alternatives):
  task 1 (main) blocked on await
    await -> task 2
  task 2 (receiver) blocked on channel/select
    RecvWait -> channel 0x1234
```

IDs, names, addresses and registration order vary. Channel identifiers are
opaque identities, not values or pointers to dereference. A channel registration
does not identify a potential sender; a select's entries are alternatives, not
edges that must all complete. The witness describes an observed global stop,
not a minimal cycle proof or a source-level causal trace. To bound diagnostic
size, it expands at most 64 tasks and prints at most 256 await/channel
registrations, explicitly marking truncation. Truncation does not change the
stop decision or exit status. Witness collection runs only after failure has
been established, using existing metadata with no extra per-operation tracking.

**Correct progressing programs remain accepted.** A queued sender, task
completion, timer, I/O event or blocking-operation completion keeps the driver
alive; returning main succeeds without waiting for detached tasks. See the
runnable [channel protocol](example/channel_wait_protocol.wi), which seeds a
bounded request before awaiting its relay and drains a closed reply channel.

**These cases have no channel/await deadlock-detection guarantee:**

- A subset of tasks is deadlocked while another task keeps running or yielding,
  or keeps a timer, I/O registration or blocking operation outstanding. An
  unrelated wake source also prevents a global-stop diagnosis.
- An external peer never responds, a blocking syscall never finishes, a native
  function violates its declared effects, or the program livelocks or starves.
  Elapsed time alone is never evidence of deadlock.
- Synchronous `main`, synchronous nested scheduler drives and native blocking
  paths. Some have their own panic diagnostics, but are not covered by the
  async-entry witness contract above.
- Detached blocked tasks after main has returned. Process exit intentionally
  does not require those tasks to finish.

Neither successful compilation nor a successful execution proves freedom from
deadlock on other executions. This contract assumes the compiler's cooperative
lowering and runtime wake-source bookkeeping, and does not certify foreign code.
Evidence: [entry driver and wake-source checks](crates/willow_runtime/src/scheduler.rs),
[witness traversal](crates/willow_runtime/src/scheduler_deadlock.rs), and
[executable deadlock and progress cases](tests/integration/concurrency/async_deadlock.rs).

## Project status

Willow is a research/engineering project exploring compiler enforcement and
agent-readable evidence for concurrent programs. Network services are a target
use case, but Willow is not currently a production substitute for Go or JVM
stacks. Its library and tooling ecosystem is still developing. Rust crates can
be used through [handwritten adapters](#rust-interop), not automatic access to
an entire crate's API. Choose it for experimentation and evaluation.

This README describes `main`; some features are newer than the latest release.

## Try Willow

Download a [prebuilt release](https://github.com/lechatthecat/willow/releases/latest)
for Linux x86_64, macOS arm64/x86_64, or Windows x86_64. Keep the distributed
`bin/` and `lib/` directories together. A system C toolchain is required for
linking; Rust is not required unless using Rust interop. Releases include checksums.

```sh
willow --version
willow init hello
cd hello
willow run
```

Or build from source (Rust 1.96+):

```sh
git clone https://github.com/lechatthecat/willow.git
cd willow
cargo build --release
./target/release/willow --version
```

See [installation and toolchain requirements](RELEASE_README.md).

## A taste of Willow

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

A contended acquisition parks the task; compiler-managed cleanup releases the
lock on supported exits. E2604 rejects waiting operations in the body, including
inside synchronous helpers. The console output above is an explicit exception.
See the [standard library](std/README.md) and [runnable examples](example/README.md)
for collections, classes, enums, error handling and I/O.

## Details and evidence

### Rust interop

Rust crates require concrete, handwritten adapters and Cargo/Rust. Willow generates
ABI wrappers, but does not analyze native bodies. Ordinary synchronous foreign
calls carry blocking effects and are rejected in tasks and lexical locks.

- <a id="boundary-types"></a>**Boundary types:** scalars, strings, supported arrays, records, opaque handles and composed Option/Result values. Arbitrary classes, closures and raw pointers are not boundary types. See the [record mapping](example/rust_record_bridge/README.md), [declaration parser](src/parser/rust_declarations.rs) and [type mapping](src/rust_bridge_ir.rs).
- <a id="async-calls-willow-task-awaits-rust-future"></a>**Async direction:** Willow can await an adapter's Rust Future; this does not enable Rust-to-Willow callbacks. Inputs are owned, and the Future must be `Send + 'static`. Adapters must arrange native runtime services. See the [async example](example/rust_async_pull) and [interop directions](protocol/ai_toolchain/RUST_BRIDGE.md#remaining-interop-directions).
- <a id="where-the-guarantees-stop"></a>**Trust boundary:** reviewed `nonblocking` and `no_io` declarations are developer assertions, including conversions and destructors. A current review fingerprint is required; native behavior is not proved. See [bounded calls](protocol/ai_toolchain/RUST_BRIDGE.md#bounded-synchronous-calls-from-tasks) and [reviewed assumptions](protocol/ai_toolchain/RUST_BRIDGE.md#reviewed-effect-assumptions).

### Concurrency details

The [small concurrency core](formal/README.md) states the Send/Sync and loan,
E2604 and lock-order judgments, soundness assumptions, bounded executable
checks, implementation differential tests and trusted computing base.

<a id="e2402-async-calls-capture-only-shareable-or-exclusively-lent-values"></a>

**Captures and transfers (E2402/E2408).** `Send` and `Sync` are structurally inferred.
Mutable arrays/maps are not `Sync`; a fresh, unescaped local value with `Send`
contents can be exclusively lent to a task until its unconditional await in the same block. Old aliases of a
transferred value are unavailable. Loops and escaping values have stricter rules.
Task-reachable non-`Sync` static accesses are rejected, including through helpers;
indirect calls use conservative candidates. See [loan examples](example/owned_task_loans.wi),
[ownership transfer](example/ownership_transfer.wi), [capture tests](tests/integration/concurrency/capture_checking.rs)
and [static sharing checks](src/semantic/type_checker/static_sharing.rs).

<a id="sending-after-channel-close"></a>

**Closed channels.** Sending after close raises a recoverable panic; accepted
buffered values remain available to receive. A selected closed send arm also
panics before its body. See the [closed-send example](example/channel_closed_send.wi).

<a id="e2604-a-lock-body-never-waits"></a>
<a id="how-the-effect-analysis-follows-calls"></a>

**Lexical lock restrictions (E2604).** `await`, `select`, channel waits, blocking
cells and synchronous file APIs are rejected inside a lock body. The analysis
follows helpers, dispatch and imports; unknown calls retain conservative effects.
Console output and reviewed nonblocking foreign calls are exceptions; an endless
computation is not rejected. See [effect propagation](src/compiler_db/effects.rs),
[lock restriction tests](tests/integration/concurrency/lock_restrictions.rs),
[indirect-call tests](tests/integration/concurrency/indirect_lock.rs) and the
[lock example](example/scheduler_aware_lock.wi).

<a id="effect-contracts-declare-the-property-on-any-function"></a>

**Effect contracts.** `no_block` excludes blocking, `no_suspend` excludes suspension,
and `no_wait` combines both. `no_panic` excludes modeled language faults, `no_alloc`
excludes managed language allocation, and `no_io` excludes modeled external access.
They include transitive effects and reject unknown effects; overrides cannot weaken
inherited contracts. Fault-reporting allocation and runtime housekeeping are outside
`no_alloc`; `no_io` is not an OS sandbox, and `no_panic` does not exclude resource
exhaustion. Changing an annotation still requires review. See [contract examples](example/effect_contracts.wi),
[I/O examples](example/io_contracts.wi), [checker](src/semantic/type_checker/effect_contracts.rs)
and [contract tests](tests/effect_contracts.rs).

### Contracts: from run-time checks to proofs

<a id="verified-prove-a-contract-with-z3"></a>

**`@pure @verified`** proves supported value contracts under their preconditions
using Z3. The subset includes integer scalars, bool/char, String equality and byte
lengths, read-only frozen arrays and closed immutable data. String lengths count UTF-8 bytes, including embedded NUL; concatenation
and other String methods remain unsupported. Unsupported constructs and unsuccessful
proofs fail closed. This trusts the compiler and solver and does not verify the
whole program. Install Z3 4.15.4
and keep `z3` on PATH, or set `WILLOW_Z3`. See the [proof example](example/static_verification.wi),
[strings](example/verified_strings.wi), [immutable data](example/verified_data.wi), [loops](example/verified_loops.wi) and
[solver-backed no-panic](example/proven_no_panic.wi).

<a id="pure-and-run-time-contracts"></a>

**`@pure`** rejects observable interaction outside arguments and results, including
mutable heap reads and unknown callees. **`requires`/`ensures`** remain runtime
checks in debug and release builds; violations raise recoverable panics. See
[pure functions](example/pure_functions.wi) and [runtime contract cases](tests/regressions/runtime_contracts.wi).

### Locks designed together with the compiler and scheduler

<a id="e2605-nested-locks-follow-one-acyclic-order"></a>

**Nested locks (E2605)** follow a project-wide acyclic order and cannot reacquire
a held lock. Indistinguishable origins, including parameters of the same lock type,
are conservatively rejected. Lexical cleanup releases acquired locks on supported
exits, panic and cancellation; it does not roll back writes. See [ordered locks](example/ordered_locks.wi),
[parent/child locks](example/parent_child_locks.wi) and [ordering and cleanup tests](tests/integration/concurrency/ordered_locks.rs).

<a id="acquiring-multiple-locks"></a>

**Group locks**, such as `lock from as mut debit, to as mut credit { ... }`, acquire
members in runtime identity order. A contended group releases acquired members
before parking. Duplicate handles are rejected. Group acquisition is not fair
and has no retry bound, so starvation remains possible. See the [group implementation
and limitations](crates/willow_runtime/src/lock_group.rs).

### Semantic tooling for people and AI agents

The CLI provides `effects`, `impact`, `refs`, `type`, `symbol`, `rename` and
`overview`, with structured output. See the [AI toolchain protocol](protocol/ai_toolchain/README.md).

- <a id="exact-rename-change-a-symbol-preserve-unrelated-names"></a>**Exact rename** edits resolved occurrences and validates the candidate. It covers supported symbols in the loaded project, not strings or external consumers. Preview with `willow rename SELECTOR NEW_NAME --dry-run --diff`; see [rename tests](tests/semantic_rename.rs).
- <a id="what-an-agent-can-trust"></a>**Effect evidence:** `MAY` has a witness, `UNKNOWN` retains a conservative capability without one, and `NONE` has neither a witness nor uncertainty in the modeled analysis. Missing evidence is not purity. See the [effect-state example](example/effect_states.wi).
- <a id="compare-scalar-contracts-across-an-edit"></a>**Contract comparisons** do not prove behavioral equivalence. `contract_preserved` does not imply `implementation_verified`; approval requires an independently trusted policy verifier. See [comparison](example/contract_comparison.wi), [repair](example/contract_repair.wi) and [contract comparison tests](tests/contract_comparison_cli.rs).
- <a id="what-needs-language-semantics-and-what-lspmcp-can-provide"></a>**LSP/MCP** can expose semantic results, but the underlying language/analyzer establishes the guarantees. The transport alone does not establish lock safety or proof validity.

### How this relates to other approaches

Willow explores selected concurrency rules and compiler-readable evidence, using
GC and familiar task/lock/channel constructs. This is a narrower claim than
whole-program safety. Navigation and exact rename can also be implemented for
other languages; this tooling does not establish an ecosystem advantage.

### Measurements

<a id="measured-ai-agent-efficiency-a-secondary-benefit"></a>

Agent cost and accuracy depend on the task and model. The semantic CLI is a tool
to evaluate in a workflow, not a guarantee of reduced tokens or correct edits.

<a id="runtime-performance-strengths-and-gaps"></a>

The [runtime comparison](benches/go_vs_willow/MEASUREMENT_2026-10-25.md) includes
workload-specific results, methods and raw data. The [benchmark harness](benches/go_vs_willow/README.md)
explains reproduction. These measurements do not establish general runtime superiority.

### Current boundaries

Willow remains experimental and is not suitable for production deployment.
The guarantees cover modeled language operations and depend on compiler/runtime
correctness and declared native assumptions. They do not prove termination,
all-race freedom, fair scheduling or global deadlock freedom. Conservative checks
can reject safe programs; absence of a bug-finder diagnostic adds no guarantee.
Contracts constrain edits only while their annotations and assumptions remain valid.

MIT licensed. See [LICENSE](LICENSE).
