use super::support::*;

#[test]
fn tuple_pair_return_and_match() {
    let (out, ok) = run(
        &[(
            "main.wi",
            "fn pair() -> (i64, i64) { return (1, 2); } fn main() { match pair() { (a, b) => { println(a + b); } } }",
        )],
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn tuple_cross_module_identity() {
    let (out, ok) = run(
        &[
            (
                "pair.wi",
                "module pair; pub fn make() -> (i64, String) { return (7, \"seven\"); }",
            ),
            (
                "main.wi",
                "import pair; fn read(p: (i64, String)) { match p { (n, s) => { println(n); println(s); } } } fn main() { read(pair::make()); }",
            ),
        ],
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\nseven\n");
}

// A prebuilt runtime allows these compiler tests to run alongside another
// workspace build without contending for that build's runtime-link lock.
fn run(files: &[(&str, &str)], env: &[(&str, &str)]) -> (String, bool) {
    let root = std::env::temp_dir().join(format!("willow_tuples_{}", unique_test_id()));
    fs::create_dir_all(&root).unwrap();
    for (name, source) in files {
        fs::write(root.join(name), source).unwrap();
    }
    let binary = root.join("program");
    let mut command = Command::new(env!("CARGO_BIN_EXE_willow"));
    command
        .arg("build")
        .arg(root.join("main.wi"))
        .arg("-o")
        .arg(&binary);
    if let Some(runtime) = std::env::var_os("WILLOW_TEST_RUNTIME") {
        command.arg("--runtime-lib").arg(runtime);
    }
    let built = command.output().unwrap();
    let result = if built.status.success() {
        let out = Command::new(&binary)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        (
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
            out.status.success(),
        )
    } else {
        (String::from_utf8_lossy(&built.stderr).into_owned(), false)
    };
    fs::remove_dir_all(root).unwrap();
    result
}

#[test]
fn tuple_gc_layout_and_evaluation_order() {
    let source = r#"
import std::collections::Array;
class Counter { pub n: i64; pub fn next(self) -> i64 { self.n = self.n + 1; gc_collect(); return self.n; } }
class Cell { pub value: i64; }
interface Value { fn get(self) -> i64; }
class Number implements Value { pub n: i64; pub fn get(self) -> i64 { return self.n; } }
fn collect() -> String { gc_collect(); return "b" + "c"; }
fn main() {
    let counter = new Counter(0);
    let (a, b, c) = (counter.next(), counter.next(), counter.next());
    println(a * 100 + b * 10 + c);
    let p: (String, i64, bool, f64, Cell, Value, Array<i64>) =
        ("a" + "b", 42, true, 2.5, new Cell(9), new Number(11), [7, 8]);
    gc_minor_collect(); gc_collect();
    let (s, n, flag, f, cell, value, values) = p;
    println(s); println(n); println(flag); println(f); println(cell.value); println(value.get()); println(values[1]);
    let nested = (("x" + "y", new Cell(17)), collect());
    gc_collect();
    let (inner, outer) = nested;
    let (text, held) = inner;
    println(text); println(held.value); println(outer);
    let entries: Array<(i64, String)> = [(4, "four"), (5, "five")];
    gc_collect();
    let (key, name) = entries[1]; println(key); println(name);
}
"#;
    let (out, ok) = run(&[("main.wi", source)], &[("WILLOW_GC_STRESS", "all")]);
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "123\nab\n42\ntrue\n2.5\n9\n11\n8\nxy\n17\nbc\n5\nfive\n"
    );
}

#[test]
fn tuple_scope_control_flow_and_context() {
    let source = r#"
fn pair() -> (i64, bool) { println(10); return (3, true); }
fn sum() -> i64 { let (a, b) = (5, 7); return a + b; }
async fn work() -> (String, i64) { return ("task", 19); }
async fn main() {
    let (a, b) = pair();
    let (a, c) = (a + 1, b); println(a); println(c);
    println(sum());
    let (single,) = (13,); println(single);
    let p: (Option<i64>, bool) = (None, true);
    let (option, _) = p; println(match option { None => 21, Some(v) => v });
    let (text, n) = await work(); gc_collect(); println(text); println(n);
    let mut i = 0;
    while i < 3 { i = i + 1; let (x, y) = (i, i + 1); if x == 1 { continue; } println(y); if x == 2 { break; } }
    defer println(30);
    let (_, end) = (0, 31); defer println(end);
}
"#;
    let (out, ok) = run(&[("main.wi", source)], &[]);
    assert!(ok, "{out}");
    assert_eq!(out, "10\n4\ntrue\n12\n13\n21\ntask\n19\n3\n31\n30\n");
}

#[test]
fn tuple_imported_inferred_type_without_local_tuple_syntax() {
    let (out, ok) = run(
        &[
            (
                "pair.wi",
                "module pair; pub fn make() -> (i64, bool) { return (7, true); } pub fn read(p: (i64, bool)) -> i64 { let (n, flag) = p; return n; }",
            ),
            (
                "main.wi",
                "import pair; fn main() { let p = pair::make(); println(pair::read(p)); }",
            ),
        ],
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn tuple_wide_gc_bitmap() {
    let mut values = vec!["1"; 70];
    values[0] = "\"first\" + \"!\"";
    values[64] = "\"late\" + \"!\"";
    values[69] = "\"last\" + \"!\"";
    let mut bindings = vec!["_"; 70];
    bindings[0] = "first";
    bindings[64] = "late";
    bindings[69] = "last";
    let source = format!(
        "fn main() {{ let p = ({}); gc_minor_collect(); gc_collect(); let ({}) = p; println(first); println(late); println(last); }}",
        values.join(","),
        bindings.join(",")
    );
    let (out, ok) = run(&[("main.wi", &source)], &[("WILLOW_GC_STRESS", "all")]);
    assert!(ok, "{out}");
    assert_eq!(out, "first!\nlate!\nlast!\n");
}

#[test]
fn tuple_let_recovery_resumes_at_the_real_source_boundary() {
    let source = r#"
fn value() -> i64 {
    if true {
        defer println("oldest");
        let (x,) = (1,);
        let (y,): (i64,) = (x,);
        defer match recover() { Some(info) => println(info.message), None => {} }
        defer println(y);
        panic("sync");
        println("unreachable");
    }
    return 9;
}
async fn work() -> i64 {
    if true {
        let (x,) = (2,);
        defer match recover() { Some(info) => println(info.message), None => {} }
        panic("async");
    }
    return 10;
}
async fn main() { println(value()); println(await work()); }
"#;
    let (out, ok) = run(&[("main.wi", source)], &[]);
    assert!(ok, "{out}");
    assert_eq!(out, "1\nsync\noldest\n9\nasync\n10\n");
}

// willow-jz15.57: the example enumerates twenty distinct signature/call
// perspectives. Check and execute the exact same project in both build modes.
#[test]
fn tuple_same_module_signatures_check_debug_release() {
    let project = TestProject::new(
        "tuple_module_signatures",
        &[
            (
                "pairs.wi",
                include_str!("../../example/module_tuple_calls/pairs.wi"),
            ),
            (
                "main.wi",
                include_str!("../../example/module_tuple_calls/main.wi"),
            ),
        ],
    );
    let checked = project.check("main.wi");
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    for release in [false, true] {
        let built = if release {
            project.compile_release("main.wi")
        } else {
            project.compile("main.wi")
        };
        assert!(
            built.status.success(),
            "release={release}: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        let output = project.run();
        assert!(
            output.status.success(),
            "release={release}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "5\n5\n5\n5\n5\n5\n5\n5\n5\n5\nfive\n2.5\ntrue\n5\n5\n5\n5\n5\n5\n5\n",
            "release={release}"
        );
    }
}

#[test]
fn tuple_same_module_original_repro_check_debug_release() {
    let project = TestProject::new(
        "tuple_module_repro",
        &[
            (
                "r.wi",
                "module r; fn take(p: (i64, i64)) -> i64 { let (a, b) = p; return a; } pub fn top(x: i64) -> i64 { return take((x, 1)); }",
            ),
            ("main.wi", "import r::top; fn main() { println(top(5)); }"),
        ],
    );
    let checked = project.check("main.wi");
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    for release in [false, true] {
        let built = if release {
            project.compile_release("main.wi")
        } else {
            project.compile("main.wi")
        };
        assert!(
            built.status.success(),
            "release={release}: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        let output = project.run();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "5\n");
    }
}
