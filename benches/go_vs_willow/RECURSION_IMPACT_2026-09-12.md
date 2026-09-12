# Recursion optimization validation — 2026-09-12

Candidate: `70fb2d6582a729ea584ba357639fa599b9e79ab5` ([CI](https://github.com/lechatthecat/willow/actions/runs/34693081610)). The local source/test files match this candidate.
Baseline: `b277388b12d768de84222f1a6119e70d1bbab1a2`, built in an isolated worktree.

## What changed

- Scalar synchronous self tail calls become loops with parallel argument copies.
- Bounded scalar early returns bypass entry-poll preparation. Recursive paths retain polls.
- Synchronous cancellation checks use the invocation-constant native-task activity bit;
  native tasks retain cancellation and cleanup checks. SSA carries the delayed lookup across joins.
- Small, pure self-recursive scalar bodies are expanded by up to three levels, capped at
  256 LIR instructions and 64 blocks. No memoization, Fibonacci-specific recognition,
  arithmetic reassociation, allocation, cleanup, reference arguments, or faulting operations
  are introduced into the expansion.

The benchmark source and exponential recurrence are unchanged. Linux `fib` code size
increased from 251 bytes to 1,580 bytes. Smaller recursion inputs and other workloads
may make different tradeoffs; this is not a universal speedup claim.

## Matched performance comparison

AMD Ryzen 7 7800X3D, Linux x86_64; freshly built release compilers and binaries.
Five fresh-process trials per variant/case; launch order rotates between variants.
`WILLOW_WORKERS=8`, `GOMAXPROCS=8`. Build time excluded; wall time includes startup/shutdown.
Local tests completed before timing. CPU affinity/frequency not pinned; background services active.
All 55 outputs passed the existing harness validation. [Raw records](RECURSION_IMPACT_2026-09-12.jsonl).

| Case | Baseline Willow (ms) | Candidate Willow (ms) | Candidate time change |
|---|---:|---:|---:|
| fibonacci | 2191.429 | 304.813 | -86.1% |
| leibniz_pow | 5328.425 | 5315.497 | -0.2% |
| leibniz_reduced | 110.984 | 111.150 | +0.2% |
| array_sum | 314.987 | 305.775 | -2.9% |
| linked_list | 1339.833 | 1329.096 | -0.8% |

For `fib(40)`, Go measured **406.079 ms**, candidate Willow **304.813 ms**.
Willow was faster in all five trial pairs: 301.148–313.465 ms versus Go 396.863–407.850 ms.
Willow/Go wall-time ratio: **0.751**. The previous poll-only candidate measured 901.835 ms;
its [records](FIBONACCI_2026-09-12_OPTIMIZED.jsonl) remain available, along with the
[earlier TRE-only rerun](FIBONACCI_2026-09-12_RERUN.jsonl).

## Correctness and platform verification

Focused regressions cover deep tail recursion, argument exchange, multiple recursive branches,
base cases, floating-point evaluation order, bounded expansion, and rejection of faults/effects/loops.
The recursive cancellation regression synchronizes task startup with a channel and awaits cleanup completion.

Local validation: 7,449 workspace tests passed (26 ignored); the separate runnable-example
audit passed. Clippy with `-D warnings` and formatting passed. The updated synchronized
recursive-cancellation test also passed in a focused run.
All four native CI jobs passed for the exact candidate commit, including formatting,
Clippy, workspace tests, and the runnable-example audit. [CI records](RECURSION_CI_2026-09-12.json).

| Native target | Result |
|---|---|
| ubuntu-24.04 / x86_64-unknown-linux-gnu | [Passed](https://github.com/lechatthecat/willow/actions/runs/34693081610/job/103551707449) |
| windows-2025 / x86_64-pc-windows-msvc | [Passed](https://github.com/lechatthecat/willow/actions/runs/34693081610/job/103551707566) |
| macos-15 / aarch64-apple-darwin | [Passed](https://github.com/lechatthecat/willow/actions/runs/34693081610/job/103551707570) |
| macos-15-intel / x86_64-apple-darwin | [Passed](https://github.com/lechatthecat/willow/actions/runs/34693081610/job/103551707574) |
The required native matrix is Linux x86_64 GNU, macOS ARM64, macOS Intel, and Windows x86_64 MSVC.
Other target triples are outside this verification claim; see [platform compatibility](../../docs/platform_compatibility.md).

Reproduce current Fibonacci/Go timings with:

```sh
python3 benches/go_vs_willow/run_sync.py fibonacci --trials 5 --workers 8
```
