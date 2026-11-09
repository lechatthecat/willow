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
    "WILLOW_LIR_LOG",
    "WILLOW_VERIFY_DEBUG",
    "WILLOW_PANIC_DEPTH_LOAD_LOG",
    "WILLOW_PANIC_EFFECTS_LOG",
    "WILLOW_QUERY_STATS",
    "WILLOW_QUERY_LOG",
    "WILLOW_QUERY_TRACE",
    "WILLOW_QUERY_CACHE_STATS",
    "WILLOW_RETAIN_PROFILE",
    "WILLOW_INCREMENTAL_VERIFY",
    "WILLOW_KEEP_OBJECT",
    "WILLOW_UNIT_MEMORY_LOG",
];

fn check(case: usize) {
    // Empty and `0` still enable presence-based logging. Arbitrary text does
    // not enable `1` switches. Non-Unicode counts only for OS-presence flags.
    let present = case != 0;
    let unicode = present && case != 10;
    let one = case == 3;
    let nonzero = present && case != 2;
    let flags = [
        (&LIR_LOG, unicode),
        (&VERIFY_DEBUG, unicode),
        (&PANIC_DEPTH_LOAD_LOG, present),
        (&PANIC_EFFECTS_LOG, present),
        (&QUERY_STATS, one),
        (&QUERY_LOG, one),
        (&QUERY_TRACE, one),
        (&QUERY_CACHE_STATS, one),
        (&RETAIN_PROFILE, one),
        (&INCREMENTAL_VERIFY, one),
        (&KEEP_OBJECT, nonzero),
        (&UNIT_MEMORY_LOG, nonzero),
    ];
    for reads in [1, 8, 64, 1024] {
        for (flag, expected) in flags {
            for _ in 0..reads {
                assert_eq!(**flag, expected);
            }
            // The same initialized storage serves all subsequent accesses.
            assert!(std::ptr::eq(
                LazyLock::force(flag),
                LazyLock::get(flag).unwrap()
            ));
        }
    }
}
