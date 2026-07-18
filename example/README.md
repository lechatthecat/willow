# Willow Examples

Examples in this directory are split into two groups.

- Root `*.wi` files are intended to compile and run as the current compiler grows.
- `future/**/*.wi` files are intentionally ambitious examples for planned language features. They may not compile yet.

Future examples should start with:

```text
// status: future
```

That marker lets tests keep them in the example catalog without treating them as runnable programs.

Interactive or intentionally non-terminating examples should contain:

```text
// test: manual
```

Manual examples remain runnable, but the automated example catalog does not execute them.
