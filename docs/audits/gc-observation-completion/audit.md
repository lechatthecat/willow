# Telemetry completion implementation audit

Date: 2026-10-21. Scope: GC observation. Review baseline: 4f303ad.
This is an implementation/review handoff, not a ticket closure or publication.

## Current-session changes and acceptance boundary

The baseline already contained V1–V4 snapshots, stop/worker telemetry, interval
capture, fixed histograms, residency providers and unwind-safe runtime handling.
This session completes missing observation semantics and reproducible consumers:

- Thread-local progress batches replace the shared per-poll increment. Concurrent
  saturation cannot wrap. Lifecycle and interval events carry individual progress;
  unregister merges the last partial batch. Detailed recording remains opt-in.
- Initial/remark/minor/recovery work now includes graph traversal work, published
  once per batch/traversal. Concurrent batches have a separate event operation.
  Termination restarts reach the existing budget/work group. Completed-cycle
  summaries retain their previous meanings and layouts.
- Nested cycle exit restores the parent's epoch. Cycle elapsed time and explicit
  cycle waits have event-only kinds, preserving the nine-slot V4 histogram ABI.
  Allocation stalls remain distinct from explicit collection/lifecycle joins.
- Unwound or early-aborted cycles invalidate coverage. Saturated concurrent batch
  data is flagged before cycle publication. A post-flush trace-error accessor
  lets independent endpoints observe sink failure without another heap snapshot.
- The consumer rejects incomplete/lost/disabled/reset/aborted streams and merges
  per-thread intervals to find simultaneous GC-attributed blocking. It requires
  independent endpoint snapshots and per-thread lifecycle/progress evidence.
- A platform-neutral residency boundary is tested with fake successful, zero,
  missing and maximum values. Heap occupancy/commit scope is preserved.

The public contract is `docs/gc_telemetry.md`. V1–V4 C structures are unchanged.
New event fields/operations are explicit; the new consumer rejects older V4
streams lacking individual progress rather than treating them as zero waiting.
Existing V2 stop and worker groups retain initial/remark/minor/recovery request,
completion, pause, fallback and failure classifications. No collector policy,
compiler transformation or final zero-STW acceptance rule is changed.

## Implementation and complexity audit

Let P be safepoint polls, S interval starts/ends, B completed graph batches,
R explicit termination restarts, T participating threads, E retained/parsed events,
and H heap regions/chunks visited by the existing snapshot.

Poll recording does O(1) TLS work. Global progress publication occurs after 256
polls or on interval entry/unregister; an empty batch does not touch the atomic.
Total publication attempts are at most ceil(P/256)+S+T, before CAS contention
retries. This removes the previous per-poll shared cache-line update. CAS retry
cost under contention is not claimed constant; the batched bound describes calls.
Each thread has fixed TLS storage, with no new global thread registry.

Graph traversal retains its existing local counts. The new shared work occurs
once per completed batch/traversal and explicit restart: O(B+R), not per object,
reference slot or root. Source review included MarkBatch drop, legacy mark_worklist,
MinorCollector completion, with_stw, termination restart, Cycle finish/drop,
cycle_wait and their callers. No new repeated heap scan, generated code, per-object
allocation, metadata index or retention of managed pointers was introduced.

Each event updates fixed-size counters and an O(1) bounded ring operation. Storage
is bounded by 4096 events; disabled history allocates no ring. Snapshot extension
copying is O(1); the enclosing snapshot keeps its pre-existing O(H) metadata work
and optional OS residency query. Flush is O(E), with E <= 4096 per drain, and
never holds the observation mutex while formatting or doing I/O. Observation
critical sections acquire no heap/coordination lock; lock acquisition spans hold
no observation guard while waiting. Phase restoration and interval cleanup use
RAII, including caught unwinds. Stop/cycle spans overlap deliberately.

The checker parses O(E) events and uses expected O(1) hash operations for pairing.
It validates monotonic per-thread streams and merges them with a heap in
O(E log T), instead of sorting all events in O(E log E). The union sweep is O(E),
including thread joins/exits and nested waits. Storage is O(E+T), retained for
this offline diagnostic; it does not allocate a range from advertised counters.
The merge comparison bound is appropriate for T independent ordered timelines.
A streaming online consumer could trade buffering for explicit watermarks;
that protocol is outside this ticket.

Deterministic scaling checks (not timing-based complexity inference): ring tests
use 0, 1, 64, 4096, 4097 and 16384 events, with exact retained/dropped counts.
Parser fixtures use T=1/4/16 and nested depth=1/8/64, asserting exactly
T*(2*depth+2) events and the same 990 ns union. Existing minor graph/root scaling
and termination-generation scaling tests run with measurement enabled. These
cover deep overlap, fan-out, repeated equivalent publication and lifecycle exits.
The saturation test starts near u64::MAX and concurrently merges 16 batches.

## Validation and test perspectives

Local Linux checks passed:

- `cargo fmt --all -- --check` and `git diff --check`.
- `cargo test -p willow_runtime --lib gc_telemetry --no-fail-fast`: 65 tests,
  plus the configured lifecycle subprocesses.
- Six named affected-path tests, with WILLOW_GC_MEASURE=1: minor graph/root
  scaling, termination-generation scaling, termination transition model,
  panicking-collector resume, tiny remark budget/recovery, explicit cycle joins.
- Workspace/all-target host Clippy and cross-target Clippy with `-D warnings` for
  x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc, aarch64-apple-darwin,
  x86_64-apple-darwin.
- Native Linux C ABI example linked to the release runtime: V1 sentinel/layout
  checks and V1–V4 size negotiation succeeded; V1 size remains 2112 bytes.
- V4 consumer self-tests and existing V2 stop-parser self-tests passed. Seven
  fresh-process V4 trace runs were independently checked by the reproduction.

The focused tests exercise these distinct perspectives: V1 layout; V2/V3/V4
negotiation; unaligned buffers; short-buffer nonmutation; histogram zero; every
power-of-two boundary; maximum duration; counter saturation; concurrent progress
saturation; coherent concurrent snapshots; allocation unregister reconciliation;
progress unregister reconciliation; per-thread progress; nested epoch restoration;
phase unwind restoration; completed versus active cycles; abort validity; zero
wait with intact progress; nonzero simultaneous waits; overlapping nested waits;
explicit joins versus allocation stalls; fallback/recovery stops; budget/restarts;
ring capacity/loss; duplicate/gapped/truncated trace; unknown event/identity;
broken trace I/O; final checkpoint versus independent endpoint; process reset;
residency success/failure fakes; memory-scope preservation; increasing-depth and
thread-count operation counts; allocation/scan-rate consumer fixtures.

Native execution was Linux x86_64 only. Windows and both macOS architectures have
local Clippy evidence, not native execution or link/test evidence. Full workspace
and example suites were intentionally not run locally; CI was not queried.
These limitations do not block the explicitly scoped implementation handoff.

## Reproduction and measurement scope

Build and run from the repository root (prefix shell commands with `rtk`):

```sh
cargo build --release -p willow_runtime --example gc_telemetry_probe --lib
python3 docs/audits/gc-observation-completion/gc_observation_check.py --self-test
python3 docs/audits/gc-observation-completion/gc_observation_repro.py \
  --binary target/release/examples/gc_telemetry_probe \
  --output docs/audits/gc-observation-completion/costs.json
cc -std=c11 -I crates/willow_runtime/include \
  crates/willow_runtime/examples/gc_telemetry_abi.c \
  target/release/libwillow_runtime.a -lpthread -ldl -lm \
  -o target/release/gc_telemetry_abi
```

`costs.json` contains every independent endpoint, verification result and timing
sample summary for seven repetitions of disabled, aggregate-only and traced modes.
The runtime probe performs 200,000 unrooted 16-byte allocations and safepoint
polls, 10,000 synthetic Assist interval recordings, 10,000 Rust snapshots and
1,000 negotiated V4 snapshots per process. Synthetic Assist spans measure record
cost, not production Assist work. Allocation timing includes trace flushes caused
by allocation/poll recording; synthetic-record and snapshot phases are outside
that timer. V4 snapshot timings include its best-effort RSS query; Rust snapshot
timings do not. These workloads can perform real collections and are not a proof
of zero STW. Binaries and raw transient traces remain outside tracked artifacts.

No fixed numeric telemetry-only overhead threshold is stated in the current
acceptance text or public baseline report. This audit reports measured ratios
against the same binary with recording disabled; it does not invent or relax a
budget. Stable-host final collector throughput/latency gates remain with the
acceptance-suite ticket, as the current ticket explicitly specifies. OS scheduling,
CPU frequency and cache state affect these short runs; no asymptotic conclusion
is inferred from timing or favorable noise.

### Final Linux cost measurements

Medians of seven runs; ns per complete synthetic interval recording, ns per
snapshot, and millions of allocations per second. V4 includes RSS sampling.

| Mode | Record ns | Rust snapshot p50 / p99 ns | V4 snapshot p50 / p99 ns | Mallocations/s | Throughput / disabled |
|---|---:|---:|---:|---:|---:|
| disabled | 2 | 101 / 32721 | 6142 / 11091 | 22.440 | 1.0000 |
| measure | 73 | 181 / 32841 | 6302 / 11211 | 22.292 | 0.9934 |
| trace | 95 | 191 / 32070 | 6322 / 11031 | 20.931 | 0.9327 |

Seven traced runs passed independent endpoint checks with no transport loss or
errors. The checker observes nonzero GC-attributed global waiting in this baseline;
that is evidence the measurement is not manufacturing a zero-pause result.

## Review repair: background marker role

The reviewer reproduced a false zero: a registered, progressing marker was
included in the population used to detect all application mutators waiting.
Each V4 event now carries role 0 (application) or 1 (background marker).
worker_loop establishes a TLS marker role before any registration and retains
it through unregister. Application collection/Assist keeps the application role.
A role scope restores TLS on unwind. No C snapshot layout or aggregate counter
meaning changed.

The checker still validates every marker's registration, progress, paired
intervals, event sequence and coverage. Only application events enter the
blocking-population merge. It reports progress separately for the two roles,
and rejects missing/unknown/changing roles and windows with only marker progress.
The deterministic fixture has two application mutators with a 10 ns overlapping
wait and a third marker progressing throughout; the result remains 10 ns, with
application progress 2 and marker progress 7. The original checker returned zero.

Validation: telemetry filter 65 tests (including subprocess checks for marker
register/unregister role fields), eight parser tests with role regressions,
worker-pool lifecycle test, fmt, host workspace Clippy and all four target Clippy
runs passed. The seven-run reproduction was rerun against the role-aware stream.
Linux native execution only; foreign native tests and CI remain unqueried.

Complexity audit: fixed TLS role read per event; no added poll work, per-object
instrumentation, lock, registry or heap scan. Existing checker maps retain one
role per observed thread, O(T) extra storage and expected O(1) validation per event.
The merge now processes application endpoints only; the O(E log T) upper bound
and O(E+T) storage bound are unchanged. Previous working changes were preserved;
no staging, commit, push or ticket closure was performed.

## Review repair: reconcile independent progress totals

The checker now requires application_progress + marker_progress to equal
(after.progress - before.progress). Both endpoints already require all threads
unregistered, so the runtime has flushed all pending TLS progress; a mismatch
means incomplete or inconsistent coverage even when the final checkpoint agrees
with the endpoint. The reviewer case (endpoint/checkpoint 102, observed progress
2) is rejected. Regressions also reject understated progress, omitted marker
progress and overstated mixed-role progress, and accept a correct delta with
nonzero pre-window cumulative progress.

Validation: nine parser tests passed, including the original 10 ns marker-role
regression; seven fresh role-aware runtime traces passed the stronger independent
progress check. fmt, host workspace/all-target Clippy and diff whitespace checks
passed. This repair changes only the Python consumer/tests and documentation;
prior 65 telemetry tests and four-target Clippy evidence apply to the unchanged
Rust implementation. No foreign native execution, full local suite or CI query.

Implementation audit: the check adds two counter reads, one addition, one
subtraction and one comparison after existing lifecycle validation. It introduces
no scans or new storage, so O(E log T) time / O(E+T) consumer storage and all runtime
cost/ABI bounds are unchanged. The reproduction data and median table were
refreshed. Ticket remains claimed, with no staging, commit, push or closure.


## Review repair: coordination acquisition coverage

Registration published its lifecycle before acquiring `coord`, leaving the
acquisition unmeasured. The same gap existed before cycle-join intervals began.
The shared GC mutex wrapper now accepts the coordination role and emits event-only
kind 11 around acquisition. It retains kind 4 for heap acquisition. The runtime
coordination field uses this wrapper, covering registration, unregister,
collector election/drop, explicit collection, allocation/current-cycle joins,
recovery queries, and reacquisition after servicing safepoints. Snapshot layouts
and the nine histogram slots remain unchanged. Uncontended acquisition overhead
is included; mutex hold time is not attributed to kind 11.

Call-path audit: ordinary `.lock()` calls use the wrapper; condition-variable
reacquisition remains inside existing handshake/cycle intervals. Diagnostic
`try_lock` does not block. The raw-mutex coercion in `reset_lock` is restricted to
runtime initialization/reset, outside valid observation windows. The wrapper
returns the original guard/poison result and preserves condvar compatibility.
Observation state is never held while attempting the GC mutex; recording after
acquisition follows the existing GC-lock-to-observation-lock ordering.

Deterministic proof: the checker fixture registers two application threads, keeps
one waiting for coordination across the other's stopped callback [10,30), and
reports the exact 20 ns union, including a nested cycle interval. The runtime
subprocess regression holds actual coordination ownership until the worker's
kind-11 begin is observed, verifies the worker cannot finish, then checks its
matching end occurs after release. It covers registration inside an actual
stopped callback and an already-registered explicit request joining an elected
cycle. Synchronization and event timestamps establish ordering; sleeps and timing
thresholds are not used to infer contention.

Complexity audit: each of A acquisitions adds exactly one begin/end pair, O(A)
recording work and O(1) local state per acquisition, with the existing bounded
4096-event ring. Mutex contention can delay recording; no constant wall-time
claim is made. There are no new scans, retries, allocations per mutator poll, or
compiler/generated-code changes. Consumer classification adds one constant-time
kind comparison; its existing O(E log T) merge and O(E + T) storage bounds remain.
The increasing-depth/thread-count consumer fixtures still pass. Broader capture
can fill the ring sooner; explicit loss flags and endpoint checks continue to
reject incomplete windows instead of asserting zero blocking.

Revalidation (Linux host): new runtime contention test and subprocess; 65
telemetry tests; named explicit-join, real root-round/remark/recovery participation,
and collector-unwind tests; ten checker tests; seven fresh endpoint-checked trace
runs; release probe and native C ABI example; fmt; host workspace/all-target Clippy;
and workspace/all-target Clippy for x86_64 Linux, x86_64 Windows, arm64 macOS and
x86_64 macOS all passed. Costs and the median table above were refreshed.
Foreign native execution and full suites were not run; CI was not queried.
Ticket remains claimed and open for review; no staging, commit or publication.
