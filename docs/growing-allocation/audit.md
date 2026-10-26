# Growing runtime allocation: implementation audit

2026-10-26. Baseline: `b147a415be0dba2086544afa594d4e7c2c516e2b`.
Linux 7.0.0-38-generic, x86_64, 16 online CPUs, Rust 1.96.0.
Release compiler and runtime; default GC settings except the explicit marker-count comparison.

## Cause and change

Large runtime allocations could proceed while the automatic collector was still queued or waiting for its initial handshake. Mark assists return before an epoch exists, and generated loop polls count iterations rather than allocated bytes. During concurrent sweep, allocations outside the snapshot accumulated without a byte budget. End-of-cycle occupancy then counted that floating garbage as live feedback, raising the next heap goal. Logs showed goals growing into hundreds of megabytes despite a final string of 120,000 bytes.

The old allocation slow path now services initial handshakes for objects larger than the 4 KiB TLAB cutoff. Before an epoch exists, occupancy beyond the major goal plus a 1 MiB runway requests background collection (if no collector owns election) and joins only initial publication. During sweep, old-region slow allocations share a 1 MiB cumulative allocation budget. Exhaustion joins the existing condition-variable protocol, with root publication and subsequent stop participation. The heap mutex is released before waiting. Collector callbacks bypass self-waits and the new safepoint. The initial-publication waiter captures the collector generation under the coordination mutex, services stop requests, and sleeps on the existing budget condition variable until an epoch is published or that generation retires. It then returns through the existing Retry-aware mark assist with zero additional allocation charge. It never self-elects synchronous marking or joins whole-cycle completion on this path. If background requests are unavailable or foreign legacy roots prevent tracing, it preserves allocation progress. Hard reservation admission remains a separate existing policy.

The change adds no compiler IR, generated instructions, ABI symbols, environment variables, or string semantic changes. Runtime allocation already permits GC and relocation. String concatenation roots its part slots and reloads them after allocation; borrowed raw string bytes use an address loan.

## Acceptance measurement

`measure.py` generates the reproducible source inputs, builds isolated executables, checks their output, and records GNU time peak RSS. `before.json` contains five runs per mode/marker setting of the original 40,000-concatenation executable. Those baseline executables were built before editing runtime sources. `after.json` contains five runs for each of 40 combinations: growing strings, the same loop in a synchronous helper, fixed 262,147-byte allocations, and empty-main controls; three workload sizes, synchronous/asynchronous entry, default/three marker threads. Fixed large-object iteration counts are 1,000/2,000/4,000. No full benchmark or example suite is run.

The check is `max(async RSS) < 2 * min(sync RSS) + 34 MiB` for each matched group. The fixed allowance is the existing scheduler cold major runway (capped at 32 MiB) plus the two 1 MiB request/sweep runways. It is independent of input size, run duration, and measured RSS; **it is a fixed scheduler GC allowance, not the resident cost of an empty task**. Empty-task controls are included to make that distinction observable. The script also checks run-to-run async spread below 8 MiB. RSS is an OS/allocator observation, not a guaranteed page-exact bound; the deterministic budget tests below verify the policy independently.

Baseline measurements are available only for the original growing-string case. The large-object/helper groups establish after-change behavior against their synchronous counterparts, not a measured before/after speedup.

Reproduce from the repository root:

```sh
cargo build --release -p willow -p willow_runtime
python3 docs/growing-allocation/measure.py --output /tmp/growing-after.json
cargo test -p willow_runtime sweep_allocation_ -- --nocapture
```

For a historical baseline use that revision's own CLI/runtime with the same script; `--existing DIR` measures saved `sync` and `async` growing-40000 binaries. Temporary binaries, object files and build caches are not evidence artifacts.

### Peak RSS (KiB, minimum–maximum of five runs)

| Workload / iterations | Markers | Before sync | Before async | After sync | After async |
|---|---|---:|---:|---:|---:|
| growing / 40000 | default | 5,644–5,768 | 628,508–779,776 | 5,488–5,892 | 41,116–42,704 |
| growing / 40000 | 3 | 5,644–5,828 | 685,236–820,784 | 5,612–5,740 | 40,988–41,344 |
| helper / 40000 | default | not measured | not measured | 5,668–5,736 | 41,256–41,916 |
| helper / 40000 | 3 | not measured | not measured | 5,668–5,732 | 41,088–41,384 |
| large / 40000 | default | not measured | not measured | 4,776–4,960 | 37,756–41,272 |
| large / 40000 | 3 | not measured | not measured | 4,500–4,904 | 37,628–38,072 |
| empty / 0 | default | not measured | not measured | 3,028–3,156 | 3,452–3,452 |
| empty / 0 | 3 | not measured | not measured | 2,860–3,032 | 3,452–3,452 |

All 20 matched after-change groups passed both the RSS inequality and the 8 MiB spread check. Full size-series data are in `after.json`.

## Complexity and responsibility audit

Let A be old slow allocations, B their total bytes, H the heap snapshot size, E its references, P participating mutators, and S stop/completion notifications. The new admission predicates do O(1) arithmetic under the already-required heap mutex; initial-wait coordination and foreign-owner checks are O(1) per wake (one owner lookup, not a root scan); the initial safepoint is an O(1) atomic fast check when no stop is pending. There is no per-allocation heap/TLAB scan or new index. Storage is one u64 in GC state, O(1); there are no per-object or per-cycle allocations for this policy. Generated code size is unchanged.

Waiting uses the existing condition variable and services required stops: O(P*S) predicate/participation work, not a busy-spin scan. Actual collection retains its existing H/E traversal and sweep costs. Waiting exchanges allocation latency for bounded floating garbage. A single object can exceed the runway; it must be admitted whole. For ordinary old-region slow allocations, the lock-serialized sweep admission bound is runway plus one maximum object, independent of waiter count. Existing small old-buffer fast paths, young allocations, native collector callbacks and foreign unregistered root-owner compatibility are not newly hard-capped by this policy. Marking has its existing assist budget and Retry escape. These are deliberate boundaries, not a claim of a universal process-memory limit.

The original repeated immutable-string concatenation still copies Θ(n²) bytes over n equal appends; final live string storage is Θ(n). This task changes GC retention and coordination, not concatenation semantics or its unavoidable copying under the current implementation. Build scheduling and runtime auto-build remain unchanged.

`scaling.txt` records actual allocated bytes with payload sizes 8,192 / 65,536 / 262,144 / 2,097,152 and 1 / 4 / 16 allocator threads. For each payload, admitted bytes are identical across thread counts and less than 1 MiB plus one aligned object. This is an operation/byte-count proof, not an asymptotic inference from timings. Separate tests cover rollover-adjacent counters, pending-election fallback, collector reentrancy, sleeping mutators joining a stop, and release on cycle completion.

## Focused validation and perspectives

All 22 selected runtime tests passed after the review fix:

- `sweep_allocation_`: eight regression tests (module filter).
- `concat_`: eight string tests.
- `sweep_retains_new_allocations`: two concurrent/recovery tests.
- `reservation_pressure_waits_until_concurrent_sweep_releases_quarantine`.
- `allocation_resumes_while_automatic_collection_is_tracing`.
- `exhausted_runway_allocation_returns_while_only_its_own_slice_retries`.
- The existing coordinator receipt test for foreign roots before/at the initial handshake.

Explicit perspectives: no sweep; zero debt; just below runway; exact runway; near-u64 boundary; one waiter; four waiters; sixteen waiters; ordinary old-region payload; dedicated large region; object larger than runway; shared byte budget; stop participation while asleep; completion wakeup; collector self-allocation; delayed election; initial handshake without generated loop polls; rooted payload survival; final garbage reclamation; string byte/terminator preservation; concatenation root balance; null operands; invalid part lists; overflow-before-read; quarantine retention; recovery sweep; hard reservation pressure; asynchronous mark progress; native Retry escape; synchronous/helper/async equivalence; default/three markers; increasing input sizes; repeated RSS stability.

`example/async_growing_string.wi` builds and runs in release mode, printing `120000`.

Formatting (`cargo fmt --all -- --check`) and host workspace/all-target Clippy passed. Workspace/all-target Clippy also passed for x86_64 Linux, x86_64 Windows MSVC, arm64 macOS and x86_64 macOS. Native execution was performed only on Linux x86_64. Windows and both macOS execution/linking remain unverified. No CI status was queried. Full local test suites and the full example audit were intentionally omitted.

## Review fix: Retry across election and initial publication

The earlier fallback called `collect_for_budget()`. Its precheck for no active epoch did not make synchronous marking safe: self-election or joining an already elected collector could wait on a native trace that required the allocation to return. The repair replaces that call with `cycle_wait::start_for_allocation()` as described above; the precheck is only a budget trigger, never a proof that whole-cycle waiting is safe.

Two deterministic tests hold a rooted native trace in Retry until the 65,536-byte allocation returns. One begins with no collector (the former self-election path); the other holds an elected collector before its initial handshake until the allocator joins the wait protocol. Both observe actual Retry calls, verify allocation returns before releasing the trace, then finish the cycle and verify the rooted child value survives. A test-only thread-local hook holds the second interleaving; it does not enter production builds.

Negative proof temporarily restored only the old `collect_for_budget()` call for each named regression, then restored the fix in a `finally` block. **Both old-path runs failed**, Cargo exit 101 / child SIGABRT, with `runtime fatal: native trace Retry exhausted bounded recovery allowance; graph not reclaimed`. Both fixed-path runs pass. These checks reproduce the actual native trace dependency instead of timing a sleeping marker or relying on an already-active small-allocation cycle. `retry-progress.txt` records the result and the fixed Retry observations; source and commands are in the regression module and this report.

```sh
cargo test -p willow_runtime large_allocation_retry_progress -- --nocapture
```

The audit also found a related sweep-to-next-mark race: the allocator can observe sweep debt, drop the heap lock, and encounter a newly elected marking cycle. `wait_for_sweep_budget()` now revalidates the sweep predicate under coord → heap lock order and captures that same collector generation before sleeping. It cannot join a later marking cycle using the stale sweep observation. A deterministic regression retires the observed sweep, elects a new collector before publication, and verifies that the stale-sweep waiter returns before that collector is released. This adds O(1) revalidation on the already-exhausted slow path; the nursery waiter is unchanged.

The original pending-request test now waits cooperatively for the background collector after the allocation before checking reclaimed bytes; immediate whole-cycle completion is intentionally no longer the allocator contract. All RSS measurements in `after.json` were rerun after this repair using the unchanged thresholds and repeat-count checks.

## Final source audit

Reviewed allocate_object/generated slow path → allocate_old, coordinator election/retirement, initial handshake publication and wakeups, mark Retry, both sweep preparations, reset/default initialization and cycle wait completion. Both normal and recovery sweep use begin_sweep, so the clock resets for both. Every new wait drops the heap lock first and rechecks the predicate on return. Sweep waits also bind the revalidated heap predicate to the collector generation under coord before sleeping. The collector TLS guard prevents self-deadlock. Initial-stop publication and collector retirement notify under the same coordination mutex used by the initial waiter, so there is no check/sleep lost wakeup; completion before reacquisition exits on the generation change. A start-wait request never calls the synchronous collector, even if the earlier heap predicate becomes stale. Retry-aware assistance runs without heap/coord locks, and already-recorded bytes are not charged twice. The new safepoint occurs before allocating the unrooted result. No unrelated source changes or private source material are included. Work remains uncommitted for review.
