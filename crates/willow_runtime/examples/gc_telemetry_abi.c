/* Native V1 ABI smoke test; linked against the runtime's own main(). */
#include "willow_gc_stats.h"
#include <assert.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>

_Static_assert(sizeof(WillowGcStatsV1) == 2112, "V1 must not grow");
_Static_assert(sizeof(WillowGcLatencyV1) == 544, "histogram layout");
_Static_assert(sizeof(WillowGcCycleV1) == 96, "cycle layout");
_Static_assert(offsetof(WillowGcStatsV1, struct_size) == 4, "version header");
_Static_assert(offsetof(WillowGcStatsV1, timestamp_ns) == 8, "timestamp offset");

extern void *willow_alloc(int64_t);
extern void willow_push_root(void **);
extern void willow_pop_root(void);
extern void willow_gc_collect(void);
void __willow_static_init(void) {}
void willow_user_main(void) {
    struct { uint64_t left; WillowGcStatsV1 stats; uint64_t right; } out = { .left = 123, .right = 456 };
    assert(willow_gc_stats_snapshot_v1(NULL) == -1);
    const int64_t size = willow_gc_stats_size(1);
    assert(size == (int64_t)sizeof(WillowGcStatsV1));
    assert(willow_gc_stats_size(2) == -1);
    unsigned char bytes[sizeof(WillowGcStatsV1) + 2];
    memset(bytes, 0xa5, sizeof(bytes));
    assert(willow_gc_stats_snapshot(2, bytes + 1, size) == -1);
    assert(willow_gc_stats_snapshot(1, bytes + 1, size - 1) == 0);
    assert(willow_gc_stats_snapshot(1, bytes + 1, -1) == 0);
    assert(willow_gc_stats_snapshot(1, NULL, size) == -1);
    for (size_t i = 0; i < sizeof(bytes); ++i) assert(bytes[i] == 0xa5);
    void *root = willow_alloc(8);
    willow_push_root(&root);
    willow_gc_collect();
    assert(willow_gc_stats_snapshot_v1(&out.stats) == 0);
    assert(out.left == 123 && out.right == 456);
    assert(out.stats.version == 1 && out.stats.struct_size == sizeof(out.stats));
    assert(out.stats.major_cycles == 1 && out.stats.pauses.count == 1);
    assert(out.stats.counters.allocation_count == 1);
    assert(out.stats.last_cycle.heap_after_bytes > 0);
    assert(out.stats.last_cycle.marked_bytes == out.stats.heap.occupied_bytes);
    uint64_t count = 0;
    for (size_t i = 0; i < 65; ++i) count += out.stats.pauses.buckets[i];
    assert(count == out.stats.pauses.count);
    assert(out.stats.resident_valid == 0 || out.stats.process_resident_bytes > 0);
    willow_pop_root();
    willow_gc_collect();
    assert(willow_gc_stats_snapshot_v1(&out.stats) == 0);
    assert(out.stats.heap.occupied_bytes == 0);
    assert(out.stats.last_major_cycle.heap_after_bytes == 0);
    assert(willow_gc_stats_snapshot(1, bytes + 1, size + 1) == size);
    assert(bytes[0] == 0xa5 && bytes[sizeof(bytes) - 1] == 0xa5);
    memcpy(&out.stats, bytes + 1, sizeof(out.stats));
    assert(out.stats.version == 1 && out.stats.struct_size == size);
    assert(out.stats.major_cycles == 2 && out.stats.heap.occupied_bytes == 0);
    printf("gc_telemetry_abi: version=%u size=%u cycles=%llu errors=%llu\n", out.stats.version, out.stats.struct_size,
           (unsigned long long)out.stats.major_cycles, (unsigned long long)out.stats.trace_errors);
}
