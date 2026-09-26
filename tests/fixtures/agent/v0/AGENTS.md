<!-- BEGIN WILLOW MANAGED -->
<!-- willow-instruction-schema: 1 -->
# Willow instructions for codex

## Willow core

Run commands from the project root. Use compiler results to validate changes.
- Check: `willow check . --format ndjson --protocol-version 1`.
- Build: `willow build . --format ndjson --protocol-version 1`.
Parse stdout as NDJSON only in toolchain machine mode; stderr is human output and must not be parsed as protocol. Require supported schema_version on every event, a single stream with contiguous seq, and a final request.finished event. Missing request.finished is failure, never success. Require terminal status and exit_code to agree with the process exit status. Reject unsupported versions without guessing a fallback.

Use willow add, willow remove, willow update, willow deps, willow fetch, and willow package verify for package work. Prefer --dry-run when planning add/remove/update changes. Package commands have their own output contract; do not interpret package output as the check/build NDJSON event protocol. Do not edit project.lock by hand.


<!-- END WILLOW MANAGED -->
