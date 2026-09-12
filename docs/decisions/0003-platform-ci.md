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

CI coverage does not enable synchronous task-stack preemption on macOS or
Windows. Those platforms retain the capability gate/E0810 described in
[decision 0001](0001-task-stack-switch-capability.md). Their tests must assert
that behavior until native stack implementations pass the required contracts.

The workflow is locally reviewable; remote macOS/Windows results require a
GitHub Actions run after publishing the change. A local Linux pass must not
be reported as evidence that those remote gates passed.
