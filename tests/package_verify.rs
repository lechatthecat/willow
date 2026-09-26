use std::{fs, path::PathBuf, process::Command};

const MANIFEST: &str = "[project]\nname='library'\nversion='1.0.0'\n[willow]\nmanifest-version=1\n";
struct Project(PathBuf);
impl Project {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = Self(std::env::temp_dir().join(format!(
            "willow-verify-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )));
        p.write("project.toml", MANIFEST);
        p.write(
            "src/value.wi",
            "module value; pub fn value() -> i64 { return 42; }",
        );
        p
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn verify(&self, ok: bool, kind: &str) -> serde_json::Value {
        let mut result = serde_json::Value::Null;
        for format in ["human", "json", "ndjson"] {
            let out = Command::new(env!("CARGO_BIN_EXE_willow"))
                .current_dir(&self.0)
                .args(["package", "verify", "--format", format])
                .output()
                .unwrap();
            let text = String::from_utf8(out.stdout).unwrap();
            assert_eq!(
                out.status.success(),
                ok,
                "{text}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                out.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(!text.contains('\x1b'));
            if format == "human" {
                assert!(
                    text.contains(if ok { "Status: valid" } else { kind }),
                    "{text}"
                );
                assert!(text.contains("security or trust"));
            } else {
                result = serde_json::from_str(&text).unwrap();
                assert_eq!(result["schema"], 1);
                assert_eq!(result["ok"], ok);
                if !ok {
                    assert_eq!(result["error"]["kind"], kind, "{result}");
                }
            }
        }
        result
    }
    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn library_and_dependency_verify_without_main_or_lock() {
    let p = Project::new();
    p.write(
        "project.toml",
        &format!("{MANIFEST}[dependencies]\nhelper={{path='helper'}}\n"),
    );
    p.write("helper/project.toml", MANIFEST);
    p.write("helper/src/helper.wi", "pub fn get() -> i64 { return 1; }");
    p.write(
        "src/value.wi",
        "module value; import helper::helper as h; pub fn value() -> i64 { return h::get(); }",
    );
    p.write(
        "src/nested/mod.wi",
        "module nested; import value; pub fn get() -> i64 { return value::value(); }",
    );
    let report = p.verify(true, "");
    assert_eq!(report["modules"], 2);
    assert_eq!(report["dependencies"], 1);
    assert!(
        report["checks"]
            .as_object()
            .unwrap()
            .values()
            .all(|v| v == true)
    );
    assert!(!p.0.join("project.lock").exists());
    p.write("project.lock", "deliberately invalid lock");
    p.verify(true, "");
    assert_eq!(
        fs::read_to_string(p.0.join("project.lock")).unwrap(),
        "deliberately invalid lock"
    );
}

#[test]
fn manifest_failure_matrix() {
    for (manifest, kind) in [
        (
            "[project]\nname='library'\nversion='1.0.0'\n".to_string(),
            "not_willow_package",
        ),
        (
            MANIFEST.replace("manifest-version=1", "manifest-version=2"),
            "unsupported_manifest_version",
        ),
        (MANIFEST.replace("1.0.0", "bad"), "manifest_invalid"),
        (
            MANIFEST.replace("name='library'", "name=''"),
            "manifest_invalid",
        ),
        (
            format!("{MANIFEST}[dependencies]\na={{path='x',git='y'}}"),
            "manifest_invalid",
        ),
    ] {
        let p = Project::new();
        p.write("project.toml", &manifest);
        p.verify(false, kind);
    }
}

#[test]
fn unreachable_parse_type_and_declaration_errors_are_checked() {
    for (source, kind) in [
        ("pub fn broken( {", "parse_error"),
        ("pub fn bad() -> i64 { return true; }", "type_check_failed"),
        ("module different; pub fn ok() {}", "source_layout_invalid"),
    ] {
        let p = Project::new();
        p.write("src/unreachable.wi", source);
        p.verify(false, kind);
    }
}

#[test]
fn ambiguous_module_layout_is_rejected() {
    let p = Project::new();
    p.write("src/value/mod.wi", "module value;");
    p.verify(false, "source_layout_invalid");
}

#[cfg(unix)]
#[test]
fn escapes_and_directory_cycles_are_rejected() {
    use std::os::unix::fs::symlink;
    let p = Project::new();
    let outside = Project::new();
    symlink(outside.0.join("src/value.wi"), p.0.join("src/escape.wi")).unwrap();
    p.verify(false, "package_path_escape");
    fs::remove_file(p.0.join("src/escape.wi")).unwrap();
    symlink(p.0.join("src"), p.0.join("src/loop")).unwrap();
    p.verify(false, "source_layout_invalid");
    fs::remove_file(p.0.join("src/loop")).unwrap();
    p.write(
        "project.toml",
        &MANIFEST.replace("version='1.0.0'", "version='1.0.0'\nentry='../escape.wi'"),
    );
    p.verify(false, "package_path_escape");
}

#[test]
fn git_head_tag_matches_manifest_version() {
    let p = Project::new();
    p.git(&["init", "-q"]);
    p.verify(true, "");
    p.git(&["add", "project.toml", "src/value.wi"]);
    p.git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "-qm",
        "fixture",
    ]);
    p.git(&["tag", "v1.0.0"]);
    p.verify(true, "");
    p.git(&["tag", "v2.0.0"]);
    p.verify(false, "tag_manifest_version_mismatch");
}

#[test]
fn explicit_path_and_equals_format() {
    let p = Project::new();
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["package", "verify"])
        .arg(&p.0)
        .arg("--format=json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["package"]["name"], "library");
    assert!(!p.0.join("project.lock").exists());
    assert!(!p.0.join("library").exists());
}

#[test]
fn package_import_failure_keeps_stable_kind_through_frontend() {
    let p = Project::new();
    p.write(
        "project.toml",
        &format!("{MANIFEST}[dependencies]\na={{path='helper'}}\n"),
    );
    p.write("helper/project.toml", MANIFEST);
    p.write(
        "helper/src/value.wi",
        "module value; pub fn value() -> i64 { return 1; }",
    );
    // The missing import is inside a loaded module, below verify's synthetic entry.
    p.write(
        "src/value.wi",
        "module value; import a::missing; pub fn value() -> i64 { return 42; }",
    );
    let report = p.verify(false, "package_module_not_found");
    assert!(
        report["error"]["reason"]
            .as_str()
            .unwrap()
            .contains("a::missing")
    );
}

#[test]
fn dependency_module_import_failure_keeps_stable_kind_through_frontend() {
    let p = Project::new();
    p.write(
        "project.toml",
        &format!("{MANIFEST}[dependencies]\na={{path='helper'}}\n"),
    );
    p.write("helper/project.toml", MANIFEST);
    p.write(
        "helper/src/value.wi",
        "module value; import missing; pub fn value() -> i64 { return 1; }",
    );
    p.write(
        "src/value.wi",
        "module value; import a::value; pub fn value() -> i64 { return 42; }",
    );
    p.verify(false, "package_module_not_found");
}
