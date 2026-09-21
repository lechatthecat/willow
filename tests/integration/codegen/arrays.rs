use super::*;

// ── Array<T> type (willow-xqm) ─────────────────────────────────────────────
// GC-managed arrays: literals, indexing (read/write), `.len()`, bounds checks.
// Element types cover scalars (i64/bool/f64) and GC references (String/object).

// Perspective 1: i64 literal, .len(), and index reads.
#[test]
fn test_array_i64_literal_len_and_index() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [10, 20, 30];
    println(xs.len());
    println(xs[0]);
    println(xs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n10\n30\n");
}

// Perspective 2: element assignment `xs[i] = v`.
#[test]
fn test_array_index_assignment() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3];
    xs[0] = 100;
    xs[2] = 300;
    println(xs[0]);
    println(xs[1]);
    println(xs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\n2\n300\n");
}

// Perspective 3: iterate with `.len()` and index, accumulating.
#[test]
fn test_array_sum_loop() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [5, 15, 25, 55];
    let mut i = 0;
    let mut sum = 0;
    while i < xs.len() {
        sum = sum + xs[i];
        i = i + 1;
    }
    println(sum);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\n");
}

#[test]
fn test_array_for_loop_sum() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn sum(values: Array<i64>) -> i64 {
    let mut total = 0;
    for value in values {
        total = total + value;
    }
    return total;
}

fn main() {
    let values: Array<i64> = [1, 1, 2, 3, 5, 8];
    println(values[0]);
    println(values.len());
    println(sum(values));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n6\n20\n");
}

#[test]
fn test_array_for_loop_gc_elements_survive_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

fn main() {
    let names: Array<String> = ["a", "b", "c"];
    for name in names {
        let message = name + "!";
        println(message);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!\nb!\nc!\n");
}

#[test]
fn test_for_loop_requires_array_iterable() {
    assert_compile_error_contains(
        r#"
fn main() {
    for value in 123 {
        println(value);
    }
}
"#,
        &[
            "error[E0201]",
            "cannot iterate over `i64`",
            "for-in requires an array",
        ],
    );
}

// Perspective 4: bool elements.
#[test]
fn test_array_bool_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let bs: Array<bool> = [true, false, true];
    println(bs[0]);
    println(bs[1]);
    println(bs.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n3\n");
}

// Perspective 5: f64 elements (exercises the f64<->word bitcast).
#[test]
fn test_array_f64_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let fs: Array<f64> = [1.5, 2.5, 3.0];
    println(fs[0] + fs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4.5\n");
}

// Perspective 6: String (reference) elements round-trip.
#[test]
fn test_array_string_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let names: Array<String> = ["alice", "bob", "carol"];
    println(names.len());
    println(names[0]);
    println(names[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\nalice\ncarol\n");
}

// Perspective 7: an array passed as a function parameter.
#[test]
fn test_array_as_parameter() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn total(xs: Array<i64>) -> i64 {
    let mut i = 0;
    let mut s = 0;
    while i < xs.len() { s = s + xs[i]; i = i + 1; }
    return s;
}
fn main() {
    let xs: Array<i64> = [10, 20, 30];
    println(total(xs));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "60\n");
}

// Perspective 8: an array returned from a function.
#[test]
fn test_array_returned_from_function() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn make() -> Array<i64> {
    return [7, 8, 9];
}
fn main() {
    let xs = make();
    println(xs.len());
    println(xs[1]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n8\n");
}

// Perspective 9: array of class instances, with method calls on elements.
#[test]
fn test_array_of_objects() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

class P {
    pub val: i64;
    pub static fn new(v: i64) -> P { return new P(v); }
    pub fn get(self) -> i64 { return self.val; }
}
fn main() {
    let ps: Array<P> = [P::new(7), P::new(8)];
    println(ps[0].get());
    println(ps[1].get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n8\n");
}

// Perspective 10: empty array with annotation has length 0.
#[test]
fn test_array_empty_annotated() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [];
    println(xs.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// Perspective 11: single-element array.
#[test]
fn test_array_single_element() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [42];
    println(xs.len());
    println(xs[0]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n42\n");
}

// Perspective 12: read back a written reference element.
#[test]
fn test_array_string_write_then_read() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<String> = ["a", "b"];
    xs[0] = "changed";
    println(xs[0]);
    println(xs[1]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "changed\nb\n");
}

// Perspective 13: doubling each element in place.
#[test]
fn test_array_mutate_in_loop() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3, 4];
    let mut i = 0;
    while i < xs.len() {
        xs[i] = xs[i] * 2;
        i = i + 1;
    }
    println(xs[0]);
    println(xs[3]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n8\n");
}

// Perspective 14: `.len()` used directly in an arithmetic expression.
#[test]
fn test_array_len_in_expression() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3, 4, 5];
    println(xs.len() * 2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// Perspective 15: string array survives a GC collection while held live.
#[test]
fn test_array_string_elements_survive_gc() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let names: Array<String> = ["alpha", "beta", "gamma"];
    gc_collect();
    println(names[0]);
    println(names[2]);
}
"#,
    );
    assert!(ok, "array string elements must survive GC");
    assert_eq!(out, "alpha\ngamma\n");
}

// Perspective 16: out-of-bounds read aborts with a clear message.
#[test]
fn test_array_index_out_of_bounds_read_aborts() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    println(xs[5]);
}
"#,
    );
    assert!(!ok, "out-of-bounds read must abort");
    assert!(
        out.contains("out of bounds"),
        "expected an out-of-bounds panic message, got: {out}"
    );
}

// Perspective 17: out-of-bounds write aborts.
#[test]
fn test_array_index_out_of_bounds_write_aborts() {
    let (_out, ok) = compile_and_run_check_exit(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<i64> = [1, 2];
    xs[9] = 0;
}
"#,
    );
    assert!(!ok, "out-of-bounds write must abort");
}

// Perspective 18: a negative index aborts.
#[test]
fn test_array_negative_index_aborts() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    let i = 0 - 1;
    println(xs[i]);
}
"#,
    );
    assert!(!ok, "negative index must abort");
    assert!(out.contains("out of bounds"), "got: {out}");
}

// Perspective 19: indexing with a non-i64 type is a compile error.
#[test]
fn test_array_index_non_i64_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    println(xs[true]);
}
"#,
        &["error[E0201]", "index must be `i64`"],
    );
}

// Perspective 20: indexing a non-array value is a compile error.
#[test]
fn test_array_index_non_array_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: i64 = 5;
    println(x[0]);
}
"#,
        &["error[E0201]", "cannot index a value of type `i64`"],
    );
}

// Perspective 21: mismatched element types in a literal is a compile error.
#[test]
fn test_array_mixed_element_types_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    let xs = [1, true, 3];
    println(xs.len());
}
"#,
        &["error[E0201]", "array elements must have the same type"],
    );
}

// Perspective 22: an unknown array method is a compile error.
#[test]
fn test_array_unknown_method_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    println(xs.first());
}
"#,
        &["error[E0201]", "no method `first` on `Array<i64>`"],
    );
}

// Perspective 23: assigning the wrong element type is a compile error.
#[test]
fn test_array_element_assign_type_mismatch_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3];
    xs[0] = true;
}
"#,
        &["error[E0201]"],
    );
}
