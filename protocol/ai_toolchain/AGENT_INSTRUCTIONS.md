# Agent instruction interface

Run `willow agent instructions codex` or `willow agent instructions claude`
to print the instructions produced by the current binary. `--format json`
returns one JSON object, not the NDJSON event stream:

- `schema_version`: 1, the instruction-response envelope version.
- `agent`: `codex` or `claude`.
- `instruction_schema`: 1 for check/build instructions and 3 when direct semantic commands are available. This is independent of package,
  manifest, and toolchain protocol versions.
- `capabilities`: boolean fields `machine_check`, `machine_build`,
  `symbol_query`, `references`, `callers`, `impact`,
  `structured_edits`, `direct_refs`, `direct_symbol`, `direct_type`,
  `direct_effects`, `direct_impact`, `direct_rename`.
  Disabled capabilities omit their command examples.
- `markdown`: the generated managed section, including its markers.

The driver supplies command examples from the modules owning their CLI syntax.
Query examples serialize the compiler's public query request type. Codex and
Claude share the same core; only the agent wrapper differs.
`agent instructions` and managed `agent sync` use the current generator.

Schema 3 makes refs, symbol, type, effects, impact and rename the ordinary
workflow. It uses human output first and project-relative file:line:column
positions, leaving IDs, byte offsets and request JSON to advanced integrations.
Semantic identity comes from the compiler; text search remains useful for
navigation, comments and strings. Ambiguity is resolved using returned selectors.
Coverage and truncation must still be read. Rename may be previewed with
`--dry-run`, then run directly; there is no manual prepare/validate/apply sequence.
A disabled direct rename capability alone enables the legacy edit fallback.
After modifications run `willow check .`, and build when executable validation
is needed. Advanced query/edit integrations use current-source queries and reject
stale revisions. Snapshot and daemon capabilities are no longer advertised.

`willow agent sync` examines AGENTS.md and CLAUDE.md in the current directory.
It prints instruction-version transitions and asks `[Y/n]` before updating;
`--yes` accepts updates, while non-TTY use without it leaves files unchanged.
Only text between the standalone `<!-- BEGIN WILLOW MANAGED -->` and
`<!-- END WILLOW MANAGED -->` markers is replaced. Outside bytes are preserved.
Missing files and files without a managed block are untouched.
Malformed, duplicate or inline markers fail before updates begin.
Each changed file is checked again before atomic replacement; this is not
a multi-file crash transaction. Unchanged instructions report already current.

Golden files in tests/fixtures/agent cover check/build and direct-command instruction schemas and both
wrappers. The bootstrap integration test executes the direct references, type,
effects, impact and rename workflow, then checks the modified program. Sync tests
verify schema upgrades, preservation of user text and malformed-block rejection.
