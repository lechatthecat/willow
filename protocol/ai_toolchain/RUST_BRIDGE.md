# Rust bridge query, update impact, and repair

Start with the [README Rust interop introduction](../../README.md#rust-interop)
for a runnable crate adapter, boundary types and the scope of effect guarantees.

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
[`rust-bridge-v2.schema.json`](rust-bridge-v2.schema.json).
Boundary signatures encode scalar types as strings, `Array` with its element type,
and `Record` with its declared name. Each bridge's optional `records` array defines
reachable record names and ordered `[field_name, value_type]` pairs. Nested records
refer to this table by name, keeping repeated type references compact. Existing
responses without `records` remain valid. `Bytes` denotes packed `Array<u8>`,
with `&[u8]` synchronous inputs,
owned `Vec<u8>` async inputs and `Vec<u8>` outputs. `Array<i64>` is encoded as
`{"Array":"I64"}` and maps to numeric `Vec<i64>`. Migrate byte declarations to
`Array<u8>` and convert integer values with `checked_to_u8()`. See
[`rust_record_bridge`](../../example/rust_record_bridge/README.md) for the adapter
mapping, ownership rules, supported subset, and an executable example.

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
identities, as in other semantic queries. The ABI binding's
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


## Synchronous foreign execution paths

| Class | Declaration and effects | Execution path |
| --- | --- | --- |
| Unknown | An ordinary synchronous declaration, with conservative `MAY_BLOCK` | Direct call on the synchronous caller's thread inside a GC safe region; rejected in Tasks by the compiler and runtime admission guard. |
| Blocking | A native body known to wait uses the same ordinary declaration and `MAY_BLOCK`; there is no separate `blocking` annotation | The same synchronous guarded GC safe region as Unknown. |
| NonBlocking | A current reviewed `nonblocking` / `no_block` assertion removes `MAY_BLOCK` | Direct call on the caller's thread, including a Task worker, inside a GC safe region. No blocking-pool submission. |

The compiler's effective bridge facts select the admission guard when generating
each adapter. There is no runtime classification switch or automatic pool hop.
The file-I/O blocking pool does not execute synchronous foreign calls. An Unknown
or Blocking body cannot occupy a Task worker because it is rejected before entry.
While an admitted synchronous caller waits, its published roots allow GC and
independently driven Tasks to proceed. Its own continuation waits for the call;
a synchronous main thread does not itself drive queued Tasks while inside Rust.
Use an explicit async bridge when a Task needs to await foreign work.

All three paths preserve native thread/TLS and stack identity. Adapters pin
borrowed managed inputs before entering the region, leave it before converting
outputs, and leave it on panic as well. Native callbacks confined to native code
remain on that thread; reverse callbacks into Willow are unsupported and must
not run inside the region. A NonBlocking assertion covers the complete native
call, including destructors, and must not conceal a wait.

## Bounded synchronous calls from tasks

A function can declare `nonblocking("<legacy_review_fingerprint>")` before `fn` inside
`extern rust`. Obtain the current fingerprint with `willow query rust-bridge`.
Review the adapter and its resolved dependencies before copying
`legacy_review_fingerprint` into each function declaration. Write the distinct
shared `review_fingerprint` into `rust-review.toml`; it also binds the canonical
legacy claim identities and effect names. Adding, removing or changing a claim
requires a new shared review, without changing the inline digest encoding. `check`, `build`, and `run` reject stale reviews.
The fingerprint binds the resolved Cargo.lock (including crate versions), root
bridge source, and Rust manifest configuration. Dependency updates require a new
review. Mutable Cargo path-dependency trees are included in the shared
fingerprint, even without version changes. Mutable path packages support normal
in-tree Rust modules and unqualified literal `include_bytes!`/`include_str!`
references to included in-tree files. Code includes, path overrides, dynamic or
aliased loaders, unknown macros, procedural attributes, native `link` attributes,
mutable build scripts and
procedural macros cannot establish a closed input set and are rejected for
review. Private/generated directories and out-of-tree files cannot be consumed
by an admitted layout. A fixed set of standard formatting/assertion macros and
built-in derives is supported; imports that could shadow them are rejected.
Cargo targets must use in-tree `.rs` files. File-loading root adapter constructs
must satisfy the closed reviewed source boundary. This includes `#[link(...)]`
in root adapters and native Rust modules, even under conditional attributes or
inside macros: declare native libraries through the manifest so their reviewed
inputs are explicit. Replacing an unbound archive cannot preserve an admitted
root claim because such a source layout is rejected before review issuance.
Root adapters and native Rust modules use the same closed expansion policy as
mutable dependencies. Global, inline and naked assembly are unsupported for
reviewed claims, including assembler file directives and macro aliases. Only
the fixed unqualified standard macro/derive set and the compiler-provided
`#[willow::assume(...)]` marker are admitted; custom macros, arbitrary procedural
attributes and environment/file-loading expansions fail closed. Root Rust
modules must be inline or imported independently through Willow. This prevents
new expansion mechanisms from silently adding unreviewed inputs.

This is a developer assertion that the complete call, including conversion and
native destructors, is short and bounded, fits the available native stack, and
never calls back into Willow. It permits direct execution on a scheduler worker;
it does not move work to a blocking pool or add preemption inside Rust. Task
native stacks reserve 8 MiB with platform guard protection; this is a total stack
budget, not a promise that all 8 MiB remain at a particular call site. Long or
blocking operations must remain undeclared. Foreign bodies retain GC idle regions,
pinned inputs and panic containment. Aggregate conversion still costs time
proportional to the values copied. Native Rust panic terminates with `RustPanic`
without unwinding through the C ABI.

The declaration removes MAY_BLOCK, permitting `no_wait` and lock contexts, while
retaining MAY_ALLOCATE, MAY_PANIC and FOREIGN_CALL. It never establishes purity.
Undeclared functions retain static task rejection and the runtime task guard.
Generated opaque close functions remain blocking unless separately reviewed:
place `nonblocking("<legacy_review_fingerprint>")` before the `opaque` declaration to
review its generated close function. Reviewing another function does not review
close; arbitrary native Drop implementations can block.

Records expose `effects`, `nonblocking`, `trust`, and `official_verified`.
`trust` distinguishes `undeclared`, `developer_declared`, and the reserved
`official_verified` category. There are currently no officially verified adapters;
source syntax cannot claim that status. Standalone Cargo-backed queries also
return `review_fingerprint` and per-record `review_current`; snapshot-only queries
do not claim that a review matches the current dependency resolution.

`example/rust_task_bridge` runs the crates.io regex implementation from two Tasks
with a fixed pattern and a 64-byte input limit. Run `willow run
example/rust_task_bridge`. If dependency resolution has changed, inspect the
query, review the new resolved graph and refresh the declaration before running.

## Remaining interop directions

The current [async bridge](../../README.md#async-calls-willow-task-awaits-rust-future)
supports Willow Tasks awaiting Rust Futures through explicit adapters. Reverse
Rust-to-Willow callbacks, Rust awaiting Willow Tasks, and automatic bindings for
arbitrary generic or trait APIs remain unsupported. Callback integration and a
restricted auto-binding subset are future directions, not available features or
a delivery commitment. Keep using concrete handwritten adapters and the current
boundary types; one-way Future support does not imply general bidirectional
runtime integration.

## Reviewed effect assumptions

Rust functions, methods and opaque types accept one attribute:
`#[willow::assume(no_block, no_io)]`. Either name may be used alone, in either
order. On an opaque type the claim covers its generated close operation and
`Drop`. `no_block` excludes waiting in synchronous calls; `no_io` excludes
external I/O. Async functions may assert `no_io`, but not `no_block`.
Unknown names, duplicates, and other effects are errors. `no_panic` and
`no_alloc` cannot be asserted because adapters may panic or allocate.

Every claim requires the same current `rust-review.toml` fingerprint, including
legacy `nonblocking("...")` and extern `@effects(no_io)` declarations. Legacy
inline fingerprints must also remain current during migration. A missing or
stale record disables every claim; dependent contracts fail closed. Querying
`rust-bridge` can still produce the review fingerprint when contracts fail.
Review sources and resolved dependencies before explicitly updating the record;
commands never approve a review automatically. Native-library adapters follow
these same rules and native manifest changes invalidate the review.

Bridge schema 2 reports declared `assumptions`, `review_current`, effective
`effects`, `evidence_kind`, and `assumption_location`. A declared but inactive
claim is `unknown`, never proof. Effects schema `willow.effects.v3` reports
`evidence_kind: "assumption"` and a source location for absence dependent on a
reviewed boundary; the `assumptions` map also covers `no_wait`. Contract
warnings use W2613 and point to the assumption. `verify-edit` schema v3 (v4
with contract approval policy) exposes `summary.assumptions.base/target`, even
for unchanged contracts. Clients must check versions and reject unsupported
versions. Archived bridge schema 1 rejects schema 2 output.

Assumption provenance keeps one representative source location per effect and
callable; the bridge inventory lists every declared boundary. Reviewed foreign
projects recheck their source/dependency boundary before semantic analysis; no
extra checks or allocations are added to generated runtime call paths. Git
verification captures tracked Cargo package inputs and fails closed for
unavailable inputs instead of borrowing current files for a historical base.
