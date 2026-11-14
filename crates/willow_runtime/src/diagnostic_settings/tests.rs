use super::*;
use std::ffi::OsString;
use std::process::Command;

const CHILD: &str = "WILLOW_DIAGNOSTIC_SETTINGS_TEST_CASE";

fn cases() -> Vec<Option<OsString>> {
    let mut cases: Vec<_> = [
        None,
        Some(""),
        Some("0"),
        Some("1"),
        Some("false"),
        Some("2"),
        Some("3"),
        Some("4"),
        Some(" alloc, ,relocate,alloc "),
        Some("524289"),
    ]
    .into_iter()
    .map(|value| value.map(OsString::from))
    .collect();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        cases.push(Some(OsString::from_vec(vec![0xff])));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        cases.push(Some(OsString::from_wide(&[0xd800])));
    }
    cases
}

#[test]
fn environment_matrix() {
    if let Ok(case) = std::env::var(CHILD) {
        check(case.parse().unwrap());
        return;
    }
    for (case, value) in cases().into_iter().enumerate() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "diagnostic_settings::tests::environment_matrix",
                "--nocapture",
            ])
            .env(CHILD, case.to_string());
        for name in NAMES {
            if let Some(value) = &value {
                command.env(name, value);
            } else {
                command.env_remove(name);
            }
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "case {case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

const NAMES: &[&str] = &[
    "WILLOW_GC_HANDSHAKE_TIMEOUT_SECS",
    "WILLOW_GC_LOG",
    "WILLOW_GC_PROFILE",
    "WILLOW_GC_VERIFY_BARRIER",
    "WILLOW_GC_VERIFY_REGIONS",
    "WILLOW_GC_NOTIFY",
    "WILLOW_GC_MEASURE",
    "WILLOW_SCHED_TRACE",
    "WILLOW_TASK_TRACE",
    "WILLOW_GC_TRACE_VERSION",
    "WILLOW_GC_STRESS",
    "WILLOW_GC_VERIFY_NO_PIN",
    "WILLOW_GC_TRACE",
    "WILLOW_GC_TRACE_CAPACITY",
    "WILLOW_GC_STOP_TRACE_CAPACITY",
];

fn check(case: usize) {
    let present = case != 0;
    let unicode = present && case != 10;
    let one = case == 3;
    let expected = Presence {
        os: present,
        unicode,
        one,
    };
    let version = match case {
        5 => 2,
        6 => 3,
        7 => 4,
        _ => 1,
    };
    let capacity = match case {
        3 => Some(1),
        5 => Some(2),
        6 => Some(3),
        7 => Some(4),
        _ => None,
    };
    let timeout = match case {
        2 => None,
        3 => Some(1),
        5 => Some(2),
        6 => Some(3),
        7 => Some(4),
        9 => Some(524289),
        _ => Some(120),
    }
    .map(std::time::Duration::from_secs);
    let modes: Vec<&str> = match case {
        0 | 1 | 10 => vec![],
        2 => vec!["0"],
        3 => vec!["1"],
        4 => vec!["false"],
        5 => vec!["2"],
        6 => vec!["3"],
        7 => vec!["4"],
        8 => vec!["alloc", "relocate", "alloc"],
        9 => vec!["524289"],
        _ => unreachable!(),
    };
    for reads in [1, 8, 64, 1024] {
        for _ in 0..reads {
            assert_eq!(
                *crate::runtime::current().diagnostics.gc_handshake_timeout,
                timeout
            );
            assert_eq!(*crate::runtime::current().diagnostics.gc_log, expected);
            assert_eq!(*crate::runtime::current().diagnostics.gc_profile, expected);
            assert_eq!(
                *crate::runtime::current().diagnostics.verify_barrier,
                expected
            );
            assert_eq!(
                *crate::runtime::current().diagnostics.verify_regions,
                unicode
            );
            assert_eq!(*crate::runtime::current().diagnostics.gc_notify, one);
            assert_eq!(*crate::runtime::current().diagnostics.gc_measure, one);
            assert_eq!(*crate::runtime::current().diagnostics.sched_trace, present);
            assert_eq!(*crate::runtime::current().diagnostics.task_trace, present);
            assert_eq!(
                *crate::runtime::current().diagnostics.gc_verify_no_pin,
                present && case != 1 && case != 2
            );
            assert_eq!(
                *crate::runtime::current().diagnostics.gc_trace_version,
                version
            );
            assert_eq!(
                crate::runtime::current().diagnostics.trace_stops(),
                version >= 2
            );
            assert_eq!(
                crate::runtime::current().diagnostics.trace_milestones(),
                version >= 3
            );
            assert_eq!(
                crate::runtime::current().diagnostics.trace_observations(),
                version == 4
            );
            assert_eq!(
                crate::runtime::current().diagnostics.gc_observations(),
                one || version == 4
            );
            assert_eq!(
                *crate::runtime::current().diagnostics.gc_trace_capacity,
                if case == 0 { Some(4096) } else { capacity }
            );
            assert_eq!(
                crate::diagnostic_settings::stop_trace_capacity(),
                if case == 0 { Some(64) } else { capacity }
            );
            assert_eq!(
                *crate::runtime::current().diagnostics.gc_stress_modes,
                modes
            );
        }
        assert!(std::ptr::eq(
            LazyLock::force(&crate::runtime::current().diagnostics.gc_log),
            LazyLock::get(&crate::runtime::current().diagnostics.gc_log).unwrap()
        ));
    }
    assert_eq!(
        gc_trace_path(),
        cases()[case].clone().filter(|v| !v.is_empty())
    );
}

#[test]
fn private_runtime_barrier_diagnostic_sources_are_independent() {
    let mut first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
    let mut second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
    second.diagnostics.verify_barrier = LazyLock::new(|| Presence::parse(Some(OsStr::new("1"))));
    first.diagnostics.verify_barrier = LazyLock::new(|| Presence::parse(None));
    std::thread::scope(|scope| {
        for owner in [&first, &second] {
            scope.spawn(crate::runtime::serving(move || {
                for _ in 0..1024 {
                    let _ = *owner.diagnostics.verify_barrier;
                }
            }));
        }
    });
    assert!(!first.diagnostics.verify_barrier.os);
    assert!(second.diagnostics.verify_barrier.one);
    assert!(LazyLock::get(&first.gc).is_none());
    assert!(LazyLock::get(&second.gc).is_none());
}

#[test]
fn private_runtime_gc_diagnostics_preserve_independent_lazy_sources() {
    let mut first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
    let mut second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
    first.diagnostics.gc_log = LazyLock::new(|| Presence::parse(None));
    first.diagnostics.gc_profile = LazyLock::new(|| Presence::parse(None));
    first.diagnostics.verify_regions = LazyLock::new(|| false);
    second.diagnostics.gc_log = LazyLock::new(|| Presence::parse(Some(OsStr::new("1"))));
    second.diagnostics.gc_profile = LazyLock::new(|| Presence::parse(Some(OsStr::new("1"))));
    second.diagnostics.verify_regions = LazyLock::new(|| true);
    std::thread::scope(|scope| {
        for (owner, enabled) in [(&first, false), (&second, true)] {
            scope.spawn(crate::runtime::serving(move || {
                for _ in 0..1024 {
                    assert_eq!(owner.diagnostics.gc_log.os, enabled);
                    assert_eq!(owner.diagnostics.gc_profile.one, enabled);
                    assert_eq!(*owner.diagnostics.verify_regions, enabled);
                }
                assert!(LazyLock::get(&owner.gc).is_none());
            }));
        }
    });
}

#[test]
fn private_runtime_trace_settings_keep_sources_and_owned_modes_separate() {
    let mut first = RuntimeDiagnostics::new();
    let mut second = RuntimeDiagnostics::new();
    for (owner, source) in [
        (&mut first, (|| false) as fn() -> bool),
        (&mut second, (|| true) as fn() -> bool),
    ] {
        owner.gc_notify = LazyLock::new(source);
        owner.gc_measure = LazyLock::new(source);
        owner.sched_trace = LazyLock::new(source);
        owner.task_trace = LazyLock::new(source);
        owner.gc_verify_no_pin = LazyLock::new(source);
    }
    first.gc_trace_version = LazyLock::new(|| 1);
    second.gc_trace_version = LazyLock::new(|| 4);
    first.gc_stress_modes = LazyLock::new(|| vec!["minor".into()]);
    second.gc_stress_modes = LazyLock::new(|| vec!["major".into()]);
    std::thread::scope(|scope| {
        for (owner, expected) in [(&first, false), (&second, true)] {
            scope.spawn(crate::runtime::serving(move || {
                for _ in 0..64 {
                    for flag in [
                        &owner.gc_notify,
                        &owner.gc_measure,
                        &owner.sched_trace,
                        &owner.task_trace,
                        &owner.gc_verify_no_pin,
                    ] {
                        assert_eq!(**flag, expected);
                    }
                }
            }));
        }
    });
    assert_eq!(*first.gc_trace_version, 1);
    assert_eq!(*second.gc_trace_version, 4);
    assert_eq!(*first.gc_stress_modes, ["minor"]);
    assert_eq!(*second.gc_stress_modes, ["major"]);
}

#[test]
fn private_runtime_derived_trace_flags_follow_their_own_sources() {
    let mut first = RuntimeDiagnostics::new();
    let mut second = RuntimeDiagnostics::new();
    first.gc_trace_version = LazyLock::new(|| 1);
    first.gc_measure = LazyLock::new(|| false);
    second.gc_trace_version = LazyLock::new(|| 4);
    second.gc_measure = LazyLock::new(|| true);
    std::thread::scope(|scope| {
        for (owner, expected) in [(&first, false), (&second, true)] {
            scope.spawn(crate::runtime::serving(move || {
                for _ in 0..1024 {
                    assert_eq!(owner.trace_stops(), expected);
                    assert_eq!(owner.trace_milestones(), expected);
                    assert_eq!(owner.trace_observations(), expected);
                    assert_eq!(owner.gc_observations(), expected);
                }
            }));
        }
    });
    // Derived flags retain their first-use result even if a fixture replaces a source.
    first.gc_trace_version = LazyLock::new(|| 4);
    assert!(!first.trace_stops());
    assert!(second.trace_stops());
}

#[test]
fn private_runtime_history_limits_and_timeout_are_independent() {
    let mut first = RuntimeDiagnostics::new();
    let mut second = RuntimeDiagnostics::new();
    first.gc_trace_capacity = LazyLock::new(|| Some(17));
    first.gc_handshake_timeout = LazyLock::new(|| None);
    second.gc_trace_capacity = LazyLock::new(|| None);
    second.gc_handshake_timeout = LazyLock::new(|| Some(std::time::Duration::from_secs(7)));
    assert!(LazyLock::get(&first.gc_trace_capacity).is_none());
    std::thread::scope(|scope| {
        for (owner, capacity, timeout) in [
            (&first, Some(17), None),
            (&second, None, Some(std::time::Duration::from_secs(7))),
        ] {
            scope.spawn(crate::runtime::serving(move || {
                for _ in 0..1024 {
                    assert_eq!(*owner.gc_trace_capacity, capacity);
                    assert_eq!(*owner.gc_handshake_timeout, timeout);
                }
            }));
        }
    });
    drop(first);
    assert_eq!(
        *second.gc_handshake_timeout,
        Some(std::time::Duration::from_secs(7))
    );
}

#[test]
fn private_stress_overrides_do_not_change_other_runtime_settings() {
    std::thread::scope(|scope| {
        for mode in ["minor", "relocate"] {
            scope.spawn(crate::runtime::serving(move || {
                let first = RuntimeDiagnostics::new();
                let second = RuntimeDiagnostics::new();
                *first.stress_override.lock().unwrap() = Some(vec![mode.into()]);
                *second.stress_override.lock().unwrap() = Some(Vec::new());
                assert!(first.stress_enabled(mode));
                assert!(!second.stress_enabled(mode));
                assert!(!first.stress_enabled("alloc"));
                *first.stress_override.lock().unwrap() = Some(vec!["all".into()]);
                assert!(first.stress_enabled("alloc"));
                assert!(!second.stress_enabled("alloc"));
            }));
        }
    });
}
