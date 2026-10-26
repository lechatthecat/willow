# Closed-world interface dispatch

Implementation audit, 2026-10-21. Baseline: `0fd7e182093f9d47d4269e1f23ea8fb21e992f3e`, initially clean working tree. Results after changes describe the uncommitted working tree. No private specifications were used in this report.

## Implementation and boundaries

Existing class-call devirtualization already makes final/non-overridable methods and singleton virtual subtrees direct. The driver (`src/lib.rs`) declares every imported module/package and the entry program before emitting bodies. This change extends the proof to interface calls in release mode. A complete singleton with matching lowered ABI emits one direct call without a vtable slot load, target-address comparison, or indirect fallback. Nil validation, argument/receiver rooting, panic propagation, reference-call cleanup, and Self reboxing retain the shared paths. Debug retains indirect interface dispatch.

The bounded candidate union retains at most four addresses. Unknown slots or missing function IDs poison the proof permanently. Two through four actual body-address targets retain guarded calls and an indirect fallback. More targets retain indirect dispatch. Guard order uses descending static vtable-slot multiplicity, with FuncId ties; this is static evidence, not measured dynamic frequency. There is no profile input.

Open-method vtables still contain re-dispatch thunks. Their canonical static class is retained, then a separate build-wide canonical hierarchy/defining-method cache proves whether all reachable objects call one body. The proof ignores caller aliases and shares results across modules. Singleton thunks can therefore be bypassed. Multiple possible thunk bodies remain indirect; body addresses must never be compared against thunk addresses. Panic/root summaries stay conservative for thunks, including singleton ones.

No separately compiled extensible Willow-object library contract is introduced. Package dependencies are source inputs to the complete build. Fresh Codegen/EmissionState instances rebuild these proofs; registration and declaration-effect updates invalidate summaries before body emission. The rebuild test adds a second implementer in another module, edits it, then removes its import. Another test crosses two real path packages with transitive aliases. An object produced for a closed build must not later be linked as an extensible library with new Willow implementers; a future such output mode must disable the proof or carry link-time completeness/dependency invalidation. This change does not add interface-vtable import through native bridges.

## Complexity audit

Let T be runtime classes/edges, M be materialized method slots (including inherited class-descriptor slots and interface-vtable memberships), and C be interface call sites. Input identifiers/ABI arities are assumed bounded for these counts. Candidate reduction is O(M + C), constant work per slot (at most four comparisons), and O(number of used interface methods) summary storage. Per-site code is O(1) dispatch overhead: one direct call for a singleton, at most four guards/calls plus fallback, or just fallback. Argument/ABI processing remains O(argument count), necessarily independent of target count.

Thunk analysis builds the reverse class graph once, then visits each relevant class/edge at most once per method. Defining-method resolution memoizes every suffix, avoiding repeated ancestry scans. Its bound is O(T + M + C) when M counts the materialized inherited method slots, with O(T + M) retained analysis storage. If “methods” instead means only source method declarations, the precise bound is O(T * distinct queried virtual methods + C) in the adverse all-methods-inherited case: existing descriptors themselves contain that many slots. We do not claim linearity in source declaration count alone or for unrelated compiler passes.

The generated tests vary implementers (1, 2, 4, 5, 32), call sites (1, 8, 32), and chain/fan-out types (8, 32, 128), with 1/4 methods, four caller functions, and eight repeated calls per method. They assert exact slot visits, graph construction/visits, and generated IR indirect-call/guard counts. One base interface vtable can cover all subclasses, so interface slot visits in the thunk fixture equal the method count, while canonical hierarchy visits equal (T - 1) * methods. This is operation-count evidence, not an inference from timing. Sources are generated in the checked-in tests; `reproduce.sh` reruns them.

Responsibility remains in compiler analysis and code emission. No build scheduling, final-artifact cache, runtime ABI, or runtime auto-build behavior changes.

## Explicit verification perspectives

| # | Perspective | Evidence |
|---|---|---|
| 1 | Empty candidate set stays indirect | candidate unit table |
| 2 | Complete singleton is direct | IR counts and dump |
| 3 | Repeated identical targets collapse | candidate unit table |
| 4 | Two targets retain guards | IR counts |
| 5 | Three targets retain bounded candidates | candidate unit table |
| 6 | Four targets retain guards | IR counts |
| 7 | Five targets drop promotion | IR counts |
| 8 | Many targets stay bounded | 32-target IR, 64-target runtime |
| 9 | Unknown-only set is conservative | candidate unit table |
| 10 | Unknown before known cannot regain proof | candidate unit table |
| 11 | Unknown after known invalidates proof | candidate unit table |
| 12 | Known/unknown/known remains invalid | candidate unit table |
| 13 | Static multiplicity orders guards | candidate unit table |
| 14 | Equal frequencies have deterministic ties | candidate unit table |
| 15 | Repeated call sites share reduction | operation counts |
| 16 | Open singleton bypasses thunk | IR test |
| 17 | Inherited singleton body bypasses thunk | IR test |
| 18 | Deep chain shares ancestry results | operation counts |
| 19 | Wide fan-out shares hierarchy results | operation counts |
| 20 | Multiple methods/callers do not rescan per site | operation counts |
| 21 | Later module adds a second implementation | rebuild runtime test |
| 22 | Edited implementation is observed on rebuild | rebuild runtime test |
| 23 | Removing import restores singleton correctly | rebuild runtime test |
| 24 | Separate path packages share interface identity | package runtime test |
| 25 | Base-typed box reaches later override | module runtime test |
| 26 | Caller-local same-name class cannot rebind proof | shadow runtime test |
| 27 | Void/reference argument ABI and side effects | runnable example |
| 28 | Floating-point return ABI | runnable example |
| 29 | Allocating String result remains rooted | example under GC stress |
| 30 | Self return preserves vtable | example under GC stress |
| 31 | Singleton panic rejects neutral result | runtime test |
| 32 | Debug fallback preserves behavior | debug/GC control |
| 33 | Interface inheritance and default method | runtime test |
| 34 | Existing polymorphic panic/ABI behavior | focused prior regression tests |

## Validation

Passed: focused closed-world compiler IR/scaling tests (3), bounded candidate table (1 test with 12 inputs), closed-world integration tests (8), existing interface regression filter (3), promoted interface ABI/panic tests (2), bounded 1/2/4/5/64-target execution (1), open-implementer panic/root preservation (1), and allocating/mutating argument receiver snapshots (1). The runnable example is exercised in release and debug, including `WILLOW_GC_STRESS=all`. Formatting and workspace/all-target Clippy passed, as did explicit Clippy for x86_64 Linux, x86_64 Windows MSVC, aarch64 macOS, and x86_64 macOS. Raw generated IR/operation counts are in `analysis.txt`; benchmark trials and interpretation are in `benchmark.jsonl` and `benchmark.md`. Native execution is Linux x86_64 only. Foreign targets are Clippy/type-check evidence only; macOS/Windows execution is unverified. Full local test suites and full example audits were deliberately omitted. CI was not queried. The ticket remains open for review; no staging, commit, push, or Beads mirror sync was performed.
