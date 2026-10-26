#!/usr/bin/env bash
# Run from the repository root. Focused validation only.
set -euo pipefail
cargo fmt --all -- --check
cargo test --lib runtime_source_names -- --nocapture
cargo test --test integration source_runtime_names -- --nocapture
cargo test --test integration panic_frames_ -- --nocapture
cargo clippy --workspace --all-targets -- -D warnings
for target in x86_64-unknown-linux-gnu x86_64-pc-windows-msvc aarch64-apple-darwin x86_64-apple-darwin; do
    cargo clippy --workspace --all-targets --target "$target" -- -D warnings
done
