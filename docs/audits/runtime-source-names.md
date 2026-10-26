# Runtime source names implementation audit

Date: 2026-10-26

Runtime panic frames previously received monomorph linker names directly.
Generic instantiation now retains one source spelling with concrete type arguments
per instance segment. Nominal arguments use their declaring module identity rather
than whichever import alias first requested the instance. The registry is moved
through the frontend database into code generation, without cloning the whole map.

The backend renders diagnostic strings for synchronous functions, methods,
constructors, async poll bodies and async cancellation cleanup. Existing source-name
normalization removes package, local-unit and foreign-receiver implementation
prefixes. Linker symbols and function lookup keys are unchanged. Stack overflow
helper descriptors, fault sites, caller frames and task tags all consume these
rendered literals; no runtime demangler, allocation or ABI change is introduced.

## Responsibility and complexity

Let I be distinct generic instances, A the total size of their displayed argument
types, B the total bytes of diagnostic names emitted across compiled bodies, and
P the total number of namespace segments in those names. The shared registry uses
expected O(I + A) construction time and storage. A repeated request checks the
existing key before formatting its arguments. Type normalization traverses the
argument type trees, and nominal identities use existing indexed symbol lookups.

Each backend rendering splits its own name once, performs one hash lookup per
segment, then runs the existing linear source-name normalizer. Expected work is
O(B + P), including output bytes, independent of the number of unrelated instances.
Output-sensitive traversal is necessary to spell the requested names. The code
does not scan the instance map for each frame/call site. Async poll and cancellation
entries each render their own name, a constant number of entries per body.
There is no new pass over modules, inheritance chains, exits or runtime objects.
Temporary rendering memory is O(the rendered name length); retained literal bytes
grow with the displayed names. Existing literal interning handles reuse at fault
sites. Generated instructions, cleanup control flow, and runtime lookup costs are
unchanged; string lengths can grow because concrete type arguments are now shown.
These bounds are code-path analysis, not timing-based claims or a benchmark of the
entire compiler. Hash-table lookup bounds are expected rather than worst-case.

The deterministic lookup counter test uses 1, 16, 128 and 1024 distinct instances.
It asserts respectively 3, 48, 384 and 3072 segment lookups for qualified names,
and 1, 16, 128 and 1024 for a single increasingly deep namespace chain. This checks
both multiplication across bodies and an adverse deeply qualified name shape.

## Explicit validation perspectives

The renderer's table covers empty/plain/Unicode names, ordinary methods, nested
modules, local prefixes, package prefixes, foreign receivers, free monomorphs,
qualified monomorphs, local/package-qualified monomorphs, class methods,
constructors, nested generic arguments, unknown-instance fallback, malformed short
monomorph markers, malformed package markers, existing multiple type arguments,
and qualified type arguments. In total it checks 24 spellings plus scaling counts.

Executable regressions cover empty-array pop; i64 and String instances; multiple
type arguments; nominal and nested generic arguments; generic methods and
constructors; defer panic; generic async functions and methods in debug/release;
generic cancellation cleanup for functions and methods in debug/release; an
imported nominal argument with an import alias; and task stack overflow in both
build modes with successful shallow controls. The example runs normally and
prints `7` and `9`; passing an argument triggers an empty-array panic in
`last<i64>`. Existing focused panic-frame tests check locations, recursion,
async methods, release reporting and cancellation behavior.

Reproduction commands are in `runtime-source-names-check.sh`; deterministic
fixture sources are in the named Rust tests and `example/runtime_source_names.wi`.
No generated binaries or private specification material are included.

## Validation results and limits

All final checks passed:

- `cargo fmt --all -- --check` and `git diff --check`.
- `cargo test --lib runtime_source_names -- --nocapture`: 2 tests.
- `cargo test --test integration source_runtime_names -- --nocapture`: 7 tests
  containing the executable cases described above.
- `cargo test --test integration panic_frames_ -- --nocapture`: 8 related tests.
- `cargo clippy --workspace --all-targets -- -D warnings`.
- The same Clippy command with each explicit target:
  `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`,
  `aarch64-apple-darwin`, `x86_64-apple-darwin`.

Native execution is Linux x86_64 only. Cross-target Clippy is compilation/lint evidence, not native linking
or execution on Windows or macOS. Full suites and the full example audit are
delegated to CI; CI is not queried for this change.
