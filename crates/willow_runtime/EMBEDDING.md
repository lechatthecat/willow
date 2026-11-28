# Runtime instances and embedding

## Current state

A Willow program runs on the process-default runtime. There is no public API
for a host to create, select, drive or destroy other runtimes. The Rust
`Runtime` type is crate-private, and `Runtime::new` / `Runtime::shared` exist
only in test builds. The exported runtime ABI is for generated Willow code, not
for host applications.

Tests use extra runtimes to run independent heaps and schedulers in parallel.
Thread-selected test runtimes live for the whole process and are reused;
dropping an `IsolatedRuntime` guard doesn't destroy its runtime. Other tests
own their runtime explicitly and exercise teardown.

The thread's current runtime is stored as a raw pointer to a runtime that lives
for the whole process. Threads started by `spawn_serving` inherit their
creator's runtime, and a registered mutator can't switch to another one.
Foreign threads start on the default runtime. The default runtime is never
dropped. Tearing down a privately owned runtime joins its workers before
freeing the collector and other services.

## Gate access

`Runtime::mark_phase` and `Runtime::card_epoch` check whether the owner is the
default runtime. The default runtime reads its gates directly from the
exported ABI storage. Other runtimes take a cold, out-of-line path that
initializes their collector lazily, so the two paths don't cost the same.

Generated write barriers don't take either path. `Context::publish_gates`
publishes the owner's gate pointers when a context is created, a thread selects
a runtime or a stack is reused. Each generated function loads the pointer once
on entry (Cranelift's `barrier_gate`), and barriers read the atomic gate
through it, with no per-store runtime check.

The cold path is fine for tests and keeps the default runtime fast. A public
embedding API would need direct and cached gate access measured separately.

## What public embedding would need

| Area | Requirement |
| --- | --- |
| Creation | An opaque, owned runtime handle with fixed configuration and defined errors. Creating one runtime must not change another's worker, GC, argument or diagnostic settings. Define when environment defaults are read. |
| Entry | An explicit host entry/attach operation holding a lease. Define nested entry, detach, re-entry from callbacks, and rejection of entry into a different runtime while one is active. Don't expose the raw thread selector. |
| Generated code | Every entry, callback, resume and stack reuse installs the matching context and restores the caller's. ABI calls and cached gates use the same owner. Test both the default and other runtimes. |
| Program state | Decide whether one compiled module can be instantiated more than once. Mutable statics, literal slots, init flags, descriptors and function values must be per-runtime or restricted. |
| Threads | Specify foreign-thread attach/detach, worker inheritance, service leases, TLS cache cleanup and thread exit. Tasks never move between runtimes. If a host thread can be reused across runtimes, clean its caches, roots and registrations. |
| Values and handles | Roots, futures, tasks, channels, locks, plugin handles and callbacks keep their runtime alive as long as needed. Reject managed pointers and handles from another runtime, or copy them explicitly. Reject handles used after shutdown. |
| Destruction | A live → closing → closed state machine. Closing rejects new work, settles or cancels tasks, stops I/O and blocking services, waits for host leases, joins workers and collector threads, finalizes objects, then frees the heap and contexts. A worker must not try to join itself. |
| Shutdown failures | Define behavior for pending I/O, blocked foreign calls, callback re-entry, failing finalizers and close racing with entry. Foreign calls may never return, so memory stays alive until every user has released it. |
| Process resources | List what stays process-wide: signal handlers, panic hooks, process exit, environment, native libraries and diagnostic output. Several runtimes in one process aren't a sandbox. |
| Performance | Compare default and other runtimes for gate access, generated barriers, allocation, entry and shutdown; first use versus warm, idle versus marking, one versus several runtimes. Set an application-level budget first. |
| Compatibility | Keep the default ABI data symbols and memory orderings. Version any public handle or context format. |

Tests for a public lifecycle should cover concurrent runtimes, nested entry,
foreign-thread exit, switching runtimes during registration, pending work at
close, callback and finalizer re-entry, repeated create/close, and stale or
foreign handles, using explicit synchronization rather than sleeps. Generated
programs, including literals and mutable statics, should run on each runtime,
and closing one runtime must leave the others working.
