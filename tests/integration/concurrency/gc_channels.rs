use super::*;

// ── GC-managed channels ───────────────────────────────────────
// Channels are ordinary GC objects now: a loop creating channels no longer
// grows memory for the program's lifetime (registry deleted), and cancel
// deregisters via task-side reverse references in O(parked channels).

#[test]
fn chgc_01_channel_churn_bounded_under_stress() {
    // 5000 short-lived channels under alloc-stress GC: every allocation
    // triggers a collection, so unreachable channels are reclaimed as we go —
    // this would OOM-or-crawl with the old program-lifetime leak + registry.
    let (out, ok) = compile_and_run_gc_stress(
        "fn main() { let mut i = 0; let mut sum = 0; while i < 5000 { let ch = Channel<i64>::new(); ch.send(i); sum = sum + ch.recv(); i = i + 1; } println(sum); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "12497500\n");
}

#[test]
fn chgc_02_live_channels_survive_stress_across_tasks() {
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn produce(ch: Channel<String>, caller: i64) { while !test_task_parked(caller) { await yield(); } ch.send(\"a\" + \"b\"); }\nasync fn main() { let ch = Channel<String>::new(); let p = produce(ch, test_current_task()); let v = ch.recv(); println(v); await p; }",
        &[("WILLOW_GC_STRESS", "alloc")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "ab\n");
}

#[test]
fn chgc_03_cancel_after_channel_churn_is_fast() {
    // Cancellation walks only the task's own registrations — churning 2000
    // channels beforehand must not slow (or break) a subsequent cancel.
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn c(ch: Channel<i64>) -> i64 { return ch.recv(); }\nasync fn main() { let mut i = 0; while i < 2000 { let tmp = Channel<i64>::new(); tmp.send(i); tmp.recv(); i = i + 1; } let ch = Channel<i64>::new(); test_observe_next_task(); let h = c(ch); let id = test_observed_task(); while !test_task_parked(id) { await yield(); } h.cancel(); let result = await h.result(); println(h.is_cancelled()); }",
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}
