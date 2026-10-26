# Public documentation naming audit

Date: 2026-10-26. Scope: the current tracked public documentation tree,
associated evidence and reproduction scripts, and incoming source references.
This is an implementation handoff; independent approval and closure are pending.

## Changes and evidence preservation

Replaced opaque internal issue keys with descriptive public names in README,
CHANGELOG, benchmark and fuzz documentation, audit headings, paths and commands.
Renamed 256 files under `docs/`; all 261 original tracked documentation files
remain present at their corresponding destinations. The initial working tree
and index were clean. Existing ignored local evidence directories, saved runtime
work and stashes were left in place; only tracked files were moved.

Updated `.gitignore` allowlists, the benchmark harness's two dispatch paths,
and the compiler test's `include_str!` path. No production compiler or runtime
behavior changed. Product names, release versions, ABI names and GC milestone
names remain intact. The private specifications ignore rule remains intact.
The identifier correspondence is retained only in local issue storage.

Historical evidence is not presented as freshly executed: recorded command and
artifact path strings were normalized to the public names. Sample values,
source revision hashes, result hashes and historical platform claims retain
their original meanings. Before the final heading cleanup, 214 original
documentation files were byte-identical, including files moved without content
changes. A comparison of all 36 original JSON/JSONL evidence files recursively
confirmed identical numeric/non-string values. A separate comparison of numeric
tokens in every original document, excluding the removed identifier tokens,
confirmed preservation of measurement values and dates.

The default-body sharing report already referred to an ignored source-only
generic-defaults reproducer. Its locally inspected public-language source was
copied to `../generic-foreign-defaults-counts.sh` and allowlisted so the updated
reference works in a fresh checkout. Its local original was preserved. No private
specifications were read or copied for this task.

## Validation

Run the narrow documentation checks from the repository root:

```sh
python3 docs/audits/public-documentation/verify.py
python3 docs/audits/benchmark-report-output/reproduce.py
cargo test --lib local_string_maps_word_count_confinement
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

All passed on Linux x86_64: six documentation checks, four report-output tests,
and one selected Rust test. The initial Rust invocation used an incomplete exact
test name and selected zero tests; the corrected filtered invocation selected
and passed the intended test. No full Rust suite, full runnable-example audit,
benchmark rerun, or CI query was performed. Native Windows and macOS execution
is unverified; this documentation/path change introduces no platform-specific
code, runtime changes or new cross-target behavior.

The verifier reads Git's public inventory, excludes missing old paths during an
unstaged rename, and includes untracked public replacements. It checks public
names, repository audit references in public documents, Markdown relative links
in audits, ignore behavior, Python/Bash/JavaScript reproducer syntax, and the
compiler/harness references. It does not execute historical reproduction scripts
that run broad test suites or expensive benchmarks. A separate tree-wide scan
found no stale renamed audit references. An unrelated existing source comment
references a local-only legacy ABI document; it was not changed by these renames.

## Implementation audit and complexity

Reviewed the complete rename inventory and content changes against HEAD, the
ignore rules, both harness branches, the compiler include and all moved script
references. The only Rust edit is a compile-time test fixture path. The affected
fixture is unchanged, and the selected Rust test exercises its actual lowering.
Measured evidence is preserved rather than regenerated with different workloads.

There is no new runtime, compilation or generated-code cost. The verifier has
independent dimensions F (public file count), B (text bytes), R (references), and
S (reproducer scripts). Its inventory sort is O(F log F); the fixed number of
scans is O(B), with O(R) filesystem checks and O(S) syntax-check invocations.
Stored text uses O(B + F) memory. These bounds are inferred from the code, not
wall-clock benchmarks. It does not traverse large ignored evidence directories
or perform pairwise file comparisons. No input-dependent language/runtime
algorithm changed, so runtime scaling experiments are not applicable.

Remaining limitations: internal references in unrelated existing source comments
and past Git history are outside this documentation task. The lexical name check
uses a reviewed allowlist of legitimate product-prefixed names; future new public
tool names may require an allowlist update. No claim of foreign native testing,
fresh historical performance measurements, or repository history cleanup is made.

## Review correction: generated benchmark reports

The report renderer still contained an internal identifier in its scheduler CPU
explanation. Removed that parenthetical from `benches/go_vs_willow/compare.py`
while retaining the technical explanation. The shared renderer supplies both
the report command and run-mode Markdown output; its only implementation change
is this constant string. Measurement calculations, output routing and benchmark
execution are unchanged, with no additional runtime or asymptotic cost.

Added a focused CLI regression using the checked-in measurement fixture. It
requires the scheduler section to be rendered and rejects internal-key-shaped
tokens in stdout, baseline comparison output, and explicit Markdown output.
The test failed in all three cases before the template fix and passed afterward.
It contains no concrete internal identifiers. The scan is linear in output bytes
for a fixed three-case matrix and adds no production work.

Revalidation after the correction: all five report-output tests and six public
documentation checks passed, as did fmt, workspace/all-targets Clippy and diff
checks on Linux x86_64. The previous Rust fixture test result remains applicable:
no Rust files changed in this correction. Full suites, benchmarks, foreign native
execution and CI were not run. The ticket remains open for independent review;
nothing was staged, committed or pushed.
