# Structured edits and warm queries

The V1 NDJSON event envelope also carries `edit.*` and `daemon.*` analysis results.
Schemas: `edit-v3.schema.json`, `daemon-v4.schema.json`.

## Editing

Start from `willow snapshot save main.wi --output base.json`. Copy the revision
and function id into a request conforming to the edit schema, for example:

```json
{"revision":"<base revision>","operations":[
  {"kind":"rename","function":"<function id>","name":"answer"},
  {"kind":"replace-body","function":"<function id>","body":"{ return 42; }"}
]}
```

```sh
willow edit prepare --root . --entry main.wi --requests edits.json
willow edit preview --root . --transaction <transaction>
willow edit validate --root . --transaction <transaction>
willow edit apply --root . --transaction <transaction>
# After interruption or a write error:
willow edit recover --root . --transaction <transaction>
```

`prepare` returns the transaction id and full before/after text for every changed
file. It leaves workspace source files unchanged. `validate` performs cold,
complete frontend validation in the isolated candidate; its diagnostics name
workspace-relative source paths, never the `.willow-edits/<tx>/` copy. `apply` accepts only
that unchanged candidate and unchanged base inputs, journals undo bytes before
writing, and returns the resulting analysis revision. Repeated apply is rejected.
Recovery restores the original bytes only if current bytes equal either the
original or candidate content. A conflicting file is preserved and recovery
stops before modifying any file in its preflight pass. Restore/resolve the
reported conflicting file explicitly before retrying recovery; no automatic
conflict resolution is attempted.

Atomic visibility is scoped to operations holding `Workspace`'s exclusive OS
file lock, including edit operations and daemon refresh. The journal blocks
those observers after an interrupted apply until recovery. Ordinary editors,
filesystem readers and standalone builds do not take this lock: they can observe
intermediate files. Non-cooperating writers are detected at the input checks,
before each replacement and during recovery, but an uncooperative write racing
the final check/rename cannot be protected by a portable advisory lock. Power-loss
durability requires filesystem support for file and directory synchronization;
Unix synchronizes parent directories after replacement and marker removal.
Windows synchronizes file contents before replacement, but does not synchronize
directory entries, so power-loss durability is not promised there. Process-crash
recovery uses the same journal on both platforms. Native recovery validation has
been run locally on Linux. No Git operation is performed.

The initial edit surface uses debug/default compiler options and workspace-local,
regular, non-symlink source files. `--project` selects project mode. External
source dependencies are rejected rather than validated against a different tree.
Rename covers proven declarations, resolved direct/qualified calls, item imports
(`import m::{f}`, `import m::f as g`; the alias and its uses are kept) and
captured class dispatch families. It rejects an already-used destination identifier,
ambiguous or uncovered occurrences (including unsupported aliases, function
values and interface contracts), and overlapping edits. Body replacement must
be exactly one block and uses the compiler's source range. Unsupported cases
fail without changing source files. A rejection tied to one occurrence adds
`location` (`path` workspace-relative, zero-based byte `start`/`end`, one-based
`line`/`column`) to the `request.finished` event. Local candidates and journals remain under
`.willow-edits/`; retain interrupted journals until recovery. Completed candidate
history may be removed manually while no edit operation is active.

## Warm query process

```sh
willow daemon main.wi
```

The process emits `daemon.ready` with its current revision. Send one JSON request
per stdin line; each response echoes its numeric `id`. Query revisions are
mandatory. Queries read the installed immutable revision; source changes become
visible only after a successful `refresh`.

```json
{"id":1,"operation":"query","request":{"kind":"symbol-info","revision":"<revision>","function":"<function id>"}}
{"id":2,"operation":"refresh"}
{"id":3,"operation":"stats"}
{"id":4,"operation":"shutdown"}
```

A refresh uses the existing CompilerDb with module-granularity revision
invalidation. Unchanged parsed inputs keep their syntax/BodyId identities;
unchanged modules outside the reverse dependency closure replay typed-body
artifacts and skip body type checking. Body, public signature, type definition,
default-method and effect changes invalidate the changed module and its
consumers. Declarations and whole-unit effect/concurrency aggregation are rebuilt.
A change to resolver topology or compiler inputs starts a cold generation.

The BodyId-keyed analysis-query family additionally reuses symbol, references
and effects responses. Transitive effect witnesses invalidate through old and
new call edges. Position queries retain their logarithmic index. This does not
cache native build artifacts or introduce another query engine. Configuration
changes require a fresh daemon. A failed refresh leaves the previous query
revision available. EOF or shutdown releases all state; there is no detached
process or persistent compiler cache.

The process permits at most 32 frontend attempts (including initialization
and failed refreshes), then responds that a restart is required and exits. This
bounds lifetime growth of the existing process-wide compiler interner; it does
not introduce a second interner or claim that interning is already session-local.

Limits: 1 MiB per request, 64 MiB serialized current snapshot, 256 cached results
and 8 MiB serialized cached values. The accepted compiler revision retains at
most 128 MiB of serialized artifacts in one temporary pack; each successful
refresh copies live records into its new pack and releases the old one. Oversized individual results are returned but
not cached. Bounds on serialized data imply proportional heap retention, not an
8 MiB RSS guarantee. Refresh can temporarily hold the old/new snapshots and frontend
working state, including both artifact packs. `stats` reports deterministic cache/recompute counts and
retained serialized bytes. These limits concern retained query data; they do not
bound peak memory needed to compile arbitrary inputs.

Compiler counters in `stats` describe the last successful analysis:
`typechecks` counts actual typed-body evaluator executions;
`reused_typed_bodies` counts distinct imported typed records subsequently read;
`semantic_input_visits` counts revision inputs compared;
`semantic_invalidation_visits` counts traversed reverse edges;
`retained_artifact_bytes` counts bytes in the current compiler pack.
The existing `computations`/`hits` fields continue to describe response queries.
Lambda public names use their lexical owner and source range, independent of
process-local compiler IDs.
