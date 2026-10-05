//! `if let`, `while let` and match guards (willow-jz15.6).
//!
//! `if let P = e { A } else { B }` is parsed as `match e { P => { A }, _ => { B } }`
//! and `while let P = e { A }` as `while true { match e { P => { A }, _ => {
//! break; } } }`. A guard `P if cond => ...` is native: it is evaluated after
//! the pattern binds, and a false guard clears the arm's GC roots and falls
//! through to the next arm. A guarded arm never counts toward exhaustiveness.
//! Type-checker diagnostics live in `src/semantic/type_checker/match_guard_tests.rs`.
//!
//! Perspectives:
//!   1  guard on an `i64` literal arm (`0 if false`) falls through to `0`
//!   2  guard on a binding with `&&`
//!   3  guard containing a nested `match`
//!   4  guarded enum payload arms fall through to an unguarded sibling
//!   5  guard on a class downcast binding (`Dog(d) if d.n > 1`)
//!   6  guard on a `bool` literal arm
//!   7  a closure capturing a binding of a guarded arm
//!   8  `if let` without `else`, both matching and not
//!   9  `if let ... else`, `else if let` ladders and `if ... else if let`
//!  10  `if let` with a fieldless variant pattern (`None`)
//!  11  `if let` returning from a function with a `String` payload
//!  12  `while let` with `continue` and `break`
//!  13  `while let` with `return` from inside the loop
//!  14  `while let` re-evaluates the scrutinee once per iteration
//!  15  `if let` in a constructor initializes a field on both paths
//!  16  a `String` binding survives a collection inside a failing guard (GC stress)
//!  17  a heap binding of `if let` survives a collection before a field write
//!  18  `await` inside a guard of an `async fn`
//!  19  `await` in a `while let` / `if let` scrutinee
//!  20  the same programs in release mode
//!  21  non-exhaustive errors when only a guarded arm covers a case (CLI)
//!  22  non-`bool` guard (CLI)
//!  23  irrefutable `if let` warns and still runs
//!  24  nested patterns remain rejected in `if let`
//!  25  the example program runs (see `runtime/examples.rs`)

use crate::support::*;

const GUARDS: &str = r#"enum Shape { Circle(i64), Rect(i64, i64), Empty }
interface Animal { fn legs(self) -> i64; }
class Dog implements Animal { pub n: i64; pub fn legs(self) -> i64 { return 4; } }
class Bird implements Animal { pub n: i64; pub fn legs(self) -> i64 { return 2; } }
fn classify(n: i64) -> String {
    return match n {
        0 if false => "never",
        0 => "zero",
        x if x < 0 && x > -10 => "small-neg",
        x if x < 0 => "neg",
        x if match x % 3 { 0 => true, _ => false } => "triple",
        x if x % 2 == 0 => "even",
        _ => "odd",
    };
}
fn area(s: Shape) -> i64 {
    return match s {
        Shape::Circle(r) if r > 10 => 999,
        Shape::Circle(r) => r * r * 3,
        Shape::Rect(w, h) if w == h => w * w,
        Shape::Rect(w, h) => w * h,
        Shape::Empty => 0,
    };
}
fn kind(a: Animal) -> String {
    return match a {
        Dog(d) if d.n > 1 => "pack",
        Dog(d) => "dog",
        _ => "other",
    };
}
fn flag(b: bool, c: bool) -> i64 {
    return match b {
        true if c => 1,
        true => 2,
        false => 3,
    };
}
fn apply(f: closure(i64) -> i64, v: i64) -> i64 { return f(v); }
fn capture(n: i64) -> i64 {
    match n {
        x if x > 0 => { return apply(|y: i64| x + y, 100); }
        _ => { return 0; }
    }
}
fn main() {
    println(classify(0));
    println(classify(-3));
    println(classify(-30));
    println(classify(9));
    println(classify(4));
    println(classify(7));
    println(area(Shape::Circle(2)));
    println(area(Shape::Circle(20)));
    println(area(Shape::Rect(3, 3)));
    println(area(Shape::Rect(3, 4)));
    println(area(Shape::Empty));
    println(kind(new Dog(3)));
    println(kind(new Dog(1)));
    println(kind(new Bird(5)));
    println(flag(true, true));
    println(flag(true, false));
    println(flag(false, true));
    println(capture(5));
    println(capture(-5));
}
"#;
const GUARDS_OUT: &str = "zero\nsmall-neg\nneg\ntriple\neven\nodd\n12\n999\n9\n12\n0\npack\ndog\nother\n1\n2\n3\n105\n0\n";

const IF_WHILE_LET: &str = r#"enum Token { Num(i64), Word(String), End }
class Queue { pub i: i64; pub n: i64; }
fn next(q: Queue) -> Option<i64> {
    if q.i < q.n { q.i = q.i + 1; return Some(q.i); }
    return None;
}
fn first_word(t: Token) -> String {
    if let Token::Word(w) = t { return w + "!"; }
    return "-";
}
fn sum_until(limit: i64) -> i64 {
    let q = new Queue(0, 100);
    let mut s = 0;
    while let Some(v) = next(q) {
        if v % 2 == 0 { continue; }
        if v > limit { break; }
        s = s + v;
    }
    return s;
}
fn find(target: i64) -> i64 {
    let q = new Queue(0, 10);
    while let Some(v) = next(q) {
        if v == target { return v * 10; }
    }
    return -1;
}
class Holder {
    pub v: i64;
    pub init(self, o: Option<i64>) {
        if let Some(x) = o { self.v = x; } else { self.v = -1; }
    }
}
fn main() {
    let a: Option<i64> = Some(5);
    let b: Option<i64> = None;
    if let Some(x) = a { println(x); }
    if let Some(x) = b { println(x); }
    if let Some(x) = b { println(x); } else { println("b none"); }
    if let Some(x) = b {
        println(x);
    } else if let Some(y) = a {
        println(y + 100);
    } else {
        println("none");
    }
    if let None = b { println("is none"); }
    let n = 3;
    if n > 5 { println("big"); } else if let Some(y) = a { println(y * n); }
    println(first_word(Token::Word("hi")));
    println(first_word(Token::Num(1)));
    println(first_word(Token::End));
    println(sum_until(9));
    println(find(4));
    println(find(40));
    println(new Holder(Some(7)).v);
    println(new Holder(None).v);
    let q = new Queue(0, 3);
    let mut calls = 0;
    while let Some(v) = next(q) { calls = calls + v; }
    println(calls);
    println(q.i);
}
"#;
const IF_WHILE_LET_OUT: &str = "5\nb none\n105\nis none\n15\nhi!\n-\n-\n25\n40\n-1\n7\n-1\n6\n3\n";

const HEAP_ASYNC: &str = r#"enum Note { Text(String), Num(i64) }
class Node { pub v: i64; }
enum Boxed { One(Node), Empty }
fn churn() -> i64 { gc_minor_collect(); return 0; }
fn long(s: String) -> bool { churn(); return s.len() > 3; }
fn label(n: Note) -> String {
    return match n {
        Note::Text(s) if long(s) => s + "+",
        Note::Text(s) => s + "-",
        Note::Num(k) if k > 0 => "pos",
        Note::Num(k) => "nonpos",
    };
}
fn bump(b: Boxed) -> i64 {
    if let Boxed::One(n) = b {
        churn();
        n.v = n.v + 1;
    }
    match b {
        Boxed::One(n) if n.v > 100 => { return -1; }
        Boxed::One(n) => { churn(); return n.v; }
        Boxed::Empty => { return 0; }
    }
}
async fn half(x: i64) -> i64 { await sleep(1); return x / 2; }
async fn pick(o: Option<i64>) -> i64 {
    match o {
        Some(v) if await half(v) > 2 => { return v; }
        Some(v) => { return 0 - v; }
        None => { return 0; }
    }
}
async fn step(k: i64) -> Option<i64> {
    if k > 0 { return Some(await half(k * 2)); }
    return None;
}
async fn drain(n: i64) -> i64 {
    let mut k = n;
    let mut total = 0;
    while let Some(v) = await step(k) {
        total = total + v;
        k = k - 1;
    }
    if let Some(w) = await step(10) { total = total + w; }
    return total;
}
async fn main() {
    let mut i = 0;
    let mut acc = 0;
    while i < 200 {
        let s = label(Note::Text(i % 2 == 0 ? "ab" + "cd" : "a" + "b"));
        acc = acc + s.len();
        i = i + 1;
    }
    println(acc);
    println(label(Note::Text("abcd")));
    println(label(Note::Text("ab")));
    println(label(Note::Num(1)));
    println(label(Note::Num(0)));
    println(bump(Boxed::One(new Node(4))));
    println(bump(Boxed::Empty));
    println(await pick(Some(10)));
    println(await pick(Some(3)));
    println(await pick(None));
    println(await drain(3));
}
"#;
const HEAP_ASYNC_OUT: &str = "800\nabcd+\nab-\npos\nnonpos\n5\n0\n10\n-3\n0\n16\n";

fn assert_runs(source: &str, expected: &str) {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "debug run failed:\n{out}");
    assert_eq!(out, expected, "debug");
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "release run failed:\n{out}");
    assert_eq!(out, expected, "release");
}

#[test]
fn match_guards_select_and_fall_through() {
    assert_runs(GUARDS, GUARDS_OUT);
}

#[test]
fn if_let_and_while_let_control_flow() {
    assert_runs(IF_WHILE_LET, IF_WHILE_LET_OUT);
}

#[test]
fn guard_and_if_let_bindings_stay_rooted_under_minor_gc_stress() {
    let (out, ok) = compile_and_run_with_env(HEAP_ASYNC, &[("WILLOW_GC_STRESS", "minor")]);
    assert!(ok, "{out}");
    assert_eq!(out, HEAP_ASYNC_OUT);
    let (out, ok) = compile_and_run_release(HEAP_ASYNC);
    assert!(ok, "{out}");
    assert_eq!(out, HEAP_ASYNC_OUT);
}

#[test]
fn guarded_only_coverage_is_rejected_by_the_cli() {
    assert_compile_error_contains(
        "enum E { A, B } fn f(e: E) -> i64 { return match e { E::A => 1, E::B if true => 2 }; } fn main() { println(f(E::A)); }",
        &["E1202", "variant `E::B` not covered"],
    );
    assert_compile_error_contains(
        "fn f(b: bool) -> i64 { return match b { true => 1, false if b => 2 }; } fn main() { println(f(true)); }",
        &["E1207"],
    );
    assert_compile_error_contains(
        "fn f(n: i64) -> i64 { return match n { x if x > 0 => 1, 0 => 2 }; } fn main() { println(f(1)); }",
        &["E1206"],
    );
    assert_compile_error_contains(
        "fn f(n: i64) -> i64 { return match n { x if x => 1, _ => 2 }; } fn main() { println(f(1)); }",
        &["E0203", "match guard must be `bool`, found `i64`"],
    );
    assert_compile_error_contains(
        "fn main() { let o: Option<Option<i64>> = None; if let Some(Some(x)) = o { println(x); } }",
        &["E0102", "nested patterns are not supported"],
    );
}

#[test]
fn irrefutable_if_let_still_runs() {
    let (out, ok) = compile_and_run(
        "fn main() { let n = 3; if let x = n { println(x + 1); } else { println(0); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}
