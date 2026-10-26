# Cleanup lowering implementation audit

2026-10-24. Working-tree implementation on baseline `7644eb5`; ticket remains
in progress for review. No closure, staging, commit, push, or CI query.

## Implementation and responsibility boundaries

`ir/lowered/cleanup.rs` lowers both synchronous and cooperative functions into
function-local typed scope, registration-state, cleanup-region, and panic-target
identities. `CleanupTransition` explicitly enters a scope, registers a site,
or exits to a verified state. The backend consumes these transitions instead
of discovering registration prefixes while visiting source blocks. Normal
cleanup regions retain the existing shared continuation dispatcher and its
call/reference/task-stack safety guards.

Each scope owns an executable panic CFG: registration test, consume, invocation,
optional lock release, root restoration, then recovery/parent propagation.
`emit_shared_panic_cleanup` translates those instructions and edges to the
existing runtime ABI. Synchronous root-depth snapshots are SSA variables defined
at the actual lexical entry. Async roots remain poll-frame based; suspension
edges carry the same verified state, and cancellation sites are bound once in
lexical graph order. Lock cleanup still commits and clears the frame-held token
through the existing ABI. Runtime auto-build remains unchanged.

`cleanup_emission.rs` binds physical flags, root variables, blocks and captured
storage once. Source block entry installs three persistent chain heads. The old
`LirDeferState`, `LirDeferScopeFrame`, and `LirDeferLedger` snapshots are gone.
Actions and captured maps are shared by `Rc`; no action/map/ancestor copies occur
when installing a block or transferring to a successor. Panic frames contain all
possible sites and therefore guard synchronous flags during cancellation;
ordinary frames contain the graph-proven registration prefix. Consume precedes
user cleanup, including cancellation during a recovering panic defer.

The verifier rejects mismatched normal/recovery registration prefixes, invalid
block targets/identities, wrong leave sets, partial/non-suffix flushes, duplicate
registration instructions, conflicting ownership, missing registration bodies,
and unowned registrations (including unreachable ones). A site whose instruction
exists but has not executed remains valid; its runtime flag prevents execution.

## Complexity and deterministic evidence

Independent dimensions: blocks B, instructions I, declared site mentions S,
exit site mentions X, lexical depth D, registrations R, deferred-body size A,
captured locals V, and cleanup contexts C. Expected graph construction is
O(B + I + S + X), with hash tables and identity comparisons on edges. Empty
scope suffixes skip to a nonempty ancestor; a traversed nonempty scope can be
charged to at least one requested site. Persistent state storage is
O(B + I + S + R), not O(B*D). Each registration adds one node; each normal
region key contains a pair of state identities, not copied prefixes.

Physical binding adds O(I + S + R + A + V) work/storage for a function. State
installation clones three reference-counted heads in O(1). Actions are cloned
once into shared entries; binding maps are captured once. At actual deferred
body emission, mutable local maps are still restored, costing O(C*V) in the
worst case; this existing body-emission boundary is not multiplied by ordinary
CFG edges. Call/reference protocol analyses and the compiler's other passes
retain their existing costs. This audit does not claim the whole compiler is
linear. Async cleanup sharing remains restricted where suspension would bypass
the selector definition; merging those contexts would be unsound.

A panic program with S unique sites has exactly 2S+1 blocks, two instructions
per site and one final root instruction (plus one optional lock release).
Parent propagation shares the parent's program. Normal shared regions retain
one action sequence per compatible context and a continuation per exit.
Runtime cleanup must inspect/execute its pending registrations and releases;
that linear work is required by semantics. Normal unwinding uses one descending
lock iterator across all frames, avoiding a repeated full lock scan. Per-scope
panic release stops when it reaches an outer depth. Deep chain destruction is
iterative, including when the last shared head is released.

Measured operation/storage evidence in `cleanup-lowering-counts.txt`:

- Depth/fanout 8/8, 64/64, 256/256: states 16/128/512, registration nodes
  8/64/256, edge identity checks 15/127/511.
- Persistent binding depths 8/64/512 crossed with fanout 4/32/256: exactly D
  payload creations and destructions, **zero payload clones** for all installs.
- A 20,000-node binding chain is destroyed on a 128 KiB thread stack.
- Panic sites 8/64/512 yield 17/129/1025 blocks and instructions (without locks).
- Deep scopes 8/64/512 each create one scope and one exit per lexical level.
- Fanout 4/32/256 yields one shared normal cleanup region for equivalent exits.

These are deterministic assertions in production graph/chain code, not an
asymptotic claim inferred from wall-clock timing. Generated-object tests also
verify cleanup action relocations independent of exit count and linear in site
count (`scaling_01` and `scaling_02`).

## Actual backend-order execution

`WILLOW_TEST_LIR_BLOCK_ORDER` selects graph traversal, source index order, or
reverse source order (entry first). It changes the actual Cranelift source-block
emission loop for sync, async and nested cleanup bodies. It does not merely
permute metadata. `tests/integration/cleanup_cfg.rs` runs each of these 24
perspectives under all three orders with explicit stdout expectations,
allocation GC stress, five workers and a one-step task budget:

1. Else-only merge followed by return.
2. Merge leaving an inner scope.
3. Two registrations unwind LIFO.
4. Continue unwinds an iteration.
5. Break unwinds an iteration.
6. Branch-local registration.
7. Recovery continues older defers.
8. A panic is consumed once.
9. Nested panic abandons the current defer without rerunning it.
10. A later unregistered defer is skipped.
11. Await before and after registration.
12. Return-position await.
13. Cancellation executes only registered async sites.
14. Cancellation before registration.
15. Captured String survives GC during cancellation.
16. Nested return after await unwinds both scopes.
17. Awaited return value is fixed before mutable cleanup.
18. Cancellation inside a recovering synchronous panic defer skips consumed and
    future sites; an atomic handshake forces cancellation inside the action.
19. Return from a Mutex section commits and releases.
20. Defers run before lock release.
21. Panic recovered outside a lock releases it.
22. RwLock write commits.
23. GC-managed protected value survives collection.
24. Cancelled holder commits/releases before outer defer.

Nested lock acquisition is intentionally unsupported by the existing V1
language checker; an exploratory fixture was removed after its expected E2605
rejection. No language restriction or acceptance condition was changed.

## Reproducible baseline comparison

`cleanup-lowering-measure.py` contains all public source generators: nested scopes,
fanout early returns, many defers, and repeated await/lock, each at N=8/16/32.
Both compilers use Rust 1.96.0 and the same release profile. The baseline binary
was built in a detached `7644eb5` worktree; both link the same fixed runtime
archive. SHA-256 identifiers, input hashes, every sample, code bytes and total
object bytes are recorded in `cleanup-lowering-comparison.json`. The earlier
baseline-only run is retained in `cleanup-lowering-baseline.json`.

For every shape/compiler the harness warms up once, then alternates five samples
and compares medians. Wall time includes CLI startup, parsing, lowering, codegen
and linking, but excludes compiler/runtime builds. Generated bytes are the sum
of ELF object sections with SHF_EXECINSTR, excluding linked runtime code.
Objects/binaries stay outside tracked docs. The companion reproduce script
accepts the two compiler paths and fixed runtime path and invokes this harness.

Final measurements (milliseconds are medians; bytes are generated executable
sections, not runtime/linker output):

| Shape | N | Baseline ms | Current ms | Baseline code bytes | Current code bytes |
|---|---:|---:|---:|---:|---:|
| deep_scopes | 8 | 96.83 | 95.51 | 5279 | 5215 |
| fanout_returns | 8 | 95.03 | 93.27 | 6239 | 6175 |
| many_defers | 8 | 94.12 | 93.33 | 5151 | 5151 |
| await_lock | 8 | 115.95 | 115.64 | 31967 | 30879 |
| deep_scopes | 16 | 109.52 | 95.21 | 6239 | 6111 |
| fanout_returns | 16 | 101.34 | 96.72 | 8479 | 8287 |
| many_defers | 16 | 99.19 | 95.14 | 6047 | 6047 |
| await_lock | 16 | 132.06 | 129.90 | 58463 | 55967 |
| deep_scopes | 32 | 196.94 | 103.19 | 8223 | 7903 |
| fanout_returns | 32 | 120.64 | 105.15 | 12191 | 11871 |
| many_defers | 32 | 141.12 | 97.89 | 7903 | 7903 |
| await_lock | 32 | 179.83 | 166.57 | 111135 | 106271 |

All 12 generated-code sizes are equal or smaller; all 12 measured compilation
medians are equal or faster (current/baseline 0.524–0.997). Small sub-percent
changes are timing noise, not claimed speedups. The deep-scope N=32 median falls
from 196.94 to 103.19 ms; many-defers N=32 from 141.12 to 97.89 ms. These finite
measurements establish non-regression for the sampled adverse shapes, not a
universal performance guarantee. Deterministic counts above establish scaling.

## Final local validation

- `cargo fmt --all -- --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- Same Clippy command with explicit targets: Linux x86_64, Windows x86_64,
  macOS arm64 and macOS x86_64 all pass after the final implementation/test fix.
- Compiler unit filter `-p willow --lib cleanup_`: 38 pass (24 graph tests,
  two persistent-chain tests, plus related existing cleanup call-path tests).
- Native `cleanup_cfg`: five grouped tests, 24 perspectives × three actual
  emission orders = 72 builds/runs, all pass under allocation GC stress.
- Existing native filters: `dfr_` 131, `scaling_0` 9, `scaling_11` 1,
  `scaling_12` 1, and `panic_recover_1` 10 all pass.
- Isolated runtime test `initial_async_cancellation_cleanup_can_yield_its_native_helpers`:
  one pass after the deterministic test-only quantum repair.
- `example/cleanup_cfg.wi`: passes on Linux, output
  `0, 1, 90, -1, 0, 1, 90, 1, recovered, resumed` (one item per line).
- Final source audit reviewed normal/exceptional exits, shared continuation
  guards, async suspension/cancellation, lock release, runtime flag consumption,
  root restoration, malformed LIR, persistent ownership and bounded-stack drop.
  No remaining concrete blocker was found in those paths.
- `git diff --check`: pass. No private specification material is included.


## Validation limits and additional test repair

Only Linux x86_64 native execution is measured. Windows x86_64 and macOS
arm64/x86_64 native linking/execution remain untested; cross-target Clippy is
reported separately. No full workspace/integration suite or full example audit
was run, and no CI results were queried.

A broad focused `cleanup_` filter also selected a pre-existing runtime test
whose 10,000 safepoints assumed a peer would run within the default 10 ms
quantum. It failed both combined and isolated; runtime source was identical to
the baseline. Its test helper now uses the existing thread-local quantum hook
with budget one. All 10,000 cancellation-suppression assertions and the peer
execution assertion remain. The isolated test passes; production scheduling is
unchanged. The final compiler-unit filter explicitly selects package `willow`.
