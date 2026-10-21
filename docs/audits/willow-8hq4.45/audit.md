# willow-8hq4.45 implementation audit — 2026-10-21

Status: implementation and required local validation complete; ready for review. Ticket remains
in progress for review, as requested. No staging, commit, push, CI query, or Beads
mirror sync was performed. Initial working tree was clean (HEAD fa5db71).

## Result and reproducibility

The checked-in runtime_compare harness did not implement either requested map
case. The new cases reconstruct the public ticket workload, not the unavailable
historical benchmark source. They use 4096 prebuilt token strings and six million
get+insert pairs, or three million distinct integer inserts followed by twelve
million successful lookups. Go and Java execute equivalent operations and verify
the same sums. All runtimes use eight workers/processors; measured regions exclude
vocabulary construction, compilation and process startup. Java runs in a fresh VM
per trial, as do the other programs; no separate warmup is claimed. Timing uses
stdout phase markers and alternates runtime order. Five raw samples and source
hashes are retained in baseline.jsonl and final.jsonl.

Run from the repository root:

```
node benches/runtime_compare/harness.js run word_count map_lookup_insert --trials 5
python3 docs/audits/willow-8hq4.45/validate.py
```

Linux x86_64, Ryzen 7800X3D, Go 1.27.0, OpenJDK 26.0.2, release:

| Case/runtime | Baseline median ms | Final median ms |
| --- | ---: | ---: |
| word_count Willow | 411.518985 | 196.570521 |
| word_count Go | 100.785242 | 99.863741 |
| word_count Java | 82.282266 | 81.958382 |
| map_lookup_insert Willow | 471.595474 | 456.936975 |
| map_lookup_insert Go | 352.868275 | 350.061216 |
| map_lookup_insert Java | 319.396091 | 328.481338 |

String get+insert: **32.762 ns/pair**, below 50 ns. Word count is **2.398x**
the fastest runtime, below 3x. Integer operations: **30.462 ns**, versus 31.440 ns
before (3.1% lower median; no observed regression). These are same-host,
same-reconstructed-source comparisons, not a claimed reproduction of the original
Windows 164 ns figure. Exact-20-byte and 128-byte common-prefix keys are also
covered by deterministic runtime tests; the public token vocabulary is 9–17 bytes.

## Cause, implementation and ownership audit

The original string path used a dynamically dispatched borrowed key view,
SipHash, repeated UTF-8 validation, and a mutex even for a nonescaping scalar-value
map. The existing integer confinement optimization excluded String keys.
A diagnostic perf sample after adding confinement still attributed about 54% of
samples to hashing/lookup and about 8% to UTF-8 conversion; these are diagnostic
samples, not acceptance timing evidence.

- MapData now uses the existing dependency hashbrown 0.17.1, with its randomized
  foldhash default. Owned and borrowed keys share one static hash/equality
  implementation. One complete byte slice is hashed per operation; no enum-tag or
  length-prefix write is needed for this private standalone key. Foldhash's byte
  write incorporates length. Cross-kind collisions cannot change equality.
- Borrowed entry insertion reuses the initial lookup/hash, allocates an owned
  String only on vacancy, and retains the existing dense value index structure.
  Replacement remains allocation-free. GC write barriers still run before both
  insertion and replacement of reference values, using the previous value on
  replacement. Finalization and native-capacity accounting remain intact.
- Borrowed reads hash/compare bytes, avoiding conversion to str and its repeated
  UTF-8 scan. New owned keys still validate UTF-8. Public string-as-str behavior
  retains validation, including the fatal-invalid-UTF-8 regression test. String
  layout and the runtime ABI are unchanged. No unchecked UTF-8 conversion or raw
  pointer cache was introduced.
- The existing fail-closed, whole-function proof now admits string keys with
  scalar values. The runtime selects RefCell storage only for a confined map
  without reference values. GC reads the immutable discriminant and skips this
  payload; it never borrows the RefCell. Local aliases remain checked, and a
  no-preempt guard outlives each local borrow and its accounting reconciliation.
- Shared/escaping/async maps and reference-valued maps retain mutex protection.
  Freeze allocates its destination first and returns a shared independent copy,
  never a confined map. Keys own their bytes, so original string objects may be
  collected without invalidating the map. Local owner destruction frees native
  storage before releasing its external-byte charge.
- The new native-memory Local wrapper reconciles after mutable borrows only;
  read-only guards do not repeatedly compute an unchanged footprint. Its owner
  and charge lifecycle reuse the existing native-memory accounting implementation.
- Integer scalar-map representation, hasher, locking and runtime dispatch are
  unchanged. Compiler responsibility remains confinement analysis; runtime
  auto-build and project build scheduling are untouched.

Hashing tradeoff: foldhash is a randomized, non-cryptographic performance hasher,
with weaker resistance to adaptive collision attacks than SipHash. It is not a
cryptographic or hostile-input HashDoS guarantee. This is explicit rather than a
claim of equivalent hash-security properties. Hash iteration order is not a
language guarantee; textual map display continues to sort entries.

## Complexity and scaling

Independent dimensions: L = input key bytes, N = distinct keys, B = total stored
key bytes, Q = operations, I = LIR instructions/terminators, V = LIR locals.

Expected lookup/replacement time is O(L), with one hash and full-content equality
on candidates; extra space is O(1). New insertion copies/validates O(L) bytes and
has expected amortized O(L) table growth. Retained storage is O(N+B), matching the
need to own every distinct key after GC. Growth rehashes existing keys, as before;
uniform bounded-length workloads have linear aggregate hash work over geometric
growth. With arbitrary unequal lengths, a very long early key can be rehashed
O(log N) times: aggregate growth is bounded by O(B log N), not claimed to be O(B)
for every insertion order. Caching every key hash could reduce that dimension but
adds persistent memory and is not needed for the required short-key workload.
Worst-case colliding lookup remains O(N*L), as with hash tables generally; timing
is not proof of collision resistance.

String length work cannot be eliminated for fresh equal-content objects without
cached hashes/interning: content equality has an Omega(L) worst-case lower bound.
No duplicated input-length scan remains on lookup/update, aside from equality
when a candidate matches. A first insertion must validate and own its bytes.
No public entry API or String-layout/hash-cache change is required.

Confinement remains O(I+V) time, O(V) temporary space, one analysis per function,
with a boolean indexed by local ID. No alias graph, repeated recursive scan,
per-key code generation, or per-exit cleanup expansion was introduced. Generated
programs use the same constructor/get/insert ABI calls. Inspecting the final
release word_count executable with `objdump --disassemble=willow_user_main -d`
confirmed one willow_map_new_local call site, one willow_map_insert call site,
and two willow_map_get_into sites (measured loop and final checksum). There are
no willow_push_root/willow_pop_roots call sites in that function. Counts are
source-site counts, independent of the six-million iteration bound. Local GC tracing is O(1);
reference-valued shared maps retain bounded dense trace slices. Copy is O(N+B),
and display retains its existing O(N log N) sort plus output bytes.

Deterministic tests cover N=16,128,1024,8192 and L=20,128 with long common prefixes.
They bound insertion hash bytes by 6*N*L, assert exactly 2*N*L bytes hashed for N
get+update pairs, zero native/GC allocations for those pairs, unchanged retained
capacity on replacement, and complete charge release after collection. They hold
a local mutable borrow while invoking all GC hooks to prove those hooks do not
borrow local storage. Compiler scaling tests cover both i64 and String maps at
16,128,1024 call sites and assert exactly one visit per instruction/terminator.
Measured counts and test results are in validation.txt.

## Explicit validation perspectives

The new compiler test covers 23 perspectives: insert, get, contains, len, display,
freeze, local alias, constructor reassignment, alias reassignment, loop, Option
match, branch; rejection for Mutex wrapping, BlockingCell wrapping, array storage,
Option storage, closure capture, defer capture, parameter map, return map,
function-argument escape, reference values, and async function. The actual
word_count source separately asserts successful confinement. Existing integer
confinement checks remain, with String's previous rejection replaced by these
positive and negative tests.

Runtime checks additionally cover empty/Unicode/embedded-NUL/content-equal keys,
new and overwritten entries, absence, stable length, local/shared copies,
reference-value collection and movement during copy, accounting/finalizers,
integer/bool/float semantics, NaN rejection, concurrent scalar-map access, and
GC hooks. Seven examples (the six acceptance examples plus map_string_local.wi)
are run normally and with minor-GC stress, with exact output assertions.

Full workspace tests, full example audit, native macOS/Windows execution, and CI
results are intentionally not run. Linux is the only native execution evidence.
The four required cross-target Clippy checks are compile/lint evidence only.

## Completed validation

`validation.txt` records all commands with exit=0: fmt; 32 runtime map tests;
7 native-accounting tests; two named string UTF-8 tests; four compiler
confinement/scaling tests; 22 map codegen tests; seven map inference tests;
and 14 exact-output example executions. Total: 74 focused Rust tests.

Workspace/all-targets Clippy with warnings denied passed natively and with each
explicit target: x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc,
aarch64-apple-darwin, x86_64-apple-darwin. These are all local compile/lint checks;
only Linux x86_64 was executed. No required implementation work remains before
review. Review approval and ticket closure are deliberately left to the next turn.
