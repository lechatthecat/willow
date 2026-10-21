# willow-jz15.117 implementation audit — 2026-10-21

Status: **review findings fixed; ready for review, not closed**. The original
acceptance criteria are unchanged. The prior execution-policy question is
resolved by the user's instruction to implement an execution guarantee. Nothing
has been staged, committed, or pushed.

## Acceptance and implementation candidate

E2402 previously classified every recorded async local without checking whether
its value survived a suspension. The candidate resolves binding identities,
builds a source control-flow graph, summarizes straight-line regions, and
propagates local use/definition facts backwards from reads. It reports a binding
only if its current value reaches a source suspension. Reassignment kills the
previous value; branch joins, loop backedges, break, continue, return, match
guard fallback, short circuiting and lexical shadowing are represented.

Saved iterator/defer/send/lock operands have separate identities and definition
points. Deferred captures use shared cleanup chains, including cleanup reachable
from suspension and early exits. Channel send/receive, lock acquisition and
blocking select count as source suspensions even without an `await` token.
Parameters and results retain their separate initial/completion task-boundary
checks. Binding addresses are ephemeral keys within the same borrowed AST body;
they are neither persisted nor dereferenced, and do not alias equal source spans.

Two older tests expected rejection of dead locals. They now explicitly read the
local after await, preserving their original marker-contract and colliding-span
coverage under the new acceptance criteria. The runnable source is
`example/async_dead_locals.wi`; its integration test builds and executes it.

## Execution guarantee and review fixes

Implicit preemption is not a source suspension. `finish_global_poll_boundary`
now installs a worker lease before publishing a preempted task as runnable.
Claims on another worker reroute it to its owner before polling or cancellation.
Repeated implicit preemptions reuse the lease. A source Pending/Yield boundary
releases it; terminal removal releases it through RAII. Blocking syscall
boundaries preserve it. Synchronous native stacks keep their own lease until
return, even at generated safepoints: their local values need not be Send.

The shared affinity registry preserves persistent OS workers across drive/pool
shrink requests. This extends the existing native-stack lifetime registry to
cooperative frames on every target. No type-based scheduling metadata or new
compiler/runtime call is needed. ABI contract revision 4 rejects revision-3
runtimes, which do not provide this guarantee. Initial scheduling and migration
after source suspension still work, with parameter/result and live-frame Send
checks enforcing those transfer boundaries.

This deliberately reduces stealing opportunities during long synchronous
segments, including segments containing only Send values. Such segments still
preempt and allow other runnable work on their owner. Selectively migrating
Send-only segments would require additional compiler proof/metadata; it is not
needed for the original acceptance behavior. No throughput improvement is claimed.

Intermediate results now receive CFG identities, definition points after
operand evaluation, and reads at their consuming operation. Thus earlier
call results, nested call results, fields, indexes, closure literals and
conditional values survive subsequent operand awaits in the analysis. Trivially
Send scalar/function-pointer operands need no extra facts. Diagnostics classify
retained intermediate types through the same per-body Send cache used for local
slots and identify both the value and the suspension. Non-Send operands that
are consumed before a later await remain accepted.

Audited call paths: AST name resolution and operand order; CFG definitions,
uses and liveness propagation; Send classification and labels; lowered implicit
preemption and cooperative poll return; scheduler boundary/claim/cancellation;
native-stack suspension/resumption/recycle; persistent worker shrink; ABI
handshake and generated-code migration probes. Lease publication occurs under
the existing task shard lock. Registry writers never acquire task locks. The
cancellation regression uses actual async frames, so cleanup is exercised.

## Follow-up review: pattern bindings and deferred callees

Pattern locals are now registered with their checked payload/downcast/scrutinee
types before checking the guard and body. Their identity comes from the original
AST name, including when enum shorthand is normalized into a temporary pattern.
The CFG defines each arm binding on the successful dispatch edge, before its
guard. It reads a saved scrutinee at subsequent pattern dispatches, so a guard
that suspends and then fails cannot hide a retained closure or enum payload.
Wildcard dispatch does not read the discarded value. A successful arm body
retains only its live bindings; dead bindings and no-await arms remain accepted.

An indirect deferred callee has a separate `DeferredCallee(ExprId)` identity and
checked type. Its snapshot is defined before evaluating registration arguments
and read by the shared cleanup chain. Reassigning the original local kills only
that local's old value, not the saved callable. Deferred arguments retain their
existing independent identities. Direct function calls create no non-Send
callee slot. This matches `capture_defer_expr` in the lowered IR, where indirect
callees are frozen before arguments and retained until scope cleanup.

The additional regression matrix has 25 perspectives: plain/shadowed/unused
bindings; live/dead/no-await and creation-after-wait; suspending and non-suspending
guards, guarded fallback and wildcard discard; qualified and normalized enum
payloads; class downcasts; deferred callee survival through reassignment,
conditional registration, ended scopes, registration after a wait, and direct
function calls. CLI tests assert binding, intermediate-scrutinee and saved-callee
E2402 labels. The runnable example now also executes a no-await deferred closure
and match binding, plus a match binding consumed before await, with unchanged
output. Existing moving-GC async guard and deferred capture execution tests were
also selected to cover affected call paths.

For N guard arms, dispatch and binding-definition construction is O(N + K),
where K is their total binding count; source/resolved bindings are zipped once
and symbol types use existing lookup. Each match adds one scrutinee identity;
each deferred call adds one saved-callee identity. Cleanup chains remain shared,
with no per-exit copy of all deferred values. Sparse fact bounds below include
these extra identities. Measured guarded chains at N=16/32/64/128 have
67/131/259/515 blocks and 48/96/192/384 facts and predecessor visits. Independent
defer scopes at those sizes have 80/160/320/640 blocks, 126/254/510/1022 facts and
128/256/512/1024 predecessor visits. These generated source shapes and count
assertions are in `async_retention_pattern_defer_scaling`; they establish linear
work for these adverse repeated shapes, not a universal linear dataflow bound.
There is no new generated code or runtime execution cost from these follow-up
analysis fixes.

## Follow-up review: ordered deferred cleanup

Deferred blocks and deferred match bodies now use the ordinary expression and
statement CFG builder instead of an unordered set of all reads minus declared
names. Each cleanup body is scheduled once, with ordered internal control flow. A shared transfer summary is then used
at each scope exit, before the appropriate continuation (see below).
The registration statement still does not execute these cleanup-time bodies.
Saved call operands and saved indirect callees retain registration-time behavior.

This applies the existing assignment kills, branch joins, constant-condition
reachability, lexical binding identities and loop backedges to cleanup. An
unconditional overwrite or overwrites in both branches kill the pre-suspension
value. A path that reads it before overwrite still retains it. LIFO tests verify
that a later-registered overwrite can satisfy an older deferred read, while the
reverse order still requires the original value.

Twenty additional explicit perspectives cover unconditional/both-branch/one-branch
overwrites, read-before-write, RHS reads, unreachable reads/writes, constant
conditions, shadowing, zero-iteration/infinite/unreachable loops, nested branches,
deferred match arms, both LIFO orders, no-await cleanup and ended scopes. A Linux
integration runs both overwrite branches with four workers, one-step preemption
and moving-GC stress (output `2`, `3`), and checks E2402 for a branch that still
reads the original value. The runnable example includes overwrite-before-use
cleanup after await without changing its output.

The shared-cleanup scaling generator combines N registered cleanup blocks with
N subsequent return branches, for N=16/32/64/128. Measured summarized block counts
are 51/99/195/387 (3N+3), with zero propagated live-in facts or predecessor visits:
every cleanup read is preceded by its own assignment. Thus bodies are not cloned
per return; source/CFG construction remains linear for this shape. General
sparse-dataflow bounds below still apply. No additional generated code, runtime
work, or allocation path was introduced. The fix reuses ordered CFG processing
rather than adding a second cleanup-specific analysis.

## Follow-up review: cleanup before its continuation

Scope completion now invokes only that scope's cleanup, followed by `next`.
Break and continue invoke cleanup down to the recorded loop boundary, followed
by the loop's break/continue destination. Return and cancellation invoke all
active cleanup. This eliminates the independent `next` edge that bypassed
assignment kills, and avoids treating outer-scope defers as already executed
when an inner scope or loop ends.

Each cleanup-time body is built and compacted once in an isolated CFG, using
shared binding identities. Postorder preprocessing handles nested defers without
recursive Rust calls. Sparse use/definition propagation computes reads required
on entry. For each assigned binding, reachability to normal return without an
assignment computes whether every returning path kills the incoming value.
A body with no returning path has no continuation edge. The resulting read/kill
sets are immutable and shared by Arc; ordinary CFG compaction preserves summary
nodes instead of copying their sets into every exit. Registration-time saved
operands retain their separate semantics.

Continuation-specific links are memoized by `(cleanup, stop, next)` and reuse
previous suffix links. This shares the cleanup body and transfer sets across
fallthrough, return, break and continue without cross-connecting their targets.
For a cleanup with B compact blocks, E edges and K assigned bindings, summary
construction costs sparse read propagation plus O(K(B+E)) worst-case reachability
work. Straight-line regions collapse before this pass: K assignments in one
region do not cause K scans of the original expression sequence. Shared-set
storage is O(total summary uses/definitions), plus O(C) small invocation links
for C distinct memoized continuation contexts. C may exceed the source node
count when many scope boundaries and continuations differ; no unconditional
linear bound or globally minimal dataflow bound is claimed. There is no cleanup
body or set duplication per context and no change to generated/runtime code.

Twenty focused perspectives cover fallthrough, optional/both-branch/nested
scopes, read-before-kill, conditional/both-branch cleanup, break/continue and
nested variants, both negative variants, preservation of outer-scope cleanup,
nested defers and LIFO. The Linux integration first passes race checking, then
runs fallthrough/break/continue with four workers, one-step preemption and moving
GC (output `2`, `2`, `1`, `2`). The example includes the reported fallthrough
shape without changing its output.

The N-cleanup/N-exit generator now measures 3N+3 main CFG blocks, 2N summary
blocks, N summary visits and zero propagated main facts for N=16/32/64/128.
This checks summary work as well as main CFG size and demonstrates shared
construction for repeated exits to the same continuation. Other measurement
rows below were refreshed after removing the spurious fallthrough edges.

## Follow-up review: exhaustive match dispatch

The type checker records successful match coverage in a body-local set of
expression identities. Retention reuses that proof rather than repeating enum
coverage analysis. For a proven exhaustive match, the final unmatched dispatch
has no successor; each successful arm still continues to the normal destination.
Guard failure still proceeds to the following arm, preserving legitimate
fallback reads. Matches without a coverage proof retain the conservative
fallthrough. Invalid matches do not acquire a proof. Both ordinary bodies and
isolated deferred-cleanup CFGs consume the same coverage set, so must-kill
summaries no longer include an impossible path bypassing every arm.

Twenty new behavioral perspectives cover boolean and enum matches, ordinary and
cleanup execution, every-arm overwrites, old-value reads, unchanged arms,
guarded overwrites and guarded fallback reads. The integration first passes race
checking, then executes both boolean values and enum variants, both outside and
inside cleanup, with four workers, one-step preemption and moving-GC stress.
Output is `2`, `4`, `6`, `8`, `3`, `5`, `7`, `9`. The runnable example includes
an exhaustive boolean overwrite in cleanup with unchanged output.

Coverage storage is O(M) for M matches in the current body, with expected O(1)
insert/lookup per match; existing coverage checking is reused. CFG construction
adds at most one unreachable sink per exhaustive match and does not rescan
variants per arm. No generated code or runtime path changes. The deterministic
N-variant test at N=16/32/64/128 measures ordinary block/fact/edge counts
37/16/31, 69/32/63, 133/64/127, 261/128/255. Cleanup main graphs have five blocks
and zero facts/edges; summary block/visit counts are 35/65, 67/129, 131/257,
259/513. This demonstrates linear behavior for growing exhaustive dispatch and
its cleanup summary, while the general summary/dataflow bounds still apply.

## Follow-up review: literal expression branch reachability

Ternary expressions now select only the then/else CFG edge named by a literal
boolean condition. Literal short-circuit operands select the RHS only for
`true && rhs` and `false || rhs`; `false && rhs` and `true || rhs` go directly
to their continuation. Nonliteral conditions retain both paths. This follows
the existing if/while literal-condition rule; it does not attempt constant
propagation or evaluate user code. Unselected expressions are still checked by
the type checker, but their unreachable awaits do not retain source locals.
The same CFG construction also feeds cleanup summaries.

Twenty new perspectives cover all literal ternary/And/Or choices, reachable
await negative controls, nonliteral conditions, and nested combinations. A CLI
integration race-checks and runs all four unreachable-await forms under four
workers, one-step preemption and moving-GC stress. Its async callee would print
`99` if invoked; output is only `1`, `false`, `true`, `false`, `true`. Three CLI
negative controls still produce E2402. The runnable example adds an unreachable
short-circuit await while a non-Send callback is live, preserving existing output
and the separate no-await function.

The change uses one constant-time boolean inspection per affected expression
and removes at most one edge. It adds no graph nodes, identities, dataflow passes,
allocations proportional to input size, generated code or runtime changes.
Existing construction and dataflow bounds and deterministic scaling tests remain
applicable; no new asymptotic claim is based on elapsed time.

## Follow-up review: resolved fieldless enum variants

Retention now consumes the existing `pattern_resolutions` table. Match dispatch
uses the resolved kind to decide whether an arm is unconditional, whether it
reads the scrutinee and whether it defines bindings. A bare fieldless variant
therefore remains a conditional variant test, just like its qualified spelling.
The name resolver likewise skips original pattern binding events when the
resolved pattern introduces no bindings; a variant spelling cannot shadow an
outer local of the same name. For real payload/downcast/binding patterns, the
CFG still defines original AST name identities, zipped with the resolved binding
list, matching the type checker's recorded local slots.

Twenty-four new perspectives compare qualified and bare variants in ordinary
matches and cleanup: later-arm reads, earlier-arm overwrite followed by a read,
every-arm overwrites, dead locals, guarded fallback, suspension inside a later
arm, and an outer closure whose name equals a variant. CLI negatives cover both
spellings with ordinary and deferred reads and require E2402 plus the local
label. The runnable example executes a bare-variant match that overwrites every
arm before use, preserving its existing output.

Each visited pattern performs one expected O(1) resolution-table lookup and a
constant-time kind check; binding iteration remains proportional to actual
payload bindings. No normalization, enum scan or cloning is repeated. Existing
postorder cleanup summaries and original binding identity handling are reused.
The N=16/32/64/128 exhaustive-match scaling test now checks both qualified and
unqualified spellings, in ordinary and cleanup forms. Counts match for each
spelling and remain those recorded in the exhaustive-dispatch section. No new
generated code or runtime behavior is introduced.

## Follow-up review: literal match guards

A literal boolean guard now selects exactly one CFG successor: `true` enters
the matched arm body; `false` proceeds to the next pattern dispatch. Nonliteral
guards retain both edges. Pattern matching itself is unchanged, so a true guard
on a refutable pattern does not remove that pattern's mismatch path. The same
construction is used in ordinary execution and deferred-cleanup summaries.
This removes impossible suspension and fallback-read paths without changing
exhaustiveness rules, source type checking or the runtime.

Twenty new perspectives cover wildcard and binding patterns with false, true
and nonliteral guards; awaits in the guarded body and fallback; and ordinary /
cleanup assignment kills with otherwise-live fallback reads. The CLI integration
race-checks and executes false-guard unreachable awaits, true-guard unreachable
fallback awaits/reads, and both cleanup variants under four workers, one-step
preemption and moving GC. Its output is `1`, `2`, `3`, `4`, with no unreachable
`99` output. The example includes a false-guard unreachable await while a closure
is live and keeps its independent no-await function and existing output.

The change performs one constant-time literal inspection per guard and removes
one edge when applicable. It adds no CFG nodes, identities, scans, summary sets,
generated code or runtime behavior. Prior graph/dataflow bounds and increasing-
size evidence remain applicable; this is a reachability correction, not a new
constant-propagation pass.

## Complexity audit

Let N be source/CFG size (including identifier bytes), B the summarized basic
blocks, E the block edges, L the distinct bindings/saved operands, F the number
of live-in `(block, binding)` facts, and P the predecessor-edge visits for those
facts. Name resolution, graph construction and basic-block summarization take
expected O(N + E) time with hash maps. Sparse propagation takes expected
O(F + P), bounded by O(BL + EL), and storage is O(N + E + F). These are algorithmic
bounds, not timing-based deductions.

The initial per-expression graph would multiply straight-line code length by
its number of live locals. The candidate removes that avoidable cost by
summarizing use/definition sets once per straight-line region. A body containing
N declarations followed by N reads has two summarized blocks and no propagated
facts. Adding one await produces four blocks and 2N fact/edge visits. Shared
cleanup chains avoid cloning the complete active-defer list at every return.
No whole-program index, artifact cache, build scheduling, or runtime auto-build
policy is changed.

Worst-case branch/loop dataflow can still contain BL facts; the analysis is
conservative and does not solve path predicates or interprocedural value flow.
No claim of globally minimal dataflow complexity is made. Intermediate operands
add at most O(N) identities/nodes; their propagated facts are included in F and
P. Classification is shared by distinct retained types, avoiding repeated marker
walks for repeated equivalent operands. No additional generated cleanup or
dispatch code is emitted by the source analysis.

For T tasks and W workers, affinity adds O(1) task metadata and O(W) registry
entries. Each segment acquires/releases once in O(log W) time; repeated polls
and claim-owner checks are O(1), without task-table scans. Pool-bound queries
are atomic O(1) reads. Native stack leases use the same registry. A stolen pinned
task is rerouted in O(1) queue operations per attempt; this does not guarantee
bounded total stealing attempts under adversarial scheduling. The tradeoff is
less available parallelism within long source segments, not extra generated
code or a repeated per-poll scan. Existing native-entry guards are preserved.

## Deterministic evidence and reproduction

Run `bash docs/audits/willow-jz15.117/reproduce.sh` for the focused checks.
The script does not run whole suites or query CI. Increasing-size generators
and their count assertions live with the named Rust tests; the script executes
them with `--nocapture` so the measurements can be reproduced.

The scaling input generators and operation-count assertions are checked in
with `async_retention.rs`. They cover increasing straight-line live sets,
sequential dead locals separated by awaits, sequential diamonds, and deep
branch nesting. No binaries, object files, or private specifications are stored
here.

Measured straight-line counts after basic-block summarization:

| Locals | Without wait: blocks / facts / edges | One wait: blocks / facts / edges |
| ---: | ---: | ---: |
| 16 | 2 / 0 / 0 | 4 / 32 / 32 |
| 32 | 2 / 0 / 0 | 4 / 64 / 64 |
| 64 | 2 / 0 / 0 | 4 / 128 / 128 |
| 128 | 2 / 0 / 0 | 4 / 256 / 256 |

Sequential dead-local inputs at sizes 16, 32, 64, 128 produced respectively
34, 66, 130, 258 summarized blocks and zero propagated live-in facts or
predecessor-edge visits.

Sequential diamonds at sizes 16, 32, 64, 128 produced block/fact/edge counts
82/80/96, 162/160/192, 322/320/384, 642/640/768. Nested branches at the same
sizes produced 21/19/35, 37/35/67, 69/67/131, 133/131/259. The generators
assert linear bounds for both shapes.

Intermediate chains at sizes 16, 32, 64, 128 produced respectively
34/62/62, 66/126/126, 130/254/254, 258/510/510 block/fact/edge counts, with exactly
N retained intermediate values. Repeated implicit preemption at 16, 64, 256
polls acquired one cooperative lease and released it once per segment. Registry
queries at 16, 128, 1024 leases and 1/8/N workers perform no registry-map scan
(the test holds its writer mutex while querying); acquire/drop updates total 2N.
These are deterministic operation counts, not timing-based asymptotic guesses.

## Validation results

The source matrix covers 32 control-flow, nine synthetic-value and 12 intermediate
operand perspectives, plus 25 pattern/deferred-callee perspectives, plus 20 ordered-cleanup perspectives, plus 20 cleanup-continuation perspectives, plus 20 exhaustive-match perspectives, plus 20 literal-expression perspectives, plus 24 resolved-variant perspectives, plus 20 literal-guard perspectives (202 total), plus increasing-size count checks. Final
focused compiler runs passed: 17 `async_retention` tests, 30 Send/Sync tests,
and the colliding-span test (20 combinations). Eleven integration retention tests
passed, including nested-call/field/index CLI diagnostics and the runnable
example under four workers, a one-step quantum and moving-GC stress. Example
output remains `42`, `42`, `2`, `3`.

Runtime affinity filter: 12 tests passed, covering cooperative thief rerouting,
source-wait release, cancellation cleanup, native stack ownership, pool shrink,
lease cleanup and deterministic registry costs. The generated native-stack
context test passed all 24 mode/root-count/depth combinations: normal return,
panic, cancellation; roots per frame 0/1/8/64; depth 0/7. It verifies rejection
of a thief, same-OS-thread resumption, moving-GC root rewrites, context restoration
and cached-stack reuse. Existing native migration expectations were updated to
this stronger ownership guarantee, retaining those GC/context perspectives.

`task_owned_shadow_roots_forced_native_preemption` passed debug/release and
moving-GC stress, asserting repeated same-owner preemption and zero native
migration. `async_mut_sync_preemption_forced_migration` and
`async_borrow_migration_forced_generated_polls` passed, preserving forced
migration after explicit source awaits. ABI fingerprint tests (6) and runtime
handshake tests (3) passed. Formatting and probe-feature native Clippy passed.
Workspace/all-targets Clippy passed on all four CI target triples:

| Target | Clippy | Native execution |
| --- | --- | --- |
| x86_64-unknown-linux-gnu | passed | focused tests passed |
| x86_64-pc-windows-msvc | passed | not executed |
| aarch64-apple-darwin | passed | not executed |
| x86_64-apple-darwin | passed | not executed |

Foreign native execution/link/run results remain unverified. Full local suites
and the full example audit were intentionally omitted per user policy. CI was
neither checked nor polled. No remaining concrete implementation blocker was
found; review approval and ticket closure are still pending as requested.

Follow-up validation also passed the named
`guard_and_if_let_bindings_stay_rooted_under_minor_gc_stress` and
`lir_defer_scopes_11_the_body_sees_the_captured_value` integration tests.
Runtime/ABI code is unchanged by this follow-up; its earlier focused results
above remain applicable. Formatting, compiler-focused checks and four-target
Clippy were rerun for this follow-up. No CI queries or full-suite runs were made.

Ordered-cleanup follow-up: all 11 retention unit tests, six retention integration
tests, 30 Send/Sync tests and the colliding-span test passed. Formatting and
four-target workspace/all-targets Clippy were rerun. Runtime/ABI code remains
unchanged; previously recorded runtime checks apply. Foreign native targets
remain unexecuted, and no full local suite or CI query was run.

Cleanup-continuation follow-up: all 12 retention unit tests (118 behavioral
perspectives), seven retention integrations, 30 Send/Sync tests and the
colliding-span test passed. Fmt and four-target workspace/all-targets Clippy
were rerun. No runtime changes, full-suite runs or CI queries were made.

Exhaustive-match follow-up: all 14 distinct retention unit tests (138 behavior
perspectives), eight retention integrations, 30 Send/Sync tests, colliding-span
test and the named body-cache/local-table regression passed. Fmt and four-target
workspace/all-targets Clippy passed. Linux is the only native execution target;
foreign native execution remains unverified. No runtime changes, full-suite
runs or CI queries were made in this follow-up.

Literal-expression follow-up: 15 retention unit tests (158 behavioral
perspectives), nine retention integrations, 30 Send/Sync tests and the
colliding-span test passed. The updated example was rechecked normally and
under forced preemption/moving GC. Fmt and four-target workspace/all-targets
Clippy passed. Foreign native execution remains unverified; no full local suite
or CI query was run. No runtime code changed in this follow-up.

Resolved-variant follow-up: 16 retention unit tests (182 behavioral
perspectives), ten retention integrations, 30 Send/Sync tests and the
colliding-span test passed. Qualified/bare increasing-size count checks passed.
Fmt and four-target workspace/all-targets Clippy passed. Linux is the only
native execution platform; foreign native targets remain unverified. No runtime
changes, full-suite runs or CI queries were made.

Literal-guard follow-up: 17 retention unit tests (202 behavior perspectives),
11 retention integrations, 30 Send/Sync tests and the colliding-span regression
passed. Fmt and four-target workspace/all-targets Clippy passed. Linux is the
only native execution platform; foreign native execution is unverified. No
runtime changes, full-suite runs or CI queries were made.


## Never-expression continuation follow-up (2026-10-21)

The review found that an ordinary call always kept its normal successor, even
when the checked expression type was `Never`. This made code after `panic()`
appear reachable and could retain a closure across an unreachable await or
through an unreachable read. Retention now ends that expression's normal flow
while still evaluating its operands in source order. No lowering, generated
code, runtime ABI or scheduler behavior changed in this follow-up.

Normal completion and uncaught unwind have distinct CFG terminals. Cleanup
summaries retain separate must-overwrite sets for those two outcomes, sharing
one immutable entry-read set. A panic in a cleanup executes older registered
cleanups through a memoized unwind suffix, rather than incorrectly returning
to the normal continuation or dropping older defers. Registration of a saved
`defer panic(...)` evaluates its operands without executing the saved call;
its non-returning effect occurs at cleanup. Lexical recovery boundaries retain
possible continuation after the recovering scope, as in lowering, rather than
after the panic expression itself. Recovery capability is conservatively
recognized from the same direct-recover syntax as the existing checker and
lowering; this is not an interprocedural panic/recovery proof.

The audit traced expression construction, operand temporaries, saved deferred
calls, cleanup summary construction/compaction, return/break/continue and
cancellation callers of the shared exit builder, and lowering's recovery scope
resume semantics. Two outcome-specific regression cases distinguish a normal
cleanup overwrite from an overwrite that happens only on the unwind path.
The existing affinity implementation and its earlier recorded validation are
preserved unchanged.

The new source test matrix has 26 perspectives (228 total across the retained
behavior matrices): unreachable await/read, reachable and operand reads,
awaited operands, conditional and both-branch panic, cleanup dead/live reads,
older deferred reads, assignments before panic, differing normal/unwind kills,
saved-panic registration and cleanup, and recovered scope continuations.
CLI diagnostics exercise five representative accepted/rejected cases. A Linux
execution test covers body panic and panic during cleanup, recovery, overwrite,
and an unreachable await under four workers, budget 1 and moving-GC stress.
The runnable `example/async_dead_locals.wi` also exercises recovery before an
unreachable await and retains its original output.

Complexity: let D be registered cleanups, C distinct normal continuation
contexts, B/E compact summary nodes/edges, K assigned values, and F propagated
read facts. The existing summary bound remains O(F + K(B+E)) per cleanup;
computing two outcome sets changes only its constant factor. Summaries and
transfer sets are still shared, not copied for each exit. Normal exit links
retain their existing context-dependent bound (up to O(DC)); unwinding adds
only O(D) cached suffix links per isolated graph. Recursive construction has at
most one normal-to-unwind transition, independent of cleanup nesting depth.
Each effect has at most two outcome nodes and one dispatch node. Lexical
recovery detection adds one linear scan of the scope's direct statements and
at most one boundary link. There is no new generated-code or runtime cost.
The source scan and distinct outcome transfer facts are necessary for the
required semantics; the implementation does not duplicate cleanup bodies.

Deterministic measurements from `async_retention_unwind_cleanup_scaling` use
N mixed normal/panicking cleanup bodies, an older read cleanup, and N exits:

| N | Main CFG blocks | Live facts | Propagation edge visits | Summary blocks | Summary visits |
| --- | --- | --- | --- | --- | --- |
| 16 | 131 | 2 | 4 | 82 | 96 |
| 32 | 259 | 2 | 4 | 162 | 192 |
| 64 | 515 | 2 | 4 | 322 | 384 |
| 128 | 1027 | 2 | 4 | 642 | 768 |

These measured shapes are 8N+3 main blocks, 5N+2 summary blocks and 6N summary
visits; they do not establish linearity for arbitrary control-flow/continuation
combinations. The prior straight-line, fan-out, nested-branch, intermediate,
pattern/defer and exhaustive-match scaling tests also remain in the focused
filter. Reproducible source generation is checked in with those tests; the
existing audit reproduction script selects them without a full-suite run.


Nested recovery capability is carried in the same postorder summary; parent
scopes inspect it in O(1) per defer rather than recursively scanning nested
bodies. The nested-unwind test at depths 16/32/64/128 measured respectively
120/248/504/1016 summary blocks and 89/185/377/761 summary visits (8N-8 and
6N-7), with 11 main blocks at every depth. These counts exercise the adverse
nested-defer shape in addition to the many-exit shape above.

Validation for this follow-up: 20 focused retention unit tests passed,
including 26 new behavior perspectives and two new deterministic scaling
checks. All 13 retention integration tests passed, including the updated
example normally and under forced preemption/moving GC; after sharing the
recovery capability summary, both new Never integration tests were rerun.
The 30 Send/Sync tests and the colliding-span regression also passed. Final
formatting and workspace/all-targets Clippy were checked for Linux x86_64,
Windows x86_64, macOS arm64 and macOS x86_64. Only Linux native execution was
performed; foreign native execution remains unverified. No full local suites,
full example audit, CI queries, new tickets, staging, commits or pushes were
performed. Ticket remains IN_PROGRESS pending review; it is not closed.


## Potentially panicking operations follow-up (2026-10-21)

The reviewer identified a safety gap in the Never-only unwind model: a
Void-returning call can panic before a cleanup assignment, leaving the original
value for an older defer to read. Ordinary checked operations now preserve
normal and exceptional successors. The exceptional successor occurs after
operand evaluation and before defining the expression result or executing the
following assignment. Both successors use the existing outcome-specific cleanup
summaries and shared unwind suffixes. Saved deferred calls also carry possible
unwind effects at invocation, without executing the saved operation during
registration; their operands still receive their own exceptional edges.

The conservative classification follows the existing constructor-flow model:
calls/methods/static calls/construction, indexing, non-short-circuit binary and
unary operations, field access, array/object creation, await/select and object
display. Direct recover and scalar display do not introduce a user-code panic
edge. Short-circuit operators rely on their operands' own edges. Index/field
stores and base initialization get exceptional successors after their operand
evaluation, and lock acquisition also retains its unwind path. This analysis
does not claim interprocedural proof that a particular returning call is safe.
No runtime, lowering or ABI code changed in this follow-up.

The audit checked ordering relative to temporary definitions, local assignment
kills, saved defer registration/invocation, block recovery boundaries, and the
normal/unwind summary transfer sets. Ordinary-call recovery can now bypass an
assignment without losing the incoming value's liveness. In main-body graphs
with no active cleanup/recovery, an uncaught unwind has no reads or continuation
and is omitted as a liveness-equivalent optimization. Isolated cleanup summaries
always keep the unwind outcome, because callers may have older defers.

Thirty new behavioral perspectives (258 total) cover five operation forms
(ordinary call, index load, division, remainder, index store) in six contexts:
cleanup before/after overwrite, recovering scope before/after overwrite,
nonrecovering normal continuation, and no source suspension. CLI tests cover
eight accepted/rejected cleanup cases. A runtime test and the runnable example
exercise overwrite before a Void-call panic, an older defer using the new value,
and recovery under four workers, budget 1 and moving-GC stress.

Complexity remains the prior sparse propagation and outcome-summary bound.
There is at most one additional constant-degree branch per potentially panicking
operation and no copied cleanup body or transfer set. Unwind suffixes remain
cached per cleanup prefix. No new repeated AST pass or interprocedural analysis
was added. Deterministic checks at N=16/32/64/128 now measure these necessary
exceptional paths:

| Shape | Main blocks | Facts | Edge visits | Summary blocks | Summary visits |
| --- | --- | --- | --- | --- | --- |
| N separate saved-defer scopes | 12N | 22N-4 | 24N-2 | 0 | 0 |
| N ordered overwrite cleanups / N exits | 8N+5 | 0 | 0 | 4N | 2N |
| N mixed-outcome cleanups / N exits | 8N+11 | 6 | 8 | 5N+4 | 6N |
| N nested unwind/recovery summaries | 15 | not recorded | not recorded | 8N-8 | 6N-7 |

The saved-defer scaling assertion retains its previous 12N+10 node bound;
fact/edge bounds now account for the required normal and exceptional paths
(22N+10 and 24N+10). This preserves linear growth assertions rather than
ignoring operation counts. Straight-line graphs without cleanup retain their
previous constant block count and linear facts; intermediate and branch shapes
retain their prior counts. These measured shapes do not imply an unconditional
linear bound across arbitrary binding/branch/continuation combinations.


Validation: 21 focused retention unit tests, 15 retention integrations,
30 Send/Sync tests and the colliding-span regression passed. Fmt and
workspace/all-targets Clippy passed on Linux x86_64, Windows x86_64, macOS
arm64 and macOS x86_64. Native execution was Linux only; foreign native targets
remain unexecuted. No full local suite, full example audit or CI query was run.
Existing affinity/runtime changes remain preserved. The ticket remains
IN_PROGRESS awaiting review, with no staging, commit, push or closure.


## Select commit exceptional ordering follow-up (2026-10-21)

The generic exceptional successor was attached to a select expression's final
continuation, after its case body. This allowed case assignments to incorrectly
kill an incoming value even when commit panicked before entering the case.
Select now owns its exceptional successor on the readiness/dispatch node,
after operand evaluation and before any selected binding or body. It no longer
uses the generic post-expression panic branch. Cancellation and successful
case successors remain intact; unwind still uses the existing shared cleanup
suffix and outcome-specific assignment sets.

The audit followed operand evaluation, dispatch, case binding definitions,
case bodies, cleanup recovery, and `LirInst::SelectCommit` code generation.
The Join commit calls `emit_task_terminal_value` before storing its binding;
a cancelled task can therefore unwind without executing the case's assignment.
Moving the source CFG edge to dispatch reflects this ordering. No lowering,
ABI, runtime or scheduler changes were required.

Eight new perspectives (266 total) cover bound/unbound joins, overwrite before
select versus overwrite only inside the selected body, and blocking/default
selects. The positive overwrite-before-select control includes a default arm:
otherwise the new closure itself may be retained across the select's suspension
and correctly requires E2402. The cancelled-task integration rejects the old
value case, accepts the default/overwrite control and runs that control with
four workers, budget 1 and moving-GC stress. It waits for cancellation before
constructing the closure. Output 2 proves recovery skipped the selected body
and retained the preceding overwrite. The runnable example also includes a
select recovery scope and an overwrite before its nonblocking select.

Complexity is unchanged: one shared unwind successor moves to the existing
select dispatch, removing the extra generic post-expression branch. This is
O(1) construction work per select, independent of its case count, with no new
AST scan, copied cleanup body or transfer set. The existing deterministic
straight-line/branch/cleanup/operand scaling checks remain unchanged and pass;
no new generated code or runtime work is introduced by this analysis fix.


Validation: 22 focused retention unit tests, 16 retention integration tests,
30 Send/Sync tests and the colliding-span regression passed. The updated example
passed normally and with forced preemption/moving GC. Formatting, diff checks
and workspace/all-targets Clippy passed for Linux x86_64, Windows x86_64, macOS
arm64 and macOS x86_64. Native execution was Linux only; foreign native targets
remain unverified. Full local suites, full example audit and CI queries were
omitted. Ticket remains IN_PROGRESS for review, without closure or publication.
