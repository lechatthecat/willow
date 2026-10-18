#include "../../../crates/willow_runtime/include/willow_gc_stats.h"
#include <stddef.h>
#include <stdio.h>
int main(void) {
    printf("m0_abi V1=%zu V2=%zu V3=%zu milestones=%zu memory=%zu survivors=%zu frames=%zu measurements=%zu running=%zu terminal_events=%zu pin_reasons=%zu\n",
           sizeof(WillowGcStatsV1), sizeof(WillowGcStatsV2), sizeof(WillowGcStatsV3),
           offsetof(WillowGcStatsV3, milestones), offsetof(WillowGcStatsV3, memory),
           offsetof(WillowGcStatsV3, survivors), offsetof(WillowGcStatsV3, frames),
           offsetof(WillowGcStatsV3, measurements), offsetof(WillowGcFramesV3, task_frames_running),
           offsetof(WillowGcFramesV3, terminal_events), offsetof(WillowGcSurvivorsV3, pin_reasons));
    return 0;
}
