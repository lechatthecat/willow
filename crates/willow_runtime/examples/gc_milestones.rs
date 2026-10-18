#![no_main]

use std::time::Duration;
use willow_runtime::gc_telemetry::milestones::{GcStage, request_gc_for_test, wait_gc_for_test};

#[unsafe(no_mangle)]
pub extern "C" fn willow_user_main() {
    let ticket = request_gc_for_test().expect("request did not complete its own cycle");
    for stage in [
        GcStage::MarkComplete,
        GcStage::SweepComplete,
        GcStage::Quiescent,
    ] {
        let event = wait_gc_for_test(ticket, stage, Duration::from_secs(1)).unwrap();
        println!(
            "epoch={} stage={stage:?} mark_ns={} sweep_ns={} quiescent_ns={}",
            event.epoch, event.mark_complete_ns, event.sweep_complete_ns, event.quiescent_ns
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn __willow_static_init() {}
