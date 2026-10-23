#!/usr/bin/env bash
set -euo pipefail
# Run from the repository root. Focused filters only; no full suites.
rtk proxy cargo test -p willow --lib cleanup_ -- --nocapture
rtk cargo test -p willow --test integration cleanup_cfg -- --nocapture
rtk cargo test -p willow --test integration dfr_ -- --nocapture
rtk cargo test -p willow --test integration scaling_0 -- --nocapture
rtk cargo test -p willow --test integration scaling_11 -- --nocapture
rtk cargo test -p willow --test integration scaling_12 -- --nocapture
rtk cargo test -p willow --test integration panic_recover_1 -- --nocapture
rtk cargo test -p willow_runtime --lib initial_async_cancellation_cleanup_can_yield_its_native_helpers -- --nocapture
rtk proxy target/debug/willow run example/cleanup_cfg.wi
rtk cargo fmt --all -- --check
for target in x86_64-unknown-linux-gnu x86_64-pc-windows-msvc aarch64-apple-darwin x86_64-apple-darwin; do
    rtk cargo clippy --workspace --all-targets --target "$target" -- -D warnings
done
# Supply prebuilt release compilers and ONE fixed runtime library. Build both
# compilers with Rust 1.96.0 and the repository's release profile; baseline is
# detached revision 7644eb5. The harness creates all public input sources itself.
# Do not time compiler builds or run timing samples alongside other validation.
if [[ -n "${CLEANUP_BASELINE_COMPILER:-}" && -n "${CLEANUP_CURRENT_COMPILER:-}" && -n "${CLEANUP_RUNTIME_LIBRARY:-}" ]]; then
    rtk proxy python3 docs/audits/willow-r52c.15-measure.py \
        --baseline "$CLEANUP_BASELINE_COMPILER" \
        --current "$CLEANUP_CURRENT_COMPILER" \
        --runtime "$CLEANUP_RUNTIME_LIBRARY" \
        --output docs/audits/willow-r52c.15-comparison.json --repeats 5
fi
