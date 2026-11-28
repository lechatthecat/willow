# Runtime instances and embedding

## Current support boundary

Willow programs use the process-default runtime. There is no supported public
API for a host to create, select, drive or destroy additional runtime instances.
The Rust `Runtime` type is crate-private; `Runtime::new` and `Runtime::shared`
are compiled only for internal tests. Exported runtime ABI functions are
implementation interfaces for generated Willow code, not a host embedding API.

Internal tests can drive independent heaps and schedulers concurrently. This
establishes fixture isolation, not support for embedding multiple Willow programs
in one process. Thread-selected fixtures have static lifetime and are recycled;
dropping an `IsolatedRuntime` guard does not destroy its runtime. Other tests use
explicitly owned runtimes and exercise service teardown. Neither arrangement
provides a general host-managed lifecycle.

The thread selector holds a raw pointer justified by a static-lifetime contract.
Threads started by `spawn_serving` inherit their creator's selection. A registered mutator
cannot switch to a different runtime. Foreign threads initially select the
process default; hosts cannot use the internal selector as an attach/detach API.
The process-default static is never dropped. Private-owner teardown joins
persistent workers before dropping collector storage and other services; this
does not establish that arbitrary host callbacks, handles or threads have stopped.

## Gate access and the current performance decision

`Runtime::mark_phase` and `Runtime::card_epoch` compare the owner against the
process default. Its gates retain their exported ABI storage and need no lazy GC
initialization. The non-default branch calls an out-of-line, cold helper that
resolves the owner's lazy collector. The two direct Rust paths are not promised
to have equal cost. Cold describes code layout, not permission to share gates.

Generated barriers follow a different path. `Context::publish_gates` publishes
owner-specific pointers at context creation, thread selection or stack reuse.
Cranelift's `barrier_gate` caches each pointer at generated function entry;
barriers load the atomic gate through it. They do not repeat the runtime identity
comparison or the cold helper on every store. Ordinary context refresh does not
republish already established gates. Correct pointer lifetime and owner selection
are still required.

Keep the current direct-access implementation for internal fixtures. This avoids
adding initialization or pointer indirection to the supported default-runtime
path. Direct and cached access must be measured separately: equal cached-load
cost does not prove equal Rust barrier cost or application throughput. Local
measurement and disassembly evidence belong under `docs/audits/runtime-embedding/`
and are intentionally not distributed with this document. This decision accepts
the cold helper for test fixtures only; it is not performance approval for a
public embedding feature.

## Requirements for a proposed public feature

These are proposed requirements derived from the current implementation, not an
API commitment. Publishing an embedding feature remains the user's decision
after considering these requirements and the local measurements.

| Area | Required contract and evidence before public support |
| --- | --- |
| Creation | An opaque, explicitly owned runtime handle with immutable configuration and a defined initialization/error contract. Creating one owner must not mutate another owner's worker, GC, argument or diagnostic configuration. Define which environment defaults are captured and when. |
| Selection and entry | An explicit host entry/attachment operation that holds a lifetime lease. Define nested same-owner entry, detach, re-entry from callbacks and rejection of cross-owner entry while a mutator, root, native frame or task is active. Do not expose the current raw static-pointer selector. |
| Generated code | Every entry, callback, resume and task-stack reuse must install the matching generated context and restore its caller's context. ABI calls and cached gates must agree on the same owner. Validate both the default and additional owners; a raw exported function address is insufficient. |
| Program state | Define whether one compiled module can be instantiated more than once. Mutable statics, literal slots, initialization flags, descriptors and function values must be owner-local or explicitly constrained. Internal heap isolation does not prove compiled-module instance isolation. |
| Thread ownership | Specify foreign-thread attach/detach, worker inheritance, service leases, TLS cache retirement and thread exit. Prohibit task migration between owners. Define whether sequential host-thread reuse across owners is allowed; prove cache/root/registration cleanup if it is. |
| Values and handles | Roots, futures, tasks, channels, locks, plugin handles and callbacks retain their owning runtime for the required duration. Reject cross-owner managed pointers and handles, or provide explicit copying/serialization with defined ownership. Prevent stale-handle reuse after shutdown. |
| Destruction | Define a live/closing/closed state machine. Closing must reject new entry and work, settle or cancel existing tasks, stop I/O and blocking services, wait for host leases, join workers and collector threads, finalize owned objects, then release heap and contexts. Specify busy/error behavior and self-worker shutdown so it cannot join itself. |
| Shutdown failures | Define behavior for pending I/O, blocked foreign calls, callback re-entry, finalizer failures and concurrent close/entry. A bounded shutdown cannot assume arbitrary foreign calls will terminate. Preserve memory until all permitted users have released it. |
| Process resources | Inventory signal handlers, panic hooks, process exit, environment, native libraries and diagnostic outputs. State which policies remain process-wide; several runtimes in a process do not imply sandboxing or fault isolation. |
| Performance | Compare default and additional owners for direct phase/epoch access, cached generated gates, allocation, write barriers, entry and shutdown. Separate first initialization from warm access, idle from marking, and one owner from concurrent owners. Agree on an application-level budget before accepting a cold direct path; a microbenchmark alone cannot establish it. |
| Compatibility | Keep default ABI data symbols and memory orderings valid. Version any public handle/context contract. Verify applicable target builds and report actual native execution separately. |

Validation for a public lifecycle must include concurrent owners, nested host
entries, foreign-thread exit, attempted owner switching during registration,
pending work at close, callback/finalizer re-entry, repeated create/close and
stale/cross-owner handles. Synchronize through explicit signals, not sleeps.
Run generated programs against each owner, including literals and mutable
statics, and verify that closing one owner leaves its peers operational.

The current default runtime and fixture helpers remain unchanged. No public
constructor, runtime handle, lifecycle API or embedding guarantee is introduced.
