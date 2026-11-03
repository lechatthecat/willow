# Isolated byte-message plugins

The runtime's `plugin::Session` accepts an explicit manifest, content-verified
artifact bytes and named immutable resource snapshots. This API is usable from
Rust adapters; it does not change existing Willow static bridges or the trusted
in-process native loader. `sandbox-rust` governs builds, not runtime containment.

Run the WASM example from the repository root:

```sh
cargo run -p willow_runtime --example plugin_isolation
```

On Linux x86-64 with `/usr/bin/bwrap`, user/PID/network namespaces, seccomp and `close_range(CLOEXEC)`:

```sh
cc -static -O2 crates/willow_runtime/examples/plugin/guest.c -o /tmp/plugin-guest
cargo run -p willow_runtime --example plugin_isolation -- /tmp/plugin-guest
```

Native execution currently supports static Linux x86-64 executables. Unsupported
platforms, missing enforcement, dynamic-loader dependencies and unrecognized
engine/target selections fail closed. No trusted-mode fallback is attempted.
The bubblewrap flags require support for `--disable-userns` and
`--assert-userns-disabled`. Its namespace initialization and filter installation
must succeed before the guest runs. The trusted computing base includes the
host kernel, system bubblewrap and pinned wasmi engine; process separation alone
is not the native boundary.

## Host API

Set the digest from trusted distribution metadata. The example computes it
locally only to demonstrate the API. Use `Manifest::from_json` for bounded,
strict parsing. `Session::start` verifies the artifact before guest execution.
Only method IDs listed in the manifest may be submitted. `submit` copies bytes
only after reserving room for the request, response and deferred callbacks.
Completed requests retain their reservation until `take` removes them.

One thread owns the session and calls `drive_one`. A returned cancellation token
can be shared across threads. Cancellation and response delivery have one atomic
winner. Active execution errors terminate the instance and fail outstanding
requests; cancellation or expiry before dispatch only completes that request.
Deadlines include queue time. `shutdown` rejects new work, discards the backend,
revokes resource IDs and resolves pending work. Create a new session to restart;
request and resource IDs cannot cross generations.

Guest callbacks are bounded byte messages. After successful `drive_one`, drain
`take_callback(request)` on the host scheduler before taking its response. No host
callback is invoked during guest execution. A failed/cancelled request discards
its callbacks. Up to 16 callbacks share the request's response-byte allowance.

Resources grant read access to immutable snapshots, not host paths. Resource
names and contents must exactly match the manifest. There are no filesystem,
network, environment or child-process grants. `diagnostics` reports the effective
configuration without echoing guest-controlled error text.

## Guest interfaces

WASM uses wasmi 2.0.0 with validation, strict compilation limits, portable dispatch, fuel, a bounded stack,
one bounded linear memory and at most one bounded table. WASI is not linked.
Export `memory` and `call(method: i32, offset: i32, length: i32, resource: i64)
-> i64. Input is copied to offset zero. Return `(offset << 32) | length`; both
are unsigned 32-bit values and are checked before copying. Memory growth may
return failure or trap. Start functions receive a small bounded fuel budget.
Host imports check cancellation/deadlines on each call and share a per-request
copy-byte allowance equal to the fuel budget. Requests run in fuel slices with
cancellation/deadline checks between slices;
a bulk memory instruction is additionally bounded by the configured memory size.

The only imports are `willow.resource_read(id: i64, offset: i32, length: i32)
-> i32`, available with resource grants, and
`willow.callback(offset: i32, length: i32) -> i32`. Resource reads copy a prefix
and return its length; denied access or bad ranges trap with a structured host
error. Callbacks return zero on
success, -1 on a range/limit error. Both copy owned bytes and expose no host
pointers. A resource number is its one-based position in the manifest.

Native guests receive only stdin/stdout protocol socket, a discarded stderr and
an empty environment. `/plugin` and `/resources/<number>` are read-only snapshots;
the root is read-only. No host directory is mounted. The syscall allowlist denies
sockets, process/thread creation, namespace changes, tracing and mounting. Address
space and CPU time have hard OS limits; the host also enforces wall deadlines and
kills/reaps the container. CPU allowance is cumulative for the session.

Each frame has a 48-byte little-endian header: `WPLG` (4 bytes), version (u32),
tag (u32), method (u32), generation (u64), request (u64), resource (u64), payload
length (u64), followed by owned payload bytes. Version is 1. Tags are request=1,
response=2, error=3, cancel=4, callback=5. A guest first sends an empty response
with all IDs zero as its hello. Responses and callbacks echo the request's IDs.
Cancellation of active native execution terminates the container rather than
waiting for cooperative cancellation. Error payloads contain exactly one byte: trap=1, resource limit=2 or denied
capability=3. Any other error encoding is a protocol failure. Invalid/oversized/truncated frames terminate the
session. `guest.c` contains an echo/resource example plus adversarial test methods.
