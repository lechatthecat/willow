# 0002 — 32-bit target selection

Status: decision recorded; native 32-bit execution remains unsupported.
Issue: willow-d9lm.1 (decision spike, no target implementation).

## Decision

Use a native 32-bit **host build** as the first eventual 32-bit bring-up.
Do not add a `--target` option as part of the reference-width migration.
Keep the current explicit 64-bit ABI guard until backend and runtime support
are both available and the migration's execution gates pass.

This decision does not make i686 or armv7 selectable today. The pinned
Cranelift 0.134 backend has no native code generator for either architecture:
`cranelift-codegen/src/isa/mod.rs::lookup` handles x86_64, aarch64, s390x,
riscv64, and feature-gated Pulley. Other architectures return `Unsupported`.
Pulley32 is interpreter bytecode, not an i686/armv7 object that can be linked
with Willow's Rust static library. A command-line target spelling or a 32-bit
Rust toolchain cannot supply the missing machine-code generator.

## Evidence in this tree

- `src/backend/cranelift/mod.rs::Codegen::new` uses the native ISA builder and
  rejects non-64-bit pointers before object generation.
- `src/toolchain.rs::HostToolchain` selects object suffixes, linker arguments,
  system libraries, and runtime archive names with host `cfg!` conditions.
  Runtime builds invoke Cargo without `--target`.
- `Cargo.toml` pins the 0.134 Cranelift family; the resolved ISA lookup above
  is available in the Cargo source cache after `cargo fetch --locked`.
- `crates/willow_abi` and the backend currently exchange reference/function
  addresses in fixed 64-bit words. Selecting an ISA alone cannot change that.

## Why host-first

A supported native backend plus a compiler/runtime built for the same target
lets existing host selection and native linking remain coherent. Initial CI
should run the compiler, runtime, ABI export/link tests, and representative
reference/closure/async/GC programs on an i686 host (native or a complete
emulated userspace). Cross-compilation would additionally require explicit
target ISA features, object format, Cargo target archive paths, linker choice,
system libraries, target-aware capability checks, and an execution runner.
That is separate product scope and does not resolve the missing backend.

## Preconditions and follow-up sequencing

Before execution work in willow-d9lm proceeds, obtain a native 32-bit backend
compatible with Willow's Cranelift APIs, or explicitly approve a backend change.
Record the chosen architecture, backend version, and runnable CI environment.
Pointer-width classification can proceed on a 64-bit host as preparatory work;
it must not claim 32-bit verification or remove the ABI guard.

Then migrate compiler reference widths, runtime layouts/FFI, and function
addresses together, run ABI/link and GC tests on the 32-bit host, and only then
advertise that host as supported. If a native 32-bit backend remains unavailable,
retain unsupported status. Revisit cross-compilation after native host parity;
never treat Pulley bytecode as a native-object substitute.
