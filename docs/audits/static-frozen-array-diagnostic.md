# Static frozen-array initializer diagnostic audit

Date: 2026-10-26

The static initializer checker now adds a `.freeze()` hint to E0301 when the declared type is unary `FrozenArray` and the initializer type is unary `Array`. Existing error code, message, label and rejection behavior remain intact. The help explicitly says the element type must also match; freezing does not convert elements. No automatic source edit is offered.

## Reproduction

```willow
class Table {
    pub static values: FrozenArray<i64> = [1, 2];
}
fn main() {}
```

The original checker emitted E0301 without help. The regression test was run before the fix and failed on precisely that absent help. Replace the initializer with `[1, 2].freeze()` to fix it. `example/static_frozen_array.wi` exercises the corrected initializer, length and indexed values at runtime.

## Validation

```sh
rtk cargo test -p willow --lib contextual_arrays_tests -- --nocapture
rtk cargo run -p willow --bin willow -- run example/static_frozen_array.wi
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets -- -D warnings
```

The focused module passed 20 tests, including 21 explicit diagnostic perspectives: integer literal, singleton, empty literal, bool, string, nested array, frozen element, constructor, parentheses, conditional, element mismatch, scalar, other target, other frozen target, reverse conversion, already-frozen element mismatch, and five corrected initializers (integer, empty, bool, nested, constructor). Existing contextual typing and generated-store checks also passed. The Linux runnable example printed `static frozen array ok`.

## Implementation and complexity audit

Reviewed `check_class_static_fields` / `check_static_initializer`, its body-query path, `builtin_types::unary_arg` / `resolve`, and the existing array freeze checker. The hint uses already-computed normalized types and runs only after the existing compatibility failure. It performs at most two outer-type classifications, one arity check per classification, and adds one fixed-length help string. It does not walk element types, expressions, ancestors, branches or other fields; there are no additional recursive compatibility checks or array allocations in generated code.

For M failing initializers and arbitrary element type depth D or literal length L, the added work and retained diagnostic memory are O(M), independent of D and L. Per diagnostic the extra work and memory are O(1). This matches the lower bound for emitting M fixed-size hints. Existing type normalization, type formatting and compatibility costs are unchanged, and are not claimed to be constant. The extra generated-code size and runtime work are zero: only the error path is changed, before lowering. Build scheduling and runtime auto-build are untouched.

Deterministic output counts from `static_frozen_array_freeze_hint_scales_per_initializer`:

| Initializers | E0301 diagnostics | Help strings |
| ---: | ---: | ---: |
| 1 | 1 | 1 |
| 8 | 8 | 8 |
| 64 | 64 | 64 |
| 256 | 256 | 256 |

Reproducible scaling input: generate a class with M distinct fields `pub static xsN: FrozenArray<i64> = [1, 2];`, for N from zero to M minus one. The named test contains the generator and exact count assertions. These are output counts, not timings or instrumentation of total compiler operations; the constant added classification bound follows directly from the two nonrecursive calls. Nested arrays, frozen element types and conditional expressions are included in the perspective matrix.

## Limits

Validation ran on Linux x86_64. macOS and Windows native execution are unverified; CI was not queried. Cross-target Clippy is not applicable to this diagnostic-only change: no runtime, platform APIs, cfg, filesystem, thread or ABI paths changed. Full local suites and the full example audit were intentionally not run. This change is scoped to static-initializer E0301; other mismatch diagnostic codes are unchanged.
