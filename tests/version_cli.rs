use std::process::Command;

#[test]
fn version_aliases_report_embedded_identity_without_tools() {
    for alias in ["--version", "-V", "version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .arg(alias)
            .current_dir(std::env::temp_dir())
            .env("PATH", "")
            .env("WILLOW_BUILD_COMMIT", "must-not-override-embedded-identity")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "willow {} (commit {})\n",
                env!("CARGO_PKG_VERSION"),
                env!("WILLOW_BUILD_COMMIT")
            )
        );
    }
}

#[test]
fn version_rejects_extra_arguments_and_has_help() {
    for alias in ["--version", "-V", "version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args([alias, "extra"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("version accepts no arguments"));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["help", "version"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("willow version"));
}
