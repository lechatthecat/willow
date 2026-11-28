# A small concurrency core

This document defines the core rules behind Willow's concurrency checks and
checks them with executable models. The models live in
[`core_models.rs`](../src/semantic/type_checker/core_models.rs), compiled only
under `cfg(test)`. Run `cargo test --lib core_model_`; the tests need no
solver, network or extra dependency.

Each section gives the rules, a bounded machine check, and the assumptions that
connect the model to the compiler. The models use their own representations and
algorithms, so a bug in the compiler is unlikely to be repeated in the model.

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

For loans, each heap object `o` has independent permissions
`(P_o, C_o,1, C_o,2)` for the parent and two child tasks. Variables map to
object identities; directed containment edges describe references stored in
objects. `R(v)` is the reflexive transitive reachable set of variable `v`.
An alias changes the variable map, not permissions. Fresh allocation gives
`P_o = 1` and both child bits zero.

`lend(t, v)` requires an idle task slot and `P_o = 1` for every `o` in `R(v)`;
it clears those parent bits and grants them to child `t`. A completed `join(t)`
returns exactly that child's permissions and clears its bits. Parent access
requires all parent bits in `R(v)`; child access requires all corresponding
`C_o,t` bits. Reads and writes preserve permissions and, in these traces, the
reference topology. A permanent transfer clears the parent's reachable bits
without a later return. Task creation, cancellation requests, or an await on
only one possible path do not by themselves return permission.

**Claim.** Every reachable state satisfies, for each object `o`,
`not(P_o and exists t. C_o,t)` and `|{t : C_o,t}| <= 1`.
Proof by induction on transitions: allocation establishes the property for a
fresh identity. A lend checks parent permission for its entire reachable set;
by induction no child owns any of those objects, so replacing their parent
bits with one child's bits preserves both clauses. A completed join clears
that child's bits while restoring the parent bits; induction excludes any
other child's bits on those objects. Alias binding grants no bits. Creating a
container allocates a fresh identity and records edges to parent-accessible
payloads without duplicating their permissions; future access and lending
must check the enlarged reachable set. A transfer removes parent permission;
reads/writes and rejected operations preserve state. Thus two tasks cannot
both be authorized to access an overlapping reachable object. This is safety,
not termination, and requires topology-changing operations to respect the
same access guards.

The test-only [`loans.rs`](../src/semantic/type_checker/core_models/loans.rs)
checks all 64 directed containment graphs without self edges (including cycles)
and all 27 maps of three variables to three allocated objects. Each fixed
heap is explored to a fixed point, not a trace-depth cutoff. The 1,728 heaps
reach 7,980 heap/permission states in total; reachable-state counts per heap
are pinned as `{3: 750, 5: 666, 7: 252, 9: 18, 11: 36, 13: 6}`. Every state
checks objectwise exclusivity, permission conservation, guarded parent/child
access and each task's join. All 11,772 successful lend transitions also
check that an overlapping second lend fails without changing state. Allocation,
alias and containment choices are represented by the initial heaps; dynamic
topology mutation is not exhaustively explored.

The independent oracle uses directed graph traversal and permission bitsets,
without production ownership structures, union-find or loan-interval helpers.
A source differential checks `rejected iff conflict` for 10,024 two-task
straight-line programs. The ten binding shapes use `a = [1]`, `b = a | [2]`,
and `c = a | b | [3] | [a] | [b]`, with `edit(Array<i64>)` and
`editn(Array<Array<i64>>)` children. All 9 pairs of loan variables and all 100
pairs of start/join intervals around three parent accesses are enumerated:
`10 * 9 * 100 = 9,000` cases. Access variables and read/write choices use a
fixed xorshift seed; 1,024 further samples have five accesses. Events at the
same point run in task-number order (start then join), followed by parent
access. The pinned result is 2,552 accepted and 7,472 rejected programs. The
original 240 single-child/direct-alias cases are retained.

Conservative source corpora check `conflict implies rejected`: 288 cases of
conditional/unconditional alias or fresh rebinding, 360 two-iteration loop
cases with inner/outer allocation, and 360 permanent `@sending` transfers
with access or re-lending before/after transfer. Branch execution follows the
generated boolean, rather than production's path-insensitive alias union.
Loop traces additionally assert the language's iteration-origin restriction:
outer values are rejected even when the finite execution has no conflicting
access; inner values joined before use are accepted. Accepted, conflicting
rejection and excess rejection counts are pinned separately for each corpus.

| Conservative corpus | Accepted | Conflicting/rejected | Excess rejection |
| --- | ---: | ---: | ---: |
| Branch/rebinding | 216 | 64 | 8 |
| Loop | 116 | 128 | 116 |
| Sending | 232 | 128 | 0 |

Only E2402 (array loan) and E2408 (ownership transfer) are allowed; lexical,
parse or unrelated semantic errors fail the test instead of counting as
rejection. These are semantic checks of generated programs, not execution.
Structural classification is independently compared with the production
Send/Sync classifier for all 800 unary type trees through depth three.

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
environments from unknown closures. These rules and explicit reference lifetimes
are not proved by the unary type enumeration. Multiple objects, aliases,
containment and two children
now have bounded fixed-point evidence; finite branch, rebinding, loop and
transfer traces have differential evidence. This does not prove arbitrary
heap sizes or task counts, inherited class fields, native values, GC,
interprocedural escapes, cancellation/scopes, or infinite loops. Their
implementation remains a refinement assumption, with focused tests providing
additional evidence. Conditional on these assumptions, the inductive
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

The source differential in
[`effects_source.rs`](../src/semantic/type_checker/core_models/effects_source.rs)
adds typed edges: Direct, Virtual, Interface, Indirect and Imported. Virtual
entries use a pure open base and an override forwarding to the target function;
the caller holds a base-typed receiver. Interface entries use a Send interface
and a forwarding implementation. The independent oracle expands these entries
itself. Indirect entries reach **all address-taken functions of the same arity**,
including values created in unreachable bodies and other modules, not just the
function assigned at that call site. Arity-one/two candidates and waiting lambda
values are included. The generated lambdas contain a receive; current source
checking gives them callable identities. Identity-less values remain a
conservative waiting assumption, not a claim that every pure lambda waits.
The oracle uses no production `dispatch_targets`, `close_reach` or
`EffectProblem`; those are only exercised on the compiler side.

| Source corpus | Fixed bound | Locked outcomes (accepted / rejected) |
| --- | --- | --- |
| Homogeneous Direct / Virtual / Interface / Indirect | 512 graphs * 8 wait sets * 4 kinds = 16,384 programs | Each of the first three: 1,024 / 3,072; Indirect: 804 / 3,292 |
| Mixed local kinds, xorshift | 256 graphs of 4–6 functions, each checked inside and outside a lock | 87 / 169 |
| Imported mixed kinds, xorshift | 64 projects of 3 modules * 3 functions, each checked inside and outside a lock | 16 / 48 |
| Isolated imported entry paths | 4 kinds * 2 wait choices, each inside and outside a lock | 1 / 1 per kind |
| Async virtual masks and arity isolation | 16 kind/wait/mask combinations + 6 disconnected-value/lambda combinations, each inside and outside a lock | Explicit expected result for every combination |

All totals are asserted, with accepted/rejected lower bounds per kind (512 for
homogeneous, 64 for mixed, 16 for imported, one for isolated imported entries).
Each mixed program contains all its corpus's edge kinds. Outside-lock twins
must all be legal. The isolated imported entries prevent an alternative direct
path from hiding a missing dynamic-dispatch edge. Async overrides must not
transmit their body's waits to the synchronous creator.

Imports are analyzed in-process with `CompilerSession::analysis_with_emitter`,
using temporary `.wi` files removed on drop (also during failure unwinding).
The 64 projects mix virtual/interface/indirect edges in every module, including
qualified receivers of imported classes/interfaces and item-imported function
values. Cyclic imports are rejected by Willow: a further driver case pins
E0403, so generated module dependencies form a DAG. Calls within a module may
still recurse. Parsing or diagnostics other than E2604 fail the differential;
the import-cycle check alone expects E0403. Failure output includes the replay
seed/kind and generated sources. No generated programs are executed.

**Refinement assumptions.** Every dynamic synchronous call is represented,
local waiting operations are classified, masks match execution, and diagnostics
are enforced by the driver. Direct, virtual, interface, indirect and imported
calls now have the bounded source evidence above. It does not enumerate all
inheritance shapes, generic instantiations, arities or module graphs, nor prove
lowering or execution. Compiler effect analysis does not prove native bodies.
Console output is an explicit exception to E2604, even if it stalls
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

`lock walk` is a separate operational extension, not part of this finite-graph
model. Its initial acquisition holds no other lock; each subsequent acquisition
makes one nonwaiting attempt and raises a recoverable panic on failure. Such
an attempt adds no task-wait edge. The compiler rejects lexical nesting into
or around a walk and retains E2604 and protected-value escape checking. Thus a
walk cannot complete a cycle of lock-acquisition waits, although an application
can still traverse a cyclic list forever. Runtime exclusion, acquire-before-
release and panic/cancellation cleanup are exercised by
`tests/integration/concurrency/lock_coupling.rs` and
`crates/willow_runtime/src/async_mutex/handoff.rs`; they are not claimed as
machine-checked by the graph enumeration below.

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

The nineteen named `core_model_` tests cover empty graphs, local waits, pure and
waiting self recursion, mutual recursion, disconnected waits, transitive paths,
diamonds, masked edges, missing evidence (three forms), acyclic lock order,
self acquisition, two/three-lock cycles, unused locks, scalar/closure leaves,
each of seven wrappers, nested wrappers, access before/during/after a loan,
direct/alias/contained accesses, disjoint and overlapping child loans, join
order, read/write accesses, exclusive permission restoration, conditional and
unconditional rebinding, loop allocation origins and permanent transfers,
base-receiver overrides, Send interface dispatch, same/different-arity function
values, waiting lambdas, async override masks, imported dynamic dispatch,
cyclic-import rejection and outside-lock controls.
Failing comparisons print the generated source or graph/seed/mask for replay.

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

The added E2604 differential performs 17,084 semantic checks: 16,384 homogeneous,
512 mixed, 128 imported, 16 isolated imported and 44 mask/arity checks. Together
with the retained 128 direct-source checks this is 17,212 E2604 comparisons,
plus one E0403 import-cycle check. There are 72 valid project fixtures and one
cycle fixture; cleanup is asserted. Only one case's sources/diagnostics are
retained at a time within each test.

For the typed-edge oracle, two shared arity hubs avoid a call-by-candidate
Cartesian expansion: each input edge and each address-taken function contributes
at most one additional adjacency entry. One root traversal and construction
cost O(V+E) time and space. Source emission indexes required wrappers and
imports once, emits each wrapper once per target, and costs O(L) time/space for
source length L, including identifier digits; the module bound is three and
arity bound is two. Thus per-case oracle plus generator work is linear in L.
Deterministic operation/emission checks at V=8,32,128 cover chains, fan-in and
dense graphs, including repeated dispatch to the same target. This counts
test-harness work, not a claim that the production compiler is linear for
arbitrary inputs: each source comparison additionally runs the production
semantic checker once on the stated bounded program. No backend or runtime
work is added to production.

The loan checks invoke the compiler 11,272 times (10,024 two-task traces,
240 retained traces, and 1,008 conservative traces), below a fixed 20,000
budget. For N objects, V variables, E containment edges, T child slots and
S reachable permission states, caching each variable's reachability costs
O(V(N+E)) time. The fixed-point traversal checks V*T lends and T joins per
state, using a hash set for expected O(1) state membership at the fixed
N=3, T=2 bounds, rather than repeatedly scanning prior states. Its bounded
state-processing cost is O(S*T*(V+1)); closure storage is V bitsets and each
stored state has T+1 bitsets. Generalizing beyond word-sized masks introduces
the corresponding bitset-width factor; the state space itself is exponential.
At the actual bounds there are exactly 63,840 candidate lend/join transitions.

Each generated trace computes its three reachability masks once, then visits
each source event once with constant-sized permission operations. With K
cases and L events per case, oracle/source construction costs O(K*(V(N+E)+L))
and retains only one program and its diagnostics at a time. The additional
compiler cost is K semantic checks of those bounded programs; this statement
does not assume the full production compiler is linear. The xorshift sample
bounds the larger access-choice/long-trace product instead of enumerating it.
The pure graph coverage includes chains, fan-out, cycles and redundant aliases;
the source corpus deliberately uses the ten stated array shapes, not all
arbitrary graphs. No production code, generated-code size or runtime cost is
added by these tests.
