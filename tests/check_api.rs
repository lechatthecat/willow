use std::{
    fs, io,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use willow_compiler::{
    CompilerOptions, check_file,
    diagnostics::{Diagnostic, DiagnosticEmitter, Severity, source_map::SourceLookup},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new(source: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "willow-check-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("main.wi"), source).unwrap();
        Self(directory)
    }

    fn check(&self, emitter: &mut dyn DiagnosticEmitter) -> anyhow::Result<()> {
        let mut options = CompilerOptions::debug();
        options.target.runtime_lib = Some(self.0.join("missing-runtime.a"));
        check_file(self.0.join("main.wi").to_str().unwrap(), &options, emitter)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Collected {
    diagnostics: Vec<Diagnostic>,
    paths: Vec<String>,
}

impl DiagnosticEmitter for Collected {
    fn emit(&mut self, diagnostic: &Diagnostic, sources: &dyn SourceLookup) -> io::Result<()> {
        for label in &diagnostic.labels {
            if label.span.line > 0 {
                let source = sources.get(label.span.file_id).expect("label source");
                assert!(
                    source
                        .source
                        .get(label.span.start..label.span.end)
                        .is_some()
                );
                self.paths.push(source.path.clone());
            }
        }
        self.diagnostics.push(diagnostic.clone());
        Ok(())
    }
}

#[test]
fn check_needs_no_runtime_or_output_artifact() {
    let fixture = Fixture::new("fn main() { println(42); }");
    let mut collected = Collected::default();
    fixture.check(&mut collected).unwrap();
    assert!(collected.diagnostics.is_empty());
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
}

#[test]
fn frontend_failures_reach_the_request_emitter() {
    for source in [
        "fn main() { ` }",
        "fn main( {}",
        "fn main() { missing(); }",
        "import absent; fn main() {}",
        "pub fn library() {}",
    ] {
        let fixture = Fixture::new(source);
        let mut collected = Collected::default();
        assert!(fixture.check(&mut collected).is_err(), "{source}");
        assert!(
            collected
                .diagnostics
                .iter()
                .any(|d| d.severity == Severity::Error)
        );
    }
}

#[test]
fn imported_errors_keep_source_locations() {
    let fixture = Fixture::new("import helper; fn main() {}");
    fs::write(fixture.0.join("helper.wi"), "pub fn bad() { missing(); }").unwrap();
    let mut collected = Collected::default();
    assert!(fixture.check(&mut collected).is_err());
    assert!(collected.paths.iter().any(|p| p.ends_with("helper.wi")));
}

#[test]
fn io_failure_is_returned_without_fabricating_a_language_diagnostic() {
    let fixture = Fixture::new("fn main() {}");
    fs::remove_file(fixture.0.join("main.wi")).unwrap();
    let mut collected = Collected::default();
    let error = fixture.check(&mut collected).unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::NotFound
    );
    assert!(collected.diagnostics.is_empty());
}

#[test]
fn emitter_failure_stops_the_request_and_does_not_leak_into_the_next_one() {
    struct Broken(usize);
    impl DiagnosticEmitter for Broken {
        fn emit(&mut self, _: &Diagnostic, _: &dyn SourceLookup) -> io::Result<()> {
            self.0 += 1;
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }
    for source in ["fn main() { ` }", "fn main( {}", "fn main() { missing(); }"] {
        let fixture = Fixture::new(source);
        let mut broken = Broken(0);
        let error = fixture.check(&mut broken).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(broken.0, 1);
        let mut collected = Collected::default();
        assert!(fixture.check(&mut collected).is_err());
        assert!(!collected.diagnostics.is_empty());
    }
    Fixture::new("fn main() {}")
        .check(&mut Collected::default())
        .unwrap();
}

#[test]
fn ordinary_build_keeps_human_diagnostics() {
    let fixture = Fixture::new("fn main() { missing(); }");
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .arg("build")
        .arg(fixture.0.join("main.wi"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("error[E0350]"));
}

#[cfg(target_os = "linux")]
#[test]
fn human_emitter_returns_stderr_failure() {
    const CHILD: &str = "WILLOW_TEST_FULL_STDERR";
    if std::env::var_os(CHILD).is_some() {
        for source in ["fn main() { ` }", "fn main( {}", "fn main() { missing(); }"] {
            let fixture = Fixture::new(source);
            let error = fixture
                .check(&mut willow_compiler::diagnostics::HumanEmitter)
                .unwrap_err();
            assert_eq!(
                error.downcast_ref::<io::Error>().unwrap().raw_os_error(),
                Some(28)
            );
        }
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "human_emitter_returns_stderr_failure",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stderr(
            fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap(),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn increasing_diagnostic_batches_emit_each_error_once() {
    for n in [16, 64, 256] {
        let body = (0..n)
            .map(|i| format!("missing_{i}();"))
            .collect::<String>();
        for imported in [false, true] {
            let fixture = if imported {
                let fixture = Fixture::new("import helper; fn main() { helper::bad(); }");
                fs::write(
                    fixture.0.join("helper.wi"),
                    format!("pub fn bad() {{ {body} }}"),
                )
                .unwrap();
                fixture
            } else {
                Fixture::new(&format!("fn main() {{ {body} }}"))
            };
            let mut collected = Collected::default();
            assert!(fixture.check(&mut collected).is_err());
            assert_eq!(collected.diagnostics.len(), n);
            assert_eq!(collected.paths.len(), n);
            eprintln!("check-count imported={imported} n={n} emissions={n} source_lookups={n}");
        }
    }
}

#[test]
fn generic_diagnostics_once_per_source_location() {
    let fixture = Fixture::new(
        r#"
class Heap<T> {
    pub value: T;
    pub fn broken(self) { self.missing(); }
}
fn main() {
    let a = new Heap<i64>(1);
    let b = new Heap<bool>(true);
}
"#,
    );
    let mut collected = Collected::default();
    let error = fixture.check(&mut collected).unwrap_err();
    let errors: Vec<_> = collected
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    assert_eq!(errors.len(), 1, "{error}: {errors:#?}");
    assert!(errors[0].message.contains("Heap<T>"));
    assert!(!format!("{errors:#?}").contains("$mono$"));
    assert!(error.to_string().contains("1 errors"), "{error}");
}

#[test]
fn generic_diagnostics_member_matrix() {
    for (body, code, count) in [
        ("self.missing();", "E0502", 1),
        ("let x = self.absent;", "E0502", 1),
        ("self.missing(); self.missing();", "E0502", 2),
        ("let x = new Box<i64>(1);", "E0501", 1),
        ("let x = new Box<T>(self.value);", "E0501", 1),
    ] {
        for imported in [false, true] {
            let declarations = format!(
                "class Box<T> {{ value: T; }} pub class Heap<T> {{ pub value: T; pub fn broken(self) {{ {body} }} }}"
            );
            let calls = "fn main() { let a = new Heap<i64>(1); let b = new Heap<bool>(true); }";
            let fixture = if imported {
                let fixture = Fixture::new(&format!("import helper::Heap; {calls}"));
                fs::write(fixture.0.join("helper.wi"), &declarations).unwrap();
                fixture
            } else {
                Fixture::new(&format!("{declarations} {calls}"))
            };
            // A fresh request must report the diagnostics again.
            for _ in 0..2 {
                let mut collected = Collected::default();
                assert!(fixture.check(&mut collected).is_err());
                let errors: Vec<_> = collected
                    .diagnostics
                    .iter()
                    .filter(|d| d.code.as_str() == code)
                    .collect();
                assert_eq!(
                    errors.len(),
                    count,
                    "{body} imported={imported}: {:#?}",
                    collected.diagnostics
                );
                assert!(!format!("{:?}", collected.diagnostics).contains("$mono$"));
                if code == "E0501" {
                    assert!(
                        errors[0]
                            .helps
                            .iter()
                            .any(|h| h.contains("public `init`") && h.contains("Box<T>`"))
                    );
                }
                if imported {
                    assert!(collected.paths.iter().all(|p| p.ends_with("helper.wi")));
                }
            }
        }
    }
}

#[test]
fn generic_diagnostics_function_body() {
    let fixture =
        Fixture::new("fn bad<T>(value: T) { missing(); } fn main() { bad(1); bad(true); }");
    let mut collected = Collected::default();
    assert!(fixture.check(&mut collected).is_err());
    assert_eq!(
        collected.diagnostics.len(),
        1,
        "{:?}",
        collected.diagnostics
    );
}

#[test]
fn generic_diagnostics_human_output() {
    let fixture = Fixture::new(
        "class Heap<T> { pub value: T; pub fn broken(self) { self.missing(); } } fn main() { let a = new Heap<i64>(1); let b = new Heap<bool>(true); }",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .arg("check")
        .arg(fixture.0.join("main.wi"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("error[E0502]").count(), 1, "{stderr}");
    assert!(stderr.contains("class `Heap<T>`"), "{stderr}");
    assert!(!stderr.contains("$mono$"), "{stderr}");
    assert!(stderr.contains("1 errors"), "{stderr}");
}

#[test]
fn generic_diagnostics_distinct_files_and_calls_survive() {
    let declarations =
        "pub class Heap<T> { pub value: T; pub fn broken(self) { self.missing(); } }";
    let fixture = Fixture::new(
        "import left; import right; fn main() { let a = new left::Heap<i64>(1); let b = new right::Heap<i64>(2); }",
    );
    for (module, parameter) in [("left", "Left"), ("right", "Rite")] {
        fs::write(
            fixture.0.join(format!("{module}.wi")),
            declarations.replace('T', parameter),
        )
        .unwrap();
    }
    let mut collected = Collected::default();
    assert!(fixture.check(&mut collected).is_err());
    assert_eq!(
        collected.diagnostics.len(),
        2,
        "{:?}",
        collected.diagnostics
    );
    assert_ne!(
        collected.diagnostics[0].primary_span().unwrap().file_id,
        collected.diagnostics[1].primary_span().unwrap().file_id
    );
    for spelling in ["Heap<Left>", "Heap<Rite>"] {
        assert!(
            collected
                .diagnostics
                .iter()
                .any(|d| d.message.contains(spelling)),
            "{:?}",
            collected.diagnostics
        );
    }

    let fixture = Fixture::new(
        "class Box<T> { value: T; } fn main() { let a = new Box<i64>(1); let b = new Box<bool>(true); }",
    );
    let mut collected = Collected::default();
    assert!(fixture.check(&mut collected).is_err());
    assert_eq!(
        collected.diagnostics.len(),
        2,
        "{:?}",
        collected.diagnostics
    );
}

#[test]
fn generic_diagnostics_declared_spelling_matrix() {
    for (source, code, message, help, count) in [
        (
            "class Heap<Key, Value> { pub key: Key; pub value: Value; pub fn bad(self) { self.missing(); } } fn main() { let a = new Heap<i64, bool>(1, true); let b = new Heap<String, i64>(\"x\", 2); }",
            "E0502",
            "class `Heap<Key, Value>`",
            "",
            1,
        ),
        (
            "class Heap<Element> { pub value: Element; pub fn bad(self) { self.missing(); } } fn main() {}",
            "E0502",
            "class `Heap<Element>`",
            "",
            1,
        ),
        (
            "class Heap<T> { Heap: T; } fn main() { let a = new Heap<i64>(1); }",
            "E0501",
            "field `Heap` of class `Heap<T>`",
            "on `Heap<T>`",
            1,
        ),
        (
            "class Heap<T> { pub value: T; fn hidden(self) {} } fn main() { new Heap<i64>(1).hidden(); }",
            "E0501",
            "method `hidden` of class `Heap<T>`",
            "pub fn hidden",
            1,
        ),
        (
            "class Heap<T> { pub value: T; pub fn bad(self) { Self::missing(); } } fn main() { let a = new Heap<i64>(1); let b = new Heap<bool>(true); }",
            "E0502",
            "class `Heap<T>`",
            "",
            1,
        ),
        (
            "class Heap<T> { pub value: T; pub fn bad(self) { let x = Self::missing; } } fn main() { let a = new Heap<i64>(1); let b = new Heap<bool>(true); }",
            "E0502",
            "`Heap<T>::missing`",
            "",
            1,
        ),
        (
            "class Heap<T> { pub value: T; pub fn bad(self) { Self::missing = 1; } } fn main() { let a = new Heap<i64>(1); let b = new Heap<bool>(true); }",
            "E0502",
            "`Heap<T>::missing`",
            "",
            1,
        ),
        (
            "class Heap<T> { pub value: T; pub fn bad(self) { self.Hep(); } pub fn Heap(self) {} } fn main() { let a = new Heap<i64>(1); }",
            "E0502",
            "no method `Hep` on class `Heap<T>`",
            "similar name: `Heap`",
            1,
        ),
        (
            "class Leaf { pub value: i64; } class Heap<T> { value: T; } fn main() { let a = new Heap<Leaf>(new Leaf(1)); }",
            "E0501",
            "class `Heap<T>`",
            "on `Heap<T>`",
            1,
        ),
        (
            "class Heap<T> { value: T; } fn main() { let a = new Heap<i64>(1); let b = new Heap<bool>(true); }",
            "E0501",
            "class `Heap<T>`",
            "on `Heap<T>`",
            2,
        ),
        (
            "class Heap { value: i64; } fn marker<T>(value: T) {} fn main() { let a = new Heap(1); }",
            "E0501",
            "class `Heap`",
            "on `Heap`",
            1,
        ),
    ] {
        let fixture = Fixture::new(source);
        let mut collected = Collected::default();
        assert!(fixture.check(&mut collected).is_err());
        let errors: Vec<_> = collected
            .diagnostics
            .iter()
            .filter(|d| d.code.as_str() == code)
            .collect();
        assert_eq!(errors.len(), count, "{source}: {:?}", collected.diagnostics);
        for error in errors {
            assert!(error.message.contains(message), "{source}: {error:?}");
            if !help.is_empty() {
                assert!(
                    error.helps.iter().any(|h| h.contains(help)),
                    "{source}: {error:?}"
                );
            }
            if source.contains("self.Hep()") {
                assert_eq!(error.fix_suggestions[0].replacement, "Heap");
            }
        }
    }
}

#[test]
fn generic_diagnostics_aliased_import_spelling() {
    let fixture = Fixture::new(
        "import helper::Heap as Renamed; fn main() { let a = new Renamed<i64>(1); let b = new Renamed<bool>(true); }",
    );
    fs::write(
        fixture.0.join("helper.wi"),
        "pub class Heap<Element> { pub value: Element; pub fn broken(self) { self.missing(); } }",
    )
    .unwrap();
    let mut collected = Collected::default();
    assert!(fixture.check(&mut collected).is_err());
    assert_eq!(
        collected.diagnostics.len(),
        1,
        "{:?}",
        collected.diagnostics
    );
    assert!(
        collected.diagnostics[0].message.contains("Heap<Element>"),
        "{:?}",
        collected.diagnostics
    );
}

#[test]
fn generic_diagnostics_alias_and_local_names_do_not_confuse_parameters() {
    for construction in ["new Alias<T>(self.value)", "new Alias<i64>(1)"] {
        let fixture = Fixture::new(&format!(
            "import helper::Box as Alias; class Box<Local> {{ pub value: Local; }} class Heap<T> {{ pub value: T; pub fn broken(self) {{ let x = {construction}; }} }} fn main() {{ let a = new Heap<i64>(1); let b = new Heap<bool>(true); }}"
        ));
        fs::write(
            fixture.0.join("helper.wi"),
            "pub class Box<Imported> { value: Imported; }",
        )
        .unwrap();
        let mut collected = Collected::default();
        assert!(fixture.check(&mut collected).is_err());
        assert_eq!(
            collected.diagnostics.len(),
            1,
            "{:?}",
            collected.diagnostics
        );
        let error = &collected.diagnostics[0];
        assert!(error.message.contains("helper::Box<Imported>"), "{error:?}");
        assert!(
            error
                .helps
                .iter()
                .any(|help| help.contains("helper::Box<Imported>")),
            "{error:?}"
        );
    }
}

#[test]
fn generic_diagnostics_constructor_human_output() {
    let fixture = Fixture::new(
        "class Box<Element> { value: Element; } class Heap<T> { pub value: T; pub fn bad(self) { let x = new Box<i64>(1); } } fn main() { let a = new Heap<i64>(1); let b = new Heap<bool>(true); }",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .arg("check")
        .arg(fixture.0.join("main.wi"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert_eq!(stderr.matches("error[E0501]").count(), 1, "{stderr}");
    assert!(stderr.contains("class `Box<Element>`"), "{stderr}");
    assert!(
        stderr.contains("provide a public `init` or factory method on `Box<Element>`"),
        "{stderr}"
    );
    assert!(!stderr.contains("$mono$"), "{stderr}");
}
