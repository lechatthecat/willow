use super::*;

#[test]
fn ownership_transfer_rejects_original_alias_routes() {
    for body in [
        "let b = new Box(0); let m = Mutex::new(b); b.set(1);",
        "let m = Mutex::new(new Box(0)); let mut x = new Box(1); lock m as b { x = b; } x.set(2);",
        "let c: Channel<Box> = Channel::new(); let b = new Box(0); c.send(b); b.set(1);",
    ] {
        let source = format!(
            "class Box {{ pub n: i64; pub fn set(self, n: i64) {{ self.n = n; }} }} async fn main() {{ {body} }}"
        );
        let (ok, diagnostics) = compile_with_data_race_check(&source);
        assert!(!ok && diagnostics.contains("E2408"), "{diagnostics}");
    }
}

#[test]
fn ownership_transfer_example_runs() {
    let (out, ok) = compile_and_run(include_str!("../../../example/ownership_transfer.wi"));
    assert!(ok, "{out}");
    assert_eq!(out, "42\n7\n");
}

#[test]
fn ownership_transfer_try_error_paths() {
    for (name, declarations, function, reject) in [
        (
            "non-Sync error with scalar success",
            "class Box { pub n: i64; pub fn set(self) { self.n = 1; } }",
            "async fn leak(m: Mutex<Result<i64, Box>>) -> Result<i64, Box> { lock m as value { let n = value?; } return Result::Ok(0); }",
            true,
        ),
        (
            "aliased error",
            "class Box { pub n: i64; pub fn set(self) { self.n = 1; } }",
            "async fn leak(m: Mutex<Result<i64, Box>>) -> Result<i64, Box> { lock m as value { let alias = value; let n = alias?; } return Result::Ok(0); }",
            true,
        ),
        (
            "implicit conversion to scalar error",
            "class Box implements Into<i64> { pub n: i64; pub fn set(self) { self.n = 1; } pub fn into(self) -> i64 { return self.n; } }",
            "async fn leak(m: Mutex<Result<i64, Box>>) -> Result<i64, i64> { lock m as value { let n = value?; } return Result::Ok(0); }",
            true,
        ),
        (
            "implicit conversion retains the error",
            "class Wrapped { pub source: Box; } class Box implements Into<Wrapped> { pub n: i64; pub fn set(self) { self.n = 1; } pub fn into(self) -> Wrapped { return new Wrapped(self); } }",
            "async fn leak(m: Mutex<Result<i64, Box>>) -> Result<i64, Wrapped> { lock m as value { let n = value?; } return Result::Ok(0); }",
            true,
        ),
        (
            "conversion hidden in a receiver method",
            "class Box implements Into<i64> { pub n: i64; pub fn set(self) { self.n = 1; } pub fn into(self) -> i64 { return self.n; } } class Holder { pub value: Result<i64, Box>; pub fn read(self) -> Result<i64, i64> { let n = self.value?; return Result::Ok(n); } }",
            "async fn leak(m: Mutex<Holder>) -> Result<i64, i64> { lock m as value { let result = value.read(); } return Result::Ok(0); }",
            true,
        ),
        (
            "scalar propagation",
            "",
            "async fn safe(m: Mutex<Result<i64, i64>>) -> Result<i64, i64> { lock m as value { let n = value?; } return Result::Ok(0); }",
            false,
        ),
        (
            "non-Sync success stays protected, scalar error escapes",
            "class Box { pub n: i64; pub fn set(self) { self.n = 1; } }",
            "async fn safe(m: Mutex<Result<Box, i64>>) -> Result<i64, i64> { lock m as value { let b = value?; b.set(); } return Result::Ok(0); }",
            false,
        ),
        (
            "Option absence carries no protected payload",
            "class Box { pub n: i64; pub fn set(self) { self.n = 1; } }",
            "async fn safe(m: Mutex<Option<Box>>) -> Option<i64> { lock m as value { let b = value?; b.set(); } return Option::Some(0); }",
            false,
        ),
    ] {
        let source = format!("{declarations} {function} fn main() {{}}");
        let (ok, diagnostics) = compile_with_data_race_check(&source);
        assert_eq!(ok, !reject, "{name}: {diagnostics}");
        if reject {
            assert!(diagnostics.contains("E2408"), "{name}: {diagnostics}");
        }
    }
}

#[test]
fn ownership_transfer_try_scalar_paths_run_and_unlock() {
    let (out, ok) = compile_and_run(
        r#"
async fn read(m: Mutex<Result<i64, i64>>) -> Result<i64, i64> {
    lock m as value { let n = value?; return Result::Ok(n + 1); }
}
async fn main() {
    let success = await read(Mutex::new(Result::Ok(4)));
    match success { Result::Ok(n) => println(n), Result::Err(e) => println(e) };
    let error: Result<i64, i64> = Result::Err(9);
    let m = Mutex::new(error);
    let failure = await read(m);
    match failure { Result::Ok(n) => println(n), Result::Err(e) => println(e) };
    lock m as value {
        match value { Result::Ok(n) => println(n), Result::Err(e) => println(e) };
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n9\n9\n");
}

#[test]
fn ownership_transfer_static_helpers_are_not_constructors() {
    for (method, result, value) in [
        ("stash", "Result<i64, i64>", "Result::Ok(0)"),
        ("stash", "Option<i64>", "Option::Some(0)"),
        ("new", "Array<i64>", "[0]"),
        ("new", "Map<i64, i64>", "Map::new()"),
        ("new", "Mutex<i64>", "Mutex::new(0)"),
    ] {
        let source = format!(
            "import std::collections::Array; import std::collections::Map;
             class Box {{ pub n: i64; pub fn set(self) {{ self.n = 1; }} }}
             class Holder {{ pub item: Box; }}
             class Helper {{ pub static fn {method}(dst: Holder, b: Box) -> {result} {{ dst.item = b; return {value}; }} }}
             async fn main() {{
                 let dst = new Holder(new Box(0));
                 let m = Mutex::new(new Box(1));
                 lock m as b {{ let ignored = Helper::{method}(dst, b); }}
                 dst.item.set();
             }}"
        );
        let (ok, diagnostics) = compile_with_data_race_check(&source);
        assert!(
            !ok && diagnostics.contains("E2408"),
            "{result}: {diagnostics}"
        );
    }
}

#[test]
fn ownership_transfer_real_collection_constructors_remain_owned() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
import std::collections::Map;
async fn main() {
    let xs: Array<i64> = Array::new();
    let m = Mutex::new(xs);
    let ys: Map<i64, i64> = Map::new();
    let n = Mutex::new(ys);
    lock m as x { x.push(1); println(x.len()); }
    lock n as y { y.insert(1, 2); println(y.len()); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n1\n");
}

#[test]
fn ownership_transfer_select_received_values() {
    for (receive, payload) in [("recv", "Box"), ("recv_opt", "Option<Box>")] {
        for (body, reject) in [
            ("let m = Mutex::new(b);", false),
            ("out.send(b);", false),
            ("let m = Mutex::new(b); let reused = b;", true),
            ("let alias = b; out.send(b); let reused = alias;", true),
        ] {
            let source = format!(
                "class Box {{ pub n: i64; pub fn set(self) {{ self.n = 1; }} }}
                 async fn main() {{
                     let c: Channel<Box> = Channel::new();
                     let out: Channel<{payload}> = Channel::new();
                     select {{ let b = c.{receive}() => {{ {body} }} }}
                 }}"
            );
            let (ok, diagnostics) = compile_with_data_race_check(&source);
            assert_eq!(ok, !reject, "{receive}: {body}: {diagnostics}");
            if reject {
                assert!(diagnostics.contains("E2408"), "{diagnostics}");
            }
        }
    }
}

#[test]
fn ownership_transfer_select_received_values_run() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub n: i64; pub fn increment(self) { self.n += 1; } }
async fn main() {
    let input: Channel<Box> = Channel::new();
    let output: Channel<Box> = Channel::new();
    for i in 0..2 {
        input.send(new Box(i));
        select {
            let b = input.recv() => {
                let m = Mutex::new(b);
                lock m as value { value.increment(); println(value.n); }
            }
        }
    }
    input.send(new Box(7));
    select { let b = input.recv() => { output.send(b); } }
    select { let b = output.recv() => { println(b.n); } }
    input.close();
    select {
        let b = input.recv_opt() => {
            let m = Mutex::new(b);
            lock m as value {
                match value { Option::Some(box) => println(box.n), Option::None => println(9) };
            }
        }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n7\n9\n");
}

#[test]
fn ownership_transfer_option_result_callbacks_retain_payloads() {
    for (ty, init, method, output, returned) in [
        ("Option<Box>", "Option::Some(new Box(0))", "map", "i64", "0"),
        (
            "Option<Box>",
            "Option::Some(new Box(0))",
            "and_then",
            "Option<i64>",
            "Option::Some(0)",
        ),
        (
            "Result<Box, i64>",
            "Result::Ok(new Box(0))",
            "map",
            "i64",
            "0",
        ),
        (
            "Result<i64, Box>",
            "Result::Err(new Box(0))",
            "map_err",
            "i64",
            "0",
        ),
        (
            "Result<Box, i64>",
            "Result::Ok(new Box(0))",
            "and_then",
            "Result<i64, i64>",
            "Result::Ok(0)",
        ),
        (
            "Result<i64, Box>",
            "Result::Err(new Box(0))",
            "or_else",
            "Result<i64, i64>",
            "Result::Ok(0)",
        ),
    ] {
        for protected in [true, false] {
            let body = if protected {
                format!(
                    "let m = Mutex::new(value); lock m as value {{ let ignored = value.{method}(keep); }}"
                )
            } else {
                format!("let ignored = value.{method}(keep); let m = Mutex::new(value);")
            };
            let source = format!(
                "class Box {{ pub n: i64; pub fn set(self) {{ self.n = 1; }} }}
                 class Saved {{ pub static mut value: Box = new Box(0); }}
                 fn keep(b: Box) -> {output} {{ Saved::value = b; return {returned}; }}
                 async fn main() {{ let value: {ty} = {init}; {body} Saved::value.set(); }}"
            );
            let (ok, diagnostics) = compile_with_data_race_check(&source);
            assert!(
                !ok && diagnostics.contains("E2408"),
                "{ty}.{method}, protected={protected}: {diagnostics}"
            );
        }
    }
}

#[test]
fn ownership_transfer_option_result_scalar_callback_controls() {
    for (ty, init, method, input, output, returned) in [
        (
            "Option<i64>",
            "Option::Some(3)",
            "map",
            "n: i64",
            "i64",
            "n + 1",
        ),
        (
            "Option<i64>",
            "Option::Some(3)",
            "and_then",
            "n: i64",
            "Option<i64>",
            "Option::Some(n)",
        ),
        (
            "Result<i64, i64>",
            "Result::Ok(3)",
            "map",
            "n: i64",
            "i64",
            "n + 1",
        ),
        (
            "Result<i64, i64>",
            "Result::Err(3)",
            "map_err",
            "n: i64",
            "i64",
            "n + 1",
        ),
        (
            "Result<i64, i64>",
            "Result::Ok(3)",
            "and_then",
            "n: i64",
            "Result<i64, i64>",
            "Result::Ok(n)",
        ),
        (
            "Result<i64, i64>",
            "Result::Err(3)",
            "or_else",
            "n: i64",
            "Result<i64, i64>",
            "Result::Ok(n)",
        ),
        // The untouched non-Sync branch is not passed to the callback.
        (
            "Result<i64, Box>",
            "Result::Ok(3)",
            "map",
            "n: i64",
            "i64",
            "n + 1",
        ),
        (
            "Result<Box, i64>",
            "Result::Err(3)",
            "map_err",
            "n: i64",
            "i64",
            "n + 1",
        ),
        // Option::or_else takes no argument, unlike Result::or_else.
        (
            "Option<Box>",
            "Option::Some(new Box(3))",
            "or_else",
            "",
            "Option<Box>",
            "Option::None",
        ),
    ] {
        let source = format!(
            "class Box {{ pub n: i64; pub fn set(self) {{ self.n = 1; }} }}
             fn callback({input}) -> {output} {{ return {returned}; }}
             async fn main() {{ let value: {ty} = {init}; let m = Mutex::new(value);
                 lock m as value {{ let ignored = value.{method}(callback); }} }}"
        );
        let (ok, diagnostics) = compile_with_data_race_check(&source);
        assert!(ok, "{ty}.{method}: {diagnostics}");
    }
}

#[test]
fn ownership_transfer_callback_results_are_borrowed() {
    let (ok, diagnostics) = compile_with_data_race_check(
        r#"
class Box { pub n: i64; pub fn set(self) { self.n = 1; } }
class Saved { pub static value: Box = new Box(0); }
fn existing(n: i64) -> Box { return Saved::value; }
async fn main() {
    let value = Option::Some(0);
    let mapped = value.map(existing);
    let m = Mutex::new(mapped);
    Saved::value.set();
}
"#,
    );
    assert!(!ok && diagnostics.contains("E2408"), "{diagnostics}");
}

#[test]
fn ownership_transfer_scalar_callbacks_run_under_lock() {
    let (out, ok) = compile_and_run(
        r#"
fn plus(n: i64) -> i64 { return n + 1; }
fn next(n: i64) -> Option<i64> { return Option::Some(n + 1); }
fn restore_result(n: i64) -> Result<i64, i64> { return Result::Ok(n + 1); }
async fn main() {
    let m = Mutex::new(Option::Some(3));
    lock m as value { println(value.map(plus).and_then(next).unwrap()); }
    let error: Result<i64, i64> = Result::Err(7);
    let n = Mutex::new(error);
    lock n as value { println(value.map_err(plus).or_else(restore_result).unwrap()); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n9\n");
}
