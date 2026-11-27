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
| Unknown / Blocking | Ordinary synchronous declaration; `MAY_SUSPEND`, no `MAY_BLOCK` | In a task, owned native work runs on the bounded Rust pool and the caller parks. Outside a task, the calling thread runs it synchronously in a GC idle region. |
| NonBlocking | Current reviewed `nonblocking` / `no_block` assertion; no waiting effects | Direct execution on the caller, including a task worker, in a GC idle region. No pool submission or transport allocation. |

The same policy applies to native Rust modules, legacy `extern rust`, and
generated opaque close. `no_block` describes scheduler-worker blocking and is
satisfied by the default pool path; `no_wait`, `no_suspend`, and lexical locks
reject that path. Diagnostics identify the need for a reviewed `nonblocking`
assertion. Stale assertions remain build errors. All calls retain conservative
allocation, panic and I/O effects unless an applicable reviewed I/O claim exists.

Rust work uses its own pool, separate from file I/O and stdout. Each Rust pool
has `WILLOW_BLOCKING_THREADS` workers (default 4) and `WILLOW_BLOCKING_QUEUE`
queued jobs (default 16 per worker). A full queue parks callers. FIFO reservations
serve older waiters before newcomers. The existing exported blocking gauges
continue to describe the general file-I/O pool. A hung Rust body can delay other
Rust calls but cannot exhaust the file-I/O pool's threads.

Inputs are decoded and pinned on the caller; strings, arrays and records become
owned native values, and opaque arguments hold owned leases. Jobs and results
must be `Send + 'static`; no GC pointer enters the pool. Shared opaque leases
require `Send + Sync + 'static` even for reviewed declarations. Conversion back
to Willow happens on the caller after completion, with output roots protected.
Numeric vectors move into the adapter without a second element copy. Native
execution and destruction run inside GC idle regions. Native-to-Willow reverse
callbacks remain unsupported.

Cancellation before start skips the body. Running native code cannot be
interrupted; its result is discarded. Cancellation after publication discards
an unconsumed result too. Native destruction uses the same bounded pool, so
cancellation may wait for capacity, a running body, or its destructor. This avoids
running arbitrary `Drop` code on a scheduler worker or leaking native values.
Deferred close still completes during cancellation. A panic in the default
body or close is contained and raised as recoverable `RustPanic` on the calling
task; the pool remains usable. Reviewed direct calls retain their fatal panic
boundary. No Rust unwind crosses the C ABI.

Pool calls do not preserve thread/TLS affinity. Non-`Send` or non-`Sync` opaque
types are build errors. For a thread-affine library, keep its state on an
adapter-owned dedicated thread and expose a `Send + Sync` proxy exchanging owned
messages. Keep waiting proxy operations unreviewed. A `nonblocking` assertion
does not establish affinity between separate task invocations.

Parallel calls use ordinary eager task creation: wrap a synchronous Rust call in
an `async fn`, call it several times, then await the returned tasks. Willow has
no separate `spawn` keyword or blocking declaration syntax. Use `defer` for
explicit handle close. See [rust_blocking_pool](../../example/rust_blocking_pool).

### When a direct-call review is worthwhile

A local Linux x86-64 release measurement with one scheduler worker measured
roughly 7–8 microseconds per trivial pooled call versus 0.23 microseconds per
reviewed direct call. For a small 64-step native computation the figures were
about 7.6 versus 0.36 microseconds; a 4096-step computation took about 17.1 versus
8.8 microseconds. These are workload and machine dependent, not latency limits.
The measurements used 1,000 and 10,000 calls per batch, three repetitions, and
matched results and exact native invocation counts for both paths.

Start with ordinary declarations. A reviewed direct path is valuable for a
measured hot sequence of short calls (for example small TLS state-machine steps)
whose own work is comparable to or below the pool handoff cost. Batch small
operations when possible. Keep socket waits, contended locks, arbitrary callbacks
and unbounded destructors on the pool even if the common case looks fast.
A successful benchmark cannot justify `nonblocking`; bounded behavior of the
whole call still needs review. Re-measure the actual workload on its target.

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
proportional to the values copied. A panic on this reviewed direct path terminates
with `RustPanic` without unwinding through the C ABI.

The declaration removes the default wait/suspend effect, permitting `no_wait`,
`no_suspend` and lock contexts, while retaining MAY_ALLOCATE, MAY_PANIC and
FOREIGN_CALL. It never establishes purity. Generated opaque close runs through
the pool unless separately reviewed:
place `nonblocking("<legacy_review_fingerprint>")` before the `opaque` declaration to
review its generated close function. Reviewing another function does not review
close; arbitrary native Drop implementations can block.

Records expose `effects`, `execution`, `nonblocking`, `trust`, and
`official_verified`. `execution` is `blocking_pool_in_task`, `direct`, or
`async_executor`; the first executes directly outside task context.
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
reviewed boundary; the `assumptions` map also covers `no_wait` and `no_suspend`. Contract
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
