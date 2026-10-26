#!/usr/bin/env bash
# Run from the Willow checkout root on Linux; full workspace tests are not run.
set -euo pipefail
rtk proxy cargo test -p willow_runtime --release retry_measurement_counts_terminal_success_and_empty -- --nocapture --test-threads=1
rtk proxy cargo test -p willow_runtime --release retry_contention_measurement -- --ignored --nocapture --test-threads=1
rtk proxy cargo test -p willow_runtime --release publication_during_failed_probes_reaches_real_task_polls -- --nocapture --test-threads=1
rtk proxy cargo test -p willow_runtime --release concurrent_publishers_and_stealers_do_not_lose_events -- --nocapture --test-threads=1
rtk proxy cargo test -p willow_runtime --release global_batch_tokens_are_popped_exactly_once_by_concurrent_workers -- --nocapture --test-threads=1
