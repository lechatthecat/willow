# Synchronous foreign safe regions

Run `willow run example/rust_foreign_region --offline`.

Both unreviewed functions use the same conservative synchronous bridge. The
native body blocks on a completion channel while another native thread explicitly
drives queued Willow Tasks. Those Tasks perform minor and major collections and
print `42`. Only their completion releases the native caller. Borrowed strings,
native stack addresses and same-thread native callbacks survive the wait, and no
general blocking-pool jobs are submitted. The 30-second timeout is a failure
guard, not synchronization.

Expected output:

```text
42
unknown input survives
42
blocking input survives
```

This runtime verification example deliberately uses native runtime ABI calls.
Ordinary adapters do not need this driver or explicit mutator registration.
These calls originate outside a Willow task and therefore remain on the caller
thread. From a task they instead use the bounded Rust pool; see
[`rust_blocking_pool`](../rust_blocking_pool). Callbacks here stay entirely in
native code, not back into Willow.
