# Survivor storage and churn performance recovery

2026-10-26; Linux x86_64, Ryzen 7 7800X3D, 8 workers, five trials per case.
Starting revision: `8e62d8a2a219d7c1ad23c27cabce40591451b08d`, clean tree.
Final measurements describe the uncommitted working tree. Machine/toolchain and
historical archive identities are in `provenance.json`. No unrelated changes
were present. No ticket closure, staging, commit, push, mirror sync or CI lookup
was performed.

## Cause and isolated evidence

The existing benchmark harness and current compiler were used with explicit
historical runtime archives. Each revision was rebuilt in its own target
directory; the four archive hashes differ. Only runtime differences are isolated,
so these are not claims to reproduce every historical compiler/environment detail.
Median phase milliseconds:

| Runtime | array_grow_only | array_literal_churn | object_churn |
|---|---:|---:|---:|
| parent of 4ca55d1 | 26.645 | 112.400 | 66.013 |
| 4ca55d1 | 24.886 | 129.441 | 71.681 |
| parent of 39db9a7 | 24.514 | 128.092 | 71.407 |
| 39db9a7 | 51.653 | 129.293 | 71.781 |
| starting tree, standard harness | 18.722 | 132.685 | 72.372 |

`4ca55d1` introduced pre-stop survivor admission: one whole destination chunk per
non-pinned source, including zero-filled storage and metadata for destinations
that mostly remain unused in churn. Admission remains necessary to keep storage
allocation outside the stopped interval. The redundant zero-fill is removed here.
`39db9a7` added `configured_count()` to repeated pacer input sampling, repeatedly
performing OS CPU discovery. That array-growth cost was already addressed by
`a5c21e3`; this session does not claim that existing fix as new work. Its separate
before/after syscall evidence is in `../gc-mark-worker-cache.md`.
The isolated historical comparisons identify introducing commits, while this
change and deterministic zero-fill counts identify the avoidable work addressed.
They do not attribute every nanosecond of either historical delta to one function.

The first exploratory candidate run had literal samples 129.37, 129.00, 129.92,
120.95, 117.99 ms (median 129.00). It is retained in `candidate-probe.jsonl` rather
than discarded. Both subsequent standard-harness confirmation runs are stable
and below the original threshold; no competing validation builds ran during them.
Initial attempts that inadvertently reused the same historical Cargo output were
invalidated and replaced by the independently rebuilt archive measurements.

## Change and safety audit

- `region.rs` retains zero-filled storage for ordinary nursery/old allocations.
  Only `allocate_survivor_storage` uses uninitialized allocation. Both paths share
  the existing allocation-failure injection and attempt counter. A const generic
  selects the allocator at compile time; no dynamic per-object allocation mode.
- `minor.rs` still admits all destination storage, both bitmaps and header-offset
  capacity before requesting the stop. Reservation accounting, hard-cap checks,
  refill admission, failure fallback, release timing and allocation layout agree
  with the previous implementation. No retained cache or new memory policy.
- `raw_heap.rs::initialize_moved_at` writes a complete header with `ptr::write`,
  avoiding a reference to an uninitialized header containing bool fields. All
  seven logical fields are initialized; padding is never interpreted as data.
  Static/interned descriptor ownership still transfers exactly once.
- `allocate_survivor` advances `used` and publishes start bits under exclusive
  collection ownership; `forward` then copies the entire payload before forwarding
  or publishing the object to concurrent marking. Stopped marker coordination
  excludes access to the intermediate header-only destination.
- `index_retired_chunk`, minor sweep and concurrent start-bit lookup only inspect
  initialized starts within the used prefix. Unused reserve blocks have no heap
  index entry; unused tails are never scanned as headers. Release only deallocates
  unused storage using its original layout. Fallback retains original sources.
- Compiler IR, generated code, runtime ABI and runtime auto-build are unchanged.

## Complexity and deterministic evidence

Let C be admitted source chunks (including R racing refills), K fixed chunk bytes,
L live bytes copied, and O live objects. Previously admission requested clearing
C*K storage bytes, even if L was tiny. It now requests zero such bytes; each moved
header/payload is initialized once in O(L + O), which is required to relocate it.
Allocator behavior and OS page faults are platform dependent; this is a reduction
in requested initialization work, not a promise that malloc has constant latency.

Metadata admission remains O(C*K/granule) bitmap initialization plus O(C) allocations,
with O(C*K) reserved space and header-index capacity; K is fixed, so admission is
still O(C) in source count. This patch does not claim to reduce that asymptotic
bound. Full pre-admission conservatively permits high survival without allocating
under the stop. No per-object scans were added to admission, no extra per-exit or
per-caller work is multiplied into collection. Across M collections the removed
storage writes total sum(C_i*K); copied bytes total sum(L_i). Memory reservation,
compiler complexity and generated-code size are unchanged; two fixed allocator
specializations add only constant code size.

`scaling.txt` records C=1,16,128: exactly C reserve storage attempts, zero reserve
zero-filled bytes, zero stopped storage allocations. The test poisons every
reserved byte with 0xa5, then verifies all relocated payload values, copy counts,
reserve ownership and release accounting. Existing increasing-size minor page-index
and concurrent relocation tests passed. Descriptor-transfer tests cover 1,100,
10000 moves with one registry acquisition/release. Wall-clock measurements alone
are not used as asymptotic evidence. No avoidable repeated scans or allocations
were introduced; this change specifically removes redundant storage initialization.

## Acceptance measurements

Standard harness, median phase milliseconds; lower values are improvements.
Thresholds retain the original historical baselines and +10% tolerance.

| Case | Baseline | Upper bound | Run 1 | Run 2 |
|---|---:|---:|---:|---:|
| array_grow_only | 31.9 | 35.09 | 18.621 | 18.650 |
| array_literal_churn | 116.6 | 128.26 | 120.958 | 121.369 |
| object_churn | 66.6 | 73.26 | 68.864 | 68.735 |

Scheduler retention against 2026-10-25, same parameters and worker count:

| Case | Baseline | Current | Change |
|---|---:|---:|---:|
| yield_switch (100000,100) | 527.573 | 517.686 | -1.9% |
| gc_scheduler (10000,100) | 84.889 | 79.021 | -6.9% |
| spawn_join_tree (10,5) | 123.693 | 117.922 | -4.7% |

Raw files include Go controls, every trial, CPU/RSS and available GC counters.
Literal/object churn retain 207/204 minor collections, zero major collections,
425986/209922 moved objects and 1025/513 promotions. No workload/output criteria
were relaxed. Selected original baseline rows are included for reproducibility.

## Validation and test perspectives

Passed `cargo fmt --all -- --check` and
`cargo clippy --workspace --all-targets --target TARGET -- -D warnings` for
x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc, aarch64-apple-darwin and
x86_64-apple-darwin. Focused runtime tests, 21 total, passed:

```sh
cargo test -p willow_runtime --lib gc::minor::tests::m4_ -- --nocapture
cargo test -p willow_runtime --lib survivor_storage_failure_preserves_reservation_accounting
cargo test -p willow_runtime --lib moving_header -- --nocapture
cargo test -p willow_runtime --lib gc::minor::overlap_tests::
cargo test -p willow_runtime --lib minor_survivor_cost_and_page_index_scale
cargo test -p willow_runtime --lib droppable_nursery_chunks_are_walked_and_drop_exactly_once
```

Explicit perspectives: (1) pre-stop accounting, (2) no stopped storage allocation,
(3) poisoned headers, (4) poisoned payload destination, (5) unused poisoned tails,
(6) increasing source counts, (7) empty reserve fallback, (8) hard-cap fallback,
(9) racing refill admission, (10) unwind release, (11) actual allocation failure,
(12) stop-component accounting, (13) static descriptors, (14) interned descriptor
transfer, (15) exactly-once drops, (16) graph/shared-child relocation, (17) snapshot-only
roots, (18) container/async-frame edges, (19) old descendants and SATB deletions,
(20) parked mutators, (21) marker pause/resume, (22) coalesced minor requests,
(23) post-snapshot allocation, (24) native retry relocation, (25) page-index scaling.
The existing `example/array_literal_churn.wi` was built in release mode and run
normally and with `WILLOW_GC_STRESS=relocate`; both produced exactly:
`50005000, 43, 6, alphabetagamma, 10, 5050, 2555840, 87, 1999000` (one per line).
An initial incorrectly qualified moving-header test filter matched zero tests;
it was corrected to `moving_header`, which executed and passed both tests.

Only Linux x86_64 was executed. Windows and both macOS architectures were
cross-linted, not linked/run. CI was not checked. Full workspace/crate/integration
tests and the full runnable-example audit were deliberately not run.

## Reproduction

Build the current release compiler and use the existing harness:

```sh
python3 benches/go_vs_willow/run_sync.py array_grow_only array_literal_churn object_churn --languages willow,go --trials 5 --workers 8
python3 benches/go_vs_willow/run_sync.py array_grow_only array_literal_churn object_churn --languages willow,go --trials 5 --workers 8 --no-build
python3 benches/go_vs_willow/run.py yield_switch 100000 100 --languages willow,go --trials 5 --workers 8
python3 benches/go_vs_willow/run.py gc_scheduler 10000 100 --languages willow,go --trials 5 --workers 8 --no-build
python3 benches/go_vs_willow/run.py spawn_join_tree 10 5 --languages willow,go --trials 5 --workers 8 --no-build
```

For each historical revision in provenance.json, extract only `Cargo.toml`,
`Cargo.lock`, `crates`, `src`, and `benches/compiler_bench.rs` using `git archive`.
Build `cargo build --release -p willow_runtime` with a distinct CARGO_TARGET_DIR
for that revision, preserving the root release profile. Then run:

```sh
python3 docs/audits/gc-churn-recovery/measure-runtime.py LABEL /absolute/path/to/libwillow_runtime.a
```

The helper uses current benchmark inputs and checks expected results via the
existing runner. Preserve JSON lines from harness stdout (its trailing CSV/summary
is presentation only). Finish builds before timed samples. Binaries and full
build logs stay outside published evidence. No private specifications or internal
identifiers are included in these new artifacts.
