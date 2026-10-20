#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
rtk cargo fmt --all -- --check
rtk cargo clippy --workspace --all-targets -- -D warnings
rtk cargo test --lib subclass_constructor_ -- --nocapture
rtk cargo test --test integration memberwise_owner -- --nocapture
rtk cargo test --test integration implicit_memberwise -- --nocapture
rtk cargo test --test integration test_new_ctor_21_implicit_inherited_memberwise_constructor
rtk git diff --check
