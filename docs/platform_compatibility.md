# Maintaining platform compatibility

Keep language behavior and compiler/runtime code shared. Build separate binaries
for each OS and CPU, and isolate OS-specific implementation details behind
explicit target gates. A fix developed on Linux is not verified on another OS
until its native tests pass.

## Support and validation boundaries

The required native CI matrix is defined in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml):

| Platform | Target triple |
| --- | --- |
| Linux x86_64, GNU | `x86_64-unknown-linux-gnu` |
| macOS Apple Silicon | `aarch64-apple-darwin` |
| macOS Intel | `x86_64-apple-darwin` |
| Windows x86_64, MSVC | `x86_64-pc-windows-msvc` |

GNU/Linux aarch64 also enables native task stacks, but does not yet have a
native CI job. Musl Linux, Windows ARM64, Windows GNU, and other triples are not
covered by this matrix. Do not infer their support from a shared OS name.
The matrix also does not establish a minimum supported OS version: running on
one runner image does not prove compatibility with older OS releases.

See [task-stack capabilities](decisions/0001-task-stack-switch-capability.md)
for the exact feature gate and [CI coverage](decisions/0003-platform-ci.md)
for recorded native validation.

## Compiler, ABI, and linking

- Keep compiler capability checks, runtime dependencies, scheduler/safepoint
  gates, and integration-test gates aligned. Enabling a language feature without
  its runtime implementation is unsafe; retain the unsupported-target diagnostic
  until the implementation exists.
- Use the target's calling convention, pointer layout, stack alignment, and
  object format. Do not extend a Linux x86_64 assumption to Apple ARM64 or
  Windows x86_64. Keep generated frame layouts consistent with `willow_abi`;
  tests allocating raw frames must reserve the complete header.
- Preserve Apple's position-independent code configuration. The earlier
  absolute text relocations and `-no_pie` assumption failed on Apple ARM64.
  Keep linker behavior in [`src/toolchain.rs`](../src/toolchain.rs) and target
  code-generation settings in the backend, rather than scattering flags through
  tests and callers.
- Inspect object/archive files with the shared `object` helpers. Do not assume
  ELF symbol names, nonzero symbol sizes, or one relocation per reference.
  Mach-O can prefix symbols with `_`, omit symbol sizes, and represent an ARM64
  address through paired relocations. Do not assume GNU `nm` output exists on
  every host.
- Honor `CARGO_TARGET_DIR` and explicit runtime-library overrides. Preserve the
  runtime archive lock through linking: concurrent Cargo invocations can recreate
  the archive alias while another compiler process is trying to link it.

## Task stacks, GC, and exceptions

Changes here need native execution tests on all four targets, even if they
compile everywhere:

- Keep suspended stacks at stable addresses and resume them on their owning OS
  thread. Worker IDs alone do not prove thread identity across scheduler drives.
- Preserve callee-saved registers, floating-point state, task context, and GC root
  registration across switches. Parked root addresses must stay valid until the
  stack resumes or cleanup unregisters them.
- Preserve stack guards and probes. Unix signal handling and Windows stack bounds
  and exception handling have different implementations. Install thread overflow
  protection before allocating Windows task stacks.
- Do not unwind Rust through `extern "C"`, drop live generated frames, or use
  coroutine teardown to bypass language defers. Cancellation must run registered
  cleanup exactly once before terminal completion is observable.
- Keep native stack overflow fatal and test it in a subprocess. Do not assume
  Linux signal codes describe Darwin signals, or that Windows uses Unix exit
  status conventions. Also verify unrelated faults retain their normal handling.

The invariants and relevant test suites are documented in
[decision 0001](decisions/0001-task-stack-switch-capability.md) and
[decision 0004](decisions/0004-stack-probes.md).

## Scheduling and portable tests

- Synchronize on observable state, not short sleeps. Before cancelling a task,
  wait until the intended defer or wait site is registered. Await `task.result()`
  before asserting cleanup output. For a test specifically about unawaited tasks,
  use a separate cleanup-completion signal instead.
- Assert only ordering guaranteed by the language. Two ready tasks may print in
  either order; timer deadlines do not guarantee print order across workers.
  Check values, counts, and required dependencies without accepting missing or
  duplicated output.
- For preemption tests, require progress with more busy tasks than workers and
  use a bounded execution timeout. A tiny CPU loop finishing after another task
  is not a reliable fairness test.
- A scheduler drive completing zero tasks is not proof of deadlock. Consider
  queued work, claims in flight, other running polls, timers, I/O, and blocking
  syscalls. Exclude the blocked caller itself from the running-peer check.
- Preserve the ordering of scheduler idle-state reads: work moves between queues,
  claims, and active polls. Changing their observation order can create a false
  idle snapshot even when each individual read is atomic.
- Synchronous channel operations must recheck readiness after bounded scheduler
  work. An unbounded nested drive can wait on an unrelated task that needs the
  current send/recv to return.
- Preserve shared test-helper contracts: compilation failure, program failure,
  timeout, stdout, and diagnostics must remain distinguishable. Fixing error
  visibility must not silently change what existing assertions receive.

The recent Windows failure with 200 channel consumers was a missing running-peer
check, not a reason to skip the test or increase its timeout. Reproduce and fix
runtime races; only relax assertions when the language permits that behavior.

## Filesystem, networking, and distribution

- Use `Path`/`PathBuf` and `Command` arguments instead of concatenating shell
  strings. Account for spaces, executable/library suffixes, path separators, and
  line endings. Use unique temporary paths and avoid requiring a Unix shell.
- Do not assume case-sensitive paths or that an open file can be removed or
  replaced. Release handles before cleanup and surface unexpected I/O errors.
- Compare documented operation outcomes rather than raw OS error numbers.
  For example, repeated socket shutdown can report `NotConnected` after the
  requested state is already reached. Normalize that specific equivalent outcome
  without swallowing other errors.
- Use loopback and dynamically allocated ports in network tests. Do not depend on
  public network availability or fixed ports being free.
- Build and label release artifacts by target triple. A shared implementation does
  not make one native executable portable across OSes or architectures. Verify
  the packaged artifact on its target, including any required runtime libraries.
- When changing Rust, Cranelift, `corosensei`, native dependencies, or runner
  images, rerun the complete native matrix. Update the lockfile, toolchain pin,
  capability documentation, and packaging assumptions together where applicable.

## Before merging a compatibility-sensitive change

1. Identify affected target gates, ABI assumptions, OS calls, and test helpers.
2. Add a focused regression test and confirm it fails before the fix when feasible.
   Run relevant runtime/integration tests locally, including genuine-deadlock
   cases when changing scheduler progress detection.
3. Run the same gates as CI using the workflow's pinned Rust toolchain:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --locked --workspace --all-targets -- -D warnings
   cargo test --locked --workspace -- --skip runtime::test_runnable_example_files_compile_and_run
   cargo test --locked --test integration runtime::test_runnable_example_files_compile_and_run -- --exact --nocapture
   ```

4. Push the candidate and check the run for that exact commit:

   ```sh
   gh run list --commit <full-commit-sha>
   gh run watch <run-id> --exit-status
   gh run view <run-id> --log-failed
   ```

   Require all four native jobs, including the example audits, to succeed.
   `cargo check`, cross-compilation, and a local Linux pass are useful preliminary
   checks, but do not validate another platform's runtime behavior. A cancelled
   or superseded run is not a pass; another push can cancel the run being watched.
5. Record any support changes and link the successful run in the relevant docs.
   Do not make CI green by dropping a platform, ignoring a failing gate, or
   weakening a valid behavioral assertion.
