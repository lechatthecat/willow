use super::*;

#[test]
fn test_dgwo9_workers_env_enables_data_race_check() {
    let (ok, stderr) = compile_with_compiler_env(NONSYNC_ARG_SRC, &[("WILLOW_WORKERS", "4")]);
    assert!(!ok, "WILLOW_WORKERS>1 should enable Send/Sync checks");
    assert!(stderr.contains("error[E2402]"), "{stderr}");
    assert!(stderr.contains("not `Sync`"), "{stderr}");
}

#[test]
fn test_dgwo9_default_workers_keep_data_race_check() {
    let (ok, stderr) = compile_with_compiler_env(NONSYNC_ARG_SRC, &[]);
    assert!(
        !ok,
        "the default configuration must enforce Send/Sync checks"
    );
    assert!(stderr.contains("error[E2402]"), "{stderr}");
}

#[test]
fn test_dgwo9_workers_one_keeps_checks_on() {
    let (ok, stderr) = compile_with_compiler_env(NONSYNC_ARG_SRC, &[("WILLOW_WORKERS", "1")]);
    assert!(
        !ok,
        "single-worker scheduling must preserve Send/Sync checks"
    );
    assert!(stderr.contains("error[E2402]"), "{stderr}");
}

#[test]
fn test_dgwo9_invalid_workers_fall_back_to_default_checks() {
    let (ok, stderr) =
        compile_with_compiler_env(NONSYNC_ARG_SRC, &[("WILLOW_WORKERS", "not-a-number")]);
    assert!(
        !ok,
        "invalid WILLOW_WORKERS must preserve Send/Sync checks: {stderr}"
    );
    assert!(stderr.contains("error[E2402]"), "{stderr}");
}

#[test]
fn test_dgwo9_single_worker_cannot_disable_capture_or_frame_checks() {
    for source in [NONSYNC_ARG_SRC, NONSEND_ASYNC_FRAME_SRC] {
        let (ok, stderr) = compile_with_compiler_env(
            source,
            &[("WILLOW_WORKERS", "1"), ("WILLOW_DATA_RACE_CHECK", "0")],
        );
        assert!(!ok, "single worker must retain type safety");
        assert!(stderr.contains("error[E2402]"), "{stderr}");
    }
}

#[test]
fn test_dgwo9_async_task_frame_must_be_send_under_workers() {
    let (ok, stderr) =
        compile_with_compiler_env(NONSEND_ASYNC_FRAME_SRC, &[("WILLOW_WORKERS", "4")]);
    assert!(!ok, "non-Send async frame should be rejected");
    assert!(stderr.contains("error[E2402]"), "{stderr}");
    assert!(
        stderr.contains("async task frame is not `Send`"),
        "{stderr}"
    );
    assert!(stderr.contains("fn(i64) -> i64"), "{stderr}");
}

#[test]
fn test_dgwo9_async_task_frame_must_be_send_under_explicit_check() {
    let (ok, stderr) = compile_with_data_race_check(NONSEND_ASYNC_FRAME_SRC);
    assert!(
        !ok,
        "explicit data-race check should reject non-Send frames"
    );
    assert!(
        stderr.contains("async task frame is not `Send`"),
        "{stderr}"
    );
}

#[test]
fn test_dgwo9_async_task_frame_rejected_by_default() {
    let (ok, stderr) = compile_with_compiler_env(NONSEND_ASYNC_FRAME_SRC, &[]);
    assert!(!ok);
    assert!(
        stderr.contains("async task frame is not `Send`"),
        "{stderr}"
    );
}
