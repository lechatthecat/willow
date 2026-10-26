#!/bin/sh
set -eu
# Run from the repository root. Focused checks only.
rtk cargo fmt --all -- --check
rtk cargo test -p willow_runtime monotonic_clock -- --nocapture
rtk cargo test -p willow --test integration monotonic_clock -- --nocapture
rtk cargo test -p willow --lib stdlib_schema::recursive_schema_tests::p21_module_surface_is_complete
rtk cargo test -p willow --lib module::std_registry::tests::registry_resolves_every_schema_entry
for target in x86_64-unknown-linux-gnu x86_64-pc-windows-msvc aarch64-apple-darwin x86_64-apple-darwin; do
    rtk cargo clippy --workspace --all-targets --target "$target" -- -D warnings
done
