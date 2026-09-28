use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-v34-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/main.wi"),
            "fn value() -> i64 { return 1; } fn main() { println(value()); }",
        )
        .unwrap();
        Self(root)
    }
    fn result(&self, args: &[&str]) -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|v| v["event"] == "analysis.result")
            .unwrap()["data"]
            .clone()
    }
    fn symbols(&self, source: &str) -> Value {
        fs::write(
            self.0.join("symbols-query.json"),
            r#"[{"kind":"symbols","symbol_kind":"function"}]"#,
        )
        .unwrap();
        let result = self.result(&["query", source, "--requests", "symbols-query.json"]);
        json!({"revision": result["revision"], "functions": result["results"][0]["result"]["symbols"]})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn prepare_documents_and_preserves_query_analysis_mode() {
    let f = Fixture::new();
    fs::write(f.0.join("project.toml"), "[willow]\nmanifest-version = 1\n[project]\nname = \"app\"\nversion = \"0.1.0\"\nentry = \"src/main.wi\"\n[dependencies]\n").unwrap();
    let help = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["edit", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("[--project]"));
    for project in [false, true] {
        let snapshot = if project {
            f.symbols(".")
        } else {
            f.symbols("src/main.wi")
        };
        let function = snapshot["functions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "value")
            .unwrap()["id"]
            .clone();
        fs::write(
            f.0.join("request.json"),
            serde_json::to_vec(&json!({
                "revision": snapshot["revision"],
                "operations": [{"kind":"rename", "function":function, "name":"answer"}]
            }))
            .unwrap(),
        )
        .unwrap();
        let before = fs::read(f.0.join("src/main.wi")).unwrap();
        let mut args = vec![
            "edit",
            "prepare",
            "--entry",
            "src/main.wi",
            "--requests",
            "request.json",
        ];
        if !project {
            args.push("--project");
        }
        let rejected = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&f.0)
            .args(&args)
            .output()
            .unwrap();
        assert!(!rejected.status.success());
        let output = String::from_utf8_lossy(&rejected.stdout);
        assert!(output.contains("base revision mismatch"), "{output}");
        assert!(output.contains("use --project"), "{output}");
        assert!(output.contains("omit --project"), "{output}");
        assert!(
            output.contains("query the current source again"),
            "{output}"
        );
        assert_eq!(fs::read(f.0.join("src/main.wi")).unwrap(), before);
        if !project {
            args.pop();
        } else {
            args.push("--project");
        }
        assert_eq!(f.result(&args)["state"], "prepared");
        assert_eq!(fs::read(f.0.join("src/main.wi")).unwrap(), before);
    }
}
