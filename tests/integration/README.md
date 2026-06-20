# Integration test tiers

The `integration` test target is split by the compiler phase or subsystem that
each end-to-end scenario primarily exercises. Cargo filters make the tiers
independently runnable:

```text
Tier 1 — frontend diagnostics: lexer, parser, diagnostics
Tier 2 — semantic analysis:    typecheck
Tier 3 — native execution:     codegen, runtime, concurrency, toolchain
```

For example:

```shell
cargo test --test integration lexer::
cargo test --test integration typecheck::
cargo test --test integration runtime::
```

Run `cargo test --test integration` for the complete end-to-end gate. Shared
temporary source trees and binaries are owned by `support::TestProject`, whose
`Drop` implementation cleans artifacts on both success and failure paths.
