#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../../.."
rtk proxy cargo test -p willow_runtime recovery_sweep_ -- --nocapture
rtk proxy cargo test -p willow_runtime registry_resume -- --nocapture
rtk proxy cargo test -p willow_runtime bounded_retirement -- --nocapture
rtk proxy cargo test -p willow_runtime concurrent_native_contract_failures -- --nocapture
rtk proxy cargo test -p willow_runtime bounded_cancellation -- --nocapture
rtk proxy cargo test -p willow_runtime satb_closure_wave_cap -- --nocapture
rtk proxy cargo test -p willow_runtime native_retry_exhaustion -- --nocapture
rtk proxy cargo test -p willow_runtime overlap_native_recovery_retry_history -- --nocapture
rtk proxy cargo test -p willow_runtime allocation_resumes_while_automatic_collection_is_tracing -- --nocapture
rtk proxy cargo test -p willow_runtime automatic_request_survives_explicit_collector_election_loss -- --nocapture
