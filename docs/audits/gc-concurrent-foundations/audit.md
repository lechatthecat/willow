# GC M0 implementation audit — 2026-10-18

Baseline: c1888af. The initial working tree and index were clean. This change
reconstructs the previously suspended M0 implementation from its public-source
patches, adapts it to the current collector, and validates the current tree.
Historical execution logs are not evidence for this tree. Private text is not
reproduced here. Identifier mappings below point exclusively to public source
and tests; they are an implementation inventory, not a copy of a specification.
The ticket remains open for review; nothing is staged, committed or pushed.

## Architecture and ownership

The runtime owns GC in `crates/willow_runtime/src/gc.rs` and its `gc/` modules.
The compiler supplies the layouts and store destination tags from
`crates/willow_abi/src/lib.rs`, using `src/backend/cranelift/gc_codegen.rs`.
There is no new compiler analysis or build scheduling/caching layer.

Mutators publish generated-context shadow roots, parked native-stack slots,
runtime address roots and writable runtime root handles. `gc/root.rs`,
`gc/runtime_roots.rs`, `gc/root_arena.rs`, `generated_context.rs` and
`gc/safepoint.rs` own these respective boundaries. Major activation takes a
stopped root snapshot in `gc/root_handshake.rs::begin`; mutators subsequently
publish SATB/insertion work through `gc/barrier.rs` and `gc/marking.rs`.
Marker workers register before tracing; a stopped fallback remains authoritative
for legacy callbacks and failures. `gc/mark_closure.rs` establishes closure;
`gc/sweep/batches.rs` performs bounded concurrent sweep and native drops.
`gc/sweep.rs::execute` preserves the explicitly classified stopped fallback.

A nested minor in `gc/minor.rs` runs under the outer collector's election.
It rewrites supported slots, retains raw runtime-stack referents by deferral or
pinning, remaps the major mark state and resumes the parent. Runtime root handles
use atomic replacement with a barrier before each successful replacement;
interior references use a scoped root/pin lease. `gc/minor_deferral_tests.rs`
covers the current debt/fallback behavior. Major old objects do not relocate.

Unregistered native threads cannot safely publish arbitrary TLS roots into a
running collector. `gc/root.rs::foreign_root_stack_owner_active_locked` and
`gc/root_handshake.rs::begin` reject the unsafe collection boundary;
`gc/progress.rs` reports skips. Native entry uses `NativeGcScope` /
`MutatorRegistration`, and persistent references use registered handles/slots.
Address roots must be old; young address registration fails explicitly.
`gc/foreign_owner_tests.rs` tests ownership transfer, unwind, and registration
races. This policy is shared across supported targets, not inferred from a
single-thread process or a Linux-only optimization.

Task frames have separate ownership: `scheduler.rs::take_quiescent_frame_unroots`
and `release_quiescent_frame_roots` respect active execution epochs before
removing scheduler roots. `frame_lifetime.rs` separates terminal, root release,
metadata removal, quarantine, drop, and reclaimed counters. A task becoming
terminal is not evidence that a GC cycle has reclaimed its frame.

## Reference stores: strategies, tests and exemptions

All destination IDs below come from `GcStoreDestination` in `willow_abi`.
The centralized runtime barrier preserves the overwritten edge while marking,
publishes the new edge where required, and remembers old→young owners. It does
not safepoint between the barrier and the store. The native test measures only
the mutation inside an active epoch, excluding setup. Every ID is tested in
open and sealed marking by `m0_all_destination_barriers_publish_deletions_in_open_and_sealed_marking`.
Generated execution is checked by `barrier_proof.py` against actual V3 counters,
not merely by finding a call name in generated text.

| ID / location | Store path or exemption | Focused evidence |
|---|---|---|
| 1 ObjectField | `gc_codegen.rs::emit_gc_heap_store`, object/class stores | Generated barrier example; all-destination test |
| 2 ArrayElement | Compiler and `array.rs` element set/pop | Generated example; native overwrite/removal test |
| 3 MapValue | `map.rs` insert/replace, retained native map storage | Generated map example; native replace test |
| 4 EnumPayload | Compiler enum construction and `rust_bridge/aggregate.rs` | Generated wide enum with >64 slots; all-destination test |
| 5 InterfaceObject | Compiler stores object word; method table is not a GC edge | Generated interface example; interface_pair_stores_trace_only_object_word_in_linear_code |
| 6 AsyncFrameSlot | `cranelift/mod.rs` spill/reload, `native_frame.rs` slot stores | Await example; native frame overwrite test |
| 7 IndirectReference | Compiler lvalue store through interior slot; no fabricated owner header | Generated indirect mutation; all-destination test |
| 8 GlobalStatic | `cranelift/compile.rs` static stores; atomic `GcRootHandle::store` | Generated static example; native root-handle release test |
| 9 ContainerInternal | `array.rs` backing-store replacement and runtime container edges | Native resize test; all-destination test |
| 10 AsyncMutexCell | `async_mutex.rs` and ABI commit path | Generated async mutex; native commit test |
| 11 AsyncRwLockCell | `async_rwlock.rs` and ABI commit path | Generated async rwlock; native commit test |
| 12 BlockingCell | `lock.rs` protected word store | Generated/native cell tests |
| 13 BlockingRwCell | `lock.rs` protected word write | Generated/native rwcell tests |
| Logical deletion | `channel.rs` receive, runtime address-root removal, `satb_delete` | Native mutation test at SATB capacities 1 and 256 |
| Atomic pointer CAS | `gc/root_arena.rs::GcRootCell::replace` invokes barrier for each observed old value before CAS; retries preserve deletion safety | Root-handle tests and native release mutation |
| Scalar atomics/CAS | `atomic.rs` i64/bool and `async_frame.rs` status words have no managed-reference layout | Written exemption; scalar_stores_skip_the_write_barrier |
| Null/scalar initialization | Zero-filled unpublished allocation and non-reference slots have no old GC edge | Written exemption; randomized scalar-shaped-pointer negative cases |
| Collector relocation writes | `gc/minor.rs` rewrites slots under stopped mutators and remaps concurrent state | Written exemption from mutator barrier; moving stress + minor tests |
| Specialized layouts | Inline masks, extended bitmaps, interface object word, closure environment, registered legacy/concurrent trace hooks | Randomized independent oracle includes native callbacks and extended bitmap slots; wide enum proof |

Reference-bearing root registries use their registration/deletion protocols;
they are not exempt from publication merely because their storage is native.
Primitive atomic exemptions do not apply to an arbitrary pointer-shaped word.

## Lock hierarchy and phase boundaries

The election path takes coordination then tries collector serialization; it
never blocks a mutator on serialization while the collector needs its roots.
Collector release takes coordination before clearing election. Stopped callbacks
may take heap under coordination. Heap metadata may acquire mark phase/work/index
locks; mark queue guards are not retained across heap locks, tracing callbacks,
root publication or safepoints (`gc_mark_queue` assertions enforce that rule).
Native container trace callbacks snapshot under their own locks without a heap
lock; callbacks may not allocate GC objects or park. Drop work in concurrent
sweep executes outside heap holds. Root registry snapshots release their own
shard locks before graph traversal. These are GC call-path ordering rules, not a
claim that every unrelated runtime lock has a single total ordering.

New edges are telemetry→milestones (begin/abort), heap→milestones (mark/sweep),
coordination→milestones (outer completion), worker stats→atomic busy gauge, and
trace→milestone lookup / measurement snapshot. Milestones never calls heap,
coordination or trace while holding its mutex. Observer registration is checked
before locking milestones. Failure diagnostics take independent nonblocking
scalar snapshots, holding no sampled lock while sampling the next. No new reverse
blocking edge was found. Ordinary trace output follows world resumption; a
nested minor can retain the parent's election, but does not retain a heap/stop
lock during that output. Existing opt-in profiling behavior is unchanged.

## Baseline verification: six source observations

| Audit entry | Current source evidence and finding |
|---|---|
| 1 | MarkWork measures graph work; sweep plans/batches reclaim objects separately. V1 start/end alone could not identify these boundaries. This change adds epoch-tagged mark/sweep/quiescence events at both normal and fallback paths. |
| 2 | Scheduler retirement already uses active execution epochs and delayed unrooting. Frame counters and cycle completion describe different ownership events. M0 exposes them together without collapsing the events. |
| 3 | Concurrent sweep and allocations invalidate before−after as a freed-byte estimate. Major finish already accepts actual sweep bytes; M0 adds candidate/quarantine/reusable gauges and completed reclamation, with a real between-region test. |
| 4 | Registered stack roots are snapshotted at a stop on every target; root publication is not replaced by a Linux-only assumption. Foreign-owner rejection and native-stack tests exercise the common semantics; foreign native execution is not claimed. |
| 5 | `native_memory.rs` charges retained native owner capacity. Managed occupied/reserved alone are not total process memory. The public counter contract distinguishes these estimates and exclusions. |
| 6 | Scheduler terminal/root-release/drop/quarantine counters already exist, but public cycle completion and structured failed assertions were absent. The new wait API and bounded diagnostics make tests identify the actual state instead of sleeping for it. |

## Audited identifier inventory

The following is a source-evidence index, not normative text. Rows state what
was inspected and how the implementation is exercised. The M0 work does not
assert a proof of all possible user/FFI programs.

| Identifier | Public implementation evidence and validation |
|---|---|
| GC-I1 | `gc/marking.rs`, `raw_heap` tracing metadata, inline/extended layouts and registered native callbacks; independent randomized identity comparison |
| GC-I2 | `gc/barrier.rs`, `gc/root_handshake.rs`, atomic start/mark publication; open/sealed destination tests and compiler old-load-before-store tests |
| GC-I3 | `gc/root.rs` parked/transferred roots, `generated_context.rs`, `native_stack.rs`, scheduler runtime slots; moving generated example and parked-root focused test |
| GC-I4 | `gc/minor.rs` raw-root pin/deferral paths and root-handle leases; minor pin/debt tests |
| GC-I5 | `gc/sweep/batches.rs` drop actions outside heap holds, `gc/sweep.rs` stopped compatibility drops, `failure.rs` terminal failure boundary; allocation failure tests |
| GC-I6 | `gc/root_arena.rs` atomic pointer replacement, typed native frame stores and runtime ABI; native mutation and all-destination cases |
| GC-I7 | Foreign-owner checks, registration and progress diagnostics; current foreign-owner focused tests |
| GC-I8 | Scheduler execution/retire epochs and root-handle ownership, `gc/minor.rs` relocation remapping; frame counters and moving stress |
| GC-I9 | `scheduler.rs::take_quiescent_frame_unroots`, poll/execution epoch accounting and `frame_lifetime.rs`; frame states sampled separately from GC stages |
| GC-I10 | Scheduler releases eligible roots through `release_quiescent_frame_roots`; explicit GC invokes that boundary before collection; live locals remain retained |
| GC-I11 | SATB deletion plus insertion publication in `gc/barrier.rs` and mark closure; overwritten native subgraphs and randomized heaps |
| GC-I12 | Store inventory above, all 13 tags, scalar and collector-write exemptions, actual generated/native counters |
| GC-I13 | Registry and root publication/removal under `gc/root.rs` and stopped handshake; foreign-entry and root-handle cases |
| GC-I14 | `gc/mark_closure.rs`, SATB shard sealing, termination guard and queue drain protocol; open/sealed barrier tests and closure tests |
| GC-I15 | Public typed layout/store ABI, checked payload validation and fatal FFI boundary; invalid-state diagnostics and scalar-store exemption tests; no new unchecked compiler operation |
| GC-I16 | Monotonic Cycle identities, bounded history, measurement groups, pending/reclaimed/reusable distinction; milestone, ABI and telemetry tests |
| GC-I17 | `gc/minor.rs` root slots/values, remembered owners and native roots; extended layouts, moving stress and pin tests |
| GC-I18 | `gc/marking.rs` current-epoch allocation publication and `gc/old_lab.rs`; concurrent_edge_publication_and_new_allocation_survive_remark |
| GC-I19 | Current `gc/minor.rs` service/defer policy and `minor_deferral_tests.rs`; bounded debt with major sweep, pinned fallback and unchanged allocation thresholds |
| GC-I20 | `gc/root_arena.rs` root/pin handles and native-stack raw-pointer handling; lease cleanup and moving/pinned focused tests; V3 survivor fields retain pin-reason attribution |

## Q1 / Q4 / Q11 source answers

Q1: The normal major path is concurrent marking followed by bounded concurrent
sweeping (`try_collect_internal`, `mark_closure::finish`, `sweep::batches::execute`).
Initial/remark coordination and classified Recovery are stops. Disabled/failed
workers or legacy callbacks use stopped graph tracing, and minor evacuation is
stopped. `gc_collect` cooperates with current election and does not mean merely
queueing an asynchronous request; its reentry/foreign-owner qualifications are
preserved. Sweep completion and allocator reuse are separate boundaries.

Q4: Native task execution and generated async frames are both represented by
registered roots, but raw native-stack aliases constrain relocation. Supported
native-stack paths are cfg-selected in `native_stack.rs` and `failure.rs` for
Linux GNU x86_64/aarch64, macOS x86_64/aarch64 and Windows MSVC x86_64; the CI
matrix's four targets are checked by Clippy. Scheduler worker/execution epochs
protect frame ownership regardless of marker worker count. Marker workers have
their own registration and CPU/busy gauges. A Linux run does not establish
foreign native-stack execution. Frame allocation counters do not include all
native stack or process overhead.

Q11: Current coverage is executable: all 13 runtime destination tags in two mark
phases, native overwrite/deletion/resize/commit paths at two SATB capacities,
generated reference stores under all/moving stress with region/barrier checking,
and a >64-slot enum. Counter assertions surround actual native mutations so setup
cannot satisfy them. Scalar atomic/status words are explicitly exempt; atomic
GC root-handle replacement is covered as GlobalStatic rather than misclassified
as a scalar exemption. The oracle compares collector retention against an
independent adjacency traversal, including cycles, DAGs, chains, repeated roots,
scalar-shaped values, extended bitmaps, and native callbacks.

## Implementation complexity and audit corrections

Independent dimensions: cycle count C, bounded observer history H=64, wakeups W,
heap nodes V, edges E, roots R, old regions B, chunks T, free holes F, timing spans
P, worker jobs J, verification stores D, and snapshots K. New stage publication
and lookup cost O(H) per operation with H fixed; storage O(H) plus at most two
active identities. A long major cannot lose its record to nested minors.
Snapshot memory work is O(B+T), not O(F): FreeSpans caches byte totals in ordinary
and incremental builders and updates them during take/take_up_to. K snapshots
therefore add O(K(B+T)) after existing LAB reconciliation. Timing costs O(P+J+C)
and opt-in destination counters O(D). Generated compiler code is unchanged.

The oracle is O(V+E+R) time / O(V) auxiliary memory, marking on enqueue and
building one retained-address set after collection rather than rescanning the
heap for every object. Deterministic tests cover V=1,8,64,512,4096 and fan-out
1,4,16, asserting exactly V×fan-out edge visits. Incremental free-span tests cover
0..4096 fragmented holes, assert linear builder-step bounds, and compare cached
bytes after every partial/full consumption. These are operation counts, not
wall-clock claims about collector optimality. An arbitrary graph requires
visiting reachable edges; the source/identity comparison also visits V nodes.

Recovery audit found changes since the historical implementation: nested minors,
new bounded sweep batches and next-epoch reuse quarantine, incremental free-span
construction, atomic root-handle replacement, and changed frame/survivor C field
order. Each was adapted rather than reverting current behavior. Legacy fallback
CPU is attributed to collector CPU as well as total mark work to avoid double
counting in profile output. Current survivor debt/retry fields are sampled from
the live policy state. Invalid-pointer and remembered-set failures emit JSON;
null/unavailable fields are explicit and threshold categories use the decision's
own sample. No GC timing sleep or threshold relaxation was used as a fix.

## Validation record

`reproduce.py` selects M0/milestone/telemetry tests, named affected-path tests,
format, host/four-target Clippy, one native example, generated barrier examples,
C/Rust ABI comparison and parsed failure JSON. It never selects the complete
workspace/crate/integration suite or full example audit. The randomized seed is
7,276,289, with four shapes × four sizes × eight rounds per configuration.
Both WILLOW_WORKERS and WILLOW_GC_MARK_THREADS are fixed to 1/2/4/8; test threads
are 1/4. Increasing-shape operation counts are printed by named tests.

Explicit perspectives include empty/rooted cycles; minor completion; monotonic
reset; actual during-sweep stages; election reentry; ordered timestamps;
duplicate/out-of-order stage calls; abort; timeout; invalid/expired tickets;
registered-observer rejection; spurious wakes; bounded history; >64 nested minors;
parent restoration after child abort; unaligned ABI; short/null/unknown ABI
requests; unchanged V1/V2 layouts; C field offsets; distinct trace events; broken
trace sink; randomized cycles/DAGs/chains/fanout; duplicate/absent roots; scalar
negative edges; native callbacks; extended bitmap; native SATB capacity 1/256;
all 13 destinations in both phases; actual generated moving/all/minor execution;
fragmented free-byte accounting; real panic/invariant/threshold/locked-heap JSON;
Unicode escaping; u64 maximum values; and unavailable CPU/heap samples.

Final executed results follow.
Full local suites, foreign native execution/linking and CI polling are excluded
by policy. Cross-target Clippy is compilation/lint evidence only.

### Final current-tree results

- Formatting and host workspace/all-target Clippy passed. Cross-target Clippy
  passed for x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc,
  aarch64-apple-darwin and x86_64-apple-darwin.
- All eight worker/test-thread configurations passed with region/barrier verify
  explicitly enabled: 9 M0 cases and 10 milestone-filter cases each. The oracle
  checks 128 randomized heaps per configuration, using seed 7,276,289.
- Telemetry regression: 45 cases passed. Free-span tests: 4 passed. The named
  native SATB, foreign-owner, failure-policy, minor pin/debt, scoped-handle,
  concurrent allocation and compiler store-ordering checks passed. Explicit
  collection cooperation and unwind/completion-generation tests also passed.
- The native gc_milestones example and generated moving/all/wide-enum programs
  passed. Moving trace counters for destination IDs 1..13 were
  [8,27,8,7,8,125,8,9,3,9,9,9,9]; the wide-enum case executed EnumPayload and
  verified retention through extended layout slots. Counts can vary with
  scheduling; the harness requires actual nonzero execution of each relevant ID.
- C/Rust ABI agrees: V1 2112 bytes, V2 6920, V3 7640. V3 offsets:
  milestones 6936, memory 6976, survivors 7056, frames 7224, measurements 7400.
  Nested frame running/terminal-event offsets are 136/16; survivor pin_reasons
  offset is 88. This caught and corrected the historical field-order mismatch.
- 72 actual/boundary failure JSON records parsed across the eight configurations;
  maximum 4005 bytes including newline, below the 8192-byte bound. Tests include
  actual invalid-pointer diagnostics, fatal invariant, ordinary panic, threshold
  failure, locked heap, maximum integers and worst-case control-character strings.
- Deterministic oracle visits equal V×fan-out (maximum 65,536 here). Cached
  free-byte accounting was checked through 4096 fragmented holes with a linear
  incremental-builder bound and two consumption samples per hole.

Linux x86_64 is the native execution platform. Windows x86_64 and both macOS
architectures have passing cross-target Clippy only; native execution/link/run
is unexecuted. Full local suites and the complete example audit were omitted;
CI was not queried. No acceptance criterion was changed, no new ticket was
created, and this ticket is not closed or publication-approved by this report.

Reproduction logs and per-command environment manifests remain local/ignored in
this directory. Only this audit, the public telemetry contract, and five exact
reproduction-source paths are allowed through the docs ignore rules. Private
paths and all unrelated documentation remain ignored; the index is empty.

## Review repair: captured thresholds and recovered markers (2026-10-18)

The current repair addresses the two subsequent findings; the earlier gate above
records the preceding implementation, not a rerun of all that evidence.

Source audit: `gc/native_memory_tests.rs` now routes the existing 225 KiB managed,
16 MiB reservation and 64 MiB RSS strict bounds through
`gc_telemetry/failure_record.rs::assert_sample_below`. No bound was increased or
made inclusive. The managed assertion and its categories use one held-lock
sample. Churn retains the sample that produced the reservation maximum instead
of resampling after collection. RSS actual bytes come from VmHWM; GC categories
are contemporaneous and cannot attribute the historical whole-process peak.
The held-lock forced-failure subprocess disables the panic hook and checks exact
actual/limit/managed bytes, nonzero external bytes (123), old reservation (262144)
and young reservation (0). Equality fails, preserving strict comparison.

`gc/marking.rs::ConcurrentCycle::drain_recovering` now emits bounded `marker_panic`
JSON from the caught payload before returning to recovery. Native slice budget,
nonadvancing cursor and partial retry invariants each produce a record with the
panic hook disabled in an isolated subprocess. Each sets worker_failed, increments
MarkerPanic once, and suppresses subsequent drains. Existing worker (1 and 5),
collector and assist panic cleanup tests also pass, including queue/active-drain
retirement and worker-pool reuse. The reporter borrows the payload reason and
uses existing try-lock snapshots; it adds no GC lock wait or graph traversal.

Complexity audit: threshold comparison and captured sample storage are O(1) time
and space per sample; retaining the peak sample does not add scans or change the
churn loop's iteration count. Failure formatting has fixed 8192-byte storage and
bounded reason length. Metadata samples and failure work are constant in heap
size; stderr latency remains externally dependent. The existing TLAB accounting
merge cost is unchanged. Failure handling occurs only for a caught panic; normal
marking gains no additional scan, allocation or per-edge work. These are source
bounds, not timing-based scaling claims. No new input-dependent algorithm needs
an additional scaling benchmark.

Executed repair validation:

- `cargo fmt --all -- --check` and host workspace/all-target Clippy passed.
- Workspace/all-target Clippy passed on x86_64-unknown-linux-gnu,
  x86_64-pc-windows-msvc, aarch64-apple-darwin and x86_64-apple-darwin.
- Both new named M0 regressions passed with workers/mark threads 1,2,4,8,
  test threads 1,4, and region/barrier verify enabled (16 filtered invocations).
- Existing unchanged-reservation, string-churn/reservation/RSS, and marker
  worker/collector/assist panic tests passed in focused invocations.
- Python parsed 104 records from the prior gate and new repair matrix combined
  (32 new records). Maximum remains 4005 bytes including newline versus 8192.
  New managed-sample accounting and recovered-marker records parse correctly.

Reproduction: the two tests use the `m0_` filter already in `reproduce.py`; the
three existing regressions are now named in that harness. Raw repair logs use
`review-` prefixes and remain ignored. Native execution was Linux x86_64 only;
foreign native execution, full suites and CI checks were not performed. No ticket
closure, staging, commit or push occurred. Review is still required.
