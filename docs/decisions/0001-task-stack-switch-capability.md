# 0001 — Task-stack switch mechanism, per platform

- **Status:** accepted
- **Issue:** `willow-38w.2.2` (A2, cross-platform task-stack capability spike)
- **Spec:** `requirements/willow_async_completion_spec.md` §5, §6, §7
- **Blocks:** `willow-38w.2.3` (A3, lazy non-moving task sync stack)
- **Verified against:** `cranelift-codegen` 0.134.3, as pinned in `Cargo.lock`
- **Probes:** `tests/integration/stack_switch_capability.rs`

This document lives in `docs/`, not `requirements/`, because `requirements/` is
gitignored (`.gitignore:12`). A record that A3 depends on has to travel with the
repository. See `willow-uy4f`.

---

## 1. Question

Spec §7.5 requires a capability spike, before any implementation, answering:

1. Can Cranelift-generated code express or call the required stack transition on
   Linux?
2. On Windows?
3. On macOS arm64?
4. Does the mechanism preserve the exact target ABI?
5. Does it coexist with stack probes?

And spec §5.2 fixes where the transition happens: at exactly one boundary, when
task context enters a call chain classified `TASK_PREEMPTIBLE_SYNC`. Not on every
poll, and not on every call.

---

## 2. Decision

**Do not build on Cranelift's `stack_switch` instruction. Implement the
transition as a single per-target runtime assembly trampoline exported from
`crates/willow_runtime`.**

| Platform | Architecture | Switch mechanism | Stack memory | Status |
| --- | --- | --- | --- | --- |
| Linux | x86_64 | runtime trampoline (System V) | POSIX scheme below | supported |
| Linux | aarch64 | runtime trampoline (AAPCS64) | POSIX scheme below | supported |
| macOS | aarch64 | runtime trampoline (Darwin AAPCS64) | POSIX scheme below | supported |
| macOS | x86_64 | runtime trampoline (System V) | POSIX scheme below | **conditional** — see below |
| Windows | x86_64 | runtime trampoline (Win64) **plus TIB maintenance** | Windows scheme below | supported, with the §6 obligation below |

macOS x86_64 is marked conditional because spec §7.4 makes Intel Mac support
conditional on CI hardware availability, and no CI exists yet
(`willow-38w.2.10`). The mechanism is identical to Linux x86_64, so the cost of
supporting it is a runner, not code.

### 2.1 Stack memory mechanism, decided per OS

**POSIX (Linux, macOS).** One `mmap` of the whole reservation as
`PROT_READ | PROT_WRITE`, `MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE`, with a
`PROT_NONE` guard region `mprotect`ed at the low end at creation time.

Laziness is the kernel's demand paging, not the runtime's. A page costs physical
memory when it is first touched, and `MAP_NORESERVE` keeps the untouched
remainder off the commit charge, so a large per-task reservation does not consume
memory it never uses. The runtime therefore makes **no `mprotect` call on the
growth path** and needs **no fault handler to grow**: there is nothing to commit,
because the mapping is already writable. Growth is a store to a fresh page.

The rejected alternative was `PROT_NONE` reservation plus a `SIGSEGV`/`SIGBUS`
handler that `mprotect`s pages in on demand. It buys nothing over `MAP_NORESERVE`
— both are lazy — and costs a signal handler on the critical path of every
stack growth, on a runtime that installs no signal handlers today.

**Windows.** `VirtualAlloc` with `MEM_RESERVE` over the whole range, `MEM_COMMIT`
for a small initial region, and one `PAGE_GUARD` page immediately below the
committed region. This is the shape the OS thread-stack grower already
understands: touching the guard page raises `STATUS_GUARD_PAGE_VIOLATION`, the
kernel commits the next page, arms a new guard page, and lowers `StackLimit`.
`MAP_NORESERVE`'s equivalent does not exist on Windows — reserved-but-uncommitted
pages are not writable — so this path is the only lazy one, and it is why §6's
TIB maintenance is load-bearing rather than cosmetic.

**Both:** overflow runs into the guard region rather than into adjacent memory,
which is what §6.1 requires. Turning that fault into the *diagnostic* §6.3 asks
for is a separate obligation, not a free consequence — see §7.

The runtime keeps Cranelift's `ControlContext` layout — `{ stack_pointer,
frame_pointer, instruction_pointer }` at offsets 0/8/16 — even though it does not
use Cranelift's instruction. If a later Cranelift gains arm64 and Windows
support, the mechanism can be swapped without changing the runtime's data
structures or the compiler's view of the boundary.

Per §7.5's closing requirement, source-language semantics do not depend on which
path is selected. The boundary is the same either way.

---

## 3. Evidence

Every claim below is pinned by a probe in
`tests/integration/stack_switch_capability.rs`, so a Cranelift upgrade that
changes one of them fails the suite rather than silently invalidating this
record. To regenerate the quoted text:

```bash
cargo test --test integration stack_switch_capability_report -- --ignored --nocapture
```

Output on x86_64 Linux against 0.134.3:

```text
none:               refused -- Unsupported("should be implemented in ISLE: inst = `v3 = stack_switch.i64 v0, v1, v2`, type = `Some(types::I64)`")
basic:              compiled, 111 bytes
update_windows_tib: refused -- Unsupported("should be implemented in ISLE: inst = `v3 = stack_switch.i64 v0, v1, v2`, type = `Some(types::I64)`")
```

### Finding 1 — `stack_switch` exists, and is x64-only

`stack_switch` lowers only in `src/isa/x64/`. No other backend in the tree
mentions it at all: not aarch64, not riscv64, not s390x, not pulley. Cranelift's
own doc comment says so directly:

> The instruction is experimental and only supported on x64 Linux at the moment.

Apple Silicon is aarch64 and is a release blocker (§7.4). There is no aarch64
lowering to select, so this alone rules the instruction out as *the* mechanism.

### Finding 2 — it is off by default and must be opted into

`stack_switch_model` defaults to `"none"`, under which the instruction does not
lower. The setting is a closed enum, so a typo fails at configuration time.

### Finding 3 — `update_windows_tib` is declared but unimplemented

The setting accepts `"update_windows_tib"`, and `StackSwitchModel` declares an
`UpdateWindowsTib` variant in `src/prelude_lower.isle`. There is no lowering rule
for it: the x64 rule matches `StackSwitchModel.Basic` and nothing else, so
selecting it produces the same `Unsupported` refusal as `none`.

This is the §7.3 requirement, and Cranelift 0.134.3 cannot meet it. Windows needs
the Thread Information Block's stack bounds to describe the *task* stack while
code runs on it; otherwise guard-page growth and any stack probing operate
against the OS thread's stack.

### Finding 4 — the instruction does not provide first entry onto a fresh stack

From the instruction documentation:

> Stack B is a newly initialized stack. The necessary initialization is
> platform-dependent and will generally involve running some kind of trampoline
> to start execution of a function on the new stack.

So a runtime trampoline is required **regardless of which mechanism is chosen**.
Adopting `stack_switch` means building the trampoline anyway *and* threading a
new instruction through the backend, for a subset of platforms. That is strictly
more work for strictly less coverage.

### Finding 5 — register preservation is real, and comes from regalloc

The x64 emission preserves nothing itself. It marks the instruction as clobbering
`ALL_CLOBBERS` **minus the payload register** — the lowering builds the full set
and then calls `clobbers.remove(payload_register())`, because the payload has to
survive the switch to be delivered to the other side. Everything else, on every
calling convention, is clobbered, so the register allocator spills live values to
the *current* stack before the switch. The emitted code exchanges only `RSP`,
`RBP`, and `RIP` through the two contexts.

The trampoline must reproduce this property by hand: save every callee-saved
register of the target ABI onto the outgoing stack before exchanging SP, and
restore on the way back.

### Finding 6 — the payload register is System V's first argument

`payload_register()` is hardcoded to `rdi`. On Win64 the first integer argument is
`rcx`, and `rdi` is *nonvolatile*. The convention is internal, so it is not fatal
by itself, but it is a second independent sign the instruction was designed for
one platform.

### Finding 7 — the model is a lowering-time gate only

`stack_switch` passes the IR verifier under every model, including `none`.
Nothing catches a misconfiguration when the IR is built. Any code that emits it
must check the flag itself.

### Finding 8 — Willow emits no stack probes today

`enable_probestack` defaults to `false`, and nothing in `src/` or `crates/` sets
it. Cranelift-generated Willow code therefore contains no probes on any platform.

This is a **pre-existing** gap, not one A3 introduces: a Willow function with a
large frame can already move the stack pointer past a guard page without touching
it. It becomes more visible with manually managed task stacks, and is tracked
separately (§7 below).

### Finding 9 — the runtime has no virtual-memory primitives yet

`crates/willow_runtime` depends on `libc`, `ryu`, and `socket2`. There is no
`mmap`, `mprotect`, or `VirtualAlloc` anywhere in it. A3 builds the platform
memory layer from scratch, and needs a Windows API dependency that is not
currently declared.

---

## 4. Answers to §7.5

| Question | Answer |
| --- | --- |
| 1. Linux? | Yes on x86_64 via `stack_switch` under `basic`; **no** on aarch64. A runtime trampoline covers both. |
| 2. Windows? | **No.** The required model is declared but has no lowering rule. |
| 3. macOS arm64? | **No.** The aarch64 backend has no `stack_switch` at all. |
| 4. Exact target ABI preserved? | By `stack_switch`, yes on x64 — regalloc clobbers everything except the payload register, so nothing else survives in a register across it. A hand-written trampoline must preserve callee-saved registers explicitly to reach the same guarantee. |
| 5. Coexists with stack probes? | Untested, and currently vacuous: Willow emits no probes (Finding 8). Cranelift's probestack calls a libcall that assumes the thread's stack limit, so if probes are ever enabled they must be reconciled with the task stack. |

Per §7.5: since the answer is "no" for two of three platforms, "provide minimal
per-target runtime shims/assembly."

---

## 5. ABI decision record (§7.1)

These invariants are common to every target and are the contract A3 implements
against. No Willow-generated function may be able to tell whether it runs on an
OS-thread stack or a task stack, except through scheduler timing.

**Common:**

1. `ControlContext` is `#[repr(C)] { stack_pointer: *mut u8, frame_pointer: *mut u8, instruction_pointer: *mut u8 }`, offsets 0/8/16. Chosen to match Cranelift's, so the mechanism stays swappable.
2. Every callee-saved register of the target ABI is saved on the outgoing stack before SP is exchanged, and restored after the switch back. This includes callee-saved vector/SIMD state where the ABI requires it.
3. Stack alignment at the switch point is whatever the target ABI requires at a call boundary, established *before* the first Willow frame runs on the task stack.
4. The current-task binding (TLS) is re-established on the worker side, not carried in a register across the boundary. A task can resume on a different worker thread.
5. Return continuation is the instruction after the switch. Both directions are ordinary returns to that point; neither direction is an unwind.
6. Willow language panic remains compiler-emitted propagation. The trampoline does not participate in SEH or DWARF unwinding, and §7.6's portability advantage is preserved. Do not replace Willow panic propagation with platform unwinding.
7. **No Rust unwind may cross the trampoline.** This is separate from invariant 6: invariant 6 is about the *language's* panic, this one is about the *runtime's*. `crates/willow_runtime` is built with the default `panic = "unwind"` — neither `Cargo.toml` sets `panic`, and three tests rely on `catch_unwind` (`scheduler.rs`, `gc.rs`, `gc_mark_queue_tests.rs`), so switching the workspace to `panic = "abort"` is not available. A runtime ABI function called from Willow code executing on a task stack can therefore panic, and that unwinder would walk into a hand-written assembly frame with no unwind information. That is undefined behavior, and on Windows it corrupts the SEH chain rather than merely failing to find a handler.

   A3 must therefore wrap the task-stack side of the trampoline in `catch_unwind` (with `AssertUnwindSafe`, since the switch already forfeits Rust's aliasing story) at the point where the fresh stack first enters Rust, and convert a caught panic into the runtime's existing fatal-abort path *before* switching back. Aborting on the task stack is correct: the worker's state is intact and reachable, and the diagnostic can name the helper.

8. **The switch frame terminates unwinders and stack walkers.** The trampoline emits `.cfi_undefined rip` (x86_64 SysV), `.cfi_undefined 30` for LR (aarch64), and an SEH `.seh_proc`/`.seh_endproc` pair with no handler (Win64), so a debugger, profiler, or crash reporter that walks the task stack stops cleanly at the boundary instead of following whatever the stack slot happens to hold. Without this, a sampling profiler attached to a Willow process walks off the end of every task stack it samples. Invariant 7 keeps unwinders out; this one keeps *walkers* from producing garbage.

**x86_64 System V (Linux, macOS):**

- callee-saved: `rbx`, `rbp`, `r12`–`r15`
- 16-byte alignment at the call boundary, with the return address making `rsp % 16 == 8` on entry
- the 128-byte red zone must not be assumed valid across the boundary

**x86_64 Win64 (Windows):**

- callee-saved: `rbx`, `rbp`, `rdi`, `rsi`, `r12`–`r15`, `xmm6`–`xmm15`
- 32 bytes of caller shadow space must be reserved before any call on the task stack
- 16-byte alignment at the call boundary
- TIB fields must describe the task stack while executing on it — see §6

**aarch64 (Linux, macOS/Apple Silicon):**

- callee-saved: `x19`–`x28`, `x29` (FP), `x30` (LR), `d8`–`d15`
- SP must be 16-byte aligned at all times, not merely at call boundaries — a misaligned SP faults on Darwin
- `x18` is reserved by the platform on Darwin and must never be used by the trampoline
- pointer authentication is not enabled in Willow today; if it is ever enabled, LR signing must be consistent across the switch, and this record must be revisited

---

## 6. Windows `__chkstk` (§7.3)

Spec §7.3 forbids pretending Windows stack probing does not exist. It is
addressed, not deferred, as follows.

**The mechanism:** the Windows trampoline saves and replaces three TIB fields
around the switch, restoring them when control returns to the worker stack:

| Field | x64 offset from `gs` | Meaning |
| --- | --- | --- |
| `StackBase` | `0x08` | high address of the current stack |
| `StackLimit` | `0x10` | low address currently committed |
| `DeallocationStack` | `0x1478` | low address of the whole reservation |

**Where the saved triple lives.** The three original values are pushed into the
switch frame **on the worker stack** — the same frame that holds the callee-saved
registers from §5 invariant 2 — and never into task state. They describe the OS
thread the switch left, not the task, and a task is free to resume on a different
worker (§5 invariant 4). Storing them per task would mean thread A's stack bounds
being written into thread B's TIB on resume, which hands B a `StackLimit` pointing
into an unrelated thread's mapping: guard-page growth then commits pages in the
wrong stack and `__chkstk` probes the wrong region. Keeping them in the switch
frame makes the save and the restore structurally the same thread by
construction, because the restore is reached only by returning to that frame.

For the same reason, the task stack's own bounds are recomputed from the task's
reservation on every entry rather than cached from the previous entry.

With those pointing at the task stack, the two consumers behave correctly:

1. **Guard-page growth.** The kernel's guard-page handler commits the next page
   and moves `StackLimit` down. Pointed at the task stack, ordinary growth works
   the same way it does for a thread stack.
2. **`__chkstk` / Cranelift probestack.** A probe walks from the current SP down
   to the requested new SP, touching one page per step. It is correct as long as
   the memory below SP is the task stack's committed-or-guarded region, which is
   exactly what the TIB fields now describe.

**Why not eager full commit instead?** It would sidestep guard pages but violates
§5.3's laziness goal and caps concurrency by committed memory. Reserve-and-commit
with correct TIB bounds keeps both.

**Probe required before Stage C acceptance**, per §7.3, and tracked as part of
A3's acceptance rather than assumed:

- a Cranelift function with a large native stack frame
- executed on the Willow task stack
- forced stack growth through the guard page
- preemption and resume afterwards
- correct in both debug and release builds

**Separately:** `enable_probestack` is `false` today (Finding 8), so no such probe
is emitted at all. Turning it on is a decision with its own blast radius and is
filed as its own issue rather than folded into A3.

---

## 7. Non-moving requirement (§6.1)

Carried into this record explicitly: **the task stack must not relocate.**

> Once a synchronous frame has an address that may be observed by Willow
> reference lowering, that frame's storage address never changes until the frame
> returns.

The §2.1 memory strategy satisfies this by construction on all three platforms.
A single contiguous reservation is made per task sync stack, and pages are backed
inside that reservation on demand — by the kernel's demand paging on POSIX, by
guard-page commit on Windows. Neither moves what is already backed, so an address
handed to reference lowering stays valid for the frame's whole lifetime.

Consequences A3 inherits:

- No `realloc`-style growth, and no copying collector applied to the task stack.
- A segmented implementation is permitted by §6.2 only if old segments never move
  and raw references into them stay valid. The single-reservation design is
  preferred precisely because it avoids that obligation.
- Exhaustion is deterministic (§6.3): the reservation has a fixed maximum, and the
  guard region at its low end means running past it faults instead of writing into
  whatever mapping follows. Silent corruption of adjacent memory is never
  permitted. Whether the condition is recoverable is a separate decision,
  deliberately not made here.

### 7.1 The overflow diagnostic is a separate obligation

§6.3 asks for a Willow runtime fatal diagnostic in the shape of
`task stack overflow while executing synchronous helper \`foo\``. **The guard
region does not produce that message.** It produces a fault. Turning the fault
into the message costs real machinery, and this record states it rather than
letting A3 discover it:

- **POSIX.** A `SIGSEGV`/`SIGBUS` handler installed with `SA_ONSTACK`, plus a
  `sigaltstack` on **every worker thread** — the ordinary handler stack is the
  stack that just overflowed, so a handler without `SA_ONSTACK` faults again and
  the process dies with no output. The handler compares `si_addr` against the
  faulting task's guard region to distinguish a stack overflow from a genuine
  memory bug, and must chain to any previously installed handler for everything
  else. The runtime installs **no** signal handlers today: `grep` for
  `sigaltstack|SIGSEGV|sigaction` across `crates/willow_runtime/src` and `src`
  finds nothing but one comment.
- **Windows.** A vectored exception handler matching
  `EXCEPTION_STACK_OVERFLOW`, which fires on the *last* guard page, plus
  `SetThreadStackGuarantee` so the handler has room to run.
- **Both.** The handler runs in async-signal / exception context, so naming the
  helper means reading precomputed state, not formatting through the normal
  diagnostic path.

Until that exists, overflow is a plain fault: memory-safe and non-corrupting,
which is what §6.1 actually requires, but silent. Filed as `willow-38w.2.12` so
A3 can ship the memory-safety guarantee without being blocked on the message.

---

## 8. What A3 must build

1. A per-task, lazily created stack reservation with a guard region, using the
   §2.1 mechanism for each OS: `mmap` RW + `MAP_NORESERVE` + a `PROT_NONE` guard
   on POSIX, `VirtualAlloc` reserve + partial commit + `PAGE_GUARD` on Windows.
2. A per-target assembly trampoline exported through the runtime ABI, covering
   first entry onto a fresh stack and switch/resume between stacks.
3. TIB save/replace/restore on Windows, with the saved triple in the switch frame
   on the worker stack (§6).
4. A `ControlContext` matching §5's layout.
5. The §5 invariant 7 unwind barrier (`catch_unwind` at the task-stack entry,
   converting to fatal abort before switching back) and the invariant 8 CFI/SEH
   terminators.
6. Runtime ABI registration for the new symbols, landed together with the ABI
   table, symbol/link tests, and
   `requirements/willow_rust_runtime_abi_inventory.md`, per spec §13.0.
7. A Windows dependency for the virtual-memory and TIB calls, which the runtime
   crate does not currently declare (Finding 9).

**Not A3's, but required before the feature is correct.** Roots held in frames on
a task stack must be reachable from the *task*, not from whichever worker is
running it, or a GC that runs while the task is parked misses them — spec §8.2,
§8.3, and the "GC while parked" cases in §7.2 and §7.4. Willow uses a
compiler-emitted shadow stack (`willow_push_root`, `gc_root_count`,
`coop_shadow_roots`) and never conservative stack scanning, so this does not come
for free from the memory layout: the shadow-root chain has to become task-owned.
That is `willow-38w.2.5` (A5), and A3's stack must be shaped so A5 can attach to
it — in practice, the task's stack handle needs a slot for the shadow-root chain
head that the trampoline saves and restores alongside the callee-saved registers.

---

## 9. Follow-ups filed

- `willow-38w.2.10` — cross-platform CI. Spec §7 requires the same semantics
  implemented and tested on Linux, Windows, and macOS, and there is no CI
  configuration in the repository at all (`.github/` is empty). Part I cannot be
  accepted without it, and Apple Silicon is the architecture the trampoline must
  be hand-written for.
- `willow-38w.2.11` — evaluate `enable_probestack` for Cranelift-generated code
  (Finding 8).
- `willow-38w.2.12` — the §7.1 overflow-diagnostic fault handler. Depends on A3
  for the guard region; A3 ships the memory-safety guarantee without it.

---

## 10. Re-verifying this record

```bash
cargo test --test integration stack_switch
```

Twelve probes. If one fails after a Cranelift bump, read it as an instruction to
revisit this document — most likely because a platform gained support, which
would be good news.

Five of them read the pinned Cranelift source out of the Cargo registry. If that
checkout is missing they **fail**, rather than skipping: a probe that quietly
passes when it cannot find its input has stopped pinning anything, and this
record would then survive a bump that invalidated it. On a build with no registry
checkout (vendored or offline packaging), opt out deliberately:

```bash
WILLOW_SKIP_CRANELIFT_SOURCE_PROBES=1 cargo test --test integration stack_switch
```

The lowering probes do not skip on any architecture. On a non-x64 host they
assert the *opposite* result — `basic` must be refused — so an Apple Silicon
runner actively confirms Finding 1 instead of staying silent about it.
