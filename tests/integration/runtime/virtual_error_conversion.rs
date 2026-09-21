use super::*;

// ── ? error conversion: virtual Into dispatch on subclassed errors (bpk6) ────

#[test]
fn try_convert_11_subclassed_error_uses_override() {
    // A Result<_, BaseErr> holding a SpecificErr (override of into) must convert
    // via the override when propagated with `?` (willow-bpk6).
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
open class BaseErr implements Into<AppErr> {
    pub open fn into(self) -> AppErr { return new AppErr(1); }
}
class SpecificErr extends BaseErr {
    pub override fn into(self) -> AppErr { return new AppErr(99); }
}
fn fails() -> Result<i64, BaseErr> {
    let e: BaseErr = new SpecificErr();
    return Result::Err(e);
}
fn run() -> Result<i64, AppErr> { let v = fails()?; return Result::Ok(v); }
fn main() {
    let out = match run() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n");
}

#[test]
fn try_convert_12_base_error_uses_base_into() {
    // The same hierarchy: a plain BaseErr converts via BaseErr::into.
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
open class BaseErr implements Into<AppErr> {
    pub open fn into(self) -> AppErr { return new AppErr(1); }
}
class SpecificErr extends BaseErr {
    pub override fn into(self) -> AppErr { return new AppErr(99); }
}
fn fails() -> Result<i64, BaseErr> { return Result::Err(new BaseErr()); }
fn run() -> Result<i64, AppErr> { let v = fails()?; return Result::Ok(v); }
fn main() {
    let out = match run() {
        Result::Ok(v) => v,
        Result::Err(e) => e.code,
    };
    println(out);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn try_convert_13_scalar_into_override_returns_by_value() {
    // The virtual `into` call takes its signature from the target's declared
    // return type: an `Into<f64>`/`Into<bool>` override returns a scalar, not a
    // pointer, and must not ICE the Cranelift verifier (review of willow-ssl7).
    let (out, ok) = compile_and_run(
        r#"
open class BaseErr implements Into<f64> {
    pub open fn into(self) -> f64 { return 1.5; }
}
class SubErr extends BaseErr {
    pub override fn into(self) -> f64 { return 2.5; }
}
open class BoolErr implements Into<bool> {
    pub open fn into(self) -> bool { return false; }
}
class TrueErr extends BoolErr {
    pub override fn into(self) -> bool { return true; }
}
class ByteErr implements Into<i64> {
    pub fn into(self) -> i64 { return 7; }
}
fn fails() -> Result<i64, BaseErr> { let e: BaseErr = new SubErr(); return Result::Err(e); }
fn base_fails() -> Result<i64, BaseErr> { return Result::Err(new BaseErr()); }
fn bool_fails() -> Result<i64, BoolErr> { let e: BoolErr = new TrueErr(); return Result::Err(e); }
fn byte_fails() -> Result<i64, ByteErr> { return Result::Err(new ByteErr()); }
fn run() -> Result<i64, f64> { let v = fails()?; return Result::Ok(v); }
fn run_base() -> Result<i64, f64> { let v = base_fails()?; return Result::Ok(v); }
fn run_bool() -> Result<i64, bool> { let v = bool_fails()?; return Result::Ok(v); }
fn run_byte() -> Result<i64, i64> { let v = byte_fails()?; return Result::Ok(v); }
fn main() {
    match run() { Result::Ok(v) => println(v), Result::Err(e) => println(e), }
    match run_base() { Result::Ok(v) => println(v), Result::Err(e) => println(e), }
    match run_bool() { Result::Ok(v) => println(v), Result::Err(e) => println(e), }
    match run_byte() { Result::Ok(v) => println(v), Result::Err(e) => println(e), }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2.5\n1.5\ntrue\n7\n");
}

#[test]
fn subclass_iface_04_inherits_generic_interface_with_args() {
    // A subclass inherits a generic interface (Into<AppErr>) from its base with
    // type args preserved (regression for the name-only propagation bug).
    let (out, ok) = compile_and_run(
        r#"
class AppErr { pub code: i64; }
open class BaseErr implements Into<AppErr> {
    pub open fn into(self) -> AppErr { return new AppErr(7); }
}
class SubErr extends BaseErr {}
fn convert(e: Into<AppErr>) -> i64 { return e.into().code; }
fn main() { println(convert(new SubErr())); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}
