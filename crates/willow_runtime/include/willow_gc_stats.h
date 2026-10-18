#ifndef WILLOW_GC_STATS_H
#define WILLOW_GC_STATS_H
#include <stdint.h>

/* Fixed V1 ABI: counters are cumulative and saturating, times are monotonic ns.
 * See docs/gc_telemetry.md for sampling boundaries and memory definitions. */
typedef struct {
    uint64_t count, total_ns, max_ns;
    uint64_t buckets[65];
} WillowGcLatencyV1;
typedef struct {
    uint64_t allocation_count, allocation_bytes, freed_objects, released_bytes;
    uint64_t tlab_fast_allocations, tlab_slow_allocations, tlab_refills;
    uint64_t promoted_objects, promoted_bytes, moved_objects;
    uint64_t barrier_calls, barrier_hits;
} WillowGcCountersV1;
typedef struct {
    uint64_t occupied_bytes, young_occupied_bytes, reserved_bytes, committed_bytes;
    uint64_t old_reserved_bytes, nursery_reserved_bytes, old_regions;
    uint64_t remembered_objects, dirty_cards, major_trigger_bytes, minor_trigger_bytes;
} WillowGcHeapV1;
typedef struct {
    uint64_t epoch;
    uint32_t kind, reserved;
    uint64_t start_ns, end_ns, pause_ns, mark_ns;
    uint64_t marked_bytes, scanned_bytes, root_scan_bytes;
    uint64_t heap_before_bytes, heap_after_bytes, reclaimed_bytes;
} WillowGcCycleV1;
typedef struct {
    uint32_t version, struct_size;
    uint64_t timestamp_ns;
    uint32_t phase, resident_valid;
    uint64_t epoch, reset_generation, process_resident_bytes;
    WillowGcCountersV1 counters;
    WillowGcHeapV1 heap;
    uint64_t minor_cycles, major_cycles, marked_bytes, scanned_bytes, root_scan_bytes, mark_ns;
    WillowGcLatencyV1 pauses, minor_pauses, major_pauses;
    WillowGcCycleV1 last_cycle, last_major_cycle;
    uint64_t trace_errors;
} WillowGcStatsV1;

typedef struct {
    uint64_t metadata_objects, metadata_bytes, root_values, swept_objects;
} WillowGcStopWorkV2;
typedef struct {
    uint64_t sequence, reset_generation;
    uint32_t reason, outcome;
    uint64_t requested_ns, stopped_ns, released_ns;
    WillowGcStopWorkV2 work;
} WillowGcStopEventV2;
typedef struct {
    uint64_t requests, completed, aborted;
    WillowGcLatencyV1 rendezvous, stopped;
    WillowGcStopWorkV2 work;
} WillowGcStopTotalsV2;
typedef struct {
    uint64_t flags, reset_generation, last_sequence, active_sequence;
    uint64_t events_dropped, events_pending;
    WillowGcStopTotalsV2 by_reason[4];
    WillowGcStopEventV2 last_event;
} WillowGcStopStatsV2;
typedef struct {
    uint64_t flags, completed_jobs, wall_ns, cpu_ns, cpu_samples, cpu_unavailable;
    uint64_t marker_panics, spawn_failures, join_timeouts, fallback_cycles, drop_hook_panics;
} WillowGcMarkWorkerStatsV2;
typedef struct {
    uint32_t version, struct_size;
    uint64_t timestamp_ns;
    WillowGcStatsV1 baseline;
    WillowGcStopStatsV2 stops;
    WillowGcMarkWorkerStatsV2 workers;
} WillowGcStatsV2;
typedef struct {
    uint64_t epoch, mark_complete_ns, sweep_complete_ns, quiescent_ns;
    /* 0 = begun; 1 = MarkComplete; 2 = SweepComplete; 3 = Quiescent. */
    uint32_t stage, aborted;
} WillowGcCycleMilestonesV3;
typedef struct {
    uint64_t managed_bytes, external_bytes, old_reserved_bytes, young_reserved_bytes;
    uint64_t pinned_bytes, fragmentation_bytes, pending_sweep_bytes, quarantined_bytes;
    uint64_t reusable_bytes, last_major_retained_bytes;
} WillowGcMemoryV3;
typedef struct {
    uint64_t survivor_copies, survivor_bytes, tenured_objects, tenured_bytes;
    uint64_t pinned_promotions, deferred_minor_collections;
    uint64_t minor_deferral_age_bytes, minor_retry_target_bytes, minor_blockers;
    uint64_t pin_fallback_count, pinned_bytes, pin_reasons[5];
    uint64_t active_pins, active_pin_bytes, released_pins;
    uint64_t survivor_space_reserved, survivor_space_live;
} WillowGcSurvivorsV3;
typedef struct {
    uint64_t frames_created, allocated_frame_bytes;
    uint64_t terminal_events, terminal_frames_reclaimed;
    uint64_t scheduler_roots_added, scheduler_roots_released;
    uint64_t quiescent_events, scope_owner_releases;
    uint64_t metadata_removed_frames, metadata_removed_bytes;
    uint64_t quarantined_frames, quarantined_bytes, pending_drops;
    uint64_t task_frames_reclaimed, reclaimed_frame_bytes, sweep_completions;
    uint64_t quarantined_terminal_frames, task_frames_running, poll_exits, task_frames_rooted, task_frames_retired, task_frames_terminal;
} WillowGcFramesV3;
typedef struct {
    uint64_t root_scan_ns, root_scan_samples, evacuation_ns, evacuation_samples;
    uint64_t resume_ns, resume_samples, mark_cpu_ns, mark_cpu_missing_cycles;
    uint64_t assist_cpu_ns, assist_cpu_missing_samples, assist_samples;
    uint64_t reclaimed_bytes, worker_threads, worker_busy, worker_idle;
} WillowGcMeasurementsV3;
typedef struct {
    uint32_t version, struct_size;
    uint64_t timestamp_ns;
    WillowGcStatsV2 baseline;
    WillowGcCycleMilestonesV3 milestones;
    WillowGcMemoryV3 memory;
    WillowGcSurvivorsV3 survivors;
    WillowGcFramesV3 frames;
    WillowGcMeasurementsV3 measurements;
    uint64_t barrier_variants[13], satb_deletions, barrier_variants_valid;
} WillowGcStatsV3;

/* V4 detailed measurement is opt-in (WILLOW_GC_MEASURE=1 or trace version 4).
 * flags: enabled=1, history loss=2, saturation=4, protocol error=8.
 * A zero interval count without enabled and intact coverage proves nothing. */
typedef struct {
    uint64_t objects, slots, bytes, metadata_bytes, budget_checks, yields, restarts;
} WillowGcWorkV4;
typedef struct {
    uint64_t flags, sequence, dropped, active_intervals, registered, unregistered, progress;
    WillowGcLatencyV1 intervals[9];
    WillowGcWorkV4 work[6];
} WillowGcObservationsV4;
typedef struct {
    uint32_t version, struct_size;
    uint64_t timestamp_ns;
    WillowGcStatsV3 baseline;
    WillowGcObservationsV4 observations;
} WillowGcStatsV4;

#ifdef __cplusplus
extern "C" {
#endif
/* out must point to a full, aligned, writable V1 object. 0 = success, -1 = null. */
int32_t willow_gc_stats_snapshot_v1(WillowGcStatsV1 *out);
/* Required bytes, or -1 for an unsupported version. */
int64_t willow_gc_stats_size(int64_t version);
/* Bytes written; 0 for short/negative length; -1 for unsupported version or
 * null output with sufficient length. Failures leave output untouched.
 * Output need not be aligned; bytes beyond the required size are untouched. */
int64_t willow_gc_stats_snapshot(int64_t version, void *out, int64_t out_len);
#ifdef __cplusplus
}
#endif
#endif
