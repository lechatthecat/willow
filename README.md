# Willow

> A statically typed, garbage-collected native programming language with its own runtime, package manager, incremental compiler infrastructure, and AI-oriented semantic tooling.

![Status](https://img.shields.io/badge/status-experimental-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

Willow is an experimental programming language that compiles to native code through [Cranelift](https://cranelift.dev/).

It combines class-based object-oriented programming with algebraic enums, pattern matching, `Option`, `Result`, closures, and stackless `async` / `await`.

Willow also explores a second question:

> What should a programming language toolchain look like when AI coding agents are first-class users of the compiler?

Instead of requiring an agent to reconstruct program structure from text search, Willow can expose compiler-resolved symbols, references, call relationships, snapshots, dependency impact, structured edits, and machine-readable diagnostics.

Willow is under active development and is **not production-ready**.

---

## What makes Willow different?

Willow is more than a parser and code generator. The project currently includes:

- **Native compilation** through Cranelift
- **Generational moving garbage collection**
- **A stackless async runtime** with tasks, cancellation, channels, `select`, locks, and work stealing
- **Class-based OOP** with inheritance, interfaces, virtual dispatch, static members, and free functions
- **Algebraic enums and pattern matching**
- **`Option<T>` and `Result<T, E>`**, including `?` propagation
- **Closures and first-class functions**
- **Multi-module and multi-package compilation**
- **Git and local-path dependencies**, lockfiles, caching, offline and frozen builds
- **Package-aware semantic identities**
- **Compiler snapshots and semantic queries**
- **Reference, caller, impact, effect, and risk analysis**
- **Structured source edits with validation and recovery**
- **A warm analysis daemon**
- **Typed-body reuse across compiler revisions**
- **A versioned NDJSON protocol for tools and AI agents**
- **Automatic `AGENTS.md` and `CLAUDE.md` generation**

The language, compiler, runtime, package system, and analysis infrastructure are developed together in one repository.

---

## A taste of Willow

```rust
enum Shape {
    Circle(f64),
    Rectangle(f64, f64),
    Dot,
}

fn area(shape: Shape) -> f64 {
    return match shape {
        Shape::Circle(r) => r * r * 3.14159,
        Shape::Rectangle(w, h) => w * h,
        Shape::Dot => 0.0,
    };
}

async fn compute() -> f64 {
    await sleep(1);
    return area(Shape::Circle(5.0));
}

async fn main() {
    let task = compute();
    println(await task);
}
```

Classes and interfaces are built into the language as well:

```rust
interface Animal {
    fn speak(self) -> String;
}

class Dog implements Animal {
    pub fn speak(self) -> String {
        return "woof";
    }
}

fn describe(animal: Animal) {
    println(animal.speak());
}

fn main() {
    describe(new Dog());
}
```

---

## Quick start

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
└── src/
    └── main.wi
```

If Claude Code or Codex is detected, Willow can also offer to generate agent instructions:

```text
Claude Code detected.
Create CLAUDE.md? [Y/n]

Codex detected.
Create AGENTS.md? [Y/n]
```

The generated instructions teach the agent how to use the Willow compiler protocol, snapshots, semantic queries, impact analysis, and structured edits instead of relying only on textual search.

For non-interactive use:

```bash
willow init hello --ai none
willow init hello --ai codex
willow init hello --ai claude
willow init hello --ai codex,claude
```

---

## Build and run

Run the current project:

```bash
willow run
```

Check it without producing a native executable:

```bash
willow check
```

Build it:

```bash
willow build
```

You can also compile a single source file directly:

```bash
willow example/hello_world.wi -o hello_world --release
./hello_world
```

Or run it without keeping the output binary:

```bash
willow run example/hello_world.wi --release
```

---

# Language

## Types and inference

Willow includes scalar types such as:

```text
i64
f64
bool
```

and GC-managed values such as:

```text
String
arrays
maps
class instances
interfaces
enums
closures
tasks
```

Type annotations can often be inferred:

```rust
let x: i64 = 10;
let y = 10;
```

Variables are immutable by default:

```rust
let x = 10;
// x = 20; // compile error

let mut y = 10;
y = 20;
```

---

## Classes and inheritance

```rust
class User {
    name: String;
    pub age: i64;

    pub init(self, name: String, age: i64) {
        self.name = name;
        self.age = age;
    }

    pub fn greet(self) -> String {
        return self.name;
    }
}

fn main() {
    let user = new User("Alice", 30);
    println(user.greet());
}
```

Willow supports:

- classes
- inheritance
- interfaces
- virtual dispatch
- constructors
- static methods
- static properties
- private-by-default members
- free functions outside classes

---

## Enums and pattern matching

Enums may carry payloads:

```rust
enum ResultCode {
    Ok(i64),
    Failed(String),
}

fn print_result(result: ResultCode) {
    match result {
        ResultCode::Ok(value) => {
            println(value);
        }
        ResultCode::Failed(message) => {
            println(message);
        }
    }
}
```

`Option<T>` and `Result<T, E>` use the same enum machinery.

Fallible results can propagate with `?`.

---

## Closures

Functions are first-class values, and closures may capture their environment.

```rust
fn main() {
    let base = 10;
    let add = |x: i64| x + base;

    println(add(5));
}
```

Nested closure bodies receive compiler identities of their own and participate in type checking and incremental analysis.

---

## Async / await

Willow uses stackless async frames managed by its own runtime.

```rust
async fn work(x: i64) -> i64 {
    await sleep(1);
    return x * 2;
}

async fn main() {
    let a = work(10);
    let b = work(20);

    println(await a);
    println(await b);
}
```

Calling an async function creates a task. Waiting is explicit.

Cancellation may either propagate or be observed:

| Operation | Result | Cancelled task |
| --- | --- | --- |
| `await task` | `T` | panics |
| `await task.result()` | `Result<T, Cancelled>` | `Err(Cancelled)` |

The runtime also includes channels, `select`, scheduler-aware synchronization, and cancellation-aware tasks.

---

# Projects and packages

A Willow project is described by `project.toml`.

```toml
[willow]
manifest-version = 1

[project]
name = "hello"
version = "0.1.0"
entry = "src/main.wi"

[dependencies]
```

Willow projects may depend on packages from:

- Git repositories
- local filesystem paths

There is currently no requirement for a central Willow package registry.

---

## Add a dependency

```bash
willow add greeting \
    --git https://github.com/example/willow-greeting.git \
    --version "^0.1"
```

Or use a local package:

```bash
willow add greeting --path ../willow-greeting
```

Import through the consumer-local alias:

```rust
import greeting::message;

fn main() {
    println(message::hello());
}
```

The alias is not the package's semantic identity.

---

## Dependency commands

```bash
willow add ...
willow remove ...
willow update ...
willow deps tree
willow deps why <package>
willow fetch
willow metadata
willow package verify
```

Dependency resolution is recorded in `project.lock`.

Application projects should normally commit this file.

Build-related commands support:

```text
--locked
--offline
--frozen
```

where:

- `--locked` rejects lockfile changes
- `--offline` forbids network access
- `--frozen` enables both

Fetched package sources are cached under `WILLOW_HOME`, or the default Willow user cache.

---

# AI-oriented toolchain

Willow exposes compiler information through a versioned machine-readable protocol.

The goal is not to replace ordinary editors or AI agents.

The goal is to let those tools ask the compiler questions that the compiler already knows how to answer.

Instead of:

```text
grep
read files
guess symbol identity
guess callers
edit text
build
retry
```

an agent can use:

```text
compiler snapshot
semantic query
impact analysis
structured edit
compiler validation
```

---

## Machine-readable diagnostics

Use NDJSON output:

```bash
willow check . --format ndjson --protocol-version 1
```

or:

```bash
willow build . --format ndjson --protocol-version 1
```

The protocol provides structured lifecycle events and compiler diagnostics.

Human-readable stderr is not part of the machine protocol.

See [`protocol/ai_toolchain/`](protocol/ai_toolchain/) for the schemas and protocol documentation.

---

## Snapshots

Create a compiler snapshot:

```bash
willow snapshot save . \
    --output snapshot.json \
    --format ndjson \
    --protocol-version 1
```

A snapshot represents compiler-resolved information for one analysis revision.

Snapshots include semantic information such as functions, symbols, package/module identities, call relationships, effects, and source locations.

Source changes make an older snapshot stale for further semantic analysis.

---

## Semantic queries

Willow can answer compiler-resolved queries such as:

- symbols
- symbol at a source position
- symbol information
- references
- checked types
- effects
- package-related impact information

Queries are supplied as JSON requests.

```bash
willow query . \
    --requests queries.json \
    --format ndjson \
    --protocol-version 1
```

For example:

```json
[
  {
    "kind": "symbols",
    "revision": "<revision>"
  }
]
```

Reference queries use compiler declaration identities rather than text matching.

This allows local variables, parameters, fields, types, enum variants, imports, methods, constructors, shadowed bindings, and package-qualified declarations to be distinguished correctly.

---

## Impact analysis

Call relationships can be traversed from a compiler-resolved function identity.

```bash
willow impact . \
    --function FUNCTION_ID \
    --revision REVISION \
    --direction callers \
    --format ndjson \
    --protocol-version 1
```

Impact information includes unresolved or incomplete evidence instead of silently presenting uncertainty as certainty.

Cross-package calls retain package-aware identities.

---

## Package-aware analysis

Package commands support machine-readable output:

```bash
willow update --dry-run --format json
```

That result can be fed into semantic analysis to estimate which loaded modules, symbols, and explicitly supplied tests may be affected by a dependency change.

This connects package resolution with compiler semantics instead of treating dependency updates as unrelated text changes.

---

## Structured edits

Willow provides a transaction-oriented editing interface intended for tooling and AI agents.

A typical workflow is:

```text
prepare
   ↓
preview
   ↓
validate
   ↓
apply
```

If an interrupted write must be restored:

```text
recover
```

Example:

```bash
willow edit prepare \
    --root . \
    --entry src/main.wi \
    --project \
    --requests edits.json
```

Prepared edits do not immediately modify the workspace.

Validation checks the isolated candidate before application.

The edit subsystem also keeps recovery information so interrupted or failed operations do not have to be repaired by guessing what was partially written.

See [`protocol/ai_toolchain/STRUCTURED_EDITS.md`](protocol/ai_toolchain/STRUCTURED_EDITS.md).

---

## Warm analysis

For repeated semantic queries:

```bash
willow daemon .
```

The daemon keeps one accepted analysis revision alive and accepts JSON requests over stdin.

After source changes, a successful refresh installs a new revision.

The current implementation can reuse:

- typed-body artifacts
- symbol-query results
- reference-query results
- effect-query results

across revisions when their inputs remain valid.

---

## Incremental frontend reuse

Willow's `CompilerDb` supports revision-aware analysis.

When a source module changes, Willow computes a reverse dependency closure.

Modules outside that invalidated region can reuse typed-body artifacts from the previous accepted revision.

Conceptually:

```text
previous revision
      │
      ├── unchanged module ──→ reuse typed bodies
      │
      └── changed module
              ↓
       invalidate consumers
              ↓
         type-check again
```

Changes to bodies, public signatures, types, default methods, or effects invalidate affected modules and their consumers.

Resolver-topology or compiler-configuration changes start a cold generation.

The current invalidation granularity is primarily module-based; Willow does not claim fully fine-grained incremental type checking at every AST node.

---

## Agent bootstrap

`willow init` can generate instructions for supported coding agents.

```text
Claude Code detected.
Create CLAUDE.md? [Y/n]

Codex detected.
Create AGENTS.md? [Y/n]
```

The generated managed section teaches agents to:

1. use machine-readable compiler diagnostics
2. take a snapshot before semantic or cross-file work
3. use compiler references and impact data instead of inferring them from text alone
4. treat old snapshots as stale after source or dependency changes
5. use Willow package commands rather than manually changing `project.lock`
6. validate changes before finishing

Existing user-written content outside the Willow-managed section is preserved.

To update generated instructions later:

```bash
willow agent sync
```

To inspect what the current Willow binary would generate:

```bash
willow agent instructions codex
willow agent instructions claude
```

---

# Compiler architecture

The compiler pipeline is roughly:

```text
source
  ↓
lexer / parser
  ↓
AST
  ↓
module + package resolution
  ↓
desugaring
  ↓
type checking / semantic analysis
  ↓
typed bodies / CompilerDb
  ↓
LIR
  ↓
Cranelift IR
  ↓
native object
  ↓
native executable
```

Semantic identities are not based only on source spelling.

The compiler uses identities for entities such as:

```text
PackageIdentity
Module identity
TypeId
FunctionId
BodyId
ExprId
PatternId
```

This identity infrastructure is shared by compilation, incremental analysis, package-aware queries, and AI tooling.

---

# Runtime architecture

Willow ships its own runtime rather than delegating concurrency and memory management to a host-language runtime.

Current runtime work includes:

- generational moving garbage collection
- scalable object reference metadata
- stackless async frames
- frame layout based on LIR liveness
- multi-worker task scheduling
- work stealing
- channels
- `select`
- scheduler-aware synchronization
- cancellation
- bounded blocking work
- GC telemetry
- stress-testing modes

The compiler and runtime are designed together, allowing features such as async frame tracing, scheduler integration, and GC metadata generation to share compiler knowledge.

---

# Examples

The [`example/`](example/) directory contains runnable Willow programs.

Useful starting points include:

- [`example/hello_world.wi`](example/hello_world.wi) — smallest program
- [`example/enum_match.wi`](example/enum_match.wi) — enums and pattern matching
- [`example/async_concurrent.wi`](example/async_concurrent.wi) — concurrent async tasks
- [`example/game_of_life.wi`](example/game_of_life.wi) — larger runnable example
- [`example/gc_scalable_bitmap.wi`](example/gc_scalable_bitmap.wi) — GC tracing with larger reference maps
- [`example/package_paths/`](example/package_paths/) — package identity and path dependencies

For example:

```bash
willow run example/game_of_life.wi --release -- \
    2,1 3,2 1,3 2,3 3,3
```

---

# Project status

Willow is **experimental and under active development**.

It is currently better viewed as a serious language/toolchain experiment than as a production replacement for Rust, Go, Java, or other established ecosystems.

Some important limitations:

- the standard library is still small
- language syntax and runtime APIs may change
- the public toolchain protocol is versioned, but higher-level analysis features are still evolving
- the incremental frontend currently invalidates primarily at module granularity
- the debugger is not yet a full interactive debugger
- the ecosystem is very small
- production hardening and real-world compatibility testing are still ongoing
- building Willow itself from source currently requires Rust

The project intentionally prioritizes compiler/runtime architecture, semantic tooling, and correctness before declaring the language surface stable.

---

# Repository layout

```text
src/
├── ai/             AI snapshots, queries, impact and editing support
├── backend/        native code generation
├── compiler_db/    semantic queries and incremental compiler state
├── module/         module loading and compilation graph
├── package/        dependency resolution, cache and lockfiles
├── parser/         syntax and AST
├── semantic/       type checking and semantic analysis
└── ...

crates/
├── willow_abi/
├── willow_continuations/
└── willow_runtime/

protocol/
└── ai_toolchain/

example/
tests/
benches/
```

---

# Development

Run the test suite:

```bash
cargo test
```

Formatting:

```bash
cargo fmt --check
```

Clippy:

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

During development:

```bash
cargo run -- run example/hello_world.wi
```

---

# License

Willow is available under the [MIT License](LICENSE).
