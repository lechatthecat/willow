# gc-mark-worker-cache: process-wide mark-worker configuration

2026-10-26. Baseline source: `2375b9462ef6a0a28ab3b7e07990b503ee3e9c3a`.
Host: Linux x86_64, AMD Ryzen 7 7800X3D, 16 logical CPUs.

## Change and affected paths

`gc/mark_workers.rs` resolves the existing environment/default policy through one
process-wide `LazyLock<usize>`. `pacer_inputs`, `ConcurrentCycle::with_epoch_index`
and lazy `Pool::new` all read that same static. The resolver still accepts zero,
clamps valid integers to 64, and falls back to available parallelism minus one,
clamped to 1–4 (or one on detection error). Settings are sampled at first use;
later environment/CPU-quota changes do not reconfigure the pool. There is no
explicit runtime reconfiguration API. Existing pool shutdown, fallback and
runtime auto-build behavior remain unchanged.

The initializer reads only the environment and OS parallelism; it neither acquires
GC locks nor calls back into GC, so lazy initialization adds no lock cycle.
Thread-safe publication comes from the standard-library LazyLock. The counter is
compiled only for tests. No compiler, generated code or runtime ABI changes.

## Complexity audit

Let Q be total lookups across pacer samples, cycle construction and pool creation,
and H the CPU-detection cgroup hierarchy depth. Previously the default path did
O(Q * H) filesystem work, plus Q environment parses/temporary strings. Now there
is one O(H) discovery and O(Q) constant-time cached accesses, using O(1) retained
memory. Environment parsing costs O(E) once for input length E. OS discovery may
also depend on platform mount/process metadata; those costs are paid once per
process, not per lookup. No cache keyed by cycle, object, exit or caller is needed.
One discovery is necessary to honor the current automatic policy; one constant-time
read per caller is sufficient. Compiler cost and generated-code size are unchanged.
The deterministic test uses 16 concurrent callers with 1, 10, 1,000 and 10,000
calls each and checks that the source query count remains exactly one at every
step. This tests cold concurrent access and 176,176 total accesses; fresh-process
environment tests independently exercise cold initialization and repeated access.

## Reproduction

Build the unmodified runtime with `cargo build --release -p willow_runtime` and
copy `target/release/libwillow_runtime.a` to a temporary baseline archive before
editing. Run `python3 docs/audits/gc-mark-worker-cache-measure.py before /tmp/before.a`.
Build the changed runtime and run the same script with `after` and the changed
archive. Both runs use the same release compiler and explicit `--runtime-lib`;
there are no runtime auto-build ambiguities. The script emits reviewed JSONL only;
full strace logs and binaries stay temporary. Do not run competing builds during
timed samples. Seven alternating default/override trials are run per concat size,
plus five trials per affected churn case using the existing benchmark runner and
8 workers. All program outputs are checked by the script/runner.

The standalone runnable example is `example/gc_mark_worker_count.wi`, expected
output `120000`. Raw measurements accompany this report.

## Validation and measured results

Median whole-process milliseconds (7 trials per mode):

| Concat iterations | Before default | Before override=3 | After default | After override=3 |
|---|---:|---:|---:|---:|
| 10000 | 334.816 | 15.265 | 16.167 | 15.627 |
| 40000 | 1974.911 | 173.353 | 177.274 | 170.992 |
| 80000 | 5147.102 | 697.288 | 729.285 | 695.996 |

Default-mode openat counts (proc/self/cgroup, cpu.max aggregate):

| Iterations | Before | After |
|---|---:|---:|
| 10000 | 3447, 20682 | 2, 12 |
| 40000 | 35885, 215310 | 2, 12 |
| 80000 | 98679, 592074 | 2, 12 |

After caching, cat40000 default is 3.67% above override=3, within the approximately 10% target.
The two remaining discovery calls are consistent with the worker configuration and
the independently cached pacer CPU capacity. Counts remain fixed across all sizes.

Related churn cases, median phase milliseconds (5 trials, 8 workers):

| Case | Historical baseline | Current before | Current after |
|---|---:|---:|---:|
| array_grow_only | 31.9 | 65.911 | 18.706 |
| array_literal_churn | 116.6 | 128.344 | 129.574 |
| object_churn | 66.6 | 72.690 | 71.747 |

The fix explains the array-growth regression and removes more overhead than the
historical regression magnitude. It does not explain the literal/object churn
regressions: their current before/after times remain close. This is an isolated
runtime comparison, not a bisect of the earlier historical revision range.
Churn metrics are identical before/after: literal churn has 207 minor collections,
425,986 moved objects and 1,025 promoted; object churn has 204 minor collections,
209,922 moved objects and 513 promoted. Both have zero major collections.
Remaining churn cost belongs to the existing separate regression investigation;
this change does not claim to resolve it.

## Checks and limitations

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --target TARGET -- -D warnings`: passed for
  x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc, aarch64-apple-darwin and x86_64-apple-darwin.
- `cargo test -p willow_runtime --lib gc::mark_workers::`: 6 passed, including
  11 fresh-process environment cases, concurrent source-query bounds, idle/used pool
  shutdown, timeout detachment, fatal infrastructure panic and spurious wake handling.
- `cargo test -p willow_runtime --lib gc::pacer::tests::fractional_workers_and_duty_repayment`: 1 passed.
- `cargo test -p willow_runtime --lib mark_queue_slot_pool_exhaustion_reuse_and_scaling`: 1 passed.
- Release example built and ran on Linux x86_64, output `120000`.
- Runtime semantics and performance were executed only on Linux x86_64. Windows
  and both macOS targets were cross-linted, not linked or executed. CI was not checked.
- Full workspace tests and full runnable-example audit were intentionally omitted.
- Timings are observations on this host, not asymptotic proof. Bounded lookup counts
  and increasing-size syscall counts supply the scaling evidence. The final after
  measurement was repeated after all Clippy builds finished to avoid build contention.
- No private specification content or internal ticket identifiers are included.

