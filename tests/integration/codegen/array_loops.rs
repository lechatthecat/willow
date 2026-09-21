use super::*;

// ── For loops over Array<T> (willow-for-loop) ───────────────────────────────
// 20 explicit perspectives: scalar/reference elements, control-flow nesting,
// scoping, diagnostics, evaluation order, GC, and cooperative async.

// Perspective 1: i64 elements can be accumulated.
#[test]
fn test_for_loop_perspective_01_i64_sum() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [2, 4, 6, 8];
    let mut total = 0;
    for x in xs {
        total = total + x;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "20\n");
}

// Perspective 2: an empty array executes the body zero times.
#[test]
fn test_for_loop_perspective_02_empty_array_skips_body() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [];
    let mut count = 7;
    for _ in xs {
        count = count + 100;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

// Perspective 3: a single-element array executes the body exactly once.
#[test]
fn test_for_loop_perspective_03_single_element_runs_once() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [42];
    let mut count = 0;
    for x in xs {
        println(x);
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n1\n");
}

// Perspective 4: bool elements work with ordinary branch logic.
#[test]
fn test_for_loop_perspective_04_bool_elements_drive_if() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let flags: Array<bool> = [true, false, true];
    let mut yes = 0;
    for flag in flags {
        if flag {
            yes = yes + 1;
        }
    }
    println(yes);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

// Perspective 5: f64 elements preserve their bit representation through the loop.
#[test]
fn test_for_loop_perspective_05_f64_accumulation() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let values: Array<f64> = [0.5, 1.25];
    let mut total = 0.0;
    for value in values {
        total = total + value;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1.75\n");
}

// Perspective 6: String elements are usable as GC-managed references.
#[test]
fn test_for_loop_perspective_06_string_concat() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let parts: Array<String> = ["will", "ow"];
    let mut text = "";
    for part in parts {
        text = text + part;
    }
    println(text);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "willow\n");
}

// Perspective 7: class instances can be iterated and called through.
#[test]
fn test_for_loop_perspective_07_object_elements_methods() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

class Score {
    pub value: i64;
    pub static fn new(value: i64) -> Score {
        return new Score(value);
    }
    pub fn get(self) -> i64 {
        return self.value;
    }
}

fn main() {
    let scores: Array<Score> = [Score::new(4), Score::new(5)];
    let mut total = 0;
    for score in scores {
        total = total + score.get();
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

// Perspective 8: nested for loops compose.
#[test]
fn test_for_loop_perspective_08_nested_for_loops() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let left: Array<i64> = [1, 2];
    let right: Array<i64> = [10, 20];
    let mut total = 0;
    for a in left {
        for b in right {
            total = total + a + b;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "66\n");
}

// Perspective 9: for loops can live inside while loops.
#[test]
fn test_for_loop_perspective_09_for_inside_while() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    let mut round = 0;
    let mut total = 0;
    while round < 2 {
        for x in xs {
            total = total + x;
        }
        round = round + 1;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// Perspective 10: while loops can live inside for loop bodies.
#[test]
fn test_for_loop_perspective_10_while_inside_for() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let limits: Array<i64> = [1, 3];
    let mut total = 0;
    for limit in limits {
        let mut i = 0;
        while i < limit {
            total = total + 1;
            i = i + 1;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

// Perspective 11: the loop variable shadows an outer binding only in the loop.
#[test]
fn test_for_loop_perspective_11_loop_var_shadows_outer_and_restores() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let value = 99;
    let xs: Array<i64> = [1, 2];
    for value in xs {
        println(value);
    }
    println(value);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n99\n");
}

// Perspective 12: `_` discards the element but still counts iterations.
#[test]
fn test_for_loop_perspective_12_underscore_discards_element() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [3, 4, 5];
    let mut count = 0;
    for _ in xs {
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

// Perspective 13: the iterable expression is evaluated once before iteration.
#[test]
fn test_for_loop_perspective_13_iterable_expression_evaluated_once() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn make() -> Array<i64> {
    println(70);
    return [1, 2];
}

fn main() {
    for x in make() {
        println(x);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "70\n1\n2\n");
}

// Perspective 14: arrays returned from functions can be iterated directly.
#[test]
fn test_for_loop_perspective_14_iterates_returned_array() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn make() -> Array<i64> {
    return [7, 8, 9];
}

fn main() {
    let mut total = 0;
    for value in make() {
        total = total + value;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "24\n");
}

// Perspective 15: arrays passed as parameters can be iterated in callees.
#[test]
fn test_for_loop_perspective_15_iterates_array_parameter() {
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
    let values: Array<i64> = [5, 6, 7];
    println(sum(values));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "18\n");
}

// Perspective 16: reference elements stay live across GC stress while iterating.
#[test]
fn test_for_loop_perspective_16_reference_elements_survive_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

fn main() {
    let names: Array<String> = ["a", "b", "c"];
    for name in names {
        gc_collect();
        println(name + "!");
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!\nb!\nc!\n");
}

// Perspective 17: element reads observe array mutations made before later turns.
#[test]
fn test_for_loop_perspective_17_mutating_array_during_iteration() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3];
    let mut total = 0;
    for x in xs {
        total = total + x;
        if x == 1 {
            xs[1] = 20;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "24\n");
}

// Perspective 18: loop variables are immutable.
#[test]
fn test_for_loop_perspective_18_loop_var_assignment_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    for value in xs {
        value = 9;
    }
}
"#,
        &[
            "error[E0301]",
            "cannot assign to immutable variable `value`",
        ],
    );
}

// Perspective 19: loop variables do not leak out of the loop body.
#[test]
fn test_for_loop_perspective_19_loop_var_is_scoped_to_body() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    for value in xs {
        println(value);
    }
    println(value);
}
"#,
        &["error[E0350]", "cannot find variable `value`"],
    );
}

// Perspective 20: await works inside for loops in both async main and leaf fns.
#[test]
fn test_for_loop_perspective_20_async_await_in_main_and_leaf() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn sum(values: FrozenArray<i64>) -> i64 {
    let mut total = 0;
    let mut index = 0;
    while index < values.len() {
        await sleep(1);
        total = total + values[index];
        index = index + 1;
    }
    return total;
}

async fn main() {
    let visible: Array<i64> = [1, 2];
    for value in visible {
        await sleep(1);
        println(value);
    }

    let hidden: Array<i64> = [3, 4];
    let total = await sum(hidden.freeze());
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n7\n");
}
