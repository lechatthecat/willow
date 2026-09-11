# Compiler and runtime boundaries

## Executable IR

Typed HIR feeds a private construction graph (`SourceFunction`). Lowering turns
expression control flow into blocks and materializes call arguments, coercions,
allocations, and reference places in source evaluation order. Optimizations and
temporary lifetime analysis run before the graph is finalized.

The public executable types in `src/ir/lowered/final_ir.rs` contain operands,
operations, metadata, and block edges. They contain no HIR expressions or statement
bodies. Lifted lambdas and deferred cleanup are executable LIR functions too.
Finalization diagnoses any source operation that was not lowered; code generation
has no expression-tree fallback. Cleanup graph cloning, dumping, and destruction use explicit
worklists so deeply nested cleanup does not consume the compiler's native stack.

Cranelift selects the machine representation of each operation. It does not
rediscover ternary, short-circuit, match, propagation, or await control flow.
Suspensions and recovery are explicit edges. Prepared method trace frames and
reference diagnostic scopes must agree at CFG joins and survive task suspension.
Entry blocks initialize the invocation and cannot have incoming edges; loops,
including cleanup loops, use separate headers where interruption checks run.

An array reference captures its checked index and original backing buffer before
later arguments execute. The buffer is an opaque GC-owner local, separate from a
language `void` value. Recomputing the address from the array handle after another
argument resizes the array would change which element the reference designates.

Temporary lifetime analysis includes implicit cleanup uses. In particular, a lock
held by an infinite loop still needs its handle and protected value when the task
is cancelled; absence of a normal release edge does not make those locals dead.

## Resolution and compiler traversal

`TypeId` and `FunctionId` are interned four-byte identities. An interner owns their
structured names, while scoped symbol tables map source spellings to canonical
identities. Serialization and diagnostics retain structured, readable names.
Interner names live for the process lifetime; this is not a per-compilation arena.

Unit resolution uses immutable alias snapshots over shared global declarations.
Compiling a unit installs its module, function, and type views explicitly and
restores the enclosing context on normal return, error, or panic. HIR carries the
unit's resolved class, interface, enum, function, and namespace metadata into
lowering. Task constructors are global canonical declarations, with unit aliases
resolved before checking their calling convention.

Type transformations, return analysis, interface inheritance traversal, defer
syntax traversal, and expression scheduling use explicit worklists. Other complex
passes still use the continuation infrastructure; its unsafe implementation has
not been removed. Runtime effects and call-graph analysis own concurrency-site
diagnostics instead of repeating them in a legacy syntax walk.

## Optimization scope

Current passes fold scalar constants, propagate local constants, simplify constant
branches and unreachable blocks, and remove unused pure computations. Integer
folding preserves wrapping behavior and leaves faulting division/remainder for the
runtime. Small scalar leaf functions can be inlined with fresh locals.
Small scalar loops can be expanded four iterations at a time, retaining each
iteration's condition and exact arithmetic order. Calls, faulting operations,
references, and cleanup exclude a loop; a per-function growth budget bounds code
size. This reduces interruption-check overhead without removing cycle checks.

Scalar replacement removes initialized fields of nonescaping objects in a
conservative single-block case. Calls, possible faults, escaping references, and
observable GC operations prevent that transformation. This is a foundation for
broader inlining and escape analysis, not a general interprocedural optimizer.

## Runtime execution and collection

Task-owned native stacks let synchronous loops and recursion yield without losing
their activation records. Stack ownership, OS-thread affinity, cancellation,
root registration, and platform limits are documented in
[the stack decision](decisions/0001-task-stack-switch-capability.md).

Major collection marks concurrently between an initial root snapshot and final
remark. It uses an incremental-update insertion barrier rather than SATB. Bounded
mutator assistance and allocation pacing share the marking work. Remark and sweep
still require a world stop; collection is not pause-free.

`WILLOW_GC_MEMORY_LIMIT` limits reserved GC regions, not process RSS, native task
stacks, or Rust-owned container storage. Collection timing, pacing, and the limit
are described in [GC telemetry](gc_telemetry.md).
