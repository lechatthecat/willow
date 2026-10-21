# Stage 6 bounded recovery audit — 2026-10-21

Baseline: `449ce62`. Ticket `willow-6fv.5.6` remains claimed/open for review;
no staging, commit, push, closure, new ticket, CI polling or mirror sync.
This audit supersedes the earlier partial-sweep audit and unanswered API-choice
notes. The recorded user decision A authorizes migration of native callbacks.

## Implementation and safety

Concurrent recovery no longer enumerates the heap, rescans roots, calls
`mark_worklist`, or prepares every region in one global stop. It retains the
original epoch and SATB barrier, repeatedly seals/transfers pending publications,
and uses `prepare_bounded` for old-LAB retirement and region publication. Each
Recovery stop admits at most 256 work slots and 16 KiB of accounted metadata;
clock admission is checked against 1 ms. These fixed recovery limits are
independent of deliberately tiny normal-remark settings. They are deterministic
operation/byte bounds, not a hard scheduling or wall-clock latency guarantee.
The initial root snapshot and deletion/insertion barriers preserve reachability
across resumes; no authoritative full-graph restart discards SATB discoveries.

Graph work drains after resume in at most 256 queue items per collector wave
with a 1 ms deadline. Existing object/bitmap/native continuations retain progress.
A native slice receives at most 512 slots; output count, strictly advancing cursor
with delta at most the limit, and empty Retry output are checked. Callback CPU
work remains an unsafe implementation contract; arbitrary native code cannot be
preempted safely. No callback executes under a concurrent recovery stop. Busy
native locks return Retry and give mutators another opportunity to release them.
Recovery now permits at most 127 consecutive Retry results at one object/cursor.
The 128th is a fatal contract violation before reclamation. Perpetual Retry cannot
leave collection looping forever. Successful cursor progress clears its history;
work on unrelated objects does not reset the allowance. Minor relocation moves
retry history with queued continuations, so copying cannot renew the allowance.

Recovery grants graph consumption to the collector, ends a batch after Retry,
and waits 1 ms with mutators resumed before retrying. Pending graph work drains
without repeated global Recovery stops. In-flight readers finish their bounded
work before relinquishing consumption. A transient lock owner can run between
attempts; callbacks must resolve within the finite policy or be rejected. OS
scheduling and unsafe callbacks that never return remain outside an attempt bound.

All standard native types now provide slices. The previously fixed-size async
mutex/RW lock use bounded atomic snapshots; blocking cells use try-lock/try-read.
A live native type with only StopOnly or ConcurrentSafe is rejected before
reclamation. Callback panic or invalid progress cannot trigger unrestricted
stopped fallback. The Rust API example and affected tests were migrated. The
explicit feature-off baseline retains stopped tracing and minor relocation still
uses stopped slot hooks; these are not concurrent recovery. Native tracing
compatibility and migration are documented in `crates/willow_runtime/NATIVE_TRACING.md`.

Closure requires drained SATB/queue and zero active readers. Failure/deferred
state is rechecked after acquiring the final reader's completion, before sweep
publication. Disabled background workers use the elected collector's existing
queue reader rather than a stopped graph tracer. Partial region publication
retains quarantine; old-LAB retirement precedes each continuation, so resumed
allocations cannot use stale clear flags. Sweep and exactly-once destructor work
run after resume through the shared resumable sweep plan.

Cancellation also seals and retires with fixed-budget stops, reverses partial
region flags with a persistent cursor, and disables the epoch only after readers
leave. Queue discard and old-region header clearing occur after resume under
collector serialization. Generated TLAB headers are not enumerated: they can be
active or intentionally unindexed, and concurrent marking uses epoch bitmaps.
Old header marks from pending-region allocations are cleared so a later
feature-off cycle cannot skip children. An already-started partial sweep is not
recoverable and terminates rather than pretending to cancel reclamation.
The generated-TLAB invalid-root regression also exposed a debug registration
path that dereferenced an invalid pointer in remembered-set publication before
collection could diagnose it. Debug registration now skips remembering invalid
pointers; collection retains the existing diagnostic/unwind behavior.

## Registry progress

`Pending<K>` is a removable FIFO implemented with hash-indexed predecessor/next
links. SATB owner queues and the old-LAB pending queue process only remaining
work. Completed owners are removed; reactivation appends at the tail. Sharded
SATB sealing preserves both shard index and shard-admission state across stops.
Unregistration removes its key; reopening starts a fresh traversal and lazily
re-enrolls active producer buffers. Old-LAB prune/swap updates pending indices.
Registry locks and existing epoch/ownership checks serialize these changes.

Divisible SATB transfers use the entries that fit the remaining byte/slot budget
instead of requiring a whole 32-entry chunk. Tests include a 65-entry buffer with
one entry admitted per stop. Progress requires space for at least one indivisible
unit: budgets below that minimum return incomplete without losing work. The
fixed Recovery allowance admits these units, so tiny normal settings no longer
force an unbounded fallback. Continuous mutator growth can increase total work;
completed prefixes are not revisited simply because a stop ended.

## Deterministic scaling evidence

Run `bash docs/audits/willow-6fv.5.6/reproduce.sh`. Measured on Linux x86_64 debug.
The recovery fixture starts with an undrained rooted native chain, equal garbage,
buffered old allocations and one large garbage region per 16 chain nodes. Every
callback and destructor asserts the stop gate is clear. Each stop records and
asserts its own admitted slots, bytes, region publications and metadata objects.
A second collection proves complete, exactly-once cleanup after root removal.

| Chain nodes | Recovery stops | Maximum slots/stop | Maximum bytes/stop | Maximum metadata objects/stop | Published regions |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 16 | 2 | 69 | 1108 | 32 | 2 |
| 256 | 6 | 255 | 4048 | 126 | 17 |
| 4096 | 24 | 256 | 5916 | 126 | 257 |

All rows have zero stopped graph tracing, native callbacks and swept objects.
Counts of stops can vary with scheduling; per-stop operation bounds do not.
Some metadata was indexed during allocation refill before recovery, hence the
largest fixture's recovery metadata total is smaller than its total heap size.
A separate existing 1 GiB fixed-root fixture exercises thousands of regions and
checks bounded publication through multiple classified Recovery stops.

Small-budget registry fixtures put all SATB owners in one shard, use at most
one delivered entry per stop, and assert eventual completion and linear aggregate
budget checks. Owner churn, unregister/re-register, reopen, old-LAB refill fairness
and prune/swap cases assert no loss or duplication.

| Owners | Legacy SATB checks | Sharded SATB checks | Old-LAB checks |
| ---: | ---: | ---: | ---: |
| 1 | 4 | 110 | 4 |
| 16 | 94 | 199 | 64 |
| 257 | 1540 | 1645 | 1028 |

The sharded totals include the fixed 64-shard traversal. Legacy rounds are
1/16/257; sharded rounds 22/37/278; old-LAB rounds 1/16/257.

## Retry termination regression (follow-up repair)

The former cap regression released contention before entering Recovery and did
not prove completion policy. It now enters with Retry still active for 1/16/257
objects, releases only after two resumed waits, and asserts no intervening Recovery
stop. Reachability and eventual cleanup still hold. A subprocess keeps Retry
permanent after exhausting the normal 128 drain waves. It records exactly 128
recovery callback attempts, 127 waits and one Recovery stop, then requires the
fatal non-progress diagnostic. Neither reachable nor unreachable destructor probes
may execute. The regression would fail if recovery returned or reclaimed anything.

`overlap_native_recovery_retry_history_follows_minor_relocation` forces a young
native object through two minor moves and verifies the same retry history advances
from 1 to 2 to 3 rather than restarting; successful tracing clears the history,
preserves its child, and subsequent unrooted collection reclaims the graph.

## Complexity and boundaries

Independent dimensions are pending owners M, SATB entries S, heap objects H,
reachable edges E, regions G, queue items Q and retry attempts T. Pending FIFO
operations are expected O(1), use O(M) memory and retain peak hash capacity.
Draining finite registries is O(M + S + T + fixed shards), without O(M) prefix
rescans on each retry. Old-LAB indexing visits each pending object once; ordered
allocation-index insertion retains its existing logarithmic cost. Preexisting
refill owner lookup and setup costs are not included in the drain measurements.

Concurrent recovery does not add an O(H) metadata/graph rediscovery pass.
Tracing retains the existing O(H + E) visitation lower bound and indexed lookup
costs; native contention contributes T. Recovery bounds T per cursor to 128 attempts;
retry bookkeeping has expected O(1) update cost and O(contended objects) storage.
Minor transports that history in O(contended objects), alongside its existing
queue transport. Successful graph work retains bounded batches rather than
forcing one allocation/reader registration per edge. Region publication visits each appended
region once (plus existing candidate-tree lookup cost), with O(1) cursor state.
Completed publication is not restarted. Sweep retains its existing bounded cursor
and quarantine storage. Cancellation may perform O(H) old-header cleanup and
O(Q) queue discard after resume; neither is hidden inside a global stop.
Its rollback visits each region once with existing O(log G) candidate insertion,
for O(G log G) aggregate rollback work, divided among bounded stops.

Per-stop slots count logical admitted operations, not hash-table implementation
steps or allocator/OS latency. Rendezvous waits, scheduler delays, lock acquisition
and a native implementation violating its unsafe contract are not hard real-time
bounds. Initial handshakes, minor collection and explicit feature-off stopped
collection retain their existing scope. No compiler/build-tool boundary changed.

## Validation

- `cargo fmt --all -- --check` and `git diff --check`.
- Workspace/all-target Clippy with `-D warnings`: Linux x86_64, Windows x86_64
  MSVC, macOS ARM64 and macOS x86_64.
- Focused runtime filters: `old_cycle_` (12 including child runs), `satb_`
  (20 passed, one throughput benchmark ignored), `recovery_sweep_` (3 selected
  plus child), `registry_resume` (4), `bounded_retirement` (4).
- Native capability tests (9), native typeinfo tests (10), deterministic blocking
  cell contention (1), reference telemetry (1), randomized reference oracle (1).
- Named tests cover rejected legacy/full-snapshot and panicking callbacks;
  disabled background workers; assist/remark overlap; registry changes during
  background marking; edge publication; new TLABs during closure; invalid root
  cancellation with generated TLABs; partial-publication cancellation; finite Retry
  rejection and transient recovery; retry-history transport through minor moves; runtime
  address/slot root retention, young-address rejection and racing registration.
- Existing named old-LAB, buffer allocation, unregister, publication-unwind and
  legacy remark cases were revalidated during registry repair.
- `gc_native_trace` Rust example and exact integration
  `runtime::examples::test_gc_old_cycle_example` passed.

Native execution was Linux only. No foreign native execution, CI result, full
local suite or full example audit is claimed. Full-suite validation remains in
CI. Performance throughput was not remeasured. Review approval and ticket closure
remain outstanding by user instruction.

## Coordinator fixture follow-up

Review found two automatic-collection tests still registering an unrestricted
native snapshot callback. Their shared leaf callback now uses the public
`SliceConcurrentSafe` registration and returns `Retry` until the test releases
it, then `Done`. Each call performs two atomic operations at most, emits no
children, allocates nothing and does not wait. Time and auxiliary space per
callback are O(1); waiting belongs to the collector's existing bounded retry
protocol. The stopped leaf hook remains available for minor/feature-off paths.

The original assertions are preserved: allocations resume while tracing is
pending, automatic requests coalesce, an automatic request survives losing an
explicit collector election, the subsequent automatic cycle runs, and final
collection reclaims the root. Both named tests passed independently on Linux.
The reproduction script includes both cases. This repair changes test fixtures
only; production coordinator scheduling and collection code are unchanged.

Formatting and diff whitespace checks passed after this repair. Workspace/all-target
Clippy with `-D warnings` passed again for all four targets listed above.
