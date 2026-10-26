# Import cycle diagnostics audit

Date: 2026-10-26

## Scope and policy

Retain acyclic imports, including type references, and document the shared-model
pattern in `docs/recursive-type-modules.md`. The runnable four-file example
retains Value -> Lambda -> Scope -> Map<String, Value> type recursion while
separating operations. No runtime, ABI, type layout, or code generation changes.

The resolver previously rendered package routing keys as numeric package indices
plus module paths. It now borrows the module path strings for both loader modes.
Internal ModuleKey identity and cycle detection are unchanged. Dependency package
cycles are rejected by package resolution; module names in a reported module
cycle are package-local, and aliases do not change their spelling.

## Call-path and complexity review

Reviewed resolve_one -> ModuleGraph::begin_key_visit and both resident/spooled
loader entry paths. The active-visit position hash map detects a back edge
without rescanning the active chain. Existing cycle extraction clones the active
cycle once; display visits each extracted key once. For C displayed nodes and B
path bytes, the changed formatting is O(C + B) time, O(C) borrowed-slice scratch
and O(B + C) output. It removes one owned String allocation per node. Writing
all displayed paths requires Omega(B + C) work. No new graph traversal or
package lookup is performed per node. Multiple errors still each pay for their
own output; there is no new multiplication across modules or passes. Generated
code size and runtime cost are unchanged because this is an error-reporting path.

Deterministic increasing-chain assertions in
`package_loader_cycle_diagnostic_scaling` measured:

| Cycle modules | Source loads | Displayed nodes |
| --- | --- | --- |
| 4 | 4 | 5 |
| 16 | 16 | 17 |
| 64 | 64 | 65 |

Exact diagnostic strings are asserted, including the repeated closing node.
These counts verify loads and output nodes, not wall-clock asymptotics; the
formatting bounds above follow from code inspection. The test itself is the
checked-in reproducible generator.

## Validation

Passed on x86_64 Linux:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --lib package_loader_cycle -- --nocapture
cargo test --lib package_loader_cycle_diagnostic_scaling -- --nocapture
cargo test --test integration test_import_diagnostic_cycle -- --nocapture
cargo test --test integration test_import_nested_cycle_shows_cycle_path -- --nocapture
cargo test --test integration test_import_recursive_types_shared_model_runs -- --nocapture
cargo test --test compiler_db importing_file_back_to_entry_is_a_cycle -- --nocapture
```

The package-loader filter passes three tests. Its matrix explicitly exercises
20 perspectives: two-module, self, alias, item import, repeated import,
class-field type, enum-payload type, three-module, reversed entry, and nested
module cycles, each through resident and spooled loading. An additional test
checks a dependency package's cycle without a numeric prefix. The standalone
CLI tests preserve simple and nested paths and entry-program rejection. The
shared-model integration test compiles, links, runs and asserts `0\n0\n`.

No full suites or full example audit ran. CI was not inspected. macOS arm64,
macOS x86_64 and Windows native execution are unverified. Cross-target Clippy
was not required: the change only formats existing diagnostic strings, with no
platform-specific API, filesystem operation, runtime or cfg change.
