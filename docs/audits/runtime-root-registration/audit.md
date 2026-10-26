# Runtime address-root registration audit

2026-10-26. Baseline: `b175b8a07f458ddbc73b9e7e81eadb5298ba5e02`.
Scope: GC address-root registration used by scheduler spawn; no compiler, ABI,
build scheduling, or generated-code changes.

## Implementation and lifetime review

Inactive marking no longer takes `GcRuntime::heap` in address-root registration.
A shared-read TLAB extent index checks a published exact payload start and reads
its generation. The existing fatal diagnostic for young address roots remains.
Nulls return immediately; old-region and foreign addresses do not require a
header dereference. Invalid-root diagnostics remain collection-time behavior.

The index shares the existing atomic start bitmaps rather than duplicating
per-object metadata. Nursery installation publishes an extent before its TLAB
is exposed. Survivor installation publishes it before copying objects. Retirement
and in-place promotion need no index rebuild: generation is read from the
published header. Minor release, both major sweep implementations, and reset
remove the extent before releasing storage. Unused survivor reserves are never
indexed. Writers take heap then extent write lock; readers release the extent
read lock before acquiring heap for an active marking cycle. There is no reverse
nested lock order. Publication has no safepoint, and its existing no-preemption
guard and collector handshake exclude a generation-changing stop.

Active marking still enqueues the root under the heap lock before publication.
Deletion retains its SATB barrier. A new test drains an active cycle to prove
that registration, without any other root publication, marks its referent.

Registration after nursery activation adds one deduplicated pending owner
under the existing root shard lock. The pending set is independent of the live
root index: unregistering the last root does **not** cancel initial-child
remembering. An old owner may remain reachable through an old-to-old parent
edge, which creates no remembered-set entry itself. Minor collection drains
pending owners under the heap lock, validates managed old storage, and remembers
it before scanning children. Subsequent stores still use the old-to-young
barrier. Pre-nursery registrations need no pending entry, as before. A deferred
minor may move pending work into the remembered set without losing it.

Pending addresses are not strong roots and do not extend object lifetimes.
Both stopped sweep paths (old regions and pinned TLAB objects) and the shared
bounded-sweep reclaim path erase pending metadata for a dead old object under
the heap lock before reclaiming its storage. Thus reclaimed addresses cannot
leave scan obligations for a later allocation. Minor reclamation only frees
young objects, which cannot enter this set. Quiescent reset clears all pending
metadata. A foreign pending address is never dereferenced without managed-old
validation at drain. Major snapshots/counts enumerate only live registrations,
not pending obligations. Sweep and minor use heap -> root shard ordering;
registration never holds a root shard while acquiring heap, so no reversed
nested lock order is introduced.

The review regression was reproduced before repair with the assertion
`transferred owner's initial child was reclaimed`. The regression initializes
old A with young Y, registers A, barrier-stores A into a rooted old P, removes
A's root, and collects. It now passes both with and without an intervening major
collection. Additional tests explicitly exercise stopped and bounded reclamation
of old-region and pinned-TLAB owners, verify pending records do not retain dead
objects, and verify fresh registration after reclamation. The old cancellation
claim in this audit was incorrect and is superseded by this ownership rule.

## Complexity audit

Independent dimensions: registrations N, live roots L, pending distinct owners P,
TLAB extents R, machine word width b, worker count W, and child edges E.

- Generation lookup uses one expected-O(1) hash lookup and checks at most two
  extent candidates, followed by one start-bit read. Each TLAB is exactly one
  index page long and may straddle two pages; disjoint equal-size extents give
  at most two candidates in any page. No object/region scan. Insert/remove
  updates at most two hash entries per chunk, not per object. Existing bitmap
  storage is shared. Live metadata is O(R); hash capacity can retain its peak
  until reset. This removes the initial radix version's depth-dependent work
  from both spawn and collector extent updates.
- Root add/remove performs expected O(1) hash/index operations under one of 32
  existing shard mutexes. A separate dense pending vector and reverse index
  use O(P) live metadata (retaining peak capacity); the root index remains O(L).
  The separate index is necessary because P can include owners absent from L.
  Re-registration before processing deduplicates even after the last root was
  removed. Both root removal and pending reclamation use swap removal with one
  reverse-index update, without scans or tombstone walks.
- Reclaiming an old object adds one address-shard lock and expected O(1) pending
  erase to the existing per-object sweep work. Young-object reclamation skips
  it. For D reclaimed old objects, this adds O(D) work and no extra heap pass;
  arbitrary sparse address reuse cannot accumulate reclaimed pending records.
- Each minor drains O(32 + P) entries, independently of L and retained hash
  capacity; managed-owner lookup is word-bounded plus a bounded region-local
  allocation lookup. Tracing adds O(E) for remembered live child edges. Each new
  pending entry is processed at most once unless the owner is registered again;
  unchanged suspended tasks do not acquire a per-minor scan cost. Across N
  registrations, total pending work is O(N), not O(N times minor count).
- All live new owners may contain unbarriered initial young edges, so processing
  each such owner once is necessary without a stronger caller contract. Work
  formerly performed at registration moves to the next minor. This trades
  bounded pending metadata for avoiding a process-wide heap mutex per spawn.
- Workers still share an extent reader lock/cache line, can wait for chunk
  writers, and can collide on root shards. This is not a lock-free claim. Active
  marking still uses the heap lock. The measured futex improvement is a workload
  result, not a universal contention bound.
- Compiler time and generated-code size are unchanged. No duplicated generated
  cleanup, branch, module, or exit paths were introduced.

Deterministic evidence is in `focused-tests.txt`: 64/256/1024/4096 fragmented
extents, 128/512/2048/8192 boundary queries, at most two candidate visits per query; duplicate
registrations, sparse root removal, and sparse reclamation drain exactly
36/144/576/2304 outstanding obligations (including unrooted owners), and zero
on a repeated drain. Root snapshots still visit only 4/16/64/256 live roots. Existing tests with 1,000/10,000/100,000
persistent frames scan only one changed owner, then zero unchanged owners.
These counters, rather than elapsed time, support the scaling claims.

## Validation perspectives

Focused coverage includes null roots, duplicate owners, balanced removals,
concurrent root ownership, major snapshots, young-address fatal rejection,
relocatable root slots, initial young children, persistent old roots, repeated
minor scans, pending deduplication, ownership transfer, pending reclamation,
re-registration,
fragmented extents, exact/gap/unaligned addresses, active nursery, retired
nursery, copied survivors, pinned old survivors, dead nursery release, major
survivor release, reset, active-mark enqueue, activation handshake, racing
collection, spawn initializer reentry, initializer panic rollback, and native
stack migration after moving collection. The held-heap registration test proves
inactive registration completes without acquiring that mutex.

All commands/results are recorded in `focused-tests.txt` and `validation.txt`.
The selected runtime-root filter includes one intentionally ignored explicit
stress test; it was not enabled. No local full test suite or full example audit
was run. The two benchmark programs are compiled and executed native Linux
release programs with checked result counts. Linux x86_64 native focused tests
pass. Workspace/all-target Clippy passes for Linux x86_64, Windows MSVC x86_64,
macOS arm64, and macOS x86_64. Foreign native execution/linking was not performed;
no CI result was queried or inferred.

A new test initially assumed a shadow-rooted young object would be pinned; the
collector correctly relocated it. The test now checks relocation separately
from the existing pinned-survivor test. Its first failing run left test-owned
root storage stale and caused a later test-process abort. Corrected focused
runs pass; this was a test-fixture lifetime error, not a production workaround.

## Performance reproduction

`measure.py BIN_DIR OUTPUT.jsonl --trials 101` imports the checked-in benchmark
`run.py` and uses its unchanged `one_run` sampler, overriding only executable
selection. Inputs are `spawn_join_tree 10 5` and `gc_scheduler 1000 1000`, eight
workers. Variant order alternates for timings; three separate `strace -f -c -e
trace=futex` runs per variant count whole-process syscalls. The tree has 111,111
tasks including its root; the harness reports 111,110 spawned operations.

Build the baseline runtime from the baseline tracked tree in an isolated
scratch directory with `cargo build --release -p willow_runtime`. Build the
working runtime with the same command. Use the same `target/release/willow`
compiler for each benchmark source and pass `--release --runtime-lib` with each
variant's `libwillow_runtime.a`. Name binaries `{benchmark}-before` and
`{benchmark}-after` in BIN_DIR. Binaries and scratch build artifacts are not
included. Measurement ran after all compiling/testing finished, on Linux x86_64
with Rust 1.96.0. Raw timings/counters are retained, including the initial
15-pair and 51-pair runs of an intermediate implementation with a separate
pending hash index. Their short GC medians were 0.90% and 0.16% higher,
respectively. The initial audit removed the separate pending index, but review
proved that pending obligations must outlive root entries; the repaired version
restores an independent index with different removal semantics.
`measurements.jsonl` is the repaired implementation. Intermediate samples remain available as
`measurements-initial.jsonl` and `measurements-pending-map.jsonl`. The radix
version with deduplicated pending metadata is retained in `measurements-short.jsonl`
(short GC: +0.42%) and `measurements-radix-long.jsonl` (long GC: +1.46%). The
final bounded-candidate index replaces that radix extent lookup/update work.
Its 31-pair pilot is retained in `measurements-page-pilot.jsonl` (long GC:
+0.15%); `measurements-before-review.jsonl` retains the previous 101-pair run
with the cancellation bug. Those samples are historical, not validation of the
repaired implementation.

### Repaired implementation results

101 alternating-order pairs using the unchanged sampler:

| Benchmark | Before median ms | Repaired median ms | Change |
| --- | ---: | ---: | ---: |
| spawn_join_tree 10 5 | 119.698913 | 97.532774 | -18.52% |
| gc_scheduler 1000 1000 | 65.584130 | 65.625157 | +0.063% |

The GC point estimate is 0.041027 ms higher; it is not evidence of a speedup
or proof of exactly zero regression. Its interquartile ranges are
64.711789–67.216231 ms before and 64.334144–66.974780 ms repaired.
A paired percentile bootstrap (Python `random.Random(0)`, 10,000 resamples,
101 trial indices sampled with replacement per resample, ratio of resampled
medians minus one, sorted indices 249 and 9749) gives a 95% interval of
-1.255% to +0.849%. Thus no measurable GC regression was detected at this
sample resolution; sub-percent effects remain unresolved. The same calculation
for spawn gives -19.111% to -17.883%.

Whole-process futex counts were 34,208 / 36,735 / 36,708 before and
31,632 / 32,391 / 32,698 repaired. Median calls fell from 36,708 to 32,391
(-11.76%), or 0.330372 to 0.291519 per task. Relative to the historical
41,873 median reference, repaired calls fell 22.64%. Every repaired count was
below every baseline count. Result counts were checked for every timed run.

## Repair audit outcome

The ownership-transfer regression failed before the repair and now passes both
with and without an intervening major collection. Explicit stopped and bounded
major-sweep tests cover pending-record retirement for both old-region and
pinned-TLAB storage, followed by replacement registration. The final focused
selection passed 33 tests (one separate stress case intentionally ignored).
The explicit reclamation-path test was rerun after its final test-only change.
Formatting and all four target Clippy gates passed after that change.

Pending scans now survive root removal, are deduplicated independently, and are
retired before managed storage reuse. Major reclamation adds one expected-O(1)
shard lookup/removal for each dead old object; that cost is inferred from the
implementation and was not separately timed in a major-heavy benchmark. The
retained benchmark evidence measures the complete repaired implementation.
No foreign native execution, full local suite, full example audit, or CI check
was performed. Changes remain available for independent review.

