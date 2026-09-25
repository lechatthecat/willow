# Willow toolchain protocol V0 (schema version 1)

`willow check main.wi --format ndjson` and `willow build main.wi -o app --format ndjson`
emit UTF-8 NDJSON on stdout. Each line is an independent JSON object. Both accept
project directories and existing compiler flags. `--format human` selects ordinary output.
`--protocol-version 1` optionally requires this protocol. Unsupported versions
fail before compilation, using the server version 1 envelope for the rejection.
V0 machine mode supports check/build, not run/debug, package operations or IR dumps.

Every event has `schema_version: 1`, an opaque `stream_id`, zero-based contiguous
`seq`, `event`, `code`, and object `data`. One request owns one stream.
`request.started` (WT0001) is first; `request.finished` is last, with `status`
(ok/error), `exit_code` (0/1/2), and display-only `message`. Success uses WT0000;
invalid arguments WT1001 (exit 2), unsupported protocol WT1002 (exit 2), diagnosed
compilation failure WT2001 (exit 1), other compiler/project/IO failure WT2002
(exit 1). Diagnostic events use stable E/W codes, with severity, message, labels,
notes, helps, and fix suggestions. Labels/fixes contain spans and resolved file
paths (null if unavailable). Byte offsets are zero-based and end-exclusive;
line/column are compiler one-based coordinates (zero means unavailable).
Internal file IDs are local to the request, not cross-request identities.

Human messages are not machine discriminators. Consumers ignore unknown fields
and event/code values within a supported version. Existing names, field types
and meanings require a new version for incompatible changes. Clients reject
unsupported versions; no implicit major-version fallback. Missing terminal
events, invalid JSON, unexpected seq, mixed streams or mismatching process exits
are transport/incomplete-request failures, never success. Broken pipes and
abrupt termination cannot guarantee a terminal event.

Stderr is unstructured human/tool output, outside the protocol. Cargo/linker
stdout goes to stderr. Machine mode never runs an app; ordinary run preserves
application output and status.

## Boundary and scope

The willow user driver owns argument/protocol adapters and statically links
willow_compiler. CompilerSession check/run emitter
methods perform the real project-aware frontend once, then (build only) native
generation/linking. There is no subprocess compiler wire ABI in V0. Driver and
library versions are built together; NDJSON is the externally versioned boundary.
The driver does not parse Willow or reconstruct compiler graphs. Library callers
own their diagnostic writer; ordinary callers retain HumanEmitter. Runtime
auto-build remains. No AI analysis runs on ordinary builds and no compiler or
protocol dependencies enter distributed applications.

The archived prototype is not the public contract. V0 adds lifecycle/version
rejection, stable command failures, build diagnostics, project check, and a user
driver to the existing diagnostic/check foundation. Impact, snapshots, risk,
queries and edits remain later releases. Within-revision identities and
cross-revision matching will be separate; impact distance and synthetic-node
presentation remain V1 decisions.

The machine-readable envelope and known payloads are defined in [toolchain-v1.schema.json](toolchain-v1.schema.json). Unknown additive fields and events remain permitted.
