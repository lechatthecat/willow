# 0001 — Task-owned synchronous stacks

- **Status:** implemented on GNU Linux x86_64 and aarch64; other targets retain E0810.
- **Runtime:** `crates/willow_runtime/src/native_stack.rs`, `scheduler.rs`, `preempt.rs`.
- **Compiler capability:** `ConcurrencyAnalyzer::with_sync_stack_preemption` and `TypeChecker::with_sync_stack_preemption`.
- **Tests:** `tests/integration/native_sync_stack.rs` and runtime native-stack tests.

## Mechanism and supported targets

The runtime uses libc `getcontext`, `makecontext`, and `swapcontext`, rather than
Cranelift's experimental `stack_switch` instruction or hand-written assembly.
The build gate is exactly:

```rust
all(target_os = "linux", target_env = "gnu",
    any(target_arch = "x86_64", target_arch = "aarch64"))
```

GNU libc exposes the required context APIs on these targets. The host x86_64
path is exercised by the runtime and integration tests; aarch64 is enabled by
the same ABI-preserving libc mechanism and still needs a dedicated CI runner.
Musl Linux, macOS, Windows, and other architectures have no enabled native-stack
backend. Their compiler retains E0810 for task calls into synchronous looping
or recursive helpers. No unsupported target silently receives a no-op version
of this safety check.

The existing `stack_switch_capability` tests continue to probe the pinned
Cranelift implementation. They describe that alternative's limitations, not the
runtime mechanism used here. Cranelift's instruction remains unnecessary for
this implementation.

## Stack ownership and scheduler affinity

A poll executes on an 8 MiB writable `mmap` reservation with inaccessible guard
pages at both ends. Physical pages are demand-paged by the kernel. The mapping
and libc contexts stay at stable addresses until the native activation finishes;
references into synchronous frames never relocate.

An initial poll obtains a cached native stack from its worker, creating one if
none is available. A completed poll returns the stack to that worker's cache.
A poll suspended inside a synchronous helper retains its stack in `RuntimeTask`.
This avoids mapping/unmapping a stack on every ordinary async poll.

Synchronous function entries and cycle headers check the task quantum, covering
recursion and loops. Iterative DFS identifies backedge targets, including recovery
edges and irreducible cycles. On expiration the runtime preserves the entire native call
chain and returns `Preempted` to the scheduler. Resumption continues after the
same check. Existing async frame dispatch and cooperative suspension remain in
use when the poll itself returns.
The optimizer may expand a small, nonfaulting scalar loop four iterations at a
time. Every original condition remains, and the expanded cycle retains its poll;
this adds at most three bounded scalar iterations between interruption checks.

Each generated invocation caches whether it runs on a task-owned native stack
and the stable address of the GC stop gate. Polls atomically reload that gate and
call the runtime only when a native task is active or a collector requests a
stop. Ordinary synchronous loops therefore avoid repeated TLS/runtime calls
while remaining visible to concurrent GC. The gate's value is never cached;
nested scheduler drives restore native activity before returning to the caller.

**Suspended native stacks do not migrate between OS threads.** Source-level
Send checks alone do not prove that Rust callback frames, TLS state, or native
borrows below a generated helper can migrate. The scheduler therefore retains
persistent OS workers, including worker 0, across separate scheduler drives.
The initiating thread coordinates the drive and cooperates with GC while
waiting. Queue claims return a suspended native task to its owning worker;
resume also asserts the original OS thread identity. A later drive keeps enough
workers active to service all suspended stack owners even if the requested
worker count decreases. Once a native poll finishes, a later poll may use
another worker.

Nested scheduler drives execute on the same owning worker. They do not replace
the native-stack preemption mechanism with recursively nested scheduler calls.

## Roots, task context, and overflow

Before suspension, `park_current_roots` transfers the poll's thread-local
shadow-root suffix to the collector's parked-slot registry. Slots continue to
point into the stable mapped stack. Both major and minor root scans include
parked slots. `resume_parked_roots` transfers them into the resuming thread's
root stack before generated code continues. Transfers have no intervening GC
safepoint; the mapping outlives its registration.

The call trace is exchanged at each switch. Panic state already belongs to the
task through `PanicContext`. Cancellation state and cleanup nesting live in the
native stack, not worker TLS. Each resumed quantum gets a fresh scheduling
budget; no-preempt regions cannot suspend through this hook.

No Rust reference to `NativeStack` spans `swapcontext`: scheduler and trampoline
code use stable raw pointers at this boundary. Context objects are initialized
after allocation because libc may keep pointers to interior register storage.
Rust unwinding is not used to implement language cancellation or panic;
`extern "C"` entry points do not permit a Rust unwind across the context boundary.

The existing alternate signal stack and native stack-overflow handler switch
the active guard range with the context and restore the worker's range on return.
Cranelift's enabled stack probes therefore encounter the task guard before an
adjacent mapping. Overflow remains fatal and uses the existing native-stack
overflow diagnostic; it is not a recoverable language panic.

## Cancellation and cleanup

`willow_sync_safepoint` returns a cancellation signal separately from yielding.
`willow_sync_cancelled` propagates that sticky signal after user calls without
requiring a panic. Generated synchronous cancellation edges execute active
defers, release their own roots and debug/defer context, and return a neutral ABI
value to the caller's cancellation edge. They never resume ordinary source code.

`willow_sync_cleanup_enter` / `leave` suppress cancellation checks while cleanup
runs, but allow scheduling preemption. A recovered panic in a cleanup helper
does not clear cancellation. An unrecovered cleanup panic remains fatal.

The outer generated poll invokes `willow_sync_poll_cancel_cleanup`, which takes
the frame's registered cancellation callback exactly once. It then unwinds poll
roots and returns; the scheduler finalizes the task as cancelled, or panicked
if cleanup raised an unrecovered panic. A task already suspended at an async
boundary runs its initial cancellation callback on a native cleanup stack too,
so synchronous helpers in async defers can yield. Runtime-defined polls without
a generated epilogue are requeued into their registered cancellation callback
before terminal finalization.

## Verification

```sh
cargo build -p willow_runtime
cargo test -p willow_runtime native_stack::tests
cargo test --test integration native_sync_stack
cargo test --test integration stack_switch
```

The native tests exercise recursive value preservation, parked roots during
collection, trace isolation, persistent OS-thread affinity across separate
drives, fairness with more busy helpers than workers, cancellation cleanup
ordering, runtime callback cleanup, and panic/recover during cancellation.

Cross-platform context implementations and platform CI remain required before
lifting E0810 on the other targets. They must satisfy the same non-moving stack,
root registration, OS affinity, and cleanup contracts; Windows additionally
requires correct stack-bound/TIB and guard-page handling.
