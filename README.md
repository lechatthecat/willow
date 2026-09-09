# Willow

> A statically typed, garbage-collected native programming language with class-based OOP, algebraic enums, pattern matching, and stackless async.

![Status](https://img.shields.io/badge/status-experimental-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

Willow combines familiar object-oriented programming with modern typed-language features such as payload-carrying enums, `match`, `Option`, `Result`, closures, and `async` / `await`.

Willow compiles to native binaries through [Cranelift](https://cranelift.dev/) and includes its own garbage collector and task runtime.

## Why Willow?

Willow is an experiment in building a native language that keeps memory management automatic without giving up expressive types or structured concurrency.

- **Native code** — compiles to native binaries through Cranelift.
- **Garbage collected** — objects, strings, collections, closures, and async frames are GC-managed.
- **OOP without forcing everything into classes** — classes, inheritance, interfaces, static members, and free functions coexist.
- **Algebraic enums and pattern matching** — enum variants can carry values and can be destructured with `match`.
- **`Option` and `Result`** — nullable/fallible values are represented explicitly; `?` propagation is supported.
- **First-class functions and closures** — functions can be passed around, and closures can capture their environment.
- **Stackless async runtime** — `async` / `await`, task cancellation, channels, `select`, locks, and scheduler-aware synchronization.
- **Multi-module compilation** — imported modules are resolved with canonical semantic identities and dependency-ordered initialization.

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

Classes and interfaces are also first-class language features:

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

## Quick start

A Rust toolchain is currently required to build the Willow compiler.

```bash
git clone https://github.com/lechatthecat/willow.git
cd willow
cargo build --release
```

Run a Willow program directly:

```bash
./target/release/willowc run example/hello_world.wi --release
```

Or compile it to a native executable:

```bash
./target/release/willowc example/hello_world.wi -o hello_world --release
./hello_world
```

During compiler development you can also run through Cargo:

```bash
cargo run --release -- run example/hello_world.wi --release
```

## Language highlights

### Types and inference

Willow currently includes scalar types such as `i64`, `f64`, and `bool`, plus GC-managed types such as `String`, arrays, maps, class instances, closures, interfaces, enums, and tasks.

Type annotations are optional when the type can be inferred:

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

### Classes and inheritance

Members are private by default; use `pub` to expose them.

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

Willow also supports interfaces, inheritance, virtual dispatch, static methods, and static properties.

### Enums and `match`

Enums can be simple tags or carry payloads:

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

`Option<T>` and `Result<T, E>` use the same enum machinery and integrate with pattern matching and `?` propagation.

### Async / await

Calling an async function starts a task. Waiting is explicit with `await`:

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

Task cancellation can either propagate as a panic or be observed as a value:

| Operation | Result | Cancelled task |
| --- | --- | --- |
| `await task` | `T` | panics |
| `await task.result()` | `Result<T, Cancelled>` | `Err(Cancelled)` |

## Compiler architecture

Willow has an explicit multi-stage compiler pipeline:

```text
source
  ↓
AST
  ↓
type checking / typed HIR
  ↓
LIR (control-flow graph + async liveness)
  ↓
Cranelift IR
  ↓
native binary
```

All checked source bodies, including static-property initializers, are emitted through the LIR path.

The compiler uses stable semantic identities such as `ExprId`, `PatternId`, `TypeId`, `FunctionId`, and `ModuleId` instead of relying on source spans or linker spellings as identity.

## Runtime architecture

Willow's runtime is implemented alongside the compiler rather than delegated to a host-language async runtime.

Highlights include:

- generational moving garbage collection
- scalable GC reference bitmaps for wide objects and async frames
- stackless async frames whose layout is derived from LIR liveness
- multi-worker task scheduling with work stealing
- channels and `select`
- scheduler-aware locks and synchronization
- cancellation-aware tasks
- GC telemetry and stress modes used by the test suite

## Examples

The [`example/`](example/) directory contains runnable programs covering the language and runtime.

A few useful starting points:

- [`example/hello_world.wi`](example/hello_world.wi) — smallest program
- [`example/enum_match.wi`](example/enum_match.wi) — enums and pattern matching
- [`example/async_concurrent.wi`](example/async_concurrent.wi) — concurrent async tasks
- [`example/game_of_life.wi`](example/game_of_life.wi) — larger runnable example
- [`example/gc_scalable_bitmap.wi`](example/gc_scalable_bitmap.wi) — GC tracing beyond the inline reference mask

Run the Game of Life example with an initial pattern:

```bash
./target/release/willowc run example/game_of_life.wi --release -- \
  2,1 3,2 1,3 2,3 3,3
```

## Project status

Willow is **experimental and under active development**. It is not yet intended as a production replacement for established languages.

Current limitations include:

- the standard library is still small: prelude plus `std::collections`, `std::option`, `std::result`, `std::io`, `std::env`, and `std::fs`
- synchronous and `*_async` filesystem forms are currently backed by a bounded blocking pool
- runs use at least five active workers; `WILLOW_WORKERS=N` can request more, while values below five are clamped to five
- syntax, runtime APIs, and compiler internals may still change
- the compiler currently requires a Rust toolchain to build from source

The project intentionally focuses on making the compiler and runtime architecture solid before treating the language surface as stable.

## Feedback

Willow is a personal language project, and feedback from compiler/runtime developers or people experimenting with new languages is welcome. Bug reports, implementation discussion, and small reproducible examples are especially useful.

## License

Willow is available under the [MIT License](LICENSE).
