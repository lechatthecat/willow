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
                let fixture = Fixture::new("import helper; fn main() {}");
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
