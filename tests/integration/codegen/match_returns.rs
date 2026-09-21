use super::*;

// ── Statement-position match + return arms (willow-zvkv) ────────────────────
// 20 perspectives: 1 return-arm sugar, 2 block arm with trailing return,
// 3 statement match at fn end satisfies the missing-return path, 4 mixed
// value + return arms in expression position (Never unifies), 5 bare
// `return` arm in a void fn, 6 optional trailing `;` after statement match,
// 7 statement match mid-function (code after it runs), 8 statement match in
// main, 9 in a class method, 10 in an async fn, 11 nested match in a return
// arm's block, 12 wildcard return arm, 13 fieldless-variant arms,
// 14 Option scrutinee, 15 user enum scrutinee (shadowing prelude name),
// 16 side effects in block arms run exactly once, 17 non-exhaustive match
// still rejected, 18 arm value/return type mismatch still rejected,
// 19 f64-returning fn ended by all-return match, 20 both arms return in
// expression-position let is rejected (Never-only match has no value).

#[test]
fn stmtmatch_01_return_arm_sugar() {
    let (out, ok) = compile_and_run(
        "fn f(r: Result<i64, String>) -> i64 { match r { Ok(v) => return v, Err(_) => return -1, } }\nfn main() { println(f(Ok(7))); println(f(Err(\"e\"))); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n-1\n");
}

#[test]
fn stmtmatch_02_block_arm_with_return() {
    let (out, ok) = compile_and_run(
        "fn f(r: Result<i64, String>) -> i64 { match r { Ok(v) => return v * 2, Err(m) => { println(m); return 0; }, } }\nfn main() { println(f(Ok(21))); println(f(Err(\"boom\"))); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\nboom\n0\n");
}

#[test]
fn stmtmatch_03_fn_ending_with_all_return_match() {
    let (out, ok) = compile_and_run(
        "fn sign(n: i64) -> i64 { match n > 0 { true => return 1, false => return -1, } }\nfn main() { println(sign(5)); println(sign(-5)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n-1\n");
}

#[test]
fn stmtmatch_04_mixed_value_and_return_arms() {
    let (out, ok) = compile_and_run(
        "fn f(r: Result<i64, String>) -> i64 { let x = match r { Ok(v) => v, Err(_) => return -1, }; return x * 10; }\nfn main() { println(f(Ok(4))); println(f(Err(\"e\"))); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "40\n-1\n");
}

#[test]
fn stmtmatch_05_bare_return_arm_void_fn() {
    let (out, ok) = compile_and_run(
        "fn f(o: Option<i64>) { match o { Some(v) => println(v), None => return, } println(99); }\nfn main() { f(Some(1)); f(None); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n99\n");
}

#[test]
fn stmtmatch_06_optional_trailing_semicolon() {
    let (out, ok) = compile_and_run(
        "fn main() { match true { true => println(1), false => println(2), }; println(3); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n3\n");
}

#[test]
fn stmtmatch_07_code_after_statement_match_runs() {
    let (out, ok) = compile_and_run(
        "fn main() { match 1 < 2 { true => println(1), false => println(2), } println(3); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n3\n");
}

#[test]
fn stmtmatch_08_statement_match_in_main() {
    let (out, ok) = compile_and_run(
        "fn main() { let o: Option<i64> = Some(5); match o { Some(v) => println(v), None => println(0), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn stmtmatch_09_in_class_method() {
    let (out, ok) = compile_and_run(
        "class C { pub fn pick(self, o: Option<i64>) -> i64 { match o { Some(v) => return v, None => return -1, } } }\nfn main() { let c = new C(); println(c.pick(Some(3))); println(c.pick(None)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n-1\n");
}

#[test]
fn stmtmatch_10_in_async_fn() {
    let (out, ok) = compile_and_run(
        "async fn f(o: Option<i64>) -> i64 { match o { Some(v) => return v, None => return -1, } }\nasync fn main() { println(await f(Some(9))); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn stmtmatch_11_nested_match_in_return_arm_block() {
    let (out, ok) = compile_and_run(
        "fn f(a: Option<i64>, b: Option<i64>) -> i64 { match a { Some(x) => { match b { Some(y) => return x + y, None => return x, } }, None => return 0, } }\nfn main() { println(f(Some(2), Some(3))); println(f(Some(2), None)); println(f(None, None)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n2\n0\n");
}

#[test]
fn stmtmatch_12_wildcard_return_arm() {
    let (out, ok) = compile_and_run(
        "fn f(n: i64) -> i64 { match n { 0 => return 100, _ => return n, } }\nfn main() { println(f(0)); println(f(7)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "100\n7\n");
}

#[test]
fn stmtmatch_13_fieldless_variant_arms() {
    let (out, ok) = compile_and_run(
        "enum Sig { Go, Stop, }\nfn f(s: Sig) -> i64 { match s { Go => return 1, Stop => return 2, } }\nfn main() { println(f(Go)); println(f(Stop)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn stmtmatch_14_option_scrutinee() {
    let (out, ok) = compile_and_run(
        "fn f(o: Option<String>) -> i64 { match o { Some(s) => { println(s); return 1; }, None => return 0, } }\nfn main() { println(f(Some(\"hi\"))); println(f(None)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hi\n1\n0\n");
}

#[test]
fn stmtmatch_15_user_enum_shadowing_prelude_name() {
    // The promoted example's exact shape: a user enum named `Result`.
    let (out, ok) = compile_and_run(
        "pub enum Result { Ok(i64), Err(String), }\nfn f(r: Result) -> i64 { match r { Ok(v) => return v, Err(m) => { println(m); return 0; }, } }\nfn main() { println(f(Ok(42))); println(f(Err(\"missing\"))); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\nmissing\n0\n");
}

#[test]
fn stmtmatch_16_side_effects_run_once() {
    let (out, ok) = compile_and_run(
        "fn main() { match true { true => { println(1); println(2); }, false => println(3), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn stmtmatch_17_non_exhaustive_still_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "enum Sig { Go, Stop, }\nfn f(s: Sig) -> i64 { match s { Go => return 1, } }\nfn main() { }",
        &[],
    );
    assert!(!ok, "non-exhaustive match must be rejected");
    assert!(!stderr.is_empty());
}

#[test]
fn stmtmatch_18_arm_type_mismatch_still_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn f(o: Option<i64>) -> i64 { let x = match o { Some(v) => v, None => \"s\", }; return x; }\nfn main() { }",
        &[],
    );
    assert!(!ok, "mismatched arm types must be rejected");
    assert!(!stderr.is_empty());
}

#[test]
fn stmtmatch_19_f64_fn_ending_with_match() {
    let (out, ok) = compile_and_run(
        "fn f(up: bool) -> f64 { match up { true => return 1.5, false => return -1.5, } }\nfn main() { println(f(true)); println(f(false)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1.5\n-1.5\n");
}

#[test]
fn stmtmatch_20_all_return_match_as_value_rejected() {
    // Every arm diverges, so the match produces no value; binding it must be
    // a type error rather than silently yielding garbage.
    let (ok, _stderr) = compile_with_compiler_env(
        "fn f(c: bool) -> i64 { let x = match c { true => return 1, false => return 2, }; return x; }\nfn main() { }",
        &[],
    );
    assert!(!ok, "binding a Never-typed match must be rejected");
}
