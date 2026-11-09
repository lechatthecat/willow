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
            assert_eq!(*GC_HANDSHAKE_TIMEOUT, timeout);
            assert_eq!(*GC_LOG, expected);
            assert_eq!(*GC_PROFILE, expected);
            assert_eq!(*GC_VERIFY_BARRIER, expected);
            assert_eq!(*GC_VERIFY_REGIONS, unicode);
            assert_eq!(*GC_NOTIFY, one);
            assert_eq!(*GC_MEASURE, one);
            assert_eq!(*SCHED_TRACE, present);
            assert_eq!(*TASK_TRACE, present);
            assert_eq!(*GC_VERIFY_NO_PIN, present && case != 1 && case != 2);
            assert_eq!(*GC_TRACE_VERSION, version);
            assert_eq!(*TRACE_STOPS, version >= 2);
            assert_eq!(*TRACE_MILESTONES, version >= 3);
            assert_eq!(*TRACE_OBSERVATIONS, version == 4);
            assert_eq!(*GC_OBSERVATIONS, one || version == 4);
            assert_eq!(
                *GC_TRACE_CAPACITY,
                if case == 0 { Some(4096) } else { capacity }
            );
            assert_eq!(
                *GC_STOP_TRACE_CAPACITY,
                if case == 0 { Some(64) } else { capacity }
            );
            assert_eq!(*GC_STRESS_MODES, modes);
        }
        assert!(std::ptr::eq(
            LazyLock::force(&GC_LOG),
            LazyLock::get(&GC_LOG).unwrap()
        ));
    }
    assert_eq!(
        gc_trace_path(),
        cases()[case].clone().filter(|v| !v.is_empty())
    );
}
