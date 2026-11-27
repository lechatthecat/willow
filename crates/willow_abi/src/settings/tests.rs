use super::*;
use std::panic::{UnwindSafe, catch_unwind};

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .expect("string panic")
}

fn observe<T: std::fmt::Debug>(read: impl FnOnce() -> T + UnwindSafe) -> String {
    match catch_unwind(read) {
        Ok(value) => format!("{value:?}"),
        Err(payload) => format!("panic: {}", panic_text(payload)),
    }
}

#[test]
fn advisory_counts_cover_values_whitespace_zero_absence_and_overflow() {
    for raw in [
        None,
        Some("0"),
        Some(""),
        Some("bad"),
        Some("-1"),
        Some("18446744073709551616"),
    ] {
        assert_eq!(positive_count(raw), None);
        assert_eq!(preempt_value(raw, 10), 10);
    }
    for raw in ["1", " 1 ", "+1", "\t1\n"] {
        assert_eq!(positive_count(Some(raw)), Some(1));
        assert_eq!(preempt_value(Some(raw), 10), 1);
    }
    assert_eq!(
        positive_count(Some(&usize::MAX.to_string())),
        Some(usize::MAX)
    );
    assert_eq!(preempt_value(Some(&u64::MAX.to_string()), 10), u64::MAX);
}

#[test]
fn bounded_counts_reject_whitespace_and_preserve_inclusive_endpoints() {
    for raw in [
        None,
        Some("0"),
        Some(""),
        Some("bad"),
        Some("-1"),
        Some(" 1 "),
        Some("1001"),
        Some("18446744073709551616"),
    ] {
        assert_eq!(bounded_count(raw, 10, 1000), 10);
    }
    for value in [1, 10, 1000] {
        assert_eq!(bounded_count(Some(&value.to_string()), 10, 1000), value);
    }
    assert_eq!(satb_capacity_value(None), 256);
    assert_eq!(satb_capacity_value(Some("65536")), 65536);
    assert_eq!(satb_capacity_value(Some("65537")), 256);
}

#[test]
fn resource_constraints_preserve_fail_closed_policy_and_panic_text() {
    for (parse, name) in [
        (
            gc_memory_limit_value as fn(Option<&str>) -> Option<usize>,
            "WILLOW_GC_MEMORY_LIMIT",
        ),
        (nursery_bytes_value, "WILLOW_GC_NURSERY_BYTES"),
    ] {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("1")), Some(1));
        assert_eq!(parse(Some(&usize::MAX.to_string())), Some(usize::MAX));
        for raw in ["0", "", "bad", "-1", " 1 ", "18446744073709551616"] {
            assert_eq!(
                observe(|| parse(Some(raw))),
                format!("panic: {name} must be a positive byte count")
            );
        }
    }
    assert_eq!(gc_memory_limit_bytes_value(None), 0);
    assert_eq!(gc_memory_limit_bytes_value(Some("0")), 0);
    assert_eq!(gc_memory_limit_bytes_value(Some("1")), 1);
    assert_eq!(
        gc_memory_limit_bytes_value(Some(&u64::MAX.to_string())),
        u64::MAX
    );
    for raw in ["", "bad", "-1", " 1 ", "18446744073709551616"] {
        // Match the existing parse-int diagnostic, including its error kind.
        let legacy = observe(|| {
            raw.parse::<u64>()
                .expect("WILLOW_GC_MEMORY_LIMIT_BYTES must be a nonnegative byte count")
        });
        assert_eq!(observe(|| gc_memory_limit_bytes_value(Some(raw))), legacy);
    }
}

#[test]
fn markers_allow_zero_saturate_valid_counts_and_default_on_bad_input() {
    for raw in [
        None,
        Some(""),
        Some("bad"),
        Some("-1"),
        Some(" 1 "),
        Some("18446744073709551616"),
    ] {
        assert_eq!(mark_threads_value(raw), None);
    }
    for (value, expected) in [(0, 0), (1, 1), (64, 64), (65, 64), (usize::MAX, 64)] {
        assert_eq!(mark_threads_value(Some(&value.to_string())), Some(expected));
    }
}

#[test]
fn switches_are_exact_predicates_not_boolean_parsers() {
    for raw in [
        None,
        Some(""),
        Some("bad"),
        Some("1"),
        Some(" 0 "),
        Some("false"),
    ] {
        assert!(enabled_unless_zero(raw.map(OsStr::new)));
    }
    assert!(!enabled_unless_zero(Some(OsStr::new("0"))));
    for raw in [
        None,
        Some(""),
        Some("bad"),
        Some("1"),
        Some(" 0 "),
        Some(" false "),
    ] {
        assert!(panic_effects_value(raw));
    }
    for raw in ["0", "false", "FALSE", "FaLsE"] {
        assert!(!panic_effects_value(Some(raw)));
    }
}

#[test]
fn verification_invalid_sentinel_and_handshake_zero_are_preserved() {
    assert_eq!(verification_budget_value(None, 5000), 5000);
    assert_eq!(handshake_timeout_value(None, 120), 120);
    for raw in ["", "bad", "-1", "18446744073709551616"] {
        assert_eq!(verification_budget_value(Some(raw), 5000), 0);
        assert_eq!(handshake_timeout_value(Some(raw), 120), 120);
    }
    for value in [0, 1, u64::MAX] {
        assert_eq!(
            verification_budget_value(Some(&value.to_string()), 5000),
            value
        );
        assert_eq!(
            handshake_timeout_value(Some(&value.to_string()), 120),
            value
        );
    }
    assert_eq!(verification_budget_value(Some(" 1 "), 5000), 0);
    assert_eq!(handshake_timeout_value(Some(" 1 "), 120), 1);
}

struct Probe {
    name: &'static str,
    read: fn() -> String,
    // unset, 7, zero, invalid, one (non-Unicode follows unset except presence switches).
    expected: [String; 5],
}

fn probe(name: &'static str, read: fn() -> String, values: [&str; 5]) -> Probe {
    Probe {
        name,
        read,
        expected: values.map(str::to_owned),
    }
}

fn probes() -> Vec<Probe> {
    let default_markers = std::thread::available_parallelism()
        .map(|count| count.get().saturating_sub(1).clamp(1, 4))
        .unwrap_or(1)
        .to_string();
    let hard = "panic: WILLOW_GC_MEMORY_LIMIT must be a positive byte count";
    let nursery = "panic: WILLOW_GC_NURSERY_BYTES must be a positive byte count";
    let soft = "panic: WILLOW_GC_MEMORY_LIMIT_BYTES must be a nonnegative byte count: ParseIntError { kind: InvalidDigit }";
    vec![
        probe(
            "WILLOW_WORKERS",
            || observe(worker_count),
            ["None", "Some(7)", "None", "None", "Some(1)"],
        ),
        probe(
            "WILLOW_BLOCKING_THREADS",
            || observe(blocking_threads),
            ["None", "Some(7)", "None", "None", "Some(1)"],
        ),
        probe(
            "WILLOW_BLOCKING_QUEUE",
            || observe(blocking_queue),
            ["None", "Some(7)", "None", "None", "Some(1)"],
        ),
        probe(
            "WILLOW_TASK_BUDGET",
            || observe(|| task_budget(u64::MAX)),
            [
                "18446744073709551615",
                "7",
                "18446744073709551615",
                "18446744073709551615",
                "1",
            ],
        ),
        probe(
            "WILLOW_TIME_QUANTUM_MS",
            || observe(|| time_quantum_ms(10)),
            ["10", "7", "10", "10", "1"],
        ),
        probe(
            "WILLOW_GC_REMARK_MAX_SLOTS",
            || observe(remark_max_slots),
            ["4096", "7", "4096", "4096", "1"],
        ),
        probe(
            "WILLOW_GC_REMARK_MAX_BYTES",
            || observe(remark_max_bytes),
            ["1048576", "7", "1048576", "1048576", "1"],
        ),
        probe(
            "WILLOW_GC_REMARK_MAX_WORK_NS",
            || observe(remark_max_work_ns),
            ["1000000", "7", "1000000", "1000000", "1"],
        ),
        probe(
            "WILLOW_GC_SATB_BUFFER_ENTRIES",
            || observe(satb_capacity),
            ["256", "7", "256", "256", "1"],
        ),
        probe(
            "WILLOW_GC_MEMORY_LIMIT",
            || observe(gc_memory_limit),
            ["None", "Some(7)", hard, hard, "Some(1)"],
        ),
        probe(
            "WILLOW_GC_NURSERY_BYTES",
            || observe(nursery_bytes),
            ["None", "Some(7)", nursery, nursery, "Some(1)"],
        ),
        probe(
            "WILLOW_GC_MEMORY_LIMIT_BYTES",
            || observe(gc_memory_limit_bytes),
            ["0", "7", "0", soft, "1"],
        ),
        probe(
            "WILLOW_GC_MARK_THREADS",
            || observe(mark_threads),
            [&default_markers, "7", "0", &default_markers, "1"],
        ),
        probe(
            "WILLOW_GC_CONCURRENT_MARK",
            || observe(concurrent_mark),
            ["true", "true", "false", "true", "true"],
        ),
        probe(
            "WILLOW_GC_ASSIST",
            || observe(gc_assist),
            ["true", "true", "false", "true", "true"],
        ),
        probe(
            "WILLOW_GC_PACER",
            || observe(gc_pacer),
            ["true", "true", "false", "true", "true"],
        ),
        probe(
            "WILLOW_PANIC_SNAPSHOT_REUSE",
            || observe(panic_snapshot_reuse),
            ["true", "true", "false", "true", "true"],
        ),
        probe(
            "WILLOW_PANIC_EFFECTS",
            || observe(panic_effects),
            ["true", "true", "false", "true", "true"],
        ),
        probe(
            "WILLOW_ASYNC_FRAME_ALL",
            || observe(async_frame_all),
            ["false", "true", "true", "true", "true"],
        ),
        probe(
            "WILLOW_QUERY_CACHE",
            || observe(query_cache),
            ["false", "false", "false", "false", "true"],
        ),
        probe(
            "WILLOW_VERIFICATION_TIMEOUT_MS",
            || observe(verification_timeout_ms),
            ["5000", "7", "0", "0", "1"],
        ),
        probe(
            "WILLOW_VERIFICATION_RLIMIT",
            || observe(verification_resource_limit),
            ["1000000", "7", "0", "0", "1"],
        ),
        probe(
            "WILLOW_GC_HANDSHAKE_TIMEOUT_SECS",
            || observe(|| handshake_timeout_seconds(120)),
            ["120", "7", "0", "120", "1"],
        ),
    ]
}

#[cfg(unix)]
fn non_unicode() -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(vec![0xff])
}

#[cfg(windows)]
fn non_unicode() -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    std::ffi::OsString::from_wide(&[0xd800])
}

#[test]
fn environment_routes_every_setting_without_mutating_parent_environment() {
    const CHILD: &str = "WILLOW_SETTINGS_TEST_CHILD";
    if let Ok(case) = std::env::var(CHILD) {
        let case: usize = case.parse().unwrap();
        for probe in probes() {
            assert_eq!(
                (probe.read)(),
                probe.expected[case.min(4)],
                "{} case {case}",
                probe.name
            );
        }
        return;
    }
    // Each case has a fresh process. No set_var races, global locks, or sleeps.
    for (case, value) in [None, Some("7"), Some("0"), Some("invalid"), Some("1")]
        .into_iter()
        .enumerate()
    {
        run_child(CHILD, case, value.map(std::ffi::OsString::from));
    }
    // The OS-string predicates agree with the unset case on malformed Unicode.
    #[cfg(any(unix, windows))]
    run_child(CHILD, 0, Some(non_unicode()));
}

fn run_child(child: &str, case: usize, value: Option<std::ffi::OsString>) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "settings::tests::environment_routes_every_setting_without_mutating_parent_environment",
        ])
        .env(child, case.to_string());
    for probe in probes() {
        command.env_remove(probe.name);
        if let Some(value) = &value {
            command.env(probe.name, value);
        }
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "case {case}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
