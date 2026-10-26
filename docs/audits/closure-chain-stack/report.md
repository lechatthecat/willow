# Closure-chain recursion: native stack budget

Measured 2026-10-26 on Linux x86-64, Rust 1.96.0, source revision
`875a3c9b0ff3028723c883af5bf808e5bc3c491c`, release compiler and runtime rebuilt together.

## Outcome and limit

This is the documented-limit resolution, not a compiler optimization or a promise
of bounded-stack recursion. The original continuation-closure reproduction reaches
100,000 in release with a **32 MiB process stack**. It still overflows with 8 MiB.
No compiler/runtime semantics or default stack sizes were changed.

A task owns a separate 8 MiB native stack (`native_stack.rs`, `NativeStack::new`).
Raising the process stack limit does not enlarge it: the async-main reproduction
still overflows at 100,000. Both synchronous and asynchronous closure reproductions
passed at 50,000 on this machine, but that is not a portable supported maximum.
Other callers, debug builds, additional captures, compiler versions, ABI and
platform can change the available depth. Use iterative state/explicit work storage
for unbounded recursion; do not rely on tail-call elimination here.

The older observation that plain recursion reaches one million is not a current
baseline guarantee. The current generated plain frame is 32 bytes, so one million
active calls alone would require about 32 MB.

## Reproduce

From the repository root, on Linux x86-64 with GNU objdump and a hard stack limit
of at least 32 MiB:

```sh
rtk cargo build --release --bin willow
rtk proxy python3 docs/audits/closure-chain-stack/measure.py
rtk proxy target/release/willow run example/closure_chain_stack.wi --release
```

The example uses 10,000 so it runs with the ordinary stack budget. The audit script
substitutes depths 1,000, 10,000, 25,000, 50,000 and 100,000, builds each in release,
and executes each with explicit 8 and 32 MiB limits. It also creates the plain and
async-main variants. All generated sources and binaries stay in an automatically
removed temporary directory; resource limits apply only to child processes.
`measurements.jsonl` records the 30 executions and generated frame measurements.
A successful execution must print exactly its input depth. Unexpected failures
or any synchronous failure with 32 MiB fail the audit. Expected overflow results
are recorded explicitly, not counted as successful executions.

## Measured generated frames

GNU objdump prologues have one saved frame pointer (8 bytes), a call return address
(8 bytes), and the following explicit stack reservation. Callee-saved registers
are stored inside that reservation and must not be counted twice.

| Body | Reservation | Total per active call | Disassembled instruction lines |
| --- | ---: | ---: | ---: |
| Plain recursive function | 16 B | 32 B | 93 |
| Closure-chain builder | 64 B | 80 B | 320 |
| Capturing continuation | 64 B | 80 B | 156 |
| Identity continuation | 0 B | 16 B | 6 |

Instruction-line counts include objdump byte-continuation/alignment lines; they
are a deterministic code-size proxy, not retired instruction counts. Counts and
frames were identical at all five depths, for both main variants. These are
static machine-frame measurements, not instrumented stack-pointer samples.

The builder's recursive call does not replace its own frame. At its base case,
the entire builder chain remains live while the continuation chain executes.
Consequently the inferred maximum generated stack is approximately
`80 * (n + 1) + 80 * n + 16 = 160n + 96` bytes, plus callers/runtime transients.
Plain recursion needs `32 * (n + 1)` plus callers/transients. Thus the present
ratio is about **5x**, not a measured 10x. At 100,000, closure generated frames
alone need 16,000,096 bytes (about 15.26 MiB); 8 MiB cannot accommodate them.
32 MiB leaves room for callers and runtime work, and passed the actual execution.

| Variant | 8 MiB, depths through 50,000 | 8 MiB, 100,000 | 32 MiB, 100,000 |
| --- | --- | --- | --- |
| Plain, sync main | pass | pass | pass |
| Closure, sync main | pass | native stack overflow | pass |
| Closure, async main | pass | task stack overflow | task stack overflow |

The process-stack setting has no effect on the separately allocated task stack.
The matrix is 27 successful executions and three observed, diagnostic-bearing
stack overflows. It brackets the closure threshold; it does not identify an exact
maximum depth.

## Implementation and complexity audit

Reviewed the generated builder and lifted continuation, LIR direct/indirect call
emission in `src/backend/cranelift/lir_gen.rs`, root registration/cleanup in
`src/backend/cranelift/emit.rs`, and task stack allocation in
`crates/willow_runtime/src/native_stack.rs`.

The environment is a GC allocation, not an environment blob copied onto each
native frame. Generated frames retain roots and saved state. Indirect calls also
carry conditional panic-depth checks and cancellation/stack-helper handling;
cleanup follows the call. Merely changing a call to a jump would discard required
cleanup/GC/panic semantics. This documentation does not remove those protocols or
increase every task's reserved memory to mask the limitation.

For depth n, the source executes n+1 builder calls, n capturing-continuation calls,
one identity call, and n capturing-environment allocations. Explicit user work,
closure heap storage and active generated stack are O(n); compiler code emitted
for the fixed source shape is O(1) in n. GC/runtime internal total cost is not
claimed to be O(n) from these counts. The independently varying dimensions are
recursion depth and capture/body size; only fixed one-closure captures were
measured. There is no newly introduced compiler analysis, repeated scan,
allocation or code-generation path. The audit script runs a fixed 3 x 5 x 2
matrix, building once per variant/depth rather than once per stack setting.

O(n) application work is required for executing n arbitrary continuation bodies.
O(n) native stack is a property of the current lowering, not a theoretical lower
bound: an explicit iterative representation could use bounded native stack with
O(n) heap work storage. General tail-call lowering would require a separate
semantic implementation covering roots, cancellation, panic and cleanup; no
claim of globally minimal compiler/runtime complexity is made here.

This deep single-chain adverse shape directly reproduces the issue. Fan-out,
many exits, large captures, fragmented heap and forced-GC stress are unmeasured;
no behavior implementation was changed, so these are limits of the measurement,
not new implementation paths asserted to pass.

## Validation

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed on Linux host.
- Focused release audit: 30 executions, with expected outcomes above.
- Runnable example: checked separately in default and release modes.
- No local full suite or full example audit; CI was not queried.
- macOS arm64/x86-64 and Windows execution and cross-target Clippy untested.
  This change adds only documentation, an audit script and an example; no Rust,
  runtime, platform API or cfg code is modified.
