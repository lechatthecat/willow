# Willow Regression Programs

This directory holds runnable Willow programs that pin compiler and runtime
behavior: code generation and lowered-IR coverage, GC and barrier paths,
preemption and task frames, effect and semantic-query precision, and
multi-module resolution. They reproduce specific shapes rather than teach a
feature, so they live here instead of in the user-facing
[`example/`](../../example/README.md) directory.

They follow the same conventions as the examples:

- Every root `*.wi` file and every project or multi-module directory entry
  with `fn main(` is a runnable program. Imports resolve relative to the entry
  file, so a `*_support` directory belongs to the entry file with the same
  stem.
- `// test: manual` excludes a program from automated execution.

The example catalog in `tests/integration/runtime/examples.rs` lists every
entry file from both directories with its expected output. The CI example
audit (`runtime::examples::test_runnable_example_files_compile_and_run`)
compiles and runs all of them, and
`runtime::examples::test_runnable_example_catalog_is_complete` fails when an
entry file in either directory has no expected output. Integration and unit
tests also load individual programs from here by path.

Run one program directly with:

```text
cargo run --bin willow -- run tests/regressions/array_fast_paths.wi
```
