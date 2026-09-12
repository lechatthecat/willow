# Roadmap acceptance audit — 2026-09-12

This audit applies the tickets' recorded tracking criteria. It does not change
the acceptance criteria or completion status of their implementation children.

bd additionally enforces open-child completion before an epic can close.
`willow-6fv.5` and `willow-38w` therefore remain open; their hierarchy check has
not been overridden. `willow-u7n` was closed after its acceptance audit.

## willow-u7n: original compiler optimization roadmap

The current `requirements/performance_improve.md` supersedes the historical
runtime-C `-O3` wording with the release Rust static library.

| Requirement | Implemented evidence |
| --- | --- |
| Backend options and debug/release optimization | `Codegen::new` passes `none`/`speed` to Cranelift using `CompilerOptions` |
| Release runtime | `HostToolchain` selects the release archive and builds `willow_runtime --release`; toolchain integration tests verify both ABI surfaces |
| Benchmarks outside printing loops | `benches/compiler_bench.rs`, `benches/README.md`; completed `willow-pz6q.18` |
| Typed lowering | Completed `willow-mb5`; `src/ir/lowered.rs` and typed LIR emission |
| Short-circuit operators | Completed `willow-u7n.1` |
| Constant folding and simple inlining | `fold_blocks` and `inline_scalar_leaves` in `src/ir/optimize.rs`, invoked by lowered IR construction |
| Mode-specific metadata and full compile diagnostics | Completed `willow-0gc`; source-aware diagnostics retained in both modes |
| Further runtime call reductions | Explicitly scoped in the separate `willow-8hq4` performance epic |

Closing this original roadmap does not close `willow-8hq4` or imply that all
possible compiler/runtime optimizations have been implemented.

## willow-6fv.5: staged GC roadmap

The issue numbers preserve the original seven-stage roadmap. The current spec
has evolved to stages 0–8, so number equality is not the mapping.

| Current spec stage | Original issue | Recorded state |
| --- | --- | --- |
| 0: correctness | `willow-6fv.5.1` | Closed |
| 1: metadata/allocation contract | `willow-6fv.5.2` | Closed |
| 2: TLABs | `willow-6fv.5.3` | Closed |
| 3: young generation | `willow-6fv.5.4` | Closed |
| 4: remembered sets | `willow-6fv.5.4` | Closed |
| 5: precise stack maps | Broader `willow-6fv.5.8` specification gate | Open; no dedicated implementation child found |
| 6: concurrent marking | `willow-6fv.5.6` | Open |
| 7: old-generation regions | `willow-6fv.5.5` | Closed |
| 8: selected compaction | `willow-6fv.5.7` | Open |

The original blocking chain is `.1 → .2 → .3 → .4 → .5 → .6 → .7`.
The correctness stage explicitly relates to the authoritative current-runtime
hardening issues `willow-6fv.1` through `.4` and `willow-lpn.6`/`.8`.
The compiler allocation/layout contract relates to `willow-u7n`.

All seven original stages have concrete child issues, dependencies preserve
correctness before later collector changes, and the existing root/trace safety
work remains authoritative. Those are the roadmap tracker's acceptance criteria.
Concurrent marking/sweeping, compaction, and the broader ApexGC work remain open
implementation gates. Explicit shadow roots remain authoritative; this audit
does not claim precise stack maps, moving/concurrent collection, or new worker
migration guarantees have passed their implementation acceptance gates.

## willow-38w: parallel and async roadmap

The description's `requirements/pararell_async.md` is a historical typo. The
source is `requirements/parallel_async.md`, also recorded by `willow-921`.

| Source scope | Implemented or explicitly scoped issues |
| --- | --- |
| Runtime foundation | Closed `willow-j1y`, `willow-gyaa.1` |
| Spawn, handles, join | Closed `willow-2xy`; intentional eager-Task/spawn-removal migration in closed `willow-h2vf` and children |
| Async/await, executor, timers | Closed `willow-jku`, `willow-gyaa.2`, `willow-cjs` |
| Channels | Closed `willow-vum`, `willow-dsw`, `willow-gyaa.5` |
| Async file/network I/O | Closed `willow-2s3.1`/`.2`, `willow-lcw` |
| Select and timeouts | Closed `willow-7aj`, `willow-soro` |
| Cancellation/scopes, workers, parallel iterators | Closed `willow-2s3.3`, `willow-gyaa.4`, `willow-2s3.4` |
| Diagnostics and task metadata | Closed `willow-fih`, `willow-9xm` |
| GC-backed state machines | Closed `willow-lpn`, `willow-k6n` |
| Scheduler-aware locks | Closed `willow-38w.1` |
| Sync preemption and async-borrow interaction extensions | Open children `willow-38w.2`/`.3`, plus `willow-7ol.12` |

The parent explicitly accepts work implemented **or scoped into child issues**.
Its original scope has that coverage. Closing the tracker does not accept the
remaining sync-stack architecture, worker migration, or async-borrow work.
