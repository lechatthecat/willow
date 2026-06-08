# Willow

Rust-like language with GC.  

- Static typing with type inference
- Immutable variables by default
- Garbage-collected runtime
- Class-based OOP with private-by-default members 
- Native binary output via Cranelift

**Not production ready yet**

## Current limitations

- The scheduler is currently cooperative.
- Active worker count is currently clamped to 1; M:N scheduling is planned.
- `join()` currently drives the scheduler broadly, not only the target task.
- Standard library is still small.
- Syntax and runtime APIs may still change.

## Install

Requires a Rust toolchain.

```bash
git clone <repo>
cd willow
cargo build --release
```

The compiler binary is at `target/release/willowc`.

## Usage
Please see [examples](https://github.com/lechatthecat/willow/tree/main/example).
```bash
# Compile a source file
./target/release/willowc example/fib.wi -o fib

# Or
cargo run --release -- build example/fib.wi -o fib

# Release build
./target/release/willowc example/fib.wi -o fib --release

# Or
./target/release/willowc example/fib.wi -o fib --release

# Run the output binary
./fib
```

During development you can use `cargo run`:

```bash
cargo run -- example/fib.wi -o fib
./fib
```

## Examples

### Hello World

```rust
fn main() {
    println("Hello World");
}
```

### Functions

```rust
fn add(a: i64, b: i64) -> i64 {
    return a + b;
}

fn main() {
    println(add(3, 4));  // 7
}
```

### Recursion

```rust
fn fib(n: i64) -> i64 {
    if n <= 1 {
        return n;
    }
    return fib(n - 1) + fib(n - 2);
}

fn main() {
    println(fib(10));  // 55
}
```

### if / else

```rust
fn main() {
    let x = 42;
    if x > 10 {
        println(x);
    } else {
        println(0);
    }
}
```

### while

```rust
fn main() {
    let mut i = 0;
    while i < 5 {
        println(i);
        i = i + 1;
    }
}
```

## Types

| Type   | Example         |
|--------|-----------------|
| `i64`  | `let x = 10;`   |
| `bool` | `let b = true;` |

Type annotations are optional when the type can be inferred:

```rust
let x: i64 = 10;  // explicit
let y = 10;       // inferred
```

## Mutability

Variables are immutable by default. Use `mut` to allow reassignment:

```rust
let x = 10;
x = 20;      // compile error

let mut y = 10;
y = 20;      // ok
```

## Classes & OOP

Willow is class-based with **private-by-default** fields and methods. Use `pub`
to expose them. Objects are GC-managed and created with `new`.

```rust
class User {
    name: String;        // private by default
    pub age: i64;        // public field

    // `init` is the constructor — it takes an explicit `self`.
    pub init(self, name: String, age: i64) {
        self.name = name;
        self.age = age;
    }

    // Methods are private by default; `pub` exposes them.
    pub fn greet(self) -> String {
        return self.name;
    }
}

fn main() {
    let u = new User("Alice", 30);   // construct with `new`
    println(u.greet());              // Alice
    println(u.age);                  // 30
}
```

A class with no `init` gets an implicit constructor taking its fields in
declaration order.

### Static members

`static fn` methods and `static` properties belong to the class, not an
instance. Call them through the type with `::`. Static properties are immutable
and initialized once before `main` runs; `Self::` refers to the enclosing class.

```rust
class Counter {
    value: i64;
    pub static origin: i64 = 100;     // class-level, immutable

    pub init(self, value: i64) {
        self.value = value;
    }

    // Static factory — no `self`.
    pub static fn make(value: i64) -> Counter {
        return new Counter(value);
    }

    pub fn get(self) -> i64 {
        return self.value;
    }
}

fn main() {
    let c = Counter::make(7);
    println(c.get());          // 7
    println(Counter::origin);  // 100
}
```

### Interfaces

An `interface` is a set of required methods. A class `implements` it and can
then be used wherever the interface type is expected; calls dispatch at runtime.

```rust
interface Animal {
    fn speak(self) -> String;
}

class Dog implements Animal {
    pub fn speak(self) -> String { return "woof"; }
}

fn describe(a: Animal) {
    println(a.speak());
}

fn main() {
    describe(new Dog());   // woof
}
```

## Async / Await

Willow has a cooperative async runtime — there is **no `spawn` keyword**.
Calling an `async fn` immediately returns a `Task<T>` that runs on the
scheduler. Wait for it with `await` (suspends the current task so others can run)
or `.join()` (drives the scheduler synchronously). Tasks are lightweight, not OS
threads.

```rust
async fn work(x: i64) -> i64 {
    await sleep(1);     // suspend so other tasks can make progress
    return x * 2;
}

async fn main() {
    let a = work(10);     // start a task (runs concurrently)
    let b = work(20);     // start another

    println(await a);     // 20
    println(b.join());    // 40
}
```
