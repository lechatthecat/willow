# Lexer/parser fuzzing

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
