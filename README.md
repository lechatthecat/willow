# Willow

Rust-like language with GC
Not production ready yet

- Static typing with type inference
- Immutable variables by default
- Garbage collection (planned)
- Class-based OOP with private-by-default members 
- Native binary output via Cranelift

## Install

Requires a Rust toolchain.

```bash
git clone <repo>
cd willow
cargo build --release
```

The compiler binary is at `target/release/willowc`.

## Usage

```bash
# Compile a source file
./target/release/willowc example/fib.wi -o fib

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
    let mut a = 10;
    a = 20;

    let b = 30;
    let c = a + b;

    println(c);  // 50
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
