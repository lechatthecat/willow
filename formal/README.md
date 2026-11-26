# A small concurrency core

This is an executable-model formalization, not a proof of the Willow compiler
or runtime. The models live in
[`core_models.rs`](../src/semantic/type_checker/core_models.rs), compiled only
under `cfg(test)`. Run `cargo test --lib core_model_`. No solver, generated
program execution, network, or additional dependency is needed for these tests.

The statements below separate a mathematical core, a bounded machine check,
and the assumptions needed to connect it to Willow. Acceptance by the compiler
is evidence for its implemented restrictions; it is not a universal data-race,
termination, or deadlock theorem. The models intentionally retain independent
representations and algorithms so a shared implementation bug is less likely
to pass as agreement.

## Types, sharing and loans

The modeled type language is

```
T ::= i64 | Closure | Array<T> | Option<T> | Mutex<T> | RwLock<T>
    | Channel<T> | Task<T> | FrozenArray<T>
```

Write `S(T)` for permission to transfer a value between tasks, and `Y(T)`
for permission to share it concurrently. These are structural judgments:

| Type | S | Y |
| --- | --- | --- |
| i64 | true | true |
| unknown-environment Closure | false | false |
| Array<T>, Task<T> | S(T) | false |
| Option<T>, FrozenArray<T> | S(T) | Y(T) |
| Mutex<T>, Channel<T> | S(T) | S(T) |
| RwLock<T> | S(T) and Y(T) | S(T) and Y(T) |

These rules describe capabilities, not Rust trait implementations. A scalar
capture copies its value and requires S. A shared GC-reference capture requires
Y, unless an exclusive transfer or loan is established. Send alone does not
authorize concurrent use of an Array. Mutex/Channel assume exclusive payload
access or ownership transfer; RwLock assumes reader sharing, hence its stronger
payload condition. FrozenArray assumes its contents cannot be replaced through
a mutable alias; nested elements must themselves satisfy the predicate.

For loans, a heap object has permissions `(P,C)` for parent and child writes.
Fresh allocation starts at `(1,0)`. `lend` changes `(1,0)` to `(0,1)`;
completed `join` changes `(0,1)` to `(1,0)`. Parent access requires P; child
access requires C. An alias denotes the same object and gets no extra
permission. Read/write does not change permission. Detached transfer never
restores the parent's permission. Returning permission at task creation,
cancellation request, or a conditional await would be unsound.

**Claim.** Every reachable state of this core satisfies `not(P and C)`.
Proof: allocation establishes it; the two permission-changing rules preserve
it; other transitions preserve the state. Thus conflicting parent/child
accesses cannot both be authorized. This is a safety claim, not a guarantee
that a child terminates or its parent can join it.

The machine check explores the entire two-state transition system to a fixed
point. A second check enumerates all 240 placements of lending and joining
among four accesses, including every direct/alias choice, translates them to
Willow, and compares the permission conflict with E2402/E2408. Structural
classification is compared with the production Send/Sync classifier for all
800 unary type trees through depth three over both leaves.

**Connection to full Willow.** The proof assumes alias identity is complete,
fresh values do not already escape, payloads are Send, and a completed join
really excludes further child access. The compiler establishes conservative
alias components and loan intervals in
[`transfer_ownership.rs`](../src/semantic/type_checker/transfer_ownership.rs).
Branch joins and rebinding may retain aliases; a loan must finish by an
unconditional await in its block instance, and loop loans must originate in
that iteration. `@sending` transfers invalidate old aliases. The separate
[`async_borrows.rs`](../src/semantic/async_borrows.rs) analysis handles explicit
`&T`/`&mut T` borrowed tasks: shared reads may coexist, a mutable loan excludes
other accesses, and tasks must finish before their borrowed storage goes out
of scope. A TaskScope cancellation request alone is not a completion event.
Lifetime safety also assumes lowering retains storage until completion and
unwind/cancellation joins and cleanup respect those lifetimes.

For full structural types, classes combine field predicates and Sync rejects
post-construction field writes; recursive types require a coinductive argument;
enums combine payloads; interfaces require declared marker contracts validated
for implementations. Callable storage analysis distinguishes actual shareable
environments from unknown closures. These rules, explicit reference lifetimes,
multiple objects/children, scopes, loops, inheritance, native values and GC
are **not proved by the two-state model or the unary type enumeration**. Their
implementation is part of the refinement assumptions, with existing focused
tests providing additional evidence. Conditional on those assumptions, the
capability argument extends object by object; it does not prove the assumptions.

## E2604: absence of modeled waits while holding a lexical lock

The core has a finite set F of functions, a call relation `f -> g`, local
wait events W(f), and a transmission mask M(g) for each callee. For a single
effect bit, the judgment is the least solution

```
E(f) = W(f) OR any(f -> g AND M(g) AND E(g)).
```

Its operational interpretation walks from the lock-body entry to a local
wait, crossing only edges transmitting that effect. Returning, branching and
repeated calls need no additional states for this may-reach property; recursion
does not require termination. Unknown calls, external targets without evidence
and declared-but-unavailable bodies denote every effect. They cannot justify
an empty summary. Bits are independent; an async task's own suspension does
not necessarily suspend its creator, which motivates masks. This does not
permit an awaited async call under a lock.

**Claim.** If the summary at the lock-body entry omits the wait bit, no finite
core execution from that entry reaches a modeled wait. Proof by path length:
a zero-edge path is a local seed; each additional transmitted edge propagates
the seed to its caller. Conversely every least-fixed-point addition has such
a path. Infinite computation is permitted; this is not a progress theorem.

The independent interpreter searches reachable call states. It is compared
with `EffectProblem::solve` on every three-function directed graph, local-wait
assignment and transmission mask: 32,768 problems and 98,304 root judgments.
A deterministic xorshift corpus adds 128 eight-function masked suspension
problems. The same graphs generate synchronous Willow helper bodies containing
channel receives; their lock-call diagnostics are compared with unmasked
operational reachability, exercising parsing, call extraction, local effect
seeding and E2604 reporting. Three additional cases require conservative
summaries for missing bodies, unknown calls and external targets.

**Refinement assumptions.** Every dynamic synchronous call is represented,
local waiting operations are classified, masks match execution, and diagnostics
are enforced by the driver. The production graph includes imported, virtual,
interface and indirect targets; the generated source corpus covers direct
single-module calls only. Compiler effect analysis does not prove native
bodies. Console output is an explicit exception to E2604, even if it stalls
the worker; reviewed `nonblocking` foreign declarations are assumptions. The
property concerns modeled waits, not arbitrary OS blocking, scheduler fairness,
GC pauses or a bound on time spent holding a lock.

## Lock order: absence of a cycle of lock acquisition waits

The core has a finite set L of lock identities. A program contributes an edge
`a < b` whenever it can request b while holding a. Accept if there is an
injective rank `r : L -> {0,...,|L|-1}` such that every edge has `r(a) < r(b)`.
An edge from a lock to itself is rejected, including shared-read reacquisition.

**Claim.** Accepted core programs cannot have a cycle of tasks, each holding
one lock and waiting to acquire the next task's lock. A supposed cycle would
give `r(a0) < r(a1) < ... < r(a0)`, contradicting strict order. Multiple held
locks do not change the argument if each held/requested pair contributes its
edge. E2604 is needed to exclude other modeled waits while locks are held.
Neither rule excludes channel/await deadlocks outside critical sections,
starvation, infinite holders or native lock cycles.

Machine checking enumerates all 512 graphs on three locks. A permutation-based
rank-certificate checker is compared with E2605 on source generated from all
graphs, independently of the compiler's SCC implementation. A separate
operational check enumerates 64 request/finish assignments per graph, detects
cycles by following task wait pointers, and verifies both directions: all 25
rankable graphs admit no cycle, and each of the other 487 has a witness. All
vertices have a distinct possible holder; a finishing task breaks the cycle,
so cycles through subsets of holders are covered. This is exhaustive within
the bound, not machine induction for unbounded programs. The argument above
states the unbounded theorem under its assumptions.

**Refinement assumptions.** The acquisition graph overapproximates possible
aliases and every nested acquisition; runtime lock exclusion and release are
correct. Willow uses allocation-site/global identities, conservative type
classes for unknown identities, and shared callable nodes in
[`lock_order.rs`](../src/semantic/type_checker/lock_order.rs). Unknown locks of
one type may alias any known lock of that type. Group acquisition also depends
on compiler group collapsing, runtime canonical ordering and duplicate-handle
handling. The generated corpus tests distinct local allocation sites and
ordinary nested Mutex locks; it does not prove type-class expansion, grouped
locks, RwLock modes, interprocedural extraction, native lock code or cleanup.

## Trusted computing base

| Layer | What must be trusted |
| --- | --- |
| Mathematical statement | Correctly chosen events, alias relation, capability rules and refinement assumptions |
| Executable evidence | Model/oracle code, corpus generators, assertions, Rust compiler, test harness and host hardware; bounded checks are not a theorem prover |
| Source to analyses | Lexer/parser, resolved identities/types, aliases, field writes, call/dispatch graph, imported facts and invalidation, builtin and foreign effect classification |
| Static decisions | Send/Sync inference, ownership/reference lifetime analyses, effect propagation, lock graph analysis and driver enforcement |
| Execution | IR transformations, code generation, runtime ABI, GC roots/lifetimes, task completion and cancellation, locks/atomics, unwind cleanup, foreign adapters, OS and hardware memory model |
| Static scalar contracts | VC encoding, arithmetic/overflow model, summaries, cache binding, solver protocol and pinned Z3; these are separate from the concurrency core |

Tests can reveal a mismatch in this chain. They do not remove the compiler,
runtime or Rust toolchain from the trusted base. The concurrency checks above
do not depend on Z3; Willow's scalar contract verification does.

## Can Z3 certificates remove the solver from the trusted base?

The current [`solver.rs`](../src/semantic/verification/solver.rs) pins Z3
4.15.4, requests models and trusts `unsat`; it neither requests `get-proof`
nor independently checks a proof. A transcript or counterexample is not an
UNSAT certificate. Z3 supports proof production enabled before assertions and
`get-proof` after UNSAT ([official command documentation](https://microsoft.github.io/z3guide/docs/logic/basiccommands/)).

Independent checking is possible for supported fragments, but support must
cover **every** proof rule used by Willow's VCs. The Z3 project's
[verified-checker report](https://github.com/Z3Prover/z3/blob/8fbb275aba0472e862e289c7af1331b3bd5d512c/fstar/TECHNICAL_CHECKER_REPORT.md)
distinguishes closed, fully checked acceptance from partial proofs with pending
obligations. Its [proof-checking documentation](https://github.com/Z3Prover/z3/blob/master/fstar/PROOF_CHECKING.md)
also describes restricted rule/theory coverage. This does not establish that
Willow's pinned producer, integer division, nonlinear arithmetic, quantified
data obligations and preprocessing are all supported by that checker.

Consequently this work does not label Willow's Z3 answers independently
certified. Such integration would need versioned proof production, binding
the original assertions and negated obligation to the checked closed root,
and fail-closed handling of unsupported rules, partial results and resource
limits. Z3's own proof checker is useful additional checking but shares Z3's
implementation trust; rerunning the same solver is not independence. Even a
fully independent certificate checker would leave VC encoding and the
source-to-runtime refinement in the trusted base.

## Coverage and cost

The eight named `core_model_` tests cover empty graphs, local waits, pure and
waiting self recursion, mutual recursion, disconnected waits, transitive paths,
diamonds, masked edges, missing evidence (three forms), acyclic lock order,
self acquisition, two/three-lock cycles, unused locks, scalar/closure leaves,
each of seven wrappers, nested wrappers, access before/during/after a loan,
direct/alias accesses and exclusive permission restoration. Failing comparisons
print the generated source or graph/seed/mask for replay.

All additional computation is test-only; production compilation, generated
code size and runtime hot paths are unchanged. For V vertices and E edges,
one operational effect query costs O(V+E) time and O(V) space. Testing every
root costs O(V(V+E)); enumerating all graphs/masks/seeds is deliberately
exponential and restricted to V=3. Rank enumeration is factorial and restricted
to three locks, favoring an independent, easily inspected oracle over reusing
the production SCC algorithm. The test visits at most 32,768 lock request
assignments. Type enumeration grows geometrically with a fixed depth of three;
each type comparison is linear in its nesting depth. Corpus sizes are explicit
and stable; no wall-clock timing is used as correctness evidence.
