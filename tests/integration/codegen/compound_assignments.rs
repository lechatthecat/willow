use super::*;

#[test]
fn compound_assignment_all_operators_and_places() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
class Cell { pub n: i64; pub init(self) { self.n = 10; } }
fn main() {
    let mut x = 10;
    x += 2; x -= 1; x *= 3; x /= 2; x %= 7;
    println(x);
    let c = new Cell();
    c.n += 2; c.n -= 1; c.n *= 3; c.n /= 2; c.n %= 7;
    println(c.n);
    let mut a: Array<i64> = [10];
    a[0] += 2; a[0] -= 1; a[0] *= 3; a[0] /= 2; a[0] %= 7;
    println(a[0]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n2\n2\n");
}

#[test]
fn compound_index_evaluates_array_and_index_once() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
class Owner {
    pub calls: i64;
    pub values: Array<i64>;
    pub init(self) { self.calls = 0; self.values = [10]; }
    pub fn array(self) -> Array<i64> { self.calls = self.calls + 1; return self.values; }
    pub fn index(self) -> i64 { self.calls = self.calls + 1; return 0; }
}
fn main() {
    let owner = new Owner();
    owner.array()[owner.index()] += 5;
    println(owner.calls);
    println(owner.values[0]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n15\n");
}

#[test]
fn compound_field_evaluates_receiver_once() {
    let (out, ok) = compile_and_run(
        r#"
class Cell { pub n: i64; pub init(self) { self.n = 4; } }
class Owner {
    pub calls: i64;
    pub cell: Cell;
    pub init(self) { self.calls = 0; self.cell = new Cell(); }
    pub fn target(self) -> Cell { self.calls = self.calls + 1; return self.cell; }
}
fn main() {
    let owner = new Owner();
    owner.target().n += 3;
    println(owner.calls);
    println(owner.cell.n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n7\n");
}

#[test]
fn raw_newline_string_runs_and_preserves_escapes() {
    let (out, ok) =
        compile_and_run("fn main() { println(\"one\\ntwo\"); println(\"three\nfour\"); }");
    assert!(ok);
    assert_eq!(out, "one\ntwo\nthree\nfour\n");
}

#[test]
fn compound_assignment_preserves_immutable_local_error() {
    assert_compile_error_contains(
        "fn main() { let x = 1; x += 2; }",
        &["cannot assign to immutable variable `x`"],
    );
}

#[test]
fn compound_self_field_in_constructor_remains_visible_to_flow_analysis() {
    let (out, ok) = compile_and_run(
        "class Counter { pub n: i64; pub init(self) { self.n = 1; self.n += 2; } } fn main() { let c = new Counter(); println(c.n); }",
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn compound_assignment_rejects_non_place_target() {
    assert_compile_error_contains(
        "fn id(x: i64) -> i64 { return x; } fn main() { id(1) += 2; }",
        &["invalid compound assignment target"],
    );
}
