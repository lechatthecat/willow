//! Async foreign ownership, scheduler progress and diagnostics.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
struct Fixture {
    temp: PathBuf,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = std::env::temp_dir().join(format!(
            "willow-async-bridge-{}-{}",
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
        self.command_env(args, &[])
    }
    fn command_env(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        let original_home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .unwrap();
        Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.root)
            .args(args)
            .envs(environment.iter().copied())
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

#[test]
fn async_bridge_scalar_smoke() {
    let f = Fixture::new();
    f.sources("extern rust { async fn add(a: i64, b: i64) -> i64; } async fn main() { println(await add(20,22)); }", "pub async fn add(a:i64,b:i64)->i64 { a+b }");
    let result = f.successful(&["run", "--offline"]);
    assert_eq!(String::from_utf8_lossy(&result.stdout), "42\n");
}

#[test]
fn async_bridge_owned_values_under_moving_gc() {
    let f = Fixture::new();
    f.sources(
        r#"
import std::collections;
extern rust {
    async fn integer(x: i64) -> i64;
    async fn float(x: f64) -> f64;
    async fn boolean(x: bool) -> bool;
    async fn touch();
    async fn text(x: String) -> String;
    async fn bytes(x: Array<i64>) -> Array<i64>;
    async fn texts(x: Array<String>) -> Array<String>;
    async fn floats(x: Array<f64>) -> Array<f64>;
    async fn bools(x: Array<bool>) -> Array<bool>;
    async fn optional(x: Option<i64>) -> Option<i64>;
    async fn nested(x: Option<Option<String>>) -> Option<Option<String>>;
    async fn pair(x: Result<i64,bool>) -> Result<i64,bool>;
    async fn result(x: Result<String,String>) -> Result<String,String>;
    opaque Resource;
    async fn make() -> Resource;
    async fn read(r: Resource) -> i64;
    class Packet { pub text: String; pub number: i64; }
    async fn packet(p: Packet) -> Packet;
}
async fn main() {
    println(await integer(-9223372036854775807) == -9223372036854775807);
    println(await float(2.5) == 2.5);
    println(await boolean(true));
    println(!(await boolean(false)));
    await touch();
    println(await text("héllo") == "héllo");
    println(await text("") == "");
    let b = await bytes([0,128,255]); println(b[2] == 255);
    let s = await texts(["a","日本語"]); println(s[1] == "日本語");
    let floats = await floats([1.25,2.5]); println(floats[1] == 2.5);
    let bools = await bools([false,true]); println(bools[1]);
    println((await optional(Some(42))).unwrap() == 42);
    println((await optional(None)).is_none());
    println((await nested(Some(Some("nested")))).unwrap().unwrap() == "nested");
    println((await pair(Ok(42))).unwrap() == 42);
    println((await pair(Err(true))).is_err());
    println((await result(Ok("ok"))).unwrap() == "ok");
    println((await result(Err("error"))).is_err());
    let r = await make(); println(await read(r) == 42);
    let p = await packet(new Packet("record",42)); println(p.text == "record" && p.number == 42);
}
"#,
        r#"
pub async fn integer(x:i64)->i64{x}
pub async fn float(x:f64)->f64{x}
pub async fn boolean(x:bool)->bool{x}
pub async fn touch(){}
pub async fn text(x:String)->String{x}
pub async fn bytes(x:Vec<u8>)->Vec<u8>{x}
pub async fn texts(x:Vec<String>)->Vec<String>{x}
pub async fn floats(x:Vec<f64>)->Vec<f64>{x}
pub async fn bools(x:Vec<bool>)->Vec<bool>{x}
pub async fn optional(x:Option<i64>)->Option<i64>{x}
pub async fn nested(x:Option<Option<String>>)->Option<Option<String>>{x}
pub async fn pair(x:Result<i64,bool>)->Result<i64,bool>{x}
pub async fn result(x:Result<String,String>)->Result<String,String>{x}
pub struct Resource(i64);
pub async fn make()->Box<Resource>{Box::new(Resource(42))}
pub async fn read(r:crate::AsyncHandle<Resource>)->i64{r.0}
pub async fn packet(p:crate::record_owned::Packet)->crate::record_outputs::Packet {
    crate::records::Packet {text:p.text,number:p.number}
}
"#,
    );
    let result = f.command_env(
        &["run", "--offline"],
        &[("WILLOW_GC_STRESS", "alloc,scheduler,relocate")],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&result.stdout), "true\n".repeat(19));
}

#[test]
fn async_bridge_diagnostics_and_query() {
    let f = Fixture::new();
    let decl = "extern rust { async fn touch(); }";
    for (body, code) in [
        ("fn main() { touch(); }", "E0801"),
        ("fn helper() { touch(); } fn main() { helper(); }", "E0801"),
        ("async fn main() { let f = || { touch(); }; }", "E0801"),
        (
            "@effects(no_suspend) async fn helper() { let t = touch(); } async fn main() { await helper(); }",
            "E2612",
        ),
        ("async fn main() { let f = touch; }", "E1012"),
        (
            "async fn main() { let m = Mutex::new(0); lock m as v { let t = touch(); } }",
            "E2604",
        ),
        (
            "async fn helper() { await touch(); } async fn main() { let m = Mutex::new(0); lock m as v { await helper(); } }",
            "E2604",
        ),
        (
            "@effects(no_suspend) async fn helper() { await touch(); } async fn main() { await helper(); }",
            "E2610",
        ),
    ] {
        f.sources(&format!("{decl} {body}"), "pub async fn touch() {}");
        let output = f.command(&["check", "."]);
        assert!(!output.status.success(), "{body}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(code),
            "{body}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    f.sources(
        "extern rust native { async fn touch(); } fn main() { native::touch(); }",
        "pub async fn touch() {}",
    );
    let output = f.command(&["check", "."]);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("E0801"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    f.sources(
        &format!("{decl} async fn main() {{ await touch(); }}"),
        "pub async fn touch() {}",
    );
    let output = f.successful(&["query", "rust-bridge", "--offline", "--format", "json"]);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["bridges"][0]["async"], true);
    let effects = json["bridges"][0]["effects"].as_u64().unwrap() as u8;
    assert_ne!(effects & willow_abi::RuntimeEffects::MAY_SUSPEND.bits(), 0);
    assert_eq!(effects & willow_abi::RuntimeEffects::MAY_BLOCK.bits(), 0);
}

#[test]
fn async_bridge_worker_progress_and_cancel_drop() {
    let f = Fixture::new();
    f.sources(r#"
extern rust {
    async fn pending() -> i64;
    async fn started() -> bool;
    async fn drops() -> i64;
}
async fn heartbeat() -> i64 { await sleep(1); return 42; }
async fn main() {
    let task = pending();
    while !(await started()) { await sleep(1); }
    println(await heartbeat());
    task.cancel();
    println(await heartbeat());
    await task.result();
    println(await drops());
}
"#, r#"
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
static STARTED: AtomicBool = AtomicBool::new(false);
static DROPS: AtomicI64 = AtomicI64::new(0);
struct Pending;
impl std::future::Future for Pending {
    type Output = i64;
    fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<i64> {
        STARTED.store(true,Ordering::SeqCst);
        std::task::Poll::Pending
    }
}
impl Drop for Pending { fn drop(&mut self) { std::thread::sleep(std::time::Duration::from_millis(30)); DROPS.fetch_add(1,Ordering::SeqCst); } }
pub async fn pending()->i64 { Pending.await }
pub async fn started()->bool { STARTED.load(Ordering::SeqCst) }
pub async fn drops()->i64 { DROPS.load(Ordering::SeqCst) }
"#);
    let output = f.command_env(&["run", "--offline"], &[("WILLOW_WORKERS", "1")]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n42\n1\n");
}

#[test]
fn async_bridge_poll_panic_is_a_willow_error() {
    let f = Fixture::new();
    f.sources(
        "extern rust { async fn fail() -> i64; } async fn main() { println(await fail()); }",
        "pub async fn fail()->i64 { panic!(\"future failure\") }",
    );
    let output = f.command(&["run", "--offline"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("RustPanic"), "{error}");
    assert!(
        !error.contains("panic in a function that cannot unwind"),
        "{error}"
    );
}

#[test]
fn async_bridge_pull_example() {
    let f = Fixture::new();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("example/rust_async_pull");
    for file in [
        "project.toml",
        "bridge.rs",
        "native/Cargo.toml",
        "native/src/lib.rs",
        "src/main.wi",
    ] {
        fs::copy(source.join(file), f.root.join(file)).unwrap();
    }
    let output = f.successful(&["run", "--offline"]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "handled request-0\nhandled request-1\nhandled request-2\n"
    );
}

#[test]
fn async_bridge_cancel_destructor_panic_is_a_willow_error() {
    let f = Fixture::new();
    f.sources(
        r#"
extern rust { async fn pending(); async fn started() -> bool; }
async fn main() {
    let task = pending();
    while !(await started()) { await sleep(1); }
    task.cancel();
    await task.result();
}
"#,
        r#"
use std::sync::atomic::{AtomicBool,Ordering};
static STARTED: AtomicBool = AtomicBool::new(false);
struct Bad;
impl Drop for Bad { fn drop(&mut self) { panic!("cancel drop panic"); } }
pub async fn pending() {
    let _bad = Bad;
    STARTED.store(true,Ordering::SeqCst);
    std::future::pending::<()>().await;
}
pub async fn started()->bool {STARTED.load(Ordering::SeqCst)}
"#,
    );
    let output = f.command_env(&["run", "--offline"], &[("WILLOW_WORKERS", "1")]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("RustPanic"), "{error}");
    assert!(
        !error.contains("panic in a function that cannot unwind"),
        "{error}"
    );
}

#[test]
fn async_bridge_generation_counts_are_linear_and_async_identity_is_distinct() {
    use willow_compiler::rust_bridge_ir::{RustBridgeSymbol, Scalar, wrappers};
    let sync = RustBridgeSymbol::new("echo".into(), vec![Scalar::String], Scalar::String);
    let mut asynchronous = sync.clone();
    asynchronous.is_async = true;
    assert_ne!(
        sync.bind("crate").abi_symbol,
        asynchronous.bind("crate").abi_symbol
    );
    // The fixed executor/support prelude size depends on template line endings
    // (CRLF checkouts on Windows), so bound only the per-declaration growth.
    let mut base = None;
    for count in [1, 16, 256, 1024] {
        let symbols: Vec<_> = (0..count)
            .map(|i| {
                let mut symbol = asynchronous.clone();
                symbol.willow_function = format!("echo{i}");
                symbol.abi_symbol = format!("bridge{i}");
                symbol
            })
            .collect();
        let code = wrappers(
            &symbols,
            Path::new("abi.rs"),
            willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION,
        );
        assert_eq!(code.matches("let task = spawn_future(").count(), count);
        assert_eq!(code.matches(".to_owned();").count(), count);
        assert_eq!(code.matches("fn future_queue()").count(), 1);
        let base = *base.get_or_insert(code.len());
        assert!(code.len() - base <= (count - 1) * 1000);
        eprintln!(
            "async declarations={count} constructors={count} owned_copies={count} executor_copies=1 bytes={}",
            code.len()
        );
    }
}

#[test]
fn async_bridge_many_pending_futures_resume_without_lost_wakes() {
    let f = Fixture::new();
    f.sources(
        r#"
import std::collections::Array;
extern rust { async fn wait(n: i64)->i64; async fn release(); }
async fn main() {
    let tasks: Array<Task<i64>> = [];
    for n in 0..256 { tasks.push(wait(n)); }
    await release();
    let mut total = 0;
    for n in 0..256 { total = total + await tasks[n]; }
    println(total);
}
"#,
        r#"
use std::sync::{Mutex,atomic::{AtomicBool,Ordering}};
static READY: AtomicBool = AtomicBool::new(false);
static WAITERS: Mutex<Vec<std::task::Waker>> = Mutex::new(Vec::new());
pub async fn wait(n:i64)->i64 {
    std::future::poll_fn(|cx| {
        let mut waiters = WAITERS.lock().unwrap();
        if READY.load(Ordering::Acquire) { std::task::Poll::Ready(n) }
        else { waiters.push(cx.waker().clone()); std::task::Poll::Pending }
    }).await
}
pub async fn release() {
    let waiters = {
        let mut waiters = WAITERS.lock().unwrap();
        READY.store(true,Ordering::Release);
        std::mem::take(&mut *waiters)
    };
    for waiter in waiters.into_iter().rev() { waiter.wake(); }
}
"#,
    );
    for workers in ["1", "4"] {
        let result = f.command_env(
            &["run", "--offline"],
            &[
                ("WILLOW_WORKERS", workers),
                ("WILLOW_GC_STRESS", "alloc,scheduler,relocate"),
            ],
        );
        assert!(
            result.status.success(),
            "workers={workers}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&result.stdout), "32640\n");
    }
}

#[test]
fn async_bridge_rejects_borrowed_inputs_and_non_send_futures() {
    let f = Fixture::new();
    for (declaration, call, rust, diagnostic) in [
        (
            "async fn text(s: String) -> String;",
            "text(\"owned\")",
            "pub async fn text(s:&str)->String {s.to_owned()}",
            "mismatched types",
        ),
        (
            "async fn hold() -> i64;",
            "hold()",
            "pub async fn hold()->i64 {let value=std::rc::Rc::new(42); std::future::pending::<()>().await; *value}",
            "cannot be sent",
        ),
    ] {
        f.sources(
            &format!(
                "extern rust {{ {declaration} }} async fn main() {{ println(await {call}); }}"
            ),
            rust,
        );
        let output = f.command(&["build", "--offline"]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(diagnostic),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn async_bridge_owned_record_aliases_do_not_shadow_native_types() {
    let f = Fixture::new();
    f.sources(
        r#"
import std::collections::Array;
extern rust {
    class u8 { pub values: Array<i64>; }
    class Vec { pub values: Array<String>; }
    async fn bytes(v: u8) -> u8;
    async fn texts(v: Vec) -> Vec;
}
async fn main() {
    let a = await bytes(new u8([0,255]));
    let b = await texts(new Vec(["owned"]));
    println(a.values[1] == 255 && b.values[0] == "owned");
}
"#,
        r#"
pub async fn bytes(v: crate::record_owned::u8)->crate::record_outputs::u8 {
    crate::records::u8 { values:v.values }
}
pub async fn texts(v: crate::record_owned::Vec)->crate::record_outputs::Vec {
    crate::records::Vec { values:v.values }
}
"#,
    );
    let output = f.successful(&["run", "--offline"]);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "true\n");
}
