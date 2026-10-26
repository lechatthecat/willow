# Inline synchronous poll gates

2026-10-26; Linux x86-64; rustc 1.96.0. Baseline source:
`7d967369ba46c9e84026fa018dfd4977030d4226`.

## Measurement and decision

Build the current CLI with `cargo build --release --bin willow`, then run
`python3 docs/audits/sync-poll-gates/measure.py`. The script uses GNU linker
`--wrap` for both gate accessor symbols. It builds the checked-in release
benchmarks and generated mutually recursive inputs. The wrapper counts external
calls from generated objects; it does not count runtime-internal inlined calls.
The workloads use synchronous main, so plain counters suffice. Raw stdout,
counts and elapsed seconds are in `baseline.jsonl` and `after.jsonl`.

| Workload | Before, calls to each accessor | After, calls to each accessor |
| --- | ---: | ---: |
| fibonacci (40) | 41,287,564 | 0 |
| closure_call (20,480,000 closure calls) | 1,025 | 0 |
| virtual_dispatch (20,000,000 method calls) | 1 | 0 |
| mutual recursion depth 8 | 256 | 0 |
| mutual recursion depth 12 | 4,096 | 0 |
| mutual recursion depth 16 | 65,536 | 0 |
| mutual recursion depth 20 | 1,048,576 | 0 |

All before/after program outputs match. The recursive counts justify removing
lookup calls. Existing leaf optimization already makes this cost negligible in
the closure and virtual-dispatch hot loops. Instrumented single-run Fibonacci
elapsed time was 0.258 s before and 0.167 s after; this is illustrative, not a
controlled speedup estimate. No asymptotic claim depends on wall-clock timing.

## Implementation and call-path audit

`Context` publishes a task-owned countdown pointer (null for non-native execution)
and a process-lifetime GC stop pointer. `Context::refresh` obtains them at runtime
entry/resume, after native-stack CURRENT is installed. `ScalarBridge::enter`
refreshes ordinary thread and callback contexts. Native stack resume refreshes
before returning to suspended generated frames. Native contexts have stable
addresses; a cached countdown belongs to the task, not worker TLS. Nested entry
uses the existing saved/restored pinned register and active context. Returning
to an outer context leaves its task pointer intact. Stack recycling refreshes
before generated reentry. Unsupported native-stack configurations continue to
publish the accessor's null result.

The compiler emits two pointer loads through the existing pinned register for
both ordinary and deferred entry polls. The existing atomic GC flag read,
countdown decrement, spare-countdown selection, runtime slow path, cancellation
and cleanup branches are unchanged. In particular a null counter does not remove
the GC gate. No process-global cached task pointer was introduced.

The visible ABI prefix grows from 48 to 64 bytes; the new offsets and changed
root offsets/size enter the ABI fingerprint. Compile-time layout assertions cover
the runtime layout. All root accesses use shared constants; source inspection
found no literal old root offsets in generated assembly. Existing accessor ABI
symbols remain available. Runtime auto-build and artifact scheduling are unchanged.

For G generated lookup sites, C executed polled invocations, R runtime bridge or
resume entries and T resident contexts: emission and code size remain O(G),
lookup execution remains O(C) with two loads instead of two calls, publication
cost is O(R), and extra memory is exactly 16T bytes. No scan, allocation or graph
analysis was added. Poll frequency, recursive program work and stack depth are
unchanged. Two independent pointer values require constant storage/access; this
uses the existing context and lifecycle rather than an additional index. Runtime
refresh may repeat at nested bridges, but each refresh is constant work and no
longer scales with ordinary generated-to-generated calls.

The codegen regression generates 1, 4, 16 and 64 distinct loop helpers in debug
and release. It checks zero relocations to either accessor, at least one gate
load per helper, and exactly one additional gate load per additional helper.
Mutual recursion measurements above cover increasing dynamic call counts and
deferred entry gates. Existing mixed-budget tests cover many resumes/exits.

## Validation perspectives

All below were run locally on Linux, with named tests/filters only.

| # | Perspective | Evidence |
| ---: | --- | --- |
| 1 | No generated lookup calls in debug | sync_poll_gates_have_no_lookup_calls_and_scale_linearly |
| 2 | No generated lookup calls in release | same, release branch |
| 3 | Increasing generated loop-helper fan-out | same, 1/4/16/64 |
| 4 | Deferred recursive entry lookup | same, mutual-recursion example |
| 5 | Debug recursive values and ordinary-thread loop | sync_poll_gates_runnable_recursive_example |
| 6 | Release recursive values and ordinary-thread loop | same, release branch |
| 7 | Thread context null counter and non-null GC pointer | sync_gates_thread_nested_and_restored_contexts |
| 8 | Nested context restoration | same |
| 9 | Exact budget trip points and runtime call counts | inline_countdown_bounds_runtime_calls_and_keeps_budget_trip_points |
| 10 | Time-quantum expiry | inline_countdown_still_honors_the_time_quantum |
| 11 | Resume after runtime wait | sync_inline_grant_is_invalidated_after_runtime_wait |
| 12 | Nested scheduler quantum | sync_inline_grant_is_invalidated_after_nested_quantum |
| 13 | Batched async/sync budget sharing | batched_async_and_sync_share_exact_budget |
| 14 | Suppressed preemption preserves partial debt | native_batched_no_preempt_preserves_partial_debt |
| 15 | Mixed sync/async exact budgets | mixed_sync_async_safepoints_share_exact_budget |
| 16 | Mixed no-preempt region | mixed_sync_async_no_preempt_region_preserves_budget |
| 17 | Mixed deadline preservation | mixed_sync_async_share_deadline_without_rearming |
| 18 | Cooperative boundary null-counter fallback | cooperative_helper_return_settles_exact_sync_budget |
| 19 | Concurrent GC stop and live thread roots via published gate | multi_mutator_stw_keeps_other_thread_roots_alive_through_generated_gate |
| 20 | Peer progress and recursive cancellation cleanup | synchronous_loop_yields_to_peer_and_cancellation_unwinds_every_frame |
| 21 | Pinned thread/nested context installation | bridge_installs_thread_and_nested_stable_contexts |
| 22 | Root prefix displacement preserves storage | shadow_roots_inline_and_runtime_mutations_share_storage |
| 23 | Leaf closure has no gate | comparison_leaf_lambda_has_no_root_or_poll |
| 24 | Looping/calling closure keeps gates | looping_lambda_keeps_poll; calling_lambda_keeps_root_and_poll |
| 25 | Indirect callees preempt on one worker | indirect_callee_loops_preempt_with_one_worker |
| 26 | Null counter spare rearming | main_thread_loop_rearms_spare_countdown |
| 27 | Ordinary-thread loop services collection | main_thread_array_loop_stops_for_collections |
| 28 | Generated native-task loop cancellation | array_read_loop_on_task_stack_remains_cancellable |
| 29 | Leaf method has no gate | field_getter_adds_no_root_or_poll |
| 30 | Looping/calling method keeps gates | loop_keeps_poll; call_keeps_root_and_poll |
| 31 | Implicit interface return/local retains gates and roots | implicit_interface_return_keeps_poll_and_receiver_root; implicit_interface_local_keeps_poll_and_receiver_root |

The focused ABI test `abi_hash_covers_every_contract_constant` also passed,
verifying fingerprint sensitivity to every new/changed offset.

`cargo fmt --all -- --check` passed. Workspace/all-targets Clippy with warnings
denied passed for x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc,
aarch64-apple-darwin and x86_64-apple-darwin. These are compile/lint checks:
Windows and both macOS targets were not natively executed. Full local test suites
and the full example audit were intentionally not run; CI was not checked.
The runnable example is `example/sync_poll_gates.wi`.
