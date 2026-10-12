# Native recursion depth

Willow synchronous calls consume native stack space, including when called from
an async function. There is no fixed Result/enum recursion-depth limit. Stack
exhaustion is fatal: it is not a recoverable language panic and does not run
`recover` or `defer` on the exhausted stack.

## Stack budgets

A synchronous `main` uses the process main-thread stack. On Linux, inspect the
soft limit with `ulimit -s` (KiB), and change it before launching the executable,
for example `(ulimit -s 16384; ./program)`, subject to the OS hard limit. This
changes the process stack, not task stacks. Other OS main-thread limits depend
on their executable/linker and OS configuration; Willow has no portable CLI
option to set them.

Synchronous call chains entered from async tasks run on runtime-owned native
stacks. `native_stack.rs::NativeStack::new` requests 8 MiB from corosensei per
stack, with platform guard/entry overhead. This size is currently fixed in the
runtime: there is no environment variable or Willow CLI option to change it.
`RUST_MIN_STACK` and a shell's stack limit do not resize these task stacks.
Changing this constant requires rebuilding the runtime. Increasing stack size
increases potential per-task memory consumption; use iteration for unbounded
input nesting.

## Compare source depth with physical frames

The release optimizer expands eligible small, pure scalar self-recursion by up
to three extra levels. In the simple `return dive(n - 1) + 1` fixture, one native
frame handles **four source levels**. GC-owning locals and Result/enum returns
are ineligible. Debug checked arithmetic also prevents this expansion.

The x86-64 frame cost includes the return address and saved frame pointer, not
just the `sub rsp` immediate. Root registrations return before the recursive
call; they do not each retain an additional native call frame. Root slots are
already included in the generated native frame, while their addresses are
stored separately in a growable thread-local heap vector (8 bytes per address
on x86-64, plus vector spare capacity). Runtime calls, allocations and GC require
some transient stack headroom.

Measured on Linux x86-64 at commit `810bdc1`, using the fixtures in
`tests/fixtures/native_stack_depth/` (depth intervals are 128 source levels):

| Shape | Build | Native bytes/frame | Source levels/frame | Descent root registrations/frame | Successful depth, 1 MiB process stack | Successful depth, 8 MiB task stack |
|---|---|---:|---:|---:|---:|---:|
| i64 | release | 32 | 4 | 0 | 130,048 | 1,048,448 |
| live String local | release | 80 | 1 | 2 | 13,056 | 104,832 |
| Result<i64, String> with `?` | release | 144 | 1 | 8 | 7,168 | 58,240 |
| enum with Array payload | release | 144 | 1 | 8 | 7,168 | 58,240 |
| i64 | debug | 64 | 1 | 0 | 16,128 | 130,944 |
| live String local | debug | 96 | 1 | 2 | 10,752 | 87,296 |
| Result<i64, String> with `?` | debug | 160 | 1 | 8 | 6,400 | 52,352 |
| enum with Array payload | debug | 160 | 1 | 8 | 6,400 | 52,352 |

These are fixture-specific measurements, not guaranteed limits. Argument size,
ASLR, compiler version, call shape, GC and platform change the boundary. Consult
the generated code when comparing different programs.

For release Result recursion, 1/2/4 MiB process stacks reach approximately
7,200/14,500/29,100 levels: native stack use scales with the 144-byte frame.
The effective release scalar cost is 32/4 = 8 bytes per source level; the
Result/scalar depth ratio is therefore about 144/8 = **18**, not 144/32 = 4.5.
A coarse doubling probe beginning at 1,000 reports last-success values of
1,024,000 and 32,000 on an 8 MiB budget even though their true limits are about
1,048,000 and 58,000. The apparent 32x gap follows from expansion and sampling;
it is not evidence of a separate root-stack cap.

The focused regression tests
`native_stack_overflow::depth_regression::result_maximum_depth_scales_with_stack_`
measure successful/overflowing depth intervals in debug and release on Linux,
verify returned values, reject unrelated failures, and assert proportional
scaling across 1, 2, 4 and 8 MiB process stacks (crossing the reported 32k boundary). The existing overflow matrix covers
ten other call/frame shapes in both build modes on supported hosts.
