use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new(source: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-protocol-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("main.wi"), source).unwrap();
        Self(path)
    }
    fn run(&self, binary: &str, args: &[&str]) -> Output {
        Command::new(binary)
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn events(output: &Output, code: i32, terminal: &str) -> Vec<Value> {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.ends_with(b"\n"));
    let result: Vec<Value> = String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(result.len() >= 2);
    for (seq, event) in result.iter().enumerate() {
        assert_eq!(event["schema_version"], 1);
        assert_eq!(event["seq"], seq);
        assert_eq!(event["stream_id"], result[0]["stream_id"]);
        assert!(event["stream_id"].as_str().unwrap().len() > 2);
        assert!(event["data"].is_object());
    }
    assert_eq!(result[0]["event"], "request.started");
    let last = result.last().unwrap();
    assert_eq!(last["event"], "request.finished");
    assert_eq!(last["code"], terminal);
    assert_eq!(last["data"]["exit_code"], code);
    assert_eq!(
        last["data"]["status"],
        if code == 0 { "ok" } else { "error" }
    );
    result
}

#[test]
fn repeated_checks_leave_no_artifacts_and_have_unique_streams() {
    let f = Fixture::new("fn main() { println(42); }");
    let mut streams = Vec::new();
    for _ in 0..2 {
        let output = f.run(
            env!("CARGO_BIN_EXE_willow"),
            &[
                "check",
                "main.wi",
                "--format",
                "ndjson",
                "--runtime-lib",
                "absent.a",
            ],
        );
        let values = events(&output, 0, "WT0000");
        assert_eq!(values.len(), 2);
        streams.push(values[0]["stream_id"].clone());
    }
    assert_ne!(streams[0], streams[1]);
    assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
}

#[test]
fn frontend_failures_are_structured_in_check_and_build() {
    for source in [
        "fn main() { ` }",
        "fn main( {}",
        "fn main() { missing(); }",
        "import absent; fn main() {}",
        "pub fn library() {}",
    ] {
        let f = Fixture::new(source);
        for command in ["check", "build"] {
            let output = f.run(
                env!("CARGO_BIN_EXE_willow"),
                &[command, "main.wi", "--format=ndjson"],
            );
            let values = events(&output, 1, "WT2001");
            assert!(values.iter().any(
                |v| v["event"] == "diagnostic" && v["code"].as_str().unwrap().starts_with('E')
            ));
            assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
        }
    }
}

#[test]
fn argument_version_and_io_failures_finish_explicitly() {
    let f = Fixture::new("fn main() {}");
    for args in [
        vec!["check", "main.wi", "--format=ndjson", "--wat"],
        vec!["check", "main.wi", "--format"],
        vec!["check", "main.wi", "--format=xml"],
        vec!["check", "main.wi", "--format=ndjson", "--format=human"],
        vec!["check", "main.wi", "--format=ndjson", "-o", "app"],
        vec!["build", "main.wi", "--format=ndjson", "--emit-hir"],
        vec!["run", "main.wi", "--format=ndjson"],
        vec!["unknown", "--format=ndjson"],
    ] {
        events(&f.run(env!("CARGO_BIN_EXE_willow"), &args), 2, "WT1001");
    }
    events(
        &f.run(
            env!("CARGO_BIN_EXE_willow"),
            &["build", "main.wi", "--protocol-version=99"],
        ),
        2,
        "WT1002",
    );
    events(
        &f.run(
            env!("CARGO_BIN_EXE_willow"),
            &["check", "missing.wi", "--protocol-version=1"],
        ),
        1,
        "WT2002",
    );
    events(
        &f.run(
            env!("CARGO_BIN_EXE_willow"),
            &["check", "absent-project", "--format=ndjson"],
        ),
        1,
        "WT2002",
    );
    assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
}

#[test]
fn imported_locations_and_project_check_reuse_compiler_resolution() {
    let f = Fixture::new("import helper; fn main() {}");
    fs::write(f.0.join("helper.wi"), "pub fn bad() { missing(); }").unwrap();
    let values = events(
        &f.run(
            env!("CARGO_BIN_EXE_willow"),
            &["check", "main.wi", "--format=ndjson"],
        ),
        1,
        "WT2001",
    );
    assert!(
        values
            .iter()
            .filter_map(|v| v["data"]["labels"].as_array())
            .flatten()
            .any(|l| l["path"].as_str().is_some_and(|p| p.ends_with("helper.wi")))
    );
    fs::write(f.0.join("helper.wi"), "pub fn good() {}").unwrap();
    fs::write(
        f.0.join("project.toml"),
        "[project]\nname = \"protocol_test\"\nversion = \"0.1.0\"\nentry = \"main.wi\"\n",
    )
    .unwrap();
    events(
        &f.run(
            env!("CARGO_BIN_EXE_willow"),
            &["check", ".", "--format=ndjson"],
        ),
        0,
        "WT0000",
    );
    assert!(!f.0.join("protocol_test").exists());
}

#[test]
fn native_build_success_and_runtime_failure_use_protocol() {
    let f = Fixture::new("fn main() { println(42); }");
    let output = f.run(
        env!("CARGO_BIN_EXE_willow"),
        &["build", "main.wi", "-o", "app", "--format=ndjson"],
    );
    events(&output, 0, "WT0000");
    let app = Command::new(f.0.join("app")).output().unwrap();
    assert!(app.status.success());
    assert_eq!(String::from_utf8(app.stdout).unwrap().trim(), "42");
    let output = f.run(
        env!("CARGO_BIN_EXE_willow"),
        &[
            "build",
            "main.wi",
            "-o",
            "bad",
            "--runtime-lib=absent.a",
            "--format=ndjson",
        ],
    );
    let values = events(&output, 1, "WT2001");
    assert!(values.iter().any(|v| v["code"] == "E0700"));
    assert!(!f.0.join("bad").exists());
    assert!(!f.0.join("bad.o").exists());
}

#[test]
fn human_check_and_legacy_cli_still_work() {
    let f = Fixture::new("fn main() {}");
    for args in [
        vec!["check", "main.wi"],
        vec!["check", "main.wi", "--format=human"],
    ] {
        let output = f.run(env!("CARGO_BIN_EXE_willow"), &args);
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
    }
    let output = f.run(
        env!("CARGO_BIN_EXE_willow"),
        &["build", "main.wi", "--emit-hir"],
    );
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().contains("main"));
}

#[test]
fn linker_failure_is_structured_and_intermediate_is_removed() {
    let f = Fixture::new("fn main() {}");
    fs::write(f.0.join("empty.a"), b"!<arch>\n").unwrap();
    let output = f.run(
        env!("CARGO_BIN_EXE_willow"),
        &[
            "build",
            "main.wi",
            "-o",
            "bad",
            "--runtime-lib=empty.a",
            "--format=ndjson",
        ],
    );
    let values = events(&output, 1, "WT2001");
    assert!(values.iter().any(|v| v["code"] == "E0700"));
    assert!(!f.0.join("bad.o").exists());
}

#[cfg(unix)]
#[test]
fn linker_stdout_does_not_corrupt_protocol() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("fn main() {}");
    fs::write(f.0.join("empty.a"), b"!<arch>\n").unwrap();
    let shim = f.0.join("cc");
    fs::write(
        &shim,
        "#!/bin/sh\necho linker-stdout\necho linker-stderr >&2\nexit 9\n",
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(&f.0)
        .env("PATH", &f.0)
        .args([
            "build",
            "main.wi",
            "-o",
            "bad",
            "--runtime-lib=empty.a",
            "--format=ndjson",
        ])
        .output()
        .unwrap();
    events(&output, 1, "WT2001");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("linker-stdout") && stderr.contains("linker-stderr"),
        "{stderr}"
    );
}

#[test]
fn backend_symbol_errors_are_delivered_to_the_request() {
    let f = Fixture::new("fn willow_future_helper() {} fn main() {}");
    let values = events(
        &f.run(
            env!("CARGO_BIN_EXE_willow"),
            &["build", "main.wi", "--format=ndjson"],
        ),
        1,
        "WT2001",
    );
    let diagnostic = values.iter().find(|v| v["code"] == "E0705").unwrap();
    assert_eq!(diagnostic["data"]["labels"][0]["path"], "main.wi");
    assert!(
        diagnostic["data"]["labels"][0]["span"]["line"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn imported_backend_warning_has_a_resolved_path_and_does_not_fail_build() {
    let f = Fixture::new("import helper; fn main() {}");
    let mut source = String::from("pub async fn oversized() {\n");
    for i in 0..1020 {
        source.push_str(&format!("let value_{i}: i64 = {i};\n"));
    }
    source.push_str("}\n");
    fs::write(f.0.join("helper.wi"), &source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(&f.0)
        .env("WILLOW_ASYNC_FRAME_ALL", "1")
        .args(["build", "main.wi", "-o", "app", "--format=ndjson"])
        .output()
        .unwrap();
    let values = events(&output, 0, "WT0000");
    let diagnostic = values.iter().find(|v| v["code"] == "W0801").unwrap();
    assert_eq!(diagnostic["data"]["severity"], "warning");
    let label = &diagnostic["data"]["labels"][0];
    assert!(label["path"].as_str().unwrap().ends_with("helper.wi"));
    let start = label["span"]["start"].as_u64().unwrap() as usize;
    let end = label["span"]["end"].as_u64().unwrap() as usize;
    assert!(source.get(start..end).is_some());
}

#[test]
fn backend_emitter_io_errors_propagate_without_panic() {
    use willow_compiler::{
        CompilerOptions, CompilerSession,
        diagnostics::{Diagnostic, DiagnosticEmitter, source_map::SourceLookup},
    };
    struct Broken(usize);
    impl DiagnosticEmitter for Broken {
        fn emit(&mut self, _: &Diagnostic, _: &dyn SourceLookup) -> std::io::Result<()> {
            self.0 += 1;
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }
    for source in ["fn willow_reserved() {} fn main() {}", "fn main() {}"] {
        let f = Fixture::new(source);
        let mut options = CompilerOptions::debug();
        options.target.runtime_lib = Some(f.0.join("absent.a"));
        let src = f.0.join("main.wi");
        let out = f.0.join("app");
        let mut broken = Broken(0);
        let error =
            CompilerSession::new(src.to_str().unwrap(), out.to_str().unwrap(), &options, None)
                .run_with_emitter(&mut broken)
                .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert_eq!(broken.0, 1);
        assert!(!f.0.join("app.o").exists());
    }
}

/// `(code, path, line, cascade)` per diagnostic, after checking that every
/// cascade follows the roots and names an earlier emitted root event.
fn diagnostic_summary(values: &[Value]) -> Vec<(String, String, u64, bool)> {
    let diagnostics: Vec<_> = values
        .iter()
        .filter(|v| v["event"] == "diagnostic")
        .collect();
    let summary: Vec<_> = diagnostics
        .iter()
        .map(|v| {
            (
                v["code"].as_str().unwrap().to_owned(),
                v["data"]["labels"][0]["path"].as_str().unwrap().to_owned(),
                v["data"]["labels"][0]["span"]["line"].as_u64().unwrap(),
                v["data"]["cascade"].as_bool().unwrap(),
            )
        })
        .collect();
    if let Some(first_cascade) = summary.iter().position(|d| d.3) {
        assert!(
            summary[first_cascade..].iter().all(|d| d.3),
            "roots precede cascades: {summary:?}"
        );
    }
    for diagnostic in &diagnostics {
        let root = &diagnostic["data"]["root_cause"];
        if diagnostic["data"]["cascade"] == true {
            let root = values
                .iter()
                .find(|v| v["seq"] == *root)
                .expect("root_cause names an emitted event");
            assert_eq!(root["event"], "diagnostic");
            assert_eq!(root["data"]["cascade"], false);
            assert!(root["seq"].as_u64() < diagnostic["seq"].as_u64());
        } else {
            assert!(root.is_null());
        }
    }
    summary
}

#[test]
fn cascaded_diagnostics_follow_their_roots_and_name_them() {
    let util = "pub fn k() -> i64 { return missing(); }\npub fn j() -> i64 { return true; }";
    let f = Fixture::new(
        "import util;\nfn step(n: i64) -> i64 {\n    if n > 1 { return 1 } else if { return 2; }\n    return 0;\n}\nfn main() { println(step(1) + util::k()); }",
    );
    fs::write(f.0.join("util.wi"), util).unwrap();
    let check = |f: &Fixture| {
        diagnostic_summary(&events(
            &f.run(
                env!("CARGO_BIN_EXE_willow"),
                &["check", "main.wi", "--format=ndjson"],
            ),
            1,
            "WT2001",
        ))
    };
    let has = |summary: &[(String, String, u64, bool)], code: &str, path: &str, cascade: bool| {
        summary
            .iter()
            .any(|d| d.0 == code && d.1.ends_with(path) && d.3 == cascade)
    };
    // A syntax error anywhere in the project stops semantic analysis: only the
    // parser root and its recovery noise (cascaded onto it) are reported.
    let summary = check(&f);
    assert!(has(&summary, "E0101", "main.wi", false), "{summary:?}");
    assert!(has(&summary, "E0102", "main.wi", true), "{summary:?}");
    assert!(
        summary.iter().all(|d| d.0.starts_with("E01")),
        "no semantic diagnostics after a syntax error: {summary:?}"
    );
    // With syntax repaired, semantic errors in the imported file are roots.
    let f = Fixture::new(
        "import util;\nfn step(n: i64) -> i64 {\n    if n > 1 { return 1; } else { return 2; }\n}\nfn main() { println(step(1) + util::k()); }",
    );
    fs::write(f.0.join("util.wi"), util).unwrap();
    let summary = check(&f);
    assert!(has(&summary, "E0350", "util.wi", false), "{summary:?}");
    // Unrelated type error on another line of util.wi stays a root.
    assert!(
        summary
            .iter()
            .any(|d| d.0 == "E0201" && d.1.ends_with("util.wi") && d.2 == 2 && !d.3),
        "{summary:?}"
    );
    // An unresolved call must not manufacture a type mismatch.
    assert!(
        !summary
            .iter()
            .any(|d| d.0 == "E0201" && d.1.ends_with("util.wi") && d.2 == 1),
        "{summary:?}"
    );
    assert!(
        !summary.iter().any(|d| d.1.ends_with("main.wi")),
        "{summary:?}"
    );
}

#[test]
fn diagnostics_without_syntax_errors_are_all_roots() {
    let f = Fixture::new("fn main() { let x: i64 = true; missing(); }");
    let values = events(
        &f.run(
            env!("CARGO_BIN_EXE_willow"),
            &["check", "main.wi", "--format=ndjson"],
        ),
        1,
        "WT2001",
    );
    let diagnostics: Vec<_> = values
        .iter()
        .filter(|v| v["event"] == "diagnostic")
        .collect();
    assert!(!diagnostics.is_empty());
    for diagnostic in diagnostics {
        assert_eq!(diagnostic["data"]["cascade"], false, "{diagnostic}");
        assert!(diagnostic["data"]["root_cause"].is_null());
    }
}

#[test]
fn failed_imports_do_not_manufacture_cross_file_semantic_errors() {
    // Four independent import shapes x five failure shapes. The entry uses
    // symbols that would all disappear if the resolved graph were discarded.
    for shape in ["module", "alias", "item", "transitive"] {
        for failure in [
            "syntax",
            "nested",
            "transitive_syntax",
            "two_files",
            "entry_syntax",
        ] {
            let import = match shape {
                "alias" => "import market as prices;",
                "item" => "import market::price;",
                "transitive" => "import bridge;",
                _ => "import market;",
            };
            let call = match shape {
                "alias" => "prices::price()",
                "item" => "price()",
                "transitive" => "bridge::price()",
                _ => "market::price()",
            };
            let extra = if failure == "entry_syntax" {
                "fn independent() { let x = ; }"
            } else {
                ""
            };
            let f = Fixture::new(&format!(
                "{import}\nimport healthy;\nfn main() {{ println({call} + healthy::value()); }}\n{extra}"
            ));
            fs::write(
                f.0.join("healthy.wi"),
                "pub fn value() -> i64 { return 1; }",
            )
            .unwrap();
            fs::write(
                f.0.join("bridge.wi"),
                "import market; pub fn price() -> i64 { return market::price(); }",
            )
            .unwrap();
            if failure != "transitive_syntax" {
                fs::write(
                    f.0.join("market.wi"),
                    if failure == "nested" {
                        "pub fn price() -> i64 { let y = 0; if true { let broken = ; } return y; }"
                    } else {
                        "pub fn price() -> i64 { let broken = ; return 0; }"
                    },
                )
                .unwrap();
            }
            if failure == "transitive_syntax" {
                fs::write(
                    f.0.join("market.wi"),
                    "import leaf; pub fn price() -> i64 { return leaf::value(); }",
                )
                .unwrap();
                fs::write(
                    f.0.join("leaf.wi"),
                    "pub fn value() -> i64 { let broken = ; return 0; }",
                )
                .unwrap();
            }
            if failure == "two_files" {
                fs::write(
                    f.0.join("healthy.wi"),
                    "pub fn value() -> i64 { let x = ; return 1; }",
                )
                .unwrap();
            }
            let values = events(
                &f.run(
                    env!("CARGO_BIN_EXE_willow"),
                    &["check", "main.wi", "--format=ndjson"],
                ),
                1,
                "WT2001",
            );
            let diagnostics: Vec<_> = values
                .iter()
                .filter(|v| v["event"] == "diagnostic")
                .collect();
            assert!(!diagnostics.is_empty(), "{shape}/{failure}");
            for diagnostic in &diagnostics {
                let code = diagnostic["code"].as_str().unwrap();
                assert!(
                    code.starts_with("E005")
                        || code.starts_with("E010")
                        || code == "E0401"
                        || code.starts_with("E07"),
                    "{shape}/{failure}: unexpected semantic fallout: {diagnostic}"
                );
            }
            // Independent parser errors remain roots, rather than being
            // attributed to whichever imported file happened to fail first.
            for path in match failure {
                "two_files" => vec!["market.wi", "healthy.wi"],
                "entry_syntax" => vec!["market.wi", "main.wi"],
                "transitive_syntax" => vec!["leaf.wi"],
                _ => vec!["market.wi"],
            } {
                assert!(
                    diagnostics.iter().any(|v| {
                        v["data"]["cascade"] == false
                            && v["data"]["labels"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|l| l["path"].as_str().is_some_and(|p| p.ends_with(path)))
                    }),
                    "{shape}/{failure}: missing independent root for {path}: {diagnostics:?}"
                );
            }
            assert!(!f.0.join("main.o").exists());
        }
    }
}

#[test]
fn independent_cross_file_type_errors_remain_roots_with_recovered_imports() {
    let f = Fixture::new(
        "import market; import broken;\nfn main() { let unrelated: i64 = true; println(market::price()); }",
    );
    fs::write(
        f.0.join("market.wi"),
        "pub fn price() -> i64 { return false; }",
    )
    .unwrap();
    // `broken` fails semantically, not syntactically: its declarations are
    // recovered, so the other files are still checked independently.
    fs::write(
        f.0.join("broken.wi"),
        "pub fn recover() -> i64 { let y = 0; y += 1; return y; }",
    )
    .unwrap();
    let check = |f: &Fixture| {
        events(
            &f.run(
                env!("CARGO_BIN_EXE_willow"),
                &["check", "main.wi", "--format=ndjson"],
            ),
            1,
            "WT2001",
        )
    };
    let root_in = |values: &[Value], path: &str, code: Option<&str>| {
        values.iter().any(|v| {
            v["event"] == "diagnostic"
                && code.is_none_or(|code| v["code"] == code)
                && v["data"]["cascade"] == false
                && v["data"]["labels"][0]["path"]
                    .as_str()
                    .is_some_and(|p| p.ends_with(path))
        })
    };
    let values = check(&f);
    for path in ["main.wi", "market.wi"] {
        assert!(
            root_in(&values, path, Some("E0201")),
            "missing independent type error: {path}: {values:?}"
        );
    }
    assert!(
        root_in(&values, "broken.wi", None),
        "missing broken.wi root: {values:?}"
    );
    // A syntax error in `broken` stops semantic analysis project-wide: the
    // parser root is the only diagnostic, not attributed to other files.
    fs::write(
        f.0.join("broken.wi"),
        "pub fn recover() -> i64 { let broken = ; return 0; }",
    )
    .unwrap();
    let values = check(&f);
    let diagnostics: Vec<_> = values
        .iter()
        .filter(|v| v["event"] == "diagnostic")
        .collect();
    assert!(root_in(&values, "broken.wi", Some("E0102")), "{values:?}");
    assert!(
        diagnostics
            .iter()
            .all(|v| v["code"].as_str().unwrap().starts_with("E01")),
        "{diagnostics:?}"
    );
}
