#!/usr/bin/env bash
# Run from the repository root. Focused checks only; no full-suite run.
set -euo pipefail
cargo test -p willow --lib closed_world_ -- --nocapture --test-threads=1
cargo test -p willow --lib interface_candidates_complete -- --nocapture
cargo test -p willow --test integration closed_world_ -- --nocapture
cargo test -p willow --test integration leaf_method_calls::interface_ -- --nocapture
cargo test -p willow --test integration promoted_interface_ -- --nocapture
cargo test -p willow --test integration bounded_interface_promotion_preserves_every_target -- --nocapture
cargo test -p willow --test integration open_implementer_keeps_bracket -- --nocapture
cargo test -p willow --test integration release_receiver_snapshot_survives_allocating_and_mutating_arguments -- --nocapture
# Existing comparison harness; builds languages, executes only virtual_dispatch.
python3 benches/go_vs_willow/run_sync.py virtual_dispatch --trials 5 --workers 8 --languages willow,go,java
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
for target in x86_64-unknown-linux-gnu x86_64-pc-windows-msvc aarch64-apple-darwin x86_64-apple-darwin; do
    cargo clippy --workspace --all-targets --target "$target" -- -D warnings
done
