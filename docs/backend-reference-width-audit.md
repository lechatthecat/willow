# Backend reference-width audit

Ticket: willow-d9lm.2. Audited 2026-09-12 against `a90d80fb332a5d19dc88cc604ecb21b033a6a80e` plus this patch.
The ticket's historical 341 occurrences have become 251 in the baseline source;
there are 198 after this patch, including test inputs. Counts are literal
`types::I64` occurrences, so a source line can contribute more than one.

`reference_type(TargetFrontendConfig)` is the reference/function-address accessor.
All semantic type lowering receives that target width. The function-address constant
is removed; emitted function addresses, hidden receivers/environments, interface
object/vtable loads, task-frame signatures and reloads, nullable pointer values,
shadow-root nulls, and TLAB cursor/limit/next pointers follow the target width.
Existing target-pointer operations also use the accessor.

The supported-target guard remains. This is a behavior-preserving 64-bit migration
stage, not a claim that native 32-bit execution or its runtime/storage ABI works.
Rollback consists of reverting this patch; no data or runtime ABI is changed.

## Classification rules

- **Scalar**: genuine i64 values, numeric enum tags/type IDs, lengths, masks,
  counters, task IDs, lock acquisition tokens, state/phase/defer flags, floating
  point bit manipulation, and unused void placeholders. These retain their
  declared 64-bit ABI. A numeric ID or token is not an object pointer.
- **Payload word**: intentionally untyped 64-bit storage for generic container
  or enum values, including float bitcasts and boolean widening. These can carry
  references, but shrinking them indiscriminately would truncate i64/f64 values.
  The runtime and layout migrations (willow-d9lm.4/.5) must agree on storage and
  add pointer-to-word/word-to-pointer conversions before accepting a 32-bit target.
- **Test width**: explicit simulated target widths or expected scalar widths.
- **Eligibility assumption**: `assignable_repr` compares current supported
  representations using I64. It is not an emitted pointer instruction. The target
  guard keeps that assumption valid; a native 32-bit port must revisit it,
  especially named fieldless enums versus pointer-valued named types.

## Occurrence inventory

Line numbers identify this patch's formatted source snapshot. Every remaining
occurrence is classified below. The before/after columns also identify files
whose pointer-only sites were fully migrated.

| File | Before | After | Remaining source lines by category |
| --- | ---: | ---: | --- |
| `src/backend/abi.rs` | 18 | 18 | Scalar: 18; Test width: 436, 438, 439, 440, 441, 450, 451, 455, 460, 465, 476, 504 |
| `src/backend/cranelift/async_codegen.rs` | 40 | 22 | Scalar: 76, 77, 814, 949, 951, 977, 980, 1147, 1156, 1305, 1337, 1395, 1436, 1464, 1495, 1551, 1656, 1675, 1748, 1758, 1835, 1851 |
| `src/backend/cranelift/compile.rs` | 3 | 0 | All pointer sites migrated |
| `src/backend/cranelift/emit_collections.rs` | 3 | 3 | Scalar: 15, 19, 23 |
| `src/backend/cranelift/emit_expr.rs` | 11 | 7 | Scalar: 16, 59, 60, 167, 188, 195, 213 |
| `src/backend/cranelift/emit_interface.rs` | 3 | 0 | All pointer sites migrated |
| `src/backend/cranelift/emit_match.rs` | 8 | 4 | Scalar: 17, 18, 97, 146 |
| `src/backend/cranelift/emit_object.rs` | 1 | 1 | Scalar: 57 |
| `src/backend/cranelift/emit_option_result.rs` | 30 | 29 | Scalar: 19, 20, 111, 112, 157, 158, 224, 368, 369, 421, 422, 472, 473, 517, 518, 578, 631, 681; Payload word: 36, 132, 174, 384, 396, 437, 447, 488, 538, 585, 587 |
| `src/backend/cranelift/emit_pow.rs` | 6 | 6 | Scalar: 124, 137, 138, 139, 141, 143 |
| `src/backend/cranelift/emit_pow_f64.rs` | 24 | 24 | Scalar: 75, 88, 207, 212, 215, 231, 246, 254, 264, 277, 287, 308, 492, 537, 599, 629, 681, 683, 739, 750, 768 |
| `src/backend/cranelift/emit_stmt.rs` | 15 | 14 | Scalar: 52, 84, 90, 196, 211, 247, 273, 530, 540, 547, 825, 826, 889, 893 |
| `src/backend/cranelift/flat_calls.rs` | 7 | 1 | Scalar: 315 |
| `src/backend/cranelift/flat_control.rs` | 2 | 2 | Scalar: 36, 84 |
| `src/backend/cranelift/flat_objects.rs` | 7 | 7 | Scalar: 29, 31, 57, 108, 158, 205, 269 |
| `src/backend/cranelift/gc_codegen.rs` | 17 | 14 | Scalar: 130, 131, 149, 257, 261, 278, 285, 297, 303, 314, 342, 346, 347, 351 |
| `src/backend/cranelift/lir_gen.rs` | 42 | 37 | Eligibility assumption: 328; Scalar: 5308, 5334, 5338, 5340, 5350, 5360, 5361, 5366, 5391, 5423, 5482, 5572, 5611, 5641, 5650, 5949, 6079, 6084, 6542, 6563, 6592, 6685, 7239, 7403, 7528, 7551, 7635, 7764, 7799, 7815, 7949, 7971, 8109, 8131; Payload word: 6639 |
| `src/backend/cranelift/mod.rs` | 4 | 6 | Payload word: 1935, 1936; Test width: 2905, 2908, 2912, 2915 |
| `src/backend/cranelift/type_helpers.rs` | 10 | 3 | Scalar: 30; Test width: 174, 179 |

## Verification perspectives

The focused test checks 11 reference shapes at both I32 and I64: String, Never,
Array, Named, Option, Task, JoinHandle, TaskResult, Future, Fn, and Closure.
It separately checks i64, f64, bool, and void at each width (30 assertions).
The existing accepted-target test checks the accessor against the host ISA.
`example/reference_width.wi` exercises interface object/vtable dispatch, String
returns, Array storage, an integer above u32::MAX, indirect function calls and a
captured closure environment. Its exact output is registered in the runnable
example audit. The existing workspace regressions cover async frames, roots,
option/result representations, and runtime symbol/signature agreement.

These tests do not supply native Windows/macOS or 32-bit execution evidence.

## Runtime handle surface (willow-d9lm.4)

The historical issue predates the current Rust signatures: pure exported Willow
object handles already use raw pointers, and poll/cancel/mapper addresses use
function-pointer aliases. No production signature change is needed in this stage.
The table migration in willow-d9lm.3 made its `AbiTy::Ptr` classifications agree
with those existing signatures. New compile-time assignments in
`crates/willow_runtime/src/abi_signature_tests.rs` pin the actual Rust side,
complementing the compiler table's simulated-I32 signature tests.

The runtime source audit includes every public `extern "C"` declaration and its
callback aliases; no macro-generated exports, `include!`, or `export_name` aliases
were found under `crates/willow_runtime/src`.

| Surface | Existing representation | Numeric words intentionally retained |
| --- | --- | --- |
| Scheduler | `RuntimePollFn`, `RuntimeCancelFn`, `*mut c_void` frame | u64 task identities, deadlines, counters |
| Parallel map | `Option<I64Mapper>`, pointer input/result frame; typed mapper field | mapper's i64 input/output values |
| GC and roots | object pointers, pointer-to-pointer root slots, bitmap pointers | type/layout IDs, sizes, masks, counters |
| String, Array, Map | raw object pointers | lengths, indexes, kinds; Array/Map generic payload words |
| Future and Channel | pointer handles, distinct pointer payload APIs | distinct scalar payload APIs and flags |
| Mutex/RwLock | pointer handles and token output addresses | lock registration tokens and generic value words |
| Native netpoll | documented descriptor/token integer ABI | i64 OS descriptor/token transport stays fixed |

Shared 64-bit payload storage can hold pointer bits as well as i64/f64 values.
Changing that storage or its pointer conversions remains part of the coordinated
layout/native-32-bit work; it must not be mistaken for a pure pointer parameter.
This stage does not change the runtime ABI or enable native 32-bit execution.

## Local gate results

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- Clean `cargo test --workspace`: 7,455 passed, 26 ignored (13 suites).
- Full rebuilt runnable-example audit: passed, including `reference_width.wi`.
- Runtime signature tests: 7 passed, covering 65 typed assignments.
- Strict cross-target Clippy passed for x86_64 Linux, x86_64 Windows MSVC,
  aarch64 macOS and x86_64 macOS with `--features blake3/pure`. The default
  Windows cross build needs the unavailable `ml64.exe` assembler; the pure
  feature only avoids that dependency's assembly and is not native execution.

The initial workspace attempt had one catalog mismatch because its test binary
preceded the new example file. The rebuilt example audit and the clean full
workspace rerun both pass. Native four-job CI remains pending.
