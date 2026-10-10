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
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("main.wi"),
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
fn structured_edit_cli_end_to_end() {
    let f = Fixture::new();
    let snapshot = f.symbols("main.wi");
    let function = snapshot["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "value")
        .unwrap()["id"]
        .clone();
    fs::write(f.0.join("request.json"), serde_json::to_vec(&json!({"revision":snapshot["revision"],"operations":[{"kind":"rename","function":function,"name":"answer"},{"kind":"replace-body","function":function,"body":"{ return 42; }"}]})).unwrap()).unwrap();
    let prepared = f.result(&[
        "edit",
        "prepare",
        "--entry",
        "main.wi",
        "--requests",
        "request.json",
    ]);
    let id = prepared["transaction"].as_str().unwrap();
    assert_eq!(prepared["state"], "prepared");
    assert_eq!(
        f.result(&["edit", "preview", "--transaction", id]),
        prepared
    );
    f.result(&["edit", "validate", "--transaction", id]);
    f.result(&["edit", "apply", "--transaction", id]);
    assert_eq!(
        fs::read_to_string(f.0.join("main.wi")).unwrap(),
        "fn answer() -> i64 { return 42; } fn main() { println(answer()); }"
    );
    assert_ne!(f.symbols("main.wi")["revision"], snapshot["revision"]);
}
#[test]
fn structured_edit_rejection_reports_source_location() {
    let f = Fixture::new();
    // Function values are renamed (0501edc); a same-spelled field is outside
    // the proven reference set, so the rename is rejected at that field.
    let main = "class Box { pub value: i64; }\nfn value() -> i64 { return 1; }\nfn main() {\n  let b = new Box(2);\n  println(b.value + value());\n}";
    fs::write(f.0.join("main.wi"), main).unwrap();
    let snapshot = f.symbols("main.wi");
    let function = snapshot["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "value")
        .unwrap()["id"]
        .clone();
    fs::write(f.0.join("request.json"), serde_json::to_vec(&json!({"revision":snapshot["revision"],"operations":[{"kind":"rename","function":function,"name":"answer"}]})).unwrap()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(&f.0)
        .args([
            "edit",
            "prepare",
            "--entry",
            "main.wi",
            "--requests",
            "request.json",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let finished: Value = serde_json::from_str(
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(finished["event"], "request.finished");
    assert_eq!(finished["code"], "WT2002");
    let location = &finished["data"]["location"];
    assert_eq!(location["path"], "main.wi");
    assert_eq!(
        (location["line"].as_u64(), location["column"].as_u64()),
        (Some(1), Some(17))
    );
    let start = location["start"].as_u64().unwrap() as usize;
    assert_eq!(
        &main[start..location["end"].as_u64().unwrap() as usize],
        "value"
    );
    assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), main);
}
