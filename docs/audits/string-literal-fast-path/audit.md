# String literal initialized fast path

Date: 2026-10-21. Baseline: `ed880560a5f6457c2f3a7aeef1c98a892004c002`.
Implementation is ready for review; ticket deliberately remains open. No staging,
commit, push, CI queries, or private mirror sync were performed.

## Implementation and call-path audit

`FuncGen::emit_string_literal` now atomically reads the aligned static slot and
branches to a cold runtime call only for zero or the shared
`willow_abi::STRING_LITERAL_INITIALIZING` sentinel. The hit value and runtime
result join through a block parameter. Raw-byte address and length preparation
are in the cold block. The fallback for an absent literal declaration is unchanged.
Both LIR expressions and AST static initializers use this emitter; nested
expressions continue in its ready block. Allocation effects remain conservatively
unchanged, preserving caller rooting around the allocating miss path.

Cranelift 0.136.2 `atomic_load` is sequentially consistent, stronger than Acquire;
its IR documentation explicitly promises happens-before edges. A normal load is
not a valid replacement. The runtime's Release publication follows allocation,
root registration and reset-registry insertion. The sentinel now comes from the
ABI crate; CAS, waiter safepoints, registry, permanent rooting and quiescent reset
are unchanged. A hit introduces no allocation or safepoint. Reset still requires
that generated readers and initializers have stopped. The generated x86-64 code
was inspected with `objdump -dr`: aligned load and comparisons precede the branch;
the helper call is in the out-of-line miss path. Baseline disassembly has
unconditional helper calls in the string-build loop.

## Deterministic proof and scaling

`measure.py` generates release programs and uses GDB entry breakpoints to count
real runtime calls and inspect the slot argument before runtime processing.
For U = 1, 8, 32 unique literals and R = 1, 16, 256 dynamic repetitions, all nine
cases made exactly U calls and **zero initialized-slot calls**. Thus up to 8,192
literal evaluations still made only 32 calls. Raw counts are in `results.json`.
The GDB proof is Linux x86-64-specific (SysV rdi argument register).

The existing structural integration test independently exercises U = 1, 8, 32 and
S = 1, 4, 16 source repetitions: slot symbols equal baseline + U - 1 and helper
relocations equal U*S, with no legacy interning helper. These assertions passed.
The runtime hit-count test covers U = 16, 64, 256 and R = 1, 8, 32, proving exactly
U initial allocations, U registry entries and no allocations on later hits.

For N emitted literal sites, added IR work and generated code are O(N), with two
blocks and a bounded instruction sequence per site. Shared static storage remains
O(U + B), where B is total unique literal bytes. Existing literal-map lookup
hashes literal bytes; this change introduces no new lookup or byte scan. Runtime
hit work and extra memory are O(1), independent of U, literal length and registry
size; an acquire-capable read is necessary to observe publication/reset safely.
For E evaluations, total hit work is O(E); initialization copies O(B) bytes and
adds O(U) roots/registry entries. Reset remains O(U). Contended first-use retries
remain scheduler-dependent and unbounded in the same way as before. No scans,
per-exit cleanup duplication, caches, or build-scheduling changes were introduced.
Measured counts establish allocation/call/relocation scaling; the IR and memory
bounds are inferred from source, not wall-clock timings. Cold code increases the
constant amount of machine code per use, in exchange for eliminating hot calls.

## Measurement

Same Linux x86-64 host (Ryzen 7 7800X3D), release compiler and release runtime,
`benches/go_vs_willow/sync/willow/string_build.wi`, 2,000,000 iterations. Both
executables printed START, END and checksum 31301585. The baseline compiler was
built before editing and its unconditional calls verified in the saved object.
The runtime rebuild changes only the source of the identical sentinel value.

One warm-up per executable, then seven samples each, alternating order:

| Whole-process wall time | Before | After |
|---|---:|---:|
| Median | 70.999 ms | 65.559 ms |

Observed reduction: 7.66%. Raw samples are in `results.json`. This includes process
startup/teardown and was measured on a shared host during validation; it is an
indicative local result, not an isolated statistical claim or a platform guarantee.

Reproduce by building the benchmark with the baseline and modified compilers using
`willow build ... --release -o /tmp/...`, then run:

```sh
python3 docs/audits/string-literal-fast-path/measure.py /tmp/before /tmp/after   "$PWD/target/release/willow" /tmp/literal-fast-path
```

The script keeps generated sources, binaries and debugger commands in the supplied
temporary directory. Only reviewed JSON evidence is retained here.

## Validation and explicit perspectives

Passed:

- `cargo fmt --all -- --check`.
- `cargo clippy --workspace --all-targets -- -D warnings`.
- The same Clippy command with each explicit target: x86_64-unknown-linux-gnu,
  x86_64-pc-windows-msvc, aarch64-apple-darwin, x86_64-apple-darwin.
- `cargo test -p willow_runtime --lib literal_slots -- --nocapture`: 5 passed.
- `cargo test -p willow --test integration literal_slots -- --nocapture`: 4 passed.
- `willow run example/string_literal_fast_path.wi --release`: printed `é水🦀`.
- GDB operation-count matrix: all 9 cases passed.

The new integration matrix explicitly covers 22 perspectives in debug, release
and GC-stress modes, repeated eight times with collection: (1) empty, (2) ASCII,
(3) Japanese, (4) emoji, (5) combining character, (6) newline escape, (7) tab escape,
(8) quote/backslash escapes, (9–11) distinct prefix lengths, (12) identical equality,
(13) distinct inequality, (14) empty length, (15) UTF-8 byte length, (16) concatenation,
(17) function argument, (18–19) both return branches, (20) closure,
(21–22) first/last array entries. Existing focused tests additionally cover async
suspension, class statics, imported module statics, GC stress, unique/use scaling,
three heap resets, NUL termination, failed initialization restoration, allocation
counts, sentinel waiters cooperating with collection, and 2/8-worker first-use races
across 64 slots with one object per slot. The runnable example combines loop,
empty, Unicode, static, sync, closure, async and collection paths.

Native execution was performed only on Linux x86-64. Windows and both macOS
architectures passed cross-target Clippy but native linking/execution is untested.
Full local suites and full example audit were omitted per policy; CI was not queried.
