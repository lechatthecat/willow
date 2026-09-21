use super::*;

// ── Array dynamic growth: push/pop (willow-5a4) ────────────────────────────

// push grows an empty array; len and indexing reflect the appended elements.
#[test]
fn test_array_push_grows_empty() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [];
    let mut i = 0;
    while i < 6 { xs.push(i * 10); i = i + 1; }
    println(xs.len());
    println(xs[0]);
    println(xs[5]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n0\n50\n");
}

// pop returns the last element and shrinks the array.
#[test]
fn test_array_pop_returns_last() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    println(xs.pop());
    println(xs.pop());
    println(xs.len());
    println(xs[0]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n2\n1\n1\n");
}

// push works on a non-empty literal (grows past initial capacity).
#[test]
fn test_array_push_onto_literal() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [10, 20];
    xs.push(30);
    xs.push(40);
    println(xs.len());
    println(xs[2]);
    println(xs[3]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n30\n40\n");
}

// push/pop of reference (String) elements round-trips.
#[test]
fn test_array_push_pop_string_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let names: Array<String> = [];
    names.push("alice");
    names.push("bob");
    println(names.len());
    println(names.pop());
    println(names[0]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\nbob\nalice\n");
}

// f64 elements survive the push word/bit-cast.
#[test]
fn test_array_push_f64_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let fs: Array<f64> = [];
    fs.push(1.5);
    fs.push(2.5);
    println(fs[0] + fs[1]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n");
}

// pop then push reuses the array correctly.
#[test]
fn test_array_pop_then_push() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    let last = xs.pop();
    xs.push(last * 10);
    println(xs.len());
    println(xs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n30\n");
}

// String elements pushed across several growths survive a GC collection.
#[test]
fn test_array_pushed_strings_survive_gc() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<String> = [];
    let mut i = 0;
    while i < 20 { xs.push("item"); i = i + 1; }
    gc_collect();
    println(xs.len());
    println(xs[0]);
    println(xs[19]);
}
"#,
    );
    assert!(ok, "pushed string elements must survive GC across growth");
    assert_eq!(out, "20\nitem\nitem\n");
}

// Popping an empty array aborts.
#[test]
fn test_array_pop_empty_aborts() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [];
    println(xs.pop());
}
"#,
    );
    assert!(!ok, "pop on empty must abort");
    assert!(out.contains("empty array"), "got: {out}");
}

// Pushing the wrong element type is a compile error.
#[test]
fn test_array_push_wrong_type_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1];
    xs.push(true);
}
"#,
        &["error[E0201]", "cannot push"],
    );
}

// push with the wrong arity is a compile error.
#[test]
fn test_array_push_wrong_arity_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1];
    xs.push();
}
"#,
        &["error[E0201]", "`Array::push` expects 1 argument"],
    );
}

// ── Arrays are GC roots (regression for is_gc_managed(Array), willow-a7j-adjacent) ──

// An array local must survive gc_collect AND subsequent allocations that would
// reuse its freed memory if it were not rooted. (The plain survive-gc tests can
// pass by reading not-yet-reused freed memory; this forces reuse.)
#[test]
fn test_array_local_rooted_across_gc_and_reuse() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<String> = ["alpha", "beta", "gamma"];
    gc_collect();
    let ys: Array<i64> = [];
    let mut i = 0;
    while i < 300 { ys.push(i); i = i + 1; }
    println(xs[0]);
    println(xs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "alpha\ngamma\n");
}

// A class field of array type must be traced (so the held array survives GC).
#[test]
fn test_array_class_field_traced() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

class Bag {
    pub items: Array<String>;
    pub static fn new(items: Array<String>) -> Bag { return new Bag(items); }
    pub fn first(self) -> String { return self.items[0]; }
}
fn main() {
    let b = Bag::new(["x", "y"]);
    gc_collect();
    let junk: Array<i64> = [];
    let mut i = 0;
    while i < 200 { junk.push(i); i = i + 1; }
    println(b.first());
}
"#,
    );
    assert!(ok, "array-typed class field must be traced as a GC ref");
    assert_eq!(out, "x\n");
}
