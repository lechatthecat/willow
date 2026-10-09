# Rust bridge query, update impact, and repair

The executable in this repository is `willow` (older design discussions call it
`willowc`). Cargo owns Rust resolution and diagnostics. Compiler snapshots own
Willow declarations, package-aware FunctionIds, and the call graph. The command
session combines that snapshot with Cargo's dependency graph; it does not inspect
Rust function bodies or implement a second Rust resolver.

## Inspect a bridge

```sh
willow query rust-bridge regex_is_match --project-dir example/rust_interop_queries --format json
```

Omit the selector for all bridges, or pass the returned `function` ID to distinguish
same-named declarations. Each record contains `schema`, `kind`, `symbol`, `crate`,
`rust_adapter`, `abi_symbol`, `signature`, `identity`, declaration spans, and the
original `declaration_text`. The response schema is
[`rust-bridge-v1.schema.json`](rust-bridge-v1.schema.json).
The example uses a tiny offline path crate named `regex`, not the crates.io regex
implementation; its scalar predicate accepts 42.

The adapter is arbitrary Rust and can call several crates, including through
macros. All direct adapter dependencies therefore form a conservative shared
`crate_candidates` set. A sole candidate also appears as `crate`; multiple
candidates give `crate: null`, rather than inventing an exact attribution.
Candidates are stored/emitted once, not copied for every bridge. Both unchanged
and unused bridge declarations remain inspectable.

Package manifests using `[willow] manifest-version = 1` retain the existing
`identity { package, module, symbol }` protocol. Legacy projects can have null
identities, as in other semantic queries. The ABI binding's historical
`rust_crate` field is a project namespace, not a Cargo crate name; consumers must
use the projected crate identities instead.

Existing query batches also accept:

```json
[{"kind":"rust-bridge","symbol":"regex_is_match"}]
```

Batches use the immutable snapshot and never invoke Cargo per query. A missing
revision binds to the CLI invocation; a mismatched revision returns `stale`.
Recorded Rust versions are included only when manifest/adapter configuration fingerprints and
the persisted Cargo.lock checksum match; otherwise versions are null. Standalone
`query rust-bridge` consults Cargo metadata for resolved versions. It leaves the
project untouched but can populate the external Cargo/Willow caches. Use
`--offline` to forbid network resolution and `--cache-dir` to select the cache.

## Preview an update

```sh
willow rust update regex --dry-run --project-dir example/rust_interop_queries --format json
```

The response contains `from`, `to`, `bridge_symbols`, and
`affected_willow_callers`, plus traversal counts and coverage in `impact`.
`from` uses the published direct-version summary when present (so live path-crate
version edits still show the old published version); without that summary it uses
an initial Cargo resolution. `to` is Cargo's candidate resolution. Published
summaries do not contain Cargo source IDs, so `from` need not have a `source` or
`id`. These fields are never fabricated.

A shared multi-source caller traversal includes transitive Willow callers once.
The set is conservative for the adapter as a whole, even for a named update or a
no-op version resolution: it describes callers to revalidate, not proof that
those callers changed behavior. Rust internals, macro expansion and unanalysed
indirect calls are not inferred; unknown Willow call coverage is marked.

Dry-run does not write the manifest, project.lock, persisted Cargo.lock, or project
lease files; even a fresh project gets no `.willow` directory. Candidate manifests,
Cargo locks and resolution state live only in the external cache. Normal update
retains its existing publication/rollback path. `--breaking` uses the existing
Cargo-driven policy; `--locked`/`--frozen` cannot be combined with dependency edits.
A preview is advisory and does not reserve a future update against concurrent
source changes.

Normal `impact` responses expose `dependency_boundary` records with
`kind: rust`, a bridge FunctionId/package identity, and the crate (when unique).
`crate_scope: rust_dependencies` links the boundary to the response's shared Rust
candidate table. Traversal stops at bridge nodes; it never pretends to traverse a
Rust crate's internal functions. `App::validate -> regex_is_match -> regex@1.2.3`
is covered by the acceptance test.

## Generate or repair an adapter

1. Inspect declarations using the query above and inspect the returned Willow
   caller identities. Generate/edit the explicit Rust adapter and Willow
   declarations as ordinary reviewed source changes.
2. Run `willow rust check --project-dir DIR --format json`. A Cargo bridge failure
   returns one structured error containing Cargo `diagnostics`, `cargo_stderr`,
   `bridge_declarations` (original declaration text, ABI and signatures), and
   `willow_callers` (caller identities and coverage).
3. Fix the reported adapter symbol/type error against that evidence. Repeat the
   check, then run the affected Willow tests and application normally.

AI generation/repair is a client workflow: it is not invoked by the package
manager and cannot silently rewrite code during resolution. Cargo check may run
build scripts/procedural macros, as in the existing Rust check workflow. Native
runtime auto-build, adapter ABI generation, and linking remain owned by their
existing build paths.
