use super::*;

// ── `?` automatic error conversion via Into<E> (willow-1ow) ─────────────────

#[test]
fn try_convert_01_err_path_converts() {
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class LowErr implements Into<AppErr> {
    pub n: i64;
    pub fn into(self) -> AppErr { return new AppErr(900 + self.n); }
}
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(5)); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "905\n");
}

#[test]
fn try_convert_02_ok_path_flows_through() {
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class LowErr implements Into<AppErr> {
    pub n: i64;
    pub fn into(self) -> AppErr { return new AppErr(0); }
}
fn low() -> Result<i64, LowErr> { return Result::Ok(11); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v + 1); }
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "12\n");
}

#[test]
fn try_convert_03_exact_match_unaffected() {
    // E1 == E2: no conversion, original error propagates.
    let (out, ok) = compile_and_run(
        r#"
fn low() -> Result<i64, String> { return Result::Err("boom"); }
fn high() -> Result<i64, String> { let v = low()?; return Result::Ok(v); }
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => -1,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-1\n");
}

#[test]
fn try_convert_04_two_question_marks() {
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class LowErr implements Into<AppErr> {
    pub n: i64;
    pub fn into(self) -> AppErr { return new AppErr(self.n); }
}
fn a() -> Result<i64, LowErr> { return Result::Ok(2); }
fn b() -> Result<i64, LowErr> { return Result::Err(new LowErr(77)); }
fn high() -> Result<i64, AppErr> {
    let x = a()?;
    let y = b()?;
    return Result::Ok(x + y);
}
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "77\n");
}

#[test]
fn try_convert_05_no_into_impl_is_error() {
    assert!(expect_compile_error(
        r#"
class AppErr { pub code: i64; }
class LowErr { pub n: i64; }
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(1)); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {}
"#,
    ));
}

#[test]
fn try_convert_06_option_question_unaffected() {
    let (out, ok) = compile_and_run(
        r#"
fn first() -> Option<i64> { return Option::None; }
fn run() -> Option<i64> { let v = first()?; return Option::Some(v + 1); }
fn main() {
    let out = match run() {
        Option::Some(v) => v,
        Option::None => -9,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-9\n");
}

#[test]
fn try_convert_07_into_wrong_target_still_errors() {
    // LowErr implements Into<Other>, not Into<AppErr>: still a mismatch.
    assert!(expect_compile_error(
        r#"
class AppErr { pub code: i64; }
class Other { pub x: i64; }
class LowErr implements Into<Other> {
    pub n: i64;
    pub fn into(self) -> Other { return new Other(0); }
}
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(1)); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {}
"#,
    ));
}

#[test]
fn try_convert_08_payload_data_preserved() {
    // The converted error carries data computed from the source error.
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class LowErr implements Into<AppErr> {
    pub n: i64;
    pub fn into(self) -> AppErr { return new AppErr(self.n * 10); }
}
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(6)); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "60\n");
}

#[test]
fn try_convert_08b_gc_managed_err_payload_rooted_during_into() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class AppErr { pub msg: String; }
class LowErr implements Into<AppErr> {
    pub msg: String;
    pub fn into(self) -> AppErr {
        let prefix = "converted: ";
        gc_collect();
        return new AppErr(prefix + self.msg);
    }
}
fn low() -> Result<i64, LowErr> { return Result::Err(new LowErr("payload")); }
fn high() -> Result<i64, AppErr> { let v = low()?; return Result::Ok(v); }
fn main() {
    let out = match high() {
        Result::Ok(v) => "ok",
        Result::Err(e) => e.msg,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "converted: payload\n");
}

#[test]
fn try_convert_09_chained_three_levels() {
    // Conversion at each ? boundary up a three-level call chain.
    let (out, ok) = compile_and_run(
        r#"
class E1 implements Into<E2> {
    pub n: i64;
    pub fn into(self) -> E2 { return new E2(self.n + 1); }
}
class E2 implements Into<E3> {
    pub n: i64;
    pub fn into(self) -> E3 { return new E3(self.n + 1); }
}
class E3 { pub n: i64; }
fn a() -> Result<i64, E1> { return Result::Err(new E1(0)); }
fn b() -> Result<i64, E2> { let v = a()?; return Result::Ok(v); }
fn c() -> Result<i64, E3> { let v = b()?; return Result::Ok(v); }
fn main() {
    let out = match c() {
        Result::Ok(v) => v,
        Result::Err(e) => e.n,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    // E1{0} -> E2{1} at b's ?, then E2{1} -> E3{2} at c's ?.
    assert_eq!(out, "2\n");
}

#[test]
fn try_convert_10_two_source_types_one_target() {
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
class IoErr implements Into<AppErr> {
    pub fn into(self) -> AppErr { return new AppErr(1); }
}
class FmtErr implements Into<AppErr> {
    pub fn into(self) -> AppErr { return new AppErr(2); }
}
fn io(fail: bool) -> Result<i64, IoErr> {
    if fail { return Result::Err(new IoErr()); }
    return Result::Ok(10);
}
fn fmt() -> Result<i64, FmtErr> { return Result::Err(new FmtErr()); }
fn high() -> Result<i64, AppErr> {
    let a = io(false)?;
    let b = fmt()?;
    return Result::Ok(a + b);
}
fn main() {
    let out = match high() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}
