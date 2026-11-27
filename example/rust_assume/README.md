# Reviewed Rust modules

`#[willow::assume(no_block, no_io)]` asserts that a synchronous function or method
finishes without blocking a worker. On an opaque struct it asserts the same
for its generated `Type_close` operation, including `Drop`. The two claims remove
MAY_BLOCK and MAY_IO; allocation, panic and foreign-call effects remain. It is a developer
assertion, never official verification or a purity proof. Methods use the
`Type_method(handle, ...)` facade with shared opaque `&self` receivers.

The included review record covers the checked-in example. After changing it,
review every Rust module and resolved dependency, then run:

```sh
willow query rust-bridge --project-dir . --offline --format json
```

Explicitly record the returned `review_fingerprint` in `rust-review.toml`:

```toml
fingerprint = "<reviewed SHA-256 from the query>"
```

This file contains the project's single review value. Never put it in Rust
or Willow declarations. `check`, `build`, `run` and dependency resolution never
write this file. Changes to Rust sources (including attributes and unimported
modules), Cargo resolution or Rust manifest settings require another review.
The diagnostic supplies the current fingerprint and these manual review steps.
Run `willow run . --offline` to print `42` twice, from a task and a lexical lock.
The included `project.lock` and `.willow/rust/Cargo.lock` pin the resolved inputs;
both are reproducible dependency records. Run `willow run . --offline --locked`
to require those inputs. The regression test copies the shipped sources,
manifest, both locks and review into a temporary project and verifies that a
fresh locked run and a normal run preserve them; it never regenerates the review
record.

The compiler injects its bundled no-op `willow_assume` procedural macro under
the reserved dependency alias `willow`. For standalone Cargo, use the explicit
path dependency in this example's Cargo.toml and run
`cargo test --manifest-path Cargo.toml reviewed_methods_work_without_willow_compiler`.
For direct rustc, build the macro as a host `proc-macro` crate named `willow`,
then pass `--extern willow=<macro-library>` while compiling `src/native.rs`.
The macro checks the effect spelling and preserves the annotated item verbatim;
only the Willow compiler validates the review and enforces worker admission.

Reviewed Rust sources must have a closed file boundary. Use inline Rust
submodules or independent `.rs` modules imported by Willow. Out-of-line Rust
`mod name;` declarations (including `#[path]` and conditional forms), `include!`,
`include_str!`, `include_bytes!`, their imports/aliases and file-loading declarations
inside macro bodies are rejected before issuing or accepting a native review.
This also applies to unimported Rust sources when a
native assertion is reviewed. Inline modules and ordinary macros remain usable.
