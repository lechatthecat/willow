#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
# Focused semantic-boundary tests, including deterministic scaling measurements.
rtk proxy cargo test --lib semantic_facts_ -- --nocapture --test-threads=1
rtk cargo test --test integration module_checker_tables::every_resolution_kind_answers_together -- --nocapture
rtk cargo run --bin willow -- run example/semantic_facts/main.wi
rtk cargo test -p willow --lib ir::lowered::value::lower::tests::array_reference_owner_is_captured_before_later_argument_cfg -- --exact --nocapture
rtk cargo test -p willow --lib unit_reference_ -- --nocapture
rtk cargo test -p willow --lib nested_reference_inference_checks_each_generic_call_once -- --nocapture
rtk cargo run --bin willow -- run example/semantic_facts/references.wi
