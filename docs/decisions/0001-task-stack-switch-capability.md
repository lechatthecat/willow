# 0001 — Task-owned synchronous stacks

- **Status:** implemented on GNU/Linux x86_64 and aarch64, macOS x86_64 and aarch64, and Windows x86_64 MSVC. Other targets retain E0810.
- **Runtime:** `crates/willow_runtime/src/native_stack.rs`, `scheduler.rs`, `preempt.rs`.
- **Compiler capability:** `ConcurrencyAnalyzer::with_sync_stack_preemption` and `TypeChecker::with_sync_stack_preemption`.
- **Tests:** `tests/integration/native_sync_stack.rs` and runtime native-stack tests.

## Mechanism and supported targets

The runtime uses `corosensei` 0.3.4 for non-moving, guarded coroutine stacks
and ABI-preserving context switches on all enabled targets. This replaces the
GNU libc `ucontext` backend so Linux exercises the same runtime integration as
macOS and Windows. Cranelift's experimental `stack_switch` is not required.

The build gate is exactly:

```rust
any(
    all(target_os = "linux", target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
)
```

The compiler, scheduler, safepoints, and integration tests use this same gate.
All four native CI targets accept task calls into synchronous looping and
recursive helpers and run the same preemption and cancellation tests. GNU/Linux
aarch64 is enabled but still needs a dedicated native CI runner. Musl Linux and
targets outside the gate retain E0810; their safety check is not a no-op.

The existing `stack_switch_capability` tests continue to probe the pinned
Cranelift implementation. They describe that alternative's limitations, not the
runtime mechanism used here. Cranelift's instruction remains unnecessary for
this implementation.

## Stack ownership and scheduler affinity

A poll executes on a stack with 8 MiB usable capacity and a lower guard.
On Unix, `corosensei::stack::DefaultStack` uses `mmap` and `mprotect`.
On Windows it uses `VirtualAlloc`, reserves stack-growth guard pages and the
current thread's overflow guarantee, and saves/restores the TEB stack bounds,
deallocation stack, and guaranteed bytes during context switches. Runtime
thread protection is installed before allocation so task stacks inherit the
64 KiB overflow guarantee. Stack storage stays at a stable address until the
native activation finishes; references into synchronous frames never relocate.

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

No Rust reference to the whole `NativeStack` spans a context switch:
scheduler and trampoline code use stable raw pointers, and resumption borrows
only the coroutine field. Rust unwinding is not used to implement language
cancellation or panic; `extern "C"` entry points do not permit a Rust unwind.
Dropping a stack requires all generated calls to have returned and all parked
roots to have been removed. Only the idle trampoline is discarded at teardown;
no generated cleanup is bypassed.

On Linux and macOS the alternate signal stack and overflow handler switch the
active guard range with the context and restore the worker's range on return.
On Windows the backend maintains native TEB bounds and growth guards, and the
vectored exception handler diagnoses stack exhaustion. Cranelift's stack
probes encounter the task guard before exhausting the reservation. Overflow
remains fatal and uses the existing native-stack-overflow diagnostic; it is
not a recoverable language panic.

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

Additional runtime tests check fatal overflow on a task stack, floating-point
locals across repeated suspension and cache reuse, and Windows TEB stack-bound
restoration. These tests and the native helper integration suite run on Linux,
macOS Apple Silicon, macOS Intel, and Windows MSVC in CI. Cross-compilation
checks compilation only; native CI results provide platform execution evidence.
