use super::super::support::*;

#[test]
fn match_01_bool_true_false_arms() {
    let src = r#"
fn describe(b: bool) -> String {
    return match b {
        true => "yes",
        false => "no",
    };
}
fn main() {
    println(describe(true));
    println(describe(false));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "yes\nno");
}

#[test]
fn match_02_i64_with_wildcard() {
    let src = r#"
fn classify(n: i64) -> String {
    return match n {
        0 => "zero",
        1 => "one",
        _ => "other",
    };
}
fn main() {
    println(classify(0));
    println(classify(1));
    println(classify(99));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "zero\none\nother");
}

#[test]
fn match_03_fieldless_enum() {
    let src = r#"
enum Color {
    Red,
    Green,
    Blue,
}
fn name(c: Color) -> String {
    return match c {
        Color::Red => "red",
        Color::Green => "green",
        Color::Blue => "blue",
    };
}
fn main() {
    println(name(Color::Red));
    println(name(Color::Green));
    println(name(Color::Blue));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "red\ngreen\nblue");
}

#[test]
fn match_04_wildcard_arm() {
    let src = r#"
fn sign(n: i64) -> String {
    return match n {
        0 => "zero",
        _ => "nonzero",
    };
}
fn main() {
    println(sign(0));
    println(sign(5));
    println(sign(-3));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "zero\nnonzero\nnonzero");
}

#[test]
fn match_05_binding_pattern() {
    let src = r#"
fn double_or_zero(n: i64) -> i64 {
    return match n {
        0 => 0,
        v => v + v,
    };
}
fn main() {
    println(double_or_zero(0));
    println(double_or_zero(5));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "0\n10");
}

#[test]
fn match_06_as_expression_assigned_to_variable() {
    let src = r#"
enum Dir {
    Up,
    Down,
}
fn main() {
    let d = Dir::Up;
    let label = match d {
        Dir::Up => "up",
        Dir::Down => "down",
    };
    println(label);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "up");
}

#[test]
fn match_07_negative_integer_pattern() {
    let src = r#"
fn describe(n: i64) -> String {
    return match n {
        -1 => "minus one",
        0 => "zero",
        _ => "other",
    };
}
fn main() {
    println(describe(-1));
    println(describe(0));
    println(describe(3));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "minus one\nzero\nother");
}

#[test]
fn match_08_enum_passed_as_function_argument() {
    let src = r#"
enum Season {
    Spring,
    Summer,
    Autumn,
    Winter,
}
fn season_msg(s: Season) -> String {
    return match s {
        Season::Spring => "bloom",
        Season::Summer => "hot",
        Season::Autumn => "fall",
        Season::Winter => "cold",
    };
}
fn main() {
    println(season_msg(Season::Winter));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "cold");
}

#[test]
fn match_09_match_example_file_compiles_and_outputs_green() {
    let (out, ok) = compile_file_and_run("example/match_color.wi");
    assert!(ok, "match_color.wi failed to compile");
    assert_eq!(out.trim(), "green");
}

#[test]
fn match_10_bool_exhaustiveness_error_missing_false() {
    let src = r#"
fn f(b: bool) -> String {
    return match b {
        true => "yes",
    };
}
fn main() { println(f(true)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected exhaustiveness error for missing false arm"
    );
}

#[test]
fn match_11_enum_exhaustiveness_error_missing_variant() {
    let src = r#"
enum Color { Red, Green, Blue, }
fn name(c: Color) -> String {
    return match c {
        Color::Red => "red",
        Color::Green => "green",
    };
}
fn main() { println(name(Color::Red)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected exhaustiveness error for missing Blue variant"
    );
}

#[test]
fn match_12_incompatible_arm_types_error() {
    let src = r#"
fn f(b: bool) -> i64 {
    return match b {
        true => 1,
        false => "nope",
    };
}
fn main() { println(f(true)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected incompatible arm types error"
    );
}

#[test]
fn match_13_unknown_enum_variant_in_pattern_error() {
    let src = r#"
enum Color { Red, Green, }
fn f(c: Color) -> String {
    return match c {
        Color::Red => "red",
        Color::Purple => "purple",
    };
}
fn main() { println(f(Color::Red)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected unknown variant error for Color::Purple"
    );
}

#[test]
fn match_14_i64_non_exhaustive_error_missing_wildcard() {
    let src = r#"
fn f(n: i64) -> String {
    return match n {
        0 => "zero",
        1 => "one",
    };
}
fn main() { println(f(0)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected non-exhaustive error for i64 without wildcard"
    );
}

#[test]
fn match_15_enum_variant_in_let_binding() {
    let src = r#"
enum State {
    Active,
    Inactive,
}
fn main() {
    let s = State::Active;
    let r = match s {
        State::Active => "on",
        State::Inactive => "off",
    };
    println(r);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "on");
}

#[test]
fn match_16_match_in_return_with_enum_multiple_values() {
    let src = r#"
enum Priority {
    Low,
    Medium,
    High,
}
fn score(p: Priority) -> i64 {
    return match p {
        Priority::Low => 1,
        Priority::Medium => 5,
        Priority::High => 10,
    };
}
fn main() {
    println(score(Priority::Low));
    println(score(Priority::Medium));
    println(score(Priority::High));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1\n5\n10");
}

#[test]
fn match_17_bool_match_both_arms_covered() {
    let src = r#"
fn to_int(b: bool) -> i64 {
    return match b {
        true => 1,
        false => 0,
    };
}
fn main() {
    println(to_int(true));
    println(to_int(false));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1\n0");
}

#[test]
fn match_18_enum_with_single_variant() {
    let src = r#"
enum Unit { Only, }
fn describe(u: Unit) -> String {
    return match u {
        Unit::Only => "just one",
    };
}
fn main() {
    println(describe(Unit::Only));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "just one");
}

#[test]
fn match_19_wildcard_after_some_integer_patterns() {
    let src = r#"
fn greet(n: i64) -> String {
    return match n {
        1 => "one",
        2 => "two",
        _ => "many",
    };
}
fn main() {
    println(greet(1));
    println(greet(2));
    println(greet(100));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "one\ntwo\nmany");
}

#[test]
fn match_20_enum_variant_as_function_result() {
    let src = r#"
enum Toggle {
    On,
    Off,
}
fn flip(t: Toggle) -> Toggle {
    return match t {
        Toggle::On => Toggle::Off,
        Toggle::Off => Toggle::On,
    };
}
fn describe(t: Toggle) -> String {
    return match t {
        Toggle::On => "on",
        Toggle::Off => "off",
    };
}
fn main() {
    let t = Toggle::On;
    let t2 = flip(t);
    println(describe(t2));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "off");
}
