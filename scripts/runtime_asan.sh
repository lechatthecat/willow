#!/usr/bin/env bash
# Opt-in, focused runtime memory-safety checks; shared by local runs and CI.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

toolchain=nightly-2026-09-20
target=x86_64-unknown-linux-gnu
export CARGO_TARGET_DIR="$PWD/target/runtime-asan"
export RUSTFLAGS='-Zsanitizer=address -Cdebuginfo=2 -Cforce-frame-pointers=yes'
export ASAN_OPTIONS='detect_leaks=1:halt_on_error=1'
export RUST_BACKTRACE=1

rustc +"$toolchain" --version

# An explicit target keeps sanitizer flags off host build scripts. Rebuild std
# so allocations and accesses in the standard library are instrumented too.
for test_filter in \
    gc::tests::test_gc_minor_collection \
    gc::tests::test_gc_runtime_root \
    channel::tests::cancellation_tokens_follow_native_core_when_gc_wrappers_move
do
    cargo +"$toolchain" test --locked -Zbuild-std \
        --target "$target" -p willow_runtime --lib "$test_filter" \
        -- --test-threads=1 --nocapture
done
