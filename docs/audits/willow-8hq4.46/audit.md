# willow-8hq4.46 implementation audit — 2026-10-21

Status: implementation prepared for review; ticket intentionally remains open/in progress.
No staging, commit, push, CI query, or Beads mirror publication was performed.

## Cause and implementation

The native chunk loop called generic array get/set, checked panic three times,
prepared/restored generated context for every mapper call, and checked the
scheduler on every element. Its 256-element poll cap also forced frequent
requeue/claim operations. Disassembly subsequently exposed two per-element
cross-crate frame-layout calls, including a slot-count prefix scan. Finally,
fresh 32 MB output buffers were explicitly cleared after their containing region
had already been obtained from `alloc_zeroed`. A temporary allocation-only timer
observed 10–11 ms per buffer; the timer was removed from the implementation.

The final loop uses bounds established by disjoint chunk construction, scalar
loads/stores, one prepared generated-context bridge per poll, one immediate
panic check per mapper, and a local cursor published on suspension. It reloads
rooted handles after callbacks rather than retaining interior pointers across
allocation/collection. The pinned generated register is still installed at
every mapper boundary on Linux/Windows x86-64 and macOS x86-64/arm64.
Inline annotations let the constant frame layout fold away across the ABI crate.

Native map safepoints now cover batches of at most 64 elements. Each batch
charges one native safepoint and settles mapper-generated safepoint debt;
flags and the time deadline are checked every batch. The hard poll cap is
65,536 elements, a full default budget of 1,024 batches. This deliberately
changes budget accounting from one native safepoint per element: budget=1 now
maps at most 64 cheap elements before yielding. It does **not** change ordered
results, at-most-one-child-per-worker, task cancellation results, panic handling,
or cleanup. Arbitrarily slow individual mappers still depend on their own
safepoints, as before. No constant wall-clock cancellation bound is claimed.

`OldRegion::pristine` skips only the first redundant clear of allocator-zeroed
storage. Object allocation, both promotion entry paths, and LAB carving consume
this state. Sweep and tail trimming never restore it. Every later ordinary
allocation still clears its full span, including reclaimed holes and a tail
trimmed back to offset zero. No uninitialized-array API or GC tracing exception
was introduced. Runtime ownership stays in the runtime; compiler runtime
auto-build, linking and project scheduling were not changed.

## Measurements

The original benchmark file was absent and the current harness did not support
`parallel_map`. The checked-in inputs here reconstruct the public ticket shape:
4M scalar inputs, three xorshift steps, a low-16-bit mask, five awaited maps,
eight workers, and checksum **after** the end marker. They are not represented
as recovered historical sources. Go uses eight goroutines over disjoint ranges;
Java uses eight platform threads. Each runtime allocates a result per round.
All results check checksum 131070000000. Java has no added JIT warmup.

`before.jsonl` contains five-trial local baseline measurements at the original
HEAD, before runtime changes (Willow and Go). `final.jsonl` records final raw
trials, CPU/OS/toolchains, source hashes, runtime diff hash, medians, and a
separately compiled **one-round** single-worker probe. The harness times stdout
markers, excluding startup and checksum, so pipe-delivery jitter remains.
The baseline and final use the same workload; harness additions supply Java,
the one-round probe and additional metadata.

| Runtime | Workers | Rounds | Median ms | ns/element |
|---|---:|---:|---:|---:|
| willow | 8 | 5 | 27.404339 | 1.370217 |
| go | 8 | 5 | 11.723241 | 0.586162 |
| java | 8 | 5 | 82.248635 | 4.112432 |
| willow | 1 | 5 | 97.186647 | 4.859332 |
| go | 1 | 5 | 57.703702 | 2.885185 |
| java | 1 | 5 | 76.510666 | 3.825533 |
| willow | 1 | 1 | 18.223443 | 4.555861 |

Acceptance: the **entire** eight-worker phase costs 1.370 ns/element,
so its overhead above a nonnegative mapper cost is conservatively below 2 ns.
Willow / fastest runtime (Go) = **2.338x**, below 5x.
One round on one worker = **18.223 ms**, below 40 ms.
The local pre-change five-round baseline was 364.460 ms with eight workers
and 703.665 ms with one worker. These are separate from the historical
Windows values in the ticket.

These are Linux x86-64 results on Ryzen 7 7800X3D, the reference CPU model.
They do not establish Windows 11 performance or reproduce the historical Go
1.27.2/OpenJDK 27 toolchains. Foreign native execution and CI evidence are not
required by the user's clarification. No foreign execution is claimed.

## Safety, complexity, and measured scaling

Independent dimensions: input length N, active workers W, round count R, mapper
work M(i), and GC regions/objects. C <= min(N, W) chunk tasks plus one coordinator
are created; chunk ranges are disjoint and cover N elements. Exactly N mapper
calls, N scalar reads and N scalar writes occur per completed map. The loop is
Theta(N + sum M(i)); it cannot avoid inspecting and producing each element.
Output storage is Theta(N), task/frame/native state is O(C), and additional loop
state is O(1) per chunk. R sequential calls cost Theta(RN + total mapper work),
with allocation/GC costs accounted for separately. No per-element allocation,
write barrier, task, context preparation or generated loop duplication remains.

For a non-safepointing mapper and no time/flag interruption, a poll maps at most
Q=min(64*budget, 65536) elements. Total polls are sum ceil(chunk_length/Q),
or O(N/Q + C); mapper safepoints and preemption requests can increase this count.
The unchanged coordinator scans C children per wake (O(C^2) in the worst case
of C separate completion wakes); it does not multiply N. Chunk-count and waiter
registration tests cover that existing scheduling boundary.

`scaling.txt` records actual mapper-call and poll counts for 17 increasing and
boundary lengths, through 131,073, at four budgets (68 combinations). It asserts
exactly N mapper calls, the expected poll count, every output value, and released
state. The pending-flag test proves observation within one 64-element batch and
idempotent cancellation cleanup. `examples-codegen.jsonl` records 661 bytes of
native poll code and one emitted mapper-call site for N=64, 1024, 65536 and 4M.
`poll-chunk.txt` retains the inspected disassembly: no generic array or layout
calls remain in the element loop. These are deterministic counts, not scaling
claims inferred solely from elapsed time.

`zero-counts.txt` records zero explicit clear bytes on the first allocation and
exactly the aligned object span on reuse for six payload sizes in regular and
large regions. Metadata overhead is one boolean per region; the decision is O(1).
The OS/allocator must still supply zeroed storage, and writing N results remains
Theta(N). This removes a redundant serial pass; it does not claim all allocation
or page-fault work is constant-time. LAB/reuse/promotion checks include dirty
storage, middle holes, and trimming a formerly populated region to zero.

## Explicit test perspectives

The existing focused integration tests cover these 20 LIR perspectives:
ordered named mapper; contextual lambda; empty/singleton; sync task-returning
helper; output remapping; repeated same input; concurrent maps joined out of
order; async array return; collection after result; rooted temporary input;
cancellation without partial result; TaskScope ownership; task in a class field;
match-arm task creation; high worker count with budget=1; result length/indexing;
one mapper reused; nested helper call; mapper panic; fully-LIR example selection.

Additional coverage includes rejection of mutable inputs, wrong signatures and
captures; nested concurrent scheduler maps; allocation stress; high worker count;
full-width signed i64 values across debug/release, 1/4 workers and ordinary,
allocation and relocation GC; pinned-register preservation; nested contexts;
bridge helper restoration and panic-depth replacement; exact chunk bounds and
coverage; one-time waiter registration; deterministic batch/poll scaling;
pending preemption; pristine/reused/LAB/promoted region zero initialization.

## Reproduction and validation

Run from repository root:

```sh
node benches/runtime_compare/harness.js run parallel_map --trials 5
python3 docs/audits/willow-8hq4.46/verify.py
cargo test --test integration parallel_ -- --nocapture
cargo test -p willow_runtime parallel_ -- --nocapture
cargo test -p willow_runtime generated_context::tests -- --nocapture
cargo test -p willow_runtime zero_initialization_tests -- --nocapture
cargo test -p willow_runtime test_gc_old_region_ -- --nocapture
cargo test -p willow_runtime test_gc_large_object_uses_dedicated_region_and_stays_non_moving -- --nocapture
cargo test -p willow_runtime promotion_cursor_rekeys_regions_instead_of_each_object -- --nocapture
cargo test -p willow_abi native_frame_layout_includes_header_and_shifts_reference_mask -- --nocapture
```

`verify.py` runs only the four ticket examples, with 1/8 workers, budget=1 and
relocation stress (12 runs), checks exact outputs, and inspects Linux x86-64
code generation. The context integration test additionally runs debug and
release with allocation/relocation stress. `validation.txt` records final test
counts, fmt and Clippy outcomes. Full local suites and the full example audit
were deliberately omitted. Linux x86-64 was executed; Windows x86-64 and both
macOS targets were Clippy-checked only, with native execution unverified.

Limitations: benchmark timings include output allocation, scheduling, GC and
marker transport; they are not isolated arithmetic instruction timings.
Scalability with arbitrary mapper effects is bounded by those effects and the
scheduler, not by this scalar-loop experiment. Independent review is pending.
