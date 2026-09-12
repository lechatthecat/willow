# Zero-root synchronous unwind bookkeeping (willow-8hq4.7)

The backend now inspects the complete LIR before binding parameters. A scalar
function with only audited operations can omit its entry root-depth snapshot and
abnormal-return root pop. Panic propagation, task cancellation, and the typed
neutral return remain in place. Direct callees restore their own roots, so even
recursive callers do not need a transitive root-effect analysis.

The proof accepts immediate values, scalar arithmetic/printing, direct and static
calls with immediate operands/results, and simple control flow. Reference
parameters, GC-owner locals, reference results, captures, cleanup regions,
indirect dispatch, and all unknown types/operations retain the conservative path.
This also covers references inside match arms and loops. Instance methods retain
their snapshot because they always root `self`; static methods can qualify.
An explicit `panic("message")` retains its String root, while a scalar wrapper
around that function can qualify. Cooperative async code is unchanged. The AST
emitter mentioned in the original ticket no longer exists.

## Code size

Measured on Linux x86_64, AMD Ryzen 7 7800X3D, rustc 1.95.0. The saved pre-change
compiler was built from `129302e`; both compilers linked the same runtime.
Counts use `objdump -dr --no-show-raw-insn` on objects kept with
`WILLOW_KEEP_OBJECT=1`. They include normal and abnormal paths, excluding
relocation records. Debug/release refer to Willow build modes.

| Function | Debug instructions before → after | Release instructions before → after |
| --- | ---: | ---: |
| `divide` | 118 → 107 | 98 → 84 |
| `middle` | 91 → 77 | 75 → 61 |
| `Math.calculate` | 91 → 78 | 75 → 63 |

In both modes, each function loses two `willow_root_depth` relocations and one
`willow_pop_roots` relocation. The normal path loses its one entry snapshot call;
the other two removed calls belong to abnormal return.

## Timing workload

```willow
fn divide(n: i64) -> i64 { return 120 / n; }
fn middle(n: i64) -> i64 { return divide(n); }
class Math { pub static fn calculate(n: i64) -> i64 { return middle(n); } }
fn main() {
    let mut i = 0;
    let mut total = 0;
    while i < 2000000 {
        total = total + Math::calculate(i % 19 + 1);
        i = i + 1;
    }
    println(total);
}
```

Build with `willowc build bench.wi -o bench` and add `--release` for release mode.
Time only the resulting executable, with one warmup per binary and seven measured
runs, alternating before/after order. The checksum must match in every run.
These local whole-process timings include startup and are not a general throughput
guarantee.

Measured after the test suites finished (2026-09-12), with checksum `44421206`:

| Mode | Before median | After median | Observed reduction |
| --- | ---: | ---: | ---: |
| Debug | 2.342818 s | 2.254341 s | 3.8% |
| Release | 0.086976 s | 0.085077 s | 2.2% |

Debug ranges were 2.307663–2.430104 s before and 2.232291–2.287802 s after.
Release ranges were 0.085464–0.087716 s before and 0.083814–0.085979 s after;
their overlap illustrates the limits of the small release timing difference.

## Regression coverage

`panic_effects::pe_zero_root_*` checks relocation absence for a recursive
panic-capable scalar chain, conservative retention across fourteen reference or
unknown cases, panic-effects-off safety, debug/release parity, typed returns and
recovery under allocation stress plus barrier verification. Existing native sync
stack tests exercise preemption and cancellation. The diagnostics example now
uses the scalar `slot_index` helper; before/after output is identical.

Validation: `cargo test --workspace` passed 7,359 tests (26 ignored, 13 suites).
All 10 `native_sync_stack` integration tests also passed with
`WILLOW_GC_STRESS=alloc WILLOW_GC_VERIFY_BARRIER=1`. Formatting and diff whitespace
checks passed. Under those global stress overrides, the broader panic/recover
run passed 151 tests with the two baseline failures below, and the GC-filtered
run passed 496 tests with the nine incompatible counter assertions below.

Repository-wide validation limitations discovered during this work:

- Clippy is blocked by existing lint errors tracked in `willow-dczj`.
- Global allocation stress reproduces a pre-existing cancellation cleanup GC abort
  (`willow-ikoh`, 5/5 runs with each compiler) and an output-order race
  (`willow-7rhg`, 29/30 pre-change runs and 30/30 changed runs).
- Applying allocation stress globally invalidates nine normal-mode TLAB and
  young-generation statistics assertions. Their default runs are the appropriate
  checks for those counters.
