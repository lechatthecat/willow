# Willow

[![Status: experimental](https://img.shields.io/badge/status-experimental-orange)](https://github.com/lechatthecat/willow)
[![Release](https://img.shields.io/github/v/release/lechatthecat/willow)](https://github.com/lechatthecat/willow/releases/latest)
[![CI](https://github.com/lechatthecat/willow/actions/workflows/ci.yml/badge.svg)](https://github.com/lechatthecat/willow/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

**An experimental native, garbage-collected language with compiler-checked concurrency rules and opt-in contracts. Not production-ready.**

Willow is statically typed and compiles through [Cranelift](https://cranelift.dev/).
It combines enums, pattern matching and classes with lightweight tasks, channels
and lexical locks. Its semantic CLI exposes the compiler's evidence to people
and coding agents.

## Concurrency checks: what the compiler guarantees

Ordinary `willow check` and `willow build` enforce the following rules. These
are guarantees within the compiler's modeled Willow semantics and the stated
assumptions, not proofs about arbitrary native code.

| Check | Guarantee for accepted code | Main limit or assumption |
| --- | --- | --- |
| [E2402 / E2408](#e2402-async-calls-capture-only-shareable-or-exclusively-lent-values) | Async captures are shareable or exclusively lent; transferred values cannot be used through old aliases. Non-`Sync` immutable statics are rejected in task-reachable code. | Conservative reachability and ownership rules can reject safe code; native shared memory is not analyzed. |
| [E2604](#e2604-a-lock-body-never-waits) | A lexical `lock` body contains no operation classified as waiting, including through calls; missing effect evidence is rejected. | Console output is allowed and can stall. Reviewed Rust `#[willow::assume(no_block)]` attributes are trusted. Computation time is unbounded. |
| [E2605](#e2605-nested-locks-follow-one-acyclic-order) | Nested lexical locks follow one acyclic acquisition order without reacquiring a held lock. | Locks the analysis cannot distinguish cannot be nested; use a [group lock](#acquiring-multiple-locks), whose acquisition is not fair. |
| [`@effects(...)`](#effect-contracts-declare-the-property-on-any-function) | An annotated function excludes the declared effects within the effect model, including transitive calls; unknown effects are rejected. | Native declarations are reviewed assumptions. `no_alloc` covers managed language allocation, not all native allocation or runtime bookkeeping. |

**Main limits:** these checks do not prove global deadlock freedom or the absence
of all data races. Tasks can still wait forever through channels and `await`
outside locks. Native adapters, compiler/runtime defects and unmodeled behavior
remain outside the guarantees. Conservative analysis can reject safe programs.
The [details below](#details-and-evidence) link to public examples, tests and
implementation evidence.

<a id="e2607-a-bug-finder-for-some-permanent-waits"></a>

**Bug finder, not a guarantee:** [E2607](src/semantic/permanent_wait.rs)
reports some permanent waits in a small straight-line subset. Unsupported code
can leave the whole program unanalyzed. The absence of E2607 provides **no
guarantee of progress**. See the [positive and unsupported cases](src/semantic/permanent_wait/tests.rs).

**Runtime guarantee:** an unfinished cooperative `async main` at global
quiescence fails with a channel/await wait witness and exit status 1. This detects
an observed whole-program stop, not partial deadlocks while other work or wake
sources remain. See the [precise scope and exclusions](#runtime-channelawait-deadlock-detection).

## Runtime instances

Willow programs use the process-default runtime. Additional runtimes are internal
test fixtures; concurrent fixture isolation is not public embedding support.
Host-managed creation, selection and destruction of multiple runtimes are
unsupported. See [runtime instances and proposed embedding requirements](crates/willow_runtime/EMBEDDING.md).

## Runtime channel/await deadlock detection

Willow uses **runtime global-quiescence detection** for channel/await deadlocks.
It does not require a new language subset or a static proof that every accepted
program terminates. E2607 remains a separate, incomplete compile-time bug finder.

**Guarantee for the covered execution.** When an unfinished `async main` and
its tasks are suspended on cooperative channel operations, `await`, or `select`,
and no runnable task, executing poll, in-flight scheduler claim, timer, registered
I/O waiter or outstanding blocking operation remains, the entry driver reports
`runtime deadlock` on stderr with a wait witness and terminates with exit status
1. It cannot treat that global stop as successful completion of main. This
covers empty receives, full bounded sends, channel cycles and await chains or
cycles once the whole execution reaches that state. Branches, loops and imported
helpers do not by themselves exclude a runtime observation.

The witness starts at main and follows registered await targets. It lists task
IDs, available names and spawn sites, and channel registrations (`RecvWait`,
`SendWait`, or ownership/handoff roles) with process-local channel identifiers.
For example, a main awaiting a receiver can produce:

```text
runtime deadlock: task 1 (main) blocked on await; no runnable task or wake source remains
witness (registered waits; select entries are alternatives):
  task 1 (main) blocked on await
    await -> task 2
  task 2 (receiver) blocked on channel/select
    RecvWait -> channel 0x1234
```

IDs, names, addresses and registration order vary. Channel identifiers are
opaque identities, not values or pointers to dereference. A channel registration
does not identify a potential sender; a select's entries are alternatives, not
edges that must all complete. The witness describes an observed global stop,
not a minimal cycle proof or a source-level causal trace. To bound diagnostic
size, it expands at most 64 tasks and prints at most 256 await/channel
registrations, explicitly marking truncation. Truncation does not change the
stop decision or exit status. Witness collection runs only after failure has
been established, using existing metadata with no extra per-operation tracking.

**Correct progressing programs remain accepted.** A queued sender, task
completion, timer, I/O event or blocking-operation completion keeps the driver
alive; returning main succeeds without waiting for detached tasks. See the
runnable [channel protocol](example/channel_wait_protocol.wi), which seeds a
bounded request before awaiting its relay and drains a closed reply channel.

**These cases have no channel/await deadlock-detection guarantee:**

- A subset of tasks is deadlocked while another task keeps running or yielding,
  or keeps a timer, I/O registration or blocking operation outstanding. An
  unrelated wake source also prevents a global-stop diagnosis.
- An external peer never responds, a blocking syscall never finishes, a native
  function violates its declared effects, or the program livelocks or starves.
  Elapsed time alone is never evidence of deadlock.
- Synchronous `main`, synchronous nested scheduler drives and native blocking
  paths. Some have their own panic diagnostics, but are not covered by the
  async-entry witness contract above.
- Detached blocked tasks after main has returned. Process exit intentionally
  does not require those tasks to finish.

Neither successful compilation nor a successful execution proves freedom from
deadlock on other executions. This contract assumes the compiler's cooperative
lowering and runtime wake-source bookkeeping, and does not certify foreign code.
Evidence: [entry driver and wake-source checks](crates/willow_runtime/src/scheduler.rs),
[witness traversal](crates/willow_runtime/src/scheduler_deadlock.rs), and
[executable deadlock and progress cases](tests/integration/concurrency/async_deadlock.rs).

## Project status

Willow is a research/engineering project exploring compiler enforcement and
agent-readable evidence for concurrent programs. Network services are a target
use case, but Willow is not currently a production substitute for Go or JVM
stacks. Its library and tooling ecosystem is still developing. Rust crates can
be used through [handwritten adapters](#rust-interop), not automatic access to
an entire crate's API. Choose it for experimentation and evaluation.

This README describes `main`; some features are newer than the latest release.

## Try Willow

Download a [prebuilt release](https://github.com/lechatthecat/willow/releases/latest)
for Linux x86_64, macOS arm64/x86_64, or Windows x86_64. Keep the distributed
`bin/` and `lib/` directories together. A system C toolchain is required for
linking; Rust is not required unless using Rust interop. Releases include checksums.

```sh
willow --version
willow init hello
cd hello
willow run
```

Or build from source (Rust 1.96+):

```sh
git clone https://github.com/lechatthecat/willow.git
cd willow
cargo build --release
./target/release/willow --version
```

See [installation and toolchain requirements](RELEASE_README.md).

## Integer overflow checks

Willow checks `i64` arithmetic overflow in debug builds (the default) and wraps
in release builds. Override that choice independently of optimization:

```sh
willow run example/integer_overflow.wi --release --overflow-checks=true
willow run example/integer_overflow.wi --debug --overflow-checks=false
```

`build`, `run`, and `debug` accept `--overflow-checks=true|false` or
`--overflow-checks true|false`. The last explicit value wins, regardless of flag
order. The library setting is `options.target.overflow_checks: Option<bool>`;
`None` uses the profile default, including wrapping for `release_with_debug_info()`.

The setting covers `i64` `+`, `-`, `*`, `**`, unary `-`, and `+=`, `-=`, `*=`
on locals, fields and array elements, across all compiled Willow modules.
Enabled checks raise the usual recoverable `integer overflow` panic with source
location and run `defer`; disabled checks wrap in two's complement. Unused
expressions still panic when checks are enabled. There is no `**=` operator.

`wrapping_*` and `checked_*` methods retain their explicit behavior. Division,
remainder, negative integer exponents, and array bounds retain their guards.
Shift amounts and other integer widths retain their debug/release policies.
Floating-point arithmetic, native Rust dependencies, and debug information are
unchanged. See the runnable [example](example/integer_overflow.wi).

## A taste of Willow

```willow
async fn deposit(balance: Mutex<i64>, amount: i64) {
    lock balance as mut value {
        value = value + amount;
    }
}

async fn main() {
    let balance = Mutex::new(100);
    await deposit(balance, 5);
    lock balance as value { println(value); } // 105
}
```

A contended acquisition parks the task; compiler-managed cleanup releases the
lock on supported exits. E2604 rejects waiting operations in the body, including
inside synchronous helpers. The console output above is an explicit exception.
See the [standard library](std/README.md) and [runnable examples](example/README.md)
for collections, classes, enums, error handling and I/O.

## Language essentials

A function, method, or block lambda returns its final expression when there is
no trailing semicolon. A terminal `if` / `else` can return a value from each
branch. Explicit `return value;` remains available for early returns. A
semicolon discards the expression's value; a non-void function that can reach
its end without a value reports E0205, with migration help to remove the final
semicolon or add a return. Loop and defer bodies do not implicitly return from
the enclosing function. Match expression arms can be used directly as a tail:

```willow
class User { pub score: i64; }

fn score(user: User) -> i64 {
    match user.score { 0 => 10, n => n }
}

fn main() {
    let user = User(2);
    println(score(user));
    println(2.0); // 2.0
}
```

`User(args...)` calls the same memberwise or declared `init` constructor as
`new User(args...)`. Both spellings support generic arguments, inference,
visibility checks and module-qualified class names. `new` remains supported;
existing construction code needs no migration. A local callable or declared
function takes precedence over a class with the same call spelling, including
module-qualified calls and module aliases.

`print` and `println` are ordinary standard-library function names, available
without an import for compatibility. They can also be imported from `std::io`,
aliased, or called as `std::io::println(value)`. Variables, parameters, functions
and methods may use these names. Local values and user functions take precedence;
use the fully qualified standard name to bypass shadowing. Both functions take
one printable value and return `void`; wrong arity, reference arguments and
unsupported values produce type diagnostics. To pass a polymorphic output
function as a callback, give it a concrete signature with a lambda, for example
`|value: i64| println(value)`.

Scalar output retains its I/O capability but is non-panicking in the language's
effect model, including through helper functions. It therefore adds no recovery
exit before constructor fields are initialized. Panicking arguments and
user-defined functions with the same names keep their own effects.

Default `f64` display preserves a decimal point for integral values in fixed
notation: `2.0`, `0.0`, `-0.0`. This applies to output, `.to_string()`, `format`
placeholders and array stringification. Scientific notation remains compact
outside `1e-6 <= abs(x) < 1e21` (for example `1e30`); finite values round-trip
without losing bits. Explicit precision formats keep their own rules. When
migrating, update textual snapshots that expected `2` for an `f64`; integer
output remains `2`.

JSON numbers retain their validated source spelling: parsing `2` and writing it
again yields `2`. Constructing a JSON number from `(2.0).to_string()` yields
`2.0`. JSON rejects non-finite numbers; display still uses `NaN`, `Infinity` and
`-Infinity`. See [the runnable language example](example/concise_surface.wi) and
[float display examples](example/f64_integral_display.wi).

## Details and evidence

### Rust interop

Place Rust code in `src/native.rs` and write `import native;` in Willow.
Public functions and structs define the interface directly; inherent Rust
methods become Willow methods. For example, `pub fn add(a: i64, b: i64) -> i64
{ a + b }` is called as `native::add(20, 22)`. Declare external Cargo crates in
`[rust-dependencies]`. Bind owned opaque resources with `use resource =
native::Resource::new();` to close them on scope exit, including errors and
cancellation. See the [native module example](example/rust_modules).

Rust crates require concrete, handwritten adapters and Cargo/Rust. Willow generates
ABI wrappers, but does not analyze native bodies. Ordinary synchronous foreign
calls from tasks run on a bounded Rust blocking pool; the task waits while its
scheduler worker runs other tasks. Generated opaque close uses the same path.
These calls satisfy `no_block`, but require a current reviewed `#[willow::assume(no_block)]`
assertion in `no_wait`, `no_suspend`, or lexical lock contexts. See the
[parallel example](example/rust_blocking_pool) and [execution policy](protocol/ai_toolchain/RUST_BRIDGE.md#synchronous-foreign-execution-paths).

- <a id="boundary-types"></a>**Boundary types:** scalars, strings, supported arrays, records, opaque handles and composed Option/Result values. Arbitrary classes, closures and raw pointers are not boundary types. See the [record mapping](example/rust_record_bridge/README.md), [module interface extraction](src/rust_module.rs) and [type mapping](src/rust_bridge_ir.rs).
- <a id="async-calls-willow-task-awaits-rust-future"></a>**Async direction:** Willow can await an adapter's Rust Future; this does not enable Rust-to-Willow callbacks. Inputs are owned, and the Future must be `Send + 'static`. Adapters must arrange native runtime services. See the [async example](example/rust_async_pull) and [interop directions](protocol/ai_toolchain/RUST_BRIDGE.md#remaining-interop-directions).
- <a id="where-the-guarantees-stop"></a>**Trust boundary:** reviewed `#[willow::assume(no_block)]` and `no_io` declarations are developer assertions, including conversions and destructors. A current review fingerprint is required; native behavior is not proved. See [bounded calls](protocol/ai_toolchain/RUST_BRIDGE.md#bounded-synchronous-calls-from-tasks) and [reviewed assumptions](protocol/ai_toolchain/RUST_BRIDGE.md#reviewed-effect-assumptions).

### Concurrency details

The [small concurrency core](formal/README.md) states the Send/Sync and loan,
E2604 and lock-order judgments, soundness assumptions, bounded executable
checks, implementation differential tests and trusted computing base.

<a id="e2402-async-calls-capture-only-shareable-or-exclusively-lent-values"></a>

**Captures and transfers (E2402/E2408).** `Send` and `Sync` are structurally inferred.
Mutable arrays/maps are not `Sync`; a fresh, unescaped local value with `Send`
contents can be exclusively lent to a task until its unconditional await in the same block. Old aliases of a
transferred value are unavailable. Loops and escaping values have stricter rules.
Task-reachable non-`Sync` static accesses are rejected, including through helpers;
indirect calls use conservative candidates. See [loan examples](example/owned_task_loans.wi),
[ownership transfer](example/ownership_transfer.wi), [capture tests](tests/integration/concurrency/capture_checking.rs)
and [static sharing checks](src/semantic/type_checker/static_sharing.rs).

**Shared mutable objects.** The compiler uses structural capabilities and
exclusive ownership checks, rather than a runtime race detector. Calling an
`async fn` creates a task (there is no `spawn` keyword). A mutable object cannot
be shared with another task through arguments, captured closures, channels, or
containing fields while retaining unprotected access. This includes reads that
could overlap a write. Diagnostics identify the unsupported boundary or alias
use; conflicting loans and reuse after transfer also point to the earlier
ownership boundary. A witness describes a conservative source-level conflict,
not a schedule observed at runtime.

Share `Mutex<T>` for mutable state, appropriate `RwLock<T>` or atomic values,
or structurally immutable/frozen data. Sending a non-`Sync` object through a
channel transfers ownership: neither the sender nor any connected alias may
keep using it. Receiving yields an owned value. A fresh local object can instead
be lent to one task; its aliases become usable after the task's unconditional
`await` in the block that lent it. Independent task-local mutation is allowed.
Protected references must stay inside their lexical lock. See the runnable
[shared-object example](example/shared_object_safety.wi).

To migrate rejected code, move construction inside each task, wrap the object
once and share its mutex, send an owned value and stop using sender aliases, or
send a frozen snapshot. For sequential work, await the exclusive loan before
reusing any alias. An opaque factory needs a checked `@sending` return contract
to establish freshness. Branches, loops, unknown callees and escaping aliases
are analyzed conservatively, so some race-free programs need these changes.
Task-local callable values do not gain sharing permission merely because the
scheduler is configured with one worker.

These checks cover the compiler's modeled Willow heap and task boundaries;
they do not verify handwritten Rust adapters, native threads, raw native aliases,
or foreign implementations of the ABI. Native code must uphold its declarations
and the runtime's ownership and synchronization contracts. The compiler, GC and
runtime remain part of the trusted implementation. No claim of detecting every
race in arbitrary native or interoperating code is made.

<a id="sending-after-channel-close"></a>

**Closed channels.** Sending after close raises a recoverable panic; accepted
buffered values remain available to receive. A selected closed send arm also
panics before its body. See the [closed-send example](example/channel_closed_send.wi).

<a id="e2604-a-lock-body-never-waits"></a>
<a id="how-the-effect-analysis-follows-calls"></a>

**Lexical lock restrictions (E2604).** `await`, `select`, channel waits, blocking
cells and synchronous file APIs are rejected inside a lock body. The analysis
follows helpers, dispatch and imports; unknown calls retain conservative effects.
Console output and reviewed nonblocking foreign calls are exceptions; an endless
computation is not rejected. See [effect propagation](src/compiler_db/effects.rs),
[lock restriction tests](tests/integration/concurrency/lock_restrictions.rs),
[indirect-call tests](tests/integration/concurrency/indirect_lock.rs) and the
[lock example](example/scheduler_aware_lock.wi).

<a id="effect-contracts-declare-the-property-on-any-function"></a>

**Effect contracts.** `no_block` excludes blocking, `no_suspend` excludes suspension,
and `no_wait` combines both. `no_panic` excludes modeled language faults, `no_alloc`
excludes managed language allocation, and `no_io` excludes modeled external access.
They include transitive effects and reject unknown effects; overrides cannot weaken
inherited contracts. Fault-reporting allocation and runtime housekeeping are outside
`no_alloc`; `no_io` is not an OS sandbox, and `no_panic` does not exclude resource
exhaustion. Changing an annotation still requires review. See [contract examples](example/effect_contracts.wi),
[I/O examples](example/io_contracts.wi), [checker](src/semantic/type_checker/effect_contracts.rs)
and [contract tests](tests/effect_contracts.rs).

### Contracts: from run-time checks to proofs

<a id="verified-prove-a-contract-with-z3"></a>

**`@pure @verified`** proves supported value contracts under their preconditions
using Z3. The subset includes integer scalars, bool/char, String equality and byte
lengths, read-only frozen arrays and closed immutable data. String lengths count UTF-8 bytes, including embedded NUL; concatenation
and other String methods remain unsupported. Unsupported constructs and unsuccessful
proofs fail closed. This trusts the compiler and solver and does not verify the
whole program. Install Z3 4.15.4
and keep `z3` on PATH, or set `WILLOW_Z3`. See the [proof example](example/static_verification.wi),
[strings](example/verified_strings.wi), [immutable data](example/verified_data.wi), [loops](example/verified_loops.wi) and
[solver-backed no-panic](example/proven_no_panic.wi).

A failed proof points to the failing clause or safety expression. For example,
replacing `non_negative`'s body in [this runnable example](example/verification_diagnostics.wi)
with `return x;` makes `willow check` report:

```text
error[E2630]: @verified `non_negative`: COUNTEREXAMPLE: ensures truth: sat; candidate inputs x = -1; result = -1
```

The primary label points to `result >= 0`, rather than the whole function.
Inputs use source parameter names and Willow values, including `[-3]`,
`new Point(-4)` and `Choice::Yes(-6)`. Explicit record constructors are not
replayed: their values use field summaries such as `Point { x: -4 }`, with a
`construction unavailable` limitation, instead of invented constructor arguments.
String values use Willow escapes such as `"\e"` for ESC. A candidate is a solver witness, **not a
replayed runtime violation**; loop induction states and abstract calls can make
it impossible to reproduce from the displayed inputs alone. Large values are
explicitly abbreviated. `UNKNOWN` remains a failed proof; an earlier type,
purity or effect error suppresses only the redundant skipped-proof diagnostic.

Verification JSON and the AI report retain `willow.verification.v1` and all
existing fields, including raw `counterexample_candidate` and `input_symbols`.
Each failed SAT obligation adds `presentation` with its own
`willow.verification.presentation.v1` schema: `evidence: "CANDIDATE"`, the failing
`location`/`obligation`, source-named `inputs`, an optional `result`, and an
`incomplete` reason when display extraction is unavailable. Consumers should
ignore unknown presentation versions and continue using the original fields.
Unsatisfiable preconditions identify the combined `requires` range and use
`evidence: "VACUOUS_PRECONDITION"` without candidate values. Display extraction
never promotes a candidate to a confirmed violation.

<a id="pure-and-run-time-contracts"></a>

**`@pure`** rejects observable interaction outside arguments and results, including
mutable heap reads and unknown callees. **`requires`/`ensures`** remain runtime
checks in debug and release builds; violations raise recoverable panics. See
[pure functions](example/pure_functions.wi) and [runtime contract cases](tests/regressions/runtime_contracts.wi).

### Locks designed together with the compiler and scheduler

<a id="e2605-nested-locks-follow-one-acyclic-order"></a>

**Nested locks (E2605)** follow a project-wide acyclic order and cannot reacquire
a held lock. Indistinguishable origins, including parameters of the same lock type,
are conservatively rejected. Lexical cleanup releases acquired locks on supported
exits, panic and cancellation; it does not roll back writes. See [ordered locks](example/ordered_locks.wi),
[parent/child locks](example/parent_child_locks.wi) and [ordering and cleanup tests](tests/integration/concurrency/ordered_locks.rs).

<a id="acquiring-multiple-locks"></a>

**Group locks**, such as `lock from as mut debit, to as mut credit { ... }`, acquire
members in runtime identity order. A contended group releases acquired members
before parking. Duplicate handles are rejected. Group acquisition is not fair
and has no retry bound, so starvation remains possible. See the [group implementation
and limitations](crates/willow_runtime/src/lock_group.rs).

**Hand-over-hand traversal** uses `lock walk cursor as node { ... }`, where
`cursor` is a local `Mutex<T>` variable distinct from `node`. Set `cursor` to
the next handle in the body. Fallthrough and `continue` acquire that handle
while the current mutex is still held, then commit/release the current mutex
and start a fresh iteration. `break` finishes the walk; `return`, panic and
cancellation release its current lock. Body defers run at each iteration's
exit, before attempting the next acquisition.

The initial acquisition parks normally. Every handover makes one nonwaiting
attempt: contention, a racing acquisition, or a self-link raises a recoverable
panic. This deliberately permits failure without proving a cycle; opposing
walks cannot wait on each other. A longer cycle may revisit a node already
released, so the application must choose when to stop. There is no automatic
retry or rollback of writes. The walk holds at most two mutexes during transfer
and one during user code. E2604 and protected-value escape checks still apply;
nesting another lock inside a walk, or a walk inside a held lock, is rejected
by E2605. See the [linked-list example](example/lock_coupling.wi).

### Semantic tooling for people and AI agents

The CLI provides `effects`, `impact`, `refs`, `type`, `symbol`, `rename` and
`overview`, with structured output. See the [AI toolchain protocol](protocol/ai_toolchain/README.md).

- <a id="exact-rename-change-a-symbol-preserve-unrelated-names"></a>**Exact rename** edits resolved occurrences and validates the candidate. It covers supported symbols in the loaded project, not strings or external consumers. Preview with `willow rename SELECTOR NEW_NAME --dry-run --diff`; see [rename tests](tests/semantic_rename.rs).
- <a id="what-an-agent-can-trust"></a>**Effect evidence:** `MAY` has a witness, `UNKNOWN` retains a conservative capability without one, and `NONE` has neither a witness nor uncertainty in the modeled analysis. Missing evidence is not purity. See the [effect-state example](example/effect_states.wi).
- <a id="compare-scalar-contracts-across-an-edit"></a>**Contract comparisons** do not prove behavioral equivalence. `contract_preserved` does not imply `implementation_verified`; approval requires an independently trusted policy verifier. See [comparison](example/contract_comparison.wi), [repair](example/contract_repair.wi) and [contract comparison tests](tests/contract_comparison_cli.rs).
  The caller supplies `--protected-base COMMIT`, `--approval-evidence FILE` and
  `--approval-verifier EXTERNAL_EXECUTABLE`. Choose a trusted verifier outside the
  editable checkout: its location alone does not authenticate it. The verifier
  must authenticate the evidence and complete request binding, exit successfully
  within 10 seconds, and return exactly
  `{"schema":"willow.contract-approval.v1","approved":true,"binding":<request binding>}`.
  The `CI` environment variable is neither required nor used as a trust signal.

- <a id="what-needs-language-semantics-and-what-lspmcp-can-provide"></a>**LSP/MCP** can expose semantic results, but the underlying language/analyzer establishes the guarantees. The transport alone does not establish lock safety or proof validity.

### Type parameter queries and rename

A type parameter uses the selector `module::owner::parameter`, for example
`util::fold::T` for `fn fold<T>` or `util::TopK::T` for `class TopK<T>`.
`symbol` accepts that selector or `file:line:column` on its declaration or a use.
`refs` lists type uses inside that owner, including uninstantiated generic bodies.
`rename` changes the declaration and those uses together; `--dry-run --diff`
previews the edits. Other owners' parameters with the same spelling are unchanged.
Rename rejects other parameters, visible type/import names and existing destination
spellings inside the owner to prevent capture. See the runnable
[type parameter example](example/semantic_type_parameters.wi).

### How this relates to other approaches

Willow explores selected concurrency rules and compiler-readable evidence, using
GC and familiar task/lock/channel constructs. This is a narrower claim than
whole-program safety. Navigation and exact rename can also be implemented for
other languages; this tooling does not establish an ecosystem advantage.

### Measurements

<a id="measured-ai-agent-efficiency-a-secondary-benefit"></a>

Agent cost and accuracy depend on the task and model. The semantic CLI is a tool
to evaluate in a workflow, not a guarantee of reduced tokens or correct edits.

<a id="runtime-performance-strengths-and-gaps"></a>

The [runtime comparison](benches/go_vs_willow/MEASUREMENT_2026-10-25.md) includes
workload-specific results, methods and raw data. The [benchmark harness](benches/go_vs_willow/README.md)
explains reproduction. These measurements do not establish general runtime superiority.

### Current boundaries

Willow remains experimental and is not suitable for production deployment.
The guarantees cover modeled language operations and depend on compiler/runtime
correctness and declared native assumptions. They do not prove termination,
all-race freedom, fair scheduling or global deadlock freedom. Conservative checks
can reject safe programs; absence of a bug-finder diagnostic adds no guarantee.
Contracts constrain edits only while their annotations and assumptions remain valid.

MIT licensed. See [LICENSE](LICENSE).
