use super::*;

// ── Task<T> Send: interface-value capture diagnostics (willow-dgwo.5) ────────
// An interface value passed to an async fn follows the interface contract, so a
// plain interface (neither Send nor Sync) → E2404; an interface that is Send but
// not Sync → E2405; an interface that `extends Sync` is accepted. Gated by the
// the always-on multi-worker data-race check.
#[test]
fn test_dgwo5_plain_interface_arg_e2404() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal { pub fn speak(self) -> String { return "woof"; } }
async fn run(a: Animal) -> i64 { await sleep(1); return 0; }
async fn main() { println(await run(new Dog())); }
"#,
    );
    assert!(!ok);
    assert!(stderr.contains("error[E2404]"), "{stderr}");
    assert!(stderr.contains("is not `Send`"), "{stderr}");
}

#[test]
fn test_dgwo5_send_only_interface_arg_e2405() {
    // `extends Send` makes it Send but not Sync; a captured GC ref needs Sync.
    let (ok, stderr) = compile_with_data_race_check(
        r#"
interface Job extends Send { fn go(self) -> i64; }
class J implements Job { pub fn go(self) -> i64 { return 1; } }
async fn run(j: Job) -> i64 { await sleep(1); return j.go(); }
async fn main() { println(await run(new J())); }
"#,
    );
    assert!(!ok);
    assert!(stderr.contains("error[E2405]"), "{stderr}");
    assert!(stderr.contains("is not `Sync`"), "{stderr}");
}

#[test]
fn test_dgwo5_sync_interface_arg_accepted() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
interface Job extends Sync { fn go(self) -> i64; }
class J implements Job { pub fn go(self) -> i64 { return 9; } }
async fn run(j: Job) -> i64 { await sleep(1); return j.go(); }
async fn main() { println(await run(new J())); }
"#,
    );
    assert!(ok, "interface extends Sync should be accepted: {stderr}");
}

#[test]
fn test_dgwo5_interface_arg_rejected_by_default() {
    let (ok, stderr) = compile_with_compiler_env(
        r#"
interface Animal { fn speak(self) -> i64; }
class Dog implements Animal { pub fn speak(self) -> i64 { return 7; } }
async fn run(a: Animal) -> i64 { await sleep(1); return a.speak(); }
async fn main() { println(await run(new Dog())); }
"#,
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("error[E2404]"), "{stderr}");
}
