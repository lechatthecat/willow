# 0003 — Platform CI coverage

Issue: willow-38w.2.10.

The workflow in `.github/workflows/ci.yml` runs formatting, strict workspace
Clippy, workspace tests, and a separately named runnable-example audit on
Linux x86_64, Windows x86_64 MSVC, macOS Apple Silicon, and macOS Intel.
The example audit is excluded from the preceding workspace test invocation
only to avoid running the same long test twice. No platform marks its gates
as allowed to fail. Rust 1.95.0 is pinned to make the Clippy gate reproducible.

Keep x86_64 macOS supported: GitHub provides the `macos-15-intel` runner, so
it gets the same gates as Apple Silicon (`macos-15`). This choice is explicit
rather than depending on whichever architecture `macos-latest` selects.
Runner labels were checked against [GitHub's runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

All four targets enable synchronous task-stack preemption through the portable
backend described in [decision 0001](0001-task-stack-switch-capability.md).
The native-stack runtime and integration suites run on each target, including
cancellation, GC roots, worker affinity, and stack-overflow diagnostics.

All four native targets passed formatting, strict workspace Clippy, workspace
tests, and the runnable-example audit for commit `51265ab` on 2026-09-12 in
[GitHub Actions run 34683897875](https://github.com/lechatthecat/willow/actions/runs/34683897875).
These are native execution results for the named targets; other target triples
are not covered by this run.

For day-to-day changes, follow the
[platform compatibility guide](../platform_compatibility.md), including its
pre-merge checklist and the platform-specific regressions to avoid.
