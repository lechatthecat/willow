# Compiler fuzzing and runtime properties

This standalone cargo-fuzz package exercises UTF-8 validation, the production
lexer, parser recovery, and destruction of the resulting AST and diagnostics.
Invalid UTF-8 is rejected; lexical and syntax errors are normal results. Panics
are not caught. The target does not type-check, resolve imports, generate code,
or execute Willow programs.

From the repository root (prefix commands with `rtk proxy` when using RTK):

```sh
cargo install cargo-fuzz --locked
rustup toolchain install nightly --profile minimal
# Build with AddressSanitizer and replay the checked-in seeds.
cargo +nightly fuzz run parser fuzz/seeds/parser -- -runs=1 -max_len=4096 -timeout=5 -rss_limit_mb=2048
# The FIRST corpus directory receives generated inputs. Keep seeds read-only.
mkdir -p fuzz/corpus/parser
cargo +nightly fuzz run parser fuzz/corpus/parser fuzz/seeds/parser -- -seed=20260920 -runs=20000 -max_len=4096 -timeout=5 -rss_limit_mb=2048
```

The initial build also compiles compiler dependencies and can take several
minutes. `fuzz/Cargo.lock` pins this package's dependencies independently of the
main workspace. Fuzzing is opt-in and adds no work to ordinary workspace tests.
Use longer campaigns and larger `-max_len` values for deeper exploration; these
bounded runs do not establish crash-freedom or parser complexity bounds.

On failure, cargo-fuzz prints the artifact path and replay command. Minimize it
with `cargo +nightly fuzz tmin parser <artifact>`, then add a reviewed regression
input to `fuzz/seeds/parser/` and a focused compiler regression test when fixing
the bug. Generated corpora, crash artifacts, coverage, and binaries are ignored.
Use only synthetic or public source as input; do not seed with private material.

## Compiler artifact properties

The private `UnitArtifacts` store also has deterministic generated-property tests
that run without cargo-fuzz or a nightly toolchain:

```sh
cargo test -p willowc --lib module::artifacts:: -- --test-threads=1
```

These exercise real disk serialization and hydration, including AST identities,
all body-owning slots, repeated offload, multi-file isolation, source snapshots,
and malformed records. The case number reproduces each generated source; this
bounded suite does not perform random mutation or shrinking. The
[test generator](../src/module/artifact_property_tests.rs) contains the synthetic
inputs and exact record-count assertions.

## Optional runtime AddressSanitizer checks

The manual **Runtime AddressSanitizer** GitHub Actions workflow runs focused
runtime tests on Linux x86_64. Run the identical gate locally with:

```sh
rustup toolchain install nightly-2026-09-20 --profile minimal --component rust-src
bash scripts/runtime_asan.sh
```

The [runner](../scripts/runtime_asan.sh) instruments the runtime and rebuilds the
standard library with AddressSanitizer, using an explicit target and a separate
`target/runtime-asan` build directory. Leak detection stays enabled; any test or
sanitizer failure fails the job. This follows the
[Rust sanitizer instructions](https://doc.rust-lang.org/unstable-book/compiler-flags/sanitizer.html).

Coverage includes minor-collection relocation/root tests, runtime-root ownership,
GC reset teardown, the full channel unit suite, and nine generated nursery graphs
at 8/64/512 nodes. Each graph checks actual movement, payloads, two edges per
live node through three collections, and reclamation after root release.
The former channel fixture teardown leaks have been fixed; leak detection remains
enabled. This does not run generated Willow machine code, prove race freedom, or
exercise concurrent old evacuation. GC objects suballocated inside one region do
not gain individual ASan redzones. ThreadSanitizer is not enabled.

## Type-checker fuzzing

The `type_checker` target validates UTF-8, lexes, parses, desugars, then checks
accepted programs. Diagnostics are expected; panics propagate. It does not
resolve filesystem imports or execute compiled code. Check the seeds actually
reach semantic checking, then run a bounded campaign:

```sh
cargo test --manifest-path fuzz/Cargo.toml --lib --locked
mkdir -p fuzz/corpus/type_checker
cargo +nightly fuzz run type_checker fuzz/corpus/type_checker fuzz/seeds/type_checker -- -seed=20260921 -runs=20000 -max_len=4096 -timeout=5 -rss_limit_mb=2048
```

The compiler's process-global symbol interner retains spellings between inputs
(willow-9tls.26). Bound RSS and campaign length and restart for longer campaigns.

## Runtime properties and cancellation

```sh
bash scripts/runtime_properties.sh
```

This opt-in runner checks generated moving graphs, modeled old-graph reachability
under `WILLOW_GC_STRESS=minor` and `alloc`, channel/select ownership and handoff,
cancellation races and cleanup, representative invalid FFI inputs, and four
compiled async/select/recovery regressions. Tests serialize access to global
runtime state. The runner clears inherited allocation stress where nursery
movement is required; the separate stress invocations explicitly select modes.
Fixed graph seeds and operation counts print with `--nocapture`; failures can be
replayed with the same command. These bounded checks are not exhaustive schedule
exploration or a proof of memory/race safety. Ordinary push/PR CI is unaffected;
dispatch the optional ASan workflow explicitly when needed.
