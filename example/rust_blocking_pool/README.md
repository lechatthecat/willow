# Concurrent synchronous Rust calls

Run `WILLOW_WORKERS=1 willow run example/rust_blocking_pool --offline` from the
repository root. Expected output is `42` followed by `42`.

The two calls to `call` eagerly create ordinary Willow tasks. Their synchronous
Rust bodies rendezvous through a condition variable on the bounded Rust pool;
the single scheduler worker remains available to start both. Keep at least two
Rust pool threads for this rendezvous example (the default is four).
No review or special blocking declaration is needed. The generated resource
close is registered with `defer` and uses the same pool.

Thread-affine native libraries need an adapter-owned thread and a transferable
message proxy. The pool does not promise TLS affinity between calls. Review only
short, bounded functions for direct execution; see the
[policy and cost guidance](../../protocol/ai_toolchain/RUST_BRIDGE.md).
