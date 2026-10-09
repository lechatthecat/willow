//! End-to-end scalar ABI contract and deterministic adapter scaling evidence.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use willow_compiler::BuildMode;
use willow_compiler::project::ProjectManifest;
use willow_compiler::rust_bridge::{RustBridgeSymbol, Scalar, wrappers};
use willow_compiler::toolchain::rust_bridge::{BridgeOptions, build_bridge};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
struct Fixture {
    temp: PathBuf,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = std::env::temp_dir().join(format!(
            "willow-scalar-bridge-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let root = temp.join("project with spaces");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_bridge/path");
        for file in [
            "project.toml",
            "bridge.rs",
            "native/Cargo.toml",
            "native/src/lib.rs",
        ] {
            fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
            fs::copy(source.join(file), root.join(file)).unwrap();
        }
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(temp.join("home")).unwrap();
        Self { temp, root }
    }
    fn sources(&self, willow: &str, rust: &str) {
        fs::write(self.root.join("src/main.wi"), willow).unwrap();
        fs::write(self.root.join("bridge.rs"), rust).unwrap();
    }
    fn command(&self, args: &[&str]) -> Output {
        let original_home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .unwrap();
        Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.root)
            .args(args)
            .env("WILLOW_KEEP_OBJECT", "1")
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
            )
            .output()
            .unwrap()
    }
    fn successful(&self, args: &[&str]) -> Output {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp);
    }
}
const ADD: &str = "pub fn add(a: i64, b: i64) -> i64 { a + b }";
const DECL: &str = "extern rust { fn add(a: i64, b: i64) -> i64; }";

#[test]
fn scalar_values_void_namespace_and_indirect_calls() {
    // Ten independent executable perspectives: integer result/sign/limits,
    // float argument/result, both booleans, void, namespace, function value.
    for (name, willow, rust, expected) in [
        ("add", format!("{DECL} fn main() {{ println(add(20, 22)); }}"), ADD, "42\n"),
        ("negative", format!("{DECL} fn main() {{ println(add(-50, 8)); }}"), ADD, "-42\n"),
        ("i64_boundary", format!("{DECL} fn main() {{ println(add(9223372036854775806, 1)); }}"), ADD, "9223372036854775807\n"),
        ("float", "extern rust { fn half(x: f64) -> f64; } fn main() { println(half(5.0) == 2.5); }".into(), "pub fn half(x: f64) -> f64 { x / 2.0 }", "true\n"),
        ("bool_true", "extern rust { fn invert(x: bool) -> bool; } fn main() { println(invert(false)); }".into(), "pub fn invert(x: bool) -> bool { !x }", "true\n"),
        ("bool_false", "extern rust { fn invert(x: bool) -> bool; } fn main() { println(invert(true)); }".into(), "pub fn invert(x: bool) -> bool { !x }", "false\n"),
        ("void", "extern rust { fn touch(); } fn main() { touch(); println(42); }".into(), "pub fn touch() {}", "42\n"),
        ("explicit_void", "extern rust { fn touch() -> void; } fn main() { touch(); println(42); }".into(), "pub fn touch() {}", "42\n"),
        ("namespace", "extern rust math { fn add(a: i64, b: i64) -> i64; } fn main() { println(math::add(20, 22)); }".into(), ADD, "42\n"),
        ("indirect", format!("{DECL} fn main() {{ let f = add; println(f(20, 22)); }}"), ADD, "42\n"),
    ] {
        let f = Fixture::new();
        f.sources(&willow, rust);
        let result = f.successful(&["run", "--offline"]);
        assert_eq!(String::from_utf8_lossy(&result.stdout), expected, "{name}");
    }
}

#[test]
fn check_build_and_frozen_reuse_preserve_authoritative_locks() {
    let f = Fixture::new();
    f.sources(
        &format!("{DECL} fn main() {{ println(add(20, 22)); }}"),
        ADD,
    );
    f.successful(&["check", "--offline"]);
    f.successful(&["build", "--offline", "-o", "app"]);
    let cargo = fs::read(f.root.join(".willow/rust/Cargo.lock")).unwrap();
    let project = fs::read(f.root.join("project.lock")).unwrap();
    let output = f.successful(&["run", "--frozen"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n");
    assert_eq!(
        cargo,
        fs::read(f.root.join(".willow/rust/Cargo.lock")).unwrap()
    );
    assert_eq!(project, fs::read(f.root.join("project.lock")).unwrap());
}

#[test]
fn missing_symbols_and_rust_signature_mismatches_are_diagnostics() {
    for (name, rust) in [
        ("missing", "pub fn other() {}"),
        (
            "argument_type",
            "pub fn add(a: f64, b: f64) -> i64 { (a + b) as i64 }",
        ),
        (
            "return_type",
            "pub fn add(a: i64, b: i64) -> f64 { (a + b) as f64 }",
        ),
        ("arity", "pub fn add(a: i64) -> i64 { a }"),
    ] {
        let f = Fixture::new();
        f.sources(
            &format!("{DECL} fn main() {{ println(add(20, 22)); }}"),
            rust,
        );
        let output = f.command(&["build", "--offline", "-o", "app"]);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{name}");
        let code = if name == "missing" {
            "rust_bridge_symbol_missing"
        } else {
            "rust_bridge_signature_mismatch"
        };
        assert!(error.contains(code), "{name}: {error}");
        assert!(!f.root.join("app").exists());
        assert!(!f.root.join("app.exe").exists());
    }
}

#[test]
fn unsupported_signatures_duplicate_and_malformed_declarations_fail() {
    for (name, declaration) in [
        ("i32_parameter", "extern rust { fn bad(x: i32); }"),
        ("u64_parameter", "extern rust { fn bad(x: u64); }"),
        ("f32_return", "extern rust { fn bad() -> f32; }"),
        (
            "raw_module_namespace",
            "extern rust native::module { fn bad(); }",
        ),
        ("reserved_namespace", "extern rust std { fn bad(); }"),
        ("string_parameter", "extern rust { fn bad(x: String); }"),
        ("array_parameter", "extern rust { fn bad(x: Array<i64>); }"),
        ("void_parameter", "extern rust { fn bad(x: void); }"),
        ("reference_parameter", "extern rust { fn bad(x: &i64); }"),
        ("string_return", "extern rust { fn bad() -> String; }"),
        ("duplicate", "extern rust { fn bad(); fn bad(); }"),
        ("local_collision", "extern rust { fn bad(); } fn bad() {}"),
        ("body_forbidden", "extern rust { fn bad() {} }"),
        ("missing_semicolon", "extern rust { fn bad() }"),
        ("missing_brace", "extern rust { fn bad();"),
        ("main_forbidden", "extern rust { fn main(); }"),
    ] {
        let f = Fixture::new();
        f.sources(&format!("{declaration} fn main() {{}}"), "pub fn bad() {}");
        let output = f.command(&["check", "--offline"]);
        assert!(!output.status.success(), "{name}: declaration was accepted");
        assert!(!output.stderr.is_empty(), "{name}: diagnostic missing");
    }
}

#[test]
fn rust_panic_stops_before_continuation_without_crossing_ffi() {
    let f = Fixture::new();
    f.sources(
        "extern rust { fn fail(); } fn main() { fail(); println(987654321); }",
        "pub fn fail() { panic!(\"bridge panic fixture\"); }",
    );
    let output = f.command(&["run", "--offline"]);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("RustPanic"), "{error}");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("987654321"));
    assert!(
        !error.contains("panic in a function that cannot unwind"),
        "{error}"
    );
}

#[test]
fn scheduler_task_rejects_foreign_call_before_running_rust_body() {
    let f = Fixture::new();
    f.sources(
        "extern rust { fn touch(); } async fn main() { touch(); println(987654321); }",
        "pub fn touch() { println!(\"RUST_BODY_MUST_NOT_RUN\"); }",
    );
    let output = f.command(&["run", "--offline"]);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("rust_bridge_worker_blocking"), "{error}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("RUST_BODY_MUST_NOT_RUN"));
    assert!(!stdout.contains("987654321"));
}

#[test]
fn bindings_include_project_identity_namespace_and_signature() {
    let original = RustBridgeSymbol::new("add".into(), vec![Scalar::I64, Scalar::I64], Scalar::I64);
    let a = original.bind("project-a");
    assert_eq!(a.abi_symbol, original.bind("project-a").abi_symbol);
    assert_ne!(a.abi_symbol, original.bind("project-b").abi_symbol);
    assert_ne!(
        a.abi_symbol,
        RustBridgeSymbol::new(
            "math::add".into(),
            vec![Scalar::I64, Scalar::I64],
            Scalar::I64
        )
        .bind("project-a")
        .abi_symbol
    );
    assert_ne!(
        a.abi_symbol,
        RustBridgeSymbol::new("add".into(), vec![Scalar::F64, Scalar::F64], Scalar::I64)
            .bind("project-a")
            .abi_symbol
    );
    assert_ne!(
        a.abi_symbol,
        RustBridgeSymbol::new("add".into(), vec![Scalar::I64, Scalar::I64], Scalar::F64)
            .bind("project-a")
            .abi_symbol
    );
}

#[test]
fn adapter_counts_scale_once_per_declaration() {
    let mut previous = None;
    for count in [16, 64, 256, 1024] {
        let symbols: Vec<_> = (0..count)
            .map(|i| {
                RustBridgeSymbol::new(
                    format!("f{i:04}"),
                    vec![Scalar::I64, Scalar::Bool],
                    Scalar::F64,
                )
                .bind("fixture")
            })
            .collect();
        let source = wrappers(&symbols, Path::new("ffi.rs"), 1);
        assert_eq!(source.matches("#[unsafe(no_mangle)]").count(), count);
        assert_eq!(source.matches("let adapter:").count(), count);
        assert_eq!(source.matches("std::panic::catch_unwind").count(), count);
        let empty_len = wrappers(&[], Path::new("ffi.rs"), 1).len();
        let bytes_per_wrapper = (source.len() - empty_len) / count;
        assert_eq!(source.len(), empty_len + bytes_per_wrapper * count);
        if let Some(previous) = previous {
            assert_eq!(bytes_per_wrapper, previous);
        }
        previous = Some(bytes_per_wrapper);
        eprintln!(
            "rust_scalar_wrappers declarations={count} wrappers={count} generated_bytes={} bytes_per_wrapper={bytes_per_wrapper}",
            source.len()
        );
    }
}

#[test]
fn abi_revision_mismatch_is_rejected_before_cargo() {
    let f = Fixture::new();
    let manifest = ProjectManifest::load(&f.root.join("project.toml")).unwrap();
    let mut options = BridgeOptions::new(BuildMode::Debug).unwrap();
    options.cache_root = f.temp.join("cache");
    options.offline = true;
    options.symbols = vec![
        RustBridgeSymbol::new("add".into(), vec![Scalar::I64, Scalar::I64], Scalar::I64)
            .bind("fixture"),
    ];
    f.sources(&format!("{DECL} fn main() {{}}"), ADD);
    let built = build_bridge(&manifest, &f.root, &options, false)
        .unwrap()
        .unwrap();
    let archive = built.staticlib.clone().unwrap();
    let before = fs::read(&archive).unwrap();
    let generated = fs::read_to_string(built.directory.join("src/lib.rs")).unwrap();
    assert!(!generated.contains(env!("CARGO_MANIFEST_DIR")));
    assert_eq!(
        fs::read(built.directory.join("src/willow_bridge_abi.rs")).unwrap(),
        include_bytes!("../crates/willow_abi/src/ffi.rs")
    );
    drop(built);
    options.cargo = f.temp.join("must-not-execute-cargo");
    options.abi_revision = "999999".into();
    let error = build_bridge(&manifest, &f.root, &options, false).unwrap_err();
    assert!(
        format!("{error:#}").contains("rust_bridge_abi_mismatch"),
        "{error:#}"
    );
    assert_eq!(before, fs::read(archive).unwrap());
}

#[test]
fn namespaced_function_value_executes() {
    let f = Fixture::new();
    f.sources("extern rust math { fn add(a: i64, b: i64) -> i64; } fn main() { let operation = math::add; println(operation(20, 22)); }", ADD);
    let output = f.successful(&["run", "--offline"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n");
}

#[test]
fn direct_and_indirect_foreign_calls_are_rejected_under_mutex_lock() {
    for (name, declaration, binding, call) in [
        ("plain_direct", "extern rust { fn touch(); }", "", "touch()"),
        (
            "namespace_direct",
            "extern rust math { fn touch(); }",
            "",
            "math::touch()",
        ),
        (
            "plain_indirect",
            "extern rust { fn touch(); }",
            "let operation = touch;",
            "operation()",
        ),
        (
            "namespace_indirect",
            "extern rust math { fn touch(); }",
            "let operation = math::touch;",
            "operation()",
        ),
    ] {
        let f = Fixture::new();
        f.sources(&format!("{declaration} async fn main() {{ let m = Mutex::new(0); {binding} lock m as value {{ {call}; }} }}"), "pub fn touch() {}");
        let output = f.command(&["check", "--offline"]);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{name}: lock-held foreign call accepted"
        );
        // Function values cannot be retained in an async frame, so indirect
        // forms fail earlier at Send validation; direct forms exercise effects.
        let code = if name.ends_with("indirect") {
            "E2402"
        } else {
            "E2604"
        };
        assert!(error.contains(code), "{name}: {error}");
    }
}

#[test]
fn imported_modules_keep_same_named_bridge_declarations_distinct() {
    for differing_signature in [false, true] {
        let f = Fixture::new();
        f.sources("import first; import second; fn main() { println(first::answer()); println(second::answer()); }", "pub fn identity<T>(value: T) -> T { value }");
        fs::write(f.root.join("src/first.wi"), "extern rust { fn identity(value: i64) -> i64; } pub fn answer() -> i64 { return identity(42); }").unwrap();
        let second = if differing_signature {
            "extern rust { fn identity(value: f64) -> f64; } pub fn answer() -> bool { return identity(2.5) == 2.5; }"
        } else {
            "extern rust { fn identity(value: i64) -> i64; } pub fn answer() -> i64 { return identity(43); }"
        };
        fs::write(f.root.join("src/second.wi"), second).unwrap();
        let output = f.successful(&["run", "--offline"]);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            if differing_signature {
                "42\ntrue\n"
            } else {
                "42\n43\n"
            }
        );
    }
}

#[test]
fn mixed_scalar_arguments_cross_register_and_stack_abi_boundaries() {
    let f = Fixture::new();
    // Nine integers and nine floating-point values overflow native argument
    // register banks on each supported ABI; bool crosses the boundary too.
    let params: Vec<_> = (0..9)
        .flat_map(|i| [format!("i{i}: i64"), format!("f{i}: f64")])
        .chain(["flag: bool".into()])
        .collect();
    let args: Vec<_> = (0..9)
        .flat_map(|i| [(i + 1).to_string(), format!("{}.5", i + 1)])
        .chain(["true".into()])
        .collect();
    let sum = (0..9)
        .map(|i| format!("i{i} as f64 + f{i}"))
        .collect::<Vec<_>>()
        .join(" + ");
    f.sources(
        &format!(
            "extern rust {{ fn mixed({}) -> f64; }} fn main() {{ println(mixed({}) == 95.5); }}",
            params.join(", "),
            args.join(", ")
        ),
        &format!(
            "pub fn mixed({}) -> f64 {{ {sum} + if flag {{ 1.0 }} else {{ 0.0 }} }}",
            params.join(", ")
        ),
    );
    let output = f.successful(&["run", "--offline"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "true\n");
}

#[test]
fn rust_panic_payload_drop_is_not_executed_at_ffi_boundary() {
    let f = Fixture::new();
    f.sources(
        "extern rust { fn fail(); } fn main() { fail(); println(987654321); }",
        r#"
        struct Payload;
        impl Drop for Payload {
            fn drop(&mut self) {
                eprintln!("PAYLOAD_DROP_MUST_NOT_RUN");
                panic!("payload destructor panic");
            }
        }
        pub fn fail() { std::panic::panic_any(Payload); }
    "#,
    );
    let output = f.command(&["run", "--offline"]);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("RustPanic"), "{error}");
    assert!(!error.contains("PAYLOAD_DROP_MUST_NOT_RUN"), "{error}");
    assert!(
        !error.contains("panic in a destructor during cleanup"),
        "{error}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("987654321"));
}

#[test]
fn ai_snapshot_exposes_scalar_signature_symbol_and_foreign_effects() {
    let f = Fixture::new();
    f.sources(
        &format!("{DECL} fn main() {{ println(add(20, 22)); }}"),
        ADD,
    );
    let entry = f.root.join("src/main.wi");
    let snapshot = willow_compiler::CompilerSession::new(
        entry.to_str().unwrap(),
        "",
        &willow_compiler::CompilerOptions::debug(),
        Some(f.root.clone()),
    )
    .analysis_with_emitter(&mut willow_compiler::diagnostics::HumanEmitter)
    .unwrap();
    snapshot.validate().unwrap();
    assert_eq!(snapshot.semantic.rust_bridges.len(), 1);
    let symbol = &snapshot.semantic.rust_bridges[0];
    assert_eq!(symbol.willow_function, "add");
    assert_eq!(symbol.input_types, [Scalar::I64, Scalar::I64]);
    assert_eq!(symbol.output_type, Scalar::I64);
    assert!(symbol.abi_symbol.starts_with("__willow_rust_"));
    assert!(!symbol.rust_crate.is_empty());
    assert_ne!(symbol.effects & willow_abi::ffi::FOREIGN_CALL, 0);
    for effect in [
        willow_abi::RuntimeEffects::MAY_PANIC,
        willow_abi::RuntimeEffects::MAY_BLOCK,
        willow_abi::RuntimeEffects::MAY_ALLOCATE,
    ] {
        assert_ne!(symbol.effects & effect.bits(), 0);
    }
    let restored: willow_compiler::ai::Snapshot =
        serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
    assert_eq!(
        restored.semantic.rust_bridges[0].abi_symbol,
        symbol.abi_symbol
    );
}

#[test]
fn independent_project_archives_keep_same_named_rust_functions_distinct() {
    let first = Fixture::new();
    let second = Fixture::new();
    first.sources("fn main() {}", ADD);
    second.sources(
        "fn main() {}",
        "pub fn add(a: i64, b: i64) -> i64 { a - b }",
    );
    let symbol = RustBridgeSymbol::new("add".into(), vec![Scalar::I64, Scalar::I64], Scalar::I64);
    let first_symbol = symbol.bind(first.root.to_str().unwrap());
    let second_symbol = symbol.bind(second.root.to_str().unwrap());
    assert_ne!(first_symbol.abi_symbol, second_symbol.abi_symbol);
    let mut options = BridgeOptions::new(BuildMode::Debug).unwrap();
    options.offline = true;
    options.cache_root = first.temp.join("bridge-cache");
    options.symbols = vec![first_symbol.clone()];
    let toolchain = willow_compiler::toolchain::rust_bridge::RustToolchain::detect(
        options.cargo.clone(),
        options.rustc.clone(),
        &first.root,
    )
    .unwrap();
    let manifest = ProjectManifest::load(&first.root.join("project.toml")).unwrap();
    let first_build = build_bridge(&manifest, &first.root, &options, false)
        .unwrap()
        .unwrap();
    options.cache_root = second.temp.join("bridge-cache");
    options.symbols = vec![second_symbol.clone()];
    let manifest = ProjectManifest::load(&second.root.join("project.toml")).unwrap();
    let second_build = build_bridge(&manifest, &second.root, &options, false)
        .unwrap()
        .unwrap();
    // Inspect the actual archive index, not just the IR's chosen name.
    use sha2::{Digest, Sha256};
    for (build, fixture, symbol) in [
        (&first_build, &first, &first_symbol),
        (&second_build, &second, &second_symbol),
    ] {
        let bytes = fs::read(build.staticlib.as_ref().unwrap()).unwrap();
        let archive = object::read::archive::ArchiveFile::parse(&*bytes).unwrap();
        let prefix = format!(
            "__willow_rust_{:x}_add_",
            Sha256::digest(fixture.root.to_str().unwrap().as_bytes())
        );
        let names: Vec<_> = archive
            .symbols()
            .unwrap()
            .unwrap()
            .map(|symbol| {
                let symbol = symbol.unwrap();
                let name = std::str::from_utf8(symbol.name()).unwrap();
                if cfg!(target_vendor = "apple") {
                    name.strip_prefix('_').unwrap_or(name)
                } else {
                    name
                }
            })
            .filter(|name| name.starts_with("__willow_rust_"))
            .collect();
        assert_eq!(names, [symbol.abi_symbol.as_str()]);
        let suffix = names[0]
            .strip_prefix(&prefix)
            .expect("required bridge-package hash and readable symbol");
        assert_eq!(suffix.len(), 64);
        assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
    let driver = first.temp.join("archive_driver.rs");
    // These runtime stubs isolate adapter/archive symbol identity. The Willow
    // runtime's scheduler and panic behavior are exercised by separate tests.
    fs::write(
        &driver,
        format!(
            r#"
        unsafe extern "C" {{
            #[link_name = "{}"] fn first(a: i64, b: i64) -> i64;
            #[link_name = "{}"] fn second(a: i64, b: i64) -> i64;
        }}
        #[unsafe(no_mangle)]
        pub extern "C" fn willow_rust_bridge_enter(revision: u32) {{ assert_eq!(revision, {}); }}
        #[unsafe(no_mangle)]
        pub extern "C" fn willow_rust_bridge_panic() -> ! {{ std::process::abort() }}
        fn main() {{
            let (a, b) = unsafe {{ (first(20, 22), second(20, 22)) }};
            assert_eq!(a, 42);
            assert_eq!(b, -2);
            println!("{{a}} {{b}}");
        }}
    "#,
            first_symbol.abi_symbol,
            second_symbol.abi_symbol,
            willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION
        ),
    )
    .unwrap();
    let executable = first
        .temp
        .join(format!("archive_driver{}", std::env::consts::EXE_SUFFIX));
    let mut command = Command::new(&toolchain.rustc);
    command
        .arg("--edition=2024")
        .arg(&driver)
        .arg("-o")
        .arg(&executable);
    for archive in [
        first_build.staticlib.as_ref().unwrap(),
        second_build.staticlib.as_ref().unwrap(),
    ] {
        let mut argument = std::ffi::OsString::from("link-arg=");
        argument.push(archive);
        command.arg("-C").arg(argument);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "archive driver link: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new(&executable).output().unwrap();
    assert!(
        output.status.success(),
        "archive driver run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42 -2\n");
}

#[test]
fn imported_foreign_call_is_rejected_under_mutex_lock() {
    let f = Fixture::new();
    f.sources("import foreign; async fn main() { let m = Mutex::new(0); lock m as value { foreign::touch(); } }", "pub fn touch() {}");
    fs::write(f.root.join("src/foreign.wi"), "extern rust { fn touch(); }").unwrap();
    let output = f.command(&["check", "--offline"]);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "imported lock-held foreign call accepted"
    );
    assert!(error.contains("E2604"), "{error}");
}

#[test]
fn indirect_bool_uses_c_abi_in_debug_and_release() {
    for release in [false, true] {
        let f = Fixture::new();
        f.sources("extern rust { fn invert(value: bool) -> bool; fn choose(value: bool, a: i64, b: f64) -> f64; } fn main() { let invert_fn = invert; let choose_fn = choose; println(invert_fn(false)); println(invert_fn(true)); println(choose_fn(false, 42, 2.5) == 2.5); println(choose_fn(true, 42, 2.5) == 42.0); }",
            "pub fn invert(value: bool) -> bool { !value } pub fn choose(value: bool, a: i64, b: f64) -> f64 { if value { a as f64 } else { b } }");
        let args: &[&str] = if release {
            &["run", "--offline", "--release"]
        } else {
            &["run", "--offline"]
        };
        let output = f.successful(args);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "true\nfalse\ntrue\ntrue\n"
        );
    }
}

#[test]
fn boolean_abi_shims_are_emitted_once_per_declaration() {
    use object::{Object, ObjectSymbol};
    for count in [8, 32, 128] {
        let f = Fixture::new();
        let declarations: String = (0..count)
            .map(|i| format!("fn b{i:04}(value: bool) -> bool;\n"))
            .collect();
        let rust: String = (0..count)
            .map(|i| format!("pub fn b{i:04}(value: bool) -> bool {{ !value }}\n"))
            .collect();
        f.sources(&format!("extern rust {{ {declarations} fn add(a: i64, b: i64) -> i64; }} fn main() {{ println(b0000(false)); println(add(20, 22)); }}"), &format!("{rust} {ADD}"));
        f.successful(&["build", "--offline", "-o", "app"]);
        let extension = if cfg!(all(windows, target_env = "msvc")) {
            "obj"
        } else {
            "o"
        };
        let bytes = fs::read(f.root.join(format!("app.{extension}"))).unwrap();
        let object = object::File::parse(&*bytes).unwrap();
        let shims = object
            .symbols()
            .filter(|symbol| {
                symbol.is_definition()
                    && symbol.name().is_ok_and(|name| {
                        name.contains("_willow_rust_") && name.ends_with("_willow")
                    })
            })
            .count();
        assert_eq!(shims, count);
        eprintln!(
            "rust_bool_shims declarations={} bool_declarations={count} shims={shims}",
            count + 1
        );
    }
}

#[test]
fn unresolved_native_reference_reports_bridge_link_error() {
    let f = Fixture::new();
    f.sources("extern rust { fn touch(); } fn main() { touch(); }", "unsafe extern \"C\" { fn willow_r2_missing_native_symbol(); } pub fn touch() { unsafe { willow_r2_missing_native_symbol(); } }");
    let output = f.command(&["build", "--offline", "-o", "app"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("rust_bridge_link_error"));
}

#[test]
fn rust_check_validates_declared_adapters_with_json_and_human_diagnostics() {
    for (rust, kind, code) in [
        ("pub fn other() {}", "rust_bridge_symbol_missing", "E0425"),
        (
            "pub fn add(a: f64, b: f64) -> i64 { (a + b) as i64 }",
            "rust_bridge_signature_mismatch",
            "E0308",
        ),
    ] {
        let f = Fixture::new();
        f.sources(
            &format!("{DECL} fn main() {{ println(add(20, 22)); }}"),
            rust,
        );
        let output = f.command(&["rust", "check", "--offline", "--format", "json"]);
        assert!(!output.status.success(), "rust check accepted {kind}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["kind"], kind);
        assert_eq!(
            output.stdout.iter().filter(|&&byte| byte == b'\n').count(),
            1
        );
        let diagnostic = value["error"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["message"]["code"]["code"] == code)
            .unwrap();
        assert!(
            diagnostic["message"]["rendered"]
                .as_str()
                .unwrap()
                .contains("add")
        );
        assert!(
            diagnostic["message"]["spans"]
                .as_array()
                .unwrap()
                .iter()
                .any(|span| span["is_primary"] == true && span["line_start"].as_u64().unwrap() > 0)
        );
        for args in [
            &["rust", "check", "--offline"][..],
            &["build", "--offline", "-o", "app"][..],
        ] {
            let output = f.command(args);
            assert!(!output.status.success());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(kind)
                    && stderr.contains(code)
                    && stderr.contains("add")
                    && stderr.contains("src/lib.rs:"),
                "{stderr}"
            );
            if code == "E0308" {
                assert!(
                    stderr.contains("let adapter: fn(i64,i64,) -> i64"),
                    "{stderr}"
                );
            }
        }
    }
}

#[test]
fn rust_check_collects_unreferenced_project_module_declarations() {
    let f = Fixture::new();
    f.sources("fn main() {}", "pub fn other() {}");
    fs::write(
        f.root.join("src/foreign.wi"),
        "extern rust { fn absent() -> i64; }",
    )
    .unwrap();
    let output = f.command(&["rust", "check", "--offline", "--format", "json"]);
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["kind"], "rust_bridge_symbol_missing");
    assert!(value["error"]["diagnostics"].to_string().contains("absent"));
}

#[test]
fn rust_check_success_uses_custom_entry_and_frozen_adapter_cache() {
    let f = Fixture::new();
    f.sources("fn main() {}", ADD);
    let manifest = f.root.join("project.toml");
    let contents = fs::read_to_string(&manifest)
        .unwrap()
        .replace("[project]", "[project]\nentry = \"src/entry.wi\"");
    fs::write(manifest, contents).unwrap();
    fs::write(
        f.root.join("src/entry.wi"),
        format!("{DECL} fn main() {{ println(add(20, 22)); }}"),
    )
    .unwrap();
    // The old default entry must not be discovered as a second main.
    fs::remove_file(f.root.join("src/main.wi")).unwrap();
    for mode in ["--offline", "--frozen"] {
        let output = f.successful(&["rust", "check", mode, "--format", "json"]);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ok"], true);
        let manifest = Path::new(value["bridge"]["manifest"].as_str().unwrap());
        let source = fs::read_to_string(manifest.parent().unwrap().join("src/lib.rs")).unwrap();
        assert_eq!(source.matches("let adapter:").count(), 1);
        assert!(source.contains("__willow_rust_"));
    }
}

#[test]
fn rust_check_does_not_skip_a_missing_willow_entry() {
    let f = Fixture::new();
    f.sources("fn main() {}", ADD);
    fs::remove_file(f.root.join("src/main.wi")).unwrap();
    let output = f.command(&["rust", "check", "--offline", "--format", "json"]);
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value.to_string().contains("cannot read"), "{value}");
}
