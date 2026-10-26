#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../../.."
cargo test -p willow_runtime array_join_measured_linear_work -- --nocapture
cargo test --test integration array_join -- --nocapture
cargo test --lib semantic::intrinsics -- --nocapture
cargo test --lib backend::abi::tests -- --nocapture
cargo test --lib backend::abi::alloc_effects_tests -- --nocapture
cargo fmt --all -- --check
for target in x86_64-unknown-linux-gnu x86_64-pc-windows-msvc aarch64-apple-darwin x86_64-apple-darwin; do
    cargo clippy --workspace --all-targets --target "$target" -- -D warnings
done
