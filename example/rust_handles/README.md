# Rust handles

Run `willow run example/rust_handles --offline` from the repository root.
Output: `42`, `42`, `1`, `7`, `2` on separate lines.

Public inherent associated functions use `native::Counter::new(...)`; instance
methods use `counter.read()`. Returning `Self` or `Box<Self>` creates an owned
handle. `&self` borrows it, while `self` or `mut self` consumes it and invalidates
every alias. `mut self` permits mutation of the consumed value inside Rust.
Straight-line use after consumption is diagnosed; ambiguous control flow and
aliases are checked by the generation-tagged handle table at runtime.

`use counter = ...;` registers lexical cleanup using the same mechanism as
`defer`. Cleanup runs on normal completion, return, propagation, loop exits and
across suspension. Generated `.close()` or a consuming call makes later scope
cleanup a no-op. No GC finalizer is added. Implicit cleanup has the same effect
requirements as explicit close, including review of nonblocking assumptions.

`&mut self` is rejected: opaque handles can alias and cannot promise exclusive
access. Keep mutable state behind an internal `Mutex` and expose `&self` methods.
Consumption while a native borrow is active is rejected without invalidating the
handle. Container arguments with owned handles are rejected; pass an owned
handle directly or use borrowed `&T` arguments.

An unclosed local owned handle produces a warning unless it escapes through an
argument, return, storage or closure capture. A local copy also counts as an
escape: static checking deliberately does not trace aliases or container
contents. All such handles retain runtime generation checks. `use` is contextual,
so existing identifiers named `use` remain valid.

On normal completion, debug builds report unclosed handles to stderr
as counts per Rust type, with the Willow creation location (`file:line`) of each
handle. This includes aliased handles and intentionally retained
pools. Reports do not change the exit status or destroy the handles. Release
bridges, abnormal exits (panic or signal), and programs without a Rust bridge
produce no report. Each bridge registers its reporter when its handle table is
initialized; the runtime has no dependency on bridge symbol names. Locations
are recorded after a call returns, including `Result`/`Option` payloads and calls
executed on the blocking pool. Forwarding a handle retains its original location;
slot reuse replaces the old entry. Release output has no recording calls.

If Rust defines synchronous `close(self) -> ()`, that operation also supplies
scope cleanup. A native instance method such as `close(&self) -> i64`
keeps its Rust behavior. You can call `counter.close()`, continue using the
counter, then call `counter.dispose()` to drop it. If the type already has an
instance method named `dispose`, use the first available spelling in the
sequence `dispose_2`, `dispose_3`, and so on. The generated disposal method,
`native::Counter_close(counter)`, and implicit `use` cleanup share the type's
Drop effect contract.
