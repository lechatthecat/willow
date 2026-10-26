#!/usr/bin/env bash
# Focused evidence only; never runs full suites or polls CI.
set -euo pipefail
cd "$(dirname "$0")/../.."
rtk proxy cargo test -p willow --lib default_body_clone_measurements -- --nocapture
rtk cargo build -p willow --bin willow
rtk proxy bash docs/audits/generic-foreign-defaults-counts.sh target/debug/willow
actual=$(rtk proxy target/debug/willow run example/default_body_fanout.wi)
[[ "$actual" == "792" ]] || { printf 'Unexpected output: %s\n' "$actual" >&2; exit 1; }
printf 'sixteen_receiver_output=%s\n' "$actual"
