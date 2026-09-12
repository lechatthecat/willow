# Runtime and toolchain performance batch — 2026-09-12

Base revision: `1e5f30c5e03e6756793c14bd901f13e5dbbb142d`.
The initial measurements are Linux x86_64 observations; the final section
records native Linux, Apple, and Windows evidence. Raw samples are in `benches/performance_2026_09_12/`. Some sampling
overlapped compiler verification; small timing differences are not evidence of
improvement. No pure-function speedup is attributed to the runtime changes.

## willow-8hq4.1: dead stripping

The existing Rust static archive contains per-function `.text.*` sections.
Linking the same object for `example/class.wi` seven times per configuration:

| Runtime archive | Link before | Link after | Binary before | Binary after |
| --- | ---: | ---: | ---: | ---: |
| Debug | 300.05 ms | 191.17 ms | 18,008,800 B | 14,297,584 B |
| Release | 115.50 ms | 65.05 ms | 6,275,304 B | 4,395,088 B |

All four binaries printed `42` and exited successfully. These initial size
measurements precede metadata retention. A final seven-sample repeat using the
same object/archive per profile retains the debug metadata symbol on both sides:

| Runtime archive | Link before | Link after | Binary before | Binary after |
| --- | ---: | ---: | ---: | ---: |
| Debug | 274.30 ms | 182.36 ms | 18,136,472 B | 14,401,496 B |
| Release | 102.62 ms | 61.66 ms | 6,292,256 B | 4,395,488 B |

Every final binary printed `42`. Raw samples are in `dead_strip_final.json`;
workspace tests ran concurrently, so timings remain host observations.

Flags use GNU `--gc-sections`, Apple `-dead_strip`, and MSVC `/OPT:REF /OPT:ICF`.
Unknown linker families retain their existing behavior. The Linux regression
fixture puts used and unused functions in the same object and proves unused
sections disappear, including when symbol stripping is enabled.

The full integration run found that metadata accessed by symbol, rather than by
generated code, was removed. When debug information is emitted, the linker now
explicitly roots `willow_runtime_metadata_v1` (with Apple's symbol prefix).
Both embedded-metadata regressions pass after this correction. Native Apple and Windows linker/execution gates passed; the final native
measurements and suite status are recorded below.

## willow-8hq4.2: runtime freshness proof

A stamp is published only after successful Cargo completion with unchanged
source/configuration inputs. It includes an archive metadata signature, a streamed BLAKE3 content digest,
and an input fingerprint; file contents plus metadata detect same-mtime edits and
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
a historical pre-change compiler baseline. A subsequent final-tree CLI matrix passed all 37 Cargo-invocation assertions:
fresh skips, repeated force, overrides even with force, independent profiles,
missing/wrong-kind archives, source/manifest/lock touches, and added/removed
nested sources. Every successful binary printed `42`. Nine-sample medians were
230.33 ms warm, 287.54 ms forced, and 31.39 ms for the isolated no-op Cargo
probe, recorded in `runtime_cache_cli.json`. Tests for unreadable/uncertain state,
coarse timestamps, and failed publication cover the conservative failure paths.
Native platform validation is recorded below.

A later Windows run exposed a same-size archive replacement with unchanged
mtime. A deterministic restored-mtime regression reproduced it. Version 3
stamps now require the full archive content digest, byte count, and stable
before/after metadata. Old stamps fail validation. Hashing is deferred until
the cheaper input/version checks pass. The first full-content implementation
used DefaultHasher and raised warm debug compilation to 398.33 ms; replacing
it with BLAKE3 reduced that to 240.43 ms. Forced builds measured 298.86 ms and
no-op Cargo 31.50 ms. All 37 CLI assertions and 12 cache unit tests pass.
Both intermediate and final raw results are retained in
`runtime_cache_content_cli.json` and `runtime_cache_blake3_cli.json`.

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
Native platform accessor measurements and the four-task context-switch
workload are recorded below.

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
comparison. Native execution passed on macOS, Windows, and Linux AArch64. Explicit
large-displacement/range proof remains required before closing the ticket.

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
claimed material speedup. Read elimination is the deterministic improvement. The fixture's whole-object
panic-depth call relocations were 10 → 10 in debug and 10 → 8 in release;
`snapshot_read_counts.json` records these exact counts. Raw timing results:
`runtime_snapshots.json`.

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
consumption. Measured send-loop times (debug runtime) were 33.800 ms for 10k sends and
406.253 ms for 100k sends, with exactly 10k/100k wake attempts.

Three-sample release medians for 10k idle tasks/private ping-pong rounds:

| Workers | Idle RSS before → after | Ping-pong before → after |
| --- | ---: | ---: |
| 1 | 760.2 → 840.9 bytes/task | 36.655 → 39.053 ms |
| 5 | 762.3 → 839.7 bytes/task | 36.987 → 38.709 ms |

The baseline runtime predates the depth changes, so these measurements cover
combined runtime edits. Exact ownership increases parked-task storage and does
not close the private-channel ping-pong gap; that remains outside this ticket.
Raw samples: `channel_scaling.json` and `channel_runtime.json`.

## Native measurements and final validation

Native runtime, toolchain, ABI export/link, and 30-perspective panic stress
checks passed on all five targets. Unix depth evidence is from
https://github.com/lechatthecat/willow/actions/runs/34688891604 ; Unix link
measurements are from
https://github.com/lechatthecat/willow/actions/runs/34689471861 . The corrected
Windows harness and both measurements passed in
https://github.com/lechatthecat/willow/actions/runs/34689717977 .

The Windows old archive needed an otherwise-unused new ABI export because
MSVC resolves all declared externals, including those without relocations.
Only the isolated baseline checkout receives that export; it aborts if called,
and all benchmark cases execute successfully. No baseline depth implementation
was changed. The first Windows harness attempts are retained in CI history;
the successful run above supplies the actual evidence.

Nine-sample panic-capable chain medians and full-accessor static instruction
counts (including cold/error paths):

| Native runner | Chain before → after | Panic accessor instructions | Root accessor instructions |
| --- | ---: | ---: | ---: |
| ubuntu-24.04 | 152.251 → 101.690 ms | 104 → 42 | 39 → 14 |
| ubuntu-24.04-arm | 284.824 → 141.891 ms | 114 → 53 | 52 → 19 |
| macos-15 | 169.252 → 104.481 ms | 103 → 47 | 51 → 18 |
| macos-15-intel | 715.276 → 466.433 ms | 104 → 49 | 44 → 15 |
| windows-2025 | 113.214 → 73.165 ms | 108 → 50 | 50 → 21 |

The worker-context benchmark ran four eager tasks with repeated panic/recovery
and yields; output stayed 10 on each target. Controls and request/GC workloads
are in each raw JSON file. Small control or worker differences are not claimed
as wins. Both accessor disassemblies are checked in beside the samples.

Seven-sample native linker measurements, same object/archive per profile:

| Native runner | Profile | Link before → after | Bytes before → after |
| --- | --- | ---: | ---: |
| ubuntu-24.04 | debug | 340.185 → 267.509 ms | 18,062,040 → 14,311,192 |
| ubuntu-24.04 | release | 141.129 → 104.640 ms | 6,292,104 → 4,384,400 |
| ubuntu-24.04-arm | debug | 318.553 → 256.229 ms | 18,576,840 → 14,555,432 |
| ubuntu-24.04-arm | release | 127.605 → 89.847 ms | 6,537,048 → 4,500,616 |
| macos-15 | debug | 66.007 → 57.885 ms | 5,780,024 → 1,109,784 |
| macos-15 | release | 53.031 → 48.894 ms | 2,558,824 → 555,592 |
| macos-15-intel | debug | 135.368 → 113.292 ms | 5,431,784 → 1,042,800 |
| macos-15-intel | release | 105.449 → 95.496 ms | 2,489,792 → 538,520 |
| windows-2025 | debug | 121.422 → 120.379 ms | 350,720 → 350,720 |
| windows-2025 | release | 63.588 → 63.162 ms | 197,120 → 197,120 |

Windows already eliminated these sections with its original linker defaults;
the explicit flags produced the same sizes and no meaningful timing gain.
The Windows comparison uses `link.exe` directly, so it excludes the constant
`cl.exe` driver startup cost. All linked executables printed 42. The POSIX
comparisons use the same `cc` driver as the compiler.

The complete local workspace run passed 7,442 tests (26 ignored), and strict
Clippy and formatting pass. Native Linux's initial full CI run passed too.
Apple Silicon's initial full run exposed two test assumptions: requiring
unused predeclared strings to survive linking, and cancelling after a fixed
20-ms delay before a worker necessarily registered its defer. The tests now
execute matched/fallback downcasts and synchronize readiness/completion.
Both corrections passed on Apple Silicon in run 34689632347, which exposed
the same fixed-delay cancellation race in fixture 88. All eleven remaining
fixtures using this pattern now signal at their intended phase and await task
completion. The full runtime safety matrix passed 109 tests; fixtures 35 and
88 each passed ten repeated GC-stress runs. Final native CI at revision
`055fceb` is tracked by
https://github.com/lechatthecat/willow/actions/runs/34690503325 .

## Ticket accounting

| Ticket | Work completed | Status |
| --- | --- | --- |
| willow-u7n | Audited original optimization roadmap against implemented acceptance | Closed |
| willow-aff | Audited Option/Result acceptance, preserving the recorded nullable-removal descope | Closed |
| willow-s9ej.9 | Verified historical native panic stress evidence at the base revision | Closed |
| willow-s9ej.13 | Fixed nested recovery restoring enclosing defer depth | Closed |
| willow-s9ej | Completed panic epic after all children closed | Closed |
| willow-8hq4.5 | Reused panic snapshots only across proven stable regions | Closed |
| willow-8hq4.8 | Added exact channel ownership and linear wake accounting | Closed |
| willow-8hq4.2 | Cached successful runtime freshness proofs conservatively | Content-digest native gate pending |
| willow-8hq4.4 | Reduced panic/root depth access cost with native evidence | Closed |
| willow-8hq4.1 | Dead stripping with metadata retention and native link measurements | Final full-suite gate pending |

The additional runtime FuncRef colocation implementation belongs to
`willow-8hq4.6`; it remains open for explicit range proof and is not counted
among these ten. GC, async, and generics implementation gaps remain open.

The Windows job in run 34690503325 exposed the archive metadata ambiguity
described above. The cache ticket was reopened rather than treating the
failure as flaky. The corrected digest implementation at `3c50d9a` has fresh
native performance/toolchain gates in run 34690931372 and full CI in
run 34690932635. Formatting and strict workspace Clippy passed locally.
