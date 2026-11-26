# Willow

[![Status: experimental](https://img.shields.io/badge/status-experimental-orange)](https://github.com/lechatthecat/willow)
[![Release](https://img.shields.io/github/v/release/lechatthecat/willow)](https://github.com/lechatthecat/willow/releases/latest)
[![CI](https://github.com/lechatthecat/willow/actions/workflows/ci.yml/badge.svg)](https://github.com/lechatthecat/willow/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**A native, garbage-collected language whose compiler enforces specific concurrency rules, proves opt-in function contracts, and explains program semantics to humans and AI coding agents.**

Willow is statically typed and compiles to native code using [Cranelift](https://cranelift.dev/). It pairs Rust-style enums and pattern matching with classes, a garbage collector and lightweight async tasks, and includes its own async runtime, package manager, and incremental compiler infrastructure.

Its premise is simple: **when a person or an AI agent changes a program, the compiler should do more than check syntax and types. It should reject certain unsafe behaviors and explain the program's semantics in a form the agent can use.**

**Keep declared properties intact as code changes.** Effect analysis also lets developers express requirements as compiler-checked contracts. With [`@effects(...)`](#effect-contracts-declare-the-property-on-any-function), an AI agent can change an implementation while the compiler checks that it still satisfies the declared restrictions, such as no I/O or no managed allocation. These checks include effects introduced through calls to other functions.

**Built for concurrent network services.** Willow is aimed at API servers and backends often written in Go or on the JVM. It combines lightweight tasks and familiar syntax with compile-time checks for specific concurrency hazards, such as unsafe task captures and waiting while holding a Willow lock.

**Readable first, proven where it matters.** Willow code is meant to be read, by people and by AI agents alike. Its syntax is deliberately familiar, so anyone who knows Rust or Kotlin can follow it and agents can apply what they already know. Concurrency checks run in the compiler without adding concepts to everyday code, and proof is opt-in: mark the functions that matter with [`@verified`](#verified-prove-a-contract-with-z3) and leave the rest as ordinary code.

**Status:** Experimental research/engineering project. **Not production-ready.** An accepted program has specific guarantees: async calls capture only shareable or exclusively lent values (E2402), a `lock` body never waits (E2604), and nested locks follow one acyclic order (E2605). These are not a proof that a Willow program is free of deadlocks, data races or other bugs. See [what each check guarantees](#concurrency-checks-what-the-compiler-guarantees) and [Current boundaries](#current-boundaries).

This README describes the current `main` branch. Some features are newer than the latest prebuilt release; see [Try Willow](#try-willow).

**Contents:**
- [A taste of Willow](#a-taste-of-willow)
- [Try Willow](#try-willow)
- [Rust interop](#rust-interop)
- [Concurrency guarantees](#concurrency-checks-what-the-compiler-guarantees)
- [Proven contracts](#contracts-from-run-time-checks-to-proofs)
- [Locks](#locks-designed-together-with-the-compiler-and-scheduler)
- [Semantic tooling](#semantic-tooling-for-people-and-ai-agents)
- [Related approaches](#how-this-relates-to-other-approaches)
- [Measurements](#measurements)
- [Current boundaries](#current-boundaries)

## A taste of Willow

```willow
import std::collections::Array;

enum Shape {
    Circle(f64),
    Rectangle(f64, f64),
}

fn area(shape: Shape) -> f64 {
    return match shape {
        Shape::Circle(r) => 3.14 * r * r,
        Shape::Rectangle(w, h) => w * h,
    };
}

class User {
    pub name: String;
    pub age: i64;

    pub fn birthday(self) -> User {
        return new User(self.name, self.age + 1);
    }
}

fn find_age(users: Array<User>, name: String) -> Option<i64> {
    for user in users {
        if user.name == name { return Some(user.age); }
    }
    return None;
}

async fn double_later(n: i64) -> i64 {
    await sleep(1);
    return n * 2;
}

async fn main() {
    let users = [new User("Alice", 32).birthday()];
    if let Some(age) = find_age(users, "Alice") {
        println(age); // 33
    }

    println(area(Shape::Rectangle(2.0, 3.0))); // 6

    let task = double_later(21); // creates the Task
    println(await task); // 42
}
```

A few things to notice:

- **No ownership or lifetime annotations.** The garbage collector manages memory; the compiler infers `Send`/`Sync` and checks exclusive local collection loans across tasks.
- **No null.** Absence is `Option<T>`, and `T?` is sugar for it. Recoverable errors use `Result<T, E>` and the `?` operator.
- **Classes are closed by default.** Inheritance needs `open`, and replacing a method needs `override`. A class without an `init` constructor can be created with its fields in declaration order.
- **`async fn` returns a Task.** Calling one creates the Task; `await` waits for its result.
- **`i64` overflow is not silent in debug builds.** It raises a recoverable panic; `--release` wraps. Methods such as `wrapping_add` and `checked_add` behave the same in both modes.
- **An unhandled panic ends the program with exit status 101** on Linux, macOS and Windows, after its defers run. It does not abort, so no core dump or crash reporter is involved. `main` returning `Err` exits with 1; only runtime integrity failures abort.
- **Panic locations depend on the build.** A debug build reports where a runtime fault happened and its call stack. A `--release` build does not record a location for faults such as an out-of-bounds index; it prints a note instead, and an async frame that stopped inside a call is marked `(in a call made here)`. `--release --debug-info` keeps release optimizations and adds the location and call stack, at the cost of recording each call.

| Area | What is available | Examples |
| --- | --- | --- |
| Data | Classes, inheritance, interfaces, enums, generics | [classes](example/class_hierarchy.wi), [enums](example/enum_match.wi), [generic interfaces](example/generic_interfaces.wi) |
| Control flow | `match`, guards, `if let`, `while let`, `for` | [guards](example/if_let_guards.wi), [loops](example/for_loops.wi) |
| Errors | `Option`, `Result`, `?` with error conversion, recoverable panics | [Option and Result](example/option_result.wi), [conversion](example/error_conversion.wi) |
| Functions | Closures and function values | [closures](example/function_closure_wrappers.wi) |
| Concurrency | Tasks, channels, `select`, cancellation, `Mutex`, `RwLock` | [channels](example/bounded_channel.wi), [select](example/select.wi), [parallel map](example/parallel_map.wi) |
| I/O | Files and TCP | [files](example/file_io.wi), [TCP echo](example/tcp_echo.wi) |
| Standard library | `HashSet`, ordered `SortedMap`, `StringBuilder`, `ByteBuffer`, paths, environment, JSON; [API and complexity reference](std/README.md) | [sets](example/hash_set.wi), [byte buffers](example/byte_buffer.wi), [paths](example/path_ops.wi) |
| Packages | Modules and `willow add` dependencies | [imports](example/import_demo) |
| Rust crates | `extern rust` adapters for `[rust-dependencies]` | [regex bridge](example/rust_regex), [bridge protocol](protocol/ai_toolchain/RUST_BRIDGE.md) |

The [`example/`](example/) directory has hundreds of small programs, and [`example/README.md`](example/README.md) explains many language details.

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

[`RELEASE_README.md`](RELEASE_README.md) covers installation, library layout, checksums and toolchain requirements.

## Rust interop

Use existing Rust crates through an explicit, **handwritten Rust adapter**.
`[rust-dependencies]` lets Cargo resolve crates; it does not automatically expose
all their APIs to Willow. The compiler generates the ABI wrappers around your
adapter. Rust interop needs Cargo and a Rust toolchain in addition to the native
linker, even when using a prebuilt Willow compiler.

The runnable [regex example](example/rust_regex) uses the crates.io `regex` crate.
Its `project.toml` declares the dependency and adapter file:

```toml
[project]
name = "rust_regex"
version = "0.1.0"

[rust]
bridge = "rust/bridge.rs"

[rust-dependencies]
regex = "1"
```

In `rust/bridge.rs`, wrap the crate API with a public Rust function:

```rust
pub fn regex_is_match(pattern: &str, text: &str) -> Result<bool, String> {
    regex::Regex::new(pattern)
        .map(|regex| regex.is_match(text))
        .map_err(|error| error.to_string())
}
```

Declare that adapter in `src/main.wi` and call it like a Willow function:

```willow
extern rust {
    fn regex_is_match(pattern: String, text: String) -> Result<bool, String>;
}

fn main() {
    match regex_is_match("^hello [a-z]+$", "hello willow") {
        Result::Ok(matched) => println(matched),
        Result::Err(error) => println(error),
    };
}
```

From the repository root, run `willow run example/rust_regex`. The checked-in
example also tests an invalid pattern: its output is `true`, `Err`, `true`, each
on its own line. The first run may download Rust dependencies.

### Boundary types

These are the supported **synchronous** adapter mappings:

| Willow declaration | Rust adapter input | Rust adapter output |
| --- | --- | --- |
| `i8`–`i64`, `u8`–`u64`, `char`, `f64`, `bool` | Same scalar | Same scalar |
| Omitted return / `void` | Not a parameter type | `()` |
| `String` | `&str` | `String` |
| `Array<u8>` | `&[u8]` (packed bytes) | `Vec<u8>` |
| `Array<i64>` | `Vec<i64>` (numeric values) | `Vec<i64>` |
| `Array<f64>`, `Array<bool>` | `Vec<f64>`, `Vec<bool>` | Same vector type |
| `Array<String>` | `Vec<&str>` | `Vec<String>` |
| `opaque Handle;` | `&Handle` | `Box<Handle>` |
| Record declared inside `extern rust` | Generated borrowed input record | Generated owned output record |
| `Option<T>`, `Result<T, E>` | Rust `Option` / `Result` of mapped inputs | Rust `Option` / `Result` of mapped outputs |

Option and Result can nest supported boundary types. Arrays support integer,
character, float, boolean and String elements. Byte transport uses one bulk copy
per direction, including async inputs (owned `Vec<u8>`). To migrate old byte
declarations, replace `Array<i64>` with `Array<u8>`; annotate byte array literals
and convert computed integers using `checked_to_u8()`, handling `None`.
`ByteBuffer::from_string(text).to_array()` extracts UTF-8 bytes;
`ByteBuffer::from_array(bytes).decode_utf8()` returns a strict UTF-8 Result.
Arbitrary Willow classes, maps, tuples, function/closure values, raw pointers and arbitrary Rust structs are not boundary types. Records require
public value fields, with no methods, inheritance, generic parameters or recursive
layout; declare nested records first. See the [record mapping and ownership
rules](example/rust_record_bridge/README.md) and [opaque handle example](example/rust_opaque_bridge).
Synchronous adapters must not retain borrowed inputs beyond the call. Opaque
values expose a generated `Handle_close` operation rather than native pointers.
The native opaque payload must satisfy `Any + Send + Sync` (including a
`'static` lifetime).

There is no direct binding of generic functions or trait APIs: instantiate or
wrap them in a concrete adapter signature. Rust-to-Willow callbacks and automatic
crate-wide binding generation are not supported. The accepted signatures and
diagnostics are defined by [the declaration parser](src/parser/rust_declarations.rs)
and [boundary type mapping](src/rust_bridge_ir.rs).

### Async calls: Willow Task awaits Rust Future

Inside `extern rust`, `async fn` supports one-way calls from a Willow Task into
an adapter returning a Rust Future. For example, the runnable
[async pull example](example/rust_async_pull) declares
`async fn respond(request: String) -> String;` and calls
`await respond(request)` from `async fn main`. Its Rust adapter is:

```rust
pub async fn respond(request: String) -> String {
    format!("handled {request}")
}
```

Run `willow run example/rust_async_pull` from the repository root. Rust produces
events and wakes a pending Future; Willow pulls each request and prints
`handled request-0`, `handled request-1`, and `handled request-2`.
This does not let Rust call a Willow function or await a Willow Task.
See [remaining interop directions](protocol/ai_toolchain/RUST_BRIDGE.md#remaining-interop-directions)
for the distinction between this support and broader integration.

Async adapters take **owned** inputs: `String`, `Vec<u8>` and `Vec<String>`
replace synchronous borrowed string/byte inputs. `Vec<f64>` and `Vec<bool>`
remain owned vectors; records use generated
`crate::record_owned::Name` types and opaque handles use
`crate::AsyncHandle<Handle>`. Option and Result compose these mappings.
The Future must be `Send + 'static`; borrowed inputs and non-`Send` Futures
are rejected. Adapters remain concrete, handwritten functions.

Calls must originate in an async context; using an async bridge as a function
value is unsupported. Calls inside Willow lexical locks are rejected (E2604),
and `async fn` cannot also declare `nonblocking`. Async bridges carry
`MAY_SUSPEND` instead of `MAY_BLOCK`, retaining allocation, panic and foreign-call
effects. Rust Future polling must return promptly: declaring an adapter async
does not prove that its native body avoids blocking or make blocking Rust work
preemptible. The bridge does not automatically provide a crate-specific async
runtime such as Tokio; adapters must arrange any required runtime services.

### Where the guarantees stop

Willow does not analyze Rust adapter or dependency bodies. Ordinary synchronous
foreign calls conservatively carry `MAY_BLOCK`, `MAY_ALLOCATE`, `MAY_PANIC` and
`FOREIGN_CALL`; they are rejected in Willow lexical locks (E2604) and in tasks.
This protects the modeled Willow boundary, but cannot prove that native code is
free of blocking, races or deadlocks.

A reviewed `nonblocking("<review_fingerprint>")` declaration permits a bounded
synchronous call from tasks and lock contexts by removing `MAY_BLOCK`. This is a
**developer assertion**, not a compiler proof of the Rust body or of purity.
Conversions and native destructors are part of that assertion. See [bounded
synchronous calls](protocol/ai_toolchain/RUST_BRIDGE.md#bounded-synchronous-calls-from-tasks)
for review freshness, stack limits and remaining effects.

For dependency inspection, update impact and adapter diagnostics, see the
[Rust bridge guide](protocol/ai_toolchain/RUST_BRIDGE.md).

## Concurrency checks: what the compiler guarantees

These checks run during ordinary `willow check` and `willow build`, and each
rejects a program with a compile-time error. The table states what every
**accepted** program is guaranteed. Each section below shows the check and
then lists its limits, with the test or source location that establishes each
limit.

| Check | Guarantee for an accepted program | Main limit |
| --- | --- | --- |
| [E2402, E2408](#e2402-async-calls-capture-only-shareable-or-exclusively-lent-values) | Every reference an `async fn` call captures is `Sync`, or is a fresh local value that nothing else can use until the Task is awaited. A value sent on a channel or put in a `Mutex` cannot be used through its old aliases. | Only these task boundaries are checked: static fields are shared without a check |
| [E2604](#e2604-a-lock-body-never-waits) | A `lock` body never awaits, selects, waits on a channel, uses a blocking cell, calls a synchronous file API or calls foreign code, directly or through any chain of calls. A callee without effect evidence counts as waiting. | Console output is allowed; foreign code declared `nonblocking` is trusted |
| [E2605](#e2605-nested-locks-follow-one-acyclic-order) | Nested `lock` statements across the whole project follow one acyclic order, and no lock is acquired again while held. | Locks that the compiler cannot tell apart cannot be nested; use a group `lock` |
| [`@effects(...)`, E2610–E2612](#effect-contracts-declare-the-property-on-any-function) | A function with an effect contract performs the forbidden effect nowhere, directly or through helpers, dispatch and imports. A call whose effect is unknown is rejected as well. | Foreign declarations that promise `no_io` are trusted |
| [E2607](#e2607-a-bug-finder-for-some-permanent-waits) | None. E2607 is a bug finder that reports some certain permanent waits. | It analyzes only very simple straight-line code reachable from `main` |

No check proves deadlock freedom: Tasks can still wait for each other forever
through channels and `await` outside locks.

### E2402: async calls capture only shareable or exclusively lent values

**Guarantee.** Calling an `async fn` captures its arguments into a Task that
may run on another worker. Each captured reference must be `Sync`, or it must
be a fresh, unescaped local `Array`, `Map` or class value whose contents are
`Send`. Such a value is *lent*: after `let t = change(values)`, neither
`values` nor its aliases can be used until `await t` completes, and
`await change(values)` restores access immediately. A loan may start inside a
branch or loop body; it ends at an `await` that runs unconditionally in the
same block, and inside a loop only values created in the current iteration (or
returned by a `@sending` call) can be lent
([`task_loans_in_loops.wi`](example/task_loans_in_loops.wi)). Independent values can be
lent to parallel Tasks, and lending a class field also locks out the containing
object and its aliases. A Task moves between workers only if its parameters,
result and locals live across `await` are `Send`. After a value is sent on a
channel (whose item type must be `Send`, E2403) or placed in a `Mutex`, its
previous aliases cannot be used (E2408).

Parameters, fields, globals and call results are borrowed, so they cannot be
sent or locked away. Ownership crosses a call only through `@sending`
([`sending_results.wi`](example/sending_results.wi)). Here `make` and
`publish` are the declared functions:

- **`@sending fn make() -> T`**: the caller owns the result. That includes a
  payload matched out of an `Ok`, `Some` or `?` result. The compiler checks
  that every `return` in `make` yields a value no parameter, field or escaped
  alias can still reach.
- **`fn publish(@sending x: T)`**: `publish` consumes `x`. It owns `x` and may
  send it. The caller cannot use `x` or its aliases afterwards.

`@sending` is accepted only on non-generic top-level functions and their
parameters. Methods, constructors, interface methods, generic and extern
functions are rejected, because overriding, dispatch or substitution would hide
the contract. A function value such as `let f: fn() -> T = make` does not
keep the contract. Neither does a Task stored before it is awaited: `let t =
make_async(); await t`. Only `await make_async()` yields an owned result.

`Send` and `Sync` are inferred structurally; user code cannot implement them
(E2401):

| Type | `Send` | `Sync` |
| --- | --- | --- |
| Scalars, `String`, fieldless enums, atomics, function pointers | Yes | Yes |
| `Array<T>`, `Map<K, V>` | When the elements are | Never |
| `FrozenArray<T>`, `FrozenMap<K, V>`, `Option`, `Result`, payload enums | When the contents are | When the contents are |
| `Mutex<T>`, `Channel<T>` | When `T` is `Send` | When `T` is `Send` |
| `Task<T>` | When `T` is `Send` | Never |
| `RwLock<T>` | When `T` is `Send` and `Sync` | When `T` is `Send` and `Sync` |
| Classes | When all fields are | When all fields are, and no field is reassigned after construction |
| Interfaces | When the interface extends `Send` or `Sync` | When the interface extends `Sync` |
| Closures | When every capture is an immutable `Send` + `Sync` binding | Same as `Send` |

A mutable collection or a class containing one is not `Sync`, so it can only be
lent:

```willow
import std::collections::Array;

class Inbox { pub items: Array<i64>; }

async fn add(inbox: Inbox, n: i64) {
    inbox.items.push(n);
}

async fn main() {
    let items: Array<i64> = [];
    let inbox = new Inbox(items);
    let task = add(inbox, 1);
    // inbox and items are unavailable until this task completes.
    await task;
    println(items.len());
}
```

To share a mutable value between running Tasks, protect it with `Mutex<T>`:

```willow
import std::collections::Array;

class Inbox { pub items: Array<i64>; }

async fn add(inbox: Mutex<Inbox>, n: i64) {
    lock inbox as data {
        data.items.push(n);
    }
}

async fn main() {
    let inbox = Mutex::new(new Inbox([1, 2]));
    await add(inbox, 3);
}
```

See the runnable [collection](example/owned_task_loans.wi),
[class](example/owned_class_task_loans.wi) and
[ownership transfer](example/ownership_transfer.wi) examples.

**Limits**

| Limit | Effect | Evidence |
| --- | --- | --- |
| Static class fields are not a checked task boundary | Tasks can mutate a non-`Sync` static field concurrently without a diagnostic. Protect such state with `Mutex<T>` or atomics yourself. | The check runs on async call arguments only: `check_async_capture` in [`send_sync.rs`](src/semantic/type_checker/send_sync.rs) |
| Loans awaited only in a nested branch, loans of values from outside the current loop iteration, loans inside `defer` or a closure capture | Rejected, even when safe. Await the task in the block that lent the value, and create loop values inside the loop body. | `task_loans_reject_unsound_paths_with_a_specific_reason` in [`task_loan_flow_tests.rs`](src/semantic/type_checker/task_loan_flow_tests.rs) |
| Lending an async parameter to another Task or storing it in shared state | Rejected | `owned_task_loans_imported_callee_and_escape_contract` in [`capture_checking.rs`](tests/integration/concurrency/capture_checking.rs) |
| Closure values | A closure that mutates a captured binding or captures a non-`Send` + `Sync` value is task-local and cannot be passed to an async call; one capturing only immutable `Send` + `Sync` bindings can | `test_closure_async_e2402_help` in [`capture_checking.rs`](tests/integration/concurrency/capture_checking.rs) |
| Memory inside Rust adapters and native libraries | Not analyzed | [Where the guarantees stop](#where-the-guarantees-stop) |

### E2604: a lock body never waits

**Guarantee.** A `lock` body (allowed only in an `async fn`, E2603) is rejected
if it may wait while the lock is held. The compiler treats these operations as
waiting: `await`, `select`, `Channel` `send`/`recv`/`recv_opt`,
`BlockingCell`/`BlockingRwCell` access, synchronous `std::fs` calls, and calls
to Rust adapters. Calls are followed through synchronous helper chains, virtual
methods and interface dispatch, function values and closures, and imported
modules. A callee without effect evidence counts as waiting. Nested locks are
checked by [E2605](#e2605-nested-locks-follow-one-acyclic-order).

This deliberately invalid program demonstrates the diagnostic:

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

`receive` contains no `await`, and the critical section calls only an ordinary
synchronous helper. But `receive` can wait on `Channel.recv()`. Willow follows
the call and rejects it instead of letting a Task wait while it holds the
mutex. Move the waiting operation outside the critical section:

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

The compiler enforces a **semantic constraint**, not a search for the literal
word `await`. That matters when an AI agent edits a helper several calls away
from the critical section.

**Limits**

| Limit | Effect | Evidence |
| --- | --- | --- |
| Console output | `print` and `println` are allowed in a lock body, although `@effects(no_block)` counts them as blocking. A slow or full stdout can stall the lock holder. | The lock-wait operations listed in `record_builtin_lock_effect` and `record_static_builtin_lock_effect` in [`check.rs`](src/semantic/type_checker/check.rs) |
| Foreign calls declared `nonblocking("<review_fingerprint>")` | Allowed in a lock body. This is a developer assertion, not a proof about the Rust code. | `nonblocking_task_scalar_aggregate_indirect_lock_and_no_wait` in [`rust_scalar_bridge.rs`](tests/rust_scalar_bridge.rs) |
| Calls through function values | Matched by parameter count: a harmless indirect call is rejected if any function value with the same number of parameters may wait | `IndirectLockSession` in [`compiler_db/effects.rs`](src/compiler_db/effects.rs); `indirect_lock_11_arity_mismatch_accepted` in [`indirect_lock.rs`](tests/integration/concurrency/indirect_lock.rs) |
| Long or endless computation | Not rejected: there is no termination effect, so a lock body can loop forever | The effect bits in `RuntimeEffects`, [`willow_abi`](crates/willow_abi/src/lib.rs) |
| Blocking compatibility types used outside a `lock` body | `BlockingCell` and `BlockingRwCell` block their worker and are not part of the lock checks | `rwlock_lower_08_blocking_compatibility_type_is_explicit` in [`lock_restrictions.rs`](tests/integration/concurrency/lock_restrictions.rs) |

The implementation and regression tests are in
[`src/semantic/effects.rs`](src/semantic/effects.rs),
[`src/compiler_db/effects.rs`](src/compiler_db/effects.rs) and
[`lock_restrictions.rs`](tests/integration/concurrency/lock_restrictions.rs).
See the runnable [scheduler-aware lock example](example/scheduler_aware_lock.wi).

### How the effect analysis follows calls

Willow computes effect summaries over the compiler's call graph with a monotone
fixed point. E2604, effect contracts, recoverable-panic handling and
scheduler/preemption analysis share these summaries.

| Situation | What the compiler accounts for |
| --- | --- |
| Direct `await`, `select` and channel operations | Local suspension or wait |
| Synchronous helper chains | Transitive `MAY_BLOCK` / `MAY_SUSPEND` |
| Virtual methods and interface dispatch | Every reachable override, including a waiting override behind a non-waiting base method |
| Function values and closures | Indirect calls, including calls reached through other helpers |
| Unknown or unavailable callees | A conservative bound, never an assumption that the call is safe |

Effects have **witnesses**: compiler evidence that points to the operation
responsible for a property. Calling an `async fn` creates a Task but does not
suspend its caller; only awaiting the Task does. This is another reason a plain
text scan is not enough.

See the [shared effect fixpoint](tests/regressions/shared_effect_fixpoint.wi),
[virtual dispatch](tests/regressions/shared_call_graph.wi) and
[indirect calls](example/lock_function_values.wi) examples.

### Effect contracts: declare the property on any function

**Why this matters for AI-assisted changes.** A contract records a property that
must survive edits. For example, adding `println` to a `@effects(no_io)` function,
or to a helper it calls, makes the program fail compilation. The compiler checks
the edited program against the same contract each time; it does not need to trust
the agent's claim that the change preserves that property.

This guarantee depends on keeping the contract in place: deleting or weakening
the annotation must itself be reviewed. Effect contracts do not prove that the
old and new implementations return the same results or satisfy every business
requirement, and foreign declarations retain the trust boundaries described below.

**Guarantee.** `@effects(...)` applies the solved effects to a declaration. A
function with a contract contains no proven source of the forbidden effect
(E2610) and no call whose effect is unknown (E2612), through helpers, virtual
and interface dispatch, and imported modules. An override cannot weaken an
inherited contract (E2611); an override that declares nothing inherits the base
contract and is checked against it.

| Contract | Forbids | Still allowed |
| --- | --- | --- |
| `no_block` | Blocking waits such as stdout writes or `BlockingCell` reads | `await` and other suspension |
| `no_suspend` | Suspension such as `await`, `select` and waiting channel operations | Blocking waits |
| `no_wait` | Both | Creating a Task without awaiting it |
| `no_panic` | Explicit panic and checked language faults, including bounds, integer arithmetic and `requires`/`ensures` guards | Operations that the effect analysis proves panic-free |
| `no_alloc` | Managed allocation, including literals, objects, closures, Tasks and allocating runtime calls | Scalar computation and access to existing values |
| `no_io` | External resource access: console output, filesystem, network, process arguments and explicit clock reads, including async operations | Computation, allocation and synchronization |

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

Rust adapters and native libraries are opaque. A caller's `no_io` proof fails
with E2612 unless the foreign declaration promises no I/O; extern declarations
accept only `@effects(no_io)`:

```willow
extern rust {
    @effects(no_io) fn calculate(x: i64) -> i64;
}
```

This declaration is a **reviewed assumption**: it requires a current shared
`rust-review.toml` fingerprint. Missing or stale review disables the claim.
Willow does not prove the foreign implementation, its dependencies or destructors;
an incorrect claim invalidates the guarantee. Rust modules use
`#[willow::assume(no_block, no_io)]` (either effect may be used alone).
Successful dependent contracts report `assumption` with its source location,
including in effects queries and `verify-edit`. It does not imply `nonblocking`; the two
modifiers are independent, and `@effects(no_io)` comes first when both are
used. `willow effects` reports the I/O bit (64), and `verify-edit` reports it
as `may-io`.

**Limits**

| Limit | Effect | Evidence |
| --- | --- | --- |
| A function-value parameter | Its target is unknown, so a `no_wait` caller fails with E2612 | `effect_contract_polymorphic_perspectives` in [`effect_contracts.rs`](src/semantic/type_checker/effect_contracts.rs) |
| Foreign declarations | Unknown for every contract; `@effects(no_io)` on the declaration is trusted, not verified | `io_contract_permission_matrix` and `panic_allocation_contract_matrix` in [`effect_contracts.rs`](src/semantic/type_checker/effect_contracts.rs) |
| `no_panic` can use a value proof inside `@pure @verified` | Proven checked arithmetic, division, immutable array bounds and contract evaluation do not violate the declaration. Without a successful proof, potential guards still count, including in profiles that wrap integer arithmetic. | [`proven_no_panic.wi`](example/proven_no_panic.wi) |
| `no_panic` does not cover the process | It does not promise termination and does not exclude hardware faults, out-of-memory aborts, stack exhaustion or failures inside runtime housekeeping | The effect bits in `RuntimeEffects`, [`willow_abi`](crates/willow_abi/src/lib.rs) |
| `no_alloc` covers managed allocation by language operations only | Allocation used only to report a compiler-generated fault (such as a failed bounds check or `unwrap()` of an empty value), GC and scheduler bookkeeping are excluded. It does not promise zero native allocator calls or no GC safepoints. `requires`/`ensures` guards count; loop invariants and variants are verifier-only and do not. | `no_alloc_fault_reporting_and_proof_only_clauses` in [`tests/effect_contracts.rs`](tests/effect_contracts.rs) |
| `no_io` is not an operating-system sandbox | Runtime housekeeping (GC, scheduling, clocks and fault reporting) is outside its scope | `io_contract_std_resource_catalog_is_complete` in [`effect_contracts.rs`](src/semantic/type_checker/effect_contracts.rs) |

`no_panic` uses two kinds of evidence: `effect-analysis` when effect analysis alone
suffices, and `solver-proof` when a successful `@verified` proof is needed. The
`effects` query, `verify-edit` report and `contracts.lock` expose this distinction.
UNKNOWN, timeout and failed obligations never discharge a potential panic.
Proofs are checked once per verified function in a checking pass and shared with
the effect checker; ordinary programs do not start the solver.

Runtime checks are **retained**. A solver-backed `no_panic` guarantee means that
failure paths are unreachable under the function's `requires`. A verified caller
must prove its callees' preconditions at each call. Ordinary callers still count
the runtime guards as possible panic sources, even when the callee is verified.
Explicit `panic`, `unwrap`, mutable `Array` indexing and unsupported operations
remain rejected. Other effect contracts, including `no_alloc`, retain their
existing meaning. Changes to contracts, bodies or immutable data declarations
cause proof rechecking; saved artifacts never grant proof authority.

What `no_alloc` counts in detail: string literals count, because first use
initializes their cached managed object. Materialized ranges and the implicit
interface and scalar-pair boxes used by storage operations count, but a
syntactic `for i in start..end` uses scalar bounds without a Range object.
Iterating a Map allocates an entries snapshot; iterating an existing array or
range does not. Tag-only enums, scalar Option/Result pairs and nullable-pointer
Options do not allocate on construction. Options containing interfaces, and
`Option<T>` of a generic parameter `T`, use or may use a heap box, including
`None` and `?` failure values. The `?` operator counts only allocations required
by its failure representation or implicit error conversion, including the
conversion method's effects. Explicit allocating expressions on a panic branch
still count. Combine `no_alloc` and `no_panic` when both are required.

See [effect contracts](example/effect_contracts.wi),
[I/O contracts](example/io_contracts.wi) and
[contracts across interface dispatch](tests/regressions/effect_contract_dispatch.wi).

### E2607: a bug finder for some permanent waits

**What it reports.** E2607 is an error with a witness chain for three shapes
that wait forever:

- A receive (`recv` or `recv_opt`) on a fresh local channel that nothing in the
  analyzed program sends to or closes.
- A Task that awaits itself, including through a handle delivered on a local
  channel.
- Tasks that form an await cycle reached from `main`'s own wait chain.

For example, `let ch = Channel<i64>::new(); ch.recv();` is rejected. A self-wait
arises when `waiter(ch)` receives a Task handle and awaits it, while its caller
does `let t = waiter(ch); ch.send(t); await t;`. Witnesses identify each Task
creation and await, and the channels transporting handles. Task-handle
transfers can also violate the exclusive-ownership rules (E2408); the compiler
reports both. See the runnable
[valid sender and escape example](tests/regressions/permanent_wait_escapes.wi).

**Absence of E2607 is not evidence of anything.** The analysis covers `main` of
the entry module and the same-module functions it calls, and only when all of
that code uses the small subset below. One construct outside the subset anywhere
in that code leaves the **whole program** unanalyzed.

| Supported | Not supported: the program is left unanalyzed |
| --- | --- |
| `let`, expression statements and `return` | `if`, `match`, loops, `lock`, `defer`, assignment and every other statement |
| Integer, float, bool and string literals, variables and local aliases | Arithmetic, comparisons, closures, `new`, field access, indexing, collections and every other expression |
| `Channel<T>::new()` and `with_capacity(n)` with a positive integer literal `n`; `send`, `recv`, `recv_opt` and `close` | `select`, timeouts, cancellation, other methods (including user methods) and other builtin functions such as `sleep` |
| `await`, `task.result()`, `print` and `println` | Function values, imported and foreign functions, generic templates, functions with `requires`/`ensures` |
| Calls to same-module top-level functions that themselves call no function; such a function may receive a Task handle as an argument or through a channel | A called function that calls another function (nested or recursive Task factories) |

Inside that subset the analysis is also deliberately cautious, so it misses
some real permanent waits:

| Rule | Evidence |
| --- | --- |
| Any send or close of the channel anywhere in the analyzed code, even after the wait, suppresses the no-sender error | `sender_in_other_task` and `close_in_other_task` in [`permanent_wait/tests.rs`](src/semantic/permanent_wait/tests.rs); the `sends == 0` test in `diagnose`, [`permanent_wait.rs`](src/semantic/permanent_wait.rs) |
| A receive from a channel that has a send is never reported, even when there are more receives than sends. A Task handle passed on a channel is followed only with exactly one send and one receive. | `ambiguous_task_delivery`; `diagnose` and `target` in [`permanent_wait.rs`](src/semantic/permanent_wait.rs) |
| A bounded send, or a send on a possibly closed channel, stops the wait chain because it can block or panic first | `bounded_send_before_cycle`, `closed_send_prevents_later_wait`, `peer_close_may_prevent_later_wait` |
| A channel capacity that is not a positive integer literal stops the proof | `zero_capacity_panics_before_receive`, `unknown_capacity_is_not_proven` |
| Only the first unresolved await of each Task is followed, because a later await may be unreachable after a panic or cancellation | `diagnose` in [`permanent_wait.rs`](src/semantic/permanent_wait.rs) |
| Uncalled helpers, detached Tasks and an imported module's `main` are not roots | `unused_helper_is_unknown`, `detached_local_blocked_task`, `imported_main_is_not_a_process_root` |
| The check does not run when the program has other type or flow errors (E2408 excepted) | The entry condition before `permanent_wait::check` in [`check.rs`](src/semantic/type_checker/check.rs); `type_errors_do_not_trigger_wait_cascades` |
| An unsupported construct stops the proof | `conditional_receive`, `loop_is_unknown`, `assignment_is_unknown`, `select_timeout`, `function_value`, `escaped_to_collection`, and the syntax filter in [`permanent_wait.rs`](src/semantic/permanent_wait.rs) |

The pass runs in expected linear time and memory in the source size, the
instantiated summary operations, Tasks, channels and recorded wait edges. Each
summary is built once and keeps only the first two sends per channel, so
repeated calls and repeated sends do not multiply. Deterministic tests count
syntax visits, operations, Tasks, channels, waits and graph steps for growing
inputs. The pass adds no generated code or runtime bookkeeping.

## Contracts: from run-time checks to proofs

Effect contracts describe waiting and external access. Value-level properties use separate annotations, from run-time checks up to static proofs.

### `@verified`: prove a contract with Z3

```willow
@pure
@verified
fn larger(a: i64, b: i64) -> i64
    ensures result >= a
    ensures result >= b
    ensures result == a || result == b
{
    if a >= b { return a; }
    return b;
}

fn main() {
    println(larger(4, 9)); // 9
}
```

These `ensures` clauses hold for every `i64` input, not only the inputs a test tries. A contract that cannot be proved fails the build, and a false one comes with a counterexample:

```willow
@pure
@verified
fn non_negative(x: i64) -> i64
    ensures result >= 0
{
    return x;
}

fn main() {
    println(non_negative(5));
}
```

```text
error[E2630]: @verified `non_negative`: COUNTEREXAMPLE: ensures truth: sat; candidate inputs ((a0_0 (- 1)))
```

Failures are reported as E2630 (counterexample), E2631 (unsupported construct) and E2632 (solver unknown or failure).

**Scope:** `@verified` requires `@pure` and proves checked i64/bool contracts, read-only `FrozenArray<T>` lengths and elements, closed immutable records, and enum payloads. Arrays use SMT array theory with explicit lengths and bounds; records and enums use algebraic datatypes. Supported data fields recursively contain these types. Reassigned fields, mutable `Array`/Map, open or recursive data types, unsupported operations, and unavailable declarations fail closed with E2631. Object identity equality and allocation remain outside the proof subset; inspect record fields or match enum variants instead. The proof trusts Z3 and the Willow compiler. See [`example/verified_data.wi`](example/verified_data.wi).

**Setup:** install Z3 4.15.4 (`python -m pip install z3-solver==4.15.4.0`) and keep `z3` on PATH, or set `WILLOW_Z3` to its executable. Verification JSON and replayable SMT files are saved under `target/willow-verification` (override with `WILLOW_VERIFICATION_DIR`). See [`example/static_verification.wi`](example/static_verification.wi).

**Quantified contracts:** `forall(lo, hi, |i:i64| predicate)` and `exists(lo, hi, |i:i64| predicate)` quantify over the half-open i64 range `[lo, hi)`. The names are reserved. Empty ranges yield true and false respectively. Bounds execute once; runtime evaluation short-circuits. Verification conservatively requires the predicate to be safe at every bounded index, including indices runtime short-circuiting might skip. Nested binders are lexical. The trigger policy uses the pinned solver's automatic pattern inference and model-based quantifier instantiation, with no user-supplied triggers or unchecked axioms. Timeout, resource exhaustion, incomplete theory and solver `unknown` fail with E2632; they never produce a proof.

**Loops:** range `for` and `FrozenArray` iteration infer their bounds invariant and termination variant; write only your application invariants (for example, `for i in 0..n invariant total == i { total = total + 1; }`). User invariants are conjoined with the inferred bounds, and an explicit `variant` overrides the default. Range bounds are evaluated once: the inferred invariant is `start <= i && i <= max(start, end)`, so empty and reversed ranges are valid. The default rank is `end - i`, checked as i64 on active iterations and backedges; a range whose rank can overflow is rejected unless a valid explicit variant is supplied. For `FrozenArray`, the captured bounds are zero and the captured array length, and annotations may use the proof-only zero-based `iteration` index (including the exhausted head at `len()`); use outer bindings rather than the element binder in annotations. `while` still requires boolean `invariant` clauses and an i64 `variant`. The verifier checks entry, preservation, and a nonnegative, strictly decreasing rank on backedges, including `continue` and the automatic `for` increment. `break` preserves its actual exit state and does not require a decrease. All loop annotations are proof-only and add no runtime code or effects. Saved verification JSON includes `inferred_loop_annotations`, with source locations, counter names, bound formulas, and a null inferred variant when an explicit one is used. See [`example/verified_loops.wi`](example/verified_loops.wi) and [`example/verified_data.wi`](example/verified_data.wi).

`verify-edit` compares these contracts over arbitrary valid data, and `contracts.lock` includes immutable data declaration identities. Changes to data definitions are conservatively reported as not preserved, rather than reusing an older proof.

### `@pure` and run-time contracts

- **`@pure`** requires a function to have no observable interaction outside its
  arguments and result. Immutable record/FrozenArray reads are permitted. Output, field and index writes, mutable heap reads,
  nondeterminism and unknown callees are rejected with E2620. Undeclared callees
  are analyzed, so they do not need their own `@pure`.
  See [pure functions](example/pure_functions.wi).
- **`requires` / `ensures`** clauses are checked at run time in both debug and
  release builds. A violation is a recoverable panic such as
  `contract violation: ensures`. See [runtime contracts](tests/regressions/runtime_contracts.wi).

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
- **No wait inside:** [E2604](#e2604-a-lock-body-never-waits) rejects suspension and blocking waits within the critical section.
- **Checked nesting:** [E2605](#e2605-nested-locks-follow-one-acyclic-order) rejects cyclic acquisition order and reacquisition.
- **Read/write modes:** `lock read` and `lock write` provide lexical access to `RwLock<T>`.

These features belong together: the parser, effect checker, async lowering, cleanup logic, and runtime cooperate to enforce a restricted locking discipline. Willow uses a GC rather than a borrow checker or general destructor-based ownership system; the compiler-owned lock scope is part of its design.

The guarantees cover Willow `lock` statements only. `BlockingCell` and
`BlockingRwCell` are blocking compatibility types with their own semantics, and
locks inside native code are outside the analysis.

See [Mutex example](example/scheduler_aware_lock.wi), [RwLock example](example/scheduler_aware_rwlock.wi), and [concurrency regression tests](tests/integration/concurrency/).

### E2605: nested locks follow one acyclic order

**Guarantee.** Across the whole project, nested `lock` statements follow one
acyclic acquisition order, and no lock is acquired again while it is held
(including two read acquisitions of one `RwLock`). E2605 reports the acquisition
sites of a cycle. Together with E2604, this rules out a cycle of Tasks in which
each holds a Willow lock while waiting to acquire another.

Locks are told apart by where they were created. A lock of unknown origin, such
as a parameter or the value held by another lock, is identified by its value
type instead, and its order applies to every known lock of that type. This makes
parent and child locks work without group syntax, because the child is known
only after the parent is held:

```willow
async fn bump(parent: Mutex<Mutex<i64>>) {
    lock parent as child {
        lock child as mut value { value = value + 1; }
    }
}
```

Taking them in the opposite order elsewhere is reported as a cycle. Awaiting an
async helper inside a lock is still E2604: compute awaited values before
entering the critical section, or acquire the required locks together. Merely
creating an async Task does not run its body on the caller's stack.

**Limits**

| Limit | Effect | Evidence |
| --- | --- | --- |
| Two locks the compiler cannot tell apart may be the same lock | Nesting them is rejected: two parameters of one type (such as two `Mutex<i64>`), two locks from one factory function, or collection elements. Use a [group `lock`](#acquiring-multiple-locks). | `ordered_locks_same_type_parameters_need_a_group`, `ordered_locks_factory_locks_are_told_to_use_a_group`, `ordered_locks_collection_elements_of_one_type_are_rejected` in [`ordered_locks.rs`](tests/integration/concurrency/ordered_locks.rs) |
| A recursive type such as `Mutex<Node>` reached through a `Node` field | Holding one node while locking the next (hand-over-hand traversal) is rejected. A loop that locks one node at a time works, but another Task can change the list between two steps. | `ordered_locks_recursive_lock_type_is_rejected`; [`parent_child_locks.wi`](example/parent_child_locks.wi) |
| A generic `Mutex<T>` has no type order | Nesting inside a generic function or class stays conservative; use group syntax there | `ordered_locks_type_parameter_has_no_type_class`, `ordered_locks_known_locks_in_generic_bodies_stay_conservative` |
| Order between Tasks waiting on channels or `await` | Not covered: a Task can still hold no lock and wait forever for another Task | [E2607](#e2607-a-bug-finder-for-some-permanent-waits) covers only some of these waits |

See [ordered locks](example/ordered_locks.wi) and [parent and child locks](example/parent_child_locks.wi) for runnable examples.

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

**Guarantee.** A group never holds a member while it waits, so it cannot
deadlock with nested locks taken in another order. A contended group releases
everything it holds before parking in the busy lock's FIFO queue, and normally
keeps that lock when its turn comes while it tries the other members. Retries
sleep rather than spin.

**Limits**

| Limit | Effect | Evidence |
| --- | --- | --- |
| Group acquisition is not fair and has no retry limit | If other Tasks keep different members busy in turn, one group can wait indefinitely. Avoid holding a member of a heavily contended group for long. | Module documentation of [`lock_group.rs`](crates/willow_runtime/src/lock_group.rs) |
| No rollback | Scope exits, panic and cancellation release acquired locks in reverse order; writes already made are committed. This is mutual exclusion, not a transaction. | `ordered_locks_recovered_panic_commits_and_releases_all`, `ordered_locks_held_cancellation_commits_before_outer_cleanup` in [`ordered_locks.rs`](tests/integration/concurrency/ordered_locks.rs) |

## Semantic tooling for people and AI agents

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

See the [AI toolchain protocol](protocol/ai_toolchain/README.md) and [example effect explanations](tests/regressions/effect_explanations.wi).

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
| Accepted lexical lock body | No operation the [E2604 rules](#e2604-a-lock-body-never-waits) classify as waiting, directly or through calls | Console output and `nonblocking` foreign calls are allowed; not a proof of global deadlock freedom or native-code behavior |
| Successful exact rename | Resolved occurrences of the selected symbol are edited and the candidate passes validation | Limited to the loaded project and supported symbols; not a proof of application correctness |
| Effect `MAY` (`may` in JSON) | A source or known-callee witness supports the capability | It may not occur on every runtime path; uncertainty can coexist in `unknown_reasons` |
| Effect `UNKNOWN` (`unknown` in JSON) | A conservative capability is retained without a supporting witness | Indirect/unresolved calls, missing bodies, incomplete flow, or external interop require assumptions; this is not evidence that the effect occurs or is absent |
| Effect `NONE` (`none` in JSON) | Neither a witness nor an uncertainty reason remains for that capability in the modeled analysis | Scoped to available compiler knowledge, not a guarantee about unmodeled behavior |

The `willow.effects.v3` output keeps `witness` and `unknown_reasons` separate.
Missing evidence is not purity. Safety checks retain conservative capability
bounds even when the explanatory query reports `UNKNOWN`; a query does not
waive E2604. See the [effect-state example](example/effect_states.wi).

### Compare scalar contracts across an edit

`willow verify-edit --base BASE --schema v4 --protected-base FULL_COMMIT --format json`
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

### What needs language semantics, and what LSP/MCP can provide

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
- [Pony](https://www.ponylang.io/) uses actors and reference capabilities to prevent data races and avoids lock-based deadlocks by not exposing locks. Willow keeps familiar async tasks, locks, and channels, while rejecting selected cases of unsafe task sharing, waiting inside lexical locks, and cyclic or uncertain lock acquisition. Willow's guarantees are narrower; its tradeoff is a Go/Kotlin-like programming model rather than Pony's actor/capability model.

## Measurements

### Measured AI-agent efficiency (a secondary benefit)

The semantic CLI also reduced work in one small controlled benchmark. Five tasks were run three times per mode on an approximately 1,300-line, eight-module Willow chess engine, with Claude Code (Sonnet 5.5), on 2026-10-22.

| Total across 15 runs per mode | Semantic CLI | Text tools only | Change |
| --- | ---: | ---: | ---: |
| Input tokens | 252,229 | 458,222 | -45% |
| Output tokens | 4,580 | 12,315 | -63% |
| Cost (USD) | $0.225 | $0.508 | -56% |
| Tool calls | 15 | 45 | -67% |

All answers were correct in both modes. One task (interface-signature impact) used *more* input tokens with the semantic CLI. This is evidence for the tested tasks only, **not** a demonstrated general improvement on large repositories. Token savings are useful, but they are not Willow's primary safety argument.

### Runtime performance: strengths and gaps

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

## Current boundaries

Willow is an experimental language and compiler, **not yet suitable for production deployment**. Each check guarantees only what its section states; the limits tables list what falls outside it. In particular:

- No check proves deadlock freedom or the absence of all data races. Tasks can wait for each other forever through channels and `await` outside locks, and E2607 reports only some such waits; its absence is **not** evidence of anything.
- E2402 checks async-call arguments and movable Task frames, not every shared-memory access: static fields are shared without a check.
- E2604 does not count console output as a wait and does not bound computation time inside a lock body.
- Conservative analysis of dynamic and indirect calls may reject code that is actually safe. Unavailable evidence is never treated as proof of purity.
- Rust adapters, native libraries and the declarations that describe them (`nonblocking`, `@effects(no_io)`, `willow::assume`) are assumptions bound to one current review record, not proofs of foreign behavior. Compiler and runtime defects are outside every static guarantee.
- The runtime benchmarks and agent-token experiment measure limited, published workloads.

**The goal:** make semantic safety properties part of the compiler's ordinary contract, and make their evidence accessible to both humans and coding agents.

MIT licensed. See [LICENSE](LICENSE).
