# GC survivor copy implementation audit (2026-10-21)

## Provenance and scope

Baseline HEAD: `3df3d93`; initially clean tree. Ticket remains IN_PROGRESS for
review as requested. No staging, commit, push, mirror sync, or CI query.
Linux x86_64 native execution only. All four CI target Clippy checks passed;
Windows x86_64 and macOS arm64/x86_64 native execution remains unverified.

The ticket describes an older collector. Commit `6283396` already introduced
in-header forwarding, removal of per-survivor needs_scan/scanned tables,
and descriptor ownership transfer. These are prior work, not this session's
implementation. `raw_heap::initialize_moved_at` preserves a static descriptor
pointer without ownership, or transfers the single interned reference from
source to target. Source reclamation therefore neither acquires nor releases
that reference. Forwarded metadata resolves directly through one destination;
a destination cannot be a source during the same stopped collection.

This session presizes `MinorCollector::chunk_pages` for twice the source chunk
count before inserting pages, and adds a deterministic survivor/page-index test.
Each fixed-size chunk intersects at most two pages, including unaligned chunks.
Destination insertions may still grow the map. There is no persistent cache,
no high-water retention, no changed ABI, and no compiler/generated-code change.
Other small sets are proportional to pinned roots or concurrent-major edges;
reserving them for all survivors would waste memory and is unnecessary to the
accepted in-header-forwarding alternative.

## Validation and deterministic evidence

`verify.sh` ran fmt, 24 selected debug tests, one release scaling test, and
workspace/all-target Clippy with warnings denied for Linux x86_64, Windows MSVC
x86_64, macOS arm64, and macOS x86_64, in that order. `checks.txt` records successful
results. Full local test suites and full example audit were deliberately omitted.
`descriptor.txt` and `graph-counts.txt` preserve the unfiltered diagnostic output
of focused reruns, not additional broad test suites.

`scaling-release.txt`: 32 / 128 / 512 / 2048 survivors move exactly once, with
1 / 4 / 16 / 64 fragmented source chunks. Root and swept counts equal survivor
counts and all payloads retain their values. Capacity is at least twice the
source count; reconstructing the page index causes zero capacity growth at each
insertion. The old map's naturally grown capacity fails this bound at four
chunks in this fixture. Release collection elapsed ns per moved object are
750.78 / 121.40 / 60.11 / 56.38. These are single diagnostic timings, including
fixed collection overhead and excluding fixture creation/index reconstruction;
they are not a stable latency estimate or a timing-based complexity proof.

Existing descriptor tests execute 1 / 100 / 10000 moves while the descriptor
registry is exclusively locked. Completion proves the move and source reclamation
paths do not consult the registry. Exactly one live reference persists until
final target reclamation. Static descriptors retain their pointer and clear
mark/remembered/age flags. These tests validate prior implementation directly.

Coverage includes the following distinct perspectives: empty/invalid destination;
interned reference transfer; static descriptor preservation; final reference
release; forwarded-source metadata; generation/mark/age/remembered flags;
increasing survivor counts; fragmented source chunks; page capacity bound;
no index growth; payload preservation; chains; fan-out; shared child;
duplicate roots; survivor-to-old promotion; direct-root pinning; budget fallback;
major reclamation; remembered old-to-young cycles; exact source boundaries;
drop exactly once; invalid start bits; adaptive tenuring; concurrent snapshots;
SATB deletions; parked mutators; native async frames; post-snapshot allocation;
minor request coalescing; marker pause/resume; recovery-history relocation.

`stress.sh` checks 10,000-iteration object and array churn fixtures in both minor
and relocate modes (checksum 4471800). Full 10-million-iteration workloads are
used for normal performance measurements, not per-allocation stress. The existing
`example/array_literal_churn.wi` runs in normal, minor, and relocate modes with
all nine expected output lines checked. This includes reference arrays, nested
arrays, strings, returned literals, escaping rings, growth after tenuring, and
large buffers. `stress-checks.txt` records the seven successful executions.

## Complexity and affected paths

Let C be source chunks, S moved survivors, B copied bytes, R root/pinned slots,
and E traced reference edges. Initial page indexing has at most 2C insertions,
expected O(C) hash work, one initial allocation, and O(C) space. Reserving the
upper bound can use up to approximately twice the entry capacity required for
adjacent source chunks (plus the hash table's normal capacity rounding). This
avoids geometric growth/rehashing without raising asymptotic time or memory.
Zero chunks reserve zero slots. Saturating multiplication avoids arithmetic
wraparound; impractically large capacities still fail allocation as before.

Forwarding and descriptor transfer are O(1) per moved object; copying is O(B),
which is required to preserve bytes. Graph traversal is expected O(S + E + R),
with one work item per evacuated object rather than per incoming edge. Existing
chain/fanout/shared/duplicate-root tests at 32/128/512/1024 nodes assert exact
metadata, scanned-byte, moved, and tenure counts (see graph-counts.txt). Their
second cycle verifies promotion rather than assuming the first move suffices.
The per-collection source index is dropped after sweep, so total index build
work over M collections is O(sum(C_i)), without multiplying it by S or E.
Destination growth remains amortized linear in destination pages. Worklist and
forwarded-per-chunk storage remain O(S) and O(C); pinned/concurrent bookkeeping
retains its existing dimensions. No new scans, per-object registry operations,
code generation, build scheduling, or link caching are introduced.

Audited new/index_chunk/chunk_index, evacuation/scan_root_slot/scan_object,
survivor and promotion allocation, forwarding metadata access, sweep's fully
forwarded fast path, and descriptor final release. Source indices remain stable
until sweep; last_chunk range checks remain intact. This change is entirely
inside the stopped collector and introduces no new synchronization or platform
APIs. Hash-table lookup complexity is expected/amortized, not a worst-case
collision bound. Existing major-collection and region-allocation costs are not
claimed to disappear or to be globally optimal.

## Churn measurements and interpretation

Release, 8 workers, checksum 4994476800 in every full workload execution.
`before-churn.txt`, `after-churn.txt`, and `after-idle-churn.txt` preserve all
initial five-trial batches. The first after batch overlapped completion of
cross-target checks; a repeat after checks showed an object-churn slowdown, so
it was not discarded or assumed to be harmless noise. Before and after binaries
were then built separately with the same compiler and measured in alternating
order, seven trials each (`paired.py`, `paired.jsonl`). Only our presizing edit
was temporarily removed to build the before runtime, then restored byte-for-byte;
no Git history/index changes or unrelated edits were made.

| Batch median ms | object_churn | array_literal_churn |
| --- | ---: | ---: |
| Initial before (5) | 99.964 | 460.254 |
| Initial after (5) | 100.478 | 458.151 |
| After checks (5) | 108.992 | 463.222 |
| Paired before (7) | 103.796 | 462.324 |
| Paired after (7) | 103.259 | 462.053 |

Paired medians improve 0.52% and 0.06%; these small differences are within the
observed run spread and do not establish a statistically significant incremental
speedup. The deterministic benefit of this session is removal of source-index
growth, not a claim of a large new churn gain. Object before spans 98.809–110.157
ms, after 100.134–110.032 ms; array before 455.200–469.487 ms, after
458.274–465.209 ms. All samples are retained. The ticket's historical estimates
(188 ms object, 1178 ms array) are substantially slower than current execution,
but those are issue-reported observations, not a controlled same-session causal
baseline. Prior forwarding/descriptor improvements must not be attributed to
this tiny final edit.

Full object churn records 1,394 minors and 1,428,482 moved objects; array churn
records 4,883 minors and 10,002,434 moved objects in the paired runs. Counts,
allocated bytes, promotion counts and checksums are unchanged between variants.
Thus the throughput comparison does not gain speed by dropping work. Whole
workload elapsed/moved is not reported as collector ns/object because it also
includes mutator work; the focused collection measurement above separates it.

For benchmark reproduction, build `--release --bin willow`, then use
`target/release/willow build benches/go_vs_willow/sync/willow/<name>.wi -o
 target/gc-survivor-copy-<variant>-<name> --release` at each runtime revision and run
`rtk proxy python3 docs/audits/gc-survivor-copy/paired.py`. The existing local
benchmark harness is required for this exact timer. The checked-in reduced
fixtures reproduce correctness independently of that harness. Rebuild the
example with `target/release/willow build example/array_literal_churn.wi -o
 target/gc-survivor-copy-example --release`, execute with WILLOW_GC_STRESS set to
normal/minor/relocate into the matching example-*.txt, then run stress.sh.

No required implementation work remains known. Review approval and closure are
explicitly left to the next ticket-loop phase. Native foreign execution and CI
are unverified and are not used as completion blockers under the user's policy.
