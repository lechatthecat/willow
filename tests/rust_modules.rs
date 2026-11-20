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
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
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
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
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
        let output = fixture.success(&[
            "query",
            "rust-bridge",
            "--project-dir",
            ".",
            "--offline",
            "--format",
            "json",
        ]);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        value["review_fingerprint"].as_str().unwrap().to_owned()
    };
    let first = review();
    fixture.write("src/main.wi", &entry(&first));
    assert_eq!(fixture.success(&["run", ".", "--offline"]).stdout, b"42\n");
    fixture.write("src/native.rs", "pub fn answer() -> i64 { 36 }");
    assert_eq!(review(), first);
    assert_eq!(fixture.success(&["run", ".", "--offline"]).stdout, b"43\n");
    fixture.write("bridge.rs", "pub fn legacy() -> i64 { 8 }");
    let second = review();
    assert_ne!(first, second);
    let stale = fixture
        .command(&["run", ".", "--offline"])
        .output()
        .unwrap();
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("rust_nonblocking_review_required"));
    fixture.write("src/main.wi", &entry(&second));
    assert_eq!(fixture.success(&["run", ".", "--offline"]).stdout, b"44\n");
    fixture.write("src/native.rs", "pub struct Data { pub value: i64 }");
    fs::remove_file(fixture.root.join("src/spare.rs")).unwrap();
    fs::remove_file(fixture.root.join("src/spare_user.wi")).unwrap();
    fixture.write("src/main.wi", &format!("import native; extern rust {{ nonblocking(\"{second}\") fn legacy() -> i64; }} fn main() {{ println(legacy()); }}"));
    assert_eq!(review(), second);
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
