use super::*;

// ── Option/Result runtime behaviour (willow-0g8j.2.1) ───────────────────────
//
// The eligibility boundary is pinned by the `p*` unit tests in
// `src/backend/cranelift/lir_gen.rs`. These pin the OUTPUT: for each shape the
// walker claims, the program it produces must print exactly the right thing.
// The representation split is what makes that non-trivial —
// `Option<String>` is a pointer niche and `Option<i64>` is a boxed
// `[tag | payload]`, and only `option_repr` knows which.

#[test]
fn lir_diff_46_boxed_option_roundtrip() {
    assert_program_output(
        r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 { return None; }
    return Some(a / b);
}
fn show(o: Option<i64>) -> i64 {
    return match o { Some(v) => v, None => -1 };
}
fn main() {
    println(show(safe_div(10, 2)));
    println(show(safe_div(10, 0)));
    println(safe_div(9, 3).unwrap());
    println(safe_div(9, 0).unwrap_or(-7));
    println(safe_div(9, 3).is_some());
    println(safe_div(9, 0).is_none());
}
"#,
        "5\n-1\n3\n-7\ntrue\ntrue\n",
    );
}

#[test]
fn lir_diff_46_niche_option_roundtrip() {
    // `Some(x)` IS `x` here and `None` is a null pointer, so the tag test the
    // walker emits is pointer arithmetic rather than a load. Getting the two
    // representations crossed would read a `WillowString` header as a tag.
    assert_program_output(
        r#"
fn lookup(id: i64) -> Option<String> {
    if id == 1 { return Some("one"); }
    return None;
}
fn show(o: Option<String>) -> String {
    return match o { Some(v) => v, None => "-" };
}
fn main() {
    println(show(lookup(1)));
    println(show(lookup(2)));
    println(lookup(1).unwrap());
    println(lookup(2).unwrap_or("fallback"));
    println(lookup(1).is_some());
    println(lookup(2).is_some());
}
"#,
        "one\n-\none\nfallback\ntrue\nfalse\n",
    );
}

#[test]
fn lir_diff_47_nested_option_is_never_the_niche() {
    // An `Option<Option<T>>` cannot use the niche at any level: the inner
    // `None` and the outer one would be the same value. Both `None`s have to
    // stay distinguishable through a full round trip.
    assert_program_output(
        r#"
fn wrap(n: i64) -> Option<Option<i64>> {
    if n < 0 { return None; }
    if n == 0 { return Some(None); }
    return Some(Some(n));
}
fn show(o: Option<Option<i64>>) -> i64 {
    return match o {
        Some(inner) => match inner { Some(v) => v, None => -1 },
        None => -2
    };
}
fn main() {
    println(show(wrap(5)));
    println(show(wrap(0)));
    println(show(wrap(-3)));
}
"#,
        "5\n-1\n-2\n",
    );
}

#[test]
fn lir_diff_48_result_ok_and_err() {
    // `unwrap_err` reads the SECOND type argument. A substitution that took the
    // first would hand a `String` slot an `i64` and print garbage rather than
    // fail loudly, so the Err payload is printed, not just tested.
    assert_program_output(
        r#"
fn parse(n: i64) -> Result<i64, String> {
    if n < 0 { return Err("negative"); }
    return Ok(n * 2);
}
fn main() {
    println(parse(4).unwrap());
    println(parse(-1).unwrap_err());
    println(parse(4).is_ok());
    println(parse(-1).is_err());
    println(parse(-1).unwrap_or(0));
    println(match parse(3) { Ok(v) => v, Err(e) => -1 });
}
"#,
        "8\nnegative\ntrue\ntrue\n0\n6\n",
    );
}

#[test]
fn lir_diff_49_try_propagate_chain() {
    // `?` is the only expression in the subset that leaves the function from
    // the middle of another expression. Both exits are exercised, and the
    // success path is taken more than once so the early return cannot have
    // been a one-shot.
    assert_program_output(
        r#"
fn digit(c: i64) -> Result<i64, String> {
    if c < 0 { return Err("bad digit"); }
    return Ok(c);
}
fn sum3(a: i64, b: i64, c: i64) -> Result<i64, String> {
    let x = digit(a)?;
    let y = digit(b)?;
    let z = digit(c)?;
    return Ok(x + y + z);
}
fn main() {
    println(match sum3(1, 2, 3) { Ok(v) => v, Err(e) => -1 });
    println(match sum3(1, -2, 3) { Ok(v) => v, Err(e) => -1 });
    println(match sum3(1, 2, -3) { Ok(v) => v, Err(e) => -1 });
}
"#,
        "6\n-1\n-1\n",
    );
}

#[test]
fn lir_diff_50_try_propagate_inside_a_loop() {
    // The early return leaves the loop and the function at once, so the loop's
    // own exit block must not be what the failure path branches to.
    assert_program_output(
        r#"
fn step(n: i64) -> Result<i64, String> {
    if n == 3 { return Err("stopped at 3"); }
    return Ok(n);
}
fn total(limit: i64) -> Result<i64, String> {
    let mut sum = 0;
    let mut i = 0;
    while i < limit {
        sum = sum + step(i)?;
        i = i + 1;
    }
    return Ok(sum);
}
fn main() {
    println(match total(3) { Ok(v) => v, Err(e) => -1 });
    println(match total(6) { Ok(v) => v, Err(e) => -1 });
    println(match total(6) { Ok(v) => "?", Err(e) => e });
}
"#,
        "3\n-1\nstopped at 3\n",
    );
}

#[test]
fn lir_diff_51_try_propagate_converts_the_error() {
    // willow-1ow: when the operand's `E1` differs from the function's `E2` the
    // failure path calls `into()` and re-wraps. Forwarding the operand pointer
    // unchanged here would hand back a `PortError` where a `ConfigError` is
    // expected, and the field read would be off by whatever the layouts differ.
    assert_program_output(
        r#"
class ConfigError {
    pub code: i64;
    pub label: String;
}
class PortError implements Into<ConfigError> {
    pub raw: i64;
    pub fn into(self) -> ConfigError {
        return new ConfigError(400 + self.raw, "port");
    }
}
fn read_port(n: i64) -> Result<i64, PortError> {
    if n > 65535 { return Err(new PortError(3)); }
    return Ok(n);
}
fn load(n: i64) -> Result<i64, ConfigError> {
    let port = read_port(n)?;
    return Ok(port + 1);
}
fn main() {
    println(match load(80) { Ok(v) => v, Err(e) => -1 });
    println(match load(70000) { Ok(v) => v, Err(e) => e.code });
    println(match load(70000) { Ok(v) => "?", Err(e) => e.label });
}
"#,
        "81\n403\nport\n",
    );
}

#[test]
fn lir_diff_52_try_propagate_across_option_representations() {
    // The two sides of a `?` pick their niche independently, so the failure
    // value is CONSTRUCTED for the destination rather than forwarded. Both
    // directions are here because neither is a special case of the other.
    assert_program_output(
        r#"
fn name_of(id: i64) -> Option<String> {
    if id == 1 { return Some("alpha"); }
    return None;
}
fn code_of(id: i64) -> Option<i64> {
    if id == 1 { return Some(11); }
    return None;
}
fn niche_to_boxed(id: i64) -> Option<i64> {
    let n = name_of(id)?;
    return Some(id * 10);
}
fn boxed_to_niche(id: i64) -> Option<String> {
    let c = code_of(id)?;
    return Some("code");
}
fn main() {
    println(match niche_to_boxed(1) { Some(v) => v, None => -1 });
    println(match niche_to_boxed(2) { Some(v) => v, None => -1 });
    println(match boxed_to_niche(1) { Some(v) => v, None => "none" });
    println(match boxed_to_niche(2) { Some(v) => v, None => "none" });
}
"#,
        "10\n-1\ncode\nnone\n",
    );
}

#[test]
fn lir_diff_53_map_get_yields_the_maps_own_option() {
    // `get` is the one builtin that hands back an `Option`, and the runtime
    // builds it from the map's OWN value type — so the walker must read back
    // the representation the runtime chose, not the one it would have picked.
    assert_program_output(
        r#"
import std::collections::Map;
fn main() {
    let scores: Map<String, i64> = Map::new();
    scores.insert("a", 1);
    scores.insert("b", 2);
    println(scores.get("a").unwrap_or(-1));
    println(scores.get("z").unwrap_or(-1));
    let names: Map<String, String> = Map::new();
    names.insert("a", "one");
    println(match names.get("a") { Some(v) => v, None => "-" });
    println(match names.get("z") { Some(v) => v, None => "-" });
}
"#,
        "1\n-1\none\n-\n",
    );
}

#[test]
fn lir_diff_54_option_in_fields_and_elements() {
    // An `Option` in storage: a class field and an array element. The store
    // has to agree with the load about the representation, and the element's
    // is-ref flag has to say the slot is a GC reference for the boxed form.
    assert_program_output(
        r#"
import std::collections::Array;
class Reading {
    pub value: Option<i64>;
    pub label: Option<String>;
}
fn value_of(r: Reading) -> i64 { return r.value.unwrap_or(-1); }
fn label_of(r: Reading) -> String { return r.label.unwrap_or("-"); }
fn present(xs: Array<Option<i64>>) -> i64 {
    let mut n = 0;
    let mut i = 0;
    while i < xs.len() {
        if xs[i].is_some() { n = n + 1; }
        i = i + 1;
    }
    return n;
}
fn main() {
    println(value_of(new Reading(Some(7), Some("hot"))));
    println(value_of(new Reading(None, None)));
    println(label_of(new Reading(Some(7), Some("hot"))));
    println(label_of(new Reading(None, None)));
    let xs: Array<Option<i64>> = [Some(1), None, Some(3)];
    println(present(xs));
}
"#,
        "7\n-1\nhot\n-\n2\n",
    );
}

#[test]
fn lir_diff_55_user_generic_enum_roundtrip() {
    // `Option` and `Result` are claimed as ORDINARY generic enums, so a
    // user-declared one with the same shape has to behave identically.
    assert_program_output(
        r#"
enum Either<L, R> { Left(L), Right(R) }
fn split(n: i64) -> Either<i64, String> {
    if n % 2 == 0 { return Either::Left(n); }
    return Either::Right("odd");
}
fn show(e: Either<i64, String>) -> String {
    return match e {
        Either::Left(v) => "even",
        Either::Right(s) => s
    };
}
fn main() {
    println(show(split(4)));
    println(show(split(5)));
}
"#,
        "even\nodd\n",
    );
}

#[test]
fn lir_diff_56_boxed_option_survives_gc_stress() {
    // Every `Some(v)` here allocates, and the loop keeps allocating around the
    // live ones. A boxed `Option` held in a local is a GC reference, so it has
    // to be rooted for the collection the next allocation triggers.
    assert_output_under_gc_stress(
        r#"
fn wrap(n: i64) -> Option<i64> {
    if n % 3 == 0 { return None; }
    return Some(n);
}
fn main() {
    let mut total = 0;
    let mut i = 0;
    while i < 30 {
        let a = wrap(i);
        let b = wrap(i + 1);
        total = total + a.unwrap_or(0) + b.unwrap_or(0);
        i = i + 1;
    }
    println(total);
}
"#,
        "600\n",
    );
}

#[test]
fn lir_diff_57_try_propagate_error_conversion_under_gc_stress() {
    // The failure path allocates twice — `into()` builds the new error and the
    // re-wrap boxes it — with the operand's payload live across both. Missing
    // that root would free the payload while `into` is still reading it.
    assert_output_under_gc_stress(
        r#"
class ConfigError { pub label: String; }
class PortError implements Into<ConfigError> {
    pub raw: String;
    pub fn into(self) -> ConfigError {
        return new ConfigError("cfg:" + self.raw);
    }
}
fn read_port(n: i64) -> Result<i64, PortError> {
    if n % 4 == 0 { return Err(new PortError("bad" + "port")); }
    return Ok(n);
}
fn load(n: i64) -> Result<i64, ConfigError> {
    let port = read_port(n)?;
    return Ok(port);
}
fn main() {
    let mut i = 0;
    while i < 20 {
        println(match load(i) { Ok(v) => "ok", Err(e) => e.label });
        i = i + 1;
    }
}
"#,
        &{
            let mut out = String::new();
            for i in 0..20 {
                out.push_str(if i % 4 == 0 { "cfg:badport\n" } else { "ok\n" });
            }
            out
        },
    );
}
