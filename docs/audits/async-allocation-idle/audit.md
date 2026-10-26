# Async allocation churn: idle GC ownership

Date: 2026-10-26. Baseline: `20eacea9ca7a053e720102a6b93171c2e125f8eb`.
Host: Linux 7.0.0-38-generic, x86_64, Rust 1.96.0; `nproc` reports 16.
Native measurements apply to this Linux host only.

## Mechanism and implementation audit

The existing `example/async_allocation_churn.wi` retains a 64-element ring,
allocating two linked nodes each iteration in a synchronous function called by
async main. The comparison changes only main's async qualifier and the iteration
count. Forty million iterations retain the same bounded graph and print
`5119995904` and `true`. Pure computation does not exercise the minor-GC stops.

Previously every stop called the scheduler's broadcast wake function. Idle pool
workers and the driver woke, republished roots at a safepoint, acknowledged the
stop, waited on the GC condition, and returned to scheduler parking. Thus a
single active allocator forced idle peers through every minor collection.
Changing marker-thread count does not remove these scheduler participants.

`IdleMutator` now publishes root-slot addresses under the GC coordination mutex
before scheduler wait-list registration. It retains the registration and SATB
buffer, flushes pending SATB work, and marks the owner parked until the idle
interval ends. The collector can reuse those slots across multiple cycles and
rewrite relocation-safe slots. Raw runtime references retain the existing
conservative pinning/deferral behavior. The guard is neither Send nor Sync, and
its unsafe entry contract prohibits managed access until drop. The only product
caller is `IdleWaiters::wait_inner`; all code after entry operates on native
wait-list metadata or parks. Every return drops wait-list locks before the guard.
The initial stale-generation fast path still avoids registration entirely.

Entry and stop construction share the coordination lock: an early idle entrant
is excluded from pending acknowledgements, while a late entrant removes its
pending entry and notifies the waiting collector. Drop waits for any active stop
and removes the parked bit under that same lock, before managed work resumes.
The existing work/completion generation protocol and deadline handling remain
intact. The scheduler's GC broadcast is therefore removed, including for the
separate completion-driver list. Active mutators still cooperate at safepoints.

Profiling the intermediate change also exposed empty condition-variable wakes.
GC stop/budget notification and milestone telemetry issued futex wake syscalls
even with no observers. After suppressing coordination/budget empty wakes,
1,526 of the roughly 1,540 futex calls in a one-million-iteration probe still
addressed the milestone condition (confirmed by symbol lookup). `GcCondvar` now
serves these three GC conditions. It registers a waiter before atomic
unlock-and-wait and decrements via RAII after reacquiring the predicate mutex,
including timeout and poison returns. Notifiers hold that same mutex, ordering
registration and the relaxed atomic count check; a zero count safely omits the
OS wake. Nonzero counts still notify all, preserving predicate-loop/spurious-wake
semantics. All product notification sites were inspected for the mutex contract.
No polling fallback or timing-dependent GC bypass was introduced.

## Complexity and boundaries

Independent dimensions are allocation iterations A, collection stops C, registered
threads W, published root slots R, and actual scheduler idle intervals I. Existing
GC registry/root scanning remains O(C*(W+R)); this change does not claim that the
entire collector is globally optimal. Previously idle handshakes additionally
caused O(C*W) wake/park activity and repeated O(C*R) owner-side snapshots. The new
owner-side work is O(I + sum of roots at idle entry), with O(R+W) retained
publication/park metadata. An unchanged idle interval adds zero GC wake calls
regardless of C. Condition checks and waiter bookkeeping are O(1), with constant
additional space per condition. Genuine active mutators, observers, timed waits,
and concurrent allocation can still require collection-dependent wakeups.

Deterministic tests vary roots over 0/1/64/1024, stops over 1/8/64, and idle
participants over 1/4/16 for both worker and completion lists. They verify one
unchanged publication per idle interval, no pending acknowledgements, zero GC
notifications, and exactly W initial probes independent of C. These assertions,
not timing alone, establish the removed multiplicative wake work. Aliased roots
exercise fan-out; moving roots are reused across minor and major cycles.
The root vectors, existing collector scans, and existing native waiter slots
remain necessary storage; no per-allocation index or growing cycle history is
added. Compiler lowering, generated-code size, ABI, build scheduling and runtime
auto-build are unchanged. All implementation changes belong to the runtime.

## Reproduction and measurement

Build with `rtk cargo build --release --bin willow` and
`rtk cargo build --release -p willow_runtime`, then run:

```sh
rtk proxy python3 docs/audits/async-allocation-idle/measure.py --strace \
  --output docs/audits/async-allocation-idle/after.jsonl
```

The script derives sources from the checked-in runnable example, compiles each
sync/async pair, asserts outputs, and deletes generated sources/binaries from its
temporary directory. Trials alternate sync/async. Wall, process CPU and context
switches come from untraced child runs; futex summaries come from separate
`strace -f -c -e futex` runs, so tracer overhead is not used for performance ratios.
Default worker configuration explicitly removes `WILLOW_WORKERS`; other cases
set it to 1, 4 or 16. No GC overrides were set for these measurements.

For the baseline, build the same two generated forty-million-iteration sources
using the baseline revision's release compiler/runtime into `sync` and `async`
binaries in a separate directory, then use the current script's
`--existing <directory> --counts 40000000 --strace` option. Baseline binaries were
built before product edits. Final before/after datasets were collected sequentially
after local validation, without concurrent builds/tests from this session.
Three untraced trials per pair are retained in `before.jsonl` and `after.jsonl`.

Forty-million-iteration medians (seconds); futex counts are separate traced runs.

| Revision | Workers | Sync wall | Async wall | Ratio | Async voluntary switches | Async futex calls |
|---|---|---:|---:|---:|---:|---:|
| before | default | 0.7086 | 1.4566 | 2.056 | 771085 | 1,716,826 |
| before | 1 | 0.7079 | 0.8221 | 1.161 | 36645 | 160,879 |
| before | 4 | 0.7110 | 0.8595 | 1.209 | 146464 | 570,366 |
| before | 16 | 0.7103 | 1.4633 | 2.060 | 771676 | 1,715,830 |
| after | default | 0.6721 | 0.7007 | 1.043 | 88 | 108 |
| after | 1 | 0.6748 | 0.6950 | 1.030 | 14 | 15 |
| after | 4 | 0.6773 | 0.7029 | 1.038 | 26 | 29 |
| after | 16 | 0.6731 | 0.7016 | 1.042 | 79 | 113 |

The full workload meets the ~20% wall-time bound at every tested pool setting.
Default async/sync ratio falls from 2.056 to 1.043. With the fix, increasing
allocation iterations by 40x does not increase idle synchronization proportionally:

| Workers | Iterations | Async voluntary switches (median) | Async futex calls |
|---|---:|---:|---:|
| default | 1,000,000 | 88 | 110 |
| default | 10,000,000 | 96 | 141 |
| default | 40,000,000 | 88 | 108 |
| 1 | 1,000,000 | 15 | 15 |
| 1 | 10,000,000 | 17 | 14 |
| 1 | 40,000,000 | 14 | 15 |
| 4 | 1,000,000 | 25 | 37 |
| 4 | 10,000,000 | 26 | 37 |
| 4 | 40,000,000 | 26 | 29 |
| 16 | 1,000,000 | 98 | 112 |
| 16 | 10,000,000 | 89 | 110 |
| 16 | 40,000,000 | 79 | 113 |

The remaining bounded counts are consistent with startup/completion and OS scheduling;
the deterministic tests independently assert zero GC-driven wakes throughout an idle interval.
Wall ratios include process/thread startup. The observations do not promise a
fixed syscall count for arbitrary workloads with multiple active allocators.

## Validation and explicit perspectives

The following focused runtime filters passed using `rtk cargo test -p
willow_runtime --lib <filter> -- --test-threads=1`:

| Filter | Passed | Ignored |
|---|---:|---:|
| `gc::condition::tests` | 3 | 0 |
| `gc::safepoint::idle_tests` | 5 | 0 |
| `scheduler::parking::tests` | 17 | 1 |
| `parallel_completion_` | 4 | 0 |
| `budget_wait_` | 5 | 1 |
| `explicit_collect_joins_active_cycles_without_busy_polling` | 1 | 0 |
| `shutdown_notification_` | 5 | 0 |
| `overlap_satb_deletions_parked_mutators_and_old_descendants_survive_sweep` | 1 | 0 |
| `multi_mutator_stw_` | 2 | 0 |
| `gc_telemetry::milestones::tests` | 13 | 0 |

The ignored cases are existing opt-in waiter CPU and idle-wake latency measurements.
An earlier `idle_` filtered run also passed 67 cases with three opt-in ignores;
the narrower final filters above verify the final affected paths. Deterministic
outputs are saved in `root-publication.txt` and `waiter-scaling.txt`. The intentional
caught panic in the root log verifies guard release on unwind and is not a failure.

Explicit perspectives checked:

1. Empty root publication.
2. One live root.
3. Many aliased roots and increasing root counts.
4. Stable publication storage across many stops.
5. Minor collection while the owner remains idle.
6. Major initial/remark/sweep cycles while idle.
7. Relocation-safe root writeback before resume.
8. Conservative raw-reference pinning.
9. Idle entry after a stop request.
10. Resume attempts during an active stop.
11. Unregistered native waiters.
12. Unwind releases parked ownership.
13. Worker-list idle ownership.
14. Completion-driver idle ownership.
15. Increasing participant counts without GC wakes or repeated probes.
16. Work published before wait registration.
17. Targeted wake and notification-count behavior.
18. Deadline/timeout and independently unparked waiters.
19. Driver completion before wait and prompt completion wake.
20. Worker GC with a nested scheduler drive.
21. Empty conditions emit no OS notifications at increasing call counts.
22. Real condition waiters receive notification; timeout counts balance.
23. Poisoned waits release waiter registration.
24. Budget waits participate in stop rounds and tolerate spurious wakes.
25. Shutdown waits cooperate with GC and completion.
26. Concurrent-mark SATB deletion/old-descendant retention.
27. Milestone observers retain timeout, expiration, reset and completion behavior.
28. Unobserved milestones preserve stage publication without wake calls.
29. Channel references survive GC stress across sleeping async tasks.
30. Cancellation after channel churn still completes.

The last two perspectives passed individually with `rtk cargo test --test
integration chgc_02_live_channels_survive_stress_across_tasks -- --test-threads=1`
and the corresponding `chgc_03_cancel_after_channel_churn_is_fast` filter.
The already-existing runnable example was executed directly with
`rtk proxy target/release/willow run example/async_allocation_churn.wi --release`
and printed `12795904` / `true`. It was reused, not newly added in this change.

`rtk cargo fmt --all -- --check` passed. Workspace/all-targets Clippy with
`-D warnings` passed for x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc,
aarch64-apple-darwin and x86_64-apple-darwin. Only Linux x86_64 executed tests
and linked/running probes; Windows and both macOS architectures remain unexecuted.
No full local suite, full runnable-example audit or CI check was performed.
