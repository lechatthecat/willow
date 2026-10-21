# virtual_dispatch comparison — 2026-10-21

Machine: AMD Ryzen 7 7800X3D (8 cores), Linux 7.0.0-38-generic x86_64, glibc 2.43. Go 1.27.0 linux/amd64; OpenJDK 26.0.2. Workers: 8. Five interleaved trials per language per run. Existing `benches/go_vs_willow/run_sync.py virtual_dispatch --trials 5 --languages willow,go,java` harness; outputs all validated as `1180000000`.

Before: clean commit `0fd7e182093f9d47d4269e1f23ea8fb21e992f3e`. After: rebuilt uncommitted working tree with this ticket’s compiler changes. No unrelated changes were present. Initial after run preceded the canonical hierarchy cache refinement; final after run includes it. `benchmark.jsonl` preserves all three runs. Harness and workload are existing ignored local benchmark sources, not new tracked copies.

| Language | Before median ms | Final after median ms | Change | Before spread | After spread |
|---|---:|---:|---:|---:|---:|
| willow | 65.237 | 65.181 | -0.09% | 2.50% | 1.25% |
| go | 41.070 | 41.515 | +1.08% | 8.98% | 3.24% |
| java | 58.361 | 58.123 | -0.41% | 1.00% | 1.21% |

Spread is (max − min) / median. Changes are within ordinary variation; no speedup or regression is established. Final Willow / Go = 1.570, Willow / Java = 1.121 (Go and Java faster on this workload). The benchmark uses multiple implementations, so it retains guarded dispatch; this is expected rather than evidence that singleton devirtualization failed. Singleton removal is established by generated IR and deterministic counts.

Wall scope is the harness START/END phase, not JVM startup time. CPU and RSS fields are whole-process measurements; JVM JIT/GC activity can still affect phase timing. Builds ran alongside compiler verification, so these machine-specific trials are not an isolated performance study. No trial spread exceeded 15%, and no median changed by 10%; the skill’s rerun threshold was not triggered. No new regression ticket was created.
