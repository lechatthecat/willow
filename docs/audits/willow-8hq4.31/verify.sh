#!/usr/bin/env bash
# Run from the repository root; focused cases only, no CI queries.
set -euo pipefail
rtk cargo fmt --all -- --check
for filter in minor_survivor_cost_and_page_index_scale moving_header_ \
  minor_tracing_work_scales_with_graph_and_roots \
  fully_forwarded_chunks_release_without_a_second_header_walk \
  survivor_direct_root_and_budget_fallback_promote_in_place \
  major_collection_traces_and_reclaims_survivor_storage \
  tenured_parent_remembers_younger_cycle_and_shared_child \
  tenure_budget_failure_retains_survivor_chunk_as_pinned \
  young_source_accepts_only_exact_pre_cycle_young_payloads \
  droppable_nursery_chunks_are_walked_and_drop_exactly_once \
  minor_rejects_unpublished_and_overlapping_start_bits_in_unindexed_chunk \
  persistent_survival_skips_intermediate_copy_and_churn_restores_it \
  minor::overlap_tests; do
  rtk cargo test -p willow_runtime --lib "$filter" -- --nocapture
done
rtk cargo test --release -p willow_runtime --lib minor_survivor_cost_and_page_index_scale -- --nocapture
for target in x86_64-unknown-linux-gnu x86_64-pc-windows-msvc \
  aarch64-apple-darwin x86_64-apple-darwin; do
  rtk cargo clippy --workspace --all-targets --target "$target" -- -D warnings
done
