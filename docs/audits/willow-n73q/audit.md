# willow-n73q implementation audit — 2026-10-26

## Problem and change

Reproduced the old report command overwriting a curated sibling Markdown file
using a temporary copy of the checked-in 2026-10-25 JSONL. Report mode now renders
to stdout by default. An explicit --out writes/overwrites the named destination.
The run command still writes its dated JSONL and Markdown artifacts. README and
CLI help document the output contract. Acceptance criteria remain unchanged.

## Verification

`python3 docs/audits/willow-n73q/reproduce.py`: four tests pass. These cover an
existing curated report and unchanged input with baseline/label overrides, no
implicit sibling creation, explicit creation/overwrite without touching the
sibling, and run-mode artifact preservation with execution mocked (no benchmarks).

Executed report twice against the actual checked-in MEASUREMENT_2026-10-25.jsonl,
with and without --baseline. Input and curated-report SHA256 hashes and complete
`git status --porcelain=v1 -uall` output stayed identical before/after.
`cargo fmt --all -- --check` and `git diff --check` passed.

Python-only command-output/documentation change: Rust Clippy, full test suites,
new performance measurements and CI polling were not run. Native Windows/macOS
execution is unverified. No platform APIs or runtime behavior changed.

## Implementation and complexity audit

Reviewed both branches of main and their shared render/output path. JSONL parsing,
baseline parsing and report contents are unchanged. Render is invoked once. The
default report branch has no write operation; explicit output is guarded by the
presence of a Path. The run branch still assigns its dated output Path.

Let N/B denote input/baseline records and M the generated Markdown length. Existing
parsing/grouping/rendering costs in N/B are unchanged. Changed output work is
O(M), required to emit M characters, with O(M) Markdown storage. File output adds
one O(M) write. Removed the former O(M) file read-back; no new scans, nested loops,
per-record work, caching or code-generation costs were introduced. These are
source-derived bounds, not timing claims. Regression tests exercise both output
branches; no scaling benchmark is warranted for the unchanged renderer.

GC acceptance work is separately stashed and blocked; it is not part of this fix.
