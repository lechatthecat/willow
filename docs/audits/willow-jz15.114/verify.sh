#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../../.."
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo test --test integration imported_error_identity -- --nocapture
rtk cargo test --test integration try_convert_ -- --nocapture
rtk cargo test --test integration test_e1805 -- --nocapture
rtk cargo test --lib structural_identity_visits_each_node_once -- --nocapture
