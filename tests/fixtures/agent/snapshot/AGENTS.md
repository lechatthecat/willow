<!-- BEGIN WILLOW MANAGED -->
<!-- willow-instruction-schema: 2 -->
# Willow instructions for codex

## Willow core

Run commands from the project root. Use compiler results to validate changes.
- Check: `willow check . --format ndjson --protocol-version 1`.
- Build: `willow build . --format ndjson --protocol-version 1`.
Parse stdout as NDJSON only in toolchain machine mode; stderr is human output and must not be parsed as protocol. Require supported schema_version on every event, a single stream with contiguous seq, and a final request.finished event. Missing request.finished is failure, never success. Require terminal status and exit_code to agree with the process exit status. Reject unsupported versions without guessing a fallback.

Use willow add, willow remove, willow update, willow deps, willow fetch, and willow package verify for package work. Prefer --dry-run when planning add/remove/update changes. Package commands have their own output contract; do not interpret package output as the check/build NDJSON event protocol. Do not edit project.lock by hand.

Before semantic or cross-file work, obtain a snapshot at the project root: `willow snapshot save . --output snapshot.json --format ndjson --protocol-version 1`. Query compiler facts against the corresponding revision; do not infer calls, references or impact from text search alone. Text search remains useful for navigation.

Source, project.toml, project.lock, or path-dependency edits make the old snapshot stale. After edits, run machine-readable check, then obtain a new snapshot before further semantic queries. Finish with check/build. On snapshot failure, explicitly describe investigation as textual and do not invent semantic results. Snapshots do not replace Git commits. Trivial comment, README or typo edits do not require snapshots.

- Symbols: `willow query . --requests queries.json --format ndjson --protocol-version 1`.
- References: `willow query . --requests queries.json --format ndjson --protocol-version 1`.
- Callers: `willow query . --requests queries.json --format ndjson --protocol-version 1`.
- Impact: `willow impact . --file src/main.wi --byte 0 --format ndjson --protocol-version 1`.
- Structured edits: `willow edit prepare --root . --entry src/main.wi --project --requests edits.json --format ndjson --protocol-version 1`.
Query request files are JSON arrays. Use compiler-returned IDs and the current revision; a stale, unknown or incomplete result is not proof of absence. The query command constructs its own immutable snapshot; bind requests to the saved revision to detect changes between commands.

<!-- END WILLOW MANAGED -->
