use super::super::support::*;

// ── Variables ────────────────────────────────────────────────────────────────

#[test]
fn test_let_mut() {
    let src = r#"
fn main() {
    let mut a = 10;
    a = 20;
    println(a);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "20");
}

#[test]
fn test_type_annotation() {
    let src = r#"
fn main() {
    let x: i64 = 99;
    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "99");
}

#[test]
fn test_mutable_f64_assignment() {
    let src = r#"
fn main() {
    let mut x: f64 = 1.5;
    x = x + 2.5;

    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "4");
}

#[test]
fn test_mut_reference_i64_local_writeback() {
    let src = r#"
fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut n = 10;
    increment(&n);
    println(n);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "11\n");
}

#[test]
fn test_mut_reference_f64_local_writeback() {
    let src = r#"
fn add_half(x: &mut f64) {
    x = x + 0.5;
}

fn main() {
    let mut n: f64 = 2.0;
    add_half(&n);
    println(n);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "2.5\n");
}

#[test]
fn test_mut_reference_bool_local_writeback() {
    let src = r#"
fn flip(x: &mut bool) {
    x = !x;
}

fn main() {
    let mut enabled = false;
    flip(&enabled);
    println(enabled);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\n");
}

#[test]
fn test_immutable_reference_reads_from_immutable_local() {
    let src = r#"
fn read(x: & i64) -> i64 {
    return x;
}

fn main() {
    let n = 10;
    println(read(&n));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "10\n");
}

#[test]
fn test_immutable_reference_parameter_rejects_assignment() {
    assert_compile_error_contains(
        r#"
fn increment(x: & i64) {
    x = x + 1;
}

fn main() {
    let n = 10;
    increment(&n);
}
"#,
        &["cannot assign to immutable parameter `x`"],
    );
}

#[test]
fn test_gc_string_immutable_reference_survives_collect_in_callee() {
    let src = r#"
fn shout(text: & String) -> String {
    gc_collect();
    return text + "!";
}

fn main() {
    let text = "he" + "llo";
    println(shout(&text));
    gc_collect();
    println(text);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "String & local should remain rooted across callee collect"
    );
    assert_eq!(out, "hello!\nhello\n");
}

#[test]
fn test_gc_string_mut_reference_assignment_survives_collect_in_callee() {
    let src = r#"
fn replace(text: &mut String) {
    text = text + "!";
    gc_collect();
}

fn main() {
    let mut text = "he" + "llo";
    replace(&text);
    gc_collect();
    println(text);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "String &mut assignment should update the caller root before callee collect"
    );
    assert_eq!(out, "hello!\ntrue\n");
}

#[test]
fn test_gc_class_immutable_reference_survives_collect_in_callee() {
    let src = r#"
class Box {
    pub value: String;
}

fn read(box: & Box) -> String {
    gc_collect();
    return box.value;
}

fn main() {
    let box = new Box("ke" + "pt");
    println(read(&box));
    gc_collect();
    println(box.value);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "class & local should remain rooted across callee collect"
    );
    assert_eq!(out, "kept\nkept\n");
}

#[test]
fn test_gc_class_mut_reference_assignment_survives_collect_in_callee() {
    let src = r#"
class Box {
    pub value: String;
}

fn replace(box: &mut Box) {
    box = new Box("after" + "!");
    gc_collect();
}

fn main() {
    let mut box = new Box("before");
    replace(&box);
    gc_collect();
    println(box.value);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "class &mut assignment should update the caller root before callee collect"
    );
    assert_eq!(out, "after!\ntrue\n");
}

#[test]
fn test_mut_reference_object_field_i64_writeback() {
    let src = r#"
class Counter {
    pub value: i64;
}

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let counter = new Counter(10);
    increment(&counter.value);
    println(counter.value);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "object field should be passable as &mut i64");
    assert_eq!(out, "11\n");
}

#[test]
fn test_immutable_reference_object_field_read() {
    let src = r#"
class Counter {
    pub value: i64;
}

fn read_twice(x: & i64) -> i64 {
    return x + x;
}

fn main() {
    let counter = new Counter(21);
    println(read_twice(&counter.value));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "object field should be passable as & i64");
    assert_eq!(out, "42\n");
}

#[test]
fn test_gc_object_field_string_mut_reference_survives_collect_in_callee() {
    let src = r#"
class User {
    pub name: String;
}

fn replace(name: &mut String) {
    name = name + "!";
    gc_collect();
}

fn main() {
    let user = new User("Al" + "ice");
    replace(&user.name);
    gc_collect();
    println(user.name);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "String field &mut assignment should survive callee collect"
    );
    assert_eq!(out, "Alice!\ntrue\n");
}

#[test]
fn test_mut_reference_private_object_field_is_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    secret: i64;

    pub static fn new(v: i64) -> User {
        return new User(v);
    }
}

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let user = User::new(10);
    increment(&user.secret);
}
"#,
        &[
            "error[E0501]",
            "field `secret` of class `User` is private",
            "private field",
        ],
    );
}

#[test]
fn test_mut_reference_array_element_i64_writeback() {
    let src = r#"
import std::collections::Array;

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut xs: Array<i64> = [10, 20];
    increment(&xs[0]);
    println(xs[0]);
    println(xs[1]);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "array element should be passable as &mut i64");
    assert_eq!(out, "11\n20\n");
}

#[test]
fn test_immutable_reference_array_element_read() {
    let src = r#"
import std::collections::Array;

fn read_twice(x: & i64) -> i64 {
    return x + x;
}

fn main() {
    let xs: Array<i64> = [21];
    println(read_twice(&xs[0]));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "array element should be passable as & i64");
    assert_eq!(out, "42\n");
}

#[test]
fn test_gc_array_element_string_mut_reference_survives_collect_in_callee() {
    let src = r#"
import std::collections::Array;

fn replace(text: &mut String) {
    text = text + "!";
    gc_collect();
}

fn main() {
    let mut names: Array<String> = ["Al" + "ice", "willow"];
    replace(&names[0]);
    gc_collect();
    println(names[0]);
    println(names[1]);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(
        ok,
        "String array element &mut assignment should survive callee collect"
    );
    assert_eq!(out, "Alice!\nwillow\ntrue\n");
}

#[test]
fn test_array_element_reference_out_of_bounds_reports_runtime_diagnostic() {
    let src = r#"
import std::collections::Array;

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut xs: Array<i64> = [1];
    increment(&xs[3]);
    println(99);
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "out-of-bounds array element reference should abort");
    assert!(
        out.contains("array index out of bounds: the length is 1 but the index is 3"),
        "missing array bounds diagnostic:\n{out}"
    );
}

#[test]
fn test_block_scope_shadowing_restores_outer_binding() {
    let src = r#"
fn main() {
    let x = 1;

    if true {
        let x = 2;
        println(x);
    }

    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "2\n1\n");
}

#[test]
fn test_nested_block_shadowing_restores_each_outer_binding() {
    let src = r#"
fn main() {
    let x = 1;

    if true {
        let x = 2;

        if true {
            let x = 3;
            println(x);
        }

        println(x);
    }

    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "3\n2\n1\n");
}
