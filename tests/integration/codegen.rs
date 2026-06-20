use super::support::*;

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

// ---------------------------------------------------------------------------
// `new` object creation + `init` constructors — willow-scq2 Stage 1.
//
//  1. explicit constructor + method call
//  2. implicit memberwise constructor (no init)
//  3. implicit memberwise sums fields
//  4. constructor with a String field
//  5. constructor validation logic on the valid path
//  6. constructor runtime panic on invalid input
//  7. zero-arg explicit constructor
//  8. `new` result used inline (method call on it)
//  9. constructor assigns from a computed expression
// 10. explicit init's arity is used (not memberwise) — 1 arg, 2 fields
// 11. implicit memberwise with mixed field types
// 12. missing field initialization is rejected (E0842)
// 13. returning a value from init is rejected (E0841)
// 14. declaring a return type on init is rejected (E0840)
// 15. calling init via `Type::init(...)` is rejected (E0843)
// 16. calling init via `obj.init(...)` is rejected (E0843)
// 17. `new` on an unknown class is rejected (E0844)
// 18. wrong constructor argument count is rejected (E0845)
// 19. wrong constructor argument type is rejected
// 20. GC stress: constructed object with a String field survives collection
// 21. implicit memberwise constructor includes inherited instance fields
// 22. subclass init needing base field initialization is rejected (E0848)
// 23. subclass init needing base init logic is rejected (E0848)
// 24. subclass init is allowed when the base has no initialization requirement
// 25. super.init calls an explicit base init
// 26. super.init fills implicit base fields
// 27. protected base init is callable from a subclass
// 28. private base init is rejected from a subclass
// 29. super.init must be the first constructor statement
// 30. super.init outside a constructor is rejected
// 31. init requires an explicit self receiver
// 32. init self receiver must be bare
// 33. private init rejects external new
// 34. public init allows external new
// 35. protected init rejects external new
// 36. private init allows an owner factory
// 37. implicit memberwise rejects private fields outside the owner
// 38. implicit memberwise allows an owner factory for private fields
// 39. static init is rejected with a constructor-specific diagnostic
// 40. fn init method syntax is rejected
// 41. static fn init method syntax is rejected
// ---------------------------------------------------------------------------

#[test]
fn test_new_ctor_01_explicit_constructor() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    name: String;
    pub init(self, name: String) { self.name = name; }
    pub fn label(self) -> String { return self.name; }
}
fn main() {
    let u = new User("John");
    println(u.label());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "John\n");
}

#[test]
fn test_new_ctor_02_implicit_memberwise() {
    let (out, ok) = compile_and_run(
        r#"
class Point { pub x: i64; pub y: i64; }
fn main() {
    let p = new Point(3, 4);
    println(p.x);
    println(p.y);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n4\n");
}

#[test]
fn test_new_ctor_03_implicit_sum() {
    let (out, ok) = compile_and_run(
        r#"
class Point { pub x: i64; pub y: i64; }
fn main() {
    let p = new Point(3, 4);
    println(p.x + p.y);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_new_ctor_04_string_field() {
    let (out, ok) = compile_and_run(
        r#"
class Greeting {
    text: String;
    pub init(self, name: String) { self.text = "hi " + name; }
    pub fn get(self) -> String { return self.text; }
}
fn main() {
    let g = new Greeting("ada");
    println(g.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hi ada\n");
}

#[test]
fn test_new_ctor_05_validation_valid_path() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    pub age: i64;
    pub init(self, age: i64) {
        if age < 0 { panic("bad age"); }
        self.age = age;
    }
}
fn main() {
    let u = new User(20);
    println(u.age);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "20\n");
}

#[test]
fn test_new_ctor_06_validation_panics() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
class User {
    pub age: i64;
    pub init(self, age: i64) {
        if age < 0 { panic("bad age"); }
        self.age = age;
    }
}
fn main() {
    let u = new User(-1);
    println(u.age);
}
"#,
    );
    assert!(
        !ok,
        "constructor panic should make the program exit non-zero"
    );
    assert!(out.contains("bad age"), "panic message expected: {out}");
}

#[test]
fn test_new_ctor_07_zero_arg_constructor() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub n: i64;
    pub init(self) { self.n = 0; }
}
fn main() {
    let c = new Counter();
    println(c.n);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

#[test]
fn test_new_ctor_08_used_inline() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    name: String;
    pub init(self, name: String) { self.name = name; }
    pub fn label(self) -> String { return self.name; }
}
fn main() {
    println(new User("inline").label());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "inline\n");
}

#[test]
fn test_new_ctor_09_computed_field() {
    let (out, ok) = compile_and_run(
        r#"
class Square {
    pub area: i64;
    pub init(self, side: i64) { self.area = side * side; }
}
fn main() {
    let s = new Square(5);
    println(s.area);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "25\n");
}

#[test]
fn test_new_ctor_10_explicit_init_arity_used() {
    // Two fields but a 1-arg init: `new User("x")` is valid because the explicit
    // init (not the memberwise constructor) determines the signature.
    let (out, ok) = compile_and_run(
        r#"
class User {
    name: String;
    pub age: i64;
    pub init(self, name: String) {
        self.name = name;
        self.age = 99;
    }
    pub fn label(self) -> String { return self.name; }
}
fn main() {
    let u = new User("x");
    println(u.label());
    println(u.age);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "x\n99\n");
}

#[test]
fn test_new_ctor_11_implicit_mixed_types() {
    let (out, ok) = compile_and_run(
        r#"
class Mix { pub a: i64; pub b: bool; }
fn main() {
    let m = new Mix(7, true);
    println(m.a);
    println(m.b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\ntrue\n");
}

#[test]
fn test_new_ctor_12_missing_field_init_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    name: String;
    age: i64;
    init(self, name: String) { self.name = name; }
}
fn main() {}
"#,
        &["error[E0842]", "not initialized by constructor"],
    );
}

#[test]
fn test_new_ctor_13_return_value_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    name: String;
    init(self, name: String) {
        self.name = name;
        return self;
    }
}
fn main() {}
"#,
        &["error[E0841]", "cannot return a value"],
    );
}

#[test]
fn test_new_ctor_14_return_type_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    name: String;
    init(self, name: String) -> User { self.name = name; }
}
fn main() {}
"#,
        &["error[E0840]", "must not declare a return type"],
    );
}

#[test]
fn test_new_ctor_15_direct_static_call_rejected() {
    assert_compile_error_contains(
        r#"
class U { init(self) {} }
fn main() { U::init(); }
"#,
        &["error[E0843]", "can only be called with `new`"],
    );
}

#[test]
fn test_new_ctor_16_direct_instance_call_rejected() {
    assert_compile_error_contains(
        r#"
class U {
    v: i64;
    init(self) { self.v = 1; }
    pub fn f(self) { self.init(); }
}
fn main() {}
"#,
        &["error[E0843]", "can only be called with `new`"],
    );
}

#[test]
fn test_new_ctor_17_unknown_class_rejected() {
    assert_compile_error_contains(
        r#"
fn main() { let x = new Missing(); }
"#,
        &["error[E0844]", "unknown class `Missing`"],
    );
}

#[test]
fn test_new_ctor_18_wrong_arg_count_rejected() {
    assert_compile_error_contains(
        r#"
class Point { pub x: i64; pub y: i64; }
fn main() { let p = new Point(1); }
"#,
        &["error[E0845]", "expects 2 argument(s) but got 1"],
    );
}

#[test]
fn test_new_ctor_19_wrong_arg_type_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    pub age: i64;
    pub init(self, age: i64) { self.age = age; }
}
fn main() { let u = new User("not an int"); }
"#,
        &["constructor argument 1"],
    );
}

#[test]
fn test_new_ctor_20_gc_stress_string_field() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class User {
    name: String;
    pub init(self, name: String) { self.name = name + "!"; }
    pub fn get(self) -> String { return self.name; }
}
fn main() {
    let u = new User("John");
    println(u.get());
}
"#,
    );
    assert!(
        ok,
        "constructed object with String field must survive GC stress"
    );
    assert_eq!(out, "John!\n");
}

#[test]
fn test_new_ctor_21_implicit_inherited_memberwise_constructor() {
    let (out, ok) = compile_and_run(
        r#"
open class Base { pub id: i64; }
class Child extends Base { pub name: String; }
fn main() {
    let c = new Child(7, "ok");
    println(c.id);
    println(c.name);
}
"#,
    );
    assert!(
        ok,
        "implicit subclass constructor should include base fields"
    );
    assert_eq!(out, "7\nok\n");
}

#[test]
fn test_new_ctor_22_subclass_init_with_base_fields_rejected() {
    assert_compile_error_contains(
        r#"
open class Base { pub id: i64; }
class Child extends Base {
    pub name: String;
    pub init(self, name: String) { self.name = name; }
}
fn main() {}
"#,
        &["error[E0848]", "super.init"],
    );
}

#[test]
fn test_new_ctor_23_subclass_init_with_base_init_rejected() {
    assert_compile_error_contains(
        r#"
open class Base { pub init(self) {} }
class Child extends Base {
    pub value: i64;
    pub init(self, value: i64) { self.value = value; }
}
fn main() {}
"#,
        &["error[E0848]", "base class requires initialization"],
    );
}

#[test]
fn test_new_ctor_24_subclass_init_with_empty_base_allowed() {
    let (out, ok) = compile_and_run(
        r#"
open class Base {}
class Child extends Base {
    pub value: i64;
    pub init(self, value: i64) { self.value = value; }
}
fn main() {
    let c = new Child(9);
    println(c.value);
}
"#,
    );
    assert!(ok, "empty base class should not require super.init");
    assert_eq!(out, "9\n");
}

#[test]
fn test_new_ctor_25_super_init_calls_explicit_base_init() {
    let (out, ok) = compile_and_run(
        r#"
open class Base {
    pub id: i64;
    pub init(self, id: i64) { self.id = id; }
}
class Child extends Base {
    pub name: String;
    pub init(self, id: i64, name: String) {
        super.init(id);
        self.name = name;
    }
}
fn main() {
    let c = new Child(7, "ok");
    println(c.id);
    println(c.name);
}
"#,
    );
    assert!(ok, "super.init should call the explicit base constructor");
    assert_eq!(out, "7\nok\n");
}

#[test]
fn test_new_ctor_26_super_init_fills_implicit_base_fields() {
    let (out, ok) = compile_and_run(
        r#"
open class Base {
    pub id: i64;
    pub label: String;
}
class Child extends Base {
    pub bonus: i64;
    pub init(self, id: i64, label: String, bonus: i64) {
        super.init(id, label);
        self.bonus = bonus;
    }
}
fn main() {
    let c = new Child(7, "base", 3);
    println(c.id);
    println(c.label);
    println(c.bonus);
}
"#,
    );
    assert!(ok, "super.init should lower implicit base memberwise init");
    assert_eq!(out, "7\nbase\n3\n");
}

#[test]
fn test_new_ctor_27_super_init_can_call_protected_base_init() {
    let (out, ok) = compile_and_run(
        r#"
open class Base {
    pub id: i64;
    prot init(self, id: i64) { self.id = id; }
}
class Child extends Base {
    pub init(self, id: i64) { super.init(id); }
}
fn main() {
    let c = new Child(9);
    println(c.id);
}
"#,
    );
    assert!(ok, "subclass should be able to call protected base init");
    assert_eq!(out, "9\n");
}

#[test]
fn test_new_ctor_28_super_init_rejects_private_base_init() {
    assert_compile_error_contains(
        r#"
open class Base {
    pub id: i64;
    init(self, id: i64) { self.id = id; }
}
class Child extends Base {
    pub init(self, id: i64) { super.init(id); }
}
fn main() {}
"#,
        &["error[E0846]", "constructor of `Base` is not visible"],
    );
}

#[test]
fn test_new_ctor_29_super_init_must_be_first_statement() {
    assert_compile_error_contains(
        r#"
open class Base { pub id: i64; }
class Child extends Base {
    pub name: String;
    pub init(self, id: i64, name: String) {
        self.name = name;
        super.init(id);
    }
}
fn main() {}
"#,
        &["error[E0848]", "must be the first statement"],
    );
}

#[test]
fn test_new_ctor_30_super_init_outside_constructor_rejected() {
    assert_compile_error_contains(
        r#"
class Plain {
    pub fn bad(self) { super.init(); }
}
fn main() {}
"#,
        &["error[E0848]", "can only be used inside a constructor"],
    );
}

#[test]
fn test_new_ctor_31_init_requires_explicit_self() {
    assert_compile_error_contains(
        r#"
class User {
    pub init(name: String) {}
}
fn main() {}
"#,
        &[
            "error[E0849]",
            "constructor `init` must declare `self` as its first parameter",
        ],
    );
}

#[test]
fn test_new_ctor_32_init_self_must_be_bare() {
    assert_compile_error_contains(
        r#"
class User {
    pub init(self: User) {}
}
fn main() {}
"#,
        &["error[E0849]", "constructor `self` parameter must be bare"],
    );
}

#[test]
fn test_new_ctor_33_private_init_rejects_external_new() {
    assert_compile_error_contains(
        r#"
class Secret {
    value: i64;
    init(self, value: i64) { self.value = value; }
}
fn main() {
    let secret = new Secret(1);
}
"#,
        &["error[E0846]", "constructor of `Secret` is not visible"],
    );
}

#[test]
fn test_new_ctor_34_public_init_allows_external_new() {
    let (out, ok) = compile_and_run(
        r#"
class Token {
    pub value: i64;
    pub init(self, value: i64) { self.value = value; }
}
fn main() {
    let token = new Token(5);
    println(token.value);
}
"#,
    );
    assert!(ok, "public constructor should be visible to external new");
    assert_eq!(out, "5\n");
}

#[test]
fn test_new_ctor_35_protected_init_rejects_external_new() {
    assert_compile_error_contains(
        r#"
open class Base {
    prot init(self) {}
}
fn main() {
    let base = new Base();
}
"#,
        &["error[E0846]", "constructor of `Base` is not visible"],
    );
}

#[test]
fn test_new_ctor_36_private_init_allows_owner_factory() {
    let (out, ok) = compile_and_run(
        r#"
class Secret {
    value: i64;
    init(self, value: i64) { self.value = value; }
    pub static fn make(value: i64) -> Secret {
        return new Secret(value);
    }
    pub fn read(self) -> i64 { return self.value; }
}
fn main() {
    let secret = Secret::make(8);
    println(secret.read());
}
"#,
    );
    assert!(ok, "owner factory should be allowed to call private init");
    assert_eq!(out, "8\n");
}

#[test]
fn test_new_ctor_37_implicit_memberwise_private_field_rejects_external_new() {
    assert_compile_error_contains(
        r#"
class Secret {
    value: i64;
    pub fn read(self) -> i64 { return self.value; }
}
fn main() {
    let secret = new Secret(8);
    println(secret.read());
}
"#,
        &[
            "error[E0501]",
            "field `value` of class `Secret` is private",
            "memberwise constructor initializes a private field",
        ],
    );
}

#[test]
fn test_new_ctor_38_implicit_memberwise_private_field_allows_owner_factory() {
    let (out, ok) = compile_and_run(
        r#"
class Secret {
    value: i64;
    pub static fn make(value: i64) -> Secret {
        return new Secret(value);
    }
    pub fn read(self) -> i64 { return self.value; }
}
fn main() {
    let secret = Secret::make(8);
    println(secret.read());
}
"#,
    );
    assert!(
        ok,
        "owner factory should be allowed to use implicit memberwise"
    );
    assert_eq!(out, "8\n");
}

#[test]
fn test_new_ctor_39_static_init_modifier_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    static init(self) {}
}
fn main() {}
"#,
        &[
            "error[E0850]",
            "`static` is not allowed on constructor `init`",
        ],
    );
}

#[test]
fn test_new_ctor_40_fn_init_method_syntax_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    fn init(self) {}
}
fn main() {}
"#,
        &[
            "error[E0850]",
            "method name `init` is reserved for constructors",
        ],
    );
}

#[test]
fn test_new_ctor_41_static_fn_init_method_syntax_rejected() {
    assert_compile_error_contains(
        r#"
class User {
    static fn init() {}
}
fn main() {}
"#,
        &[
            "error[E0850]",
            "method name `init` is reserved for constructors",
        ],
    );
}

#[test]
fn test_self_field_assign_type_mismatch_is_error() {
    assert_compile_error_contains(
        r#"
class Typed {
    n: i64;
    pub fn bad(self) {
        self.n = true;
    }
}
fn main() {}
"#,
        &["mismatched types"],
    );
}

#[test]
fn test_gc_during_method_does_not_corrupt_self_receiver() {
    let (out, ok) = compile_and_run(
        r#"
class Holder {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn safe(self) -> i64 {
        gc_collect();
        return self.v;
    }
}
fn main() {
    let h = new Holder(55);
    println(h.safe());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "55\n");
}

// ── WillowString GC migration tests (requirements/willow_string_gc_requirements.md sec 11) ─

// 11.1: String literal survives gc_collect
#[test]
fn test_string_gc_11_1_literal_survives_gc_collect() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s = "hello";
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 11.2: String concatenation survives gc_collect
#[test]
fn test_string_gc_11_2_concat_survives_gc_collect() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s = "hello" + " " + "world";
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello world\n");
}

// 11.3: String field survives gc_collect
#[test]
fn test_string_gc_11_3_string_field_survives_gc_collect() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    pub name: String;
    pub fn get_name(self) -> String { return self.name; }
}
fn main() {
    let u = new User("alice");
    gc_collect();
    println(u.get_name());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "alice\n");
}

// 11.4: Multiple string fields can be concatenated
#[test]
fn test_string_gc_11_4_multiple_string_fields_concat() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    pub first: String;
    pub last: String;
    pub fn full(self) -> String { return self.first + " " + self.last; }
}
fn main() {
    let u = new User("Ada", "Lovelace");
    println(u.full());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Ada Lovelace\n");
}

// 11.5: Option<String> survives gc_collect
#[test]
fn test_string_gc_11_5_option_string_survives_gc() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s = Option::Some("hello");
    gc_collect();
    println(s.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 11.6: Result<String, String> survives gc_collect
#[test]
fn test_string_gc_11_6_result_string_survives_gc() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r: Result<String, String> = Result::Ok("ok");
    gc_collect();
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "ok\n");
}

// 11.7: Option<String> with gc_collect (nullable String pattern via Option)
#[test]
fn test_string_gc_11_7_nullable_string_survives_gc() {
    let (out, ok) = compile_and_run(
        r#"
fn make_opt(flag: bool) -> Option<String> {
    if flag {
        return Option::Some("hello");
    }
    return Option::None;
}
fn main() {
    let s = make_opt(true);
    gc_collect();
    if s.is_some() {
        println(s.unwrap());
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 11.8: Repeated string concatenation and GC does not crash
#[test]
fn test_string_gc_11_8_repeated_concat_no_crash() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut s = "a";
    let mut i = 0;
    while i < 10 {
        s = s + "b";
        gc_collect();
        i = i + 1;
    }
    println(s);
}
"#,
    );
    assert!(ok);
    // "a" + 10 "b"s = 11 chars + "\n" = 12 total
    assert_eq!(out.len(), "abbbbbbbbbb\n".len());
}

// String GC stress: multiple objects with String fields across GC cycles
#[test]
fn test_string_gc_stress_class_fields_across_gc_cycles() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub label: String;
    pub fn get_label(self) -> String { return self.label; }
}
fn main() {
    let a = new Node("alpha");
    let b = new Node("beta");
    gc_collect();
    let c = new Node("gamma");
    gc_collect();
    println(a.get_label() + " " + b.get_label() + " " + c.get_label());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "alpha beta gamma\n");
}

// ── T → T? implicit coercion (willow-thk) ────────────────────────────────────

// 1. let s: String? = literal compiles and prints
#[test]
fn test_nullable_coerce_string_literal_to_nullable() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s: String? = "hello";
    if s != nil { println(s); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 2. Function returning String? can return a plain String
#[test]
fn test_nullable_coerce_return_string_from_nullable_fn() {
    let (out, ok) = compile_and_run(
        r#"
fn greet(flag: bool) -> String? {
    if flag { return "hi"; }
    return nil;
}
fn main() {
    let a = greet(true);
    let b = greet(false);
    if a != nil { println(a); }
    if b == nil { println("nil"); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hi\nnil\n");
}

// 3. Passing T to T? parameter compiles
#[test]
fn test_nullable_coerce_pass_string_to_nullable_param() {
    let (out, ok) = compile_and_run(
        r#"
fn print_maybe(s: String?) {
    if s != nil { println(s); } else { println("empty"); }
}
fn main() {
    print_maybe("world");
    print_maybe(nil);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "world\nempty\n");
}

// 4. Unrelated type to T? is still a compile error
#[test]
fn test_nullable_coerce_unrelated_type_rejected() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let s: String? = 42;
}
"#
    ));
}

// 5. nil can still be assigned to T?
#[test]
fn test_nullable_coerce_nil_still_works() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s: String? = nil;
    if s == nil { println("nil"); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "nil\n");
}

// 6. Class T → T? coercion also works
#[test]
fn test_nullable_coerce_class_to_nullable() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } }
fn maybe(flag: bool) -> Box? {
    if flag { return new Box(99); }
    return nil;
}
fn main() {
    let b = maybe(true);
    if b != nil { println(b.get()); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// ── GC-managed temporary rooting (willow-5mb) ────────────────────────────────

// Chain of string concatenations: intermediate r1 = (a + b) must survive
// the GC that runs during the second concat allocation.
#[test]
fn test_gc_tmp_string_concat_chain_is_safe() {
    let (out, ok) = compile_and_run(
        r#"
class Names {
    pub first: String;
    pub last: String;
    pub fn full(self) -> String { return self.first + " " + self.last; }
}
fn main() {
    let n = new Names("Ada", "Lovelace");
    let s = n.first + " " + n.last;
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Ada Lovelace\n");
}

// Method return values used directly in concat must be safe.
#[test]
fn test_gc_tmp_method_return_in_concat_is_safe() {
    let (out, ok) = compile_and_run(
        r#"
fn bang(s: String) -> String { return s + "!"; }
fn main() {
    let s = bang("hello") + bang("world");
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello!world!\n");
}

// Object literal with String fields: partially-initialised object must not
// be collected while field initialisers are still being evaluated.
#[test]
fn test_gc_tmp_object_literal_not_collected_during_init() {
    let (out, ok) = compile_and_run(
        r#"
fn make_str(s: String) -> String { return s + "."; }
class Rec {
    pub a: String;
    pub b: String;
    pub fn both(self) -> String { return self.a + self.b; }
}
fn main() {
    let r = new Rec(make_str("x"), make_str("y"));
    println(r.both());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "x.y.\n");
}

// 4-level concat chain stress test.
#[test]
fn test_gc_tmp_four_level_concat_chain() {
    let (out, ok) = compile_and_run(
        r#"
class W { pub v: String; pub fn get(self) -> String { return self.v; } }
fn main() {
    let a = new W("a");
    let b = new W("b");
    let c = new W("c");
    let d = new W("d");
    let s = a.get() + b.get() + c.get() + d.get();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "abcd\n");
}

// ── Lambda return type inference (willow-cuq) ────────────────────────────────

// and_then with unannotated expression-body lambda
#[test]
fn test_lambda_infer_and_then_expr_body() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 { return Option::None; }
    return Option::Some(a / b);
}
fn main() {
    let r = safe_div(20, 4).and_then(|v: i64| safe_div(v, 2));
    println(r.is_some());
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n2\n");
}

// and_then with unannotated block-body lambda
#[test]
fn test_lambda_infer_and_then_block_body() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 { return Option::None; }
    return Option::Some(a / b);
}
fn main() {
    let r = safe_div(100, 5).and_then(|v: i64| {
        return safe_div(v, 4);
    });
    println(r.is_some());
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n5\n");
}

// map with unannotated lambda
#[test]
fn test_lambda_infer_map() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = Option::Some(7).map(|x: i64| x * 2);
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "14\n");
}

#[test]
fn test_lambda_context_infers_fn_parameter_type() {
    let (out, ok) = compile_and_run(
        r#"
fn apply(x: i64, f: fn(i64) -> i64) -> i64 {
    return f(x);
}

fn main() {
    println(apply(11, |x| x + 1));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_lambda_context_infers_option_map_parameter_type() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = Option::Some(7).map(|x| x * 2);
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "14\n");
}

#[test]
fn test_lambda_context_infers_let_annotation_parameter_type() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let f: fn(i64) -> i64 = |x| x * 3;
    println(f(4));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_lambda_context_infers_assignment_parameter_type() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut f: fn(i64) -> i64 = |x| x + 1;
    f = |x| x * 3;
    println(f(4));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

// or_else with unannotated lambda
#[test]
fn test_lambda_infer_or_else() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r: Option<i64> = Option::None;
    let r2 = r.or_else(|| Option::Some(99));
    println(r2.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// Result and_then with unannotated lambda
#[test]
fn test_lambda_infer_result_and_then() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r: Result<i64, String> = Result::Ok(10);
    let r2 = r.and_then(|v: i64| {
        return Result::Ok(v + 5);
    });
    println(r2.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n");
}

// Explicit annotation still works
#[test]
fn test_lambda_explicit_annotation_unchanged() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 { return Option::None; }
    return Option::Some(a / b);
}
fn main() {
    let r = safe_div(20, 4).and_then(|v: i64| -> Option<i64> { return safe_div(v, 2); });
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

// ── GC safety: remaining fixes (willow-7q1) ──────────────────────────────────

// Fix 2: GC-managed function parameter survives allocation in function body
#[test]
fn test_gc_safety_string_param_survives_alloc() {
    let (out, ok) = compile_and_run(
        r#"
fn echo_after_alloc(s: String) {
    let tmp = "x" + "y";
    gc_collect();
    println(s);
}
fn main() { echo_after_alloc("alive"); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "alive\n");
}

#[test]
fn test_gc_safety_class_param_survives_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub value: String; pub fn get(self) -> String { return self.value; } }
fn print_after_alloc(b: Box) {
    let tmp = "x" + "y";
    gc_collect();
    println(b.get());
}
fn main() { print_after_alloc(new Box("object alive")); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "object alive\n");
}

// Fix 3: self receiver survives allocation during method body
#[test]
fn test_gc_safety_self_receiver_survives_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class User {
    pub name: String;
    pub fn show(self) {
        let tmp = "x" + "y";
        gc_collect();
        println(self.name);
    }
}
fn main() {
    let u = new User("alice");
    u.show();
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "alice\n");
}

// Fix 3: method String parameter survives allocation
#[test]
fn test_gc_safety_method_string_param_survives_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Printer { pub fn show(self, s: String) {
    let tmp = "x" + "y";
    gc_collect();
    println(s);
} }
fn main() {
    let p = new Printer();
    p.show("method param alive");
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "method param alive\n");
}

// Fix 3: method class parameter survives allocation
#[test]
fn test_gc_safety_method_class_param_survives_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub value: String; pub fn get(self) -> String { return self.value; } }
class Printer { pub fn show(self, b: Box) {
    let tmp = "x" + "y";
    gc_collect();
    println(b.get());
} }
fn main() {
    let p = new Printer();
    p.show(new Box("box alive"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "box alive\n");
}

// Fix 5: GC-managed function call arguments survive later-argument allocation
#[test]
fn test_gc_safety_call_args_rooted_fn() {
    let (out, ok) = compile_and_run(
        r#"
fn make(s: String) -> String { return s + "!"; }
fn combine(a: String, b: String) -> String { return a + b; }
fn main() { println(combine(make("a"), make("b"))); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "a!b!\n");
}

// Fix 5: GC-managed method call arguments survive later-argument allocation
#[test]
fn test_gc_safety_call_args_rooted_method() {
    let (out, ok) = compile_and_run(
        r#"
class C {
    pub fn make(self, s: String) -> String { return s + "!"; }
    pub fn combine(self, a: String, b: String) -> String { return a + b; }
}
fn main() {
    let c = new C();
    println(c.combine(c.make("a"), c.make("b")));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "a!b!\n");
}

// Fix 5: GC-managed object arguments survive later-argument allocation
#[test]
fn test_gc_safety_call_args_object_rooted() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub value: String; pub fn get(self) -> String { return self.value; } }
fn make_box(s: String) -> Box { return new Box(s + "!"); }
fn combine(a: Box, b: Box) -> String { return a.get() + b.get(); }
fn main() { println(combine(make_box("a"), make_box("b"))); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "a!b!\n");
}

// ── GC root semantics: local objects survive gc_collect() inside the same scope ─

// Semantics doc: a GC-managed local is rooted until the function returns.
// gc_collect() inside the function does NOT free it; it is freed only after
// the caller performs a gc_collect() once the function's roots are popped.
#[test]
fn test_gc_local_survives_inner_collect() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub fn get(self) -> i64 { return self.v; } }
fn alloc_and_collect() -> i64 {
    let n = new Node(3);
    let r = n.get();
    gc_collect();
    // n is still rooted here (scope has not ended), so the Node is NOT freed
    return r;
}
fn main() {
    let r = alloc_and_collect();
    // The function has returned; n's root is popped. A collect now frees it.
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n0\n");
}

// The object is still allocated right after the inner gc_collect() (still rooted).
#[test]
fn test_gc_bytes_nonzero_after_inner_collect() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub v: i64; }
fn make_and_collect() -> i64 {
    let b = new Box(7);
    gc_collect();
    // b is still rooted: allocated_bytes > 0 here
    return gc_allocated_bytes();
}
fn main() {
    let during = make_and_collect();
    gc_collect();
    let after = gc_allocated_bytes();
    println(during > 0);
    println(after);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n0\n");
}

// Two calls: each call allocates, inner collect keeps it alive, outer collect frees.
#[test]
fn test_gc_two_calls_freed_after_outer_collect() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub fn get(self) -> i64 { return self.v; } }
fn alloc_and_collect(v: i64) -> i64 {
    let n = new Node(v);
    gc_collect();
    return n.get();
}
fn main() {
    let r1 = alloc_and_collect(10);
    let r2 = alloc_and_collect(20);
    gc_collect();
    println(r1 + r2);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "30\n0\n");
}

// String locals survive inner gc_collect() (concat result is still rooted).
// String literals ("hello", "!") are permanently interned and never freed;
// only the temporary concat result is freed after the function returns.
#[test]
fn test_gc_string_local_survives_inner_collect() {
    let (out, ok) = compile_and_run(
        r#"
fn make_and_collect(s: String) -> String {
    let t = s + "!";
    gc_collect();
    return t;
}
fn main() {
    let r = make_and_collect("hello");
    gc_collect();
    println(r);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello!\n");
}

// Nested functions: inner collect keeps the inner function's local alive,
// but the outer function's locals are also still rooted.
#[test]
fn test_gc_nested_scope_rooting() {
    let (out, ok) = compile_and_run(
        r#"
class N { pub v: i64; pub fn get(self) -> i64 { return self.v; } }
fn inner(v: i64) -> i64 {
    let a = new N(v);
    gc_collect();
    return a.get();
}
fn outer() -> i64 {
    let b = new N(100);
    let x = inner(42);
    return b.get() + x;
}
fn main() {
    let r = outer();
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "142\n0\n");
}

// ── std namespace and basic item imports (willow-4bv.2, Stage 2) ───────────
// The reserved `std` namespace is resolved against the built-in registry, not
// the filesystem. Single-item imports use `::` paths: `import std::mod::item;`.
// Stage 2 establishes namespace + resolver; concrete collection *types* arrive
// in Stage 3, so these tests import known items and use the ones the prelude
// and builtins already provide.

// Perspective 1: importing a known collections item resolves (compiles).
#[test]
fn test_std_import_collections_array_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
fn main() { println(1); }
"#,
    );
    assert!(ok, "import std::collections::Array should resolve");
    assert_eq!(out, "1\n");
}

// Perspective 2: importing std::collections::Map resolves.
#[test]
fn test_std_import_collections_map_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;
fn main() { println(2); }
"#,
    );
    assert!(ok, "import std::collections::Map should resolve");
    assert_eq!(out, "2\n");
}

// Perspective 3: importing std::option::Option resolves and Option is usable.
#[test]
fn test_std_import_option_resolves_and_usable() {
    let (out, ok) = compile_and_run(
        r#"
import std::option::Option;
fn main() {
    let x: Option<i64> = Option::Some(10);
    println(x.unwrap());
}
"#,
    );
    assert!(
        ok,
        "import std::option::Option should resolve and be usable"
    );
    assert_eq!(out, "10\n");
}

// Perspective 4: importing std::result::Result resolves and Result is usable.
#[test]
fn test_std_import_result_resolves_and_usable() {
    let (out, ok) = compile_and_run(
        r#"
import std::result::Result;
fn make() -> Result<i64, String> { return Result::Ok(5); }
fn main() {
    println(match make() { Result::Ok(v) => v, Result::Err(_) => -1, });
}
"#,
    );
    assert!(
        ok,
        "import std::result::Result should resolve and be usable"
    );
    assert_eq!(out, "5\n");
}

// Perspective 5: importing std::io::println (a builtin-keyword item) resolves.
#[test]
fn test_std_import_io_println_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std::io::println;
fn main() { println(7); }
"#,
    );
    assert!(ok, "import std::io::println should resolve");
    assert_eq!(out, "7\n");
}

// Perspective 6: importing std::io::print (a builtin-keyword item) resolves.
#[test]
fn test_std_import_io_print_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std::io::print;
fn main() { print(3); println(0); }
"#,
    );
    assert!(ok, "import std::io::print should resolve");
    assert_eq!(out, "30\n");
}

// Perspective 7: importing std::env items resolves.
#[test]
fn test_std_import_env_args_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std::env::args;
import std::env::program_name;
fn main() { println(4); }
"#,
    );
    assert!(ok, "import std::env items should resolve");
    assert_eq!(out, "4\n");
}

// Perspective 8: a whole-module import resolves.
#[test]
fn test_std_module_import_resolves() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections;
fn main() { println(8); }
"#,
    );
    assert!(ok, "import std::collections (module) should resolve");
    assert_eq!(out, "8\n");
}

// Perspective 9: multiple std imports coexist in one file.
#[test]
fn test_std_multiple_imports_coexist() {
    let (out, ok) = compile_and_run(
        r#"
import std::io::println;
import std::option::Option;
import std::result::Result;
import std::collections::Array;
fn main() {
    let o: Option<i64> = Option::Some(99);
    println(o.unwrap());
}
"#,
    );
    assert!(ok, "multiple std imports should coexist");
    assert_eq!(out, "99\n");
}

// Perspective 10: an unknown item in a known module reports E2006.
#[test]
fn test_std_unknown_item_reports_e2006() {
    assert_compile_error_contains(
        r#"
import std::collections::Vec;
fn main() { println(1); }
"#,
        &["error[E2006]", "no item `Vec` in `std::collections`"],
    );
}

// Perspective 11: a near-miss item name suggests the correct one.
#[test]
fn test_std_unknown_item_suggests_nearest() {
    assert_compile_error_contains(
        r#"
import std::collections::Aray;
fn main() { println(1); }
"#,
        &["error[E2006]", "did you mean `Array`?"],
    );
}

// Perspective 12: lists available items for an unknown item.
#[test]
fn test_std_unknown_item_lists_available() {
    assert_compile_error_contains(
        r#"
import std::io::flush;
fn main() { println(1); }
"#,
        &["error[E2006]", "available items:"],
    );
}

// Perspective 13: an unknown std module reports E2007.
#[test]
fn test_std_unknown_module_reports_e2007() {
    assert_compile_error_contains(
        r#"
import std::networking::Socket;
fn main() { println(1); }
"#,
        &["error[E2007]", "unknown std module `networking`"],
    );
}

// Perspective 14: a near-miss module name suggests the correct one.
#[test]
fn test_std_unknown_module_suggests_nearest() {
    assert_compile_error_contains(
        r#"
import std::collection::Array;
fn main() { println(1); }
"#,
        &["error[E2007]", "did you mean `std::collections`?"],
    );
}

// Perspective 15: importing the bare `std` root is reserved (E2005).
#[test]
fn test_std_bare_root_is_reserved_e2005() {
    assert_compile_error_contains(
        r#"
import std;
fn main() { println(1); }
"#,
        &["error[E2005]", "reserved namespace"],
    );
}

// Perspective 16: a too-deep std path reports E2007.
#[test]
fn test_std_too_deep_path_reports_e2007() {
    assert_compile_error_contains(
        r#"
import std::collections::Array::extra;
fn main() { println(1); }
"#,
        &["error[E2007]", "not a valid std import path"],
    );
}

// Perspective 17: an unknown module on a two-segment path also reports E2007.
#[test]
fn test_std_unknown_module_two_segments_reports_e2007() {
    assert_compile_error_contains(
        r#"
import std::bogus;
fn main() { println(1); }
"#,
        &["error[E2007]", "unknown std module `bogus`"],
    );
}

// Perspective 18: std imports coexist with local declarations.
#[test]
fn test_std_import_with_local_declarations() {
    let (out, ok) = compile_and_run(
        r#"
import std::io::println;
fn helper(n: i64) -> i64 { return n + 1; }
fn main() { println(helper(40)); }
"#,
    );
    assert!(ok, "std import should not disturb local declarations");
    assert_eq!(out, "41\n");
}

// Perspective 19: dotted std imports are rejected; std paths use `::`.
#[test]
fn test_std_dotted_import_is_rejected() {
    assert_compile_error_contains(
        r#"
import std.io.println;
fn main() {}
"#,
        &["error[E0101]"],
    );
}

// Perspective 20: a duplicate std import is accepted (deduplicated silently).
#[test]
fn test_std_duplicate_import_is_accepted() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_duplicate_std_import_{}.wi", id));
    let bin_path = temp_path(format!("willow_duplicate_std_import_{}", id));
    fs::write(
        &src_path,
        r#"
import std::collections::Array;
import std::collections::Array;
fn main() { println(55); }
"#,
    )
    .unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");
    assert!(
        output.status.success(),
        "duplicate identical std import should be accepted: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning[W2002]"), "stderr: {stderr}");

    let run = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "55\n");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
}

// Perspective 21: prelude items remain available without any std import.
#[test]
fn test_prelude_items_available_without_std_import() {
    let (out, ok) = compile_and_run(
        r#"
fn make() -> Result<i64, String> { return Result::Ok(1); }
fn main() {
    let o: Option<i64> = Option::Some(2);
    println(o.unwrap());
    println(match make() { Result::Ok(v) => v, Result::Err(_) => -1, });
}
"#,
    );
    assert!(ok, "Option/Result/println come from the prelude");
    assert_eq!(out, "2\n1\n");
}

// Perspective 22: E2005, E2006, and E2007 are distinct diagnostic codes.
#[test]
fn test_std_import_diagnostic_codes_are_distinct() {
    assert_compile_error_contains("import std;\nfn main() {}\n", &["error[E2005]"]);
    assert_compile_error_contains(
        "import std::collections::Nope;\nfn main() {}\n",
        &["error[E2006]"],
    );
    assert_compile_error_contains(
        "import std::nope::Thing;\nfn main() {}\n",
        &["error[E2007]"],
    );
}

// ── std::collections type imports (willow-4bv.3, Stage 3) ───────────────────

#[test]
fn test_std_collections_array_import_enables_annotations() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    println(xs.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

#[test]
fn test_std_collections_module_import_enables_array_and_map() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections;

fn main() {
    let xs: Array<i64> = [1];
    let m: Map<String, i64> = Map::new();
    println(xs.len() + m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

#[test]
fn test_array_literal_infers_without_array_import() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let xs = [1, 2, 3];
    println(xs.len());
}
"#,
    );
    assert!(ok, "array literals remain language syntax");
    assert_eq!(out, "3\n");
}

#[test]
fn test_missing_array_import_reports_e2001() {
    assert_compile_error_contains(
        r#"
fn main() {
    let xs: Array<i64> = [1, 2];
    println(xs.len());
}
"#,
        &["error[E2001]", "import std::collections::Array"],
    );
}

#[test]
fn test_missing_array_import_on_parameter_reports_e2001() {
    assert_compile_error_contains(
        r#"
fn total(xs: Array<i64>) -> i64 { return xs.len(); }
fn main() { println(total([1])); }
"#,
        &["error[E2001]", "import std::collections::Array"],
    );
}

#[test]
fn test_missing_array_import_on_main_args_reports_e2001() {
    assert_compile_error_contains(
        r#"
fn main(args: Array<String>) {
    println(args.len());
}
"#,
        &["error[E2001]", "import std::collections::Array"],
    );
}

#[test]
fn test_std_collections_map_import_enables_constructor() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

#[test]
fn test_missing_map_import_reports_e2002() {
    assert_compile_error_contains(
        r#"
fn main() {
    let m: Map<String, i64> = Map::new();
    println(m.len());
}
"#,
        &["error[E2002]", "import std::collections::Map"],
    );
}

#[test]
fn test_missing_map_import_on_static_constructor_reports_e2002() {
    assert_compile_error_contains(
        r#"
fn main() {
    let m = Map::new();
    println(1);
}
"#,
        &["error[E2002]", "import std::collections::Map"],
    );
}

#[test]
fn test_importing_map_does_not_import_array() {
    assert_compile_error_contains(
        r#"
import std::collections::Map;

fn main() {
    let xs: Array<i64> = [1];
    let m: Map<String, i64> = Map::new();
    println(xs.len() + m.len());
}
"#,
        &["error[E2001]", "import std::collections::Array"],
    );
}

#[test]
fn test_importing_array_does_not_import_map() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1];
    let m: Map<String, i64> = Map::new();
    println(xs.len() + m.len());
}
"#,
        &["error[E2002]", "import std::collections::Map"],
    );
}

#[test]
fn test_std_collection_item_import_collision_reports_e2004() {
    assert_compile_error_contains(
        r#"
import std::collections::Array as Thing;
import std::collections::Map as Thing;
fn main() {}
"#,
        &["error[E2004]", "defined multiple times"],
    );
}

#[test]
fn test_std_collection_item_import_vs_local_class_reports_e2003() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;
class Array { pub v: i64; }
fn main() {}
"#,
        &["error[E2003]", "import and a local declaration"],
    );
}

// ── std::collections module imports (willow-4bv.4, Stage 4) ─────────────────

#[test]
fn test_std_collections_module_import_enables_qualified_types_and_constructor() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections;

fn main() {
    let xs: collections::Array<i64> = [1, 2, 3];
    let m: collections::Map<String, i64> = collections::Map::new();
    println(xs.len() + m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn test_std_collections_module_import_enables_qualified_main_args() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std::collections;

fn main(args: collections::Array<String>) {
    println(args.len());
}
"#,
        &["one", "two"],
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

#[test]
fn test_std_collections_module_import_coexists_with_item_import_and_prelude() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections;
import std::collections::Array;

fn make() -> Option<i64> {
    return Option::Some(40);
}

fn main() {
    let xs: collections::Array<i64> = [make().unwrap(), 2];
    let ys: Array<i64> = [1];
    println(xs[0] + xs[1] + ys.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "43\n");
}

#[test]
fn test_std_collections_unknown_qualified_type_reports_e2006() {
    assert_compile_error_contains(
        r#"
import std::collections;

fn main() {
    let xs: collections::Vec<i64> = [];
    println(1);
}
"#,
        &["error[E2006]", "no item `Vec` in `std::collections`"],
    );
}

#[test]
fn test_std_collections_unknown_qualified_constructor_reports_e2006() {
    assert_compile_error_contains(
        r#"
import std::collections;

fn main() {
    collections::Vec::new();
}
"#,
        &["error[E2006]", "no item `Vec` in `std::collections`"],
    );
}

#[test]
fn test_std_collections_module_import_vs_local_decl_reports_e2003() {
    assert_compile_error_contains(
        r#"
import std::collections;
fn collections() -> i64 { return 0; }
fn main() {}
"#,
        &["error[E2003]", "import and a local declaration"],
    );
}

#[test]
fn test_std_collections_module_import_vs_item_alias_reports_e2004() {
    assert_compile_error_contains(
        r#"
import std::collections;
import std::collections::Array as collections;
fn main() {}
"#,
        &["error[E2004]", "defined multiple times"],
    );
}

// ── std::collections alias imports (willow-4bv.5, Stage 5) ──────────────────

#[test]
fn test_std_collection_array_alias_enables_type_positions() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array as Arr;

fn main() {
    let xs: Arr<i64> = [1, 2, 3, 4];
    println(xs.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n");
}

#[test]
fn test_std_collection_map_alias_enables_type_and_constructor() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map as Dict;

fn main() {
    let m: Dict<String, i64> = Dict::new();
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

#[test]
fn test_std_collection_alias_can_shadow_prelude_name() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map as Option;

fn main() {
    let m: Option<String, i64> = Option::new();
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

#[test]
fn test_std_collection_alias_conflict_reports_e2004() {
    assert_compile_error_contains(
        r#"
import std::collections::Array as Bag;
import std::collections::Map as Bag;
fn main() {}
"#,
        &["error[E2004]", "defined multiple times"],
    );
}

#[test]
fn test_std_collection_duplicate_alias_warns() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_duplicate_std_alias_{}.wi", id));
    let bin_path = temp_path(format!("willow_duplicate_std_alias_{}", id));
    fs::write(
        &src_path,
        r#"
import std::collections::Array as Arr;
import std::collections::Array as Arr;
fn main() {
    let xs: Arr<i64> = [9];
    println(xs[0]);
}
"#,
    )
    .unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");
    assert!(
        output.status.success(),
        "duplicate identical alias should compile with a warning: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning[W2002]"), "stderr: {stderr}");

    let run = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "9\n");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
}

#[test]
fn test_std_collection_alias_vs_local_decl_reports_e2003() {
    assert_compile_error_contains(
        r#"
import std::collections::Array as Bag;
class Bag { pub v: i64; }
fn main() {}
"#,
        &["error[E2003]", "import and a local declaration"],
    );
}

// ── fully qualified std paths (willow-4bv.6, Stage 6) ──────────────────────

#[test]
fn test_fully_qualified_std_collection_array_type() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let xs: std::collections::Array<i64> = [3, 4];
    println(xs[0] + xs[1]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_fully_qualified_std_collection_map_type_and_constructor() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let m: std::collections::Map<String, i64> = std::collections::Map::new();
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

#[test]
fn test_fully_qualified_std_option_and_result_paths() {
    let (out, ok) = compile_and_run(
        r#"
fn make() -> std::result::Result<i64, String> {
    return std::result::Result::Ok(41);
}

fn main() {
    let value: std::option::Option<i64> = std::option::Option::Some(1);
    println(value.unwrap() + make().unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_fully_qualified_std_io_println() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    std::io::println(123);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "123\n");
}

#[test]
fn test_fully_qualified_std_unknown_item_reports_e2006() {
    assert_compile_error_contains(
        r#"
fn main() {
    let xs: std::collections::Vec<i64> = [];
    println(1);
}
"#,
        &["error[E2006]", "no item `Vec` in `std::collections`"],
    );
}

// ── Array<T> type (willow-xqm) ─────────────────────────────────────────────
// GC-managed arrays: literals, indexing (read/write), `.len()`, bounds checks.
// Element types cover scalars (i64/bool/f64) and GC references (String/object).

// Perspective 1: i64 literal, .len(), and index reads.
#[test]
fn test_array_i64_literal_len_and_index() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [10, 20, 30];
    println(xs.len());
    println(xs[0]);
    println(xs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n10\n30\n");
}

// Perspective 2: element assignment `xs[i] = v`.
#[test]
fn test_array_index_assignment() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3];
    xs[0] = 100;
    xs[2] = 300;
    println(xs[0]);
    println(xs[1]);
    println(xs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\n2\n300\n");
}

// Perspective 3: iterate with `.len()` and index, accumulating.
#[test]
fn test_array_sum_loop() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [5, 15, 25, 55];
    let mut i = 0;
    let mut sum = 0;
    while i < xs.len() {
        sum = sum + xs[i];
        i = i + 1;
    }
    println(sum);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\n");
}

#[test]
fn test_array_for_loop_sum() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn sum(values: Array<i64>) -> i64 {
    let mut total = 0;
    for value in values {
        total = total + value;
    }
    return total;
}

fn main() {
    let values: Array<i64> = [1, 1, 2, 3, 5, 8];
    println(values[0]);
    println(values.len());
    println(sum(values));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n6\n20\n");
}

#[test]
fn test_array_for_loop_gc_elements_survive_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

fn main() {
    let names: Array<String> = ["a", "b", "c"];
    for name in names {
        let message = name + "!";
        println(message);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!\nb!\nc!\n");
}

#[test]
fn test_for_loop_requires_array_iterable() {
    assert_compile_error_contains(
        r#"
fn main() {
    for value in 123 {
        println(value);
    }
}
"#,
        &[
            "error[E0201]",
            "cannot iterate over `i64`",
            "for-in requires an array",
        ],
    );
}

// Perspective 4: bool elements.
#[test]
fn test_array_bool_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let bs: Array<bool> = [true, false, true];
    println(bs[0]);
    println(bs[1]);
    println(bs.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n3\n");
}

// Perspective 5: f64 elements (exercises the f64<->word bitcast).
#[test]
fn test_array_f64_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let fs: Array<f64> = [1.5, 2.5, 3.0];
    println(fs[0] + fs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4.5\n");
}

// Perspective 6: String (reference) elements round-trip.
#[test]
fn test_array_string_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let names: Array<String> = ["alice", "bob", "carol"];
    println(names.len());
    println(names[0]);
    println(names[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\nalice\ncarol\n");
}

// Perspective 7: an array passed as a function parameter.
#[test]
fn test_array_as_parameter() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn total(xs: Array<i64>) -> i64 {
    let mut i = 0;
    let mut s = 0;
    while i < xs.len() { s = s + xs[i]; i = i + 1; }
    return s;
}
fn main() {
    let xs: Array<i64> = [10, 20, 30];
    println(total(xs));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "60\n");
}

// Perspective 8: an array returned from a function.
#[test]
fn test_array_returned_from_function() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn make() -> Array<i64> {
    return [7, 8, 9];
}
fn main() {
    let xs = make();
    println(xs.len());
    println(xs[1]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n8\n");
}

// Perspective 9: array of class instances, with method calls on elements.
#[test]
fn test_array_of_objects() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

class P {
    pub val: i64;
    pub static fn new(v: i64) -> P { return new P(v); }
    pub fn get(self) -> i64 { return self.val; }
}
fn main() {
    let ps: Array<P> = [P::new(7), P::new(8)];
    println(ps[0].get());
    println(ps[1].get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n8\n");
}

// Perspective 10: empty array with annotation has length 0.
#[test]
fn test_array_empty_annotated() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [];
    println(xs.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// Perspective 11: single-element array.
#[test]
fn test_array_single_element() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [42];
    println(xs.len());
    println(xs[0]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n42\n");
}

// Perspective 12: read back a written reference element.
#[test]
fn test_array_string_write_then_read() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<String> = ["a", "b"];
    xs[0] = "changed";
    println(xs[0]);
    println(xs[1]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "changed\nb\n");
}

// Perspective 13: doubling each element in place.
#[test]
fn test_array_mutate_in_loop() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3, 4];
    let mut i = 0;
    while i < xs.len() {
        xs[i] = xs[i] * 2;
        i = i + 1;
    }
    println(xs[0]);
    println(xs[3]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n8\n");
}

// Perspective 14: `.len()` used directly in an arithmetic expression.
#[test]
fn test_array_len_in_expression() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3, 4, 5];
    println(xs.len() * 2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// Perspective 15: string array survives a GC collection while held live.
#[test]
fn test_array_string_elements_survive_gc() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let names: Array<String> = ["alpha", "beta", "gamma"];
    gc_collect();
    println(names[0]);
    println(names[2]);
}
"#,
    );
    assert!(ok, "array string elements must survive GC");
    assert_eq!(out, "alpha\ngamma\n");
}

// Perspective 16: out-of-bounds read aborts with a clear message.
#[test]
fn test_array_index_out_of_bounds_read_aborts() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    println(xs[5]);
}
"#,
    );
    assert!(!ok, "out-of-bounds read must abort");
    assert!(
        out.contains("out of bounds"),
        "expected an out-of-bounds panic message, got: {out}"
    );
}

// Perspective 17: out-of-bounds write aborts.
#[test]
fn test_array_index_out_of_bounds_write_aborts() {
    let (_out, ok) = compile_and_run_check_exit(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<i64> = [1, 2];
    xs[9] = 0;
}
"#,
    );
    assert!(!ok, "out-of-bounds write must abort");
}

// Perspective 18: a negative index aborts.
#[test]
fn test_array_negative_index_aborts() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    let i = 0 - 1;
    println(xs[i]);
}
"#,
    );
    assert!(!ok, "negative index must abort");
    assert!(out.contains("out of bounds"), "got: {out}");
}

// Perspective 19: indexing with a non-i64 type is a compile error.
#[test]
fn test_array_index_non_i64_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    println(xs[true]);
}
"#,
        &["error[E0201]", "index must be `i64`"],
    );
}

// Perspective 20: indexing a non-array value is a compile error.
#[test]
fn test_array_index_non_array_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: i64 = 5;
    println(x[0]);
}
"#,
        &["error[E0201]", "cannot index a value of type `i64`"],
    );
}

// Perspective 21: mismatched element types in a literal is a compile error.
#[test]
fn test_array_mixed_element_types_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    let xs = [1, true, 3];
    println(xs.len());
}
"#,
        &["error[E0201]", "array elements must have the same type"],
    );
}

// Perspective 22: an unknown array method is a compile error.
#[test]
fn test_array_unknown_method_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    println(xs.first());
}
"#,
        &["error[E0201]", "no method `first` on `Array<i64>`"],
    );
}

// Perspective 23: assigning the wrong element type is a compile error.
#[test]
fn test_array_element_assign_type_mismatch_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3];
    xs[0] = true;
}
"#,
        &["error[E0201]"],
    );
}

// ── For loops over Array<T> (willow-for-loop) ───────────────────────────────
// 20 explicit perspectives: scalar/reference elements, control-flow nesting,
// scoping, diagnostics, evaluation order, GC, and cooperative async.

// Perspective 1: i64 elements can be accumulated.
#[test]
fn test_for_loop_perspective_01_i64_sum() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [2, 4, 6, 8];
    let mut total = 0;
    for x in xs {
        total = total + x;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "20\n");
}

// Perspective 2: an empty array executes the body zero times.
#[test]
fn test_for_loop_perspective_02_empty_array_skips_body() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [];
    let mut count = 7;
    for _ in xs {
        count = count + 100;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

// Perspective 3: a single-element array executes the body exactly once.
#[test]
fn test_for_loop_perspective_03_single_element_runs_once() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [42];
    let mut count = 0;
    for x in xs {
        println(x);
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n1\n");
}

// Perspective 4: bool elements work with ordinary branch logic.
#[test]
fn test_for_loop_perspective_04_bool_elements_drive_if() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let flags: Array<bool> = [true, false, true];
    let mut yes = 0;
    for flag in flags {
        if flag {
            yes = yes + 1;
        }
    }
    println(yes);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

// Perspective 5: f64 elements preserve their bit representation through the loop.
#[test]
fn test_for_loop_perspective_05_f64_accumulation() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let values: Array<f64> = [0.5, 1.25];
    let mut total = 0.0;
    for value in values {
        total = total + value;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1.75\n");
}

// Perspective 6: String elements are usable as GC-managed references.
#[test]
fn test_for_loop_perspective_06_string_concat() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let parts: Array<String> = ["will", "ow"];
    let mut text = "";
    for part in parts {
        text = text + part;
    }
    println(text);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "willow\n");
}

// Perspective 7: class instances can be iterated and called through.
#[test]
fn test_for_loop_perspective_07_object_elements_methods() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

class Score {
    pub value: i64;
    pub static fn new(value: i64) -> Score {
        return new Score(value);
    }
    pub fn get(self) -> i64 {
        return self.value;
    }
}

fn main() {
    let scores: Array<Score> = [Score::new(4), Score::new(5)];
    let mut total = 0;
    for score in scores {
        total = total + score.get();
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

// Perspective 8: nested for loops compose.
#[test]
fn test_for_loop_perspective_08_nested_for_loops() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let left: Array<i64> = [1, 2];
    let right: Array<i64> = [10, 20];
    let mut total = 0;
    for a in left {
        for b in right {
            total = total + a + b;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "66\n");
}

// Perspective 9: for loops can live inside while loops.
#[test]
fn test_for_loop_perspective_09_for_inside_while() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    let mut round = 0;
    let mut total = 0;
    while round < 2 {
        for x in xs {
            total = total + x;
        }
        round = round + 1;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// Perspective 10: while loops can live inside for loop bodies.
#[test]
fn test_for_loop_perspective_10_while_inside_for() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let limits: Array<i64> = [1, 3];
    let mut total = 0;
    for limit in limits {
        let mut i = 0;
        while i < limit {
            total = total + 1;
            i = i + 1;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

// Perspective 11: the loop variable shadows an outer binding only in the loop.
#[test]
fn test_for_loop_perspective_11_loop_var_shadows_outer_and_restores() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let value = 99;
    let xs: Array<i64> = [1, 2];
    for value in xs {
        println(value);
    }
    println(value);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n99\n");
}

// Perspective 12: `_` discards the element but still counts iterations.
#[test]
fn test_for_loop_perspective_12_underscore_discards_element() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [3, 4, 5];
    let mut count = 0;
    for _ in xs {
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

// Perspective 13: the iterable expression is evaluated once before iteration.
#[test]
fn test_for_loop_perspective_13_iterable_expression_evaluated_once() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn make() -> Array<i64> {
    println(70);
    return [1, 2];
}

fn main() {
    for x in make() {
        println(x);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "70\n1\n2\n");
}

// Perspective 14: arrays returned from functions can be iterated directly.
#[test]
fn test_for_loop_perspective_14_iterates_returned_array() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn make() -> Array<i64> {
    return [7, 8, 9];
}

fn main() {
    let mut total = 0;
    for value in make() {
        total = total + value;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "24\n");
}

// Perspective 15: arrays passed as parameters can be iterated in callees.
#[test]
fn test_for_loop_perspective_15_iterates_array_parameter() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn sum(values: Array<i64>) -> i64 {
    let mut total = 0;
    for value in values {
        total = total + value;
    }
    return total;
}

fn main() {
    let values: Array<i64> = [5, 6, 7];
    println(sum(values));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "18\n");
}

// Perspective 16: reference elements stay live across GC stress while iterating.
#[test]
fn test_for_loop_perspective_16_reference_elements_survive_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

fn main() {
    let names: Array<String> = ["a", "b", "c"];
    for name in names {
        gc_collect();
        println(name + "!");
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!\nb!\nc!\n");
}

// Perspective 17: element reads observe array mutations made before later turns.
#[test]
fn test_for_loop_perspective_17_mutating_array_during_iteration() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let mut xs: Array<i64> = [1, 2, 3];
    let mut total = 0;
    for x in xs {
        total = total + x;
        if x == 1 {
            xs[1] = 20;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "24\n");
}

// Perspective 18: loop variables are immutable.
#[test]
fn test_for_loop_perspective_18_loop_var_assignment_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    for value in xs {
        value = 9;
    }
}
"#,
        &[
            "error[E0301]",
            "cannot assign to immutable variable `value`",
        ],
    );
}

// Perspective 19: loop variables do not leak out of the loop body.
#[test]
fn test_for_loop_perspective_19_loop_var_is_scoped_to_body() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2];
    for value in xs {
        println(value);
    }
    println(value);
}
"#,
        &["error[E0350]", "cannot find variable `value`"],
    );
}

// Perspective 20: await works inside for loops in both async main and leaf fns.
#[test]
fn test_for_loop_perspective_20_async_await_in_main_and_leaf() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn sum(values: Array<i64>) -> i64 {
    let mut total = 0;
    for value in values {
        await sleep(1);
        total = total + value;
    }
    return total;
}

async fn main() {
    let visible: Array<i64> = [1, 2];
    for value in visible {
        await sleep(1);
        println(value);
    }

    let hidden: Array<i64> = [3, 4];
    let total = await sum(hidden);
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n7\n");
}

// ── For loops over i64 ranges (willow-range-for) ────────────────────────────
// 22 explicit perspectives: half-open behavior, empty ranges, bound typing,
// evaluation order, scoping, array interop, and cooperative async.

// Perspective 1: `start..end` is half-open.
#[test]
fn test_range_for_loop_perspective_01_half_open_prints_start_to_end_minus_one() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    for n in 1..4 {
        println(n);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// Perspective 2: `1..101` covers 1 through 100.
#[test]
fn test_range_for_loop_perspective_02_one_to_one_hundred_sum() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut total = 0;
    for n in 1..101 {
        total = total + n;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5050\n");
}

// Perspective 3: equal start/end runs zero iterations.
#[test]
fn test_range_for_loop_perspective_03_equal_bounds_are_empty() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut count = 0;
    for _ in 5..5 {
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

// Perspective 4: descending ranges run zero iterations.
#[test]
fn test_range_for_loop_perspective_04_descending_range_is_empty() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut count = 0;
    for _ in 5..2 {
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

// Perspective 5: negative starts work with the same +1 step.
#[test]
fn test_range_for_loop_perspective_05_negative_start() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    for n in -2..2 {
        println(n);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-2\n-1\n0\n1\n");
}

// Perspective 6: variable bounds are accepted.
#[test]
fn test_range_for_loop_perspective_06_variable_bounds() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let start = 2;
    let end = 5;
    let mut total = 0;
    for n in start..end {
        total = total + n;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

// Perspective 7: arithmetic bound expressions are accepted.
#[test]
fn test_range_for_loop_perspective_07_arithmetic_bounds() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut total = 0;
    for n in (1 + 1)..(3 + 2) {
        total = total + n;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

// Perspective 8: bound expressions are evaluated once, left to right.
#[test]
fn test_range_for_loop_perspective_08_bounds_evaluated_once_left_to_right() {
    let (out, ok) = compile_and_run(
        r#"
fn start() -> i64 {
    println(10);
    return 1;
}

fn stop() -> i64 {
    println(20);
    return 3;
}

fn main() {
    for n in start()..stop() {
        println(n);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n1\n2\n");
}

// Perspective 9: nested range loops compose.
#[test]
fn test_range_for_loop_perspective_09_nested_ranges() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut total = 0;
    for a in 1..3 {
        for b in 1..3 {
            total = total + a * 10 + b;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "66\n");
}

// Perspective 10: range loops can live inside while loops.
#[test]
fn test_range_for_loop_perspective_10_range_inside_while() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut round = 0;
    let mut total = 0;
    while round < 2 {
        for n in 1..3 {
            total = total + n;
        }
        round = round + 1;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// Perspective 11: while loops can live inside range loop bodies.
#[test]
fn test_range_for_loop_perspective_11_while_inside_range() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut total = 0;
    for limit in 1..4 {
        let mut i = 0;
        while i < limit {
            total = total + 1;
            i = i + 1;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// Perspective 12: `_` discards range elements but preserves iteration count.
#[test]
fn test_range_for_loop_perspective_12_underscore_discards_range_item() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut count = 0;
    for _ in 3..7 {
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

// Perspective 13: the loop variable shadows only inside the range loop.
#[test]
fn test_range_for_loop_perspective_13_shadowing_restores_outer() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let n = 99;
    for n in 1..3 {
        println(n);
    }
    println(n);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n99\n");
}

// Perspective 14: returning from inside a range loop terminates the function.
#[test]
fn test_range_for_loop_perspective_14_return_inside_range_loop() {
    let (out, ok) = compile_and_run(
        r#"
fn first() -> i64 {
    for n in 2..5 {
        return n;
    }
    return 0;
}

fn main() {
    println(first());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

// Perspective 15: range loops interoperate with Array indexing.
#[test]
fn test_range_for_loop_perspective_15_range_indexes_array() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [5, 6, 7];
    let mut total = 0;
    for i in 0..xs.len() {
        total = total + xs[i];
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "18\n");
}

// Perspective 16: the end bound is snapshotted before the loop starts.
#[test]
fn test_range_for_loop_perspective_16_end_bound_snapshot() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut end = 4;
    let mut total = 0;
    for n in 1..end {
        total = total + n;
        if n == 1 {
            end = 2;
        }
    }
    println(total);
    println(end);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n2\n");
}

// Perspective 17: range loop variables are immutable.
#[test]
fn test_range_for_loop_perspective_17_loop_var_assignment_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    for n in 1..3 {
        n = 9;
    }
}
"#,
        &["error[E0301]", "cannot assign to immutable variable `n`"],
    );
}

// Perspective 18: range loop variables do not leak out of the body.
#[test]
fn test_range_for_loop_perspective_18_loop_var_is_scoped_to_body() {
    assert_compile_error_contains(
        r#"
fn main() {
    for n in 1..3 {
        println(n);
    }
    println(n);
}
"#,
        &["error[E0350]", "cannot find variable `n`"],
    );
}

// Perspective 19: the start bound must be i64.
#[test]
fn test_range_for_loop_perspective_19_start_bound_must_be_i64() {
    assert_compile_error_contains(
        r#"
fn main() {
    for n in true..3 {
        println(n);
    }
}
"#,
        &["error[E0201]", "range bounds must be `i64`"],
    );
}

// Perspective 20: the end bound must be i64.
#[test]
fn test_range_for_loop_perspective_20_end_bound_must_be_i64() {
    assert_compile_error_contains(
        r#"
fn main() {
    for n in 1..3.5 {
        println(n);
    }
}
"#,
        &["error[E0201]", "range bounds must be `i64`"],
    );
}

// Perspective 21: a range outside a `for` loop is now a first-class value.
#[test]
fn test_range_for_loop_perspective_21_range_value_outside_for_is_allowed() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = 1..3;
    println(r.start);
    println(r.end);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n3\n");
}

// Perspective 22: await works inside range loops in async main and leaf fns.
#[test]
fn test_range_for_loop_perspective_22_async_await_in_range_main_and_leaf() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn sum() -> i64 {
    let mut total = 0;
    for n in 1..5 {
        await sleep(1);
        total = total + n;
    }
    return total;
}

async fn main() {
    for n in 1..4 {
        await sleep(1);
        println(n);
    }
    println(await sum());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n10\n");
}

// ── Map<K,V> type (willow-5t6) ─────────────────────────────────────────────
// GC-managed hash map: Map::new(), .insert(k,v), .get(k) -> Option<V>,
// .contains(k) -> bool, .len() -> i64. Keys: String (by content) or i64.

// Perspective 1: insert/get/len with String keys.
#[test]
fn test_map_string_key_insert_get_len() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut ages: Map<String, i64> = Map::new();
    ages.insert("Alice", 30);
    ages.insert("Bob", 25);
    println(ages.len());
    println(match ages.get("Alice") { Option::Some(a) => a, Option::None => -1, });
    println(match ages.get("Bob") { Option::Some(a) => a, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n30\n25\n");
}

// Perspective 2: a missing key returns None.
#[test]
fn test_map_get_missing_returns_none() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    println(match m.get("zzz") { Option::Some(v) => v, Option::None => -99, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "-99\n");
}

// Perspective 3: insert overwrites an existing key.
#[test]
fn test_map_insert_overwrites() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("k", 1);
    m.insert("k", 2);
    println(m.len());
    println(match m.get("k") { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

// Perspective 4: contains reports presence/absence.
#[test]
fn test_map_contains() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("x", 1);
    println(m.contains("x"));
    println(m.contains("y"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

// Perspective 5: i64 keys.
#[test]
fn test_map_i64_keys() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<i64, i64> = Map::new();
    m.insert(10, 100);
    m.insert(20, 200);
    println(match m.get(20) { Option::Some(v) => v, Option::None => -1, });
    println(match m.get(30) { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "200\n-1\n");
}

// Perspective 6: String values (GC references).
#[test]
fn test_map_string_values() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<i64, String> = Map::new();
    m.insert(1, "one");
    m.insert(2, "two");
    println(match m.get(2) { Option::Some(s) => s, Option::None => "none", });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "two\n");
}

// Perspective 7: empty map has length 0.
#[test]
fn test_map_empty_len_zero() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// Perspective 8: a map passed as a function parameter.
#[test]
fn test_map_as_parameter() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn get_or(m: Map<String, i64>, k: String, d: i64) -> i64 {
    return match m.get(k) { Option::Some(v) => v, Option::None => d, };
}
fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("a", 7);
    println(get_or(m, "a", -1));
    println(get_or(m, "b", -1));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n-1\n");
}

// Perspective 9: a map returned from a function.
#[test]
fn test_map_returned_from_function() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn build() -> Map<String, i64> {
    let mut m: Map<String, i64> = Map::new();
    m.insert("v", 99);
    return m;
}
fn main() {
    let m = build();
    println(match m.get("v") { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// Perspective 10: String keys compare by content, not identity.
#[test]
fn test_map_string_keys_by_content() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn key() -> String { return "dynamic"; }
fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("dynamic", 5);
    // A value produced separately but equal in content must hit.
    println(match m.get(key()) { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

// Perspective 11: len grows with distinct keys.
#[test]
fn test_map_len_distinct_keys() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<i64, i64> = Map::new();
    m.insert(1, 1);
    m.insert(2, 2);
    m.insert(3, 3);
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

// Perspective 12: a get result bound to a variable, then matched.
#[test]
fn test_map_get_result_in_variable() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("k", 42);
    let r = m.get("k");
    println(match r { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 13: reference values survive a GC collection while the map lives.
#[test]
fn test_map_string_values_survive_gc() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<i64, String> = Map::new();
    m.insert(1, "alpha");
    m.insert(2, "beta");
    gc_collect();
    println(match m.get(1) { Option::Some(s) => s, Option::None => "gone", });
    println(match m.get(2) { Option::Some(s) => s, Option::None => "gone", });
}
"#,
    );
    assert!(ok, "map string values must survive GC");
    assert_eq!(out, "alpha\nbeta\n");
}

// Perspective 14: a get value used in arithmetic.
#[test]
fn test_map_value_in_arithmetic() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("n", 21);
    let v = match m.get("n") { Option::Some(x) => x, Option::None => 0, };
    println(v * 2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 15: a wrong key type is a compile error.
#[test]
fn test_map_wrong_key_type_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert(1, 2);
}
"#,
        &["error[E0201]", "map key type mismatch"],
    );
}

// Perspective 16: a wrong value type is a compile error.
#[test]
fn test_map_wrong_value_type_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("a", true);
}
"#,
        &["error[E0201]", "map value type mismatch"],
    );
}

// Perspective 17: an unknown method is a compile error.
#[test]
fn test_map_unknown_method_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    m.clear();
}
"#,
        &["error[E0201]", "no method `clear` on `Map<"],
    );
}

// Perspective 18: get with the wrong argument count is a compile error.
#[test]
fn test_map_get_wrong_arity_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    let r = m.get();
}
"#,
        &["error[E0201]", "`Map::get` expects 1 argument"],
    );
}

// Perspective 19: Map::new with arguments is a compile error.
#[test]
fn test_map_new_with_args_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Map;

fn main() {
    let m: Map<String, i64> = Map::new(5);
}
"#,
        &["error[E0201]", "`Map::new` takes no arguments"],
    );
}

// Perspective 20: two independent maps do not share state.
#[test]
fn test_map_independent_instances() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut a: Map<String, i64> = Map::new();
    let mut b: Map<String, i64> = Map::new();
    a.insert("k", 1);
    b.insert("k", 2);
    println(match a.get("k") { Option::Some(v) => v, Option::None => -1, });
    println(match b.get("k") { Option::Some(v) => v, Option::None => -1, });
    println(b.contains("k"));
    println(a.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\ntrue\n1\n");
}

// ── Command-line arguments: fn main(args) and env::args() (willow-b86) ──────

// Perspective 1: main(args) receives the user arguments (excluding program name).
#[test]
fn test_main_args_length_and_elements() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std::collections::Array;

fn main(args: Array<String>) {
    println(args.len());
    let mut i = 0;
    while i < args.len() { println(args[i]); i = i + 1; }
}
"#,
        &["alpha", "beta", "gamma"],
    );
    assert!(ok);
    assert_eq!(out, "3\nalpha\nbeta\ngamma\n");
}

// Perspective 2: main(args) with no arguments sees an empty array.
#[test]
fn test_main_args_empty() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std::collections::Array;

fn main(args: Array<String>) {
    println(args.len());
}
"#,
        &[],
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// Perspective 3: env::args() returns the same arguments.
#[test]
fn test_env_args_length() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() {
    let a = env::args();
    println(a.len());
    println(a[0]);
    println(a[1]);
}
"#,
        &["one", "two"],
    );
    assert!(ok);
    assert_eq!(out, "2\none\ntwo\n");
}

// Perspective 4: env::args() and main(args) agree.
#[test]
fn test_main_args_matches_env_args() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std::collections::Array;

fn main(args: Array<String>) {
    let other = env::args();
    println(args.len() == other.len());
    println(args.len() == env::args_len());
}
"#,
        &["x", "y", "z"],
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// Perspective 5: env::args() in a non-main function.
#[test]
fn test_env_args_in_helper() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn count() -> i64 { return env::args().len(); }
fn main() { println(count()); }
"#,
        &["a", "b"],
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

// Perspective 6: the args array can be passed to another function.
#[test]
fn test_main_args_passed_to_helper() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std::collections::Array;

fn first(xs: Array<String>) -> String {
    if xs.len() > 0 { return xs[0]; }
    return "none";
}
fn main(args: Array<String>) {
    println(first(args));
}
"#,
        &["hello", "world"],
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// Perspective 7: a single argument.
#[test]
fn test_main_args_single() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std::collections::Array;

fn main(args: Array<String>) {
    println(args.len());
    println(args[0]);
}
"#,
        &["solo"],
    );
    assert!(ok);
    assert_eq!(out, "1\nsolo\n");
}

// Perspective 8: env::args() stored in a variable, then indexed.
#[test]
fn test_env_args_in_variable() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() {
    let a = env::args();
    let mut i = 0;
    while i < a.len() { println(a[i]); i = i + 1; }
}
"#,
        &["p", "q"],
    );
    assert!(ok);
    assert_eq!(out, "p\nq\n");
}

// Perspective 9: a plain fn main() still works, ignoring any arguments.
#[test]
fn test_main_no_params_ignores_args() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() { println(42); }
"#,
        &["ignored", "args"],
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 10: args length used in arithmetic.
#[test]
fn test_main_args_len_arithmetic() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std::collections::Array;

fn main(args: Array<String>) {
    println(args.len() * 10);
}
"#,
        &["a", "b", "c", "d"],
    );
    assert!(ok);
    assert_eq!(out, "40\n");
}

// Perspective 11: env::arg(i) and env::args()[i] agree.
#[test]
fn test_env_arg_index_agrees_with_array() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() {
    let a = env::args();
    println(a[1]);
    println(env::arg(1));
}
"#,
        &["zero", "first"],
    );
    assert!(ok);
    assert_eq!(out, "first\nfirst\n");
}

// Perspective 12: an empty env::args() iterates zero times.
#[test]
fn test_env_args_empty_no_iteration() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
fn main() {
    let a = env::args();
    println(a.len());
    let mut i = 0;
    while i < a.len() { println(a[i]); i = i + 1; }
    println(99);
}
"#,
        &[],
    );
    assert!(ok);
    assert_eq!(out, "0\n99\n");
}

// Perspective 13: an invalid main signature is rejected (E1301).
#[test]
fn test_main_invalid_arg_type_is_error() {
    assert_compile_error_contains(
        r#"
fn main(n: i64) {
    println(n);
}
"#,
        &["error[E1301]", "invalid entry point signature"],
    );
}

// Perspective 14: a non-Array<String> single param is rejected.
#[test]
fn test_main_array_of_i64_param_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main(args: Array<i64>) {
    println(args.len());
}
"#,
        &["error[E1301]"],
    );
}

// Perspective 15: the last argument is reachable by index.
#[test]
fn test_main_args_last_element() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std::collections::Array;

fn main(args: Array<String>) {
    println(args[args.len() - 1]);
}
"#,
        &["a", "b", "last"],
    );
    assert!(ok);
    assert_eq!(out, "last\n");
}

// Perspective 16: arguments preserve order and content.
#[test]
fn test_main_args_order_preserved() {
    let (out, ok) = compile_and_run_with_program_args(
        r#"
import std::collections::Array;

fn main(args: Array<String>) {
    println(args[0]);
    println(args[2]);
}
"#,
        &["first", "middle", "third"],
    );
    assert!(ok);
    assert_eq!(out, "first\nthird\n");
}

// ── User module declarations (willow-y0o, spec 4.1 / 8 / 20) ───────────────

// Perspective 1: a module declaration is accepted and the program runs (the
// declaration is otherwise inert for an entry file).
#[test]
fn test_module_decl_entry_compiles() {
    let (out, ok) = compile_and_run(
        r#"
module myapp;
fn main() { println(7); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// Perspective 2: `::`-separated module paths are accepted on the entry file.
#[test]
fn test_module_decl_colon_entry_compiles() {
    let (out, ok) = compile_and_run(
        r#"
module myapp::tools;
fn main() { println(8); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "8\n");
}

// Perspective 3: `module std...` is rejected (reserved namespace).
#[test]
fn test_module_decl_std_rejected() {
    assert_compile_error_contains(
        "module std::io;\nfn main() {}\n",
        &["error[E2010]", "reserved namespace"],
    );
}

// Perspective 4: a module declaration after an item is rejected.
#[test]
fn test_module_decl_after_item_rejected() {
    assert_compile_error_contains(
        "fn helper() {}\nmodule myapp;\nfn main() {}\n",
        &["error[E2008]", "must appear before imports and items"],
    );
}

// Perspective 5: a duplicate module declaration is rejected.
#[test]
fn test_module_decl_duplicate_rejected() {
    assert_compile_error_contains(
        "module a;\nmodule b;\nfn main() {}\n",
        &["error[E2009]", "duplicate module declaration"],
    );
}

// Perspective 6: programs without a module declaration still compile.
#[test]
fn test_no_module_decl_backward_compatible() {
    let (out, ok) = compile_and_run(r#"fn main() { println(1); }"#);
    assert!(ok);
    assert_eq!(out, "1\n");
}

// Perspective 7: an imported file whose declared module matches the import path
// resolves and runs.
#[test]
fn test_imported_module_matching_decl_runs() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math::add(2, 3)); }\n",
            ),
            (
                "math.wi",
                "module math;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

// Perspective 8: an imported file whose declared module does not match the
// import path is an error (E2011).
#[test]
fn test_imported_module_mismatched_decl_errors() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math::add(2, 3)); }\n",
            ),
            (
                "math.wi",
                "module other;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2011]"), "stderr: {stderr}");
    assert!(
        stderr.contains("does not match import path"),
        "stderr: {stderr}"
    );
}

// Perspective 9: an imported file with no module declaration still resolves
// (identity derived from the path — backward compatible).
#[test]
fn test_imported_module_no_decl_backward_compatible() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math::add(4, 5)); }\n",
            ),
            (
                "math.wi",
                "pub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "9\n");
}

// Perspective 10: a nested module path matches a declared nested module.
#[test]
fn test_nested_imported_module_matching_decl_runs() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import foo::bar;\nfn main() { println(bar::val()); }\n",
            ),
            (
                "foo/bar.wi",
                "module foo::bar;\npub fn val() -> i64 { return 77; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "77\n");
}

// Perspective 11: a nested module with a mismatched declaration is an error.
#[test]
fn test_nested_imported_module_mismatch_errors() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import foo::bar;\nfn main() { println(bar::val()); }\n",
            ),
            (
                "foo/bar.wi",
                "module foo::baz;\npub fn val() -> i64 { return 1; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2011]"), "stderr: {stderr}");
}

// ── Single-item imports (willow-om7, spec 10 / 12.2) ───────────────────────

fn math_module() -> (&'static str, &'static str) {
    (
        "math.wi",
        "module math;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\npub fn mul(a: i64, b: i64) -> i64 { return a * b; }\nfn secret() -> i64 { return 99; }\n",
    )
}

// Perspective 1: a directly imported function is callable unqualified.
#[test]
fn test_item_import_function_call() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::add;\nfn main() { println(add(2, 3)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

// Perspective 2: an item import with an alias.
#[test]
fn test_item_import_alias() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::add as plus;\nfn main() { println(plus(10, 20)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "30\n");
}

// Perspective 3: two item imports from the same module.
#[test]
fn test_item_import_two_items() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::add;\nimport math::mul;\nfn main() { println(add(2, 3)); println(mul(2, 3)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "5\n6\n");
}

// Perspective 4: importing a private item is rejected.
#[test]
fn test_item_import_private_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math::secret;\nfn main() { println(secret()); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2006]"), "stderr: {stderr}");
    assert!(stderr.contains("private"), "stderr: {stderr}");
}

// Perspective 5: importing a non-existent item is rejected.
#[test]
fn test_item_import_missing_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            ("main.wi", "import math::nope;\nfn main() { println(1); }\n"),
            math_module(),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2006]"), "stderr: {stderr}");
    assert!(stderr.contains("no item `nope`"), "stderr: {stderr}");
}

// Perspective 6: a module import still works alongside item imports.
#[test]
fn test_item_import_with_module_import() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math;\nimport math::add;\nfn main() { println(add(1, 1)); println(math::mul(2, 4)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "2\n8\n");
}

// Perspective 7: an item import without any plain `import math;` still loads
// the module (no explicit module import required).
#[test]
fn test_item_import_loads_module_implicitly() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::mul;\nfn main() { println(mul(6, 7)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 8: an item-imported function used inside a helper.
#[test]
fn test_item_import_used_in_helper() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::add;\nfn twice(n: i64) -> i64 { return add(n, n); }\nfn main() { println(twice(21)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 9: the item-imported function's result in an expression.
#[test]
fn test_item_import_result_in_expression() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::add;\nfn main() { println(add(3, 4) * 2); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "14\n");
}

// Perspective 10: a nested-module item import (`import foo::bar::baz;`).
#[test]
fn test_item_import_nested_module() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import foo::bar::baz;\nfn main() { println(baz()); }\n",
            ),
            (
                "foo/bar.wi",
                "module foo::bar;\npub fn baz() -> i64 { return 88; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "88\n");
}

// Perspective 11: two item imports + an alias together.
#[test]
fn test_item_import_mixed() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::add;\nimport math::mul as times;\nfn main() { println(add(1, 2)); println(times(3, 4)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "3\n12\n");
}

// ── validate_type rejects unknown/module type annotations (willow-a7j) ─────

// A module name used as a type is rejected.
#[test]
fn test_module_name_as_param_type_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import calc;\nfn f(x: calc) -> i64 { return 0; }\nfn main() { println(1); }\n",
            ),
            (
                "calc.wi",
                "module calc;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E0350]"), "stderr: {stderr}");
    assert!(
        stderr.contains("is a module, not a type"),
        "stderr: {stderr}"
    );
}

// An undefined type name in a parameter is rejected.
#[test]
fn test_unknown_param_type_rejected() {
    assert_compile_error_contains(
        "fn f(x: Bogus) -> i64 { return 0; }\nfn main() {}\n",
        &["error[E0350]", "cannot find type `Bogus`"],
    );
}

// An undefined type name in a return position is rejected.
#[test]
fn test_unknown_return_type_rejected() {
    assert_compile_error_contains(
        "fn f() -> Nope { return 0; }\nfn main() {}\n",
        &["error[E0350]", "cannot find type `Nope`"],
    );
}

// An undefined type name in a let annotation is rejected.
#[test]
fn test_unknown_let_type_rejected() {
    assert_compile_error_contains(
        "fn main() { let x: Whatever = 1; println(1); }\n",
        &["error[E0350]", "cannot find type `Whatever`"],
    );
}

// An undefined type name in a class field is rejected.
#[test]
fn test_unknown_field_type_rejected() {
    assert_compile_error_contains(
        "class C { pub v: Ghost; }\nfn main() {}\n",
        &["error[E0350]", "cannot find type `Ghost`"],
    );
}

// Regression guard: a real class type is still accepted.
#[test]
fn test_known_class_type_accepted() {
    let (out, ok) = compile_and_run(
        r#"
class P {
    pub v: i64;
    pub static fn new(v: i64) -> P { return new P(v); }
    pub fn get(self) -> i64 { return self.v; }
}
fn use_p(p: P) -> i64 { return p.get(); }
fn main() { println(use_p(P::new(42))); }
"#,
    );
    assert!(ok, "a known class type must validate");
    assert_eq!(out, "42\n");
}

// Regression guard: enum types (Option/Result) are still accepted.
#[test]
fn test_known_enum_type_accepted() {
    let (out, ok) = compile_and_run(
        r#"
fn pick(x: Option<i64>) -> Result<i64, String> {
    return match x { Option::Some(v) => Result::Ok(v), Option::None => Result::Err("none"), };
}
fn main() {
    let r = pick(Option::Some(5));
    println(match r { Result::Ok(v) => v, Result::Err(_) => -1, });
}
"#,
    );
    assert!(ok, "Option/Result types must validate");
    assert_eq!(out, "5\n");
}

// Regression guard: a module-qualified class type annotation is accepted.
#[test]
fn test_module_qualified_class_type_accepted() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import geom;\nfn show(p: geom::Point) -> i64 { return p.getx(); }\nfn main() { println(1); }\n",
            ),
            (
                "geom.wi",
                "module geom;\npub class Point {\n    pub x: i64;\n    pub fn getx(self) -> i64 { return self.x; }\n}\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "module-qualified class type must validate");
    assert_eq!(out, "1\n");
}

// Regression guard: a module-qualified class constructor parses, type-checks,
// links to the imported module's class method, and returns the qualified object.
#[test]
fn test_module_qualified_class_constructor_runs() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import geom;\nfn main() { let p = geom::Point::new(10, 32); println(p.sum()); }\n",
            ),
            (
                "geom.wi",
                "module geom;\npub class Point {\n    pub x: i64;\n    pub y: i64;\n    pub static fn new(x: i64, y: i64) -> Point { return new Point(x, y); }\n    pub fn sum(self) -> i64 { return self.x + self.y; }\n}\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "module-qualified class construction should run");
    assert_eq!(out, "42\n");
}

// Imported module bodies can still use their local class name while the entry
// module uses the qualified class name.
#[test]
fn test_module_class_body_can_call_local_constructor() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import geom;\nfn main() { println(geom::origin_sum()); }\n",
            ),
            (
                "geom.wi",
                "module geom;\npub class Point {\n    pub x: i64;\n    pub y: i64;\n    pub static fn new(x: i64, y: i64) -> Point { return new Point(x, y); }\n    pub fn sum(self) -> i64 { return self.x + self.y; }\n}\npub fn origin_sum() -> i64 { let p = Point::new(3, 4); return p.sum(); }\n",
            ),
        ],
        "main.wi",
    );
    assert!(
        ok,
        "module class methods should be available inside the module"
    );
    assert_eq!(out, "7\n");
}

#[test]
fn test_module_alias_class_constructor_uses_canonical_symbol() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import geom as g;\nfn main() { let p = g::Point::new(5, 6); println(p.sum()); }\n",
            ),
            (
                "geom.wi",
                "module geom;\npub class Point {\n    pub x: i64;\n    pub y: i64;\n    pub static fn new(x: i64, y: i64) -> Point { return new Point(x, y); }\n    pub fn sum(self) -> i64 { return self.x + self.y; }\n}\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "aliased module class construction should run");
    assert_eq!(out, "11\n");
}

#[test]
fn test_nested_item_imports_same_leaf_module_do_not_collide() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import left::math::value as left_value;\nimport right::math::value as right_value;\nfn main() { println(left_value()); println(right_value()); }\n",
            ),
            (
                "left/math.wi",
                "module left::math;\npub fn value() -> i64 { return 11; }\n",
            ),
            (
                "right/math.wi",
                "module right::math;\npub fn value() -> i64 { return 22; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(
        ok,
        "canonical module symbol names should avoid leaf-name collisions"
    );
    assert_eq!(out, "11\n22\n");
}

// ── Module aliases + `::` access; `.` reserved for instances (willow-u98) ──

fn aliasable_math() -> (&'static str, &'static str) {
    (
        "math.wi",
        "module math;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\npub fn square(n: i64) -> i64 { return n * n; }\n",
    )
}

// A module imported under an alias is accessed with `alias::item`.
#[test]
fn test_module_alias_qualified_call() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math as m;\nfn main() { println(m::add(2, 3)); println(m::square(4)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "5\n16\n");
}

// The plain `module::item` form still works without an alias.
#[test]
fn test_module_qualified_call_no_alias() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math::add(10, 20)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "30\n");
}

// Accessing a module item with `.` is an error that points at `::`.
#[test]
fn test_module_dot_access_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math.add(1, 2)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E0350]"), "stderr: {stderr}");
    assert!(stderr.contains("is a module; use `::`"), "stderr: {stderr}");
}

// `.` on an aliased module is likewise rejected.
#[test]
fn test_module_alias_dot_access_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math as m;\nfn main() { println(m.add(1, 2)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E0350]"), "stderr: {stderr}");
}

// After aliasing, the original module name is not in scope.
#[test]
fn test_module_alias_hides_original_name() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math as m;\nfn main() { println(math::add(1, 2)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    // `math` is not a known module under the alias import.
    assert!(
        !stderr.is_empty(),
        "expected an error using the original name"
    );
}

// Instance `.` method/field access is unaffected by the module-dot rule.
#[test]
fn test_instance_dot_access_still_works() {
    let (out, ok) = compile_and_run(
        r#"
class P {
    pub v: i64;
    pub static fn new(v: i64) -> P { return new P(v); }
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let p = P::new(9);
    println(p.get());
    println(p.v);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n9\n");
}

// ── Import visibility + collision diagnostics (willow-pwa, spec 11/13) ─────

fn s5_modules() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "a.wi",
            "module a;\npub fn f() -> i64 { return 1; }\npub fn dup() -> i64 { return 10; }\nfn hidden() -> i64 { return 9; }\n",
        ),
        (
            "b.wi",
            "module b;\npub fn g() -> i64 { return 2; }\npub fn dup() -> i64 { return 20; }\n",
        ),
    ]
}

fn s5_project(main: &str) -> Vec<(&'static str, &'static str)> {
    let mut v = s5_modules();
    v.insert(0, ("main.wi", Box::leak(main.to_string().into_boxed_str())));
    v
}

// Importing a private (non-pub) item is rejected.
#[test]
fn test_import_private_item_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a::hidden;\nfn main() { println(hidden()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2006]"), "stderr: {stderr}");
    assert!(stderr.contains("private"), "stderr: {stderr}");
}

// Two item imports binding the same local name collide.
#[test]
fn test_duplicate_item_import_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a::dup;\nimport b::dup;\nfn main() { println(dup()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2004]"), "stderr: {stderr}");
    assert!(
        stderr.contains("defined multiple times"),
        "stderr: {stderr}"
    );
}

// An item import colliding with a local function is rejected.
#[test]
fn test_item_import_vs_local_fn_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a::f;\nfn f() -> i64 { return 0; }\nfn main() { println(f()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2003]"), "stderr: {stderr}");
    assert!(
        stderr.contains("import and a local declaration"),
        "stderr: {stderr}"
    );
}

// An item import colliding with a local class is rejected.
#[test]
fn test_item_import_vs_local_class_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a::f;\nclass f { pub v: i64; }\nfn main() {}\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2003]"), "stderr: {stderr}");
}

// Two module imports aliased to the same name collide.
#[test]
fn test_module_alias_collision_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a as x;\nimport b as x;\nfn main() { println(x::f()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2004]"), "stderr: {stderr}");
}

// A module access-name colliding with a local declaration is rejected.
#[test]
fn test_module_name_vs_local_fn_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a;\nfn a() -> i64 { return 0; }\nfn main() {}\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2003]"), "stderr: {stderr}");
}

// Distinct imports and declarations compile and run.
#[test]
fn test_distinct_imports_and_decls_ok() {
    let (out, ok) = compile_temp_project_and_run(
        &s5_project(
            "import a::f;\nimport b::g;\nfn helper() -> i64 { return 100; }\nfn main() { println(f() + g() + helper()); }\n",
        ),
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "103\n");
}

// An alias disambiguates two otherwise-colliding item imports.
#[test]
fn test_alias_disambiguates_duplicate_item() {
    let (out, ok) = compile_temp_project_and_run(
        &s5_project(
            "import a::dup;\nimport b::dup as bdup;\nfn main() { println(dup() + bdup()); }\n",
        ),
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "30\n");
}

// ── Array dynamic growth: push/pop (willow-5a4) ────────────────────────────

// push grows an empty array; len and indexing reflect the appended elements.
#[test]
fn test_array_push_grows_empty() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [];
    let mut i = 0;
    while i < 6 { xs.push(i * 10); i = i + 1; }
    println(xs.len());
    println(xs[0]);
    println(xs[5]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n0\n50\n");
}

// pop returns the last element and shrinks the array.
#[test]
fn test_array_pop_returns_last() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    println(xs.pop());
    println(xs.pop());
    println(xs.len());
    println(xs[0]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n2\n1\n1\n");
}

// push works on a non-empty literal (grows past initial capacity).
#[test]
fn test_array_push_onto_literal() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [10, 20];
    xs.push(30);
    xs.push(40);
    println(xs.len());
    println(xs[2]);
    println(xs[3]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n30\n40\n");
}

// push/pop of reference (String) elements round-trips.
#[test]
fn test_array_push_pop_string_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let names: Array<String> = [];
    names.push("alice");
    names.push("bob");
    println(names.len());
    println(names.pop());
    println(names[0]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\nbob\nalice\n");
}

// f64 elements survive the push word/bit-cast.
#[test]
fn test_array_push_f64_elements() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let fs: Array<f64> = [];
    fs.push(1.5);
    fs.push(2.5);
    println(fs[0] + fs[1]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n");
}

// pop then push reuses the array correctly.
#[test]
fn test_array_pop_then_push() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1, 2, 3];
    let last = xs.pop();
    xs.push(last * 10);
    println(xs.len());
    println(xs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n30\n");
}

// String elements pushed across several growths survive a GC collection.
#[test]
fn test_array_pushed_strings_survive_gc() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<String> = [];
    let mut i = 0;
    while i < 20 { xs.push("item"); i = i + 1; }
    gc_collect();
    println(xs.len());
    println(xs[0]);
    println(xs[19]);
}
"#,
    );
    assert!(ok, "pushed string elements must survive GC across growth");
    assert_eq!(out, "20\nitem\nitem\n");
}

// Popping an empty array aborts.
#[test]
fn test_array_pop_empty_aborts() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [];
    println(xs.pop());
}
"#,
    );
    assert!(!ok, "pop on empty must abort");
    assert!(out.contains("empty array"), "got: {out}");
}

// Pushing the wrong element type is a compile error.
#[test]
fn test_array_push_wrong_type_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1];
    xs.push(true);
}
"#,
        &["error[E0201]", "cannot push"],
    );
}

// push with the wrong arity is a compile error.
#[test]
fn test_array_push_wrong_arity_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [1];
    xs.push();
}
"#,
        &["error[E0201]", "`Array::push` expects 1 argument"],
    );
}

// ── Arrays are GC roots (regression for is_gc_managed(Array), willow-a7j-adjacent) ──

// An array local must survive gc_collect AND subsequent allocations that would
// reuse its freed memory if it were not rooted. (The plain survive-gc tests can
// pass by reading not-yet-reused freed memory; this forces reuse.)
#[test]
fn test_array_local_rooted_across_gc_and_reuse() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<String> = ["alpha", "beta", "gamma"];
    gc_collect();
    let ys: Array<i64> = [];
    let mut i = 0;
    while i < 300 { ys.push(i); i = i + 1; }
    println(xs[0]);
    println(xs[2]);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "alpha\ngamma\n");
}

// A class field of array type must be traced (so the held array survives GC).
#[test]
fn test_array_class_field_traced() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

class Bag {
    pub items: Array<String>;
    pub static fn new(items: Array<String>) -> Bag { return new Bag(items); }
    pub fn first(self) -> String { return self.items[0]; }
}
fn main() {
    let b = Bag::new(["x", "y"]);
    gc_collect();
    let junk: Array<i64> = [];
    let mut i = 0;
    while i < 200 { junk.push(i); i = i + 1; }
    println(b.first());
}
"#,
    );
    assert!(ok, "array-typed class field must be traced as a GC ref");
    assert_eq!(out, "x\n");
}

// ── `void` is a writable type (foundation for willow-exg) ──────────────────

// An explicit `-> void` return annotation is accepted and behaves like an
// omitted return type.
#[test]
fn test_explicit_void_return_type() {
    let (out, ok) = compile_and_run(
        r#"
fn greet() -> void { println(1); }
fn main() { greet(); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

// `void` is usable as a generic type argument in an annotation (e.g. a future
// Result<void, E>); the annotation parses and type-checks.
#[test]
fn test_void_as_generic_type_arg_annotation() {
    let (out, ok) = compile_and_run(
        r#"
fn use_r(r: Result<void, String>) -> i64 { return 0; }
fn main() { println(2); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

// ----------------------------------------------------------------------------
// Range<i64> as a first-class value (willow: range-value feature).
// 20 perspectives on materializing, reading, passing, returning, and iterating
// a `Range<i64>` held as a value rather than only as an inline `for` iterable.
// ----------------------------------------------------------------------------

// P1: `let r = a..b` materializes a value; P2: `.start`; P3: `.end`.
#[test]
fn range_value_p01_let_and_fields() {
    let (out, ok) =
        compile_and_run("fn main() { let r = 4..9; println(r.start); println(r.end); }");
    assert!(ok, "{out}");
    assert_eq!(out, "4\n9\n");
}

// P4: a function may return `Range<i64>`; P5: and accept it as a parameter.
#[test]
fn range_value_p02_return_and_param() {
    let (out, ok) = compile_and_run(
        r#"
fn make() -> Range<i64> { return 3..8; }
fn width(r: Range<i64>) -> i64 { return r.end - r.start; }
fn main() {
    let r = make();
    println(r.start);
    println(width(r));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n5\n");
}

// P6: `for x in <range variable>` iterates the stored bounds.
#[test]
fn range_value_p03_for_over_variable() {
    let (out, ok) = compile_and_run("fn main() { let r = 1..4; for x in r { println(x); } }");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// P7: bounds may be arbitrary i64 expressions (not just literals).
#[test]
fn range_value_p04_expression_bounds() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let a = 2;
    let b = a + 3;
    let r = (a - 1)..(b * 2);
    println(r.start);
    println(r.end);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n10\n");
}

// P8: an empty range (start == end) yields no iterations; fields still correct.
#[test]
fn range_value_p05_empty_range() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = 5..5;
    let mut n = 0;
    for _ in r { n = n + 1; }
    println(n);
    println(r.end - r.start);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n0\n");
}

// P9: a reversed range (start > end) yields no iterations.
#[test]
fn range_value_p06_reversed_range_no_iterations() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = 7..3;
    let mut n = 0;
    for _ in r { n = n + 1; }
    println(n);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

// P10: negative bounds; P11: summing a range variable.
#[test]
fn range_value_p07_negative_bounds_sum() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = -2..3;
    let mut total = 0;
    for x in r { total = total + x; }
    println(total);
    println(r.start);
}
"#,
    );
    assert!(ok, "{out}");
    // -2 + -1 + 0 + 1 + 2 = 0
    assert_eq!(out, "0\n-2\n");
}

// P12: multiple range values coexist independently.
#[test]
fn range_value_p08_multiple_ranges() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let a = 0..2;
    let b = 10..13;
    println(a.end);
    println(b.start);
    for x in a { println(x); }
    for y in b { println(y); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n10\n0\n1\n10\n11\n12\n");
}

// P13: range value survives GC stress (heap object is rooted).
#[test]
fn range_value_p09_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let r = 2..6;
    let s = "keepalive";
    let mut total = 0;
    for x in r { total = total + x; }
    println(s);
    println(total);
    println(r.start);
}
"#,
    );
    assert!(ok, "{out}");
    // 2+3+4+5 = 14
    assert_eq!(out, "keepalive\n14\n2\n");
}

// P14: iterate directly over a range returned by a call.
#[test]
fn range_value_p10_for_over_call_result() {
    let (out, ok) = compile_and_run(
        r#"
fn upto(n: i64) -> Range<i64> { return 0..n; }
fn main() { for x in upto(3) { println(x); } }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n");
}

// P15: a `mut` range may be reassigned to another range value.
#[test]
fn range_value_p11_mut_reassign() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut r = 0..1;
    r = 5..8;
    println(r.start);
    println(r.end);
    for x in r { println(x); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n8\n5\n6\n7\n");
}

// P16: range fields participate in conditions/arithmetic.
#[test]
fn range_value_p12_field_in_condition() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = 4..10;
    if r.end > r.start {
        println(r.end - r.start);
    } else {
        println(0);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// P17: a range literal `for` loop still works (no regression).
#[test]
fn range_value_p13_literal_for_loop_regression() {
    let (out, ok) =
        compile_and_run("fn main() { let mut t = 0; for x in 1..5 { t = t + x; } println(t); }");
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}

// P18: range value lives in an async frame across an await; fields read after.
#[test]
fn range_value_p14_async_frame_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn compute() -> i64 {
    let r = 3..7;
    await sleep(1);
    return r.start + r.end;
}
async fn main() {
    let v = await compute();
    println(v);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}

// P19: cooperative `for` over a range variable with an await in the body.
#[test]
fn range_value_p15_cooperative_for_over_variable() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run() -> i64 {
    let r = 1..4;
    let mut total = 0;
    for x in r {
        await sleep(1);
        total = total + x;
    }
    return total;
}
async fn main() {
    let t = await run();
    println(t);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// P20: range bounds must be `i64` (float bound is a diagnostic).
#[test]
fn range_value_p16_non_i64_bound_is_error() {
    assert_compile_error_contains(
        "fn main() { let r = 0.0..5; println(r.start); }",
        &["error[E0201]", "range bounds must be `i64`"],
    );
}

// P21: accessing an unknown range field is a diagnostic.
#[test]
fn range_value_p17_unknown_field_is_error() {
    assert_compile_error_contains(
        "fn main() { let r = 0..5; println(r.middle); }",
        &["error[E0201]", "has no field `middle`"],
    );
}

// ----------------------------------------------------------------------------
// Cooperative spawn/join (willow: spawn migrated off OS threads onto the
// single-threaded cooperative scheduler). `spawn` queues a lightweight task;
// `join()` (and channel `recv()`) drive the scheduler until it completes.
// ----------------------------------------------------------------------------

// Spawn/join returns each task's result, regardless of join order.
#[test]
fn coop_spawn_01_join_order_independent() {
    let (out, ok) = compile_and_run(
        r#"
async fn sq(x: i64) -> i64 { return x * x; }
fn main() {
    let a = sq(2);
    let b = sq(3);
    let c = sq(4);
    println(c.join());
    println(a.join());
    println(b.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "16\n4\n9\n");
}

// Many lightweight tasks: spawning a lot is cheap (no OS thread per spawn).
#[test]
fn coop_spawn_02_many_tasks() {
    let (out, ok) = compile_and_run(
        r#"
async fn id(x: i64) -> i64 { return x; }
fn main() {
    let a = id(1);
    let b = id(2);
    let c = id(3);
    let d = id(4);
    let e = id(5);
    let f = id(6);
    let g = id(7);
    let h = id(8);
    let total = a.join() + b.join() + c.join() + d.join()
        + e.join() + f.join() + g.join() + h.join();
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "36\n");
}

// A spawned producer is driven by the consumer's `recv()` (cooperative, no
// cross-thread deadlock).
#[test]
fn coop_spawn_03_channel_producer_consumer() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) {
    ch.send(1);
    ch.send(2);
    ch.send(3);
    ch.close();
}
fn main() {
    let ch = Channel<i64>::new();
    let h = producer(ch);
    println(ch.recv());
    println(ch.recv());
    println(ch.recv());
    h.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// Spawn with GC-managed args (object + string), result read via join, under
// GC stress: the frame roots the args and traces the result slot.
#[test]
fn coop_spawn_04_gc_args_and_result() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Box { v: i64; pub static fn new(v: i64) -> Box { return new Box(v); } pub fn get(self) -> i64 { return self.v; } }
async fn label(b: Box, name: String) -> String {
    return name;
}
async fn value(b: Box) -> i64 {
    return b.get();
}
fn main() {
    let b = Box::new(7);
    let h1 = label(b, "tag");
    let h2 = value(b);
    println(h1.join());
    println(h2.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "tag\n7\n");
}

// A non-i64 (bool) spawn result round-trips through the frame result slot.
#[test]
fn coop_spawn_05_bool_result() {
    let (out, ok) = compile_and_run(
        r#"
async fn positive(x: i64) -> bool { return x > 0; }
fn main() {
    let a = positive(5);
    let b = positive(-5);
    println(a.join());
    println(b.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\nfalse\n");
}

// Slice 5: awaits inside if/else and while are lowered by the CFG-based
// cooperative state machine (willow-lpn.5.3 / willow-8fh3 regression).
#[test]
fn coop_async_09_await_in_if_else_both_return() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn pick(flag: bool) -> i64 {
    if flag {
        await sleep(1);
        return 10;
    } else {
        await sleep(1);
        await sleep(1);
        return 20;
    }
}
async fn main() {
    println(await pick(true));
    println(await pick(false));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n");
}

#[test]
fn coop_async_10_await_in_if_else_join() {
    // Both arms fall through to a shared join, carrying a frame-backed local.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run(flag: bool) -> i64 {
    let mut r = 0;
    if flag {
        await sleep(1);
        r = 10;
    } else {
        await sleep(1);
        r = 20;
    }
    await sleep(1);
    return r + 1;
}
async fn main() {
    println(await run(true));
    println(await run(false));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "11\n21\n");
}

#[test]
fn coop_async_11_await_in_while() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn sum(n: i64) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < n {
        await sleep(1);
        total = total + i;
        i = i + 1;
    }
    return total;
}
async fn main() { println(await sum(4)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn coop_async_12_await_in_if_inside_while() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run(n: i64) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < n {
        if i == 1 {
            await sleep(1);
            total = total + 100;
        } else {
            await sleep(1);
            total = total + i;
        }
        i = i + 1;
    }
    return total;
}
async fn main() { println(await run(3)); }
"#,
    );
    assert!(ok, "{out}");
    // i=0: +0, i=1: +100, i=2: +2 => 102
    assert_eq!(out, "102\n");
}

#[test]
fn coop_async_13_gc_string_built_across_while_awaits() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn build(n: i64) -> String {
    let mut s = "";
    let mut i = 0;
    while i < n {
        await sleep(1);
        s = s + "x";
        i = i + 1;
    }
    return s;
}
async fn main() { println(await build(3)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "xxx\n");
}

// ----------------------------------------------------------------------------
// Async-GC stress suite (willow-lpn.5.5): GC-safety of the cooperative state
// machine — collection before await, after await, GC objects/strings carried
// across awaits, and JoinHandle keeping a GC result alive. All under
// WILLOW_GC_STRESS=alloc (collect at every allocation) plus explicit gc_collect.
// ----------------------------------------------------------------------------

// 16.1: collection BEFORE an await — a frame-backed GC local survives.
#[test]
fn coop_gc_01_collect_before_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run() -> String {
    let s = "kept";
    gc_collect();
    await sleep(1);
    return s;
}
async fn main() { println(await run()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "kept\n");
}

// 16.2: collection AFTER an await — the local declared before the await survives.
#[test]
fn coop_gc_02_collect_after_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run() -> String {
    let s = "kept";
    await sleep(1);
    gc_collect();
    return s;
}
async fn main() { println(await run()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "kept\n");
}

// GC object (class instance) carried across an await with collections on both
// sides; field access after the await reads the live object.
#[test]
fn coop_gc_03_object_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Box { v: i64; pub static fn new(v: i64) -> Box { return new Box(v); } pub fn get(self) -> i64 { return self.v; } }
async fn run() -> i64 {
    let b = Box::new(42);
    gc_collect();
    await sleep(1);
    gc_collect();
    return b.get();
}
async fn main() { println(await run()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

// 16.9: a JoinHandle keeps the spawned task's GC result alive across a collection
// performed before join().
#[test]
fn coop_gc_04_joinhandle_keeps_result_alive() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn tag(n: i64) -> String { return "tag"; }
async fn main() {
    let h = tag(7);
    gc_collect();
    gc_collect();
    println(h.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "tag\n");
}

// Combined stress: many awaits in a loop, each iteration allocates (string
// concat) and collects, while the accumulator local survives every collection.
#[test]
fn coop_gc_05_combined_stress_loop() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn build(n: i64) -> String {
    let mut s = "";
    let mut i = 0;
    while i < n {
        await sleep(1);
        s = s + "ab";
        gc_collect();
        i = i + 1;
    }
    return s;
}
async fn main() { println(await build(4)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "abababab\n");
}

// spawn of a cooperative-leaf ASYNC fn: join() must return the async function's
// REAL result, not the constructor's frame pointer (willow-lpn.5.4 fix).
#[test]
fn coop_spawn_06_spawn_async_leaf_sync_main() {
    let (out, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 {
    await sleep(1);
    return x + 1;
}
fn main() {
    let h = work(41);
    println(h.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn coop_spawn_07_spawn_async_leaf_multiple_gc() {
    // Multiple spawned async leaves (i64 + String results) joined; under GC
    // stress to exercise frame/result tracing.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn add(a: i64, b: i64) -> i64 {
    await sleep(1);
    return a + b;
}
async fn tag(name: String) -> String {
    await sleep(1);
    return "hi " + name;
}
async fn main() {
    let h1 = add(40, 2);
    let h2 = add(10, 5);
    let h3 = tag("willow");
    println(h1.join());
    println(h2.join());
    println(h3.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n15\nhi willow\n");
}

#[test]
fn coop_spawn_08_spawn_async_leaf_runs_to_completion() {
    // The spawned leaf actually runs (side effects observed) and join returns
    // its real result; spawn does not block (the println(2) happens first).
    let (out, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 {
    println(100);
    await sleep(1);
    println(200);
    return x;
}
fn main() {
    println(1);
    let h = work(42);
    println(2);
    let r = h.join();
    println(3);
    println(r);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n100\n200\n3\n42\n");
}

// Cooperative concurrency: spawned async-leaf tasks suspend independently at
// their awaits and the single-threaded scheduler interleaves them — observably
// distinct from sequential execution (willow-lpn.5.4).
#[test]
fn coop_concurrent_01_two_workers_interleave() {
    let (out, ok) = compile_and_run(
        r#"
async fn worker(id: i64) -> i64 {
    println(id);
    await sleep(1);
    println(id + 100);
    return id;
}
fn main() {
    let a = worker(1);
    let b = worker(2);
    println(a.join() + b.join());
}
"#,
    );
    assert!(ok, "{out}");
    // Interleaved: both print id, both sleep, both resume, then the sum.
    assert_eq!(out, "1\n2\n101\n102\n3\n");
}

#[test]
fn coop_yield_01_main_resumes_without_timer() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    println(1);
    await yield();
    println(2);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn coop_yield_02_spawned_workers_interleave() {
    let (out, ok) = compile_and_run(
        r#"
async fn worker(id: i64) -> i64 {
    println(id);
    await yield();
    println(id + 10);
    return id;
}
fn main() {
    let a = worker(1);
    let b = worker(2);
    println(a.join() + b.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n11\n12\n3\n");
}

#[test]
fn coop_yield_03_gc_string_survives_yield() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
async fn keep(text: String) -> String {
    let held = text + "!";
    gc_collect();
    await yield();
    gc_collect();
    return held + "?";
}
fn main() {
    let task = keep("yield");
    gc_collect();
    println(task.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "yield!?\n");
}

#[test]
fn coop_concurrent_02_three_workers_interleave_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn worker(id: i64) -> i64 {
    println(id);
    await sleep(1);
    println(id * 10);
    return id;
}
async fn main() {
    let a = worker(1);
    let b = worker(2);
    let c = worker(3);
    println(a.join() + b.join() + c.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n10\n20\n30\n6\n");
}

#[test]
fn coop_concurrent_03_spawn_then_await_in_main() {
    // An eager main spawns a background worker, then `await f()` block-drives the
    // scheduler — the background worker interleaves during that await.
    let (out, ok) = compile_and_run(
        r#"
async fn bg() -> i64 {
    println(7);
    await sleep(1);
    println(8);
    return 0;
}
async fn f() -> i64 {
    await sleep(1);
    return 42;
}
async fn main() {
    let h = bg();
    let x = await f();
    println(x);
    h.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n8\n42\n");
}

// ----------------------------------------------------------------------------
// Cooperative awaiter-suspend model (willow-lpn.5.3.1): a `let x = await f()` /
// `await f()` of a cooperative leaf SUSPENDS the awaiter via willow_sched_await
// (dependency-wake) rather than block-on, so a fn that MIXES call-awaits and
// sleep-awaits is itself a cooperative task. The callee frame is held in a
// GC-traced awaiter slot across suspension.
// ----------------------------------------------------------------------------

// A spawned worker that mixes a call-await and a sleep-await joins its REAL
// result (previously returned a frame ptr / garbage).
#[test]
fn coop_await_01_mixed_call_and_sleep_await_spawned() {
    let (out, ok) = compile_and_run(
        r#"
async fn helper(x: i64) -> i64 {
    await sleep(1);
    return x * 10;
}
async fn worker(id: i64) -> i64 {
    println(id);
    let h = await helper(id);
    await sleep(1);
    println(h);
    return h + id;
}
fn main() {
    let a = worker(1);
    println(a.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n10\n11\n");
}

// Two mixed-await workers interleave (true concurrency WITH composition), GC.
#[test]
fn coop_await_02_mixed_workers_interleave_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn helper(x: i64) -> i64 {
    await sleep(1);
    return x * 10;
}
async fn worker(id: i64) -> i64 {
    println(id);
    let h = await helper(id);
    println(h);
    return h + id;
}
async fn main() {
    let a = worker(1);
    let b = worker(2);
    println(a.join() + b.join());
}
"#,
    );
    assert!(ok, "{out}");
    // Both print id (interleave at the call-await), both resume + print h, then sum.
    // Timer wake order can resume the two helpers in either order.
    assert!(
        matches!(out.as_str(), "1\n2\n10\n20\n33\n" | "1\n2\n20\n10\n33\n"),
        "{out}"
    );
}

// Sequential call-awaits chaining a GC (String) result through the awaiter
// frame, under GC stress.
#[test]
fn coop_await_03_sequential_string_call_awaits_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn step(s: String) -> String {
    await sleep(1);
    return s + "!";
}
async fn main() {
    let a = await step("a");
    let b = await step(a);
    let c = await step(b);
    println(c);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!!!\n");
}

// A call-await result drives later control flow + arithmetic in the awaiter.
#[test]
fn coop_await_04_call_await_result_in_control_flow() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn compute(x: i64) -> i64 {
    await sleep(1);
    return x + 5;
}
async fn main() {
    let v = await compute(10);
    if v > 12 {
        await sleep(1);
        println(v * 2);
    } else {
        println(0);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "30\n");
}

// A discarded call-await (`await f();` with no binding) still suspends + runs.
#[test]
fn coop_await_05_discarded_call_await() {
    let (out, ok) = compile_and_run(
        r#"
async fn tick(n: i64) -> i64 {
    await sleep(1);
    println(n);
    return n;
}
async fn main() {
    await tick(1);
    await tick(2);
    println(3);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// A call-await can assign into an existing frame-backed local and then keep
// running after another suspension.
#[test]
fn coop_await_06_assignment_call_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn next(n: i64) -> i64 {
    await sleep(1);
    return n + 1;
}
async fn worker() -> i64 {
    let mut total = 0;
    total = await next(10);
    await sleep(1);
    return total + 5;
}
async fn main() {
    println(await worker());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "16\n");
}

// A cooperative leaf can return the result of a call-await directly.
#[test]
fn coop_await_07_return_call_await_chain_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn mark(s: String) -> String {
    await sleep(1);
    return s + "!";
}
async fn wrap(s: String) -> String {
    return await mark(s);
}
async fn main() {
    println(await wrap("ok"));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "ok!\n");
}

// A call-await can assign a GC result into an object field, then survive another
// suspension before the field is read.
#[test]
fn coop_await_08_field_assignment_call_await_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Holder {
    pub text: String;
}
async fn mark(s: String) -> String {
    await sleep(1);
    return s + "!";
}
async fn main() {
    let h = new Holder("seed");
    h.text = await mark("field");
    await sleep(1);
    println(h.text);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "field!\n");
}

// A call-await can assign a GC result into an array element through the
// cooperative awaiter path.
#[test]
fn coop_await_09_index_assignment_call_await_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn mark(s: String) -> String {
    await sleep(1);
    return s + "!";
}
async fn main() {
    let mut xs: Array<String> = ["seed"];
    xs[0] = await mark("index");
    await sleep(1);
    println(xs[0]);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "index!\n");
}

// ----------------------------------------------------------------------------
// Cooperative channels (willow-dsw): channel `recv` is a cooperative suspend
// point — an empty `recv` parks the consuming task as a channel waiter, and
// `send`/`close` wake it. This makes a recv-consumer a real cooperative task
// (spawn/join works) and lets producer/consumer tasks interleave correctly.
// ----------------------------------------------------------------------------

// Spawned producer + spawned consumer task; consumer's join returns its result.
#[test]
fn coop_chan_01_task_producer_consumer() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    let mut i = 1;
    while i <= 3 {
        await sleep(1);
        ch.send(i * 10);
        i = i + 1;
    }
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut v = ch.recv();
    while v != 0 {
        println(v);
        total = total + v;
        v = ch.recv();
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join());
    p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n30\n60\n");
}

// Same, under GC stress (the channel value queue + frame slots survive).
#[test]
fn coop_chan_02_task_producer_consumer_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    let mut i = 1;
    while i <= 3 {
        await sleep(1);
        ch.send(i);
        i = i + 1;
    }
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut v = ch.recv();
    while v != 0 {
        total = total + v;
        v = ch.recv();
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join());
    p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// A consumer that recvs in a `let` binding (first value) then loops with assign.
#[test]
fn coop_chan_03_recv_let_and_assign() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(7);
    ch.send(8);
    ch.close();
    return 0;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let a = await consume_first(ch);
    println(a);
    p.join();
}
async fn consume_first(ch: Channel<i64>) -> i64 {
    let x = ch.recv();
    let y = ch.recv();
    return x + y;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "15\n");
}

// Channel<GC-type> buffers are GC-traced: computed (non-literal) string values
// queued in a channel survive collection until received (willow-dsw GC tracing).
#[test]
fn coop_chan_04_gc_element_channel_traced() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<String>, tag: String) -> i64 {
    await sleep(1);
    ch.send(tag + "-1");
    ch.send(tag + "-2");
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<String>) -> i64 {
    let a = ch.recv();
    let b = ch.recv();
    println(a);
    println(b);
    return 0;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch, "x");
    let c = consumer(ch);
    c.join();
    p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "x-1\nx-2\n");
}

#[test]
fn coop_chan_05_parked_receiver_frame_survives_gc_before_send() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<String>) -> i64 {
    await sleep(1);
    gc_collect();
    ch.send("done");
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<String>, prefix: String) -> String {
    let kept = prefix + "-keep";
    let v = ch.recv();
    gc_collect();
    return kept + ":" + v;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch);
    let c = consumer(ch, "rx");
    gc_collect();
    println(c.join());
    p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "rx-keep:done\n");
}

#[test]
fn coop_chan_06_gc_stress_all_scheduler_boundaries() {
    let (out, ok) = compile_and_run_gc_stress_all(
        r#"
class Box { pub text: String; }
async fn producer(ch: Channel<Box>) -> i64 {
    await sleep(1);
    ch.send(new Box("v" + "1"));
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<Box>, prefix: String) -> String {
    let kept = prefix + "-keep";
    let b = ch.recv();
    return kept + ":" + b.text;
}
async fn main() {
    let ch = Channel<Box>::new();
    let p = producer(ch);
    let c = consumer(ch, "rx");
    println(c.join());
    p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "rx-keep:v1\n");
}

fn assert_catalog_lines(out: &str, cases: &[(&str, &str)]) {
    let actual = out.lines().collect::<Vec<_>>();
    assert_eq!(
        actual.len(),
        cases.len(),
        "catalog output line count mismatch:\n{out}"
    );
    for (index, ((name, expected), actual)) in cases.iter().zip(actual.iter()).enumerate() {
        assert_eq!(
            *actual,
            *expected,
            "catalog case {} ({name}) failed",
            index + 1
        );
    }
}

#[test]
fn async_catalog_50_cases() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

async fn id_i64(x: i64) -> i64 { await sleep(1); return x; }
async fn plus(a: i64, b: i64) -> i64 { await sleep(1); return a + b; }
async fn flag(value: bool) -> bool { await sleep(1); return value; }
async fn half(value: f64) -> f64 { await sleep(1); return value / 2.0; }
async fn mark(value: String) -> String { await sleep(1); return value + "!"; }
async fn wrap(value: String) -> String { return await mark(value); }
async fn delayed_sum(values: Array<i64>) -> i64 {
    let mut total = 0;
    for value in values { await sleep(1); total = total + value; }
    return total;
}
async fn range_sum(end: i64) -> i64 {
    let mut total = 0;
    for value in 1..end { await sleep(1); total = total + value; }
    return total;
}
async fn while_sum(end: i64) -> i64 {
    let mut total = 0;
    let mut value = 1;
    while value <= end { await sleep(1); total = total + value; value = value + 1; }
    return total;
}
async fn choose(cond: bool, a: i64, b: i64) -> i64 { await sleep(1); return cond ? a : b; }
async fn mutate_local(seed: i64) -> i64 {
    let mut value = seed;
    value = await plus(value, 2);
    await sleep(1);
    return value;
}
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(10);
    ch.send(20);
    ch.close();
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let a = ch.recv();
    let b = ch.recv();
    return a + b;
}
async fn string_producer(ch: Channel<String>, prefix: String) -> i64 {
    await sleep(1);
    ch.send(prefix + "-a");
    ch.send(prefix + "-b");
    ch.close();
    return 0;
}
async fn string_consumer(ch: Channel<String>) -> String {
    let a = ch.recv();
    let b = ch.recv();
    return a + b;
}
async fn square(x: i64) -> i64 { return x * x; }
async fn async_square(x: i64) -> i64 { await sleep(1); return x * x; }
async fn async_bool(value: i64) -> bool { await sleep(1); return value > 0; }
async fn async_text(value: String) -> String { await sleep(1); return value + "?"; }
async fn nested_left(x: i64) -> i64 {
    let y = await plus(x, 1);
    await sleep(1);
    return y + 1;
}
async fn nested_right(x: i64) -> i64 {
    let y = await nested_left(x);
    await sleep(1);
    return y + 1;
}
async fn count_down(seed: i64) -> i64 {
    let mut value = seed;
    while value > 0 { await sleep(1); value = value - 1; }
    return value;
}
async fn maybe_sleep(flag_value: bool) -> i64 {
    if flag_value { await sleep(1); return 31; } else { await sleep(1); return 32; }
}
async fn array_pick(values: Array<i64>, index: i64) -> i64 { await sleep(1); return values[index]; }
async fn array_update() -> i64 {
    let mut values: Array<i64> = [1, 2, 3];
    values[1] = await plus(values[0], values[2]);
    await sleep(1);
    return values[1];
}
async fn gc_string(value: String) -> String {
    gc_collect();
    await sleep(1);
    gc_collect();
    return value + "*";
}
async fn return_array() -> Array<i64> { await sleep(1); return [4, 5, 6]; }
async fn join_after_sleep(value: i64) -> i64 { await sleep(1); return value; }

async fn main() {
    println(await id_i64(1));
    println(await plus(1, 1));
    println(await flag(true));
    println(await flag(false));
    println(await half(5.0));
    println(await mark("hello"));
    println(await wrap("wrap"));
    let s1 = await id_i64(3);
    let s2 = await id_i64(4);
    println(s1 + s2);
    let mut assigned = 0;
    assigned = await plus(5, 5);
    println(assigned);
    await id_i64(10);
    println(11);
    if true { await sleep(1); println(12); }
    if false { println(0); } else { await sleep(1); println(13); }
    println(await while_sum(3));
    println(await delayed_sum([1, 2, 3]));
    println(await range_sum(4));
    let h1 = square(4);
    println(h1.join());
    let h2 = async_square(5);
    println(h2.join());
    let ha = async_square(2);
    let hb = async_square(3);
    println(ha.join() + hb.join());
    let hc = join_after_sleep(21);
    await sleep(1);
    println(hc.join());
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join());
    p.join();
    let sch = Channel<String>::new();
    let sp = string_producer(sch, "m");
    let sc = string_consumer(sch);
    println(sc.join());
    sp.join();
    ch.close();
    println(ch.recv());
    println(await gc_string("live"));
    let array_value: Array<i64> = [4, 5];
    println(await delayed_sum(array_value));
    println(await choose(true, 27, 0));
    println(await choose(false, 0, 28));
    println(await plus(14, 15));
    println(await plus(15, 16));
    println(await maybe_sleep(true));
    println(await maybe_sleep(false));
    println(await nested_right(30));
    println(await count_down(3));
    println(await array_pick([40, 41, 42], 1));
    println(await array_update());
    let returned = await return_array();
    println(returned[2]);
    println(await async_bool(1));
    println(await async_bool(-1));
    println(await async_text("text"));
    let j1 = async_bool(2);
    println(j1.join());
    let j2 = async_text("join");
    println(j2.join());
    let j3 = half(3.0);
    println(j3.join());
    let mut loop_total = 0;
    for n in 1..5 { await sleep(1); loop_total = loop_total + n; }
    println(loop_total);
    let mut while_total = 0;
    let mut wi = 0;
    while wi < 3 { await sleep(1); while_total = while_total + wi; wi = wi + 1; }
    println(while_total);
    await sleep(0);
    println(48);
    await sleep(-1);
    println(49);
    println(await mutate_local(40));
    let j4 = async_square(6);
    println(j4.join());
    println(await delayed_sum([7, 8]));
    println(await mark("last"));
    println(await plus(25, 25));
}
"#,
    );
    assert!(ok, "{out}");
    assert_catalog_lines(
        &out,
        &[
            ("await_i64", "1"),
            ("await_add", "2"),
            ("await_bool_true", "true"),
            ("await_bool_false", "false"),
            ("await_f64", "2.5"),
            ("await_string", "hello!"),
            ("return_call_await", "wrap!"),
            ("sequential_awaits", "7"),
            ("assign_await", "10"),
            ("discard_await", "11"),
            ("await_in_if", "12"),
            ("await_in_else", "13"),
            ("await_in_while", "6"),
            ("await_in_array_for", "6"),
            ("await_in_range_for", "6"),
            ("spawn_sync_join", "16"),
            ("spawn_async_join", "25"),
            ("multiple_async_joins", "13"),
            ("await_before_join", "21"),
            ("channel_i64", "30"),
            ("channel_string", "m-am-b"),
            ("closed_channel_default", "0"),
            ("gc_string_across_await", "live*"),
            ("array_param_across_await", "9"),
            ("ternary_true_after_await", "27"),
            ("ternary_false_after_await", "28"),
            ("await_add_again", "29"),
            ("await_add_second", "31"),
            ("if_true_return", "31"),
            ("if_false_return", "32"),
            ("nested_call_await", "33"),
            ("countdown_loop", "0"),
            ("array_index_after_await", "41"),
            ("array_assignment_await", "4"),
            ("async_return_array", "6"),
            ("spawn_bool_true", "true"),
            ("spawn_bool_false", "false"),
            ("async_text", "text?"),
            ("join_bool", "true"),
            ("join_string", "join?"),
            ("join_f64", "1.5"),
            ("main_range_loop", "10"),
            ("main_while_loop", "3"),
            ("zero_sleep", "48"),
            ("negative_sleep", "49"),
            ("mutate_local_after_await", "42"),
            ("spawn_square_again", "36"),
            ("array_sum_again", "15"),
            ("string_mark_again", "last!"),
            ("final_add", "50"),
        ],
    );
}

#[test]
fn async_object_catalog_50_cases() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;

class Box {
    pub v: i64;
    pub fn get(self) -> i64 { return self.v; }
    pub fn add(self, n: i64) { self.v = self.v + n; }
    pub fn set(self, n: i64) { self.v = n; }
    pub fn copy(self) -> Box { return new Box(self.v); }
    pub static fn new(v: i64) -> Box { return new Box(v); }
}
class Holder { pub text: String; pub child: Box?; }
class Pair { pub left: Box; pub right: Box; }
class FlagBox { pub ok: bool; }
class FloatBox { pub v: f64; }
class Node { pub v: i64; pub next: Node?; }
interface Named { fn name(self) -> String; }
interface Greeter { fn name(self) -> String; fn greet(self) -> String { return "hi " + self.name(); } }
class User implements Named, Greeter { pub label: String; pub fn name(self) -> String { return self.label; } }
open class Animal { pub open fn score(self) -> i64 { return 1; } }
class Dog extends Animal { pub bonus: i64; pub override fn score(self) -> i64 { return self.bonus + 2; } }

async fn read_value(b: Box) -> i64 { await sleep(1); return b.v; }
async fn read_method(b: Box) -> i64 { await sleep(1); return b.get(); }
async fn add_after(b: Box, n: i64) -> i64 { await sleep(1); b.add(n); return b.v; }
async fn set_after(b: Box, n: i64) -> i64 { await sleep(1); b.set(n); return b.v; }
async fn make_box(v: i64) -> Box { await sleep(1); return new Box(v); }
async fn same_box(b: Box) -> Box { await sleep(1); return b; }
async fn copy_after(b: Box) -> Box { await sleep(1); return b.copy(); }
async fn plus_i64(a: i64, b: i64) -> i64 { await sleep(1); return a + b; }
async fn holder_text(h: Holder) -> String { await sleep(1); return h.text; }
async fn update_holder(h: Holder, suffix: String) -> String { await sleep(1); h.text = h.text + suffix; return h.text; }
async fn child_value(h: Holder) -> i64 { await sleep(1); let child = h.child; if child == nil { return 0; } return child.v; }
async fn pair_sum(p: Pair) -> i64 { await sleep(1); return p.left.v + p.right.v; }
async fn array_sum(xs: Array<Box>) -> i64 { let mut total = 0; for x in xs { await sleep(1); total = total + x.v; } return total; }
async fn array_sum_gc(xs: Array<Box>) -> i64 { gc_collect(); let mut total = 0; for x in xs { await sleep(1); gc_collect(); total = total + x.v; } return total; }
async fn box_producer(ch: Channel<Box>) -> i64 { await sleep(1); ch.send(new Box(9)); ch.send(new Box(10)); ch.close(); return 0; }
async fn box_consumer(ch: Channel<Box>) -> i64 { let a = ch.recv(); let b = ch.recv(); return a.v + b.v; }
async fn return_boxes() -> Array<Box> { await sleep(1); return [new Box(9), new Box(11)]; }
async fn gc_box_value(b: Box) -> i64 { gc_collect(); await sleep(1); gc_collect(); return b.v; }
async fn gc_holder_text(h: Holder) -> String { gc_collect(); await sleep(1); gc_collect(); return h.text; }
async fn named_name(n: Named) -> String { await sleep(1); return n.name(); }
async fn greet_text(g: Greeter) -> String { await sleep(1); return g.greet(); }
async fn animal_score(a: Animal) -> i64 { await sleep(1); return a.score(); }
async fn option_box(opt: Option<Box>) -> i64 { await sleep(1); return match opt { Option::Some(b) => b.v, Option::None => 0 }; }
async fn result_box(r: Result<Box, String>) -> i64 { await sleep(1); return match r { Result::Ok(b) => b.v, Result::Err(e) => 0 }; }
fn sound(n: Named) -> String { return match n { User(u) => u.name() + "!", _ => "?" }; }
async fn named_sound(n: Named) -> String { await sleep(1); return sound(n); }
fn sum_nodes(node: Node?) -> i64 { if node == nil { return 0; } return node.v + sum_nodes(node.next); }
async fn async_sum_nodes(node: Node?) -> i64 { await sleep(1); return sum_nodes(node); }
async fn choose_box(cond: bool, a: Box, b: Box) -> Box { await sleep(1); return cond ? a : b; }
async fn make_from_static(v: i64) -> Box { await sleep(1); return Box::new(v); }
async fn flag_value(f: FlagBox) -> bool { await sleep(1); return f.ok; }
async fn float_half(f: FloatBox) -> f64 { await sleep(1); return f.v / 2.0; }
async fn make_holder(text: String, value: i64) -> Holder { await sleep(1); return new Holder(text, new Box(value)); }
async fn holder_child_copy_value(h: Holder) -> i64 { await sleep(1); let child = h.child; if child == nil { return 0; } let copied = child.copy(); return copied.v; }
async fn user_producer(ch: Channel<User>) -> i64 { await sleep(1); ch.send(new User("chan")); ch.close(); return 0; }
async fn user_consumer(ch: Channel<User>) -> String { let u = ch.recv(); return u.name(); }
async fn nested_box(v: i64) -> Box { return await make_box(v); }

async fn main() {
    println(await read_value(new Box(1)));
    println(await read_method(new Box(2)));
    let b3 = new Box(3);
    println(await add_after(b3, 1));
    println(b3.v);
    let b5 = await make_box(5);
    println(b5.v);
    let b6 = await same_box(b5);
    println(b6.v);
    let alias = b3;
    println(await add_after(alias, 3));
    println(b3.v);
    println(await set_after(b3, 9));
    println(b3.v);
    let h = new Holder("a", b3);
    println(await holder_text(h));
    println(await update_holder(h, "b"));
    println(h.text);
    println(await child_value(h));
    let empty = new Holder("empty", nil);
    println(await child_value(empty));
    let pair = new Pair(new Box(7), new Box(8));
    println(await pair_sum(pair));
    println(await array_sum([new Box(1), new Box(2), new Box(3)]));
    let mut arr: Array<Box> = [new Box(4), new Box(5)];
    arr[1] = await make_box(18);
    println(arr[1].v);
    let ch = Channel<Box>::new();
    let p = box_producer(ch);
    let c = box_consumer(ch);
    println(c.join());
    p.join();
    let boxes = await return_boxes();
    println(boxes[0].v + boxes[1].v);
    let j = make_box(21);
    println(j.join().v);
    let jr = read_value(new Box(22));
    println(jr.join());
    let shared = new Box(20);
    let r1 = read_value(shared);
    let r2 = read_method(shared);
    println(r1.join() + r2.join());
    println(await gc_box_value(new Box(24)));
    println(await gc_holder_text(new Holder("gc", new Box(1))));
    let u = new User("Ada");
    println(await named_name(u));
    println(await greet_text(u));
    println(await animal_score(new Dog(26)));
    println(await option_box(Option::Some(new Box(29))));
    println(await option_box(Option::None));
    println(await result_box(Result::Ok(new Box(31))));
    println(await result_box(Result::Err("bad")));
    println(await named_sound(new User("Rex")));
    let n3 = new Node(3, nil);
    let n2 = new Node(2, n3);
    let n1 = new Node(1, n2);
    println(await async_sum_nodes(n1));
    println((await choose_box(true, new Box(35), new Box(0))).v);
    println((await choose_box(false, new Box(0), new Box(36))).v);
    let copied = await copy_after(new Box(37));
    println(copied.v);
    let b38 = await make_from_static(38);
    println(b38.get());
    let h39 = new Holder("h", nil);
    h39.child = await make_box(39);
    println(await child_value(h39));
    let b40 = new Box(0);
    b40.v = await plus_i64(20, 20);
    println(b40.v);
    println(await flag_value(new FlagBox(true)));
    println(await float_half(new FloatBox(84.0)));
    println(await array_sum_gc([new Box(20), new Box(23)]));
    let h44 = await make_holder("n", 44);
    println(await child_value(h44));
    println(await holder_child_copy_value(h44));
    let user_ch = Channel<User>::new();
    let up = user_producer(user_ch);
    let uc = user_consumer(user_ch);
    println(uc.join());
    up.join();
    let jh = make_holder("j", 47);
    println(await child_value(jh.join()));
    println((await nested_box(48)).v);
    println(await named_name(new User("last")));
    println(await read_value(new Box(50)));
}
"#,
    );
    assert!(ok, "{out}");
    assert_catalog_lines(
        &out,
        &[
            ("object_param_field", "1"),
            ("object_method_after_await", "2"),
            ("object_mutation_return", "4"),
            ("object_mutation_visible", "4"),
            ("async_returns_object", "5"),
            ("same_object_return", "5"),
            ("alias_mutation_return", "7"),
            ("alias_mutation_visible", "7"),
            ("set_after_await_return", "9"),
            ("set_after_await_visible", "9"),
            ("string_field_read", "a"),
            ("string_field_update", "ab"),
            ("string_field_visible", "ab"),
            ("nullable_child_present", "9"),
            ("nullable_child_nil", "0"),
            ("nested_pair_sum", "15"),
            ("object_array_sum", "6"),
            ("object_array_assignment", "18"),
            ("object_channel_sum", "19"),
            ("async_returns_object_array", "20"),
            ("spawn_returns_object", "21"),
            ("spawn_reads_object", "22"),
            ("two_tasks_read_same_object", "40"),
            ("gc_object_across_await", "24"),
            ("gc_string_field_across_await", "gc"),
            ("interface_dispatch_after_await", "Ada"),
            ("interface_default_after_await", "hi Ada"),
            ("virtual_dispatch_after_await", "28"),
            ("option_some_object", "29"),
            ("option_none_object", "0"),
            ("result_ok_object", "31"),
            ("result_err_object", "0"),
            ("interface_downcast_after_await", "Rex!"),
            ("nullable_chain_sum", "6"),
            ("ternary_object_true", "35"),
            ("ternary_object_false", "36"),
            ("copy_method_after_await", "37"),
            ("static_constructor_after_await", "38"),
            ("nullable_field_assignment_await", "39"),
            ("field_assignment_await_scalar", "40"),
            ("bool_field_after_await", "true"),
            ("f64_field_after_await", "42"),
            ("gc_object_array_after_await", "43"),
            ("async_returns_nested_holder", "44"),
            ("copy_nullable_child", "44"),
            ("channel_user_object", "chan"),
            ("join_holder_then_await", "47"),
            ("nested_async_object_return", "48"),
            ("interface_gc_final", "last"),
            ("final_object_read", "50"),
        ],
    );
}

#[test]
fn async_method_instance_static_and_gc_values() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
class Counter {
    pub value: i64;
    pub async fn add_after(self, n: i64) -> i64 {
        await sleep(1);
        self.value = self.value + n;
        return self.value;
    }
    pub static async fn twice(n: i64) -> i64 {
        await sleep(1);
        return n * 2;
    }
}
class Label {
    pub text: String;
    pub async fn suffix(self, s: String) -> String {
        await sleep(1);
        gc_collect();
        return self.text + s;
    }
}
async fn main() {
    let c = new Counter(10);
    let first = await c.add_after(5);
    println(first);
    let task = c.add_after(7);
    println(task.join());
    println(c.value);
    let doubled = await Counter::twice(4);
    println(doubled);
    c.value = await Counter::twice(6);
    println(c.value);
    let label = new Label("async");
    let text = await label.suffix("-method");
    println(text);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "15\n22\n22\n8\n12\nasync-method\n");
}

#[test]
fn async_method_dispatch_and_interface_task_surface() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
open class Base {
    pub open async fn score(self) -> i64 {
        await sleep(1);
        return 1;
    }
}
class Child extends Base {
    pub override async fn score(self) -> i64 {
        await sleep(1);
        return 9;
    }
}
interface AsyncGetter {
    fn get(self) -> Task<i64>;
}
class Box implements AsyncGetter {
    pub v: i64;
    pub async fn get(self) -> i64 {
        await sleep(1);
        return self.v;
    }
}
async fn main() {
    let b: Base = new Child();
    let score = await b.score();
    println(score);
    let g: AsyncGetter = new Box(6);
    let value = await g.get();
    println(value);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n6\n");
}

#[test]
fn async_method_return_task_handle_annotation_is_rejected() {
    assert_compile_error_contains(
        r#"
class Bad {
    async fn work(self) -> Task<i64> {
        return 1;
    }
}
fn main() {}
"#,
        &[
            "error[E0809]",
            "async method return type must be the awaited value",
        ],
    );
}

// ----------------------------------------------------------------------------
// select (willow-7aj): wait on multiple channel ops. A recv case is ready when
// its channel has a value or is closed; a send case (unbounded) is always
// ready; the first ready case runs; `default` runs when nothing is ready.
// ----------------------------------------------------------------------------

#[test]
fn select_01_default_on_empty() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    select {
        let v = ch.recv() => { println(v); }
        default => { println(-1); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-1\n");
}

#[test]
fn select_02_recv_ready_value() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    ch.send(42);
    select {
        let v = ch.recv() => { println(v); }
        default => { println(-1); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn select_03_recv_drives_scheduler_until_producer() {
    // No default: select drives the scheduler until a spawned producer sends.
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(99);
    return 0;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    select {
        let v = ch.recv() => { println(v); }
    }
    p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n");
}

#[test]
fn select_04_first_ready_of_multiple_recv() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let a = Channel<i64>::new();
    let b = Channel<i64>::new();
    b.send(7);
    select {
        let x = a.recv() => { println(x + 1000); }
        let y = b.recv() => { println(y); }
        default => { println(-1); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn select_05_send_case() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let out = Channel<i64>::new();
    select {
        out.send(55) => { println(1); }
        default => { println(-1); }
    }
    println(out.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n55\n");
}

#[test]
fn select_06_string_channel_literal_gc() {
    // A String channel select-send of a literal queues correctly (literal must
    // be collected from the select case), and survives GC stress.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn main() {
    let ch = Channel<String>::new();
    select {
        ch.send("hello") => { println(1); }
    }
    println(ch.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\nhello\n");
}

#[test]
fn select_07_non_channel_is_error() {
    assert_compile_error_contains(
        r#"
async fn main() {
    let x = 5;
    select {
        let v = x.recv() => { println(v); }
    }
}
"#,
        &["error[E0807]", "Channel"],
    );
}

// willow-lpn.7: a task parked on a TIMER keeps its async-frame GC roots alive
// while a CONCURRENT task triggers collection. The sleeper's frame is a runtime
// root while parked, so its live String survives.
#[test]
fn coop_gc_06_timer_parked_frame_survives_concurrent_gc() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn sleeper() -> i64 {
    let s = "kept-across-timer-park";
    await sleep(5);
    println(s);
    return 0;
}
async fn collector() -> i64 {
    await sleep(1);
    gc_collect();
    let junk = "x" + "y";
    gc_collect();
    return 0;
}
async fn main() {
    let a = sleeper();
    let b = collector();
    a.join();
    b.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "kept-across-timer-park\n");
}

// ── willow-7aj: cooperative-suspend `select` (a select INSIDE a task PARKS on
// its channels instead of block-driving). 20 test perspectives:
//  1. single recv parks when empty, woken by a later send -> receives value
//  2. repeated select in a while loop (park/wake each iteration)
//  3. multi-channel select: parks on all, woken by whichever is ready first
//  4. multi-channel across iterations (channel a then channel b)
//  5. default present + channel empty -> default branch runs (no park)
//  6. default present + channel ready -> ready branch runs (default skipped)
//  7. send case is always ready and fires
//  8. Channel<String> recv binding is GC-traced (survives gc_collect after recv)
//  9. recv binding is usable inside the case body
// 10. case body with its OWN suspend (await sleep) after the binding -> binding survives
// 11. select woken by close() -> recv returns the element default (0)
// 12. unregister: after picking channel a, a later send on the OTHER channel b
//     does not corrupt the next select iteration
// 13. `_` discard binding recv
// 14. select nested in a while loop summing values (canonical consumer)
// 15. source-order priority when multiple recv cases are ready
// 16. send-case value matches the channel element type
// 17. a select-only task is a cooperative leaf (spawn/join works)
// 18. whole thing under WILLOW_GC_STRESS=all
// 19. select runs in a spawned task joined by main
// 20. case body contains a second recv (nested suspend points)

#[test]
fn coop_select_01_single_recv_parks_and_wakes() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 { await sleep(1); ch.send(42); return 0; }
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    select { let v = ch.recv() => { total = v; } }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join()); p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn coop_select_02_while_loop_sum() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1); ch.send(10);
    await sleep(1); ch.send(20);
    await sleep(1); ch.send(30);
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < 3 {
        select { let v = ch.recv() => { total = total + v; } }
        i = i + 1;
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join()); p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "60\n");
}

#[test]
fn coop_select_03_multi_channel_parks_on_both() {
    // Perspectives 3, 4, 12: parks on both channels; after a wakes it, the next
    // iteration parks again and b wakes it; unregistering from the non-chosen
    // channel keeps the second iteration correct.
    let (out, ok) = compile_and_run(
        r#"
async fn p1(ch: Channel<i64>) -> i64 { await sleep(1); ch.send(100); return 0; }
async fn p2(ch: Channel<i64>) -> i64 { await sleep(2); ch.send(200); return 0; }
async fn consumer(a: Channel<i64>, b: Channel<i64>) -> i64 {
    let mut total = 0;
    let mut n = 0;
    while n < 2 {
        select {
            let v = a.recv() => { total = total + v; }
            let v = b.recv() => { total = total + v; }
        }
        n = n + 1;
    }
    return total;
}
async fn main() {
    let a = Channel<i64>::new();
    let b = Channel<i64>::new();
    let x = p1(a);
    let y = p2(b);
    let c = consumer(a, b);
    println(c.join()); x.join(); y.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "300\n");
}

#[test]
fn coop_select_04_default_when_empty() {
    let (out, ok) = compile_and_run(
        r#"
async fn worker(ch: Channel<i64>) -> i64 {
    await sleep(1);
    let mut hit = 0;
    select {
        let v = ch.recv() => { hit = v; }
        default => { hit = -1; }
    }
    return hit;
}
async fn main() {
    let ch = Channel<i64>::new();
    let w = worker(ch);
    println(w.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-1\n");
}

#[test]
fn coop_select_05_default_skipped_when_ready() {
    let (out, ok) = compile_and_run(
        r#"
async fn worker(ch: Channel<i64>) -> i64 {
    ch.send(5);
    await sleep(1);
    let mut hit = 0;
    select {
        let v = ch.recv() => { hit = v; }
        default => { hit = -1; }
    }
    return hit;
}
async fn main() {
    let ch = Channel<i64>::new();
    let w = worker(ch);
    println(w.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn coop_select_06_send_case() {
    let (out, ok) = compile_and_run(
        r#"
async fn sender(ch: Channel<i64>) -> i64 {
    await sleep(1);
    select { ch.send(7) => { } }
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 { let v = ch.recv(); return v; }
async fn main() {
    let ch = Channel<i64>::new();
    let s = sender(ch);
    let c = consumer(ch);
    println(c.join()); s.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn coop_select_07_string_binding_gc_safe() {
    // Perspectives 8, 18: the recv binding's frame slot is GC-traced.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<String>) -> i64 {
    await sleep(1);
    let s = "hello-" + "world";
    ch.send(s);
    return 0;
}
async fn consumer(ch: Channel<String>) -> i64 {
    let mut out = "empty";
    select { let v = ch.recv() => { out = v; } }
    gc_collect();
    println(out);
    return 0;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch);
    let c = consumer(ch);
    c.join(); p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hello-world\n");
}

#[test]
fn coop_select_08_woken_by_close() {
    // Perspective 11: close() wakes a parked select; recv returns the default (0).
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 { await sleep(1); ch.close(); return 0; }
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut got = 99;
    select { let v = ch.recv() => { got = v; } }
    return got;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join()); p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn coop_select_09_case_body_nested_suspend() {
    // Perspectives 10, 20: the case body itself suspends (await sleep, then a
    // second recv) after binding; the binding and locals survive those suspends.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1); ch.send(11);
    await sleep(1); ch.send(22);
    return 0;
}
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut total = 0;
    select {
        let v = ch.recv() => {
            await sleep(1);
            let w = ch.recv();
            total = v + w;
        }
    }
    return total;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(c.join()); p.join();
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "33\n");
}

#[test]
fn coop_select_10_source_order_priority() {
    // Perspectives 13, 15: when several recv cases are ready, the first in source
    // order wins; `_` discard binding is allowed.
    let (out, ok) = compile_and_run(
        r#"
async fn worker(a: Channel<i64>, b: Channel<i64>) -> i64 {
    a.send(1);
    b.send(2);
    await sleep(1);
    let mut picked = 0;
    select {
        let _ = a.recv() => { picked = 10; }
        let v = b.recv() => { picked = v; }
    }
    return picked;
}
async fn main() {
    let a = Channel<i64>::new();
    let b = Channel<i64>::new();
    let w = worker(a, b);
    println(w.join());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}

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

// ── willow-ca2: lexer numeric/comment diagnostics (end-to-end) ───────────────

// End-to-end: an integer literal that overflows i64 surfaces as E0052 through
// the full compiler (previously it was silently parsed as 0).
