# Agent instruction interface

Run `willow agent instructions codex` or `willow agent instructions claude`
to print the instructions produced by the current binary. `--format json`
returns one JSON object, not the NDJSON event stream:

- `schema_version`: 1, the instruction-response envelope version.
- `agent`: `codex` or `claude`.
- `instruction_schema`: 1 for check/build instructions, 2 when snapshot
  workflow instructions are available. This is independent of package,
  manifest, and toolchain protocol versions.
- `capabilities`: boolean fields `machine_check`, `machine_build`,
  `snapshots`, `symbol_query`, `references`, `callers`, `impact`,
  `structured_edits`. Disabled capabilities omit their command examples.
- `markdown`: the generated managed section, including its markers.

The driver supplies command examples from the modules owning their CLI syntax.
Query examples serialize the compiler's public query request type. Codex and
Claude share the same core; only the agent wrapper differs. Init uses this
same generator.

Snapshot instructions describe saving at the project root, revision-bound
compiler queries, invalidation by source/manifest/lock/path-dependency changes,
checking before refreshing, and final check/build. Each save requires a new
output path because snapshots cannot overwrite existing files. The current query command
recompiles its own immutable snapshot; it does not load the saved file.
Explicit request revisions detect intervening changes. Callers are obtained
through impact with `--direction callers`, not a nonexistent callers query.
The examples contain placeholders that must be replaced with returned IDs
and revisions. Query-level status and impact coverage must be inspected even
when the protocol request succeeded.

`willow agent sync` examines AGENTS.md and CLAUDE.md in the current directory.
It prints instruction-version transitions and asks `[Y/n]` before updating;
`--yes` accepts updates, while non-TTY use without it leaves files unchanged.
Only text between the standalone `<!-- BEGIN WILLOW MANAGED -->` and
`<!-- END WILLOW MANAGED -->` markers is replaced. Outside bytes are preserved.
Missing files and files without a managed block are untouched.
Malformed, duplicate or inline markers fail before updates begin.
Each changed file is checked again before atomic replacement; this is not
a multi-file crash transaction. Unchanged instructions report already current.

Golden files in tests/fixtures/agent cover both schema generations and wrappers.
The bootstrap integration test executes the generated snapshot/query/callers
examples and checks stale rejection followed by check and snapshot refresh.
