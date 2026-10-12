# Native GC tracing capabilities

Register native payload hooks before publishing instances. `gc::willow_register_type`
remains compatible and selects `TraceMode::StopOnly`: its callback returns mutable
slot addresses for stopped marking and young-pointer relocation. It is never used
by concurrent marking. A legacy type can force a stopped recovery traversal of
other reachable native objects, so even safe types can accrue stopped calls.

To migrate, keep the stopped hook and provide a separate implementation through
`gc::willow_register_type_capability(id, stopped, capability)`:

| Capability | Required implementation |
| --- | --- |
| `NativeTraceCapability::StopOnly` | Existing stopped slot callback. |
| `NativeTraceCapability::ConcurrentSafe(snapshot)` | Copy child **values** using atomic loads or short synchronized critical sections. Do not return slot addresses or let references to protected storage escape the lock. |
| `NativeTraceCapability::SliceConcurrentSafe(slice)` | Copy at most the supplied limit, bound all work including empty slots, and return `Done`, a strictly advancing finite cursor, or `Retry` with no children when a lock is busy. |

These are Rust registration interfaces; their enums are not C ABI layouts. The
existing C GC statistics V1/V2 layouts and symbols remain unchanged. Embedders
using a Rust adapter can retain legacy registration until the adapter satisfies
the concurrent contract. There is no flag-only way to upgrade a callback.

All callbacks must avoid GC allocation, safepoints, GC reentry, user code, and long
critical sections. Appending to the supplied Rust vector may use the system
allocator. Every concurrently accessed slot needs compatible atomic accesses or
the same synchronization on both reader and writer. Publish reference mutations
through the GC write barrier before the store: snapshotting alone does not retain
edges removed after one slice or added outside an earlier snapshot.

For growing containers, use stable indexed storage and capture a finite bound for
the epoch. Never restart a mutable hash iterator or skip a shifting prefix. A
bounded slice may retry contention; prolonged retry still has an authoritative
stopped fallback. Retain a valid stopped hook for relocation and recovery even
when all ordinary marking uses slices. A small fixed payload can use the full
snapshot hook; unbounded containers should use slices.

Registrations install matched hooks under a common lock order; epoch snapshots
capture them together. A running epoch keeps its captured callbacks. Replacing
an existing legacy registration clears its concurrent hooks; unregistering clears
all three. Register during initialization, or before exposing a new type. Do not
replace a live type with a different layout, unload its code, or change its
synchronization contract while instances/epochs still use it. Native descriptors
also reject a concurrent hook without a stopped hook. When a native descriptor
contains both snapshot forms, slices take precedence.

## Inventory and diagnostics

`gc::native_trace_modes()` returns current `(type_id, TraceMode)` entries. It omits
bitmap-only and leaf payloads, which need no native trace callback. The result is
an inventory, not the capability snapshot of an already-running epoch.

`gc_telemetry::native_trace::snapshot()` returns cumulative per-type counters since
GC initialization, in unspecified order, omitting types with zero activity:

- `deferred_objects`: unique objects discovered with StopOnly capability during
  concurrent marking, summed across epochs. Repeated references do not recount
  an object within the epoch.
- `stopped_calls`: attempted native callback invocations during stopped major
  tracing, including safe callbacks retraced by recovery. Minor relocation is
  excluded. An unwinding callback still counts as an attempt.
- `stop_only_calls`: the subset registered as StopOnly when encountered by the
  stopped walk, including descendants first reached during that walk.

Counters saturate at `u64::MAX`. Deferrals become visible at bounded drain
completion; stopped calls publish when the walk finishes or unwinds. Active work
is not included until publication. GC initialization resets these counters, and
pre-reset batches cannot repopulate them. A deferred object and its later stopped
call describe two events, not two distinct objects. Inspect changes over a known
collection interval together with the existing Remark and Recovery stop totals;
a process-wide cumulative count alone does not measure pause time.

Run `cargo run -p willow_runtime --example gc_native_trace` for a complete legacy
versus atomic-snapshot example, including barrier publication and observed counts.

## Standard runtime types

The standard hooks already used concurrent implementations before this capability
API; this change preserves them and verifies their registration after reset.

| Type ID | Type | Effective mode | Synchronization/progress basis |
| --- | --- | --- | --- |
| `0xA22A0001` | Reference array buffer | SliceConcurrentSafe | Atomic published prefix and atomic slots; fixed buffer capacity bounds cursor; growth allocates a separate buffer. |
| `0xA22A0002` | Map | SliceConcurrentSafe | Try-lock traced storage, capture stable indexed value bound, and visit at most the limit. Scalar-only maps have no children. |
| `0xC4A20001` | Channel | SliceConcurrentSafe | Try-lock state; bounded indexed deque access; pop bookkeeping adjusts the remaining snapshot; appends do not extend it. |
| `0x53435001` | Task scope | SliceConcurrentSafe | State then root-arena lock order; try-lock retry; stable root slots and finite root/child bounds. Descendant scopes are separate objects. |
| `0x10C40001` | Async mutex | ConcurrentSafe | One atomic value load; immutable reference-kind flag. |
| `0x10C40002` | Async RW lock | ConcurrentSafe | One atomic value load; immutable reference-kind flag. |
| `0x10C40003` | Blocking cell | ConcurrentSafe | Copy one word under its value mutex; no user callback under that lock. |
| `0x10C40004` | Blocking RW cell | ConcurrentSafe | Copy one word under its value read lock; no user callback under that lock. |

Cancellation tokens and network handles are leaves with finalizers, not StopOnly
trace types. Generated objects/bitmaps use their existing field descriptors.
Legacy extension registrations remain the source of StopOnly deferrals. Declaring
a mode does not prove safety: audit actual stores, barriers, lock order, storage
lifetime, cursor stability, and progress, then test mutation during marking and
contention/recovery before migrating another type.
