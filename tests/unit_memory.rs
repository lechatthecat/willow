//! Body ownership stays bounded while the global module/signature graph grows.
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
struct Project(PathBuf);
impl Project {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-memory-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn build(&self, release: bool) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_willowc"));
        command
            .arg("build")
            .arg(self.0.join("main.wi"))
            .arg("-o")
            .arg(self.0.join("app"))
            .env("WILLOW_UNIT_MEMORY_LOG", "1");
        if release {
            command.arg("--release");
        }
        command.output().unwrap()
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn live_ast_checker_declared_and_lir_units_stay_one_for_growing_module_graphs() {
    for count in [1, 12, 40] {
        let project = Project::new();
        for index in 0..count {
            let source = if index == 0 {
                "pub fn value() -> i64 { let add = |x: i64| x + 1; return add(0); }".into()
            } else {
                format!(
                    "import unit{}; pub fn value() -> i64 {{ let add = |x: i64| x + 1; return add(unit{}::value()); }}",
                    index - 1,
                    index - 1
                )
            };
            std::fs::write(project.0.join(format!("unit{index}.wi")), source).unwrap();
        }
        std::fs::write(
            project.0.join("main.wi"),
            format!(
                "import unit{}; fn main() {{ println(unit{}::value()); }}",
                count - 1,
                count - 1
            ),
        )
        .unwrap();
        let output = project.build(count == 12);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(
            stderr.contains("peak_ast=1 peak_checker=1 peak_declared=1 peak_lir=1"),
            "{stderr}"
        );
        let run = Command::new(project.0.join("app")).output().unwrap();
        assert!(run.status.success(), "{:?}", run);
        assert_eq!(String::from_utf8_lossy(&run.stdout), format!("{count}\n"));
    }
}

#[test]
fn lazy_defaults_statics_lambdas_and_later_overrides_run_together() {
    let project = Project::new();
    for file in ["main.wi", "base.wi"] {
        std::fs::copy(
            format!("example/current_unit_memory/{file}"),
            project.0.join(file),
        )
        .unwrap();
    }
    let output = project.build(false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new(project.0.join("app")).output().unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n");
}

#[test]
fn normalized_unit_provenance_preserves_lambdas_before_await_and_builtin_aliases() {
    for module in [false, true] {
        for captures in [false, true] {
            let project = Project::new();
            let callable = if captures { "closure" } else { "fn" };
            let extra = if captures { "extra" } else { "2" };
            let source = format!(
                "import std::collections::Array as Values;\n\
                 fn apply(f: {callable}(i64) -> i64, value: i64) -> i64 {{ return f(value); }}\n\
                 async fn fetch(value: i64) -> i64 {{ await yield(); return value; }}\n\
                 pub async fn run() -> i64 {{\n\
                     let values: Values<i64> = [40];\n\
                     let extra = 2;\n\
                     return apply(|n: i64| n + {extra}, await fetch(values[0]));\n\
                 }}\n"
            );
            if module {
                std::fs::write(project.0.join("worker.wi"), source).unwrap();
                std::fs::write(
                    project.0.join("main.wi"),
                    "import worker; async fn main() { println(await worker::run()); }",
                )
                .unwrap();
            } else {
                std::fs::write(
                    project.0.join("main.wi"),
                    format!("{source} async fn main() {{ println(await run()); }}"),
                )
                .unwrap();
            }
            let output = project.build(captures);
            assert!(
                output.status.success(),
                "module={module}, captures={captures}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let output = Command::new(project.0.join("app")).output().unwrap();
            assert!(
                output.status.success(),
                "module={module}, captures={captures}: {output:?}"
            );
            assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n");
        }
    }
}
