use super::*;

// ── Debug call-chain stack traces on panic (willow-992h) ─────────────────────

#[test]
fn callchain_01_nested_panic_prints_ordered_chain() {
    // deeper() <- helper() <- main(): the panic prints the active call chain,
    // most recent call first, with the call-site file:line:col of each frame.
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn deeper() {
    panic("boom");
}
fn helper() {
    deeper();
}
fn main() {
    helper();
}
"#,
    );
    assert!(!ok, "program should abort on panic");
    assert!(
        out.contains("runtime panic: boom"),
        "missing panic line: {out}"
    );
    assert!(
        out.contains("call stack (most recent call first):"),
        "missing call stack header: {out}"
    );
    // Frame 0 is the innermost call (deeper), frame 1 is helper.
    let zero = out
        .find("0: deeper")
        .unwrap_or_else(|| panic!("no frame 0: {out}"));
    let one = out
        .find("1: helper")
        .unwrap_or_else(|| panic!("no frame 1: {out}"));
    assert!(zero < one, "frames out of order: {out}");
    // Each frame records its call site, not the callee body.
    assert!(
        out.contains("0: deeper at "),
        "frame 0 missing location: {out}"
    );
    assert!(
        out.contains("1: helper at "),
        "frame 1 missing location: {out}"
    );
}

#[test]
fn callchain_02_direct_panic_in_main_has_no_chain() {
    // main is the entry (not called via the instrumented path), so a panic
    // directly in main prints no call-stack section.
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() {
    panic("top");
}
"#,
    );
    assert!(!ok);
    assert!(out.contains("runtime panic: top"), "{out}");
    assert!(
        !out.contains("call stack"),
        "main-only panic should have no chain: {out}"
    );
}

#[test]
fn callchain_03_release_build_omits_chain() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_callchain_rel_{}.wi", id));
    let bin_path = temp_path(format!("willow_callchain_rel_{}", id));
    fs::write(
        &src_path,
        "fn inner() { panic(\"x\"); }\nfn main() { inner(); }\n",
    )
    .unwrap();

    let compiler = env!("CARGO_BIN_EXE_willow");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");
    assert!(status.success(), "release build failed");

    let out = Command::new(&bin_path).output().expect("run failed");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(combined.contains("runtime panic: x"), "{combined}");
    assert!(
        !combined.contains("call stack"),
        "release should omit call chain: {combined}"
    );
}

#[test]
fn callchain_04_three_levels() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn c() { panic("deep"); }
fn b() { c(); }
fn a() { b(); }
fn main() { a(); }
"#,
    );
    assert!(!ok);
    let f0 = out
        .find("0: c")
        .unwrap_or_else(|| panic!("{}", out.to_string()));
    let f1 = out
        .find("1: b")
        .unwrap_or_else(|| panic!("{}", out.to_string()));
    let f2 = out
        .find("2: a")
        .unwrap_or_else(|| panic!("{}", out.to_string()));
    assert!(f0 < f1 && f1 < f2, "chain order wrong: {out}");
}

#[test]
fn callchain_05_method_call_in_chain() {
    // A panic inside a class method shows the method frame above its caller
    // (willow-phx3).
    let (out, ok) = compile_and_run_check_exit(
        r#"
class Worker {
    pub fn run(self) {
        panic("worker failed");
    }
}
fn helper(w: Worker) {
    w.run();
}
fn main() {
    let w = new Worker();
    helper(w);
}
"#,
    );
    assert!(!ok);
    assert!(out.contains("runtime panic: worker failed"), "{out}");
    let run = out
        .find("0: run")
        .unwrap_or_else(|| panic!("no method frame: {out}"));
    let helper = out
        .find("1: helper")
        .unwrap_or_else(|| panic!("no caller frame: {out}"));
    assert!(run < helper, "method frame must be innermost: {out}");
}

#[test]
fn async_frame_shadowed_locals_get_distinct_slots() {
    // An outer GC-managed local and a nested shadowed local of the SAME name,
    // both live across awaits, must occupy distinct async-frame slots — the
    // inner write must not clobber the outer (willow-lpn.11). Run under GC
    // stress so a mis-traced/aliased slot is caught.
    let src = r#"
async fn work() -> String {
    let s = "outer";
    await sleep(1);
    if s == "outer" {
        let s = "inner";
        await sleep(1);
        println(s);
    }
    await sleep(1);
    return s;
}

async fn main() {
    let r = await work();
    println(r);
}
"#;
    let (out, ok) = compile_and_run_gc_stress(src);
    assert!(ok, "async shadowing program must run: {out}");
    assert_eq!(
        out, "inner\nouter\n",
        "outer local was clobbered by inner: {out}"
    );
}

#[test]
fn generic_interface_neg_08_two_instantiations_unsatisfiable_rejected() {
    // A class MAY implement two instantiations of the same generic interface
    // (willow-1js.6), but only when one method body can satisfy every
    // instantiation. Here `get(self) -> T` cannot return both `i64` and
    // `String`, so conformance rejects it (E0417), not the duplicate check.
    assert!(expect_compile_error(
        r#"
interface Container<T> { fn get(self) -> T; }
class C implements Container<i64>, Container<String> {
    pub fn get(self) -> i64 { return 1; }
}
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_09_phantom_two_instantiations_allowed() {
    // When the interface's type parameter appears in no method signature
    // (a phantom/marker parameter), a class can implement several
    // instantiations at once; they share one identical vtable (willow-1js.6).
    let (out, ok) = compile_and_run(
        r#"
interface Tagged<T> { fn tag_name(self) -> String; }
class Item implements Tagged<i64>, Tagged<String> {
    pub fn tag_name(self) -> String { return "item"; }
}
fn use_int(t: Tagged<i64>) -> String { return t.tag_name(); }
fn use_str(t: Tagged<String>) -> String { return t.tag_name(); }
fn main() {
    let it = new Item();
    let a: Tagged<i64> = it;
    let b: Tagged<String> = it;
    println(use_int(a));
    println(use_str(b));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "item\nitem\n");
}

#[test]
fn generic_interface_10_exact_duplicate_instantiation_rejected() {
    // The same instantiation listed twice is still a duplicate (E0414).
    assert!(expect_compile_error(
        r#"
interface Tagged<T> { fn tag_name(self) -> String; }
class Item implements Tagged<i64>, Tagged<i64> {
    pub fn tag_name(self) -> String { return "item"; }
}
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_11_phantom_three_instantiations_allowed() {
    // More than two instantiations of a phantom-parameter interface.
    let (out, ok) = compile_and_run(
        r#"
interface Marker<T> { fn kind(self) -> i64; }
class Node implements Marker<i64>, Marker<String>, Marker<bool> {
    pub fn kind(self) -> i64 { return 7; }
}
fn main() {
    let n: Marker<bool> = new Node();
    println(n.kind());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}
