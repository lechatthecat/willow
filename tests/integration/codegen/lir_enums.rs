use super::*;

// ── Enum values and `match` on the LIR path (willow-0g8j.8) ────────────────
// The emitted-code half of willow-0g8j.8; the eligibility half is the m01-m28
// perspectives in `src/backend/cranelift/lir_gen.rs`. Every program below runs
// on both paths and must print the same thing, and the last four repeat that
// under `WILLOW_GC_STRESS=alloc` — the mode that collects at every allocation,
// so a scrutinee or half-built enum left unrooted is reclaimed and the output
// changes rather than staying accidentally correct.
//
// m29 a fieldless enum round-tripped through a function, m30 a payload variant
// destructured, m31 the representation rule (a payload-less variant of a
// payload-carrying enum is still a heap object), m32 a multi-payload variant,
// m33 an `f64` payload through the bitcast path, m34 a `String` payload, m35 a
// binding pattern aliasing the whole scrutinee, m36 an `i64` scrutinee with
// literal arms, m37 a `bool` scrutinee, m38 a match in statement position, m39
// nested matches, m40 an enum in a class field, m41 enums as array elements,
// m42 a `match` whose scrutinee is a call, m43 tag order independence, m44 an
// arm allocating while a payload binding is live, m45 the same under GC
// stress, m46 an enum constructed from allocating arguments under GC stress,
// m47 an interface payload boxed at construction under GC stress, m48 an array
// of payload enums traced under GC stress.

#[test]
fn lir_diff_26_fieldless_enum_roundtrip() {
    assert_program_output(
        r#"
enum Direction { North, East, South, West }
fn turn(d: Direction) -> Direction {
    return match d {
        Direction::North => Direction::East,
        Direction::East => Direction::South,
        Direction::South => Direction::West,
        _ => Direction::North
    };
}
fn name(d: Direction) -> String {
    return match d {
        Direction::North => "north",
        Direction::East => "east",
        Direction::South => "south",
        _ => "west"
    };
}
fn main() {
    println(name(Direction::North));
    println(name(turn(Direction::North)));
    println(name(turn(turn(Direction::West))));
}
"#,
        "north\neast\neast\n",
    );
}

#[test]
fn lir_diff_27_payload_variant_destructured() {
    assert_program_output(
        r#"
enum Shape { Nothing, Circle(i64) }
fn area(s: Shape) -> i64 {
    return match s {
        Shape::Circle(r) => r * r * 3,
        _ => 0
    };
}
fn main() {
    println(area(Shape::Circle(4)));
    println(area(Shape::Nothing));
}
"#,
        "48\n0\n",
    );
}

#[test]
fn lir_diff_28_payloadless_variant_of_a_payload_enum_is_still_an_object() {
    // The representation follows the ENUM, not the variant: `Nothing` carries
    // nothing, but because `Circle` does, a `Nothing` value is still a heap
    // object whose word 0 is the tag. Reading it as a bare tag would compare a
    // pointer against 0 and take the wrong arm.
    assert_program_output(
        r#"
enum Shape { Nothing, Circle(i64) }
fn tag_of(s: Shape) -> i64 {
    return match s {
        Shape::Nothing => 10,
        Shape::Circle(r) => 20
    };
}
fn main() {
    println(tag_of(Shape::Nothing));
    println(tag_of(Shape::Circle(1)));
}
"#,
        "10\n20\n",
    );
}

#[test]
fn lir_diff_29_multi_payload_variant() {
    assert_program_output(
        r#"
enum Shape { Nothing, Rect(i64, i64), Triple(i64, i64, i64) }
fn f(s: Shape) -> i64 {
    return match s {
        Shape::Rect(w, h) => w * h,
        Shape::Triple(a, b, c) => a + b + c,
        _ => -1
    };
}
fn main() {
    println(f(Shape::Rect(3, 5)));
    println(f(Shape::Triple(1, 2, 3)));
    println(f(Shape::Nothing));
}
"#,
        "15\n6\n-1\n",
    );
}

#[test]
fn lir_diff_30_float_payload_roundtrip() {
    // An `f64` payload is stored as a raw word and read back with a bitcast, so
    // a missing cast on either side shows up as a nonsense number rather than a
    // crash.
    assert_program_output(
        r#"
enum Val { Nothing, Num(f64), Mixed(i64, f64) }
fn to_f(v: Val) -> f64 {
    return match v {
        Val::Num(x) => x,
        Val::Mixed(n, x) => x,
        _ => -1.0
    };
}
fn main() {
    println(to_f(Val::Num(3.5)));
    println(to_f(Val::Mixed(2, 4.25)));
    println(to_f(Val::Nothing));
}
"#,
        "3.5\n4.25\n-1\n",
    );
}

#[test]
fn lir_diff_31_string_payload() {
    assert_program_output(
        r#"
enum Label { Anonymous, Named(String) }
fn text(l: Label) -> String {
    return match l {
        Label::Named(n) => n,
        _ => "anon"
    };
}
fn main() {
    println(text(Label::Named("plate")));
    println(text(Label::Anonymous));
}
"#,
        "plate\nanon\n",
    );
}

#[test]
fn lir_diff_32_binding_pattern_aliases_the_scrutinee() {
    assert_program_output(
        r#"
enum Shape { Nothing, Circle(i64), Rect(i64, i64) }
fn width(s: Shape) -> i64 {
    return match s {
        Shape::Rect(w, h) => w,
        other => -1
    };
}
fn kind(d: i64) -> i64 {
    return match d {
        1 => 1,
        rest => rest * 10
    };
}
fn main() {
    println(width(Shape::Rect(3, 5)));
    println(width(Shape::Circle(9)));
    println(kind(1));
    println(kind(4));
}
"#,
        "3\n-1\n1\n40\n",
    );
}

#[test]
fn lir_diff_33_int_literal_scrutinee() {
    assert_program_output(
        r#"
fn classify(n: i64) -> i64 {
    return match n {
        0 => 100,
        1 => 200,
        k => k * 3
    };
}
fn main() {
    println(classify(0));
    println(classify(1));
    println(classify(7));
    println(classify(-2));
}
"#,
        "100\n200\n21\n-6\n",
    );
}

#[test]
fn lir_diff_34_bool_scrutinee() {
    // The arm test compares an `i8`, so the expected constant has to be built
    // at that width — a mismatch is a verifier error, not a wrong answer.
    assert_program_output(
        r#"
fn parity(b: bool) -> String {
    return match b {
        true => "yes",
        false => "no"
    };
}
fn main() {
    println(parity(true));
    println(parity(false));
    println(parity(1 < 2));
}
"#,
        "yes\nno\nyes\n",
    );
}

#[test]
fn lir_diff_35_statement_position_match() {
    assert_program_output(
        r#"
enum Direction { North, South }
fn announce(d: Direction) {
    match d {
        Direction::North => println("north"),
        _ => println("elsewhere")
    }
}
fn main() {
    announce(Direction::North);
    announce(Direction::South);
}
"#,
        "north\nelsewhere\n",
    );
}

#[test]
fn lir_diff_36_nested_match() {
    // The inner match's merge block must leave the builder exactly where the
    // outer one expects it, or the outer arm's jump lands in the wrong block.
    assert_program_output(
        r#"
enum Color { Red, Blue }
enum Shape { Nothing, Circle(i64), Rect(i64, i64) }
fn f(c: Color, s: Shape) -> i64 {
    return match c {
        Color::Red => match s {
            Shape::Circle(r) => r,
            _ => 0
        },
        _ => match s {
            Shape::Rect(w, h) => w * h,
            _ => -1
        }
    };
}
fn main() {
    println(f(Color::Red, Shape::Circle(7)));
    println(f(Color::Red, Shape::Nothing));
    println(f(Color::Blue, Shape::Rect(2, 3)));
    println(f(Color::Blue, Shape::Circle(7)));
}
"#,
        "7\n0\n6\n-1\n",
    );
}

#[test]
fn lir_diff_37_enum_in_a_class_field() {
    assert_program_output(
        r#"
enum Color { Red, Blue }
enum Shape { Nothing, Circle(i64) }
class Course {
    pub facing: Color;
    pub outline: Shape;
}
fn describe(c: Course) -> i64 {
    let base = match c.facing {
        Color::Red => 100,
        _ => 200
    };
    return base + match c.outline {
        Shape::Circle(r) => r,
        _ => 0
    };
}
fn main() {
    println(describe(new Course(Color::Red, Shape::Circle(5))));
    println(describe(new Course(Color::Blue, Shape::Nothing)));
}
"#,
        "105\n200\n",
    );
}

#[test]
fn lir_diff_38_enum_array_elements() {
    assert_program_output(
        r#"
import std::collections::Array;
enum Shape { Nothing, Circle(i64), Rect(i64, i64) }
fn area(s: Shape) -> i64 {
    return match s {
        Shape::Circle(r) => r * r,
        Shape::Rect(w, h) => w * h,
        _ => 0
    };
}
fn total(xs: Array<Shape>) -> i64 {
    let mut sum = 0;
    let mut i = 0;
    while i < xs.len() {
        sum = sum + area(xs[i]);
        i = i + 1;
    }
    return sum;
}
fn main() {
    println(total([Shape::Circle(3), Shape::Rect(2, 4), Shape::Nothing]));
}
"#,
        "17\n",
    );
}

#[test]
fn lir_diff_39_match_on_a_call_result() {
    // The scrutinee is evaluated once, before any arm test — a re-evaluating
    // emitter would call `build` once per arm and the counter would drift.
    assert_program_output(
        r#"
enum Shape { Nothing, Circle(i64), Rect(i64, i64) }
fn build(n: i64) -> Shape {
    if n == 0 { return Shape::Nothing; }
    if n == 1 { return Shape::Circle(n + 3); }
    return Shape::Rect(n, n + 1);
}
fn f(n: i64) -> i64 {
    return match build(n) {
        Shape::Circle(r) => r,
        Shape::Rect(w, h) => w * h,
        _ => 0
    };
}
fn main() {
    println(f(0));
    println(f(1));
    println(f(3));
}
"#,
        "0\n4\n12\n",
    );
}

#[test]
fn lir_diff_40_arms_out_of_declaration_order() {
    // The arm chain tests in SOURCE order but compares against DECLARATION
    // tags, so writing the arms out of order must not change which one runs.
    assert_program_output(
        r#"
enum Grade { A, B, C, D }
fn score(g: Grade) -> i64 {
    return match g {
        Grade::D => 4,
        Grade::B => 2,
        Grade::A => 1,
        _ => 3
    };
}
fn main() {
    println(score(Grade::A));
    println(score(Grade::B));
    println(score(Grade::C));
    println(score(Grade::D));
}
"#,
        "1\n2\n3\n4\n",
    );
}

#[test]
fn lir_diff_41_arm_allocates_while_a_payload_binding_is_live() {
    assert_program_output(
        r#"
enum Label { Anonymous, Named(String) }
fn render(l: Label) -> String {
    return match l {
        Label::Named(n) => "<" + n + "/" + n + ">",
        _ => "<anon>"
    };
}
fn main() {
    println(render(Label::Named("plate")));
    println(render(Label::Anonymous));
}
"#,
        "<plate/plate>\n<anon>\n",
    );
}

#[test]
fn lir_diff_42_payload_binding_survives_gc_stress() {
    // The binding is a plain local aliasing a word inside the scrutinee, which
    // is only safe because the SCRUTINEE stays rooted for the whole match. Drop
    // that root and the concatenations below reclaim the string the binding
    // points at.
    assert_output_under_gc_stress(
        r#"
enum Label { Anonymous, Named(String) }
fn render(l: Label) -> String {
    return match l {
        Label::Named(n) => "<" + n + "/" + n + ">",
        _ => "<anon>"
    };
}
fn main() {
    let mut i = 0;
    while i < 20 {
        println(render(Label::Named("plate")));
        i = i + 1;
    }
}
"#,
        &"<plate/plate>\n".repeat(20),
    );
}

#[test]
fn lir_diff_43_half_built_enum_survives_allocating_arguments() {
    // The enum object is allocated, its tag stored, and only then are the
    // payload expressions evaluated — each of which allocates here. Without the
    // root over that window the half-built enum is reclaimed and the payload
    // stores land in freed memory.
    assert_output_under_gc_stress(
        r#"
enum Pair { Empty, Both(String, String) }
fn make(a: String, b: String) -> Pair {
    return Pair::Both(a + "!", b + "?");
}
fn text(p: Pair) -> String {
    return match p {
        Pair::Both(x, y) => x + y,
        _ => "empty"
    };
}
fn main() {
    let mut i = 0;
    while i < 20 {
        println(text(make("l", "r")));
        i = i + 1;
    }
}
"#,
        &"l!r?\n".repeat(20),
    );
}

#[test]
fn lir_diff_44_interface_payload_is_boxed_under_gc_stress() {
    // An interface-typed payload slot holds a BOX built from the DECLARED
    // payload type. Storing the raw object instead would dispatch through a
    // vtable word that is really a field.
    assert_output_under_gc_stress(
        r#"
interface Named { fn describe(self) -> String; }
class Marker implements Named {
    pub label: String;
    pub fn describe(self) -> String { return self.label + "!"; }
}
enum Tag { Untagged, Marked(Named) }
fn name(t: Tag) -> String {
    return match t {
        Tag::Marked(n) => n.describe(),
        _ => "untagged"
    };
}
fn main() {
    let mut i = 0;
    while i < 20 {
        println(name(Tag::Marked(new Marker("m"))));
        i = i + 1;
    }
    println(name(Tag::Untagged));
}
"#,
        &format!("{}untagged\n", "m!\n".repeat(20)),
    );
}

#[test]
fn lir_diff_45_enum_array_traced_under_gc_stress() {
    // A payload enum is a GC reference, so the array's element `is_ref` flag
    // has to say so — otherwise the collector walks past live elements.
    assert_output_under_gc_stress(
        r#"
import std::collections::Array;
enum Label { Anonymous, Named(String) }
fn text(l: Label) -> String {
    return match l {
        Label::Named(n) => n,
        _ => "anon"
    };
}
fn main() {
    let xs = [Label::Named("a"), Label::Anonymous, Label::Named("c")];
    let mut i = 0;
    while i < 20 {
        println(text(xs[0]) + text(xs[1]) + text(xs[2]));
        i = i + 1;
    }
}
"#,
        &"aanonc\n".repeat(20),
    );
}

#[test]
fn lirreq_53_enum_match_example_is_fully_lir() {
    // Same contract as the other examples: the header claims every free
    // function in the example is compiled from the lowered IR, and this mode is
    // what keeps that claim honest. Its class method compiles through
    // `compile_class_method_inner`, which the mode does not police.
    let source = include_str!("../../../example/lir_enum_match.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        ok,
        "example/lir_enum_match.wi must compile with every free function on the LIR path: {stderr}"
    );
}

#[test]
fn lirreq_54_generic_enums_are_claimed() {
    // The boundary moved in willow-0g8j.2.1: `Option` and `Result` are ordinary
    // prelude enums, so a function that matches one is inside the walker's
    // subset at all. Both representations appear here —
    // `Option<i64>` is boxed, `Option<String>` is the pointer niche — because
    // the walker has to pick between them per instantiation, not per enum.
    for source in [
        "fn f(x: Option<i64>) -> i64 { return match x { Some(v) => v, None => -1 }; }\n\
         fn main() { println(f(Some(2))); }",
        "fn f(x: Option<String>) -> String { return match x { Some(v) => v, None => \"-\" }; }\n\
         fn main() { println(f(Some(\"a\"))); }",
        "fn f(r: Result<i64, String>) -> i64 { return match r { Ok(v) => v, Err(e) => -1 }; }\n\
         fn main() { println(f(Ok(2))); }",
    ] {
        let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
        assert!(
            ok,
            "a generic enum must compile through the walker: {stderr}"
        );
    }
}

#[test]
fn lirreq_55_a_closure_combinator_is_claimed() {
    // The combinators arrived with function values (willow-0g8j.2.2): the
    // lambda is lifted to a top-level function and `map` calls it indirectly,
    // so both the caller and the lifted body are inside the walker's subset and
    // the build must accept the program rather than reject a body.
    let source = "fn f(x: Option<i64>) -> i64 { return x.map(|v: i64| v * 2).unwrap_or(-1); }\n\
                  fn main() { println(f(Some(2))); }";
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        ok,
        "a closure combinator over a supported lambda must stay on the LIR path: {stderr}"
    );
}

#[test]
fn lirreq_55b_an_unsupported_lambda_body_fails_the_build() {
    // The boundary moved but did not disappear. A lambda is a FUNCTION, so it
    // faces eligibility on its own terms, and the lifted body is what has to be
    // reported even though the function that takes it is perfectly eligible.
    // The unsupported body here is a static property whose TYPE is outside the
    // subset: a map keyed by a reference that is not a `String` has no `MapKey`
    // spelling the walker can pass. (Scalar `toString` served until it joined
    // the subset in willow-0g8j.2.5, any static property read until
    // willow-0g8j.2.4 gave the walker the same ancestry walk
    // `emit_static_field_read` uses, and an f64-keyed map until willow-0g8j.3
    // admitted every scalar key.)
    let source = "import std::collections::Map;\n\
                  class Config { pub static bad: Map<Map<String, i64>, i64> = Map::new(); }\n\
                  fn f(x: Option<i64>) -> i64 { return x.map(|v: i64| v + Config::bad.len()).unwrap_or(-1); }\n\
                  fn main() { println(f(Some(2))); }";
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        !ok,
        "a lambda whose body leaves the subset must fail the build, not be claimed by the walker"
    );
    assert!(
        stderr.contains("function `$lambda.")
            && stderr.contains("the static property `Config::bad`")
            && stderr.contains("invalid lowered IR"),
        "the refusal must name the construct that blocked it: {stderr}"
    );
}

#[test]
fn lirreq_56_option_result_example_is_fully_lir() {
    // Same contract as the other examples: the header claims every free
    // function in the example is compiled from the lowered IR, and this mode is
    // what keeps that claim honest. Its class method compiles through
    // `compile_class_method_inner`, which the mode does not police.
    let source = include_str!("../../../example/lir_option_result.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        ok,
        "example/lir_option_result.wi must compile with every free function on the LIR path: {stderr}"
    );
}

#[test]
fn lirreq_58_divergence_example_is_fully_lir() {
    // The divergence example (willow-0g8j.2.5) exists to be compiled in this
    // mode: statement panics, formatted panics, `return`-diverging match arms
    // and an all-arms-returning match must every one of them stay inside the
    // walker's subset, or the build is a compile error.
    let source = include_str!("../../../example/lir_divergence.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        ok,
        "example/lir_divergence.wi must compile with every free function on the LIR path: {stderr}"
    );
}
