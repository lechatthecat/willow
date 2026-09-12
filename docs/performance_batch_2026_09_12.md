# Runtime and toolchain performance batch — 2026-09-12

Base revision: `1e5f30c5e03e6756793c14bd901f13e5dbbb142d`.
Measurements below are Linux x86_64 observations, not native Apple/Windows
acceptance. Raw samples are in `benches/performance_2026_09_12/`. Some sampling
overlapped compiler verification; small timing differences are not evidence of
improvement. No pure-function speedup is attributed to the runtime changes.

## willow-8hq4.1: dead stripping

The existing Rust static archive contains per-function `.text.*` sections.
Linking the same object for `example/class.wi` seven times per configuration:

| Runtime archive | Link before | Link after | Binary before | Binary after |
| --- | ---: | ---: | ---: | ---: |
| Debug | 300.05 ms | 191.17 ms | 18,008,800 B | 14,297,584 B |
| Release | 115.50 ms | 65.05 ms | 6,275,304 B | 4,395,088 B |

All four binaries printed `42` and exited successfully. These size measurements
precede the metadata-retention correction described below; the release fixture
does not emit metadata, while final debug binaries retain their metadata blob.

Flags use GNU `--gc-sections`, Apple `-dead_strip`, and MSVC `/OPT:REF /OPT:ICF`.
Unknown linker families retain their existing behavior. The Linux regression
fixture puts used and unused functions in the same object and proves unused
sections disappear, including when symbol stripping is enabled.

The full integration run found that metadata accessed by symbol, rather than by
generated code, was removed. When debug information is emitted, the linker now
explicitly roots `willow_runtime_metadata_v1` (with Apple's symbol prefix).
Both embedded-metadata regressions pass after this correction. Native Apple and
Windows linker/execution gates remain required.

## willow-8hq4.2: runtime freshness proof

A stamp is published only after successful Cargo completion with unchanged
source/configuration inputs. It includes an archive metadata signature and an
input fingerprint; file contents plus metadata detect same-mtime edits and
ordinary touches. The scan includes local dependency sources, nested new files,
workspace/runtime manifests, and the lockfile. Failed builds invalidate prior
proof before Cargo runs. Missing, wrong-kind, unreadable, or malformed state
falls back to Cargo. Explicit runtime-library overrides keep their bypass.

Nonempty custom Cargo configuration, custom compiler/wrapper/profile/source
environment settings, external local dependencies, and local build scripts
conservatively use Cargo. The fast path does not attempt to replace Cargo's
general dependency resolver. Relative target directories are normalized before
changing Cargo's working directory. Environment values are hashed, not stored.

An isolated `CARGO_TARGET_DIR` demonstrated one initial Cargo invocation and no
Cargo invocation on the next compile. Nine-sample observations:

| Path | Median |
| --- | ---: |
| Isolated no-op Cargo probe | 23.556 ms |
| Warm default debug compile | 232.984 ms |
| `WILLOW_FORCE_RUNTIME_BUILD=1` debug compile | 269.877 ms |

The forced path includes the new fingerprint work, so it is not represented as
a historical pre-change compiler baseline. Further platform measurements and
the complete CLI input-invalidation matrix remain acceptance work.

## willow-8hq4.4: panic and root depth

Panic contexts may be shared, so depth uses the ticket's atomic fallback.
Push/recover update an atomic count while holding the state mutex; readers use
an acquire load through a borrowed TLS context, avoiding Arc clone/drop and the
mutex. No dangling raw TLS pointer is introduced.

Root depth uses destructor-free `Cell<usize>` TLS. Push, pop, truncation, root
parking, root resumption, and runtime reset all update the mirror. Overflow
still aborts; underflow/clamping behavior is preserved. Tests compare the mirror
with the authoritative stack and cover context swaps, shared-thread mutation,
fresh workers, parking/resumption, nested recovery, and saturation.

Fifteen samples per side, identical generated release programs and separate
before/after runtime archives:

| Workload | Before | After |
| --- | ---: | ---: |
| Panic-capable chain | 83.287 ms | 51.934 ms |
| Pure chain control | 18.105 ms | 18.948 ms |
| Fibonacci control | 15.988 ms | 16.749 ms |
| Requests, 0% panics | 1.493 ms | 1.434 ms |
| Requests, 1% panics | 1.528 ms | 1.451 ms |
| Requests, 10% panics | 1.842 ms | 1.729 ms |
| GC roots with recovery | 3.645 ms | 3.532 ms |

The primary chain improves materially; the controls do not. Linux disassembly
removes the panic accessor's `lock inc`/`lock cmpxchg`/Arc-release sequence.
The root accessor reduces to TLS address/load, overflow check, and return on
the normal path, without Vec initialization/destruction or RefCell machinery.
Native platform measurements and worker-migration timing remain outstanding.

Reproduce with `scripts/benchmark_runtime_depth.py --before BEFORE_ARCHIVE
--after AFTER_ARCHIVE --output RESULTS.json`.

## willow-8hq4.6: direct runtime calls

One `RuntimeObjectModule::declare_func_in_func` path recognizes exact ABI-table
symbols and marks their references colocated. Imports keep `Linkage::Import`;
they are not incorrectly declared as definitions. Unknown imports and targets
retain their prior policy. Supported targets share the small-code-model
reachability assumption already used for generated direct functions.

Linux x86_64 debug/release object tests prove `R_X86_64_PLT32` relocations for
runtime print calls and execute the resulting binaries. The same runtime
archive with before/after compilers showed no material runtime timing change.
Request fixture binaries decreased by 4,096 bytes (4,609,888 to 4,605,792);
other fixture file sizes were unchanged due to section alignment.

The benchmark supports `--before-compiler` and `--after-compiler` for this
comparison. Native macOS/Windows/Linux-AArch64 and large-displacement gates
remain required before closing the ticket.

## willow-8hq4.5 and willow-s9ej.13: stable snapshots and nested recovery

Snapshot reuse is limited to the successful continuation block of a checked
call with no intervening call instruction. The reused value is the observed
post-call depth, including a decrease caused by recovery. Block changes and
any intervening call invalidate reuse. `WILLOW_PANIC_SNAPSHOT_REUSE=0` provides
an unoptimized compiler comparison. Proven-pure calls still have no reads.

Object tests prove three straight-line surviving calls remove at least two
reads in release; debug call-stack instrumentation conservatively prevents
some reuse. Five new regressions cover loop/branch joins, active panic entry,
recovery followed by a second panic, methods, constructors, GC stress, and
nested deferred recovery. All 288 panic-related integration tests passed.
Current lowering selects LIR/fallback automatically; no obsolete environment
switch is represented as independent backend evidence.

The tests also reproduced an existing bug with reuse disabled: recovery in a
nested deferred action resumed the enclosing action without restoring its
runtime defer depth. Cleanup now starts at its actual runtime depth and the
recovered edge reinstalls the suspended enclosing defer entries. A subsequent
panic during the resumed action is covered too.

Nine-sample stable-region timings were 97.543 ms before and 95.654 ms after;
the generic chain was 52.146 vs 52.666 ms. These small differences are not a
claimed material speedup. Read elimination is the deterministic improvement.
Raw results: `runtime_snapshots.json`.

## Native panic portability evidence (willow-s9ej.9)

The previously outstanding gate had already run successfully at the base SHA:
https://github.com/lechatthecat/willow/actions/runs/34686254018 . All 30
`panic_recover_stress` perspectives passed on Windows x86_64, Intel macOS,
and Apple Silicon macOS. Workspace-test step durations were 31m56s, 29m30s,
and 12m22s respectively. `psr_30` at that revision checks debug/release output
against the canonical LF transcript and checks actual process success.
The existing 180-second per-program deadline needed no relaxation.

## willow-8hq4.8: exact channel ownership

Wait queues expose generation tickets. Task reverse links distinguish receive
waits/claims and send waits/handoffs, and cleanup removes only exact captured
generations. Receiver claims reserve availability while values stay in the
GC-traced queue; bounded send handoffs reserve capacity. Select lowering
cleans each distinct runtime raw once and retains the winning direction.
Live queued/running owners retain claims when an additional wake cannot enqueue
them again. All wakes happen outside channel locks, and received references
are rooted across wake-triggered GC.

Sixty channel unit tests and 103 existing select integration tests passed.
Three new integration fixtures passed in normal, budget-one, and scheduler-GC
stress configurations. Shared 10k/100k gates assert exactly N wake attempts;
the combined test completed in under one second locally including setup and
consumption. Exact per-size timings and footprint/ping-pong measurements are
recorded separately when collected.
