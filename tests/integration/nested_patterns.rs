//! Nested patterns, let chains and enum names omitted inside patterns, run
//! end to end in debug and release builds. Checker diagnostics and the
//! exhaustiveness scaling measurements live in
//! `src/semantic/type_checker/nested_pattern_tests.rs`.
//!
//! Perspectives:
//!   1  variant inside variant (`Ok(Some(n))`) with literal, guarded and catch-all rows
//!   2  enum inside a user enum (`Tagged::Pair(Shape::Circle(r), Color::Red)`)
//!   3  tuples nesting options and strings (`(0, None)`, `(x, Some(s))`)
//!   4  deep nesting: ten `Some` levels, each level reaching its own arm
//!   5  let chain: every step position failing, and all passing
//!   6  let chain mixing `if` conditions with `let` steps that use earlier bindings
//!   7  `let ... else` with a nested pattern, both diverging and binding
//!   8  `while let` with a nested pattern stops at the first non-matching value
//!   9  omitted enum names for the scrutinee's own enum (`Some(Circle(r))`)
//!  10  generic enum payloads instantiated with `Option<i64>`
//!  11  interface downcast nested in `Option` (`Some(Dog(d))`)
//!  12  a nested guard that collects garbage before falling through (GC roots)
//!  13  class payloads bound from nested positions survive a collection
//!  14  async: a let-chain step awaiting its scrutinee, bindings live across awaits
//!  15  async: a guard `let` step awaiting inside a nested `match` arm
//!  16  closures capturing bindings of nested patterns and let chains
//!  17  `bool` and integer literals inside payloads
//!  18  nested patterns in a match guard's `let` step (`P if let Q = e`)
//!  19  a binding repeated across arms (shadowing in separate arm scopes)
//!  20  effect contracts see the scrutinee of every let-chain step
//!  21  the runnable example `example/nested_patterns.wi` (runtime/examples.rs)
use crate::support::*;

fn assert_runs(source: &str, expected: &str) {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "debug run failed:\n{out}");
    assert_eq!(out, expected, "debug");
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "release run failed:\n{out}");
    assert_eq!(out, expected, "release");
}

#[test]
fn nested_variants_tuples_and_deep_nesting_run() {
    assert_runs(
        r#"
enum Color { Red, Green, Blue }
enum Shape { Circle(i64), Rect(i64, i64) }
enum Tagged { Pair(Shape, Color), Named(String, Option<i64>) }

fn classify(r: Result<Option<i64>, String>) -> String {
    return match r {
        Ok(Some(0)) => "zero",
        Ok(Some(n)) if n < 0 => "negative",
        Ok(Some(n)) => "positive " + n.to_string(),
        Ok(None) => "none",
        Err(e) => "error " + e,
    };
}

fn tag(t: Tagged) -> String {
    return match t {
        Tagged::Pair(Shape::Circle(r), Color::Red) => "red circle " + r.to_string(),
        Tagged::Pair(Shape::Rect(w, h), _) if w == h => "square",
        Tagged::Pair(_, Color::Blue) => "blue thing",
        Tagged::Pair(_, _) => "other pair",
        Tagged::Named(name, Some(v)) => name + "=" + v.to_string(),
        Tagged::Named(name, None) => name,
    };
}

fn tup(p: (i64, Option<String>)) -> String {
    return match p {
        (0, None) => "origin",
        (x, Some(s)) => s + x.to_string(),
        (x, None) => x.to_string(),
    };
}

fn depth(o: Option<Option<Option<Option<Option<Option<Option<Option<Option<Option<i64>>>>>>>>>>) -> i64 {
    return match o {
        Some(Some(Some(Some(Some(Some(Some(Some(Some(Some(v)))))))))) => v,
        Some(Some(Some(Some(Some(None))))) => 5,
        Some(Some(Some(Some(Some(Some(_)))))) => 6,
        Some(Some(None)) => 2,
        Some(_) => 1,
        None => 0,
    };
}

fn main() {
    println(classify(Ok(Some(0))));
    println(classify(Ok(Some(-3))));
    println(classify(Ok(Some(5))));
    println(classify(Ok(None)));
    println(classify(Err("bad")));
    println(tag(Tagged::Pair(Shape::Circle(4), Color::Red)));
    println(tag(Tagged::Pair(Shape::Rect(2, 2), Color::Red)));
    println(tag(Tagged::Pair(Shape::Rect(2, 3), Color::Blue)));
    println(tag(Tagged::Pair(Shape::Circle(1), Color::Green)));
    println(tag(Tagged::Named("k", Some(9))));
    println(tag(Tagged::Named("j", None)));
    println(tup((0, None)));
    println(tup((3, Some("s"))));
    println(tup((7, None)));
    println(depth(Some(Some(Some(Some(Some(Some(Some(Some(Some(Some(42))))))))))));
    println(depth(Some(Some(Some(Some(Some(None)))))));
    println(depth(Some(Some(Some(Some(Some(Some(None))))))));
    println(depth(Some(Some(None))));
    println(depth(Some(None)));
    println(depth(None));
}
"#,
        "zero\nnegative\npositive 5\nnone\nerror bad\nred circle 4\nsquare\nblue thing\nother pair\nk=9\nj\norigin\ns3\n7\n42\n5\n6\n2\n1\n0\n",
    );
}

#[test]
fn let_chains_let_else_and_while_let_run() {
    assert_runs(
        r#"
import std::collections::Array;

fn chain(a: Option<i64>, b: Result<i64, String>) -> i64 {
    if let Some(x) = a && let Ok(y) = b && x < y {
        return x + y;
    }
    return -1;
}

fn steps(a: Option<(i64, i64)>, scale: i64) -> i64 {
    if scale > 0 && let Some((x, y)) = a && x != y && let Some(z) = Option::Some(x * scale) {
        return z + y;
    }
    return 0;
}

fn unwrap2(o: Option<Option<i64>>) -> i64 {
    let Some(Some(v)) = o else { return -1; };
    return v;
}

fn main() {
    println(chain(Some(1), Ok(5)));
    println(chain(Some(9), Ok(5)));
    println(chain(None, Ok(5)));
    println(chain(Some(1), Err("x")));
    println(steps(Some((2, 3)), 10));
    println(steps(Some((2, 2)), 10));
    println(steps(Some((2, 3)), 0));
    println(steps(None, 10));
    println(unwrap2(Some(Some(42))));
    println(unwrap2(Some(None)));
    println(unwrap2(None));
    let items: Array<Option<Result<i64, String>>> = [Some(Ok(1)), Some(Ok(2)), Some(Err("stop")), Some(Ok(4))];
    let mut i = 0;
    let mut total = 0;
    while let Some(Ok(v)) = items[i] {
        total = total + v;
        i = i + 1;
    }
    println(total);
    // The leading bound check runs first, so `items[4]` is never read.
    let mut k = 3;
    let mut tail = 0;
    while k < items.len() && let Some(Ok(v)) = items[k] {
        tail = tail + v;
        k = k + 1;
    }
    println(tail);
    let words: Array<Option<(String, i64)>> = [Some(("a", 1)), None, Some(("b", 2))];
    let mut j = 0;
    while j < words.len() {
        let Some((w, n)) = words[j] else { j = j + 1; continue; };
        println(w + n.to_string());
        j = j + 1;
    }
}
"#,
        "6\n-1\n-1\n-1\n23\n0\n0\n0\n42\n-1\n-1\n3\n4\na1\nb2\n",
    );
}

#[test]
fn omitted_enum_names_generics_and_downcasts_run() {
    assert_runs(
        r#"
interface Animal { fn legs(self) -> i64; }
class Dog implements Animal { pub n: i64; pub fn legs(self) -> i64 { return 4; } }
class Bird implements Animal { pub n: i64; pub fn legs(self) -> i64 { return 2; } }
enum Pair<T> { Both(T, T), One(T), Zero }
enum Shape { Circle(i64), Rect(i64, i64) }
enum Flag { On(bool), Level(i64) }

fn pet(o: Option<Animal>) -> String {
    return match o {
        Some(Dog(d)) if d.n > 1 => "big dog",
        Some(Dog(d)) => "dog",
        Some(Bird(_)) => "bird",
        Some(_) => "other",
        None => "nobody",
    };
}

fn pairs(p: Pair<Option<i64>>) -> i64 {
    return match p {
        Pair::Both(Some(a), Some(b)) => a + b,
        Pair::Both(Some(a), None) => a,
        Pair::Both(None, _) => -1,
        Pair::One(Some(v)) => v * 10,
        Pair::One(None) => -2,
        Pair::Zero => 0,
    };
}

fn area(s: Option<Shape>) -> i64 {
    return match s {
        Some(Circle(r)) => 3 * r * r,
        Some(Rect(w, h)) => w * h,
        None => 0,
    };
}

fn flag(f: Option<Flag>) -> i64 {
    return match f {
        Some(Flag::On(true)) => 1,
        Some(Flag::On(false)) => 0,
        Some(Flag::Level(-1)) => -100,
        Some(Flag::Level(n)) => n,
        None => -1,
    };
}

fn main() {
    println(pet(Some(new Dog(3))));
    println(pet(Some(new Dog(1))));
    println(pet(Some(new Bird(1))));
    println(pet(None));
    println(pairs(Pair::Both(Some(1), Some(2))));
    println(pairs(Pair::Both(Some(5), None)));
    println(pairs(Pair::Both(None, Some(3))));
    println(pairs(Pair::One(Some(4))));
    println(pairs(Pair::One(None)));
    println(pairs(Pair::Zero));
    println(area(Some(Shape::Circle(2))));
    println(area(Some(Shape::Rect(2, 5))));
    println(area(None));
    println(flag(Some(Flag::On(true))));
    println(flag(Some(Flag::On(false))));
    println(flag(Some(Flag::Level(-1))));
    println(flag(Some(Flag::Level(7))));
    println(flag(None));
}
"#,
        "big dog\ndog\nbird\nnobody\n3\n5\n-1\n40\n-2\n0\n12\n10\n0\n1\n0\n-100\n7\n-1\n",
    );
}

#[test]
fn nested_bindings_survive_collections_in_guards() {
    assert_runs(
        r#"
class Node { pub value: i64; pub label: String; }
enum Tree { Leaf(Node), Branch(Tree, Tree) }

fn churn() -> bool {
    let mut i = 0;
    while i < 500 {
        let n = new Node(i, "x" + i.to_string());
        i = i + n.label.len() - n.label.len() + 1;
    }
    gc_minor_collect();
    return true;
}

fn sum(t: Tree) -> i64 {
    return match t {
        Tree::Branch(Tree::Leaf(a), Tree::Leaf(b)) if churn() && a.value > 100 => 0,
        Tree::Branch(Tree::Leaf(a), Tree::Leaf(b)) if churn() => a.value + b.value + a.label.len(),
        Tree::Branch(Tree::Branch(x, y), right) if churn() => sum(x) + sum(y) + sum(right),
        Tree::Branch(left, right) => sum(left) + sum(right),
        Tree::Leaf(n) => n.value,
    };
}

fn main() {
    let t = Tree::Branch(
        Tree::Branch(Tree::Leaf(new Node(1, "a")), Tree::Leaf(new Node(2, "bb"))),
        Tree::Branch(Tree::Leaf(new Node(3, "c")), Tree::Branch(Tree::Leaf(new Node(4, "d")), Tree::Leaf(new Node(5, "e")))),
    );
    println(sum(t));
    let o: Option<(Node, Option<Node>)> = Some((new Node(7, "seven"), Some(new Node(8, "eight"))));
    if let Some((first, Some(second))) = o && churn() && let Some(n) = Option::Some(first.value + second.value) {
        println(first.label + second.label + n.to_string());
    }
}
"#,
        "16\nseveneight15\n",
    );
}

#[test]
fn async_let_chains_and_guards_with_nested_patterns_run() {
    assert_runs(
        r#"
async fn lookup(n: i64) -> Option<i64> {
    await sleep(1);
    if n > 0 { return Some(n * 2); }
    return None;
}

async fn pipeline(o: Option<Result<i64, String>>) -> i64 {
    if let Some(Ok(n)) = o && let Some(doubled) = await lookup(n) && doubled > 4 {
        await sleep(1);
        return doubled + n;
    }
    return match o {
        Some(Err(msg)) if let Some(6) = await lookup(msg.len()) => 100,
        Some(Err(msg)) => msg.len(),
        Some(Ok(_)) => -1,
        None => -2,
    };
}

async fn main() {
    println(await pipeline(Some(Ok(5))));
    println(await pipeline(Some(Ok(1))));
    println(await pipeline(Some(Err("abc"))));
    println(await pipeline(Some(Err("ab"))));
    println(await pipeline(None));
}
"#,
        "15\n-1\n100\n2\n-2\n",
    );
}

#[test]
fn closures_literals_guard_lets_and_shadowing_run() {
    assert_runs(
        r#"
import std::collections::Array;

fn pick(o: Option<(i64, Option<i64>)>) -> i64 {
    return match o {
        Some((x, Some(y))) if let Some(z) = Option::Some(x + y) && z > 10 => z,
        Some((x, Some(x2))) => x * x2,
        Some((x, None)) => x,
        None => 0,
    };
}

fn main() {
    let items: Array<Option<Result<i64, String>>> = [Some(Ok(1)), Some(Ok(2)), None, Some(Ok(4))];
    let fs: Array<fn() -> i64> = [];
    for o in items {
        if let Some(Ok(v)) = o && v % 2 == 0 {
            fs.push(|| v * 100);
        }
        match o {
            Some(Ok(v)) if v == 1 => fs.push(|| v),
            _ => {}
        }
    }
    for f in fs { println(f()); }
    println(pick(Some((5, Some(6)))));
    println(pick(Some((2, Some(3)))));
    println(pick(Some((9, None))));
    println(pick(None));
}
"#,
        "1\n200\n400\n11\n6\n9\n0\n",
    );
}

#[test]
fn effect_contracts_see_let_chain_scrutinees() {
    assert_compile_error_contains(
        "fn noisy(n: i64) -> Option<i64> { println(n); return Some(n); } \
         @effects(no_io) fn a(o: Option<Option<i64>>) -> i64 { \
         if let Some(Some(x)) = o && let Some(y) = noisy(x) { return y; } return 0; } \
         fn main() { println(a(None)); }",
        &["E2610", "no_io"],
    );
    assert_compile_error_contains(
        "fn noisy(n: i64) -> bool { println(n); return true; } \
         @effects(no_io) fn b(o: Option<Option<i64>>) -> i64 { \
         return match o { Some(Some(v)) if noisy(v) => v, _ => 0 }; } \
         fn main() { println(b(None)); }",
        &["E2610", "no_io"],
    );
    assert_runs(
        "fn quiet(n: i64) -> Option<i64> { return Some(n); } \
         @effects(no_io) fn c(o: Option<Option<i64>>) -> i64 { \
         if let Some(Some(x)) = o && let Some(y) = quiet(x) { return y; } \
         let Some(Some(v)) = o else { return 0; }; return v; } \
         fn main() { println(c(Some(Some(3)))); println(c(None)); }",
        "3\n0\n",
    );
}
