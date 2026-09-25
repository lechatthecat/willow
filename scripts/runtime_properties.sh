#!/usr/bin/env bash
# Opt-in bounded runtime properties; compiler fuzzing and ASan have separate runners.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# A nursery-moving test must run without alloc/all (which bypass the nursery).
# Scope environment overrides to subprocesses; preserve the caller's shell.
env -u WILLOW_GC_STRESS cargo test --locked -p willow_runtime --lib \
    stress_region_11_generated_graph_preserves_edges_through_moving_collection \
    -- --ignored --test-threads=1 --nocapture
for mode in minor alloc; do
    WILLOW_GC_STRESS="$mode" cargo test --locked -p willow_runtime --lib \
        stress_region_07_deterministic_random_graph_matches_reachability_model \
        -- --ignored --test-threads=1
done
for filter in channel::tests:: cancellation::tests:: array_null_push future::tests::null_handles; do
    env -u WILLOW_GC_STRESS cargo test --locked -p willow_runtime --lib "$filter" \
        -- --test-threads=1
done

# Execute generated code, including select suspension, GC stress, and recovery
# scope cancellation. Fixtures assert output and cleanup counts themselves.
for filter in async_lir_55_ async_lir_58_ async_lir_85_ async_lir_86_; do
    env -u WILLOW_GC_STRESS cargo test --locked -p willow --test integration "$filter" \
        -- --test-threads=1
done
