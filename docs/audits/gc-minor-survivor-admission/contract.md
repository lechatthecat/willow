# Fixed M4 confirmation contract — 2026-10-24

The user explicitly adopted this workload AFTER viewing the exploratory results.
It is an environment-specific M4 comparison, not independent workload selection
or a universal runtime guarantee. This document is frozen before confirmation.
The original baseline/session and all exploratory samples remain preserved.

Baseline: 8af6c52ba8f6e95e7de14d84f12e9faf98d70995. Same instrumented probe
is injected into a NEW public baseline archive (only candidate component access
is omitted); the original /tmp/willow-m4-baseline-0gzu3gvx is not modified.
Candidate: current runtime working tree, identified by source hash in each report.

Workload: fixed 16 MiB old payload; 16 young chunks, 524160 live allocated
bytes / 21840 objects; 256 roots, zero leases, zero additional workers, 32 of
256 remembered owners. First survival of a pointer-linked chain. Every sample
validates the complete post-collection chain and moved-plus-pinned object count.
All original independent-axis cases remain required and their raw results retained.

Five independent processes PER build, 20 predeclared warmups plus 2000 measured
collections per process. Alternate baseline/candidate execution order each pair.
No retries replacing failed runs: keep stdout/stderr/status and completed rows;
any failed process invalidates that confirmation batch. Preserve and diagnose it.
Do not run builds, tests, reviews, or other deliberate heavy tasks during timing.

Throughput = sum(young objects allocated) / sum(elapsed allocation, root/pin/
remembered setup and minor collection seconds), excluding fixture reset, fixed
old setup, validation and printing. Compare candidate median process throughput
to baseline median: ratio must be >= 0.97. Both use identical inputs and timers.
Footprint = peak entire process RSS from GNU time (KiB converted to bytes),
including harness, setup, warmups, allocator retention, reserve and cleanup.
The worst candidate process peak / worst baseline process peak must be <= 1.10;
also report ratio of medians. This is not only post-GC managed reservation.
GC CPU = collecting-thread CPU nanoseconds around the entire synchronous
minor_collect_internal call (including pre-admission and post-stop cleanup),
divided by young bytes allocated in those samples. No GC background workers
are active in this fixture. Candidate median CPU/byte must be <= baseline
median (flat or lower; no additional CPU tolerance introduced).
STW = request through gate release as existing stop telemetry defines it;
each candidate process p99 must be <= 2 ms. Report each p99.9 and maximum.
Nearest-rank percentiles; 2000 samples yield only two samples in the 0.1% tail.
Report raw distributions and a deterministic paired bootstrap 95% interval
for median ratios (10000 resamples, seed 675), not just a point estimate.

Same host: AMD Ryzen 7 7800X3D, 16 logical CPUs, Linux x86_64. Host facts
and source/toolchain hashes are saved in confirmation-lock.json. Governor
powersave, boost enabled, no CPU affinity or priority override; environment
is not noise-free. Same rustc/cargo, release profile, default features,
Cargo.lock, no RUSTFLAGS/CARGO_ENCODED_RUSTFLAGS or WILLOW/M4 environment
overrides except explicit probe settings. CARGO_BUILD_JOBS=4 for both builds.
No profiling/verification mode differences between builds. CPU clock reads
are identical and outside the measured STW boundary on both builds.
Foreign native runs and CI are unverified; no full local suites or CI polling.

Reproduce: CARGO_BUILD_JOBS=4 python3 docs/audits/gc-minor-survivor-admission/measure.py
--output-directory docs/audits/gc-minor-survivor-admission/confirmation-01
--case medium_candidate --trials 5
Output directory must not exist: previous evidence cannot be overwritten.
