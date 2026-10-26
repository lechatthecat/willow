# TLAB refill accounting audit

## Implementation

A refill publishes only its registered TLAB's fast-byte delta. The shared
major-policy evaluator takes an explicit sync flag: ordinary callers keep the
full byte merge, while the TLAB caller uses its already-published aggregate.
Minor checks and queued-minor retries inspect the same aggregate without a
registry walk. Both generated and runtime TLAB misses use this path. Compiler
code generation, TLS layout, ABI, allocation size and chunk capacity are unchanged.

Exact telemetry, collection snapshots and owner retirement retain their existing
full/owner-specific merges. The observed-byte cursor is shared by both merges,
so a later exact read charges only the residual delta. Assist accounting retains
its separate observation cursor and is unchanged.

## Complexity and lag proof

Let R be normal small-object refills, T registered TLAB records, L registered
old buffers, and K the fixed 32 KiB chunk capacity. Previously each refill's
accounting scanned T and L, with additional scans at policy evaluation. Now
its byte publication uses one expected O(1) hash lookup and one cursor read;
policy rechecks read scalar fields. Thus accounting is expected O(R) total,
O(1) amortized per refill, with no new persistent storage and no generated-code
growth. The existing hash map can resize amortized on registration; this is not
a worst-case hash-table lookup claim. O(1) publication is the feasible lower
bound for observing a refilling owner.

Each owner publishes before replacing its active chunk. All production chunk
retirement paths merge before retirement or immediately afterward while owners
are stopped. Therefore unobserved nursery allocation is at most T*K, regardless
of how often a hot owner refills while others are idle. At the refilling owner's
publication point its own lag is zero. First slow-path objects are charged at
installation; fast bytes exclude that first object. Repeated observations and
retired cumulative totals preserve continuity without charging twice.

Old buffers publish on refill/retirement via their existing sync_record path.
Unobserved old allocation is bounded by L*OLD_LAB_BYTES (128 KiB per buffer).
The major trigger/pacer input can therefore lag by at most T*K plus that old
buffer bound. This is an allocation-byte bound, not a wall-clock freshness
promise: an idle owner's unfinished buffer may stay unpublished until telemetry,
collection, refill or unregistration. The existing per-allocator chunk pacing
bound is retained; a full snapshot still observes all owners. Minor retry debt
uses the monotonic published young-allocation clock and has the same nursery
lag bound. Hard reservation limits still include the entire reserved chunk and
are unaffected by byte-publication lag.

Audited affected callers: tlab_alloc_slow, generated and runtime TLAB entry
points, minor_allocation_due, minor_refill_should_wait, major policy/pacer
sampling, install_tlab_chunk, all production retire_tlab_locked callers,
owner unregistration, telemetry merges, old-buffer refill/retirement and
collection snapshot retirement. There is no new loop multiplied by T or L.
Real collection, mark/sweep assist, exact telemetry, and non-TLAB allocation
policy may still traverse heap/registry data; their costs are not claimed to be
constant. Large-object fallback and diagnostic allocation stress are not normal
small-object chunk refills. No build-tool or compiler responsibilities moved.

## Deterministic evidence

Run `python3 docs/audits/tlab-refill-accounting/verify.py` from the repository.
The checked-in tests are the reproducible inputs; validation.txt records commands,
counts and exit status. No wall-clock inference is used for complexity.

| Registered TLABs | Hot-owner refills | Record visits | Residual bytes |
| ---: | ---: | ---: | ---: |
| 1 | 8 | 8 | 0 |
| 4 | 8 | 8 | 72 |
| 16 | 8 | 8 | 360 |
| 64 | 8 | 8 | 1512 |

The hot owner alternates fragmented tails with full chunks. Idle owners retain
unpublished objects. Every refill asserts both the exact residual and its bound;
repeated telemetry and unregistration then assert exact totals. The trigger test
asserts nursery crossing, retry-clock boundaries, and paced/unpaced major
crossings with zero additional registry visits.

Focused perspectives: T=1/4/16/64; one hot owner versus idle owners; repeated
refills; fragmented tails; exhausted chunks; exact residual bytes; young-clock
monotonicity; telemetry reconciliation; repeated-read idempotence; unregistration;
retired-count continuity; nursery threshold crossing; minor retry below/at its
clock boundary; paced/unpaced major threshold; soft-memory trigger; old-buffer
threshold matrix; partial old-buffer retirement; parallel TLAB debt; busy sweep
runway; multiple waiting owners; saturation; hard reservation caps; rooted
survival during deferred collection. Existing focused tests exercise these
surrounding paths without a full-suite run.

## Validation scope

All gates passed: fmt, 22 focused Linux runtime tests and workspace/all-target
Clippy for x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc,
aarch64-apple-darwin and x86_64-apple-darwin (see validation.txt). Foreign native execution/linking
was not performed. CI was not queried. Full workspace tests and the full runnable
example audit were intentionally not run locally. This runtime-only accounting
change introduces no language or compiler behavior and no new example program.
