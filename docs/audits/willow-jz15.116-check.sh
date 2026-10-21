#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo test --lib result_lambda_context_records_scale_linearly
rtk cargo test --test integration lambda_result_ -- --test-threads=4
rtk cargo run --bin willow -- run example/lambda_result_context.wi
