//! Gate lookup relocations must disappear, including deferred entry polls.
use super::support::{
    compile_and_collect_panic_events_mode, compile_and_run, compile_and_run_release,
};

#[test]
fn sync_poll_gates_have_no_lookup_calls_and_scale_linearly() {
    for release in [false, true] {
        let mut previous = None;
        for count in [1, 4, 16, 64] {
            let functions: String = (0..count).map(|i| format!(
                "fn work{i}(n: i64) -> i64 {{ let mut x = n; while x > 0 {{ x = x - 1; }} return x; }}\n"
            )).collect();
            let calls: String = (0..count)
                .map(|i| format!("println(work{i}(3));"))
                .collect();
            let source = format!("{functions}fn main() {{ {calls} }}");
            let events = compile_and_collect_panic_events_mode(&source, &[], release);
            for forbidden in ["willow_sync_poll_counter", "willow_gc_stop_flag"] {
                assert!(
                    !events.iter().any(|event| event == forbidden),
                    "{forbidden}"
                );
            }
            let gates = events
                .iter()
                .filter(|event| event.as_str() == "inline_sync_gate")
                .count();
            assert!(gates >= count, "missing loop gates: {gates}");
            if let Some((old_count, old_gates)) = previous {
                assert_eq!(gates - old_gates, count - old_count);
            }
            previous = Some((count, gates));
        }
        let events = compile_and_collect_panic_events_mode(
            include_str!("../../example/sync_poll_gates.wi"),
            &[],
            release,
        );
        assert!(events.iter().any(|event| event == "inline_sync_gate"));
        assert!(
            !events
                .iter()
                .any(|event| event == "willow_sync_poll_counter" || event == "willow_gc_stop_flag")
        );
    }
}

#[test]
fn sync_poll_gates_runnable_recursive_example() {
    for (out, ok) in [
        compile_and_run(include_str!("../../example/sync_poll_gates.wi")),
        compile_and_run_release(include_str!("../../example/sync_poll_gates.wi")),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, "1\n2\n4\n8\n");
    }
}
