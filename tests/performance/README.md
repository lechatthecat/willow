# Synchronous preemption performance gate

Run on an otherwise idle native Linux, macOS or Windows host with Python 3.12+
and the repository's Rust toolchain/dependencies already available:

```sh
python3 tests/performance/preemption_gate.py --output /tmp/preemption.json
python3 tests/performance/preemption_gate.py --check /tmp/preemption.json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests/performance -p test_preemption_gate.py
```

The first command builds release probes offline, runs the measurements, writes
raw samples and provenance, and exits nonzero if any threshold fails. Missing
historical objects/dependencies, crashes, timeouts and invalid samples fail the
run; they are never reported as a passing gate. `--check` recomputes the verdict
from raw samples using the checked-in thresholds. It does not certify provenance.
Use a platform-appropriate output path on Windows.

| Metric | Fixed threshold | Measurement |
|---|---:|---|
| Never-task extra cost | <= 2 ns/iteration | Paired instrumented minus control loop |
| Task/helper extra cost | <= 50 ns/iteration | Paired task with native helper minus direct cooperative task |
| Wake latency p99 | <= 20 ms | Pending peer explicitly woken inside a still-running helper, one worker |
| Existing async hot path | <= 1.10 x baseline | Identical two-task yield/requeue workload against pinned historical runtime |

These are engineering budgets, fixed before the first measurement, not thresholds
derived from the measured result. The 2 ns bound permits small countdown/entry
bookkeeping, while detecting a runtime call on every non-task iteration. The
50 ns bound budgets amortized polling, switching and native-stack setup over a
long helper. It does not bound a short helper's one-time stack acquisition cost.
20 ms is twice the configured 10 ms time quantum; the measured helper's cheap
iterations should normally exhaust its 1024-point budget much sooner. These are
host-load-sensitive acceptance measurements, not hard real-time guarantees.
The async allowance is a 10% median regression budget.

`crates/willow_runtime/examples/preemption_gate.rs` models the compiler's inline
counter and atomic GC-stop gate, invokes the actual runtime slow path and lazy
native-stack boundary, and keeps the arithmetic/GC gate identical in its controls.
The control omits the preemption counter. The non-task arm checks that it never
enters the slow path. The task arm includes scheduler and native-stack setup and
all resumption costs. Checksums must agree. Counter loads/stores are volatile to
prevent LLVM deleting the operation being measured. This is a **runtime and
polling microbenchmark**, not Cranelift-generated application throughput. It
does not measure arbitrary helper call graphs, allocations, GC stress or many
short helper calls. `example/preemption_acceptance.wi` and its focused integration
test independently exercise actual generated helpers and peer progress.

The wake peer first returns Pending. The helper waits for that handshake, records
an Instant immediately before `willow_sched_wake`, and keeps executing until the
peer records its next poll. Completion before the peer runs is an error. There
is only one scheduler worker, so another worker cannot hide failed preemption.
The metric includes the wake call and queue delay; it excludes initial task/stack
allocation. p99 is nearest rank from 100 fresh-process samples.

The async baseline is `29d709faec149209acd86c706c4f79aeead59416`, the initial lazy
task-stack implementation. The exact same `async_probe.rs` is built against both
runtimes with the same Rust compiler, release profile and runtime codegen-units=1.
Only the historical runtime/ABI crate directories and lockfile are extracted into
a temporary workspace; the checkout and index are untouched. Each side retains
its historical dependencies. This compares the existing cooperative hot path
since lazy stacks were introduced, including intervening runtime changes; it is
not causal attribution to a single ticket or a pre-preemption baseline.

Defaults: 11 paired fresh-process samples, 5,000,000 arithmetic iterations,
20,000 yields per async task, one unrecorded warm-up per mode. Pair order alternates
AB/BA. Durations are measured *inside* the process, excluding startup and output.
The controls retain GC initialization and gates; sub-nanosecond signed differences
are noisy and are neither clamped nor claimed as an exact zero cost. Configuration
is fixed to one worker, budget 1024, quantum 10 ms; other WILLOW_* settings are
removed from the child environment. Raw samples, source/binary hashes, platform,
toolchain, revisions, thresholds and operation counts are saved in JSON.

Four increasing workload sizes also verify equal checksums, zero non-task slow
calls and bounded linear task slow-call counts. This is deterministic work-count
evidence; wall-clock scaling alone is not used to infer complexity. Foreign-native
results must be measured on those hosts; a Linux result makes no foreign execution
claim. Full native benchmark runs are opt-in and are not added to normal CI tests.
