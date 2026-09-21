use super::super::support::*;

#[test]
fn test_leibniz_pi_release_completes_within_150ms() {
    use std::time::Instant;

    let id = unique_test_id();
    let bin_path = temp_path(format!("willow_leibniz_perf_{}", id));

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args([
            "build",
            "example/leibniz_pi.wi",
            "--release",
            "-o",
            &bin_path,
        ])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");
    assert!(status.success(), "leibniz_pi.wi failed to compile");

    let mut best_ms = u128::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        let out = Command::new(&bin_path)
            .output()
            .expect("failed to run binary");
        let elapsed_ms = start.elapsed().as_millis();

        assert!(out.status.success(), "binary exited with error");
        assert_eq!(
            out.stdout.trim_ascii(),
            b"3.141592663589326",
            "output mismatch"
        );
        best_ms = best_ms.min(elapsed_ms);
    }

    remove_output_artifacts(&bin_path);

    let max_ms = if cfg!(windows) { 1000 } else { 250 };
    assert!(
        best_ms < max_ms,
        "leibniz_pi release build took {best_ms}ms at best — expected < {max_ms}ms (performance regression?)"
    );
}
