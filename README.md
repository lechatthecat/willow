# Willow

> A statically typed, garbage-collected native programming language with its own runtime, package manager, incremental compiler infrastructure, and AI-oriented semantic tooling.

![Status](https://img.shields.io/badge/status-experimental-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

Willow is an experimental programming language that compiles to native code through [Cranelift](https://cranelift.dev/).

It combines class-based object-oriented programming with algebraic enums, pattern matching, `Option`, `Result`, closures, and stackless `async` / `await`.

## How to start

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

---

# License

[MIT License](LICENSE)
