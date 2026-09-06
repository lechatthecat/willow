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

#ifdef __cplusplus
extern "C" {
#endif
/* out must point to a full, aligned, writable V1 object. 0 = success, -1 = null. */
int32_t willow_gc_stats_snapshot_v1(WillowGcStatsV1 *out);
#ifdef __cplusplus
}
#endif
#endif
