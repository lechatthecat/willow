<!-- BEGIN WILLOW MANAGED -->
<!-- willow-instruction-schema: 2 -->
# Willow instructions for claude

## Willow core

Run commands from the project root. Use compiler results to validate changes.
- Check: `willow check . --format ndjson --protocol-version 1`.
- Build: `willow build . --format ndjson --protocol-version 1`.
Parse stdout as NDJSON only in toolchain machine mode; stderr is human output and must not be parsed as protocol. Require supported schema_version on every event, a single stream with contiguous seq, and request.started first and request.finished last. Missing request.finished is failure, never success. Require terminal status and exit_code to agree with the process exit status. Reject unsupported versions without guessing a fallback.

Use willow add, willow remove, willow update, willow deps, willow fetch, and willow package verify for package work. Prefer --dry-run when planning add/remove/update changes. Package commands have their own output contract; do not interpret package output as the check/build NDJSON event protocol. Do not edit project.lock by hand.

Before semantic or cross-file work, obtain a snapshot at the project root: `willow snapshot save . --output snapshot.json --format ndjson --protocol-version 1`. The --output destination must not exist; choose a new filename for every snapshot, including refreshes. Query compiler facts against the corresponding revision; do not infer calls, references or impact from text search alone. Text search remains useful for navigation.

Source, project.toml, project.lock, or path-dependency edits make the old snapshot stale. After edits, run machine-readable check, then obtain a new snapshot before further semantic queries. Finish with check/build. On snapshot failure, explicitly describe investigation as textual and do not invent semantic results. Snapshots do not replace Git commits. Trivial comment, README or typo edits do not require snapshots.

- Symbols: `willow query . --requests queries.json --format ndjson --protocol-version 1`.
- References: `willow query . --requests queries.json --format ndjson --protocol-version 1`.
- Callers: `willow impact . --function FUNCTION_ID --revision REVISION --direction callers --format ndjson --protocol-version 1`.
- Impact: `willow impact . --function FUNCTION_ID --revision REVISION --direction callers --format ndjson --protocol-version 1`.
- Structured edits: `willow edit prepare --root . --entry src/main.wi --project --requests edits.json --changes diff --format ndjson --protocol-version 1`.
Query request files are JSON arrays. Use compiler-returned IDs and the current revision; a stale, unknown or incomplete result is not proof of absence. The query command constructs its own immutable snapshot; bind requests to the saved revision to detect changes between commands.
Replace REVISION with analysis.result.data.revision from snapshot.saved (or the current query result). Write each following JSON array to queries.json before running its command. Check each query result status as well as request.finished: a successful transport can contain stale or unknown results.
Keep symbol lists small: replace NAME_PREFIX with a name prefix, or filter by `name`, `module` or `symbol_kind`; `total`/`truncated` report what `limit` cut. Read `type_display` for types.

Run `willow query . --requests queries.json --format ndjson --protocol-version 1` with:

```json
[{"kind":"symbols","revision":"REVISION","prefix":"NAME_PREFIX","limit":50}]
```

Run `willow query . --requests queries.json --format ndjson --protocol-version 1` with:

```json
[{"kind":"references","revision":"REVISION","function":"SYMBOL_ID"}]
```
Replace SYMBOL_ID with a compiler-returned declaration or callable ID from this revision; the references request field is named function even for non-callable declarations.
Replace FUNCTION_ID with a callable id from the saved snapshot file's functions and REVISION with that snapshot's revision. Impact constructs a fresh snapshot and checks --revision before lookup. Read coverage, truncation and unresolved-call evidence; an incomplete result is not proof of no callers.

<!-- END WILLOW MANAGED -->
