"""Focused assist verification; run from repository root. No full suites or CI."""
import subprocess
from pathlib import Path

FILTERS = [
    "gc::assist::tests", "assist_",
    "large_bitmap_continuations_bound_scan_work_without_rescanning_prefixes",
    "native_reference_arrays_use_bounded_continuations_in_the_real_engine",
    "array_shrink_between_slices_preserves_unscanned_deleted_references",
    "map_growth_during_slices_has_a_finite_cursor_and_preserves_deleted_values",
    "channel_pops_and_appends_do_not_shift_or_extend_the_snapshot",
    "satb_parallel_mutators_retain_deleted_subgraphs_across_phase_flips",
    "satb_registration_owns_buffer_and_unwind_unregister_drains_it",
    "marker_panics_publish_failure_and_retire_worker_collector_and_assist",
    "termination_repeated_restarts_and_empty_assists_preserve_progress",
    "bounded_cancellation_clears_partial_publication_without_touching_active_tlabs",
    "memory_limit_bounds_region_reservations_and_recovers_after_sweep",
    "memory_limit_also_bounds_generated_tlab_reservations",
    "background_marker_snapshot_survives_registry_changes_and_mutator_allocations",
    "concurrent_chain_continuations_bound_queue_publication_and_preserve_tails",
    "concurrent_nonleaf_fanout_batches_claims_without_losing_shared_edges",
    "cpu_budget_thresholds_and_missing_windows",
]
with Path(__file__).with_name("focused-tests.txt").open("w") as output:
    for name in FILTERS:
        command = ["rtk", "proxy", "cargo", "test", "-p", "willow_runtime", name, "--", "--nocapture"]
        print(" ".join(command), file=output, flush=True)
        result = subprocess.run(command, stdout=output, stderr=subprocess.STDOUT, timeout=180)
        print(name, result.returncode, flush=True)
        if result.returncode:
            raise SystemExit(result.returncode)
