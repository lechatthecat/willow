# Inline SATB barrier implementation audit

Date: 2026-10-26. Baseline: `a15120aca4e61615e7246daa0d39127795cca598`.
This report derives from public source, generated instruction counts and local
execution. No private specification text is reproduced.

## Result and protocol

The generated reference-store barrier now appends the overwritten **and** inserted
reference to the existing registered mutator buffer, without a runtime call, when:

1. The acquire-loaded GC phase is concurrent marking.
2. The existing owner filter proves no new generational record is necessary
   (young owner, remembered owner, or null inserted value).
3. The current execution context has an admitted buffer and room for two entries.
4. The acquire-loaded epoch closing flag is false.

`willow_abi::barrier_buffer` defines the prefix offsets and phase/pair constants;
`generated_context` defines the context pointer offset. Runtime compile-time
layout assertions verify both. The ABI fingerprint covers the new fields and
changed root-stack offsets so incompatible artifacts cannot be silently reused.

The first successful runtime barrier registers storage with the epoch and admits
it only for a registered mutator. The registration Arc keeps the buffer alive.
The generated path captures the previous reference, writes both buffer words,
then publishes length, and branches to a shared atomic reference store. There is
no allocation, call, yield or safepoint across this sequence. Null entries are
legal; the existing mark candidate filter discards them. Retaining insertions as
well as deletions preserves the incremental insertion barrier; deletion-only
inlining would be incorrect.

The current context loses admission at runtime safepoints and native entry/resume.
Unregister revokes all nested ancestor contexts before releasing registration
ownership. Stopped bounded and unbounded drains revoke the buffer's capacity
before consuming it. Reopening after a minor requires fresh admission. Checking
closing prevents producers from extending a partially drained buffer after the
first closure boundary. An in-flight pre-closing pair crosses no safepoint, so the
following stopped drain observes it before termination can finish.

Only the running owner writes the prefix without its mutex. Foreign buffer reads
remain restricted to stopped ownership under COORD. Runtime buffer operations
synchronize the Vec length from the published prefix before pushing/reserving.
The Vec's allocation remains stable while generated code runs; usize entries have
no destructors, so clearing or dropping a stale Vec length loses no resources.
The pending-prefix drain cursor is unchanged; entries are never shifted on retry.

## Investigated alternatives and scope

Card marking and remembered-set insertion remain in `remember_owner`, under the
heap lock. Both are hash sets, and the remembered header byte promises membership
in that set. Merely setting the byte/card in generated code would make minor GC
skip an unrecorded owner; exposing a Rust hash-table layout to generated code is
not an ABI. A separate deferred generational queue would require another ownership,
minor-root, duplicate suppression and overflow protocol. It is unnecessary for
the selected common case: an already remembered owner needs no repeated record.
New old-to-young edges therefore retain the existing fused runtime slow path.

Full/unadmitted buffers, closing/remark, unregistered legacy mutators, unknown
owner shapes and generational edges retain existing slow-path work. Global and
indirect-reference barriers are unchanged. Scalar array buffers keep their old
filter. Old-to-old stores with an unremembered old owner conservatively stay slow
during marking; this implementation does not broaden header-read assumptions.

## Complexity audit

Independent dimensions are source reference-store sites S, executed stores N,
registered mutators M, buffer capacity C, pending entries E, and nested execution
context depth D. The new eligible path takes constant work and no allocation or
runtime call per store. It appends exactly two words, including nulls. Storage
reuses the existing O(M*C) buffers, with 32 extra prefix bytes per buffer and one
8-byte pointer per generated context. No second queue or per-store allocation is
introduced. Slow-path Vec pushes remain amortized constant time.

Compiler emission and generated code are O(S), the required lower bound for S
independent stores. The deterministic codegen test uses 1/16/256 sites and measures
30/480/7680 instructions (including shared publication), zero calls in the inline
helper and one atomic store per site. The runtime fallback is still one call site.
Fast and slow paths share their atomic store rather than duplicating it.

Drain work remains O(E + M + 64) per complete epoch traversal; partial drains retain
both entry and registry cursors. Existing fake-clock/operation-count tests cover
many owners, one-entry budgets, fragmented pending registries and repeated retries.
The new capacity/order test covers C=1/2/3/32/256 and 1/16/256 pairs. No new scan
multiplies work across stores, exits or passes. Admission revocation at ordinary
safepoints is O(1). Unregister traverses D ancestor contexts once, O(D), because
all D cached pointers must become unreachable before releasing their shared owner;
this is not repeated per generated store. Other GC marking/indexing costs are
unchanged and are not claimed constant.

## Explicit validation perspectives

1. ABI prefix offsets on all four compiled targets.
2. ABI fingerprint sensitivity.
3. No runtime call in the inline emitter.
4. Generated-code growth at 1/16/256 sites.
5. One shared atomic publication per site.
6. Capacity one always falls back.
7. Exactly-full capacity two.
8. Odd capacity and remaining one-slot fallback.
9. Capacity 32 and 256 batching.
10. Runtime append after generated append preserves entry order.
11. Null deletion/insertion entries are handled safely.
12. Closing revokes appends before stopped sealing.
13. One-entry bounded drain retains its suffix exactly once.
14. Sealed capacity remains disabled after reopening until readmission.
15. Registered buffer storage can be reused after minor overlap.
16. Unregister drains pending entries.
17. Unwind unregister retains snapshot edges.
18. Nested context retirement revokes ancestor pointers.
19. Ownership transfer to another worker refreshes admission.
20. Buffer-only young deletion and insertion roots survive minor relocation.
21. Concurrent closure/sweep preserves newly active TLABs.
22. Chain/cycle/fanout/shared graphs survive concurrent-major/minor overlap.
23. Arrays/maps/channels/async frames preserve rewritten edges.
24. Reference-heavy minor/major remembered-set stress.
25. Generated-graph moving-collection stress.
26. Scalar versus reference async-frame barrier site counts.
27. Debug/release generated execution and alloc/minor/relocate stress.
28. Barrier verification and four workers with one-poll task budget.
29. Standalone reference replacement example across yields and collections.

`validate.py` stores actual named test results and four-target Clippy exits in
`validation.json`. `integration.json` records the three narrowly filtered
async-frame integration tests. `examples.json` records final executable runs.
Broader workspace/integration tests and the full example audit were deliberately
not run. No CI result was requested or checked. Linux x86_64 native execution is
verified; Windows x86_64 and macOS arm64/x86_64 have Clippy coverage only, with
native link/run behavior unverified.

## Reproduction and measurement interpretation

Build `linked_list.wi` and `object_churn.wi` from
`benches/go_vs_willow/sync/willow/` with baseline and candidate release compilers
into separate external directories, using `willow build SOURCE -o OUTPUT --release`.
The baseline binaries were built before source edits; neither checkout nor history
was rewritten. Run:

```sh
python3 docs/audits/inline-satb-barrier/measure.py --baseline BASELINE_DIR --candidate CANDIDATE_DIR --output docs/audits/inline-satb-barrier/measurements.json
python3 docs/audits/inline-satb-barrier/validate.py
cargo test -p willow --test integration async_frame_store:: -- --nocapture
```

Measurements use five independent process runs per workload and variant, alternating
order, two workers, checksum validation and peak RSS. Timing includes process
startup and teardown for both variants. The reported 95% intervals use Student's
t with four degrees of freedom. They are descriptive, not proof of an asymptotic
bound. The original workloads are retained; no claim is made that they force a
particular number of active-epoch eligible stores. The deterministic buffer and
codegen tests establish call elimination; application timings establish only the
observed workload-level effect. Detailed numerical results are in
`measurements.json`.

### Measured application results

| Workload | Baseline median | Candidate median | Time change | Median RSS before / after |
| --- | ---: | ---: | ---: | ---: |
| linked_list | 36.188 ms | 36.077 ms | -0.31% | 45,176 / 45,176 KiB |
| object_churn | 71.380 ms | 71.789 ms | +0.57% | 6,792 / 6,796 KiB |

The confidence intervals overlap for both workloads; these runs establish no
statistically clear application throughput improvement or regression. The concrete
improvement is elimination of runtime calls for the admitted active-mark case,
verified independently of these short-running application timings. No broader
standard-set or cross-platform performance claim is made.
