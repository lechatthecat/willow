use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
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
    fn snapshot(&self) -> Value {
        let _ = fs::remove_file(self.0.join("snapshot.json"));
        self.result(&["snapshot", "save", "main.wi", "--output", "snapshot.json"]);
        // Decode like consumers do: workspace placeholders expand to paths.
        let mut value =
            serde_json::from_slice(&fs::read(self.0.join("snapshot.json")).unwrap()).unwrap();
        willow_compiler::ai::expand_snapshot_paths(&mut value).unwrap();
        value
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
    let snapshot = f.snapshot();
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
    assert_ne!(f.snapshot()["revision"], snapshot["revision"]);
}
#[test]
fn structured_edit_rejection_reports_source_location() {
    let f = Fixture::new();
    let main = "fn value() -> i64 { return 1; }\nfn main() {\n  let f = value;\n  println(f());\n}";
    fs::write(f.0.join("main.wi"), main).unwrap();
    let snapshot = f.snapshot();
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
        (Some(3), Some(11))
    );
    let start = location["start"].as_u64().unwrap() as usize;
    assert_eq!(
        &main[start..location["end"].as_u64().unwrap() as usize],
        "value"
    );
    assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), main);
}
fn result(reader: &mut impl BufRead) -> Value {
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0, "daemon exited");
        let event: Value = serde_json::from_str(&line).unwrap();
        if event["event"] == "analysis.result" {
            return event["data"].clone();
        }
    }
}
#[test]
fn daemon_refresh_matches_cold_and_reuses_equivalent_queries() {
    let f = Fixture::new();
    let before = f.snapshot();
    let function = before["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "value")
        .unwrap()["id"]
        .clone();
    let mut child = Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(&f.0)
        .args(["daemon", "main.wi"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let ready = result(&mut output);
    assert_eq!(ready["revision"], before["revision"]);
    for id in 0..2 {
        writeln!(input,"{}",json!({"id":id,"operation":"query","request":{"kind":"symbol-info","revision":before["revision"],"function":function}})).unwrap();
        let response = result(&mut output);
        assert!(response.get("error").is_none(), "{response}");
    }
    writeln!(input, "{}", json!({"id":2,"operation":"stats"})).unwrap();
    let stats = result(&mut output);
    assert_eq!(stats["result"]["computations"], 1);
    assert_eq!(stats["result"]["hits"], 1);
    fs::write(
        f.0.join("main.wi"),
        "fn value() -> i64 { return 2; } fn main() { println(value()); }",
    )
    .unwrap();
    writeln!(input, "{}", json!({"id":3,"operation":"refresh"})).unwrap();
    let refreshed = result(&mut output);
    assert!(refreshed.get("error").is_none(), "{refreshed}");
    let cold = f.snapshot();
    assert_eq!(refreshed["revision"], cold["revision"]);
    writeln!(input,"{}",json!({"id":4,"operation":"query","request":{"kind":"symbol-info","revision":cold["revision"],"function":function}})).unwrap();
    let response = result(&mut output);
    let symbol = cold["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == function)
        .unwrap();
    assert_eq!(&response["result"]["result"]["symbol"], symbol);
    writeln!(input, "{}", json!({"id":5,"operation":"shutdown"})).unwrap();
    result(&mut output);
    assert!(child.wait().unwrap().success());
}

#[test]
fn daemon_failed_refresh_preserves_revision_and_bounds_process_lifetime() {
    let f = Fixture::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(&f.0)
        .args(["daemon", "main.wi"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let ready = result(&mut output);
    fs::write(f.0.join("main.wi"), "fn main() { missing(); }").unwrap();
    for id in 1..=32 {
        writeln!(input, "{}", json!({"id":id,"operation":"refresh"})).unwrap();
        let response = result(&mut output);
        assert_eq!(response["revision"], ready["revision"]);
        assert!(response.get("error").is_some(), "{response}");
        if id == 32 {
            assert!(
                response["error"]
                    .as_str()
                    .unwrap()
                    .contains("restart required")
            );
        }
    }
    assert!(child.wait().unwrap().success());
}

#[test]
fn daemon_selectively_rechecks_semantics_and_reports_real_body_counts() {
    let f = Fixture::new();
    fs::write(
        f.0.join("main.wi"),
        "import value; import independent; fn main() { println(value::get()); }",
    )
    .unwrap();
    fs::write(f.0.join("value.wi"), "pub fn get() -> i64 { return 1; }").unwrap();
    fs::write(f.0.join("independent.wi"), "pub fn untouched() {}").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(&f.0)
        .args(["daemon", "main.wi"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    result(&mut output);
    for (id, source, checked, reused) in [
        (1, "pub fn get() -> i64 { return 1; }", 0, 3),
        // A body-only edit keeps `get`'s signature, so `main` stays reused.
        (2, "pub fn get() -> i64 { return 2; }", 1, 2),
        (3, "pub fn get() -> String { return \"value\"; }", 2, 1),
    ] {
        fs::write(f.0.join("value.wi"), source).unwrap();
        writeln!(input, "{}", json!({"id":id,"operation":"refresh"})).unwrap();
        let response = result(&mut output);
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["revision"], f.snapshot()["revision"]);
        writeln!(input, "{}", json!({"id":id+10,"operation":"stats"})).unwrap();
        let stats = result(&mut output);
        assert_eq!(stats["result"]["typechecks"], checked);
        assert_eq!(stats["result"]["reused_typed_bodies"], reused);
        assert!(stats["result"]["retained_artifact_bytes"].as_u64().unwrap() > 0);
    }
    writeln!(input, "{}", json!({"id":100,"operation":"shutdown"})).unwrap();
    result(&mut output);
    assert!(child.wait().unwrap().success());
}
