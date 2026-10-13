use super::*;

const EXAMPLE: &str = include_str!("../../../example/async_scalar_reference.wi");
const SCALARS: [(&str, &str, &str, &str); 3] = [
    ("i64", "42", "81", "81\n"),
    ("f64", "2.5", "4.25", "4.25\n"),
    ("bool", "false", "true", "true\n"),
];

#[test]
fn async_scalar_reference_example() {
    let (out, ok) = compile_and_run(EXAMPLE);
    assert!(ok, "{out}");
    assert_eq!(out, "42\n2.5\ntrue\n");
}

#[test]
fn async_scalar_reference_release() {
    let (out, ok) = compile_and_run_release(EXAMPLE);
    assert!(ok, "{out}");
    assert_eq!(out, "42\n2.5\ntrue\n");
}

// Eight distinct paths for each scalar representation (24 perspectives).
#[test]
fn async_scalar_reference_paths() {
    for (ty, _, value, expected) in SCALARS {
        for shape in 0..8 {
            let read = format!("async fn read(x: &{ty}) -> {ty} {{ await sleep(1); return x; }}");
            let source = match shape {
                0 => format!(
                    "{read} async fn main() {{ let x = {value}; let task = read(&x); await sleep(1); let y = await task; println(y); }}"
                ),
                1 => format!(
                    "{read} async fn forward(x: &{ty}) -> {ty} {{ await sleep(1); return await read(&x); }} async fn main() {{ let x = {value}; let y = await forward(&x); println(y); }}"
                ),
                2 => format!(
                    "fn sync_read(x: &{ty}) -> {ty} {{ return x; }} async fn read(x: &{ty}) -> {ty} {{ await sleep(1); return sync_read(&x); }} async fn main() {{ let x = {value}; let y = await read(&x); println(y); }}"
                ),
                3 => format!(
                    "async fn read(x: &{ty}) {{ defer {{ println(x); }} await sleep(1); }} async fn main() {{ let x = {value}; await read(&x); }}"
                ),
                4 => format!(
                    "{read} async fn main() {{ let x = {value}; let mut i = 0; while i < 3 {{ let y = await read(&x); if i == 2 {{ println(y); }} i = i + 1; }} }}"
                ),
                5 => format!(
                    "{read} async fn main() {{ let x = {value}; if true {{ let z = await read(&x); println(z); }} else {{ println(x); }} }}"
                ),
                6 => format!(
                    "async fn read(a: i64, x: &{ty}, b: String, y: &{ty}) -> {ty} {{ await sleep(1); println(a); println(b); return y; }} async fn main() {{ let x = {value}; let y = await read(7, &x, \"kept\", &x); println(y); }}"
                ),
                _ => format!(
                    "{read} async fn main() {{ let x = {value}; let first = read(&x); let second = read(&x); let a = await first; let b = await second; println(b); }}"
                ),
            };
            let (out, ok) = compile_and_run(&source);
            assert!(ok, "type={ty}, shape={shape}: {source}\n{out}");
            let expected = if shape == 6 {
                format!("7\nkept\n{expected}")
            } else {
                expected.to_string()
            };
            assert_eq!(out, expected, "type={ty}, shape={shape}");
        }
    }
}

#[test]
fn async_scalar_reference_observes_caller_storage() {
    for (ty, initial, updated, expected) in SCALARS {
        // The gate orders the caller's mutation before the callee's second
        // read. No timing assumptions or concurrent read/write race.
        let source = format!(
            "async fn read(x: &{ty}, ready: Channel<i64>, gate: Channel<i64>) -> {ty} {{ let before = x; ready.send(1); gate.recv(); await sleep(1); return x; }} async fn main() {{ let mut x = {initial}; let ready = Channel<i64>::new(); let gate = Channel<i64>::new(); let task = read(&x, ready, gate); ready.recv(); x = {updated}; gate.send(1); let result = await task; println(result); }}"
        );
        for release in [false, true] {
            let (out, ok) = if release {
                compile_and_run_release(&source)
            } else {
                compile_and_run(&source)
            };
            assert!(ok, "type={ty}, release={release}: {out}");
            assert_eq!(out, expected, "type={ty}, release={release}");
        }
    }
}

#[test]
fn async_scalar_reference_gc_stress() {
    for mode in ["minor", "all", "relocate"] {
        let (out, ok) = compile_and_run_gc_stress_mode(EXAMPLE, mode);
        assert!(ok, "mode={mode}: {out}");
        assert_eq!(out, "42\n2.5\ntrue\n");
    }
}

#[test]
fn async_scalar_reference_diagnostics() {
    for (ty, _, value, _) in SCALARS {
        for (body, call, code) in [
            (
                format!("x = {value};"),
                format!("let x = {value}; await read(&x);"),
                "E0302",
            ),
            (
                String::new(),
                format!("let x = {value}; await read(x);"),
                "E1702",
            ),
            (
                String::new(),
                format!("let x = {value}; let task = read(&x);"),
                "E1708",
            ),
            (String::new(), format!("await read(&({value}));"), "E1704"),
        ] {
            let source = format!(
                "async fn read(x: &{ty}) {{ await sleep(1); {body} }} async fn main() {{ {call} }}"
            );
            let diagnostics = compile_error_stderr(&source);
            assert!(diagnostics.contains(code), "{source}\n{diagnostics}");
            assert!(!diagnostics.contains("E1707"), "{diagnostics}");
        }
    }
}

#[test]
fn async_scalar_reference_methods_and_imports() {
    let source = "class Reader { pub async fn read(self, x: &f64) -> f64 { await sleep(1); return x; } pub static async fn flag(x: &bool) -> bool { await sleep(1); return x; } } async fn main() { let reader = new Reader(); let x = 2.5; let flag = true; let y = await reader.read(&x); println(y); let z = await Reader::flag(&flag); println(z); }";
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "2.5\ntrue\n");
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "reader.wi",
                "module reader; pub async fn read(x: &i64) -> i64 { await sleep(1); return x; }",
            ),
            (
                "main.wi",
                "import reader::read; async fn main() { let x = 42; let y = await read(&x); println(y); }",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn async_scalar_reference_heap_places_relocate() {
    let source = "import std::collections::Array; class Box { pub value: i64; } async fn read(x: &i64) -> i64 { await sleep(1); gc_collect(); return x; } async fn main() { let b = new Box(42); let values: Array<i64> = [81]; let a = await read(&b.value); println(a); let v = await read(&values[0]); println(v); }";
    for mode in ["all", "relocate"] {
        let (out, ok) = compile_and_run_gc_stress_mode(source, mode);
        assert!(ok, "{mode}: {out}");
        assert_eq!(out, "42\n81\n");
    }
}

#[test]
fn async_scalar_reference_forwarding_chain() {
    for depth in [1, 8, 32] {
        let mut source = String::from("async fn f0(x: &i64) -> i64 { await sleep(1); return x; }");
        for i in 1..depth {
            source.push_str(&format!(
                "async fn f{i}(x: &i64) -> i64 {{ await sleep(1); return await f{}(&x); }}",
                i - 1
            ));
        }
        source.push_str(&format!(
            "async fn main() {{ let x = 42; let y = await f{}(&x); println(y); }}",
            depth - 1
        ));
        let (out, ok) = compile_and_run_gc_stress_mode(&source, "relocate");
        assert!(ok, "depth={depth}: {out}");
        assert_eq!(out, "42\n");
    }
}

#[test]
fn async_scalar_reference_preserves_reference_errors() {
    for (source, code) in [
        (
            "fn update(x: &mut i64) {} async fn read(x: &i64) { await sleep(1); update(&x); } fn main() {}",
            "E1701",
        ),
        (
            "async fn read(x: i64) {} async fn main() { let x = 42; await read(&x); }",
            "E1703",
        ),
        (
            "async fn read(x: &i64) {} async fn main() { let x = true; await read(&x); }",
            "E1705",
        ),
    ] {
        let diagnostics = compile_error_stderr(source);
        assert!(diagnostics.contains(code), "{diagnostics}");
    }
}
