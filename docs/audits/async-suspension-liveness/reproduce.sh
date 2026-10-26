#!/usr/bin/env bash
# Focused ticket evidence only; no full suites, publication or CI access.
set -euo pipefail
cd "$(dirname "$0")/../../.."
rtk cargo fmt --all -- --check
for target in x86_64-unknown-linux-gnu x86_64-pc-windows-msvc aarch64-apple-darwin x86_64-apple-darwin; do
    rtk cargo clippy --workspace --all-targets --target "$target" -- -D warnings
done
rtk cargo clippy -p willow_runtime --all-targets --features borrow-migration-probe -- -D warnings
rtk proxy cargo test -p willow --lib async_retention -- --nocapture
rtk cargo test -p willow --lib semantic::type_checker::send_sync::
rtk cargo test -p willow --lib async_local_occurrences_survive_shared_spans
rtk proxy cargo test -p willow_runtime --lib affinity -- --nocapture
rtk cargo test -p willow_runtime --lib generated_stack_preserves_affinity_roots_and_context_after_moving_collection
rtk cargo test -p willow_abi --lib fingerprint::tests
rtk cargo test -p willow_runtime --lib abi_handshake
rtk cargo test --test integration async_retention
rtk cargo test --test integration task_owned_shadow_roots_forced_native_preemption
rtk cargo test --test integration async_mut_sync_preemption_forced_migration
rtk cargo test --test integration async_borrow_migration_forced_generated_polls
rtk cargo test --test integration guard_and_if_let_bindings_stay_rooted_under_minor_gc_stress
rtk cargo test --test integration lir_defer_scopes_11_the_body_sees_the_captured_value
rtk cargo test -p willow --lib typed_body_computations_scale_with_roots_and_cache_replays_keep_local_tables
