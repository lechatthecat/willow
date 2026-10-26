# Willow vs Go vs Java measurement — 2026-10-25

- Commit: `453fc6cca583cfba36e9df3bbe2d3e2a1dd20c74` (working tree clean at measurement time).
- AMD Ryzen 7 7800X3D 8-Core Processor, 16 logical CPUs; Linux 7.0.0-38-generic, x86_64.
- release Willow compiler and benchmark binaries; go version go1.27.0 linux/amd64; openjdk version "26.0.2" 2026-07-21 (default JVM flags plus `-XX:ActiveProcessorCount` and virtual-thread parallelism = workers).
- Trials per language and case: 5; medians below. `WILLOW_WORKERS=8`, `GOMAXPROCS=8`, JVM processor count 8.
- Scheduler timing measures marked phases and runs all trials of one language before the next. Synchronous timing is whole-process (including JVM startup and JIT warm-up for Java) except `*_only` cases, which time START/END markers; synchronous launch order rotates and every output is validated.
- Build time excluded. CPU affinity/frequency not pinned; background services active.
- Raw trial records: [`MEASUREMENT_2026-10-25.jsonl`](MEASUREMENT_2026-10-25.jsonl).

## Wall time

Ratios are Willow / other; greater than 1 means the other language is faster.

| Case | Work | Willow (ms) | Go (ms) | Java (ms) | Willow / Go | Willow / Java | Willow Δ vs baseline | Go Δ vs baseline | Java Δ vs baseline |
|---|---|---:|---:|---:|---|---|---:|---:|---:|
| idle_spawn | 100,000 tasks parked on channels | 55.731 | 108.138 | 94.902 | 0.52× (Willow 1.94x) | 0.59× (Willow 1.70x) | -27.5% | -5.7% | +10.3% |
| wake_fanout | 100,000 tasks, one send + one completion each | 63.531 | 13.773 | 177.874 | 4.61× (Go 4.61x) | 0.36× (Willow 2.80x) | -11.9% | +3.3% | -12.6% |
| yield_switch | 100,000 tasks × 100 yields | 527.573 | 1,405.322 | 541.895 | 0.38× (Willow 2.66x) | 0.97× (Willow 1.03x) | -53.2% | -4.6% | -2.1% |
| ping_pong | 1,000,000 channel round trips | 695.998 | 163.865 | 782.292 | 4.25× (Go 4.25x) | 0.89× (Willow 1.12x) | -4.6% | -2.5% | -2.7% |
| gc_scheduler | 10,000 tasks × 100 alloc/yield rounds | 84.889 | 134.698 | 140.974 | 0.63× (Willow 1.59x) | 0.60× (Willow 1.66x) | -40.0% | -1.2% | -1.8% |
| channel_select_fan_in | 1,000,000 select receives from 4 producers + cancel | 955.205 | 237.084 | 530.741 | 4.03× (Go 4.03x) | 1.80× (Java 1.80x) | -6.9% | +1.6% | -1.7% |
| spawn_join_tree | fanout 10 × depth 5: 111,110 spawn+join | 123.693 | 17.398 | 109.260 | 7.11× (Go 7.11x) | 1.13× (Java 1.13x) | -41.4% | +5.4% | +12.7% |
| general_pow | 20,480,000 powers; 1,024 bases in [1.125, 2.125), exponent 0.375 | 582.662 | 605.197 | 200.409 | 0.96× (Willow 1.04x) | 2.91× (Java 2.91x) | +1.1% | +1.5% | +1.2% |
| leibniz_pow | 100,000,001 terms, sign via pow (base -1: pow shortcut) | 491.294 | 6,554.700 | 1,787.973 | 0.07× (Willow 13.34x) | 0.27× (Willow 3.64x) | -0.9% | +1.4% | +1.0% |
| leibniz_pow_nonunit | 100,000,001 terms, pow with base -1.0000001 | 4,964.362 | 6,326.353 | 1,787.705 | 0.78× (Willow 1.27x) | 2.78× (Java 2.78x) | -1.2% | -0.4% | -0.2% |
| leibniz_reduced | 100,000,001 terms, sign flip | 109.034 | 98.818 | 264.905 | 1.10× (Go 1.10x) | 0.41× (Willow 2.43x) | +0.6% | -0.2% | +0.1% |
| fibonacci | naive fib(40) | 232.417 | 386.685 | 226.049 | 0.60× (Willow 1.66x) | 1.03× (Java 1.03x) | -0.8% | -0.4% | -1.6% |
| array_sum | append + sum 5,000,000 i64 | 56.427 | 52.333 | 84.290 | 1.08× (Go 1.08x) | 0.67× (Willow 1.49x) | +5.5% | +1.5% | +0.9% |
| linked_list | build + traverse 1,000,000 nodes | 32.556 | 29.413 | 44.553 | 1.11× (Go 1.11x) | 0.73× (Willow 1.37x) | +1.2% | +3.6% | -0.8% |
| array_build_only | push 5,000,000 values (phase) | 45.487 | 50.823 | 55.427 | 0.90× (Willow 1.12x) | 0.82× (Willow 1.22x) | +1.9% | +17.8% | -0.5% |
| array_read_sum_only | index-read 5,000,000 values (phase) | 4.196 | 2.175 | 5.100 | 1.93× (Go 1.93x) | 0.82× (Willow 1.22x) | +0.2% | -5.5% | +2.7% |
| array_write_only | index-write 5,000,000 values (phase) | 4.976 | 3.193 | 7.039 | 1.56× (Go 1.56x) | 0.71× (Willow 1.41x) | +0.6% | -4.3% | -1.2% |
| array_grow_only | 1,000 arrays × 5,000 pushes (phase) | 59.459 | 12.023 | 29.826 | 4.95× (Go 4.95x) | 1.99× (Java 1.99x) | +86.2% | -3.1% | +0.2% |
| object_churn | 10,000,000 short-lived objects, 1,024 live (phase) | 72.724 | 54.210 | 68.712 | 1.34× (Go 1.34x) | 1.06× (Java 1.06x) | +9.1% | -1.3% | -1.5% |
| array_literal_churn | 10,000,000 three-element array literals, 1,024 live (phase) | 131.709 | 83.524 | 77.404 | 1.58× (Go 1.58x) | 1.70× (Java 1.70x) | +13.0% | -2.3% | -1.2% |
| map_lookup_insert | 1,000,000 inserts + 2,000,000 gets (phase) | 85.452 | 84.121 | 197.899 | 1.02× (Go 1.02x) | 0.43× (Willow 2.32x) | -3.1% | -0.4% | -0.8% |
| virtual_dispatch | 20,000 × 1,000 interface calls, 4 classes (phase) | 69.401 | 41.396 | 58.116 | 1.68× (Go 1.68x) | 1.19× (Java 1.19x) | -0.6% | +1.9% | -0.3% |
| closure_call | 20,000 × 1,024 indirect calls to capturing closures (phase) | 57.644 | 26.123 | 16.564 | 2.21× (Go 2.21x) | 3.48× (Java 3.48x) | -0.9% | -0.2% | +0.3% |
| string_build | 2,000,000 strings: 2 int→decimal + 3 concats each (phase) | 66.143 | 79.667 | 84.097 | 0.83× (Willow 1.20x) | 0.79× (Willow 1.27x) | +2.4% | +0.1% | +0.4% |

## Scheduler phase CPU and throughput

CPU is whole-process `utime + stime` at 10 ms resolution. Willow's driver thread busy-spins during parallel phases, so `Willow CPU − wall` approximates worker CPU for comparison with the others.

| Case | Willow wall / CPU (ms) | Go wall / CPU (ms) | Java wall / CPU (ms) | Willow CPU − wall (ms) | Willow ops/s | Go ops/s | Java ops/s |
|---|---:|---:|---:|---:|---:|---:|---:|
| idle_spawn | 55.731 / 200 | 108.138 / 270 | 94.902 / 540 | 144 | n/a | n/a | n/a |
| wake_fanout | 63.531 / 300 | 13.773 / 90 | 177.874 / 1,600 | 236 | 1,574,026 | 7,260,716 | 562,197 |
| yield_switch | 527.573 / 4,130 | 1,405.322 / 9,180 | 541.895 / 4,570 | 3,602 | 18,954,716 | 7,115,805 | 18,453,775 |
| ping_pong | 695.998 / 690 | 163.865 / 170 | 782.292 / 2,040 | -6 | 1,436,785 | 6,102,590 | 1,278,295 |
| gc_scheduler | 84.889 / 500 | 134.698 / 950 | 140.974 / 1,200 | 415 | 11,780,082 | 7,424,041 | 7,093,519 |
| channel_select_fan_in | 955.205 / 1,690 | 237.084 / 350 | 530.741 / 2,020 | 735 | 1,046,896 | 4,217,906 | 1,884,158 |
| spawn_join_tree | 123.693 / 720 | 17.398 / 60 | 109.260 / 940 | 596 | 898,270 | 6,386,219 | 1,016,931 |

## Task footprint (idle_spawn)

Willow reports occupied GC heap bytes; Go `TotalAlloc` and JVM thread-allocated bytes are cumulative allocation counters. These are separate metrics.

| Metric | Willow | Go | Java |
|---|---:|---:|---:|
| Spawn wall (ms) | 55.731 | 108.138 | 94.902 |
| RSS delta (MiB) | 70.99 | 261.61 | 128.88 |
| RSS after park (MiB) | 74.83 | 264.13 | 173.09 |
| RSS bytes / task | 744.37 | 2,743.13 | 1,351.35 |
| Cumulative allocated bytes (Go/JVM) | n/a | 65,501,736 | 97,519,456 |
| Occupied GC heap bytes (Willow) | 7,200,529 | n/a | n/a |

## GC plus scheduler (gc_scheduler)

Counters are not equivalent across runtimes: Willow exposes no pause time, Go reports stop-the-world pauses, and the JVM reports cumulative collector time at 1 ms resolution.

| Metric | Willow | Go | Java |
|---|---:|---:|---:|
| GC cycles | 2 minor / 0 major | 3 | 11 |
| Promoted / moved objects | 4,106 / 147,824 | n/a | n/a |
| TLAB fast allocations / refills | 1,075,902 / 1,600 | n/a | n/a |
| Allocated bytes during phase | 55,360,000 | 22,450,304 | 85,389,032 |
| Heap in use after phase (bytes) | 53,653,842 | 22,186,544 | 70,619,984 |
| GC pause / collector time total (µs) | n/a | 183.884 | 28,000 |
| Max GC pause (µs) | n/a | 61.906 | n/a |

## GC under allocation churn (object_churn)

Counters are not equivalent across runtimes: Willow exposes no pause time, Go reports stop-the-world pauses, and the JVM reports cumulative collector time at 1 ms resolution.

| Metric | Willow | Go | Java |
|---|---:|---:|---:|
| GC cycles | 204 minor / 0 major | 21 | 10 |
| Promoted / moved objects | 513 / 209,922 | n/a | n/a |
| TLAB fast allocations / refills | 9,990,235 / 9,765 | n/a | n/a |
| Allocated bytes during phase | 320,000,000 | 80,032,912 | 240,223,480 |
| Heap in use after phase (bytes) | 545,237 | 1,637,416 | 43,423,272 |
| GC pause / collector time total (µs) | n/a | 391.211 | 3,000 |
| Max GC pause (µs) | n/a | 70.271 | n/a |

## GC under array-literal churn (array_literal_churn)

Counters are not equivalent across runtimes: Willow exposes no pause time, Go reports stop-the-world pauses, and the JVM reports cumulative collector time at 1 ms resolution.

| Metric | Willow | Go | Java |
|---|---:|---:|---:|
| GC cycles | 207 minor / 0 major | 64 | 12 |
| Promoted / moved objects | 1,025 / 425,986 | n/a | n/a |
| TLAB fast allocations / refills | 19,970,675 / 29,325 | n/a | n/a |
| Allocated bytes during phase | 960,000,000 | 240,033,152 | 400,223,536 |
| Heap in use after phase (bytes) | 2,439,989 | n/a | n/a |
| GC pause / collector time total (µs) | n/a | 926.752 | 4,000 |
| Max GC pause (µs) | n/a | n/a | n/a |

## Synchronous CPU time and peak RSS

User/system are whole-process seconds from GNU `time`; RSS is peak RSS.

| Case | Willow user / sys (s) | Go user / sys (s) | Java user / sys (s) | Willow RSS (MiB) | Go RSS (MiB) | Java RSS (MiB) |
|---|---:|---:|---:|---:|---:|---:|
| general_pow | 0.58 / 0.00 | 0.60 / 0.00 | 0.19 / 0.01 | 3.12 | 1.91 | 41.06 |
| leibniz_pow | 0.49 / 0.00 | 6.56 / 0.00 | 1.78 / 0.01 | 3.12 | 1.80 | 41.28 |
| leibniz_pow_nonunit | 4.96 / 0.00 | 6.33 / 0.00 | 1.78 / 0.01 | 3.12 | 1.93 | 41.05 |
| leibniz_reduced | 0.10 / 0.00 | 0.09 / 0.00 | 0.26 / 0.00 | 3.12 | 1.80 | 40.96 |
| fibonacci | 0.23 / 0.00 | 0.38 / 0.00 | 0.22 / 0.01 | 2.92 | 1.80 | 41.00 |
| array_sum | 0.02 / 0.03 | 0.05 / 0.03 | 0.04 / 0.05 | 104.44 | 71.82 | 170.96 |
| linked_list | 0.01 / 0.01 | 0.02 / 0.00 | 0.04 / 0.02 | 44.09 | 18.43 | 72.02 |
| array_build_only | 0.01 / 0.03 | 0.06 / 0.03 | 0.03 / 0.05 | 104.52 | 96.27 | 170.69 |
| array_read_sum_only | 0.01 / 0.03 | 0.05 / 0.03 | 0.04 / 0.05 | 104.59 | 72.02 | 171.25 |
| array_write_only | 0.02 / 0.03 | 0.05 / 0.03 | 0.05 / 0.05 | 104.63 | 71.72 | 171.11 |
| array_grow_only | 0.03 / 0.03 | 0.01 / 0.00 | 0.03 / 0.02 | 8.43 | 7.38 | 90.61 |
| object_churn | 0.07 / 0.00 | 0.05 / 0.00 | 0.07 / 0.05 | 6.61 | 7.41 | 154.57 |
| array_literal_churn | 0.12 / 0.00 | 0.09 / 0.00 | 0.07 / 0.05 | 13.36 | 8.02 | 154.85 |
| map_lookup_insert | 0.06 / 0.02 | 0.07 / 0.01 | 0.35 / 0.07 | 54.14 | 44.46 | 194.79 |
| virtual_dispatch | 0.06 / 0.00 | 0.04 / 0.00 | 0.07 / 0.01 | 3.23 | 1.96 | 41.53 |
| closure_call | 0.05 / 0.00 | 0.02 / 0.00 | 0.03 / 0.01 | 3.25 | 1.96 | 42.56 |
| string_build | 0.06 / 0.00 | 0.08 / 0.00 | 0.09 / 0.05 | 3.78 | 7.05 | 136.55 |

## Reproduce

```sh
python3 benches/go_vs_willow/compare.py run --label 2026-10-25 --trials 5 --workers 8 --languages willow,go,java
```

## Notes

- **Provenance.** All three languages come from one `compare.py run` at `453fc6c` with a clean tree. The JSONL's leading environment record was added afterwards from this report's header, because the harness only started writing it after this run. The Δ columns compare against a local 2026-10-23 run at `49f1900` (clean, same host; that JSONL is not checked in). Extra records for the regression check: [`GC_CHURN_RERUN_2026-10-25.jsonl`](GC_CHURN_RERUN_2026-10-25.jsonl) (Willow + Go, same binaries, `--no-build`).
- **Willow improvements (real).** Trial ranges do not overlap the baseline: `yield_switch` 1127.8 → 527.6 ms (-53%; Willow now 2.66x faster than Go, 1.03x than Java), `spawn_join_tree` 211.2 → 123.7 ms (-41%), `gc_scheduler` 141.6 → 84.9 ms (-40%; baseline spread was 26%, but the new max 90.1 is below the baseline min 136.5), `idle_spawn` 76.8 → 55.7 ms (-27%; both runs 17% spread, ranges disjoint; Willow 1.94x faster than Go, 744 B/task RSS). `wake_fanout` -12% has slightly overlapping ranges and is marginal. These follow the scheduler commits in range (`376ac93` lock-free spawn, `8f68d34` async scheduling overhead, `7c175d4` idle-peer spreading).
- **Willow regressions (confirmed by rerun).** `array_grow_only` 31.9 → 59.5 ms (rerun 75.1, +86–135%; sys 0.01 → 0.03–0.04 s, RSS 5.3 → 8.8 MiB), `array_literal_churn` 116.6 → 131.7 ms (rerun 132.9, +13%), `object_churn` 66.6 → 72.7 ms (rerun 72.4, +9%). Go is unchanged on all three. Willow GC counters (minor cycles, moved/promoted objects, TLAB fast allocations/refills, allocated bytes) are identical to the baseline, so the extra time is per-GC/per-refill or page commit cost, not more collections. Cause not yet bisected; candidates are the GC/region commits between `49f1900` and `453fc6c`.
- **Other languages.** Java `idle_spawn` +10.3% and `spawn_join_tree` +12.7%, Go `array_build_only` +17.8%: not Willow, not rerun; treat as noise.
- Everything else moved less than ~6%. Sync whole-process cases (`array_sum`, `linked_list`) include startup; their +1–6% is noise.
- **gc_scheduler Willow GC counters.** In this run `willow/gc_scheduler.wi` printed TLAB, collection, promoted and moved counters as process-lifetime totals (allocated bytes was already a phase delta). The program now prints phase deltas like the churn cases; a 5-trial rerun with the fixed program gave 2 minor / 0 major collections, 1,075,893–1,075,920 TLAB fast allocations, 1,601–1,604 refills and 4,115–4,550 promoted objects, i.e. the same as the table above, because almost nothing is allocated before `START`.
