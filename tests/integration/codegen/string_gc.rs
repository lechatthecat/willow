use super::*;

// ── WillowString GC migration tests (requirements/willow_string_gc_requirements.md sec 11) ─

// 11.1: String literal survives gc_collect
#[test]
fn test_string_gc_11_1_literal_survives_gc_collect() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s = "hello";
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 11.2: String concatenation survives gc_collect
#[test]
fn test_string_gc_11_2_concat_survives_gc_collect() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s = "hello" + " " + "world";
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello world\n");
}

// 11.3: String field survives gc_collect
#[test]
fn test_string_gc_11_3_string_field_survives_gc_collect() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    pub name: String;
    pub fn get_name(self) -> String { return self.name; }
}
fn main() {
    let u = new User("alice");
    gc_collect();
    println(u.get_name());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "alice\n");
}

// 11.4: Multiple string fields can be concatenated
#[test]
fn test_string_gc_11_4_multiple_string_fields_concat() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    pub first: String;
    pub last: String;
    pub fn full(self) -> String { return self.first + " " + self.last; }
}
fn main() {
    let u = new User("Ada", "Lovelace");
    println(u.full());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Ada Lovelace\n");
}

// 11.5: Option<String> survives gc_collect
#[test]
fn test_string_gc_11_5_option_string_survives_gc() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s = Option::Some("hello");
    gc_collect();
    println(s.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 11.6: Result<String, String> survives gc_collect
#[test]
fn test_string_gc_11_6_result_string_survives_gc() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r: Result<String, String> = Result::Ok("ok");
    gc_collect();
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "ok\n");
}

// 11.7: Option<String> with gc_collect (nullable String pattern via Option)
#[test]
fn test_string_gc_11_7_nullable_string_survives_gc() {
    let (out, ok) = compile_and_run(
        r#"
fn make_opt(flag: bool) -> Option<String> {
    if flag {
        return Option::Some("hello");
    }
    return Option::None;
}
fn main() {
    let s = make_opt(true);
    gc_collect();
    if s.is_some() {
        println(s.unwrap());
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 11.8: Repeated string concatenation and GC does not crash
#[test]
fn test_string_gc_11_8_repeated_concat_no_crash() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut s = "a";
    let mut i = 0;
    while i < 10 {
        s = s + "b";
        gc_collect();
        i = i + 1;
    }
    println(s);
}
"#,
    );
    assert!(ok);
    // "a" + 10 "b"s = 11 chars + "\n" = 12 total
    assert_eq!(out.len(), "abbbbbbbbbb\n".len());
}

// String GC stress: multiple objects with String fields across GC cycles
#[test]
fn test_string_gc_stress_class_fields_across_gc_cycles() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub label: String;
    pub fn get_label(self) -> String { return self.label; }
}
fn main() {
    let a = new Node("alpha");
    let b = new Node("beta");
    gc_collect();
    let c = new Node("gamma");
    gc_collect();
    println(a.get_label() + " " + b.get_label() + " " + c.get_label());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "alpha beta gamma\n");
}
