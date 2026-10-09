# Willow toolchain protocol V0 (schema version 1)

## Direct semantic commands

For interactive work, use `willow refs order::Order::qty`, `willow symbol
order::Order::qty`, `willow type src/order.wi:18:12`, `willow effects
order::submit`, `willow impact order::submit`, or `willow rename book::min
smaller`. These commands resolve names through compiler semantics in one process;
no saved snapshot, request file, manual identity or byte offset is required.
`references` aliases `refs`. Project discovery searches for `project.toml`;
use `--project-dir DIR` or `--source FILE` to select an explicit analysis scope.

Selectors are qualified/unqualified semantic names or file:line:column (one-based
Unicode scalar columns). Ambiguity returns reusable candidate selectors without
choosing a candidate. `--kind`, `--module` and `--package` narrow selection.
Invalid positions fail rather than being clamped. Display paths are project-relative;
`--absolute-paths` requests absolute paths for read queries. Human output is the
default, with coverage and truncation visible. References show at most 50 entries;
`--all` removes the limit, `--show-id` exposes identities and `--explain` retains
detailed evidence. `--format json` returns one `{schema,kind,status,result}`
object; `--format ndjson` writes that same object on one line. Exit codes are
0 for success, 1 for resolution/analysis failures and 2 for invalid arguments.
This direct-command envelope is separate from the event-stream protocol below.

Rename uses compiler-owned structured edits, checks its source/configuration
revision, validates a candidate, and applies under the existing workspace lock.
`--dry-run` only returns the prospective diff. Failed direct apply rolls back
files it actually published, preserving differing concurrent edits. An OS-level
rollback failure reports the recovery transaction. Ordinary filesystem readers
may observe intermediate files; cross-file isolation is for cooperating clients.
The transaction ID is hidden on success unless `--verbose` is set. Unsupported
coverage or ambiguous names fail without a text-replacement fallback.

The existing `query --requests`, `impact --function/--revision` and `edit`
protocols remain available for batch/external integrations.

## Event-stream protocol

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
Each diagnostic also carries `cascade` (bool) and `root_cause` (the `seq` of the
diagnostic it likely follows from, or null). A request's diagnostics are written
roots first, then cascades, so fixing the roots and re-checking is the intended
loop. Cascade marking is a heuristic: later errors in a file after its first
lexer/parser error (E005x/E010x) cascade from that error, and a type error
(E02xx) on the line of an earlier name-resolution error (E035x) cascades from it.
Warnings are never cascades.

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
driver to the existing diagnostic/check foundation. Later protocol revisions
add impact, queries and structured edits; their contracts are
documented below and in the linked protocol documents.

CLI query and impact payloads store the workspace path once in `workspace` and
set `path_encoding: "workspace-placeholder-v1"`. Paths under that workspace use
`${workspace}/rest`. Opaque IDs are never rewritten. Rust callers can use
`willow_compiler::ai::expand_output_paths` to expand response paths. Position
queries and `impact --file` accept the returned `${workspace}` paths. Library
query results retain absolute paths. If a payload already contains a literal
`${workspace}` prefix, compaction is skipped to preserve unambiguous decoding.

The machine-readable envelope and known payloads are defined in [toolchain-v1.schema.json](toolchain-v1.schema.json). Unknown additive fields and events remain permitted.

Agent instruction generation, capability flags and managed sync are documented
in [AGENT_INSTRUCTIONS.md](AGENT_INSTRUCTIONS.md).

## Advanced integration

Structured editing is documented in [STRUCTURED_EDITS.md](STRUCTURED_EDITS.md),
including isolation and recovery boundaries. Each query invocation analyzes the
current source and returns its revision. Re-query after changing source or configuration.

The former `snapshot` commands, file-based `risk`, and `daemon` have been removed.
There is no replacement baseline file format. Use direct semantic commands for
current-source facts and `query --requests` for batched integrations. Existing
saved files are left untouched; this CLI no longer loads or manages them.

V2 query batches use [query-v2.schema.json](query-v2.schema.json). `symbols`
lists compiler-resolved declarations. It accepts optional AND-combined filters
`name` (exact), `prefix`, `module` (logical module path) and `symbol_kind`, plus
`limit`; the result adds `total` (matches before `limit`) and `truncated`. Prefer
filtered requests: an unfiltered list grows with every binding and parameter.
`symbol-at` takes a file and byte offset.

Typed results keep the exact compiler tree in `ty` (a flat node array rooted at
index 0; child indices are traversal order, not parameter order) and add
`type_display`, a source-like spelling such as `fn(shipping::Parcel) -> i64`.
Its names are canonical across modules: the root package's types are
`module::Name`, a dependency's are `package::module::Name`, and a bare name
declared or item-imported as a type in the declaring module is replaced by the
target's canonical name. Builtins and type parameters keep their spelling.
`type_display` appears on declarations in `symbols`, `symbol-at`, `symbol-info`
and on `type-at` results. Use it for reading; compare types by `ty`.
`symbol-info` and `references` accept either a callable ID or a declaration ID
in the historical `function` field. Source declarations and references include
bindings, parameters, types, type parameters, fields, enum variants, imports,
modules, methods and constructors. Builtins have no source declaration location.
Reference results include value/import uses and possible virtual dispatch;
signature-compatible indirect calls remain unresolved candidates, not resolved
references. `type-at` includes checked declaration types and expression types.
Successful query envelopes carry `revision` and `result`; an invalid revision
returns top-level `status: stale` before lookup. Unknown positions/identities,
ambiguous source positions and incomplete evidence remain distinct.

`effects` preserves compiler capability bits and supplies `effect_evidence`
for each set bit, with a source witness or explicit missing evidence. Compiler
facts, runtime capability bounds and external boundaries are distinguished.
A missing witness produces `incomplete`; unknown callees produce `unknown`.
These are capability facts, not proof of external business writes.

Package-aware analysis adds `identity: {package, module, symbol}` to callable,
source-symbol, reference and impact-node results. `package` contains `name`,
`version`, `revision` (null for live path packages), and `source`. Logical module
paths and package identities do not include consumer dependency aliases. Existing
opaque IDs and source locations remain revision-scoped. Non-callable members use
owner-qualified symbols (for example `Box::field:value` and
`Choice::type-parameter:T`). Parameters and bindings additionally carry their
declaration token's byte offset (`first::parameter:x@42`); this distinguishes
shadowing, sibling scopes and lambda bindings within a named owner. These local
identities are revision-scoped and can change when source offsets change.
References carry the same identity as their target declaration. Single-file/legacy
analysis without a PackageGraph reports null identities. Impact includes
`external_dependencies` with `from`, `target`, and `relation: "callee"` for
cross-package calls from discovered nodes (including outgoing boundary edges).

Use package commands as the planning interface:

1. Run `willow update --dry-run --format json` (or `willow add ... --format json`).
2. Put its complete report object into a V2 request's `delta` field:
   `{"kind":"affected","delta":<report>,"tests":["<test FunctionId>"]}`.
3. Run `willow query --requests requests.json` in the project. As with other
   query batches, requests.json is an array; omitted revision binds to this
   invocation.
4. Use the returned modules/symbols/tests to select checks. Review the plan before
   applying the package mutation; the query itself never edits manifests or locks.

`affected` matches full before/after package identities from direct and transitive
changes, ignoring alias-only changes. It returns a conservative reverse import
closure and all declarations within that closure, including type-only consumers.
The indexed graph is limited to modules loaded from the selected entry point.
`tests` is an explicit caller-supplied list of test FunctionIds: no naming
convention or unloaded test discovery is assumed. Unknown test IDs produce
`incomplete`; unmatched package identities are returned explicitly (a planned
new revision will normally be unmatched in a pre-update analysis revision). Empty change
sets return empty results. Invalid report schema/kind/success state returns
`invalid-delta`; stale revisions retain the existing top-level `stale` response.
`module_visits` and `edge_visits` expose closure work per request, excluding the
one-time session index construction and JSON output serialization.

## Rust bridge queries and repair

See [Rust bridge query, update impact, and repair](RUST_BRIDGE.md) for standalone
and batch queries, external dependency boundaries, read-only update previews,
and the combined Cargo/declaration/caller diagnostic workflow.
