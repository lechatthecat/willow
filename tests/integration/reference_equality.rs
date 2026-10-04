//! `==` and `!=` on class and interface values compare reference identity;
//! types without defined equality are rejected by the checker instead of
//! reaching lowering without an operand (willow-zvnt).
use super::support::{assert_compile_error_contains, compile_and_run_with_env};

const HEADER: &str = r#"
import std::collections::Array;
interface Base extends Sync { fn get(self) -> i64; }
interface Value extends Base { fn twice(self) -> i64; }
open class Number implements Value {
    pub n: i64;
    pub fn get(self) -> i64 { return self.n; }
    pub fn twice(self) -> i64 { return self.n * 2; }
}
class Special extends Number { }
class Holder { pub value: Value; pub number: Number; }
"#;

fn check(body: &str, expected: &str) {
    let (out, ok) =
        compile_and_run_with_env(&format!("{HEADER}\n{body}"), &[("WILLOW_GC_STRESS", "all")]);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

macro_rules! perspective {
    ($name:ident, $source:literal, $expected:literal) => {
        #[test]
        fn $name() {
            check($source, $expected);
        }
    };
}

macro_rules! rejected {
    ($name:ident, $source:literal, $needles:expr) => {
        #[test]
        fn $name() {
            assert_compile_error_contains($source, $needles);
        }
    };
}

perspective!(
    ref_eq_01_class_same_reference,
    r#"fn main() { let a = new Number(1); let b = a; println(a == b); println(a != b); }"#,
    "true\nfalse\n"
);

perspective!(
    ref_eq_02_class_equal_fields_distinct_objects,
    r#"fn main() { let a = new Number(1); let b = new Number(1); println(a == b); println(a != b); }"#,
    "false\ntrue\n"
);

perspective!(
    ref_eq_03_interface_pair_same_object,
    r#"fn main() { let a: Value = new Number(2); let b: Value = a; println(a == b); println(a != b); }"#,
    "true\nfalse\n"
);

perspective!(
    ref_eq_04_interface_pair_distinct_objects,
    r#"fn main() { let a: Value = new Number(2); let b: Value = new Number(2); println(a == b); println(a != b); }"#,
    "false\ntrue\n"
);

perspective!(
    ref_eq_05_interface_against_class_both_orders,
    r#"fn main() { let n = new Number(3); let v: Value = n; let o = new Number(3); println(v == n); println(n == v); println(v == o); println(o != v); }"#,
    "true\ntrue\nfalse\ntrue\n"
);

perspective!(
    ref_eq_06_subclass_against_base_both_orders,
    r#"fn main() { let s = new Special(4); let n: Number = s; println(s == n); println(n == s); println(n != new Special(4)); }"#,
    "true\ntrue\ntrue\n"
);

perspective!(
    ref_eq_07_sub_interface_against_super_interface,
    r#"fn main() { let n = new Number(5); let v: Value = n; let b: Base = n; println(b == v); println(v == b); let other: Base = new Number(5); println(v == other); }"#,
    "true\ntrue\nfalse\n"
);

perspective!(
    ref_eq_08_interface_parameters,
    r#"fn same(a: Base, b: Base) -> bool { return a == b; } fn main() { let n = new Number(6); println(same(n, n)); println(same(n, new Special(6))); }"#,
    "true\nfalse\n"
);

perspective!(
    ref_eq_09_returned_interface_values,
    r#"fn wrap(n: Number) -> Value { return n; } fn main() { let n = new Number(7); println(wrap(n) == wrap(n)); println(wrap(n) == n); println(wrap(n) == wrap(new Number(7))); }"#,
    "true\ntrue\nfalse\n"
);

perspective!(
    ref_eq_10_fields_of_class_and_interface_type,
    r#"fn main() { let n = new Number(8); let h = new Holder(n, n); gc_collect(); println(h.value == h.number); println(h.number == n); h.number = new Number(8); println(h.value == h.number); }"#,
    "true\ntrue\nfalse\n"
);

perspective!(
    ref_eq_11_array_elements,
    r#"fn main() { let n = new Number(9); let values: Array<Value> = [n, new Number(9)]; gc_collect(); println(values[0] == n); println(values[1] == n); println(values[0] != values[1]); }"#,
    "true\nfalse\ntrue\n"
);

perspective!(
    ref_eq_12_if_condition_and_ternary,
    r#"fn main() { let n = new Number(10); let v: Value = n; if v == n { println("same"); } let k = n != v ? 1 : 2; println(k); }"#,
    "same\n2\n"
);

perspective!(
    ref_eq_13_short_circuit_and_negation,
    r#"fn main() { let a = new Number(11); let b = new Number(11); println(a == b || a == a); println(a == b && a == a); println(!(a == b)); }"#,
    "true\nfalse\ntrue\n"
);

perspective!(
    ref_eq_14_match_arm_values,
    r#"enum Pick { Same, Diff } fn test(p: Pick, a: Value, b: Number) -> bool { return match p { Pick::Same => a == b, Pick::Diff => a != b }; } fn main() { let n = new Number(12); println(test(Pick::Same, n, n)); println(test(Pick::Diff, n, new Number(12))); }"#,
    "true\ntrue\n"
);

perspective!(
    ref_eq_15_closure_capture,
    r#"fn main() { let n = new Number(13); let v: Value = n; let is_n = |x: Value| -> bool { return x == v; }; println(is_n(n)); println(is_n(new Number(13))); }"#,
    "true\nfalse\n"
);

perspective!(
    ref_eq_16_async_frame_across_suspension,
    r#"async fn job(a: Value, b: Number) -> bool { await sleep(1); let first = a == b; await sleep(1); return first && a == b; } async fn main() { let n = new Number(14); println(await job(n, n)); println(await job(n, new Number(14))); }"#,
    "true\nfalse\n"
);

perspective!(
    ref_eq_17_identity_survives_gc_moves,
    r#"fn main() { let a: Value = new Number(15); let b = a; let c: Value = new Number(15); gc_collect(); gc_minor_collect(); println(a == b); println(a == c); }"#,
    "true\nfalse\n"
);

perspective!(
    ref_eq_18_scalar_and_enum_equality_unchanged,
    r#"enum Color { Red, Green } fn main() { println(Color::Red == Color::Green); println(Color::Red != Color::Green); println(1 == 1); println("a" != "b"); println(1.5 == 1.5); println(true == false); }"#,
    "false\ntrue\ntrue\ntrue\ntrue\nfalse\n"
);

rejected!(
    ref_eq_19_unrelated_classes_are_mismatched,
    r#"class A {} class B {} fn main() { println(new A() == new B()); }"#,
    &["error[E0201]", "mismatched types: `A` and `B`"]
);

rejected!(
    ref_eq_20_arrays_have_no_equality,
    r#"fn main() { let a = [1, 2]; let b = a; println(a == b); }"#,
    &[
        "error[E0201]",
        "`Array<i64>` does not support `==`",
        "reference identity",
    ]
);

rejected!(
    ref_eq_21_payload_enums_have_no_equality,
    r#"enum Shape { Dot(i64), Empty } fn main() { let a = Shape::Dot(1); println(a != Shape::Empty); }"#,
    &["error[E0201]", "`Shape` does not support `!=`"]
);

rejected!(
    ref_eq_22_result_has_no_equality,
    r#"fn main() { let a: Result<i64, String> = Result::Ok(1); println(a == a); }"#,
    &[
        "error[E0201]",
        "`Result<i64, String>` does not support `==`"
    ]
);

rejected!(
    ref_eq_23_function_values_have_no_equality,
    r#"fn f() -> i64 { return 1; } fn main() { let a = f; println(a == a); }"#,
    &["error[E0201]", "`fn() -> i64` does not support `==`"]
);

rejected!(
    ref_eq_24_void_has_no_equality,
    r#"fn f() {} fn main() { println(f() == f()); }"#,
    &["error[E0201]", "`void` does not support `==`"]
);

rejected!(
    ref_eq_25_option_keeps_its_specific_diagnostic,
    r#"fn main() { let a: Option<i64> = Some(1); println(a == a); }"#,
    &["error[E0201]", "`Option<T>` does not support `==`"]
);
