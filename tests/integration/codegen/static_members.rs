use super::super::support::*;

// ---------------------------------------------------------------------------
// Static members + implicit self — willow-qsqf Stage 1 (static fn + implicit
// self). `static fn` is class-level (called `Type::m(...)`, no `self`); a plain
// `fn` is an instance method whose `self` is implicit (no `self` parameter).
//
//  1. static fn returns a value, called via Type::method
//  2. static fn with multiple args
//  3. static fn calls another static fn on the same class
//  4. static fn called via `Self::` inside an instance method
//  5. static factory returns a class instance
//  6. implicit self reads an instance field
//  7. implicit self method takes extra params
//  8. implicit self mutates an instance field
//  9. implicit self calls another instance method
// 10. static fn returns bool
// 11. static fn returns f64
// 12. static fn returns String (GC-managed result)
// 13. implicit-self String field roundtrips (no explicit self param)
// 14. legacy explicit `self` still compiles (migration compatibility)
// 15. static and instance methods coexist in one class
// 16. `self` in a static method is rejected (E0831)
// 17. explicit `self` on a `static fn` is a parse error (E0831)
// 18. static method called with `.` is rejected (E0834)
// 19. instance method called with `::` is rejected (E0835)
// 20. GC stress: implicit-self String field survives collection
// ---------------------------------------------------------------------------

#[test]
fn test_static_members_01_static_fn_basic() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    pub static fn add(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() { println(Math::add(1, 2)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn test_static_members_02_static_fn_multi_args() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    pub static fn sum3(a: i64, b: i64, c: i64) -> i64 { return a + b + c; }
}
fn main() { println(Math::sum3(10, 20, 12)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_members_03_static_calls_static_same_class() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    pub static fn add(a: i64, b: i64) -> i64 { return a + b; }
    pub static fn square(x: i64) -> i64 { return Math::add(x * x, 0); }
}
fn main() { println(Math::square(5)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "25\n");
}

#[test]
fn test_static_members_04_self_static_call_in_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub static fn make(value: i64) -> Counter { return new Counter(value); }
    pub fn clone_plus(n: i64) -> i64 {
        let next = Self::make(self.value + n);
        return next.value;
    }
}
fn main() {
    let c = new Counter(8);
    println(c.clone_plus(4));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_static_members_05_static_factory_returns_instance() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    value: i64;
    pub static fn start(at: i64) -> Counter { return new Counter(at); }
    pub fn get() -> i64 { return self.value; }
}
fn main() {
    let c = Counter::start(40);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "40\n");
}

#[test]
fn test_static_members_06_implicit_self_reads_field() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    pub init(self, name: String) {
        self.name = name;
    }
    name: String;
    pub fn getName() -> String { return self.name; }
}
fn main() {
    let u = new User("John");
    println(u.getName());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "John\n");
}

#[test]
fn test_static_members_07_implicit_self_with_params() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub fn plus(n: i64) -> i64 { return self.value + n; }
}
fn main() {
    let c = new Counter(40);
    println(c.plus(2));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_members_08_implicit_self_mutates_field() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub fn bump() { self.value = self.value + 1; }
    pub fn get() -> i64 { return self.value; }
}
fn main() {
    let c = new Counter(0);
    c.bump();
    c.bump();
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

#[test]
fn test_static_members_09_implicit_self_calls_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub fn get() -> i64 { return self.value; }
    pub fn doubled() -> i64 { return self.get() + self.get(); }
}
fn main() {
    let c = new Counter(21);
    println(c.doubled());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_members_10_static_fn_returns_bool() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    pub static fn positive(x: i64) -> bool { return x > 0; }
}
fn main() { println(Math::positive(5)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_static_members_11_static_fn_returns_f64() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    pub static fn half(x: f64) -> f64 { return x / 2.0; }
}
fn main() { println(Math::half(5.0)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "2.5\n");
}

#[test]
fn test_static_members_12_static_fn_returns_string() {
    let (out, ok) = compile_and_run(
        r#"
class Greeter {
    pub static fn hello() -> String { return "hi"; }
}
fn main() { println(Greeter::hello()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "hi\n");
}

#[test]
fn test_static_members_13_implicit_self_string_field() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    pub init(self, name: String) {
        self.name = name;
    }
    name: String;
    pub fn shout() -> String { return self.name + "!"; }
}
fn main() {
    let u = new User("Ada");
    println(u.shout());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Ada!\n");
}

#[test]
fn test_static_members_14_legacy_explicit_self_still_compiles() {
    // Migration compatibility: an explicit `self` parameter on an instance
    // method is still accepted in Stage 1.
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub fn get(self) -> i64 { return self.value; }
}
fn main() {
    let c = new Counter(7);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_static_members_15_static_and_instance_coexist() {
    let (out, ok) = compile_and_run(
        r#"
class Adder {
    pub init(self, base: i64) {
        self.base = base;
    }
    base: i64;
    pub fn add_base(n: i64) -> i64 { return self.base + n; }
    pub static fn pure(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() {
    let a = new Adder(10);
    println(a.add_base(5));
    println(Adder::pure(2, 3));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n5\n");
}

#[test]
fn test_static_members_16_self_in_static_method_rejected() {
    assert_compile_error_contains(
        r#"
class Math {
    value: i64;
    pub static fn bad() -> i64 { return self.value; }
}
fn main() {}
"#,
        &["error[E0831]", "`self` is not available in static method"],
    );
}

#[test]
fn test_static_members_17_explicit_self_on_static_is_parse_error() {
    assert_compile_error_contains(
        r#"
class Math {
    pub static fn bad(self) -> i64 { return 1; }
}
fn main() {}
"#,
        &["error[E0831]", "static methods cannot take `self`"],
    );
}

#[test]
fn test_static_members_18_static_called_with_dot_rejected() {
    assert_compile_error_contains(
        r#"
class Math {
    pub static fn add(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() {
    let m = new Math();
    println(m.add(1, 2));
}
"#,
        &[
            "error[E0834]",
            "static method called with `.`",
            "write `Math::add` instead",
        ],
    );
}

#[test]
fn test_static_members_19_instance_called_with_colon_rejected() {
    assert_compile_error_contains(
        r#"
class Box {
    v: i64;
    pub fn get() -> i64 { return self.v; }
}
fn main() {
    println(Box::get());
}
"#,
        &["error[E0835]", "instance method called with `::`"],
    );
}

#[test]
fn test_static_members_20_implicit_self_gc_stress() {
    // Under GC-on-every-allocation, the implicit-self receiver and its String
    // field must stay rooted across the body's allocations.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class User {
    pub init(self, name: String) {
        self.name = name;
    }
    name: String;
    pub fn decorated() -> String { return "[" + self.name + "]"; }
}
fn main() {
    let u = new User("x");
    println(u.decorated());
}
"#,
    );
    assert!(ok, "implicit-self String field should survive GC stress");
    assert_eq!(out, "[x]\n");
}

// ---------------------------------------------------------------------------
// Immutable static properties — willow-qsqf Stage 2. A `static name: T = expr`
// property lives in global storage, is initialized once before `main`, and is
// read as `ClassName::property`.
//
//  1. static i64 property read
//  2. static String property read
//  3. static bool property read
//  4. static f64 property read
//  5. static property read inside a static method of the same class
//  6. static property read inside an instance method
//  7. a later static may reference an earlier one of the same class
//  8. static property used in arithmetic
//  9. multiple classes each with their own statics (no collision)
// 10. static property initialized from a static method call
// 11. missing initializer is rejected (E0830)
// 12. initializer type mismatch is rejected (E0301)
// 13. `self` in a static initializer is rejected (E0837)
// 14. forward reference to a later static is rejected (E0838)
// 15. instance field accessed via `::` is rejected (E0835)
// 16. reading an unknown static property is rejected
// 17. assigning to an immutable static is rejected (compile error)
// 18. GC stress: static String survives collection (slot rooting)
// 19. GC stress: static String read repeatedly stays valid
// 20. private static property is not accessible from outside the class
// ---------------------------------------------------------------------------

#[test]
fn test_static_prop_01_i64() {
    let (out, ok) = compile_and_run(
        r#"
class Config { pub static version: i64 = 7; }
fn main() { println(Config::version); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_static_prop_02_string() {
    let (out, ok) = compile_and_run(
        r#"
class Config { pub static name: String = "willow"; }
fn main() { println(Config::name); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "willow\n");
}

#[test]
fn test_static_prop_03_bool() {
    let (out, ok) = compile_and_run(
        r#"
class Config { pub static enabled: bool = true; }
fn main() { println(Config::enabled); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_static_prop_04_f64() {
    let (out, ok) = compile_and_run(
        r#"
class Config { pub static ratio: f64 = 2.5; }
fn main() { println(Config::ratio); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "2.5\n");
}

#[test]
fn test_static_prop_05_read_in_static_method() {
    let (out, ok) = compile_and_run(
        r#"
class Limits {
    pub static max: i64 = 100;
    pub static fn cap() -> i64 { return Limits::max; }
}
fn main() { println(Limits::cap()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\n");
}

#[test]
fn test_static_prop_06_read_in_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class Widget {
    pub init(self, id: i64) {
        self.id = id;
    }
    id: i64;
    pub static count: i64 = 3;
    pub fn total() -> i64 { return self.id + Widget::count; }
}
fn main() {
    let w = new Widget(39);
    println(w.total());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_prop_07_references_earlier_static() {
    let (out, ok) = compile_and_run(
        r#"
class C {
    pub static a: i64 = 10;
    pub static b: i64 = C::a + 1;
}
fn main() { println(C::b); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "11\n");
}

#[test]
fn test_static_prop_08_in_arithmetic() {
    let (out, ok) = compile_and_run(
        r#"
class K { pub static base: i64 = 20; }
fn main() { println(K::base * 2 + 2); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_prop_09_multiple_classes_no_collision() {
    let (out, ok) = compile_and_run(
        r#"
class A { pub static v: i64 = 1; }
class B { pub static v: i64 = 2; }
fn main() {
    println(A::v);
    println(B::v);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

#[test]
fn test_static_prop_10_initialized_from_static_method() {
    let (out, ok) = compile_and_run(
        r#"
class Seed {
    pub static fn make() -> i64 { return 42; }
    pub static value: i64 = Seed::make();
}
fn main() { println(Seed::value); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_prop_11_missing_initializer_rejected() {
    assert_compile_error_contains(
        r#"
class C { static x: i64; }
fn main() {}
"#,
        &["error[E0830]", "requires an initializer"],
    );
}

#[test]
fn test_static_prop_12_initializer_type_mismatch_rejected() {
    assert_compile_error_contains(
        r#"
class C { static x: i64 = true; }
fn main() {}
"#,
        &["error[E0301]"],
    );
}

#[test]
fn test_static_prop_13_self_in_initializer_rejected() {
    assert_compile_error_contains(
        r#"
class C {
    x: i64;
    static y: i64 = self.x;
}
fn main() {}
"#,
        &["error[E0837]", "static property initializer"],
    );
}

#[test]
fn test_static_prop_14_forward_reference_rejected() {
    assert_compile_error_contains(
        r#"
class C {
    static b: i64 = C::a + 1;
    static a: i64 = 1;
}
fn main() {}
"#,
        &["error[E0838]", "used before it is initialized"],
    );
}

#[test]
fn test_static_prop_15_instance_field_via_colon_rejected() {
    assert_compile_error_contains(
        r#"
class C { v: i64; }
fn main() {
    let x = C::v;
    println(x);
}
"#,
        &["error[E0835]", "requires an object"],
    );
}

#[test]
fn test_static_prop_16_unknown_static_property_rejected() {
    assert_compile_error_contains(
        r#"
class C { pub static a: i64 = 1; }
fn main() {
    let x = C::missing;
    println(x);
}
"#,
        &["error[E0502]", "no static property"],
    );
}

#[test]
fn test_static_prop_17_assign_to_immutable_static_rejected() {
    // Immutable static properties cannot be reassigned (willow-qsqf §5.1). In
    // Stage 2 this is a compile error (static-field assignment + the dedicated
    // E0832 message arrive with `static mut` in Stage 3).
    let (_out, ok) = compile_and_run(
        r#"
class C { pub static x: i64 = 1; }
fn main() { C::x = 2; }
"#,
    );
    assert!(!ok, "assigning to an immutable static must not compile");
}

#[test]
fn test_static_prop_18_string_survives_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Config { pub static name: String = "willow"; }
fn main() { println(Config::name); }
"#,
    );
    assert!(ok, "static String must survive GC stress");
    assert_eq!(out, "willow\n");
}

#[test]
fn test_static_prop_19_string_read_repeatedly_under_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Config { pub static name: String = "ok"; }
fn main() {
    println(Config::name);
    println(Config::name);
    println(Config::name);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "ok\nok\nok\n");
}

#[test]
fn test_static_prop_20_private_static_not_accessible_outside() {
    assert_compile_error_contains(
        r#"
class C { static secret: i64 = 1; }
fn main() {
    println(C::secret);
}
"#,
        &["error[E0419]", "private"],
    );
}

// ---------------------------------------------------------------------------
// Mutable static properties + mutability enforcement — willow-qsqf Stage 3.
// `static mut name: T = expr` is reassignable via `ClassName::name = value`;
// a plain `static` rejects assignment (E0832).
//
//  1. static mut i64 reassigned and read back
//  2. static mut updated relative to its own value
//  3. static mut String reassigned
//  4. static mut bool reassigned
//  5. static mut f64 reassigned
//  6. static method mutates a static mut of its class
//  7. instance method mutates a static mut of its class
//  8. mutation persists across separate method calls (shared state)
//  9. assigning to an immutable static is rejected (E0832)
// 10. E0832 help mentions `static mut`
// 11. assigning to an unknown static is rejected
// 12. type mismatch on static mut assignment is rejected
// 13. static mut starts from its initializer value
// 14. two static mut properties are independent
// 15. static mut i64 reassigned under GC stress
// 16. static mut String reassigned under GC stress (old value collectible)
// 17. static mut String reassigned many times under GC stress
// 18. reassigned static mut readable from another class's method
// 19. static mut bool toggled in a loop
// 20. private static mut not assignable from outside (E0419)
// ---------------------------------------------------------------------------

#[test]
fn test_static_mut_01_i64_reassign() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut n: i64 = 1; }
fn main() {
    S::n = 42;
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_mut_02_update_relative_to_self() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut n: i64 = 10; }
fn main() {
    S::n = S::n + 32;
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_mut_03_string_reassign() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut s: String = "a"; }
fn main() {
    S::s = "b";
    println(S::s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "b\n");
}

#[test]
fn test_static_mut_04_bool_reassign() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut flag: bool = false; }
fn main() {
    S::flag = true;
    println(S::flag);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_static_mut_05_f64_reassign() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut r: f64 = 1.0; }
fn main() {
    S::r = 2.5;
    println(S::r);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2.5\n");
}

#[test]
fn test_static_mut_06_mutated_by_static_method() {
    let (out, ok) = compile_and_run(
        r#"
class S {
    pub static mut n: i64 = 0;
    pub static fn add(x: i64) { S::n = S::n + x; }
}
fn main() {
    S::add(40);
    S::add(2);
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_mut_07_mutated_by_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class S {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub static mut n: i64 = 0;
    pub fn record() { S::n = self.v; }
}
fn main() {
    let s = new S(7);
    s.record();
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_static_mut_08_shared_across_calls() {
    let (out, ok) = compile_and_run(
        r#"
class S {
    pub static mut n: i64 = 0;
    pub static fn inc() { S::n = S::n + 1; }
}
fn main() {
    S::inc();
    S::inc();
    S::inc();
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn test_static_mut_09_immutable_assign_rejected() {
    assert_compile_error_contains(
        r#"
class C { pub static x: i64 = 1; }
fn main() { C::x = 2; }
"#,
        &[
            "error[E0832]",
            "cannot assign to immutable static property `C::x`",
        ],
    );
}

#[test]
fn test_static_mut_10_immutable_assign_help_mentions_static_mut() {
    assert_compile_error_contains(
        r#"
class C { pub static x: i64 = 1; }
fn main() { C::x = 2; }
"#,
        &["static mut"],
    );
}

#[test]
fn test_static_mut_11_assign_unknown_static_rejected() {
    assert_compile_error_contains(
        r#"
class C { pub static mut x: i64 = 1; }
fn main() { C::missing = 2; }
"#,
        &["error[E0502]", "no static property"],
    );
}

#[test]
fn test_static_mut_12_assign_type_mismatch_rejected() {
    assert_compile_error_contains(
        r#"
class C { pub static mut x: i64 = 1; }
fn main() { C::x = true; }
"#,
        &["mismatched types"],
    );
}

#[test]
fn test_static_mut_13_starts_from_initializer() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut n: i64 = 99; }
fn main() { println(S::n); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

#[test]
fn test_static_mut_14_two_props_independent() {
    let (out, ok) = compile_and_run(
        r#"
class S {
    pub static mut a: i64 = 0;
    pub static mut b: i64 = 0;
}
fn main() {
    S::a = 1;
    S::b = 2;
    println(S::a);
    println(S::b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

#[test]
fn test_static_mut_15_i64_reassign_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class S { pub static mut n: i64 = 0; }
fn main() {
    S::n = 5;
    S::n = S::n + 5;
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

#[test]
fn test_static_mut_16_string_reassign_gc_stress() {
    // The slot is a permanent GC root, so the reassigned String stays live and
    // the old one becomes collectible — must be safe under GC stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class S { pub static mut s: String = "old"; }
fn main() {
    S::s = "new";
    println(S::s);
}
"#,
    );
    assert!(ok, "reassigned static mut String must survive GC stress");
    assert_eq!(out, "new\n");
}

#[test]
fn test_static_mut_17_string_many_reassigns_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class S {
    pub static mut s: String = "0";
    pub static fn set(v: String) { S::s = v; }
}
fn main() {
    S::set("a");
    S::set("b");
    S::set("c");
    println(S::s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "c\n");
}

#[test]
fn test_static_mut_18_read_from_other_class() {
    let (out, ok) = compile_and_run(
        r#"
class State { pub static mut n: i64 = 0; }
class Reader {
    pub static fn get() -> i64 { return State::n; }
}
fn main() {
    State::n = 42;
    println(Reader::get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_mut_19_bool_toggled_in_loop() {
    let (out, ok) = compile_and_run(
        r#"
class S { pub static mut n: i64 = 0; }
fn main() {
    let mut i = 0;
    while i < 5 {
        S::n = S::n + i;
        i = i + 1;
    }
    println(S::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

#[test]
fn test_static_mut_20_private_mut_not_assignable_outside() {
    assert_compile_error_contains(
        r#"
class C { static mut x: i64 = 1; }
fn main() { C::x = 2; }
"#,
        &["error[E0419]", "private"],
    );
}

// ---------------------------------------------------------------------------
// Static members: visibility, inheritance, interfaces — willow-qsqf Stage 4.
// Static members are non-virtual (resolved by type name, inherited statics
// reachable through a subclass, redefinition rejected); interfaces reject
// static members; explicit `self` keeps a migration path.
//
//  1. static fn in an interface is rejected (E0836)
//  2. static property in an interface is rejected (E0836)
//  3. static mut property in an interface is rejected (E0836)
//  4. subclass redefining an inherited static property is rejected (E0839)
//  5. subclass redefining an inherited static method is rejected (E0839)
//  6. E0839 names the hidden inherited member
//  7. distinct static names across base/child are allowed
//  8. an inherited static property is readable through the subclass
//  9. an inherited static is readable inside a subclass static method
// 10. an inherited static mut is assignable through the subclass
// 11. base and child each expose their own statics (non-virtual)
// 12. two-level inheritance: grandchild reads a grandparent static
// 13. interface instance method satisfied by an implicit-self method
// 14. interface default method (explicit self) still works
// 15. private static is not accessible from outside (E0419)
// 16. private static IS accessible from a same-class static method
// 17. protected static IS accessible from a subclass method
// 18. explicit `self` instance method still compiles (migration path)
// 19. explicit `self` on a static fn is still rejected (E0831)
// 20. GC stress: an inherited static String read through a subclass is valid
// ---------------------------------------------------------------------------

#[test]
fn test_static_s4_01_static_fn_in_interface_rejected() {
    assert_compile_error_contains(
        r#"
interface I { static fn helper() -> i64; }
fn main() {}
"#,
        &["error[E0836]", "static interface members are not supported"],
    );
}

#[test]
fn test_static_s4_02_static_prop_in_interface_rejected() {
    assert_compile_error_contains(
        r#"
interface I { static x: i64 = 1; }
fn main() {}
"#,
        &["error[E0836]"],
    );
}

#[test]
fn test_static_s4_03_static_mut_in_interface_rejected() {
    assert_compile_error_contains(
        r#"
interface I { static mut x: i64 = 1; }
fn main() {}
"#,
        &["error[E0836]"],
    );
}

#[test]
fn test_static_s4_04_subclass_hides_static_prop_rejected() {
    assert_compile_error_contains(
        r#"
open class Base { pub static x: i64 = 1; }
class Child extends Base { pub static x: i64 = 2; }
fn main() {}
"#,
        &["error[E0839]", "hides inherited static member"],
    );
}

#[test]
fn test_static_s4_05_subclass_hides_static_method_rejected() {
    assert_compile_error_contains(
        r#"
open class Base { pub static fn h() -> i64 { return 1; } }
class Child extends Base { pub static fn h() -> i64 { return 2; } }
fn main() {}
"#,
        &["error[E0839]", "hides inherited static member"],
    );
}

#[test]
fn test_static_s4_06_hiding_error_names_member() {
    assert_compile_error_contains(
        r#"
open class Base { pub static x: i64 = 1; }
class Child extends Base { pub static x: i64 = 2; }
fn main() {}
"#,
        &["Child::x", "Base::x"],
    );
}

#[test]
fn test_static_s4_07_distinct_names_allowed() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { pub static x: i64 = 1; }
class Child extends Base { pub static y: i64 = 2; }
fn main() {
    println(Base::x);
    println(Child::y);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

#[test]
fn test_static_s4_08_inherited_static_readable_via_subclass() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { pub static x: i64 = 7; }
class Child extends Base {}
fn main() { println(Child::x); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_static_s4_09_inherited_static_in_subclass_static_method() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { pub static base: i64 = 40; }
class Child extends Base {
    pub static fn doubled() -> i64 { return Base::base + 2; }
}
fn main() { println(Child::doubled()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_s4_10_inherited_static_mut_assignable_via_subclass() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { pub static mut n: i64 = 0; }
class Child extends Base {}
fn main() {
    Child::n = 9;
    println(Base::n);
    println(Child::n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n9\n");
}

#[test]
fn test_static_s4_11_base_and_child_own_statics() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { pub static a: i64 = 1; }
class Child extends Base { pub static b: i64 = 2; }
fn main() {
    println(Base::a);
    println(Child::a);
    println(Child::b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n1\n2\n");
}

#[test]
fn test_static_s4_12_two_level_inheritance_reads_grandparent_static() {
    let (out, ok) = compile_and_run(
        r#"
open class A { pub static v: i64 = 5; }
open class B extends A {}
class C extends B {}
fn main() { println(C::v); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn test_static_s4_13_interface_implicit_self_conformance() {
    let (out, ok) = compile_and_run(
        r#"
interface Named { fn name(self) -> String; }
class User implements Named {
    pub init(self, label: String) {
        self.label = label;
    }
    label: String;
    pub fn name(self) -> String { return self.label; }
}
fn describe(n: Named) -> String { return n.name(); }
fn main() {
    let u = new User("ada");
    println(describe(u));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "ada\n");
}

#[test]
fn test_static_s4_14_interface_default_method_works() {
    let (out, ok) = compile_and_run(
        r#"
interface Named {
    fn name(self) -> String;
    fn greeting(self) -> String { return self.name(); }
}
class User implements Named {
    pub init(self, label: String) {
        self.label = label;
    }
    label: String;
    pub fn name(self) -> String { return self.label; }
}
fn main() {
    let u = new User("bob");
    println(u.greeting());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "bob\n");
}

#[test]
fn test_static_s4_15_private_static_inaccessible_outside() {
    assert_compile_error_contains(
        r#"
class C { static secret: i64 = 1; }
fn main() { println(C::secret); }
"#,
        &["error[E0419]", "private"],
    );
}

#[test]
fn test_static_s4_16_private_static_accessible_in_same_class() {
    let (out, ok) = compile_and_run(
        r#"
class C {
    static secret: i64 = 42;
    pub static fn reveal() -> i64 { return C::secret; }
}
fn main() { println(C::reveal()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_static_s4_17_protected_static_accessible_in_subclass() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { prot static p: i64 = 5; }
class Child extends Base {
    pub static fn get() -> i64 { return Base::p; }
}
fn main() { println(Child::get()); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn test_static_s4_18_explicit_self_still_compiles() {
    let (out, ok) = compile_and_run(
        r#"
class C {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let c = new C(8);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "8\n");
}

#[test]
fn test_static_s4_19_explicit_self_on_static_rejected() {
    assert_compile_error_contains(
        r#"
class C { pub static fn bad(self) -> i64 { return 1; } }
fn main() {}
"#,
        &["error[E0831]", "static methods cannot take `self`"],
    );
}

#[test]
fn test_static_s4_20_inherited_static_string_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
open class Base { pub static name: String = "willow"; }
class Child extends Base {}
fn main() { println(Child::name); }
"#,
    );
    assert!(
        ok,
        "inherited static String via subclass must survive GC stress"
    );
    assert_eq!(out, "willow\n");
}
