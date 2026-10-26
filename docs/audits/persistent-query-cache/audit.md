# Persistent compiler query cache audit

The build/check frontend persists the successful typed-body query dependency closure. A subsequent process restores immutable body artifacts and red-green metadata, rebuilds declarations, validates dependencies, and reuses eligible bodies. Native code generation, runtime auto-build, and linking still run. Read-only AI/edit analysis deliberately bypasses persistent cache I/O.

## Compatibility and storage

The envelope includes a format revision, compiler source/version hash, target triple, runtime ABI hash, standard-library schema hash, entry identity and captured configuration. Build-time source hashing includes compiler sources, ABI and continuation dependencies, Cargo manifests/lock and the build script; no Git working-tree diff or private material is inspected. StableDefPath contains the canonical (or compiler-owned virtual) module identity, declaration kind, semantic owner, name and signature discriminator. Lambda ordinals are local to their owning declaration. Ambiguous identities are excluded. Function and type identities use existing name-based serializers; numeric syntax identities are snapshot-local references and their allocator ranges are reserved before new syntax is built.

Persisted query values include fingerprints, dependency edges, observed change stamps, verified_at and changed_at. Loading checks compatibility, a SHA-256 envelope checksum, size, metadata kinds, duplicate/missing graph references, timestamp ordering, exhausted revisions and cycles. Malformed or stale files are cache misses. Writers use a unique same-directory temporary file, flush it and atomically rename it over the destination; failed writes do not invalidate a successful compilation. Cache files are capped at 512 MiB. Cache setup errors also fall back to ordinary compilation.

Default location: the source directory's `target/willow-query-cache/`, partitioned by entry-path digest. `WILLOW_QUERY_CACHE_DIR` overrides the directory; `WILLOW_QUERY_CACHE=0` disables reuse and publication. `WILLOW_QUERY_CACHE_STATS=1` prints deterministic final body typecheck/reuse and tracked query counters. These counters do not include the existing generic discovery typecheck pass.

## Affected paths and fixes

- The existing revision path now supports a restored previous revision independently of a full previous Frontend. Bundled standard-library parsing participates in source tracking.
- Expanded generic units retain/reconcile syntax identities before spooling. Generated owner mappings follow reconciled IDs, including lambda ownership. Parsed and expanded snapshots are retained separately because their schemas and identities differ.
- Shared typed records are stored once; signature payloads remain shared by unit/read key.
- Backend runtime-debug and nil-check string inventories are sorted before emission. Previously even two cache-disabled builds could assign different string offsets. The native acceptance probe compares complete binary bytes and executable output.
- AI snapshot paths recognize the bundled virtual source. Queries read embedded bytes and validate hashes; edit transactions verify but exclude bundled sources from filesystem inventories and reject virtual edit targets. This repairs the source lookup failure exposed by existing revision tests without allowing cache files into isolated edit candidates.

## Complexity audit

Let S be encoded syntax/artifact bytes, B indexed bodies, M modules, V retained query nodes, E dependency edges, and D distinct signature payload bytes.

Export/load and graph validation visit O(S + B + M + V + E + D) data, apart from existing canonical map sorting and stable-name comparisons. The dependency closure is traversed once with a visited set; load uses Kahn traversal for cycle validation. Shared records and signatures are emitted once. Map canonicalization sorts structured keys, giving O(K log K) comparisons for each such map; deterministic encoding is the reason for this tradeoff. Hash table operations have expected constant cost, with key lengths charged separately.

Resident snapshot construction/decoding uses O(S + B + M + V + E + D) memory under the size limit, plus bounded artifact I/O windows. This implementation intentionally materializes a bounded snapshot before checksum validation and atomic publication; it does not claim streaming/minimal-constant memory. Compatibility checking currently decodes the envelope before comparing all key components, so rejected compatible-format files still cost O(S). This cost is bounded, not constant.

Source capture and generic discovery still inspect the input program; disk reuse does not make the whole compiler proportional only to changed text. Body candidate discovery and identity matching are indexed. Each expanded unit is reconciled once. Lambda identity traversal uses a visited set and a declaration-local ordinal rather than copying a path at every nesting level. Invalidation visits the demanded dependency graph; the generated body/module probes measure operation counts, not timing. Generated-code size and runtime complexity are unchanged by caching. Sorting debug string inventories adds O(N log N) comparisons over N distinct names and prevents nondeterministic symbol/data allocation.

## Reproducible validation

Run `python3 docs/audits/persistent-query-cache/reproduce.py target/debug/willow` after building the CLI. `measurements.json` records separate-process body counts (9/33/129 bodies), chain/fan-out module counts (4/16/64), corrupted-file recovery, equal diagnostics and native warm/cold binary comparisons. The runnable example is `example/persistent_query_cache.wi`.

Explicit focused perspectives:

1. Unchanged second process reuses all eligible bodies.
2. One body edit executes one final typecheck.
3. The following unchanged revision again executes none.
4. Compiler stamp invalidation.
5. Target invalidation.
6. Runtime ABI invalidation.
7. Standard-library schema invalidation.
8. Format revision invalidation.
9. Entry/configuration changes.
10. Build mode changes.
11. Debug/source-map/strip options.
12. Worker and semantic-feature options.
13. Project manifest/lock changes.
14. Missing cache file.
15. Empty/short/header-only files.
16. Truncated payload.
17. Checksum, separator and payload damage.
18. Invalid JSON/missing or malformed fields.
19. Invalid allocator ranges.
20. Exhausted revision counter.
21. Missing/duplicate dependency nodes, cycles and wrong payload kinds.
22. Invalid graph timestamps.
23. Concurrent atomic writers publish complete bytes.
24. Shared body records survive a round trip without duplication.
25. Complete signature/dependency payload round trips.
26. BodyIndex ownership round trip without retaining a previous chain.
27. Stable paths survive unrelated insertions and change with signatures.
28. Lambda inventories at increasing sizes have distinct bounded-size keys.
29. Virtual bundled module identity.
30. Whitespace/span changes preserve semantic reuse and diagnostics.
31. Defaults, constructors, statics and nested lambdas match cold analysis.
32. Generic instances and generic-lambda edits match cold analysis.
33. Native simple/nested-lambda build outputs match cold builds.
34. Read-only bundled source queries validate embedded bytes/digests.
35. Rename transactions preserve isolated candidate inventories.
36. Snapshot structural size/digest validation.

Named Rust test filters: `disk_body_`, `disk_snapshot_`, `stable_paths_`, `persistent_cache_`, `revision_preserves_defaults_constructors_statics_and_nested_lambdas`, `revision_whitespace_and_offset_edits_keep_semantic_bodies`, `revision_generic_instances_bind_template_syntax`, `revision_lambda_edit_in_generic_instantiation_keeps_effect_bindings`, `bundled_source_locations_use_embedded_bytes_and_verify_hash`, `rename_across_modules_and_replace_body_in_one_candidate`, and `snapshot_structural_size_and_legacy_digest`.

Linux native execution is the only native execution evidence. Formatting and workspace/all-target Clippy are required, including Linux x86_64, Windows x86_64 MSVC, macOS arm64 and macOS x86_64 target checks. Foreign linking/execution, full test suites, full example audits and CI are deliberately not run. Cross-target Clippy is static evidence only.

## Final observed results

All 12 snapshot/stable-path/persistent-cache unit tests passed, as did the four named revision regression tests and three named AI source/edit/storage tests above. The full separate-process reproducer passed. The runnable example printed `42`. `cargo fmt --all -- --check`, native workspace/all-target Clippy and all four explicit cross-target Clippy commands passed after the final implementation changes. `git diff --check` passed; the index remains empty. No CI status was requested or checked.
