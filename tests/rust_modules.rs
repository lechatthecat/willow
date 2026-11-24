//! Native Rust module discovery, diagnostics, queries and executable ABI coverage.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture {
    temp: PathBuf,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = std::env::temp_dir().join(format!(
            "willow-native-modules-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let root = temp.join("project with spaces");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(temp.join("home")).unwrap();
        fs::write(
            root.join("project.toml"),
            "[project]\nname = \"rust_modules\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        Self { temp, root }
    }
    fn write(&self, path: &str, source: &str) {
        fs::create_dir_all(self.root.join(path).parent().unwrap()).unwrap();
        fs::write(self.root.join(path), source).unwrap();
    }
    fn local_dependency(&self) {
        self.write(
            "dependency/Cargo.toml",
            "[package]\nname=\"native_probe\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[workspace]\n",
        );
        self.write("dependency/src/lib.rs", "pub fn value() -> i64 { 1 }");
    }
    fn command(&self, args: &[&str]) -> Command {
        let original_home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_willow"));
        command
            .current_dir(&self.root)
            .args(args)
            .env("HOME", self.temp.join("home"))
            .env("USERPROFILE", self.temp.join("home"))
            .env(
                "RUSTUP_HOME",
                std::env::var_os("RUSTUP_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(&original_home).join(".rustup")),
            )
            .env(
                "CARGO_HOME",
                std::env::var_os("CARGO_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(&original_home).join(".cargo")),
            );
        command
    }
    fn success(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        output
    }
    fn without_toolchain(&self, args: &[&str]) -> Output {
        let tools = self.temp.join("unavailable-tools");
        fs::create_dir_all(&tools).unwrap();
        #[cfg(unix)]
        for name in ["cargo", "rustc"] {
            use std::os::unix::fs::PermissionsExt;
            let path = tools.join(name);
            fs::write(
                &path,
                "#!/bin/sh\nprintf invoked >> \"$WILLOW_TOOLCHAIN_MARKER\"\nexit 97\n",
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(windows)]
        for name in ["cargo.cmd", "rustc.cmd"] {
            fs::write(
                tools.join(name),
                "@echo invoked >> %WILLOW_TOOLCHAIN_MARKER%\r\n@exit /b 97\r\n",
            )
            .unwrap();
        }
        let marker = self.temp.join("toolchain-invoked");
        let output = self
            .command(args)
            .env("PATH", tools)
            .env("WILLOW_TOOLCHAIN_MARKER", &marker)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!marker.exists(), "Rust toolchain was invoked");
        output
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp);
    }
}

#[test]
fn native_module_check_and_query_need_no_rust_toolchain() {
    let fixture = Fixture::new();
    fixture.write(
        "src/main.wi",
        "import native; fn main() { println(native::answer()); }",
    );
    fixture.write("src/native.rs", "pub fn answer() -> i64 { 42 }");
    fixture.without_toolchain(&["check", ".", "--offline"]);
    let query = fixture.without_toolchain(&[
        "query",
        "rust-bridge",
        "--project-dir",
        ".",
        "--format",
        "json",
    ]);
    let value: serde_json::Value = serde_json::from_slice(&query.stdout).unwrap();
    let bridges = value["bridges"].as_array().unwrap();
    assert_eq!(bridges.len(), 1, "{value}");
    assert!(
        bridges[0]["symbol"]
            .as_str()
            .unwrap()
            .ends_with("native::answer")
    );
    assert_eq!(bridges[0]["signature"]["output"], "I64");
    assert!(
        bridges[0]["declaration_text"]
            .as_str()
            .unwrap()
            .contains("fn answer")
    );
    assert!(!fixture.root.join(".willow/rust/Cargo.lock").exists());
}

#[test]
fn rust_free_project_checks_without_a_rust_toolchain() {
    let fixture = Fixture::new();
    fixture.write("src/main.wi", "fn main() { println(42); }");
    fixture.without_toolchain(&["check", ".", "--offline"]);
}

#[test]
fn module_collision_and_rust_errors_report_original_source() {
    let fixture = Fixture::new();
    fixture.write("src/main.wi", "import native; fn main() {}");
    fixture.write("src/native.rs", "pub fn answer() -> i64 { 42 }");
    fixture.write("src/native.wi", "pub fn answer() -> i64 { return 42; }");
    let collision = fixture
        .command(&["check", ".", "--offline"])
        .output()
        .unwrap();
    assert!(!collision.status.success());
    let diagnostic = String::from_utf8_lossy(&collision.stderr);
    assert!(
        diagnostic.contains("native.rs") && diagnostic.contains("native.wi"),
        "{diagnostic}"
    );
    fs::remove_file(fixture.root.join("src/native.wi")).unwrap();
    for source in [
        "// header\npub fn broken( {",
        "// header\npub fn broken(v: usize) {}",
        "// header\npub fn broken<T>(v: T) {}",
        "// header\npub fn broken(v: impl Send) {}",
    ] {
        fixture.write("src/native.rs", source);
        let output = fixture
            .command(&["check", ".", "--offline"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains("native.rs") && diagnostic.contains(":2:"),
            "{diagnostic}"
        );
    }
}

const NATIVE_API: &str = r#"
mod helper;
pub fn answer() -> i64 { 42 }
pub fn relative_sources() -> String { format!("{}:{}", helper::answer(), include_str!("label.txt")) }
pub fn signed_byte(value: i8) -> i8 { value }
pub fn signed_short(value: i16) -> i16 { value }
pub fn signed_word(value: i32) -> i32 { value }
pub fn unsigned_byte(value: u8) -> u8 { value }
pub fn unsigned_short(value: u16) -> u16 { value }
pub fn unsigned_word(value: u32) -> u32 { value }
pub fn unsigned_long(value: u64) -> u64 { value }
pub fn character(value: char) -> char { value }
pub fn float(value: f64) -> f64 { value }
pub fn boolean(value: bool) -> bool { value }
pub fn text(value: String) -> String { value }
pub fn borrowed(value: &str) -> String { value.into() }
pub fn bytes(value: &[u8]) -> Vec<u8> { value.to_vec() }
pub fn integers(value: Vec<i64>) -> Vec<i64> { value }
pub fn strings(value: Vec<String>) -> Vec<String> { value }
pub fn floats(value: Vec<f64>) -> Vec<f64> { value }
pub fn booleans(value: Vec<bool>) -> Vec<bool> { value }
pub fn optional(value: Option<String>) -> Option<String> { value }
pub fn result(value: Result<i64, String>) -> Result<i64, String> { value }
pub struct Packet { pub leaf: Leaf, pub label: String, pub data: Vec<u8> }
pub struct Leaf { pub value: i64 }
pub struct TextRecord { pub leaf: Leaf, pub label: String }
pub fn packet(value: Packet) -> Packet { value }
pub struct Handle { value: i64 }
pub struct Wrapped { pub handle: Option<Box<Handle>> }
pub fn wrapped() -> Wrapped { Wrapped { handle: Some(Box::new(Handle { value: 99 })) } }
pub fn make() -> Box<Handle> { Box::new(Handle { value: 42 }) }
pub fn read(value: &Handle) -> i64 { value.value }
pub async fn async_text(value: String) -> String { value }
pub async fn async_borrowed(value: &str) -> String { value.into() }
pub async fn async_bytes(value: &[u8]) -> Vec<u8> { value.to_vec() }
pub async fn async_owned_bytes(value: Vec<u8>) -> Vec<u8> { value }
pub async fn async_strings(value: Vec<String>) -> Vec<String> { value }
pub async fn async_borrowed_strings(value: Vec<&str>) -> Vec<String> { value.into_iter().map(str::to_owned).collect() }
pub async fn async_optional(value: Option<String>) -> Option<String> { value }
pub async fn async_result(value: Result<i64, String>) -> Result<i64, String> { value }
pub async fn async_make() -> Box<Handle> { Box::new(Handle { value: 42 }) }
pub async fn async_unit() {}
pub async fn async_packet(value: Leaf) -> Leaf { value }
pub async fn async_make_packet() -> Packet { Packet { leaf: Leaf { value: 77 }, label: "async record".into(), data: vec![255] } }
pub async fn async_read(value: &Handle) -> i64 { value.value }
pub async fn async_optional_handle(value: Option<&Handle>) -> i64 { value.map_or(0, |handle| handle.value) }
pub async fn async_either(value: Result<String, &Handle>) -> i64 { match value { Ok(text) => text.len() as i64, Err(handle) => handle.value } }
pub async fn async_record(value: TextRecord) -> TextRecord { value }
pub fn unit() {}
fn hidden<T>(value: T) -> T { value }
pub(crate) fn internal(value: usize) -> usize { value }
"#;

const SYNC_MAIN: &str = r#"
import native;
import native::Packet;
import native::Leaf;
import native::answer as native_answer;
import other::answer as other_answer;
fn main() {
    println(native::answer() == 42);
    println(native::relative_sources() == "7:source-relative");
    println(native::signed_byte(-128) == -128);
    println(native::signed_short(-32768) == -32768);
    println(native::signed_word(-2147483648) == -2147483648);
    println(native::unsigned_byte(255) == 255);
    println(native::unsigned_short(65535) == 65535);
    println(native::unsigned_word(4294967295) == 4294967295);
    println(native::unsigned_long(18446744073709551615) == 18446744073709551615);
    println(native::character('🌲') == '🌲');
    println(native::float(2.5) == 2.5);
    println(native::boolean(true));
    println(other_answer() == 7);
    println(other::leaf().value == 9);
    println(native::text("日本語🙂") == "日本語🙂");
    println(native::borrowed("") == "");
    println(native::bytes([0,128,255])[2] == 255);
    println(native::integers([-42, 42])[0] == -42);
    println(native::strings(["a", "b"])[1] == "b");
    println(native::floats([-1.25, 2.5])[0] == -1.25);
    println(native::booleans([false, true])[1]);
    println(native::optional(Some("ok")).unwrap() == "ok");
    println(native::optional(None).is_none());
    println(native::result(Ok(42)).unwrap() == 42);
    println(native::result(Err("error")).unwrap_err() == "error");
    let p = new Packet(new Leaf(42), "record", [128,255]);
    let q = native::packet(p);
    println(q.leaf.value == 42);
    println(q.label == "record");
    println(q.data[1] == 255);
    let handle = native::make();
    println(native::read(handle) == 42);
    native::Handle_close(handle);
    let wrapped = native::wrapped();
    let wrapped_handle = wrapped.handle.unwrap();
    println(native::read(wrapped_handle) == 99);
    native::Handle_close(wrapped_handle);
    native::unit();
    let indirect = native_answer;
    println(indirect() == 42);
}
"#;

const ASYNC_MAIN: &str = r#"
import native;
import native::Leaf;
import native::TextRecord;
async fn main() {
    println(await native::async_text("async") == "async");
    println(await native::async_borrowed("borrowed") == "borrowed");
    let bytes = await native::async_bytes([0,128,255]);
    println(bytes[2] == 255);
    let owned_bytes = await native::async_owned_bytes([128,255]);
    println(owned_bytes[0] == 128);
    let strings = await native::async_strings(["a","b"]);
    println(strings[1] == "b");
    let borrowed_strings = await native::async_borrowed_strings(["borrowed","text"]);
    println(borrowed_strings[1] == "text");
    println((await native::async_optional(Some("ok"))).unwrap() == "ok");
    println((await native::async_optional(None)).is_none());
    println((await native::async_result(Ok(42))).unwrap() == 42);
    println((await native::async_result(Err("error"))).unwrap_err() == "error");
    let handle = await native::async_make();
    println(await native::async_read(handle) == 42);
    println(await native::async_optional_handle(Some(handle)) == 42);
    println(await native::async_optional_handle(None) == 0);
    println(await native::async_either(Ok("abc")) == 3);
    println(await native::async_either(Err(handle)) == 42);
    let a = await native::async_packet(new Leaf(42));
    println(a.value == 42);
    let nested = await native::async_record(new TextRecord(new Leaf(9), "nested"));
    println(nested.leaf.value == 9 && nested.label == "nested");
    let produced = await native::async_make_packet();
    println(produced.leaf.value == 77 && produced.label == "async record" && produced.data[0] == 255);
    await native::async_unit();
}
"#;

#[test]
fn native_modules_execute_all_bridge_shapes_and_share_a_locked_crate() {
    let fixture = Fixture::new();
    fixture.write("src/native.rs", NATIVE_API);
    fixture.write("src/helper.rs", "pub fn answer() -> i64 { 7 }");
    fixture.write("src/label.txt", "source-relative");
    fixture.write("src/other.rs", "pub struct Leaf { pub value: i64 } pub fn answer() -> i64 { 7 } pub fn leaf() -> Leaf { Leaf { value: 9 } }");
    fixture.write("src/main.wi", SYNC_MAIN);
    let run = fixture.success(&["run", ".", "--offline"]);
    assert_eq!(String::from_utf8_lossy(&run.stdout), "true\n".repeat(31));
    let cargo_lock = fs::read(fixture.root.join(".willow/rust/Cargo.lock")).unwrap();
    let project_lock = fs::read(fixture.root.join("project.lock")).unwrap();
    let repeated = fixture.success(&["run", ".", "--frozen"]);
    assert_eq!(repeated.stdout, run.stdout);
    assert_eq!(
        fs::read(fixture.root.join(".willow/rust/Cargo.lock")).unwrap(),
        cargo_lock
    );
    assert_eq!(
        fs::read(fixture.root.join("project.lock")).unwrap(),
        project_lock
    );
    for operation in ["check", "metadata"] {
        let output = fixture.success(&["rust", operation, ".", "--offline", "--format", "json"]);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["enabled"], true, "{value}");
        assert_eq!(value["ok"], true, "{value}");
        assert_eq!(value["kind"], format!("rust.{operation}"));
        assert!(value["toolchain"]["cargo"].is_string(), "{value}");
        assert!(value["direct_dependencies"].as_array().unwrap().is_empty());
    }
    let query = fixture.without_toolchain(&[
        "query",
        "rust-bridge",
        "--project-dir",
        ".",
        "--format",
        "json",
    ]);
    let value: serde_json::Value = serde_json::from_slice(&query.stdout).unwrap();
    let bridges = value["bridges"].as_array().unwrap();
    for (name, kind, expected) in [
        ("native::packet", "Record", "Packet"),
        ("native::make", "Opaque", "Handle"),
    ] {
        let bridge = bridges
            .iter()
            .find(|b| b["symbol"].as_str().unwrap().ends_with(name))
            .unwrap();
        let output = bridge["signature"]["output"][kind].as_str().unwrap();
        assert!(
            output.ends_with(expected) && !output.contains("native_"),
            "{bridge}"
        );
    }
    // Unreviewed synchronous foreign calls remain forbidden from tasks. This
    // entry exercises asynchronous exports without weakening that policy.
    fixture.write("src/main.wi", ASYNC_MAIN);
    let asynchronous = fixture.success(&["run", ".", "--frozen"]);
    assert_eq!(
        String::from_utf8_lossy(&asynchronous.stdout),
        "true\n".repeat(18)
    );
}

#[test]
fn native_and_legacy_bridges_coexist_without_redeclarations() {
    let fixture = Fixture::new();
    fixture.write(
        "project.toml",
        "[project]\nname = \"coexist\"\nversion = \"0.1.0\"\n[rust]\nbridge = \"bridge.rs\"\n[rust-dependencies]\nnative_probe = { path = \"dependency\" }\n",
    );
    fixture.local_dependency();
    fixture.write("bridge.rs", "pub fn legacy() -> i64 { 7 }");
    fixture.write("src/native.rs", "pub fn answer() -> i64 { 35 }");
    fixture.write("src/main.wi", "import native; extern rust { fn legacy() -> i64; } fn main() { println(native::answer() + legacy()); }");
    let run = fixture.success(&["run", ".", "--offline"]);
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");
    let lock = fs::read(fixture.root.join("project.lock")).unwrap();
    for operation in ["metadata", "tree"] {
        let output = fixture.success(&["rust", operation, "--frozen", "--format=json"]);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["enabled"], true, "{value}");
        assert_eq!(fs::read(fixture.root.join("project.lock")).unwrap(), lock);
    }
}

#[test]
fn native_module_aliases_share_an_interface() {
    let fixture = Fixture::new();
    fixture.write("src/native.rs", "pub fn answer() -> i64 { 42 }");
    fixture.write("src/main.wi", "import native as first; import native as second; fn main() { println(first::answer() + second::answer()); }");
    fixture.without_toolchain(&["check", ".", "--offline"]);
}

#[test]
fn native_record_only_modules_check_with_unresolved_rust_dependencies() {
    let fixture = Fixture::new();
    fixture.write("project.toml", "[project]\nname = \"record_only\"\nversion = \"0.1.0\"\n[rust-dependencies]\nmissing_native = { path = \"not-installed\" }\n");
    fixture.write("src/record.rs", "pub struct Packet { pub value: i64 }");
    fixture.write(
        "src/main.wi",
        "import record::Packet; fn main() { let p = new Packet(42); println(p.value); }",
    );
    fixture.without_toolchain(&["check", ".", "--offline"]);
    let output = fixture.without_toolchain(&[
        "query",
        "rust-bridge",
        "--project-dir",
        ".",
        "--format",
        "json",
    ]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["bridges"].as_array().unwrap().is_empty(), "{value}");
}

#[cfg(unix)]
#[test]
fn native_module_symlinks_cannot_create_duplicate_source_identities() {
    let fixture = Fixture::new();
    fixture.write("src/native.rs", "pub fn answer() -> i64 { 42 }");
    std::os::unix::fs::symlink("native.rs", fixture.root.join("src/alias.rs")).unwrap();
    fixture.write("src/main.wi", "import native; import alias; fn main() {}");
    let output = fixture
        .command(&["check", ".", "--offline"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostics.contains("multiple module paths"),
        "{diagnostics}"
    );
}

#[test]
fn native_modules_preserve_legacy_nonblocking_review_recovery() {
    let fixture = Fixture::new();
    fixture.write(
        "project.toml",
        "[project]\nname=\"mixed_review\"\nversion=\"0.1.0\"\n[rust]\nbridge=\"bridge.rs\"\n[rust-dependencies]\nnative_probe = { path = \"dependency\" }\n",
    );
    fixture.local_dependency();
    fixture.write("bridge.rs", "pub fn legacy() -> i64 { 7 }");
    fixture.write("src/native.rs", "pub fn answer() -> i64 { 35 }");
    // Inspection discovers this module; executable builds do not import it.
    fixture.write("src/spare.rs", "pub fn unused() -> i64 { 123 }");
    fixture.write(
        "src/spare_user.wi",
        "import spare; pub fn unused() -> i64 { return spare::unused(); }",
    );
    let entry = |fingerprint: &str| {
        format!(
            "import native; extern rust {{ nonblocking(\"{fingerprint}\") fn legacy() -> i64; }} fn main() {{ println(native::answer() + legacy()); }}"
        )
    };
    fixture.write("src/main.wi", &entry(&"0".repeat(64)));
    let review = || {
        let value = fixture.review_query();
        (
            value["review_fingerprint"].as_str().unwrap().to_owned(),
            value["legacy_review_fingerprint"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
    };
    let approve = |fingerprint: &str| {
        fixture.write(
            "rust-review.toml",
            &format!("fingerprint = \"{fingerprint}\"\n"),
        );
    };
    let run_fails_review = || {
        let stale = fixture
            .command(&["run", ".", "--offline"])
            .output()
            .unwrap();
        assert!(!stale.status.success());
        assert!(
            String::from_utf8_lossy(&stale.stderr).contains("rust_nonblocking_review_required")
        );
    };
    let first = review();
    fixture.write("src/main.wi", &entry(&first.1));
    // The inline legacy claim covers the legacy bridge; the shared record
    // covers every Rust module. Both must be current.
    run_fails_review();
    approve(&first.0);
    assert_eq!(fixture.success(&["run", ".", "--offline"]).stdout, b"42\n");
    fixture.write("src/native.rs", "pub fn answer() -> i64 { 36 }");
    let native_edit = review();
    assert_eq!(native_edit.1, first.1);
    assert_ne!(native_edit.0, first.0);
    run_fails_review();
    approve(&native_edit.0);
    assert_eq!(fixture.success(&["run", ".", "--offline"]).stdout, b"43\n");
    fixture.write("bridge.rs", "pub fn legacy() -> i64 { 8 }");
    let second = review();
    assert_ne!(first.1, second.1);
    approve(&second.0);
    run_fails_review();
    fixture.write("src/main.wi", &entry(&second.1));
    assert_eq!(fixture.success(&["run", ".", "--offline"]).stdout, b"44\n");
    fixture.write("src/native.rs", "pub struct Data { pub value: i64 }");
    fs::remove_file(fixture.root.join("src/spare.rs")).unwrap();
    fs::remove_file(fixture.root.join("src/spare_user.wi")).unwrap();
    let legacy_only = |fingerprint: &str| {
        format!(
            "import native; extern rust {{ nonblocking(\"{fingerprint}\") fn legacy() -> i64; }} fn main() {{ println(legacy()); }}"
        )
    };
    fixture.write("src/main.wi", &legacy_only(&second.1));
    // Removing a discovered module changes the crate both reviews describe.
    let third = review();
    approve(&third.0);
    fixture.write("src/main.wi", &legacy_only(&third.1));
    assert_eq!(review(), third);
    assert_eq!(fixture.success(&["run", ".", "--offline"]).stdout, b"8\n");
}

#[test]
fn native_inspection_preserves_resolution_with_discovery_only_modules() {
    let fixture = Fixture::new();
    fixture.write("project.toml", "[project]\nname=\"discovery\"\nversion=\"0.1.0\"\n[rust]\nbridge=\"bridge.rs\"\n[rust-dependencies]\nnative_probe = { path = \"dependency\" }\n");
    fixture.local_dependency();
    fixture.write("bridge.rs", "pub fn legacy() -> i64 { 7 }");
    fixture.write("src/native.rs", "pub fn answer() -> i64 { 35 }");
    fixture.write(
        "src/unused.wi",
        "import native; pub fn answer() -> i64 { return native::answer(); }",
    );
    fixture.write(
        "src/main.wi",
        "extern rust { fn legacy() -> i64; } fn main() { println(legacy()); }",
    );
    assert_eq!(fixture.success(&["run", ".", "--offline"]).stdout, b"7\n");
    let original = fs::read(fixture.root.join("project.lock")).unwrap();
    for operation in ["metadata", "tree"] {
        fixture.success(&["rust", operation, "--frozen", "--format=json"]);
        assert_eq!(
            fs::read(fixture.root.join("project.lock")).unwrap(),
            original
        );
    }
    fixture.success(&["rust", "metadata", "--offline", "--format=json"]);
    let inspected = fs::read(fixture.root.join("project.lock")).unwrap();
    assert!(String::from_utf8_lossy(&inspected).contains("modules = true"));
    assert_eq!(fixture.success(&["run", ".", "--frozen"]).stdout, b"7\n");
    assert_eq!(
        fs::read(fixture.root.join("project.lock")).unwrap(),
        inspected
    );
}

#[test]
fn native_presence_probe_preserves_legacy_metadata_without_willow_entry() {
    let fixture = Fixture::new();
    fixture.write("project.toml", "[project]\nname=\"rust_metadata\"\nversion=\"0.1.0\"\n[rust]\nbridge=\"src/bridge.rs\"\n[rust-dependencies]\nnative_probe = { path = \"dependency\" }\n");
    fixture.local_dependency();
    fixture.write("src/bridge.rs", "pub fn legacy() -> i64 { 7 }");
    for (operation, flag) in [("metadata", "--offline"), ("tree", "--frozen")] {
        let output = fixture.success(&["rust", operation, flag, "--format=json"]);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["enabled"], true, "{value}");
    }
}

impl Fixture {
    fn review_query(&self) -> serde_json::Value {
        let output = self.success(&[
            "query",
            "rust-bridge",
            "--project-dir",
            ".",
            "--offline",
            "--format",
            "json",
        ]);
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn approve_native(&self) -> String {
        let query = self.review_query();
        let fingerprint = query["review_fingerprint"].as_str().unwrap();
        let record = format!("fingerprint = \"{fingerprint}\"\n");
        self.write("rust-review.toml", &record);
        record
    }
}

const ASSUMED_NATIVE: &str = r#"
#[willow::assume(no_block)]
pub struct Handle(i64);
#[willow::assume(no_block)]
pub fn make() -> Box<Handle> { Box::new(Handle(42)) }
#[willow::assume(no_block)]
pub fn answer() -> i64 { 42 }
impl Handle {
    #[willow::assume(no_block)]
    pub fn read(&self) -> i64 { self.0 }
}
"#;

#[test]
fn native_assume_contexts_and_review_queries() {
    let f = Fixture::new();
    f.write("src/native.rs", ASSUMED_NATIVE);
    f.write(
        "src/main.wi",
        r#"
import native;
@effects(no_wait) fn exercise() -> i64 {
    let handle = native::make();
    let value = native::Handle_read(handle) + native::answer();
    native::Handle_close(handle);
    return value;
}
async fn main() {
    println(exercise());
    let m = Mutex::new(0);
    lock m as value { println(exercise()); }
}
"#,
    );
    let q = f.review_query();
    for b in q["bridges"].as_array().unwrap() {
        assert_eq!(b["trust"], "developer_declared");
        assert_eq!(b["review_current"], false);
        let effects = b["effects"].as_u64().unwrap() as u8;
        assert_ne!(effects & willow_abi::RuntimeEffects::MAY_BLOCK.bits(), 0);
        assert_ne!(effects & willow_abi::RuntimeEffects::MAY_ALLOCATE.bits(), 0);
        assert_ne!(effects & willow_abi::RuntimeEffects::MAY_PANIC.bits(), 0);
        assert_ne!(effects & willow_abi::ffi::FOREIGN_CALL, 0);
    }
    let record = f.approve_native();
    assert!(
        f.review_query()["bridges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["review_current"] == true)
    );
    for _ in 0..2 {
        f.success(&["check", ".", "--offline"]);
        f.success(&["build", ".", "--offline"]);
        let output = f.success(&["run", ".", "--offline"]);
        assert_eq!(String::from_utf8_lossy(&output.stdout), "84\n84\n");
        f.success(&["rust", "metadata", "--offline"]);
        f.success(&["rust", "update", "--offline"]);
        assert_eq!(
            fs::read_to_string(f.root.join("rust-review.toml")).unwrap(),
            record
        );
    }
    let fingerprint = f.review_query()["review_fingerprint"]
        .as_str()
        .unwrap()
        .to_owned();
    for path in [
        "src/native.rs",
        "src/main.wi",
        "project.toml",
        "project.lock",
        ".willow/rust/Cargo.lock",
    ] {
        assert!(
            !fs::read_to_string(f.root.join(path))
                .unwrap()
                .contains(&fingerprint),
            "{path}"
        );
    }
}

#[test]
fn native_assume_mutations_expire_all_commands_without_writing_review() {
    let f = Fixture::new();
    f.local_dependency();
    f.write("project.toml", "[project]\nname=\"review\"\nversion=\"0.1.0\"\n[rust-dependencies]\nnative_probe={path=\"dependency\"}\n");
    f.write(
        "src/main.wi",
        "import native; fn main() { println(native::answer()); }",
    );
    f.write("src/native.rs", ASSUMED_NATIVE);
    f.write("src/unused.rs", "pub fn unused() {}\n");
    for mutation in [
        "missing",
        "body",
        "remove",
        "add",
        "unused",
        "dependency",
        "manifest",
    ] {
        let record = if mutation == "missing" {
            None
        } else {
            Some(f.approve_native())
        };
        match mutation {
            "body" => f.write("src/native.rs", &ASSUMED_NATIVE.replace("42", "43")),
            "remove" => f.write("src/native.rs", &ASSUMED_NATIVE.replace("#[willow::assume(no_block)]", "")),
            "add" => f.write("src/native.rs", ASSUMED_NATIVE),
            "unused" => f.write("src/unused.rs", "pub fn unused() { let _x = 1; }\n"),
            "dependency" => f.write("dependency/Cargo.toml", "[package]\nname=\"native_probe\"\nversion=\"0.1.1\"\nedition=\"2024\"\n[workspace]\n"),
            "manifest" => f.write("project.toml", "[project]\nname=\"review\"\nversion=\"0.1.0\"\n[rust-dependencies]\nnative_probe={path=\"dependency\",default-features=false}\n"),
            _ => {}
        }
        for command in ["check", "build", "run"] {
            let output = f.command(&[command, ".", "--offline"]).output().unwrap();
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{mutation}/{command}");
            assert!(
                error.contains("rust_nonblocking_review_required")
                    && error.contains("rust-review.toml"),
                "{mutation}/{command}: {error}"
            );
            assert_eq!(
                fs::read_to_string(f.root.join("rust-review.toml")).ok(),
                record
            );
        }
        let q = f.review_query();
        assert!(
            q["bridges"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b["review_current"] == false)
        );
    }
}

#[test]
fn native_assume_unreviewed_contexts_keep_diagnostics() {
    for call in [
        "native::answer();",
        "let h = native::make(); native::Handle_read(h);",
        "let h = native::make(); native::Handle_close(h);",
    ] {
        for context in ["task", "no_wait", "lock"] {
            let f = Fixture::new();
            // Isolate the unasserted operation from the reviewed constructor.
            let source = ASSUMED_NATIVE
                .replace("#[willow::assume(no_block)]\npub struct", "pub struct")
                .replace(
                    "#[willow::assume(no_block)]\npub fn answer",
                    "pub fn answer",
                )
                .replace("    #[willow::assume(no_block)]\n", "");
            f.write("src/native.rs", &source);
            let body = match context {
                "task" => format!("async fn main() {{ {call} }}"),
                "no_wait" => format!("@effects(no_wait) fn test() {{ {call} }} fn main() {{}}"),
                _ => format!(
                    "async fn main() {{ let m = Mutex::new(0); lock m as value {{ {call} }} }}"
                ),
            };
            f.write("src/main.wi", &format!("import native; {body}"));
            // Review the remaining constructor claim so only context errors remain.
            f.approve_native();
            let output = f.command(&["check", ".", "--offline"]).output().unwrap();
            assert!(!output.status.success(), "{call}/{context}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                error.contains(match context {
                    "task" => "E0810",
                    "lock" => "E2604",
                    _ => "E2612",
                }) && !error.contains("rust_nonblocking_review_required"),
                "{error}"
            );
        }
    }
}

#[test]
fn native_assume_macro_supports_standalone_rustc_and_cargo() {
    let f = Fixture::new();
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let macro_file = f.temp.join(format!(
        "{}willow{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    let output = Command::new("rustc")
        .args([
            "--edition=2024",
            "--crate-type=proc-macro",
            "--crate-name=willow",
        ])
        .arg(repo.join("crates/willow_assume/src/lib.rs"))
        .arg("-o")
        .arg(&macro_file)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new("rustc")
        .args(["--edition=2024", "--crate-type=lib", "--extern"])
        .arg(format!("willow={}", macro_file.display()))
        .arg(repo.join("example/rust_assume/src/native.rs"))
        .arg("-o")
        .arg(f.temp.join("standalone.rlib"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // A fresh standalone package avoids modifying the checked-in example lock.
    f.write("Cargo.toml", &format!("[package]\nname=\"standalone_assume\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[lib]\npath=\"src/native.rs\"\n[workspace]\n[dependencies]\nwillow={{package=\"willow_assume\",path={:?}}}\n", repo.join("crates/willow_assume").to_string_lossy()));
    f.write(
        "src/native.rs",
        include_str!("../example/rust_assume/src/native.rs"),
    );
    let output = Command::new("cargo")
        .current_dir(&f.root)
        .args([
            "test",
            "--offline",
            "reviewed_methods_work_without_willow_compiler",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_assume_example_runs() {
    let f = Fixture::new();
    f.write(
        "src/main.wi",
        include_str!("../example/rust_assume/src/main.wi"),
    );
    f.write(
        "src/native.rs",
        include_str!("../example/rust_assume/src/native.rs"),
    );
    f.write(
        "rust-review.toml",
        include_str!("../example/rust_assume/rust-review.toml"),
    );
    let output = f.success(&["run", ".", "--offline"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n42\n");
}

#[test]
fn native_assume_external_sources_fail_closed() {
    let f = Fixture::new();
    f.write(
        "src/main.wi",
        "import native; fn main() { println(native::answer()); }",
    );
    f.write("src/native.rs", "mod helper { pub fn answer() -> i64 { 42 } } #[willow::assume(no_block)] pub fn answer() -> i64 { helper::answer() }");
    let record = f.approve_native();
    assert_eq!(f.success(&["run", ".", "--offline"]).stdout, b"42\n");
    f.write("src/native.rs", "#[path = \"../helper.rs\"] mod helper; #[willow::assume(no_block)] pub fn answer() -> i64 { helper::answer() }");
    for value in [42, 43] {
        f.write(
            "helper.rs",
            &format!("pub fn answer() -> i64 {{ {value} }}"),
        );
        // Even a new query must refuse to issue a review for this layout.
        let output = f
            .command(&[
                "query",
                "rust-bridge",
                "--project-dir",
                ".",
                "--offline",
                "--format",
                "json",
            ])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "uncovered source received a review: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        for command in ["check", "build", "run"] {
            let output = f.command(&[command, ".", "--offline"]).output().unwrap();
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success(),
                "{command}: uncovered helper edit accepted"
            );
            assert!(
                error.contains("rust_review_source_layout") && error.contains("native.rs"),
                "{command}: {error}"
            );
            assert_eq!(
                fs::read_to_string(f.root.join("rust-review.toml")).unwrap(),
                record
            );
        }
    }
}

#[test]
fn native_assume_external_sources_in_unused_and_legacy_files_fail_closed() {
    for mixed_legacy in [false, true] {
        let f = Fixture::new();
        f.write(
            "src/main.wi",
            "import native; fn main() { println(native::answer()); }",
        );
        f.write(
            "src/native.rs",
            "#[willow::assume(no_block)] pub fn answer() -> i64 { 42 }",
        );
        if mixed_legacy {
            f.write("project.toml", "[project]\nname=\"review_layout\"\nversion=\"0.1.0\"\n[rust]\nbridge=\"bridge.rs\"\n[rust-dependencies]\nnative_probe={path=\"dependency\"}\n");
            f.local_dependency();
            f.write("bridge.rs", "pub fn legacy() -> i64 { 1 }");
        }
        let record = f.approve_native();
        let path = if mixed_legacy {
            "bridge.rs"
        } else {
            "src/unused.rs"
        };
        f.write(path, "#[path = \"helper.rs\"] mod helper;");
        f.write("helper.rs", "pub fn value() -> i64 { 1 }");
        f.write("src/helper.rs", "pub fn value() -> i64 { 1 }");
        for command in ["check", "build", "run"] {
            let output = f.command(&[command, ".", "--offline"]).output().unwrap();
            assert!(!output.status.success());
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                error.contains("rust_review_source_layout"),
                "{mixed_legacy}/{command}: {error}"
            );
            assert_eq!(
                fs::read_to_string(f.root.join("rust-review.toml")).unwrap(),
                record
            );
        }
    }
}

fn unified_assumption_effects(fixture: &Fixture, name: &str) -> serde_json::Value {
    use willow_compiler::{
        CompilerOptions, CompilerSession,
        ai::{QueryRequest, QuerySession},
        diagnostics::HumanEmitter,
    };
    let mut options = CompilerOptions::debug();
    options.offline = true;
    let snapshot = CompilerSession::new(
        fixture.root.join("src/main.wi").to_str().unwrap(),
        "",
        &options,
        Some(fixture.root.clone()),
    )
    .analysis_with_emitter(&mut HumanEmitter)
    .unwrap();
    let function = snapshot
        .functions
        .iter()
        .find(|f| f.name == name)
        .unwrap()
        .id
        .clone();
    let revision = snapshot.revision.clone();
    QuerySession::new(snapshot)
        .unwrap()
        .query(QueryRequest::Effects { revision, function })["result"]
        .take()
}

#[test]
fn unified_assumption_native_review_and_transitive_evidence() {
    use willow_abi::RuntimeEffects;
    for no_block in [false, true] {
        let f = Fixture::new();
        let effects = if no_block { "no_block, no_io" } else { "no_io" };
        let native = ASSUMED_NATIVE.replace("no_block", effects);
        f.write("src/native.rs", &native);
        let contracts = if no_block { "no_io, no_wait" } else { "no_io" };
        f.write(
            "src/main.wi",
            &format!(
                r#"
import native;
fn helper() -> i64 {{
    let handle = native::make();
    let value = native::Handle_read(handle) + native::answer();
    native::Handle_close(handle);
    return value;
}}
@effects({contracts}) fn exercise() -> i64 {{ return helper(); }}
fn main() {{ println(exercise()); }}
"#
            ),
        );
        let missing = f.review_query();
        assert_eq!(missing["schema"], 2);
        assert_eq!(missing["bridges"].as_array().unwrap().len(), 4);
        for bridge in missing["bridges"].as_array().unwrap() {
            assert_eq!(bridge["schema"], 2);
            assert_eq!(bridge["assumptions"]["no_io"], true);
            assert_eq!(bridge["assumptions"]["no_block"], no_block);
            assert_eq!(bridge["review_current"], false);
            assert_eq!(bridge["evidence_kind"], "unknown");
            let bits = bridge["effects"].as_u64().unwrap() as u8;
            assert_ne!(bits & RuntimeEffects::MAY_IO.bits(), 0);
            assert_ne!(bits & RuntimeEffects::MAY_BLOCK.bits(), 0);
        }
        let output = f.command(&["check", ".", "--offline"]).output().unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("E2612"),
            "{output:?}"
        );

        let record = f.approve_native();
        f.success(&["check", ".", "--offline"]);
        let reviewed = f.review_query();
        for bridge in reviewed["bridges"].as_array().unwrap() {
            assert_eq!(bridge["review_current"], true);
            assert_eq!(bridge["evidence_kind"], "assumption");
            assert_eq!(bridge["official_verified"], false);
            let bits = bridge["effects"].as_u64().unwrap() as u8;
            assert_eq!(bits & RuntimeEffects::MAY_IO.bits(), 0);
            assert_eq!(bits & RuntimeEffects::MAY_BLOCK.bits() == 0, no_block);
            assert_ne!(bits & RuntimeEffects::MAY_ALLOCATE.bits(), 0);
            assert_ne!(bits & RuntimeEffects::MAY_PANIC.bits(), 0);
            let location = &bridge["assumption_location"];
            assert!(location["path"].as_str().unwrap().ends_with("native.rs"));
            let start = location["start"].as_u64().unwrap() as usize;
            let end = location["end"].as_u64().unwrap() as usize;
            assert_eq!(&native[start..end], format!("#[willow::assume({effects})]"));
        }
        for function in ["helper", "exercise"] {
            let evidence = unified_assumption_effects(&f, function);
            assert_eq!(evidence["schema"], "willow.effects.v3");
            assert_eq!(evidence["assumptions"]["no_io"]["kind"], "assumption");
            assert_eq!(!evidence["assumptions"]["no_wait"].is_null(), no_block);
            let io = evidence["effects"]
                .as_array()
                .unwrap()
                .iter()
                .find(|effect| effect["effect"] == RuntimeEffects::MAY_IO.bits())
                .unwrap();
            assert_eq!(io["state"], "none", "{evidence}");
            assert_eq!(io["evidence_kind"], "assumption", "{evidence}");
            assert!(
                io["assumption"]["location"]["path"]
                    .as_str()
                    .unwrap()
                    .ends_with("native.rs")
            );
        }
        assert_eq!(
            fs::read_to_string(f.root.join("rust-review.toml")).unwrap(),
            record
        );
        // Both changed source and a removed record revoke all claims together.
        f.write("src/native.rs", &native.replace("42", "43"));
        let stale = f.review_query();
        assert_ne!(stale["review_fingerprint"], reviewed["review_fingerprint"]);
        assert!(
            stale["bridges"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b["review_current"] == false)
        );
        let output = f.command(&["check", ".", "--offline"]).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("E2612"));
        f.approve_native();
        f.success(&["check", ".", "--offline"]);
        fs::remove_file(f.root.join("rust-review.toml")).unwrap();
        let output = f.command(&["check", ".", "--offline"]).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("E2612"));
    }
}

#[test]
fn unified_assumption_native_library_legacy_no_io_requires_shared_review() {
    use willow_abi::RuntimeEffects;
    let f = Fixture::new();
    f.local_dependency();
    f.write("project.toml", "[project]\nname=\"native_review\"\nversion=\"0.1.0\"\n[rust]\nbridge=\"bridge.rs\"\n[rust-dependencies]\nnative_probe={path=\"dependency\"}\n");
    f.write("bridge.rs", "unsafe extern \"C\" { fn abs(value: i32) -> i32; }\npub fn absolute(value: i64) -> i64 { unsafe { abs(value as i32) as i64 } }\n");
    f.write("src/main.wi", "extern rust { @effects(no_io) fn absolute(value: i64) -> i64; }\nfn helper() -> i64 { return absolute(-7); }\n@effects(no_io) fn exercise() -> i64 { return helper(); }\nfn main() { println(exercise()); }\n");
    let missing = f.review_query();
    assert_eq!(missing["schema"], 2);
    assert_eq!(missing["bridges"][0]["review_current"], false);
    assert_ne!(
        missing["bridges"][0]["effects"].as_u64().unwrap() as u8 & RuntimeEffects::MAY_IO.bits(),
        0
    );
    let output = f.command(&["check", ".", "--offline"]).output().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("E2612"),
        "{output:?}"
    );
    f.approve_native();
    f.success(&["check", ".", "--offline"]);
    let reviewed = f.review_query();
    assert_eq!(reviewed["bridges"][0]["review_current"], true);
    assert_eq!(reviewed["bridges"][0]["evidence_kind"], "assumption");
    let evidence = unified_assumption_effects(&f, "exercise");
    assert_eq!(evidence["assumptions"]["no_io"]["kind"], "assumption");
    assert!(
        evidence["assumptions"]["no_io"]["location"]["path"]
            .as_str()
            .unwrap()
            .ends_with("main.wi")
    );
    f.write("bridge.rs", "unsafe extern \"C\" { fn abs(value: i32) -> i32; }\npub fn absolute(value: i64) -> i64 { unsafe { abs(value as i32) as i64 + 1 } }\n");
    let output = f.command(&["check", ".", "--offline"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("E2612"));
    assert_eq!(f.review_query()["bridges"][0]["review_current"], false);
}

#[test]
fn unified_assumption_legacy_claim_edits_invalidate_shared_review() {
    let f = Fixture::new();
    f.local_dependency();
    f.write("project.toml", "[project]\nname='legacy_claims'\nversion='0.1.0'\n[rust]\nbridge='bridge.rs'\n[rust-dependencies]\nnative_probe={path='dependency'}\n");
    f.write(
        "bridge.rs",
        "pub fn safe() -> i64 { 42 }\npub fn noisy() -> i64 { println!(\"I/O\"); 7 }\n",
    );
    let source = |safe: &str, noisy: &str, callee: &str| {
        format!(
            "extern rust {{ {safe} fn safe() -> i64; {noisy} fn noisy() -> i64; }}\n@effects(no_io) fn guard() -> i64 {{ return {callee}(); }}\nfn main() {{ println(guard()); }}\n"
        )
    };
    let original = source("@effects(no_io)", "", "safe");
    f.write("src/main.wi", &original);
    let record = f.approve_native();
    f.success(&["check", ".", "--offline"]);
    let reviewed = f.review_query();
    let inline = reviewed["legacy_review_fingerprint"].as_str().unwrap();
    let shared = &reviewed["review_fingerprint"];

    // Layout and declaration iteration order are not new claims.
    f.write("src/main.wi", "// moved declarations\nextern rust { fn noisy() -> i64;\n    @effects(no_io) fn safe() -> i64; }\n@effects(no_io) fn guard() -> i64 { return safe(); }\nfn main() { println(guard()); }\n");
    let reordered = f.review_query();
    assert_eq!(&reordered["review_fingerprint"], shared);
    assert_eq!(reordered["legacy_review_fingerprint"], inline);
    assert!(
        reordered["bridges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["symbol"] == "safe" && b["review_current"] == true)
    );
    f.success(&["check", ".", "--offline"]);

    for (mutation, edited) in [
        (
            "add no_io to an already-declared I/O function",
            source("@effects(no_io)", "@effects(no_io)", "noisy"),
        ),
        ("remove no_io", source("", "", "safe")),
        (
            "change no_io to no_block",
            source(&format!("nonblocking(\"{inline}\")"), "", "safe"),
        ),
    ] {
        f.write("src/main.wi", &edited);
        let query = f.review_query();
        assert_ne!(&query["review_fingerprint"], shared, "{mutation}: {query}");
        assert_eq!(query["legacy_review_fingerprint"], inline, "{mutation}");
        assert!(
            query["bridges"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b["review_current"] == false),
            "{mutation}: {query}"
        );
        let output = f.command(&["check", ".", "--offline"]).output().unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{mutation}");
        assert!(
            error.contains("E2612") && error.contains("no_io"),
            "{mutation}: {error}"
        );
        assert_eq!(
            fs::read_to_string(f.root.join("rust-review.toml")).unwrap(),
            record
        );
    }
    f.write("src/main.wi", &original);
    assert_eq!(&f.review_query()["review_fingerprint"], shared);
    f.success(&["check", ".", "--offline"]);
}

#[test]
fn unified_assumption_dependency_input_escape_fails_closed() {
    let f = Fixture::new();
    f.local_dependency();
    f.write("project.toml", "[project]\nname='dependency_boundary'\nversion='0.1.0'\n[rust-dependencies]\nnative_probe={path='dependency'}\n");
    f.write(
        "src/native.rs",
        "#[willow::assume(no_block, no_io)]\npub fn answer() -> i64 { native_probe::value() }\n",
    );
    f.write("src/main.wi", "import native;\n@effects(no_io, no_wait) fn guard() -> i64 { return native::answer(); }\nfn main() { println(guard()); }\n");
    let record = f.approve_native();
    f.success(&["check", ".", "--offline"]);
    f.write("outside.bin", "*");
    for (case, dependency, hidden) in [
        (
            "safe code hidden in an excluded directory",
            "include!(\"../target/behavior.rs\");\n",
            "pub fn value() -> i64 { 1 }\n",
        ),
        (
            "I/O and waiting code hidden in an excluded directory",
            "include!(\"../target/behavior.rs\");\n",
            "pub fn value() -> i64 { println!(\"I/O\"); std::thread::sleep(std::time::Duration::from_millis(1)); 1 }\n",
        ),
        (
            "data outside the dependency root",
            "pub fn value() -> i64 { include_bytes!(\"../../outside.bin\")[0] as i64 }\n",
            "pub fn value() -> i64 { 1 }\n",
        ),
    ] {
        f.write("dependency/target/behavior.rs", hidden);
        f.write("dependency/src/lib.rs", dependency);
        let query = f
            .command(&[
                "query",
                "rust-bridge",
                "--project-dir",
                ".",
                "--offline",
                "--format",
                "json",
            ])
            .output()
            .unwrap();
        let error = format!(
            "{}\n{}",
            String::from_utf8_lossy(&query.stdout),
            String::from_utf8_lossy(&query.stderr)
        );
        assert!(!query.status.success(), "{case}: {query:?}");
        assert!(
            error.contains("rust_review_source_layout"),
            "{case}: {error}"
        );
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&query.stdout) {
            assert!(value["review_fingerprint"].is_null(), "{case}: {value}");
        }
        let checked = f.command(&["check", ".", "--offline"]).output().unwrap();
        let error = String::from_utf8_lossy(&checked.stderr);
        assert!(!checked.status.success(), "{case}: {checked:?}");
        for expected in ["E2612", "no_io", "no_wait"] {
            assert!(error.contains(expected), "{case}/{expected}: {error}");
        }
        assert_eq!(
            fs::read_to_string(f.root.join("rust-review.toml")).unwrap(),
            record,
            "{case}"
        );
    }
}

#[test]
fn unified_assumption_root_native_link_archive_replacement_fails_closed() {
    for legacy in [false, true] {
        let f = Fixture::new();
        f.local_dependency();
        let bridge_setting = if legacy {
            "[rust]\nbridge='bridge.rs'\n"
        } else {
            ""
        };
        f.write("project.toml", &format!("[project]\nname='root_link'\nversion='0.1.0'\n{bridge_setting}[rust-dependencies]\nnative_probe={{path='dependency'}}\n"));
        let path = if legacy { "bridge.rs" } else { "src/native.rs" };
        let attribute = if legacy {
            ""
        } else {
            "#[willow::assume(no_block, no_io)]\n"
        };
        f.write(
            path,
            &format!("{attribute}pub fn answer() -> i64 {{ 42 }}\n"),
        );
        let main = |fingerprint: &str| {
            let prefix = if legacy {
                format!(
                    "extern rust {{ @effects(no_io) nonblocking(\"{fingerprint}\") fn answer() -> i64; }}\n"
                )
            } else {
                "import native;\n".into()
            };
            let call = if legacy { "answer" } else { "native::answer" };
            format!(
                "{prefix}@effects(no_io, no_wait) fn guard() -> i64 {{ return {call}(); }}\nfn main() {{ println(guard()); }}\n"
            )
        };
        f.write("src/main.wi", &main(&"0".repeat(64)));
        let native = root_link_archive(&f, false);
        let run = |args: &[&str]| {
            f.command(args)
                .env_remove("RUSTFLAGS")
                .env(
                    "CARGO_ENCODED_RUSTFLAGS",
                    format!("-L\u{1f}native={}", native.display()),
                )
                .output()
                .unwrap()
        };
        let query_args = [
            "query",
            "rust-bridge",
            "--project-dir",
            ".",
            "--offline",
            "--format",
            "json",
        ];
        if legacy {
            let output = run(&query_args);
            assert!(output.status.success(), "{output:?}");
            let query: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            f.write(
                "src/main.wi",
                &main(query["legacy_review_fingerprint"].as_str().unwrap()),
            );
        }
        let output = run(&query_args);
        assert!(output.status.success(), "{output:?}");
        let query: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let record = format!(
            "fingerprint='{}'\n",
            query["review_fingerprint"].as_str().unwrap()
        );
        f.write("rust-review.toml", &record);
        let checked = run(&["check", ".", "--offline"]);
        assert!(checked.status.success(), "{checked:?}");

        f.write(path, &format!("#[link(name=\"willow_payload\", kind=\"static\")]\nunsafe extern \"C\" {{ fn payload() -> i64; }}\n{attribute}pub fn answer() -> i64 {{ unsafe {{ payload() }} }}\n"));
        for io in [false, true] {
            if io {
                assert_eq!(root_link_archive(&f, true), native);
            }
            reject_opaque_root_claims(&f, &run, &record);
        }
    }
}

fn reject_opaque_root_claims(fixture: &Fixture, run: &impl Fn(&[&str]) -> Output, record: &str) {
    let output = run(&[
        "query",
        "rust-bridge",
        "--project-dir",
        ".",
        "--offline",
        "--format",
        "json",
    ]);
    let diagnostic = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.status.success(), "{diagnostic}");
    assert!(
        diagnostic.contains("rust_review_source_layout"),
        "{diagnostic}"
    );
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
        assert!(value["review_fingerprint"].is_null(), "{value}");
    }
    let checked = run(&["check", ".", "--offline"]);
    let diagnostic = String::from_utf8_lossy(&checked.stderr);
    assert!(!checked.status.success(), "{diagnostic}");
    for expected in ["E2612", "no_io", "no_wait"] {
        assert!(diagnostic.contains(expected), "{expected}: {diagnostic}");
    }
    assert_eq!(
        fs::read_to_string(fixture.root.join("rust-review.toml")).unwrap(),
        record
    );
}

#[test]
fn unified_assumption_root_assembly_input_replacement_fails_closed() {
    for legacy in [false, true] {
        let f = Fixture::new();
        f.local_dependency();
        let bridge = if legacy {
            "[rust]\nbridge='bridge.rs'\n"
        } else {
            ""
        };
        f.write("project.toml", &format!("[project]\nname='root_assembly'\nversion='0.1.0'\n{bridge}[rust-dependencies]\nnative_probe={{path='dependency'}}\n"));
        let path = if legacy { "bridge.rs" } else { "src/native.rs" };
        let attribute = if legacy {
            ""
        } else {
            "#[willow::assume(no_block, no_io)]\n"
        };
        f.write(
            path,
            &format!("{attribute}pub fn answer() -> i64 {{ 42 }}\n"),
        );
        let main = |fingerprint: &str| {
            let prefix = if legacy {
                format!(
                    "extern rust {{ @effects(no_io) nonblocking(\"{fingerprint}\") fn answer() -> i64; }}\n"
                )
            } else {
                "import native;\n".into()
            };
            let call = if legacy { "answer" } else { "native::answer" };
            format!(
                "{prefix}@effects(no_io, no_wait) fn guard() -> i64 {{ return {call}(); }}\nfn main() {{ println(guard()); }}\n"
            )
        };
        f.write("src/main.wi", &main(&"0".repeat(64)));
        if legacy {
            let query = f.review_query();
            f.write(
                "src/main.wi",
                &main(query["legacy_review_fingerprint"].as_str().unwrap()),
            );
        }
        let record = f.approve_native();
        f.success(&["check", ".", "--offline"]);
        let external = f.temp.join("external_payload.s");
        let run = |args: &[&str]| f.command(args).output().unwrap();
        for (mac, operation) in [
            ("global_asm", ".include"),
            ("asm", ".include"),
            ("asm", ".incbin"),
            ("naked_asm", ".include"),
        ] {
            let directive = format!("{operation} {:?}", external.to_string_lossy());
            let source = root_assembly_source(attribute, mac, &directive);
            f.write(path, &source);
            // These inputs are deliberately never assembled by this test:
            // the reviewed-source gate must reject them on every host.
            for payload in [
                ".text\n.globl assembly_payload\nassembly_payload:\nmov rax, 42\nret\n",
                ".text\n.globl assembly_payload\nassembly_payload:\nsub rsp, 8\nlea rdi, [rip + message]\ncall puts\nadd rsp, 8\nmov rax, 42\nret\n.section .rodata\nmessage: .asciz \"unexpected I/O\"\n",
            ] {
                fs::write(&external, payload).unwrap();
                reject_opaque_root_claims(&f, &run, &record);
                assert_eq!(fs::read_to_string(f.root.join(path)).unwrap(), source);
            }
        }
    }
}

fn root_assembly_source(attribute: &str, mac: &str, directive: &str) -> String {
    match mac {
        "global_asm" => format!(
            "std::arch::global_asm!({directive:?});\nunsafe extern \"C\" {{ fn assembly_payload() -> i64; }}\n{attribute}pub fn answer() -> i64 {{ unsafe {{ assembly_payload() }} }}\n"
        ),
        "asm" => format!(
            "{attribute}pub fn answer() -> i64 {{ unsafe {{ std::arch::asm!({directive:?}); }} 42 }}\n"
        ),
        "naked_asm" => format!(
            "#[unsafe(naked)]\nunsafe extern \"C\" fn assembly_payload() -> i64 {{ std::arch::naked_asm!({directive:?}); }}\n{attribute}pub fn answer() -> i64 {{ unsafe {{ assembly_payload() }} }}\n"
        ),
        _ => unreachable!(),
    }
}

/// Compile and execute a real native archive independently of Willow admission.
/// The directory deliberately lies outside the reviewed project source tree.
fn root_link_archive(fixture: &Fixture, io: bool) -> PathBuf {
    let directory = fixture.temp.join("external-native");
    fs::create_dir_all(&directory).unwrap();
    let source = directory.join("payload.c");
    fs::write(&source, if io {
        "#include <stdint.h>\n#include <stdio.h>\nint64_t payload(void) { puts(\"unexpected I/O\"); return 42; }\n"
    } else {
        "#include <stdint.h>\nint64_t payload(void) { return 42; }\n"
    }).unwrap();
    let host = target_lexicon::HOST.to_string();
    let mut builder = cc::Build::new();
    builder
        .cargo_metadata(false)
        .host(&host)
        .target(&host)
        .opt_level(0)
        .debug(false)
        .out_dir(&directory)
        .file(&source);
    let compiler = builder.get_compiler();
    let archive = directory.join(if compiler.is_like_msvc() {
        "willow_payload.lib"
    } else {
        "libwillow_payload.a"
    });
    if archive.exists() {
        fs::remove_file(&archive).unwrap();
    }
    builder.compile("willow_payload");
    let harness = directory.join("harness.c");
    fs::write(&harness, "#include <stdint.h>\nint64_t payload(void);\nint main(void) { return payload() == 42 ? 0 : 1; }\n").unwrap();
    let executable = directory.join(if cfg!(windows) {
        "harness.exe"
    } else {
        "harness"
    });
    let mut command = compiler.to_command();
    command.current_dir(&directory).arg(&harness).arg(&archive);
    if compiler.is_like_msvc() {
        command
            .arg("/nologo")
            .arg(format!("/Fe{}", executable.display()));
    } else {
        command.arg("-o").arg(&executable);
    }
    let compiled = command.output().unwrap();
    assert!(compiled.status.success(), "{compiled:?}");
    let output = Command::new(&executable).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        if io { "unexpected I/O\n" } else { "" }
    );
    directory
}

#[test]
fn unified_assumption_native_manifest_review_and_execution() {
    let f = Fixture::new();
    let mut prepare = Command::new(if cfg!(windows) { "python" } else { "python3" });
    prepare
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("example/native_library/prepare.py"))
        .arg(&f.root);
    #[cfg(all(windows, target_env = "msvc"))]
    {
        let compiler = cc::windows_registry::find_tool(&target_lexicon::HOST.to_string(), "cl.exe")
            .expect("native example requires MSVC");
        prepare.env("CC", compiler.path());
        for (key, value) in compiler.get_envs() {
            prepare.env(key, value);
        }
    }
    let output = prepare
        .output()
        .expect("native example requires Python and a C toolchain");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    f.write("src/main.wi", "extern rust { @effects(no_io) fn add(a: i64, b: i64) -> i64; }\n@effects(no_io) fn exercise() -> i64 { return add(20, 22); }\nfn main() { println(exercise()); }\n");
    f.success(&["fetch", "--offline"]);
    assert_eq!(f.review_query()["bridges"][0]["review_current"], false);
    let output = f.command(&["check", ".", "--offline"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("E2612"));
    let record = f.approve_native();
    f.success(&["check", ".", "--offline"]);
    let output = f.success(&["run", ".", "--offline"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n");
    assert_eq!(
        f.review_query()["bridges"][0]["evidence_kind"],
        "assumption"
    );
    let manifest = fs::read_to_string(f.root.join("project.toml")).unwrap();
    f.write(
        "project.toml",
        &manifest.replace("locally compiled add.c", "reviewed native provider changed"),
    );
    let stale = f.review_query();
    assert_eq!(stale["bridges"][0]["review_current"], false);
    let output = f.command(&["check", ".", "--offline"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("E2612"));
    assert_eq!(
        fs::read_to_string(f.root.join("rust-review.toml")).unwrap(),
        record
    );
    f.approve_native();
    f.success(&["check", ".", "--offline"]);
}
