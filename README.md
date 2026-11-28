# Willow

[![Status: experimental](https://img.shields.io/badge/status-experimental-orange)](https://github.com/lechatthecat/willow)
[![Release](https://img.shields.io/github/v/release/lechatthecat/willow)](https://github.com/lechatthecat/willow/releases/latest)
[![CI](https://github.com/lechatthecat/willow/actions/workflows/ci.yml/badge.svg)](https://github.com/lechatthecat/willow/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

Willow is an experimental, statically typed, garbage-collected language that
compiles to native code through [Cranelift](https://cranelift.dev/). It has
enums, pattern matching, classes, lightweight tasks, channels and lexical locks.
The compiler enforces a set of concurrency rules, and a semantic CLI exposes its
analysis to people and coding agents. Willow is not ready for production use.

<a id="concurrency-checks-what-the-compiler-guarantees"></a>

## What the compiler checks

`willow check` and `willow build` reject programs that break these rules:

| Check | Accepted code | Limits |
| --- | --- | --- |
| [E2402 / E2408](#e2402-async-calls-capture-only-shareable-or-exclusively-lent-values) | Values captured by a task are shareable or exclusively lent to it. A transferred value can't be used through its old aliases. Tasks can't reach non-`Sync` immutable statics. | The analysis is conservative and can reject safe code. Native shared memory isn't analyzed. |
| [E2604](#e2604-a-lock-body-never-waits) | A `lock` body never waits, including inside the functions it calls. Calls with unknown effects are rejected. | Console output is allowed. Rust functions marked `#[willow::assume(no_block)]` are trusted. Long computations are allowed. |
| [E2605](#e2605-nested-locks-follow-one-acyclic-order) | Nested locks follow one acyclic order and never reacquire a held lock. | Locks the analysis can't tell apart can't be nested; use a [group lock](#acquiring-multiple-locks) instead. |
| [`@effects(...)`](#effect-contracts-declare-the-property-on-any-function) | A function has none of the declared effects, including through its calls. Unknown effects are rejected. | Native declarations are trusted. `no_alloc` covers Willow allocation, not native allocation. |

These rules don't exclude every deadlock or data race: tasks can still wait
forever on channels or `await` outside a lock. See [current limits](#current-boundaries).

<a id="e2607-a-bug-finder-for-some-permanent-waits"></a>

[E2607](src/semantic/permanent_wait.rs) reports some permanent waits in simple
straight-line code. It is a bug finder: a program without E2607 can still hang.
See the [supported and unsupported cases](src/semantic/permanent_wait/tests.rs).

At run time, when an unfinished `async main` and all of its tasks are stuck on
channels or `await`, the program prints a wait witness and exits with status 1.
See [deadlock detection](#runtime-channelawait-deadlock-detection).

## Project status

Willow is a research project on compiler-enforced concurrency rules and
machine-readable compiler evidence. Network services are a target use case,
but the standard library and tooling are still small. Rust crates are used
through [handwritten adapters](#rust-interop).

This README describes `main`; some features are newer than the latest release.

## Try Willow

Download a [prebuilt release](https://github.com/lechatthecat/willow/releases/latest)
for Linux x86_64, macOS arm64/x86_64 or Windows x86_64, and keep its `bin/` and
`lib/` directories together. Linking needs a system C toolchain. Rust is needed
only for Rust interop. Releases include checksums.

```sh
willow --version
willow init hello
cd hello
willow run
```

To build from source (Rust 1.96+):

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

If the lock is taken, the task parks until it is free. The lock is released on
every exit from the body. E2604 rejects waiting inside the body, including in
called functions; console output is the one exception. See the
[standard library](std/README.md) and [runnable examples](example/README.md).

## Language essentials

A function, method or block lambda returns its final expression when it has no
trailing semicolon. Each branch of a final `if` / `else` or `match` can supply
the value. `return value;` is still available for early returns. A semicolon
discards a value; a non-void function that can reach its end without one
reports E0205. Loop and `defer` bodies never return from the enclosing function.

```willow
class User { pub score: i64; }

fn score(user: User) -> i64 {
    match user.score { 0 => 10, n => n }
}

fn main() {
    let user = User(2);
    println(score(user));
    println(2.0); // 2.0
}
```

`User(args...)` and `new User(args...)` call the same constructor; both forms
are supported. A local callable or function with the same name as a class takes
precedence.

`print` and `println` are ordinary functions from `std::io`, available without
an import. They can be imported, aliased, shadowed or called as
`std::io::println(value)`. Each takes one printable value. To pass one as a
callback, give it a concrete type with a lambda such as
`|value: i64| println(value)`. Printing scalars never panics, so it adds no
panic path to the caller.

`f64` values print with a decimal point when integral: `2.0`, `0.0`, `-0.0`.
This applies to output, `.to_string()`, `format` and array display. Values
outside `1e-6 <= abs(x) < 1e21` use scientific notation (`1e30`), and finite
values round-trip exactly. Integers still print as `2`. JSON numbers keep
their source spelling, so parsing and writing `2` yields `2`. See the
[language example](example/concise_surface.wi) and
[float display example](example/f64_integral_display.wi).

## Integer overflow checks

`i64` arithmetic is checked in debug builds (the default) and wraps in release
builds. `--overflow-checks` changes this independently of the build profile:

```sh
willow run example/integer_overflow.wi --release --overflow-checks=true
willow run example/integer_overflow.wi --debug --overflow-checks=false
```

`build`, `run` and `debug` accept `--overflow-checks=true|false` or
`--overflow-checks true|false`; the last value wins. In the library API, set
`options.target.overflow_checks: Option<bool>`, where `None` uses the profile
default.

The setting covers `+`, `-`, `*`, `**`, unary `-`, `+=`, `-=` and `*=` on
`i64`, in every compiled module. A failed check raises a recoverable
`integer overflow` panic with the source location and runs `defer`. With checks
off, arithmetic wraps in two's complement. `wrapping_*` and `checked_*` methods,
division, remainder, shifts, other integer widths and array bounds keep their
own behavior. See the [example](example/integer_overflow.wi).

## Runtime channel/await deadlock detection

When an unfinished `async main` and all of its tasks are suspended on channel
operations, `await` or `select`, and nothing else can wake them (no runnable
task, timer, I/O waiter or blocking operation), the runtime prints
`runtime deadlock` with a wait witness and exits with status 1. This covers
empty receives, full bounded sends, channel cycles and await chains.

```text
runtime deadlock: task 1 (main) blocked on await; no runnable task or wake source remains
witness (registered waits; select entries are alternatives):
  task 1 (main) blocked on await
    await -> task 2
  task 2 (receiver) blocked on channel/select
    RecvWait -> channel 0x1234
```

The witness starts at main and follows await targets, listing task IDs, names,
spawn sites and channel waits. IDs, names and addresses vary between runs.
Channel identifiers are opaque; a registration doesn't name a sender, and
`select` entries are alternatives. Output is capped at 64 tasks and 256
registrations, with truncation marked. The witness is collected only after the
stop is detected, so normal execution pays nothing for it.

A program that is still making progress is never reported: a queued sender,
finished task, timer, I/O event or completed blocking call keeps it running.
Returning from main succeeds without waiting for detached tasks. See the
[channel protocol example](example/channel_wait_protocol.wi).

Not detected:

- Some tasks are stuck while another keeps running, or keeps a timer, I/O
  registration or blocking call outstanding.
- An external peer never answers, a system call never returns, native code
  breaks its declared effects, or the program livelocks.
- Synchronous `main`, nested synchronous scheduler drives and native blocking
  paths.
- Detached tasks still blocked after main returns.

See the [scheduler](crates/willow_runtime/src/scheduler.rs),
[witness traversal](crates/willow_runtime/src/scheduler_deadlock.rs) and
[deadlock tests](tests/integration/concurrency/async_deadlock.rs).

## Runtime instances

A Willow program runs on one process-wide runtime. Creating and managing
several runtimes from a host application isn't supported yet; see
[embedding](crates/willow_runtime/EMBEDDING.md).

## Details

<a id="details-and-evidence"></a>

### Rust interop

Put Rust code in `src/native.rs` and write `import native;` in Willow. Public
functions and structs become the interface, and inherent methods become Willow
methods: `pub fn add(a: i64, b: i64) -> i64 { a + b }` is called as
`native::add(20, 22)`. List Cargo crates under `[rust-dependencies]`. Bind an
owned resource with `use resource = native::Resource::new();` to close it when
the scope exits, including on errors and cancellation. See the
[native module example](example/rust_modules).

Willow generates the ABI wrappers but doesn't analyze Rust function bodies.
Synchronous Rust calls made from tasks run on a bounded blocking pool, so the
worker keeps running other tasks while one waits. Inside `no_wait`,
`no_suspend` or a lock body, a Rust call needs a reviewed
`#[willow::assume(no_block)]` attribute. See the
[parallel example](example/rust_blocking_pool) and the
[execution policy](protocol/ai_toolchain/RUST_BRIDGE.md#synchronous-foreign-execution-paths).

- <a id="boundary-types"></a>**Boundary types:** scalars, strings, supported arrays, records, opaque handles, and Option/Result built from them. Classes, closures and raw pointers can't cross. See the [record mapping](example/rust_record_bridge/README.md), [interface extraction](src/rust_module.rs) and [type mapping](src/rust_bridge_ir.rs).
- <a id="async-calls-willow-task-awaits-rust-future"></a>**Async:** Willow can await a Rust `Future` that is `Send + 'static` and owns its inputs. Rust can't call back into Willow. See the [async example](example/rust_async_pull) and [interop directions](protocol/ai_toolchain/RUST_BRIDGE.md#remaining-interop-directions).
- <a id="where-the-guarantees-stop"></a>**Assumptions:** `#[willow::assume(no_block)]` and `no_io` are the developer's claims about Rust code, including its conversions and destructors. Each needs a current review fingerprint. See [bounded calls](protocol/ai_toolchain/RUST_BRIDGE.md#bounded-synchronous-calls-from-tasks) and [reviewed assumptions](protocol/ai_toolchain/RUST_BRIDGE.md#reviewed-effect-assumptions).

### Concurrency

The [concurrency core](formal/README.md) defines the Send/Sync and loan, E2604
and lock-order rules, with executable models and differential tests.

<a id="e2402-async-calls-capture-only-shareable-or-exclusively-lent-values"></a>

**Captures and transfers (E2402/E2408).** `Send` and `Sync` are inferred from a
type's structure. Mutable arrays and maps aren't `Sync`. A fresh local value
with `Send` contents can be lent to one task until that task is awaited
unconditionally in the same block. Values that loop or escape follow stricter rules. Tasks can't reach
non-`Sync` statics, even through helpers. See [loans](example/owned_task_loans.wi),
[ownership transfer](example/ownership_transfer.wi),
[capture tests](tests/integration/concurrency/capture_checking.rs) and
[static sharing](src/semantic/type_checker/static_sharing.rs).

**Shared mutable objects.** Calling an `async fn` starts a task; there is no
`spawn` keyword. A mutable object can't be shared with another task through
arguments, closures, channels or fields while unprotected access to it remains.
The diagnostic names the boundary or alias involved.

To share mutable state, use `Mutex<T>`, `RwLock<T>` or atomics, or share frozen
data. Sending a non-`Sync` object through a channel transfers it: the sender
and its aliases can't use it afterwards. A fresh local object can be lent to one
task and used again after that task is awaited unconditionally in the block
that lent it. See the
[shared-object example](example/shared_object_safety.wi).

To fix rejected code, create the object inside the task, wrap it in a mutex and
share the mutex, send it and stop using it, or send a frozen copy. A factory
function needs a `@sending` return contract to show its result is fresh. Some
race-free code needs these changes because branches, loops, unknown calls and
escaping aliases are analyzed conservatively.

<a id="sending-after-channel-close"></a>

**Closed channels.** Sending on a closed channel raises a recoverable panic;
values already buffered can still be received. A `select` send arm on a closed
channel panics before its body runs. See the
[closed-send example](example/channel_closed_send.wi).

<a id="e2604-a-lock-body-never-waits"></a>
<a id="how-the-effect-analysis-follows-calls"></a>

**Lock bodies (E2604).** `await`, `select`, channel waits, blocking cells and
synchronous file APIs are rejected inside a lock body, including through
helpers, dynamic dispatch and imports. Console output and reviewed nonblocking
Rust calls are allowed. See [effect propagation](src/compiler_db/effects.rs),
[lock tests](tests/integration/concurrency/lock_restrictions.rs),
[indirect-call tests](tests/integration/concurrency/indirect_lock.rs) and the
[lock example](example/scheduler_aware_lock.wi).

<a id="effect-contracts-declare-the-property-on-any-function"></a>

**Effect contracts.** `no_block` excludes blocking, `no_suspend` excludes
suspension and `no_wait` excludes both. `no_panic` excludes language faults,
`no_alloc` excludes Willow allocation and `no_io` excludes external access.
Each covers called functions too, and an override can't weaken an inherited
contract. `no_alloc` doesn't cover allocation for panic reports or runtime
bookkeeping, `no_io` isn't an OS sandbox, and `no_panic` doesn't cover running
out of memory. See [contract examples](example/effect_contracts.wi),
[I/O examples](example/io_contracts.wi),
[checker](src/semantic/type_checker/effect_contracts.rs) and
[tests](tests/effect_contracts.rs).

### Contracts and proofs

<a id="verified-prove-a-contract-with-z3"></a>

**`@pure @verified`** proves a function's contracts with Z3. It
supports integers, bool, char, String equality and byte length, frozen arrays
and immutable data. Other String methods aren't supported yet. Unsupported code
and failed proofs are compile errors. Install Z3 4.15.4 and put `z3` on PATH,
or set `WILLOW_Z3`. See [the proof example](example/static_verification.wi),
[strings](example/verified_strings.wi), [immutable data](example/verified_data.wi),
[loops](example/verified_loops.wi) and
[no-panic proofs](example/proven_no_panic.wi).

A failed proof points at the clause that failed. Replacing `non_negative`'s
body in [this example](example/verification_diagnostics.wi) with `return x;`
gives:

```text
error[E2630]: @verified `non_negative`: COUNTEREXAMPLE: ensures truth: sat; candidate inputs x = -1; result = -1
```

The label points at `result >= 0`. Inputs use source names and Willow values
such as `[-3]`, `new Point(-4)` and `Choice::Yes(-6)`; records built by
explicit constructors are shown as `Point { x: -4 }`. The values come from the
solver, so with loops or abstracted calls they may not reproduce the failure
when run. `UNKNOWN` also fails the proof.

Verification JSON (`willow.verification.v1`) keeps its existing fields. Each
failed obligation also gets a `presentation` object
(`willow.verification.presentation.v1`) with `evidence: "CANDIDATE"`, the
failing `location` and `obligation`, source-named `inputs`, an optional
`result`, and an `incomplete` reason when values can't be shown. Preconditions
that can never hold report `evidence: "VACUOUS_PRECONDITION"`. Ignore
presentation versions you don't recognize.

<a id="pure-and-run-time-contracts"></a>

**`@pure`** forbids any interaction beyond arguments and results, including
reads of mutable heap data and calls to unknown functions.

**`requires` / `ensures`** are checked at run time in debug and release builds
and raise recoverable panics. See [pure functions](example/pure_functions.wi)
and [runtime contract cases](tests/regressions/runtime_contracts.wi).

### Locks

<a id="e2605-nested-locks-follow-one-acyclic-order"></a>

**Nested locks (E2605)** follow one project-wide order and can't reacquire a
held lock. Locks the compiler can't tell apart, such as two parameters of the
same lock type, can't be nested. Locks are released on every exit, panic and
cancellation; writes are not rolled back. See [ordered locks](example/ordered_locks.wi),
[parent/child locks](example/parent_child_locks.wi) and
[tests](tests/integration/concurrency/ordered_locks.rs).

<a id="acquiring-multiple-locks"></a>

**Group locks**, such as `lock from as mut debit, to as mut credit { ... }`,
acquire their members in runtime identity order. If one is taken, the group
releases the others before parking. Duplicate handles are rejected. Group
acquisition isn't fair, so a group can starve. See the
[implementation](crates/willow_runtime/src/lock_group.rs).

**Hand-over-hand traversal** uses `lock walk cursor as node { ... }`, where
`cursor` is a local `Mutex<T>` variable. Set `cursor` to the next node in the
body. At the end of each iteration, the walk takes the next lock before
releasing the current one. `break` ends the walk; `return`, panic and
cancellation release the held lock. Body `defer`s run at the end of each
iteration.

The first lock is waited for normally. Each handover tries once: if the next
lock is taken, or the cursor points back at the held node, the walk panics.
Two walks in opposite directions therefore can't deadlock. In a longer cycle
the walk can revisit an earlier node, so the program decides when to stop. At
most two locks are held during a handover and one while the body runs.
Nesting a walk and another lock is rejected by E2605. See the
[linked-list example](example/lock_coupling.wi).

### Semantic tooling

The CLI provides `effects`, `impact`, `refs`, `type`, `symbol`, `rename` and
`overview`, with structured output. See the
[AI toolchain protocol](protocol/ai_toolchain/README.md).

- <a id="exact-rename-change-a-symbol-preserve-unrelated-names"></a>**Rename** edits every resolved occurrence in the project and checks the result. Strings and code outside the project are untouched. Preview with `willow rename SELECTOR NEW_NAME --dry-run --diff`. See [rename tests](tests/semantic_rename.rs).
- <a id="what-an-agent-can-trust"></a>**Effect states:** `MAY` comes with a witness. `UNKNOWN` means the analysis couldn't decide and treats the effect as possible. `NONE` means the analysis found no way for the effect to happen (native declarations are trusted). See the [effect-state example](example/effect_states.wi).
- <a id="compare-scalar-contracts-across-an-edit"></a>**Contract comparison** checks whether an edit keeps a function's contracts; it doesn't prove the old and new code behave the same. Approving a change needs `--protected-base COMMIT`, `--approval-evidence FILE` and `--approval-verifier EXECUTABLE`. Use a verifier outside the editable checkout; being outside it doesn't make a verifier trusted, so choose one you trust. It must check the evidence and request binding, finish within 10 seconds, and print exactly `{"schema":"willow.contract-approval.v1","approved":true,"binding":<request binding>}`. See [comparison](example/contract_comparison.wi), [repair](example/contract_repair.wi) and [tests](tests/contract_comparison_cli.rs).

### Type parameter queries and rename

A type parameter's selector is `module::owner::parameter`, for example
`util::fold::T` for `fn fold<T>` or `util::TopK::T` for `class TopK<T>`.
`symbol` also accepts `file:line:column` on the parameter or any use of it.
`refs` lists its uses inside the owner, and `rename` changes them together with
the declaration; `--dry-run --diff` previews the edit. Same-named parameters of
other owners don't change. Rename refuses names that would clash inside the
owner. See the [type parameter example](example/semantic_type_parameters.wi).

### Measurements

<a id="runtime-performance-strengths-and-gaps"></a>

The [Willow/Go comparison](benches/go_vs_willow/MEASUREMENT_2026-10-25.md)
gives per-workload results, method and raw data; results vary by workload. The
[benchmark harness](benches/go_vs_willow/README.md) explains how to reproduce
them.

<a id="current-boundaries"></a>

### Current limits

- Willow is experimental and not ready for production.
- The checks cover Willow code. Rust adapters, native threads and native memory
  aren't analyzed; Rust effect declarations are trusted.
- The checks don't prove termination, fair scheduling, freedom from every data
  race or freedom from deadlock outside locks.
- Conservative analysis can reject correct programs.
- The guarantees depend on the compiler, runtime and GC being correct.

MIT licensed. See [LICENSE](LICENSE).
