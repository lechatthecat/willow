//! Named functions and lambdas are values of one function type at runtime:
//! they mix in arrays, fields, results and task arguments, a panic raised
//! through any of them unwinds the same way, and a call through a local that
//! can only hold one named function is still a direct call.
use super::support::*;
use std::process::Command;

const DECLS: &str = "import std::collections::Array;
fn inc(x: i64) -> i64 { return x + 1; }
fn double(x: i64) -> i64 { return x * 2; }
fn apply(f: fn(i64) -> i64, x: i64) -> i64 { return f(x); }
fn twice<T>(f: fn(T) -> T, x: T) -> T { return f(f(x)); }
fn run_all(fs: Array<fn(i64) -> i64>, x: i64) -> i64 { let mut v = x; for f in fs { v = f(v); } return v; }
fn pick(capture: bool, k: i64) -> fn(i64) -> i64 { if capture { return |x: i64| x + k; } return double; }
class Stage { pub run: fn(i64) -> i64; }
interface Shared extends Sync {}
class SharedStage implements Shared { pub run: fn(i64) -> i64; }
async fn in_task(f: fn(i64) -> i64, x: i64) -> i64 { await sleep(1); return f(x); }
async fn stage_task(s: SharedStage, x: i64) -> i64 { let f = s.run; await sleep(1); return f(x); }
";

#[test]
fn named_functions_and_lambdas_interchange_at_runtime() {
    let source = format!(
        "{DECLS}
async fn main() {{
    let k = 10;
    println(apply(inc, 1));
    println(apply(|x: i64| x + k, 1));
    println(twice(double, 3));
    println(twice(|s: String| s + \"!\", \"hi\"));
    println(run_all([inc, double, |x: i64| x + k], 1));
    let f = pick(true, 5);
    let g = pick(false, 5);
    println(f(1) + g(1));
    let stage = new Stage(inc);
    stage.run = |x: i64| x * k;
    let h = stage.run;
    println(h(4));
    println(await in_task(inc, 1));
    println(await in_task(|x: i64| x + k, 1));
    println(await stage_task(new SharedStage(|x: i64| x - k), 1));
    let mut m = inc;
    m = |x: i64| x + k + k;
    println(m(0));
}}"
    );
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(&source);
        assert!(ok, "{out}");
        assert_eq!(out, "2\n11\n12\nhi!!\n14\n8\n40\n2\n11\n-9\n20\n");
    }
}

#[test]
fn panics_unwind_through_every_function_value() {
    for (call, message) in [
        ("apply(boom, 5)", "runtime panic: boom"),
        (
            "apply(|x: i64| { if x > k { panic(\"lambda boom\"); } return x; }, 5)",
            "runtime panic: lambda boom",
        ),
        ("run_all([inc, boom], 4)", "runtime panic: boom"),
    ] {
        let source = format!(
            "{DECLS}
fn boom(x: i64) -> i64 {{ if x > 2 {{ panic(\"boom\"); }} return x; }}
fn main() {{ let k = 3; println(apply(inc, 0)); println({call}); println(\"unreachable\"); }}"
        );
        let (out, ok) = compile_and_run_check_exit(&source);
        assert!(!ok, "{call}: {out}");
        assert!(out.starts_with("1\n"), "{call}: {out}");
        assert!(out.contains(message), "{call}: {out}");
        assert!(!out.contains("unreachable"), "{call}: {out}");
    }
}

#[test]
fn a_local_holding_one_named_function_is_called_directly() {
    let id = unique_test_id();
    let path = temp_path(format!("willow_fn_value_lir_{id}.wi"));
    std::fs::write(
        &path,
        "fn inc(x: i64) -> i64 { return x + 1; }
fn main() { let f = inc; let g = f; println(g(1)); let mut h = inc; h = |x: i64| x; println(h(1)); }",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["build", &path, "--emit-lir"])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    let lir = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{lir}");
    // `g` is a copy of a once-defined `f`: a direct call to `inc`.
    assert!(
        lir.contains(
            "DirectCall { callee: FunctionId { namespace: None, owner: None, name: \"inc\" }"
        ),
        "{lir}"
    );
    // `h` is reassigned, so its call stays indirect.
    assert_eq!(lir.matches("IndirectCall").count(), 1, "{lir}");
}

// A plain class field, an enum payload and a generic class field hold only
// shareable function values, so each crosses a task without any interface.
#[test]
fn shareable_slots_cross_tasks_without_a_promise() {
    let source = "import std::collections::Array;
fn inc(x: i64) -> i64 { return x + 1; }
fn double(x: i64) -> i64 { return x * 2; }
class Stage { pub run: fn(i64) -> i64; }
class Holder<T> { pub f: fn(T) -> T; }
enum Op { Map(fn(i64) -> i64), Skip }
class Pipeline { pub stages: Array<Stage>; }
async fn run_stage(s: Stage, x: i64) -> i64 { let f = s.run; await sleep(1); return f(x); }
async fn run_holder(h: Holder<i64>, x: i64) -> i64 { let f = h.f; await sleep(1); return f(x); }
async fn run_op(op: Op, x: i64) -> i64 {
    await sleep(1);
    match op { Op::Map(f) => { return f(x); } Op::Skip => { return x; } }
}
async fn run_pipeline(p: Pipeline, x: i64) -> i64 {
    let mut v = x;
    for s in p.stages { await sleep(1); let f = s.run; v = f(v); }
    return v;
}
async fn main() {
    let k = 10;
    println(await run_stage(new Stage(inc), 1));
    println(await run_stage(new Stage(|x: i64| x + k), 1));
    println(await run_holder(new Holder<i64>(double), 4));
    println(await run_op(Op::Map(|x: i64| x * k), 3));
    println(await run_op(Op::Map(inc), 3));
    println(await run_op(Op::Skip, 3));
    println(await run_pipeline(new Pipeline([new Stage(inc), new Stage(double)]), 5));
}";
    for run in [compile_and_run_gc_stress, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "2\n11\n8\n30\n4\n3\n12\n");
    }
}

// Storing one mutating lambda anywhere in the program makes that slot
// task-local, so the same task crossing is rejected for each slot shape.
#[test]
fn a_task_local_store_keeps_each_slot_shape_off_tasks() {
    for (decls, keep, call) in [
        (
            "class Stage { pub run: fn(i64) -> i64; } async fn go(s: Stage) -> i64 { let f = s.run; return f(1); }",
            "let s = new Stage(|x: i64| { n = n + x; return n; });",
            "go(new Stage(inc))",
        ),
        (
            "enum Op { Map(fn(i64) -> i64) } async fn go(op: Op) -> i64 { match op { Op::Map(f) => { return f(1); } } }",
            "let o = Op::Map(|x: i64| { n = n + x; return n; });",
            "go(Op::Map(inc))",
        ),
        (
            "class Holder<T> { pub f: fn(T) -> T; } async fn go(h: Holder<i64>) -> i64 { let f = h.f; return f(1); }",
            "let h = new Holder<i64>(|x: i64| { n = n + x; return n; });",
            "go(new Holder<i64>(inc))",
        ),
    ] {
        let source = format!(
            "fn inc(x: i64) -> i64 {{ return x + 1; }}
{decls}
fn keep() {{ let mut n = 0; {keep} }}
async fn main() {{ println(await {call}); }}"
        );
        let (ok, out) = compile_with_compiler_env(&source, &[]);
        assert!(!ok, "{decls}: {out}");
        assert!(out.contains("error[E2402]"), "{decls}: {out}");
        assert!(
            out.contains("every value the program stores"),
            "{decls}: {out}"
        );
    }
}

// A promise made through an interface imported from another module makes a
// class shareable at every depth, including nested function slots that
// store inference alone keeps task-local; a plain imported class follows
// its stores across modules.
#[test]
fn imported_promises_and_imported_classes_cross_tasks() {
    let lib = "
pub interface Shared extends Sync {}
pub class Plain { pub f: fn(i64) -> i64; }
pub fn triple(x: i64) -> i64 { return x * 3; }
";
    let app = "
import std::collections::Array;
import lib::Shared;
import lib::Plain;
import lib::triple;
fn inc(x: i64) -> i64 { return x + 1; }
class Table implements Shared { pub fs: FrozenArray<fn(i64) -> i64>; pub one: fn(i64) -> i64; }
async fn run_table(t: Table, x: i64) -> i64 {
    let mut v = x;
    for f in t.fs { await sleep(1); v = f(v); }
    let g = t.one;
    return g(v);
}
async fn run_plain(p: Plain, x: i64) -> i64 { let f = p.f; await sleep(1); return f(x); }
async fn main() {
    let k = 2;
    println(await run_table(new Table([inc, |x: i64| x * k].freeze(), inc), 1));
    println(await run_plain(new Plain(inc), 1));
    println(await run_plain(new Plain(triple), 2));
}
";
    let (out, ok) = compile_temp_project_and_run(&[("lib.wi", lib), ("app.wi", app)], "app.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "5\n2\n6\n");
}

// Once every call through `f` is direct, the binding itself is removed, so
// the function no longer loads `inc`'s shared value; a binding a lambda
// still captures keeps its value.
#[test]
fn a_devirtualized_binding_loads_no_function_value() {
    let source = "fn inc(x: i64) -> i64 { return x + 1; }
fn twice(x: i64) -> i64 { let f = inc; let g = f; return g(f(x)); }
fn keep(x: i64) -> i64 { let f = inc; let k = || f(x); return f(x) + k(); }
fn main() { println(twice(1)); println(keep(1)); }";
    let id = unique_test_id();
    let path = temp_path(format!("willow_fn_value_dead_{id}.wi"));
    std::fs::write(&path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["build", &path, "--emit-lir"])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    let lir = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{lir}");
    let body = |name: &str| {
        let start = lir.find(&format!("fn {name}(")).expect(name);
        let rest = &lir[start..];
        let end = rest[1..].find("\nfn ").map_or(rest.len(), |end| end + 1);
        rest[..end].to_string()
    };
    let twice = body("twice");
    assert!(!twice.contains("FunctionRef"), "{twice}");
    assert!(!twice.contains("IndirectCall"), "{twice}");
    assert_eq!(twice.matches("DirectCall").count(), 2, "{twice}");
    let keep = body("keep");
    assert_eq!(keep.matches("FunctionRef").count(), 1, "{keep}");
    for run in [compile_and_run_gc_stress, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "3\n4\n");
    }
}
