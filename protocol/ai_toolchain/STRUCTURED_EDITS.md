# Structured edits

The V1 NDJSON event envelope also carries `edit.*` analysis results.
Schema: `edit-v3.schema.json`.

## Editing

For ordinary renames use `willow rename SELECTOR NEW_NAME`. For advanced edits,
write `[ {"kind":"symbols","name":"value","symbol_kind":"function"} ]` to
`queries.json`, then run `willow query main.wi --requests queries.json`.
Copy the returned revision and callable `id` into an edit request, for example:

```json
{"revision":"<base revision>","operations":[
  {"kind":"rename","function":"<function id>","name":"answer"},
  {"kind":"replace-body","function":"<function id>","body":"{ return 42; }"}
]}
```

```sh
willow edit prepare --root . --entry main.wi --requests edits.json --changes diff
willow edit preview --root . --transaction <transaction> [--changes diff]
willow edit validate --root . --transaction <transaction>
willow edit apply --root . --transaction <transaction>
# After interruption or a write error:
willow edit recover --root . --transaction <transaction>
```

For a project query, use the same analysis mode when preparing edits:

```sh
willow query . --requests queries.json
willow edit prepare --root . --entry src/main.wi --project --requests edits.json --changes diff
```

`--entry` is relative to `--root`. `--project` is accepted only by `prepare`;
subsequent operations reuse the mode stored in the transaction. Omit it when
querying a source file. Revisions include the analysis mode,
so even unchanged sources can produce a revision mismatch if the modes differ.
For changed inputs, query the current source again and rebuild the request with its revision
and function ids; do not merely replace the revision in an old request.

`prepare` returns the transaction id and full before/after text for every changed
file. With `--changes diff` (accepted by `prepare` and `preview`) each change is
instead `{"path", "diff"}`: a unified diff with three context lines and
workspace-relative `a/`/`b/` headers, usually a small fraction of the file text.
The stored transaction is identical in both modes. It leaves workspace source files unchanged. `validate` performs cold,
complete frontend validation in the isolated candidate; its diagnostics name
workspace-relative source paths, never the `.willow-edits/<tx>/` copy. `apply` accepts only
that unchanged candidate and unchanged base inputs, journals undo bytes before
writing, and returns the resulting analysis revision. Repeated apply is rejected.
Recovery restores the original bytes only if current bytes equal either the
original or candidate content. A conflicting file is preserved and recovery
stops before modifying any file in its preflight pass. Restore/resolve the
reported conflicting file explicitly before retrying recovery; no automatic
conflict resolution is attempted.

## Visibility, concurrency, and recovery contract

`Workspace` holds the exclusive advisory OS lock at `.willow-edits/lock`
for its entire lifetime. Cooperating edit clients must acquire this lease before
reading or changing the workspace and retain it until their operation ends.
A second edit process fails to acquire it while the first holds it. Package and
Rust-bridge transactions share the lease. A lock alone does not resolve an
interrupted AI edit: subsequent edit operations reject the active marker until
explicit `edit recover`. Raw readers, ordinary editors, query commands, and
standalone builds do not universally participate in this lease; callers must
not infer snapshot consistency from a query/build command alone.

The observable states are:

| Durable boundary | Source files visible to raw readers | Next cooperating edit |
| --- | --- | --- |
| Prepared / validated | Original files; candidate lives separately | May preview, validate, or apply if inputs are unchanged |
| Active marker saved, before applying state | Original files | Refuses normal operations; recover restores originals |
| Applying, after any replacement prefix | Candidate bytes in replaced files, original bytes in the rest | Refuses normal operations; recover restores originals |
| Applied state saved, active marker still present | All candidate files | Recover clears the marker and keeps the committed candidate |
| Active marker removed | All candidate files | Normal operations resume; replay of this apply is rejected |
| Recovery interrupted | A mixture of original and candidate files | Recover may be retried; normal operations remain blocked |
| Recovery conflict | Conflicting external bytes and any existing prefix | Reports an error, preserves the conflict and active marker |

Replacement is per file; a multi-file edit has **no atomic filesystem visibility**.
An individual successful replacement exposes complete old or new file bytes.
Cooperating clients exclude live concurrent edits and reject interrupted states;
that is different from a cross-process snapshot for arbitrary tools. Recovery
preflights all changed files before its first replacement. It accepts only original
or candidate bytes, rechecks each file, restores original bytes for uncommitted
plans, and marks the plan aborted. A durable applied/aborted state needs only
marker cleanup. Thus a crash after commit preserves the candidate rather than
undoing a completed transaction. Direct `willow rename` additionally attempts
immediate rollback on errors; advanced `edit apply` leaves recovery explicit.

Changes from another cooperating process cannot interleave under the lease.
Non-cooperating changes are checked against all captured inputs before applying,
before each file replacement, after analysis, and during recovery. Observed
conflicts cause failure instead of silent overwrite. A source moved or deleted
since preparation invalidates the plan; apply does not recreate its old path.
A move, deletion, or modification encountered in recovery is a conflict and is
preserved. Symbol rename changes text at existing paths; it does not itself move
or delete source files. Re-query and prepare a new request after external path
changes; restoring/resolving a recovery conflict requires explicit user action.

These are advisory guarantees: an uncooperative writer racing between the last
check and replacement (or writing identical candidate bytes) cannot be distinguished
or protected by this protocol. Such writers must cooperate with the same lease
for guaranteed exclusion. Stronger arbitrary-reader visibility would require a
separate versioned snapshot layer with a single pointer switch and readers that
use that pointer; it is not implemented by this journal.

Power-loss durability requires filesystem support for file and directory
synchronization. Undo data and active intent are persisted before source writes.
Unix synchronizes the workspace root when acquiring the shared lease, so the
metadata directory entry is durable before journal or source publication, even
if a previous opener created it without completing initialization. It also
synchronizes parent directories after replacement and marker removal.
Windows synchronizes file contents before replacement, but does not synchronize
directory entries, so power-loss durability is not promised there. Process-kill
recovery uses the same journal on both platforms. Deterministic process-kill tests
cover journal boundaries, every prefix of a multi-file apply, commit, and repeated
recovery on Linux; these simulate interruption, not physical power loss. Foreign
native execution is unverified. No Git operation is performed.

The initial edit surface uses debug/default compiler options and workspace-local,
regular, non-symlink source files. `--project` selects project mode. External
source dependencies are rejected rather than validated against a different tree.
Rename covers proven declarations, resolved direct/qualified calls, item imports
(`import m::{f}`, `import m::f as g`; the alias and its uses are kept), local
bindings and parameters, fields, enum variants, and captured interface/class
dispatch families. Selecting a contract or implementation method renames its
connected dispatch family while preserving unrelated same-name symbols. It
rejects destination collisions, ambiguous or uncovered occurrences, unsupported
targets, and overlapping edits. Body replacement must
be exactly one block and uses the compiler's source range. Unsupported cases
fail without changing source files. A rejection tied to one occurrence adds
`location` (`path` workspace-relative, zero-based byte `start`/`end`, one-based
`line`/`column`) to the `request.finished` event. Local candidates and journals remain under
`.willow-edits/`; retain interrupted journals until recovery. Completed candidate
history may be removed manually while no edit operation is active.
