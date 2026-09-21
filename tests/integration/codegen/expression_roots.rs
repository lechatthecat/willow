use super::*;

// ── willow-oewp.6: GC-safety of remaining expression forms + temporaries ──────
// Each test runs under WILLOW_GC_STRESS=alloc (collect on every allocation), so
// any live GC value that is not reachable from the root graph during an
// allocation is freed and the program corrupts/segfaults. The 24 perspectives
// below cover spec sections 8-9/12: parameters, self, call arguments, object
// literals, chained concatenation, literal cache, map key/value/get, array
// literal/push, static/interface/dynamic dispatch, Option payloads, and
// receivers produced by temporaries, nested calls, and field-access chains.

#[test]
fn oewp6_01_string_param_survives_alloc() {
    // Perspective 1: String fn parameter stays rooted while the callee allocates.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn use_after(s: String) -> String { let x = "z" + "z"; return s + x; }
fn main() { println(use_after("a")); }
"#,
    );
    assert!(ok, "oewp6_01: String param must survive callee allocation");
    assert_eq!(out, "azz\n");
}
#[test]
fn oewp6_02_class_param_survives_alloc() {
    // Perspective 2: class-object fn parameter stays rooted across an allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Box { pub v: String; }
fn use_after(b: Box) -> String { let x = "z" + "z"; return b.v + x; }
fn main() { let b = new Box("a"); println(use_after(b)); }
"#,
    );
    assert!(ok, "oewp6_02: class param must survive callee allocation");
    assert_eq!(out, "azz\n");
}
#[test]
fn oewp6_03_method_self_survives_alloc() {
    // Perspective 3: method receiver self stays rooted while the body allocates.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class C { pub v: String; pub fn go(self) -> String { let x = "y" + "y"; return self.v + x; } }
fn main() { let c = new C("a"); println(c.go()); }
"#,
    );
    assert!(ok, "oewp6_03: self must survive method-body allocation");
    assert_eq!(out, "ayy\n");
}
#[test]
fn oewp6_04_method_string_param_survives_alloc() {
    // Perspective 4: method String parameter stays rooted across an allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class C { pub fn go(self, s: String) -> String { let x = "z" + "z"; return s + x; } }
fn main() { let c = new C(); println(c.go("a")); }
"#,
    );
    assert!(ok, "oewp6_04: method String param must survive allocation");
    assert_eq!(out, "azz\n");
}
#[test]
fn oewp6_05_method_class_param_survives_alloc() {
    // Perspective 5: method class parameter stays rooted across an allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Box { pub v: String; }
class C { pub fn go(self, b: Box) -> String { let x = "z" + "z"; return b.v + x; } }
fn main() { let c = new C(); let b = new Box("a"); println(c.go(b)); }
"#,
    );
    assert!(ok, "oewp6_05: method class param must survive allocation");
    assert_eq!(out, "azz\n");
}
#[test]
fn oewp6_06_fn_arg_temporaries_survive() {
    // Perspective 6: function call GC-arg temporaries survive a later allocating argument.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn make(s: String) -> String { return s + "!"; }
fn combine(a: String, b: String) -> String { return a + b; }
fn main() { println(combine(make("a"), make("b"))); }
"#,
    );
    assert!(
        ok,
        "oewp6_06: first fn arg must survive second arg allocation"
    );
    assert_eq!(out, "a!b!\n");
}
#[test]
fn oewp6_07_method_arg_temporaries_survive() {
    // Perspective 7: method call GC-arg temporaries survive a later allocating argument.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Comb {
    pub fn make(self, s: String) -> String { return s + "!"; }
    pub fn combine(self, a: String, b: String) -> String { return a + b; }
}
fn main() { let c = new Comb(); println(c.combine(c.make("a"), c.make("b"))); }
"#,
    );
    assert!(
        ok,
        "oewp6_07: first method arg must survive second arg allocation"
    );
    assert_eq!(out, "a!b!\n");
}
#[test]
fn oewp6_08_object_literal_fields_survive() {
    // Perspective 8: object construction with GC fields survives initializer allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Pair { pub a: String; pub b: String; }
fn make(s: String) -> String { return s + "!"; }
fn main() { let p = new Pair(make("a"), make("b")); println(p.a + p.b); }
"#,
    );
    assert!(
        ok,
        "oewp6_08: first field value must survive second field allocation"
    );
    assert_eq!(out, "a!b!\n");
}
#[test]
fn oewp6_09_chained_concat_survives() {
    // Perspective 9: chained String concatenation survives repeated allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn make(s: String) -> String { return s + "!"; }
fn main() { println(make("a") + make("b") + make("c") + make("d")); }
"#,
    );
    assert!(
        ok,
        "oewp6_09: chained concat operands must survive later allocations"
    );
    assert_eq!(out, "a!b!c!d!\n");
}
#[test]
fn oewp6_10_literal_cache_after_gc() {
    // Perspective 10: string literal cache stays valid after an explicit GC.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() { let a = "hello"; gc_collect(); let b = "hello"; println(a + b); }
"#,
    );
    assert!(
        ok,
        "oewp6_10: literal cache must not return freed pointers after gc"
    );
    assert_eq!(out, "hellohello\n");
}
#[test]
fn oewp6_11_temp_receiver_single_dispatch() {
    // Perspective 11: temporary method receiver (single dispatch) survives arg allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Holder { pub label: String; pub fn combined(self, o: String) -> String { return self.label + o; } }
fn make_holder() -> Holder { return new Holder("H"); }
fn make(s: String) -> String { return s + "!"; }
fn main() { println(make_holder().combined(make("x"))); }
"#,
    );
    assert!(
        ok,
        "oewp6_11: temporary receiver must survive arg allocation"
    );
    assert_eq!(out, "Hx!\n");
}
#[test]
fn oewp6_12_temp_receiver_dynamic_dispatch() {
    // Perspective 12: temporary method receiver (dynamic/overridden dispatch) survives arg allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
open class Animal { pub label: String; pub open fn combined(self, o: String) -> String { return self.label + o; } }
class Dog extends Animal { pub override fn combined(self, o: String) -> String { return self.label + "/" + o; } }
fn make_dog() -> Animal { return new Dog("D"); }
fn make(s: String) -> String { return s + "!"; }
fn main() { println(make_dog().combined(make("x"))); }
"#,
    );
    assert!(
        ok,
        "oewp6_12: temporary receiver must survive arg allocation under dynamic dispatch"
    );
    assert_eq!(out, "D/x!\n");
}
#[test]
fn oewp6_13_temp_interface_receiver() {
    // Perspective 13: interface-typed temporary receiver survives arg allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
interface Greeter { fn combined(self, o: String) -> String; }
class Hello implements Greeter { pub label: String; pub fn combined(self, o: String) -> String { return self.label + o; } }
fn make_greeter() -> Greeter { return new Hello("H"); }
fn make(s: String) -> String { return s + "!"; }
fn main() { println(make_greeter().combined(make("x"))); }
"#,
    );
    assert!(
        ok,
        "oewp6_13: temporary interface receiver must survive arg allocation"
    );
    assert_eq!(out, "Hx!\n");
}
#[test]
fn oewp6_14_map_insert_key_survives_value() {
    // Perspective 14: map insert GC key survives the value argument allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Map;
fn make(s: String) -> String { return s + "!"; }
fn main() { let mut m: Map<String, String> = Map::new(); m.insert(make("k"), make("v")); println(m.get(make("k")).unwrap()); }
"#,
    );
    assert!(ok, "oewp6_14: map key must survive value-arg allocation");
    assert_eq!(out, "v!\n");
}
#[test]
fn oewp6_15_map_insert_kv_survive_call() {
    // Perspective 15: map insert GC key+value survive the insert call's own allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Map;
fn make(s: String) -> String { return s + "!"; }
fn main() {
    let mut m: Map<String, String> = Map::new();
    m.insert(make("k1"), make("v1"));
    m.insert(make("k2"), make("v2"));
    println(m.get(make("k1")).unwrap() + m.get(make("k2")).unwrap());
}
"#,
    );
    assert!(
        ok,
        "oewp6_15: map key/value must survive the insert call allocation"
    );
    assert_eq!(out, "v1!v2!\n");
}
#[test]
fn oewp6_16_map_get_temp_map() {
    // Perspective 16: map get on a temporary map survives the Option result allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Map;
fn make(s: String) -> String { return s + "!"; }
fn build() -> Map<String, String> { let mut m: Map<String, String> = Map::new(); m.insert("k", make("v")); return m; }
fn main() { println(build().get("k").unwrap()); }
"#,
    );
    assert!(
        ok,
        "oewp6_16: temporary map must survive get's Option allocation"
    );
    assert_eq!(out, "v!\n");
}
#[test]
fn oewp6_17_array_literal_allocating_elems() {
    // Perspective 17: array literal of allocating element expressions stays consistent.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;
fn make(s: String) -> String { return s + "!"; }
fn main() { let xs: Array<String> = [make("a"), make("b"), make("c")]; println(xs[0] + xs[1] + xs[2]); }
"#,
    );
    assert!(
        ok,
        "oewp6_17: array literal elements must survive later element allocations"
    );
    assert_eq!(out, "a!b!c!\n");
}
#[test]
fn oewp6_18_array_push_allocating_value() {
    // Perspective 18: array push of an allocating value keeps earlier elements alive.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;
fn make(s: String) -> String { return s + "!"; }
fn main() { let mut xs: Array<String> = []; xs.push(make("a")); xs.push(make("b")); println(xs[0] + xs[1]); }
"#,
    );
    assert!(
        ok,
        "oewp6_18: array push value/elements must survive allocation"
    );
    assert_eq!(out, "a!b!\n");
}
#[test]
fn oewp6_19_static_call_arg_temporaries() {
    // Perspective 19: static method call GC-arg temporaries survive a later allocating argument.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class S { pub static fn combine(a: String, b: String) -> String { return a + b; } }
fn make(s: String) -> String { return s + "!"; }
fn main() { println(S::combine(make("a"), make("b"))); }
"#,
    );
    assert!(
        ok,
        "oewp6_19: static-call first arg must survive second arg allocation"
    );
    assert_eq!(out, "a!b!\n");
}
#[test]
fn oewp6_20_option_payload_allocating() {
    // Perspective 20: Option::Some payload from an allocating expression survives wrapping.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn make(s: String) -> String { return s + "!"; }
fn main() { let o: Option<String> = Option::Some(make("x")); println(o.unwrap()); }
"#,
    );
    assert!(
        ok,
        "oewp6_20: Option payload must survive the enum allocation"
    );
    assert_eq!(out, "x!\n");
}
#[test]
fn oewp6_21_nested_call_receiver() {
    // Perspective 21: a receiver produced by a nested call survives arg allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Holder { pub label: String; pub fn combined(self, o: String) -> String { return self.label + o; } }
fn make_holder() -> Holder { return new Holder("H"); }
fn id(h: Holder) -> Holder { return h; }
fn make(s: String) -> String { return s + "!"; }
fn main() { println(id(make_holder()).combined(make("x"))); }
"#,
    );
    assert!(
        ok,
        "oewp6_21: nested-call receiver must survive arg allocation"
    );
    assert_eq!(out, "Hx!\n");
}
#[test]
fn oewp6_22_field_access_chain_receiver() {
    // Perspective 22: a receiver reached through a field-access chain survives arg allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Inner { pub label: String; pub fn combined(self, o: String) -> String { return self.label + o; } }
class Outer { pub inner: Inner; }
fn make_outer() -> Outer { return new Outer(new Inner("H")); }
fn make(s: String) -> String { return s + "!"; }
fn main() { println(make_outer().inner.combined(make("x"))); }
"#,
    );
    assert!(
        ok,
        "oewp6_22: field-access-chain receiver must survive arg allocation"
    );
    assert_eq!(out, "Hx!\n");
}
#[test]
fn oewp6_23_ternary_gc_operand() {
    // Perspective 23: a ternary-produced GC value used as a concat operand survives later allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn make(s: String) -> String { return s + "!"; }
fn pick() -> String { return "T"; }
fn main() { let c = true; println((c ? pick() : pick()) + make("x")); }
"#,
    );
    assert!(
        ok,
        "oewp6_23: ternary result must survive the concat rhs allocation"
    );
    assert_eq!(out, "Tx!\n");
}
#[test]
fn oewp6_24_match_gc_arm() {
    // Perspective 24: a match-arm-produced GC value survives a later allocation.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn make(s: String) -> String { return s + "!"; }
fn main() { let n = 1; let r = match n { 1 => make("H"), _ => make("Z") }; let pad = "y" + "y"; println(r + pad); }
"#,
    );
    assert!(ok, "oewp6_24: match result must survive a later allocation");
    assert_eq!(out, "H!yy\n");
}
