# 0004 — Inline stack probes

Issue: willow-38w.2.11.

## Decision

Retain the production backend's enabled stack probes and inline strategy.
Pin `probestack_size_log2 = 12` explicitly: touch intervening 4096-byte regions
before consuming a large frame. This was already Cranelift's default threshold;
making it explicit prevents dependency defaults from silently changing policy.
The production-configuration unit test checks all three settings.

Inline probes touch the active stack and need no probe libcall with a
thread-stack limit assumption. They therefore compose with the task-owned
non-moving Linux/macOS/Windows stacks in [decision 0001](0001-task-stack-switch-capability.md).
Those stacks retain guard pages; Unix registers the active guard range and
Windows maintains native stack bounds for the fatal overflow handler. Probes do not replace guard pages or make overflow
recoverable. Four-KiB spacing also touches every page on systems with larger
pages, at the cost of additional touches.

## Measurement (Linux x86_64, 2026-09-12)

Built release-mode Willow object files with `WILLOW_KEEP_OBJECT=1` and inspected
`objdump -d` output. These numbers are the largest immediate `sub ..., %rsp`
in each generated object, not a sum of all pushes or a bound on total stack use:

| Source | Largest immediate stack allocation |
| --- | ---: |
| `example/hello_world.wi` | 64 bytes |
| `example/game_of_life.wi` | 304 bytes |
| recursive `dive(n, a0, ..., a599)` with 600 i64 arguments | 9600 bytes |

The wide fixture passes its parameters to the recursive call, keeping the
outgoing argument area live even in release mode. It exceeds a guard-page
interval today. The two ordinary samples do not; this is a sample, not a proof
that realistic generated frames stay small. There is no language-level frame
size ceiling that would justify disabling probes.

Reproduce the wide shape using the `wide_frame` case in
`tests/integration/native_stack_overflow.rs`. Its prologue contains ordered
page touches before reserving its large frame. That suite runs ten call/frame
shapes in both debug and release: shallow calls return normally and deep calls
terminate with the native-stack-overflow diagnostic.

## Verification and platforms

The 20 call/frame perspectives pass locally on Linux x86_64. The production
flag test and overflow suite have no platform skip and are included in the
Linux/macOS/Windows CI workflow from decision 0003. Native synchronous task
stack tests separately cover stack switching and cancellation on all four CI targets.
macOS and Windows execution results remain pending those remote CI runs;
this decision does not claim a local Linux run verifies their exception paths.
