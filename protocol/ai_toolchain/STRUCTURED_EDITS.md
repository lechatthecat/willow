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

Atomic visibility is scoped to operations holding `Workspace`'s exclusive OS
file lock, including edit operations. The journal blocks
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
