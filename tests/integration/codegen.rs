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

// ── Explicit Option construction (willow-glaj.8 migration) ──────────────────

// 1. Explicit Some(String) construction compiles and prints.
#[test]
fn test_option_some_string_literal() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s: Option<String> = Some("hello");
    println(s.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 2. An Option<String> return explicitly chooses Some or None.
#[test]
fn test_option_return_string_is_explicit() {
    let (out, ok) = compile_and_run(
        r#"
fn greet(flag: bool) -> Option<String> {
    if flag { return Some("hi"); }
    return None;
}
fn main() {
    let a = greet(true);
    let b = greet(false);
    println(a.unwrap());
    if b.is_none() { println("none"); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hi\nnone\n");
}

// 3. Option parameters require explicit construction at the call site.
#[test]
fn test_option_argument_construction_is_explicit() {
    let (out, ok) = compile_and_run(
        r#"
fn print_maybe(s: Option<String>) {
    match s { Some(value) => println(value), None => println("empty") }
}
fn main() {
    print_maybe(Some("world"));
    print_maybe(None);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "world\nempty\n");
}

// 4. A non-Option value cannot initialize an Option.
#[test]
fn test_option_rejects_unwrapped_unrelated_type() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let s: Option<String> = 42;
}
"#
    ));
}

// 5. None is the sole absence value.
#[test]
fn test_option_none_is_explicit() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s: Option<String> = None;
    if s.is_none() { println("none"); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "none\n");
}

// 6. Class values use explicit Some construction too.
#[test]
fn test_option_class_construction_is_explicit() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } }
fn maybe(flag: bool) -> Option<Box> {
    if flag { return Some(new Box(99)); }
    return None;
}
fn main() {
    let b = maybe(true);
    match b { Some(value) => println(value.get()), None => println(0) }
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

// ── grouped imports (willow-4bv.7, Stage 7) ────────────────────────────────

#[test]
fn test_grouped_std_collection_imports_are_usable() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::{Array, Map};

fn main() {
    let xs: Array<i64> = [10, 20];
    let values: Map<String, i64> = Map::new();
    values.insert("answer", xs[0] + xs[1] + 12);
    println(values.get("answer").unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_grouped_import_per_item_aliases_are_usable() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::{Array as List, Map as Dict,};

fn main() {
    let xs: List<i64> = [1, 2, 3];
    let values: Dict<String, i64> = Dict::new();
    values.insert("size", xs.len());
    println(values.get("size").unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn test_grouped_import_unknown_item_reuses_e2006() {
    assert_compile_error_contains(
        r#"
import std::collections::{Array, Missing};
fn main() {}
"#,
        &["error[E2006]", "no item `Missing` in `std::collections`"],
    );
}

#[test]
fn test_grouped_import_local_declaration_conflict_reuses_e2003() {
    assert_compile_error_contains(
        r#"
import std::collections::{Array, Map};
class Map {}
fn main() {}
"#,
        &["error[E2003]", "import and a local declaration"],
    );
}

#[test]
fn test_grouped_user_module_items_are_callable() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "math.wi",
                "module math;\n\
                 pub fn add(a: i64, b: i64) -> i64 { return a + b; }\n\
                 pub fn mul(a: i64, b: i64) -> i64 { return a * b; }\n",
            ),
            (
                "main.wi",
                "import math::{add, mul as times};\n\
                 fn main() { println(add(20, 22)); println(times(6, 7)); }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "42\n42\n");
}

#[test]
fn test_grouped_user_module_private_item_reuses_visibility_diagnostic() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "helpers.wi",
                "module helpers;\n\
                 pub fn visible() -> i64 { return 1; }\n\
                 fn hidden() -> i64 { return 2; }\n",
            ),
            (
                "main.wi",
                "import helpers::{visible, hidden};\n\
                 fn main() { println(visible()); println(hidden()); }\n",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2006]"), "stderr: {stderr}");
    assert!(stderr.contains("private"), "stderr: {stderr}");
}

#[test]
fn test_glob_import_reports_clear_unsupported_diagnostic() {
    assert_compile_error_contains(
        "import std::collections::*;\nfn main() {}\n",
        &["error[E0102]", "glob imports are not supported"],
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

async fn sum(values: FrozenArray<i64>) -> i64 {
    let mut total = 0;
    let mut index = 0;
    while index < values.len() {
        await sleep(1);
        total = total + values[index];
        index = index + 1;
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
    let total = await sum(hidden.freeze());
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
    println(env::arg(1).unwrap());
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
// Cooperative task await (willow: async work migrated off one-OS-thread-per
// task onto the cooperative scheduler). Calling an async fn queues a
// lightweight task; `await` suspends until its target completes.
// ----------------------------------------------------------------------------

// Await returns each task's result, regardless of await order.
#[test]
fn coop_spawn_01_await_order_independent() {
    let (out, ok) = compile_and_run(
        r#"
async fn sq(x: i64) -> i64 { return x * x; }
async fn main() {
    let a = sq(2);
    let b = sq(3);
    let c = sq(4);
    println(await c);
    println(await a);
    println(await b);
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
async fn main() {
    let a = id(1);
    let b = id(2);
    let c = id(3);
    let d = id(4);
    let e = id(5);
    let f = id(6);
    let g = id(7);
    let h = id(8);
    let total = await a + await b + await c + await d
        + await e + await f + await g + await h;
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
async fn main() {
    let ch = Channel<i64>::new();
    let h = producer(ch);
    println(ch.recv());
    println(ch.recv());
    println(ch.recv());
    await h;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// Task with GC-managed args (object + string), result read via await, under
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
async fn main() {
    let b = Box::new(7);
    let h1 = label(b, "tag");
    let h2 = value(b);
    println(await h1);
    println(await h2);
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
async fn main() {
    let a = positive(5);
    let b = positive(-5);
    println(await a);
    println(await b);
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
fn coop_async_10_await_in_if_else_merge() {
    // Both arms fall through to a shared CFG merge, carrying a frame-backed local.
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

// 16.9: a JoinHandle keeps the task's GC result alive across a collection
// performed before `await`.
#[test]
fn coop_gc_04_joinhandle_keeps_result_alive() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn tag(n: i64) -> String { return "tag"; }
async fn main() {
    let h = tag(7);
    gc_collect();
    gc_collect();
    println(await h);
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

// Awaiting a cooperative-leaf async fn must return the async function's
// REAL result, not the constructor's frame pointer (willow-lpn.5.4 fix).
#[test]
fn coop_spawn_06_spawn_async_leaf_sync_main() {
    let (out, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 {
    await sleep(1);
    return x + 1;
}
async fn main() {
    let h = work(41);
    println(await h);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn coop_spawn_07_spawn_async_leaf_multiple_gc() {
    // Multiple spawned async leaves (i64 + String results) awaited; under GC
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
    println(await h1);
    println(await h2);
    println(await h3);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n15\nhi willow\n");
}

#[test]
fn coop_spawn_08_spawn_async_leaf_runs_to_completion() {
    // The spawned leaf actually runs (side effects observed), spawn does not
    // block the spawner, and await returns the leaf's real result.
    //
    // The exact interleaving of the spawner's prints with the leaf's is NOT an
    // invariant and must not be asserted (willow-0uce). `willow_sched_spawn`
    // publishes the task to the shared run queues and the runtime drives them
    // on a worker pool of at least 5 threads — `WILLOW_WORKERS` is clamped UP
    // to `DEFAULT_WORKERS`, so even a single-worker request gets the pool — so
    // a peer worker may claim and poll `work` the instant it is spawned,
    // concurrently with `main` running on to `println(2)`. Both `1 2 100 ...`
    // and `1 100 2 ...` are legal; a wider gap between the spawn and the next
    // statement makes `1 100 200 2 ...` legal too. Asserting one ordering was
    // a ~10% flake.
    //
    // What IS guaranteed, and is what this test exists to check:
    //   * every side effect happens exactly once,
    //   * each task's own prints keep their program order (1 before 2 before 3
    //     in `main`, 100 before 200 in `work`),
    //   * `await h` does not return until `work` has run to completion, so 200
    //     precedes 3, and
    //   * the awaited value is the leaf's real return value.
    let (out, ok) = compile_and_run(
        r#"
async fn work(x: i64) -> i64 {
    println(100);
    await sleep(1);
    println(200);
    return x;
}
async fn main() {
    println(1);
    let h = work(42);
    println(2);
    let r = await h;
    println(3);
    println(r);
}
"#,
    );
    assert!(ok, "{out}");

    let lines: Vec<&str> = out.lines().collect();
    let at = |needle: &str| {
        let hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| **line == needle)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "{needle} must be printed exactly once: {out}"
        );
        hits[0]
    };
    assert_eq!(lines.len(), 6, "unexpected extra/missing output: {out}");
    let (one, two, three, result) = (at("1"), at("2"), at("3"), at("42"));
    let (leaf_start, leaf_end) = (at("100"), at("200"));
    assert!(one < two && two < three, "spawner order broken: {out}");
    assert!(leaf_start < leaf_end, "leaf order broken: {out}");
    assert!(one < leaf_start, "leaf ran before it was spawned: {out}");
    assert!(
        leaf_end < three,
        "await returned before the leaf finished: {out}"
    );
    assert_eq!(result, three + 1, "awaited value must be the leaf's: {out}");
}

// Cooperative concurrency: spawned async-leaf tasks suspend independently at
// their awaits and the scheduler interleaves them — observably distinct from
// sequential execution (willow-lpn.5.4). The scheduler is a worker POOL, not a
// single thread, so only each task's own output order is an invariant; the
// interleaving between tasks is not (willow-0uce).
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
async fn main() {
    let a = worker(1);
    let b = worker(2);
    println(await a + await b);
}
"#,
    );
    assert!(ok, "{out}");
    let lines = out.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 5, "{out}");
    assert_eq!(
        lines[4], "3",
        "both awaits must complete before the sum: {out}"
    );
    for (start, finish) in [("1", "101"), ("2", "102")] {
        let start_at = lines[..4].iter().position(|line| *line == start).unwrap();
        let finish_at = lines[..4].iter().position(|line| *line == finish).unwrap();
        assert!(
            start_at < finish_at,
            "worker {start} reordered its output: {out}"
        );
    }
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
async fn main() {
    let a = worker(1);
    let b = worker(2);
    println(await a + await b);
}
"#,
    );
    assert!(ok, "{out}");
    let lines = out.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 5, "{out}");
    assert_eq!(
        lines[4], "3",
        "both awaits must complete before the sum: {out}"
    );
    for (start, finish) in [("1", "11"), ("2", "12")] {
        let start_at = lines[..4].iter().position(|line| *line == start).unwrap();
        let finish_at = lines[..4].iter().position(|line| *line == finish).unwrap();
        assert!(
            start_at < finish_at,
            "worker {start} reordered its output: {out}"
        );
    }
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
async fn main() {
    let task = keep("yield");
    gc_collect();
    println(await task);
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
    println(await a + await b + await c);
}
"#,
    );
    assert!(ok, "{out}");
    let lines = out.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 7, "{out}");
    assert_eq!(lines[6], "6", "sum must print after every await: {out}");
    for (start, finish) in [("1", "10"), ("2", "20"), ("3", "30")] {
        let start_at = lines[..6].iter().position(|line| *line == start).unwrap();
        let finish_at = lines[..6].iter().position(|line| *line == finish).unwrap();
        assert!(
            start_at < finish_at,
            "worker {start} finished before it started: {out}"
        );
    }
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
    await h;
}
"#,
    );
    assert!(ok, "{out}");
    let lines = out.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 3, "{out}");
    for value in ["7", "8", "42"] {
        assert!(lines.contains(&value), "missing {value}: {out}");
    }
    let started = lines.iter().position(|line| *line == "7").unwrap();
    let finished = lines.iter().position(|line| *line == "8").unwrap();
    assert!(
        started < finished,
        "background task reordered its output: {out}"
    );
}

// ----------------------------------------------------------------------------
// Cooperative awaiter-suspend model (willow-lpn.5.3.1): a `let x = await f()` /
// `await f()` of a cooperative leaf SUSPENDS the awaiter via willow_sched_await
// (dependency-wake) rather than block-on, so a fn that MIXES call-awaits and
// sleep-awaits is itself a cooperative task. The callee frame is held in a
// GC-traced awaiter slot across suspension.
// ----------------------------------------------------------------------------

// A spawned worker that mixes a call-await and a sleep-await returns its REAL
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
async fn main() {
    let a = worker(1);
    println(await a);
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
    println(await a + await b);
}
"#,
    );
    assert!(ok, "{out}");
    let lines = out.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 5, "{out}");
    assert_eq!(
        lines[4], "33",
        "both awaits must complete before the sum: {out}"
    );
    for (start, finish) in [("1", "10"), ("2", "20")] {
        let start_at = lines[..4].iter().position(|line| *line == start).unwrap();
        let finish_at = lines[..4].iter().position(|line| *line == finish).unwrap();
        assert!(
            start_at < finish_at,
            "worker {start} reordered its output: {out}"
        );
    }
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
// (task await works) and lets producer/consumer tasks interleave correctly.
// ----------------------------------------------------------------------------

// Producer and consumer tasks interleave; awaiting the consumer returns its result.
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
    ch.send(0);
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
    println(await c);
    await p;
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
    ch.send(0);
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
    println(await c);
    await p;
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
    await p;
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
    await c;
    await p;
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
    println(await c);
    await p;
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
    println(await c);
    await p;
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
async fn delayed_sum(a: i64, b: i64, c: i64) -> i64 {
    let values: Array<i64> = [a, b, c];
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
async fn array_pick(a: i64, b: i64, c: i64, index: i64) -> i64 { let values: Array<i64> = [a, b, c]; await sleep(1); return values[index]; }
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
async fn await_after_sleep(value: i64) -> i64 { await sleep(1); return value; }

// Split into parts: one async fn holding all 50 cases would need more
// GC-managed frame slots than the frame's reference mask can describe.
async fn main() {
    await part1();
    await part2();
    await part3();
    await part4();
}

async fn part1() {
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
    println(await delayed_sum(1, 2, 3));
    println(await range_sum(4));
    let h1 = square(4);
    println(await h1);
    let h2 = async_square(5);
    println(await h2);
    let ha = async_square(2);
    let hb = async_square(3);
    println(await ha + await hb);
    let hc = await_after_sleep(21);
    await sleep(1);
    println(await hc);
}

async fn part2() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(await c);
    await p;
    let sch = Channel<String>::new();
    let sp = string_producer(sch, "m");
    let sc = string_consumer(sch);
    println(await sc);
    await sp;
    let buffered = Channel<i64>::new();
    buffered.send(0);
    buffered.close();
    println(buffered.recv());
    println(await gc_string("live"));
    let array_value: Array<i64> = [4, 5];
    println(await delayed_sum(array_value[0], array_value[1], 0));
}

async fn part3() {
    println(await choose(true, 27, 0));
    println(await choose(false, 0, 28));
    println(await plus(14, 15));
    println(await plus(15, 16));
    println(await maybe_sleep(true));
    println(await maybe_sleep(false));
    println(await nested_right(30));
    println(await count_down(3));
    println(await array_pick(40, 41, 42, 1));
    println(await array_update());
    let returned = await return_array();
    println(returned[2]);
    println(await async_bool(1));
    println(await async_bool(-1));
    println(await async_text("text"));
    let j1 = async_bool(2);
    println(await j1);
    let j2 = async_text("await");
    println(await j2);
    let j3 = half(3.0);
    println(await j3);
}

async fn part4() {
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
    println(await j4);
    println(await delayed_sum(7, 8, 0));
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
            ("spawn_sync_await", "16"),
            ("spawn_async_await", "25"),
            ("multiple_async_awaits", "13"),
            ("sleep_before_task_await", "21"),
            ("channel_i64", "30"),
            ("channel_string", "m-am-b"),
            ("closed_channel_buffered_value", "0"),
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
            ("await_bool", "true"),
            ("await_string", "await?"),
            ("await_f64", "1.5"),
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
class Holder { pub text: String; pub child: Option<Box>; }
class Pair { pub left: Box; pub right: Box; }
class FlagBox { pub ok: bool; }
class FloatBox { pub v: f64; }
class Node { pub v: i64; pub next: Option<Node>; }
interface Named extends Sync { fn name(self) -> String; }
interface Greeter extends Sync { fn name(self) -> String; fn greet(self) -> String { return "hi " + self.name(); } }
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
async fn child_value(h: Holder) -> i64 { await sleep(1); return match h.child { Some(child) => child.v, None => 0 }; }
async fn pair_sum(p: Pair) -> i64 { await sleep(1); return p.left.v + p.right.v; }
async fn array_sum(a: Box, b: Box, c: Box) -> i64 { let xs: Array<Box> = [a, b, c]; let mut total = 0; for x in xs { await sleep(1); total = total + x.v; } return total; }
async fn array_sum_gc(a: Box, b: Box) -> i64 { let xs: Array<Box> = [a, b]; gc_collect(); let mut total = 0; for x in xs { await sleep(1); gc_collect(); total = total + x.v; } return total; }
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
async fn async_sum_nodes(node: Option<Node>) -> i64 { await sleep(1); let mut total = 0; let mut current = node; while current.is_some() { let value = current.unwrap(); total = total + value.v; current = value.next; } return total; }
async fn choose_box(cond: bool, a: Box, b: Box) -> Box { await sleep(1); return cond ? a : b; }
async fn make_from_static(v: i64) -> Box { await sleep(1); return Box::new(v); }
async fn flag_value(f: FlagBox) -> bool { await sleep(1); return f.ok; }
async fn float_half(f: FloatBox) -> f64 { await sleep(1); return f.v / 2.0; }
async fn make_holder(text: String, value: i64) -> Holder { await sleep(1); return new Holder(text, Some(new Box(value))); }
async fn holder_child_copy_value(h: Holder) -> i64 { await sleep(1); return match h.child { Some(child) => child.copy().v, None => 0 }; }
async fn user_producer(ch: Channel<User>) -> i64 { await sleep(1); ch.send(new User("chan")); ch.close(); return 0; }
async fn user_consumer(ch: Channel<User>) -> String { let u = ch.recv(); return u.name(); }
async fn nested_box(v: i64) -> Box { return await make_box(v); }

// Split into parts: one async fn holding all 50 cases would need more
// GC-managed frame slots than the frame's reference mask can describe.
async fn main() {
    await part1();
    await part2();
    await part3();
    await part4();
}

async fn part1() {
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
    let h = new Holder("a", Some(b3));
    println(await holder_text(h));
    println(await update_holder(h, "b"));
    println(h.text);
    println(await child_value(h));
    let empty = new Holder("empty", None);
    println(await child_value(empty));
    let pair = new Pair(new Box(7), new Box(8));
    println(await pair_sum(pair));
    println(await array_sum(new Box(1), new Box(2), new Box(3)));
}

async fn part2() {
    let mut arr: Array<Box> = [new Box(4), new Box(5)];
    arr[1] = await make_box(18);
    println(arr[1].v);
    let ch = Channel<Box>::new();
    let p = box_producer(ch);
    let c = box_consumer(ch);
    println(await c);
    await p;
    let boxes = await return_boxes();
    println(boxes[0].v + boxes[1].v);
    let j = make_box(21);
    println((await j).v);
    let jr = read_value(new Box(22));
    println(await jr);
    let shared = new Box(20);
    let r1 = read_value(shared);
    let r2 = read_method(shared);
    println(await r1 + await r2);
    println(await gc_box_value(new Box(24)));
    println(await gc_holder_text(new Holder("gc", Some(new Box(1)))));
}

async fn part3() {
    let u = new User("Ada");
    println(await named_name(u));
    println(await greet_text(u));
    println(await animal_score(new Dog(26)));
    println(await option_box(Option::Some(new Box(29))));
    println(await option_box(Option::None));
    println(await result_box(Result::Ok(new Box(31))));
    println(await result_box(Result::Err("bad")));
    println(await named_sound(new User("Rex")));
    let n3 = new Node(3, None);
    let n2 = new Node(2, Some(n3));
    let n1 = new Node(1, Some(n2));
    println(await async_sum_nodes(Some(n1)));
    println((await choose_box(true, new Box(35), new Box(0))).v);
    println((await choose_box(false, new Box(0), new Box(36))).v);
    let copied = await copy_after(new Box(37));
    println(copied.v);
    let b38 = await make_from_static(38);
    println(b38.get());
    let h39 = new Holder("h", None);
    h39.child = Some(await make_box(39));
    println(await child_value(h39));
    let b40 = new Box(0);
    b40.v = await plus_i64(20, 20);
    println(b40.v);
    println(await flag_value(new FlagBox(true)));
    println(await float_half(new FloatBox(84.0)));
    println(await array_sum_gc(new Box(20), new Box(23)));
}

async fn part4() {
    let h44 = await make_holder("n", 44);
    println(await child_value(h44));
    println(await holder_child_copy_value(h44));
    let user_ch = Channel<User>::new();
    let up = user_producer(user_ch);
    let uc = user_consumer(user_ch);
    println(await uc);
    await up;
    let jh = make_holder("j", 47);
    println(await child_value(await jh));
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
            ("option_child_some", "9"),
            ("option_child_none", "0"),
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
            ("await_holder_then_continue", "47"),
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
    println(await task);
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
interface AsyncGetter extends Sync {
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
    await p;
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
    await a;
    await b;
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
// 11. send followed by close wakes a parked select and drains the buffered value
// 12. unregister: after picking channel a, a later send on the OTHER channel b
//     does not corrupt the next select iteration
// 13. `_` discard binding recv
// 14. select nested in a while loop summing values (canonical consumer)
// 15. source-order priority when multiple recv cases are ready
// 16. send-case value matches the channel element type
// 17. a select-only task is a cooperative leaf (task await works)
// 18. whole thing under WILLOW_GC_STRESS=all
// 19. select runs in a spawned task awaited by main
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
    println(await c); await p;
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
    println(await c); await p;
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
    println(await c); await x; await y;
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
    println(await w);
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
    println(await w);
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
    println(await c); await s;
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
    await c; await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hello-world\n");
}

#[test]
fn coop_select_08_send_then_close_wakes_and_drains_value() {
    // Perspective 11: close after a send wakes a parked select; recv drains the
    // buffered value instead of observing closed-empty.
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 { await sleep(1); ch.send(0); ch.close(); return 0; }
async fn consumer(ch: Channel<i64>) -> i64 {
    let mut got = 99;
    select { let v = ch.recv() => { got = v; } }
    return got;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let c = consumer(ch);
    println(await c); await p;
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
    println(await c); await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "33\n");
}

#[test]
fn coop_select_10_fair_pick_among_ready() {
    // Perspectives 13, 15 (revised, willow-0a6k.6): when several recv cases
    // are ready the pick is PSEUDO-RANDOMIZED (mixed global counter) rather
    // than always favoring source order — across many one-shot selects BOTH
    // cases must win at least once. This checks absence of systematic
    // source-order starvation, not bounded fairness. `_` discard binding is
    // allowed; each iteration drains both channels so readiness is identical
    // every time.
    let (out, ok) = compile_and_run(
        r#"
async fn round(a: Channel<i64>, b: Channel<i64>) -> i64 {
    a.send(1);
    b.send(2);
    await sleep(1);
    let mut picked = 0;
    select {
        let _ = a.recv() => { picked = 10; }
        let v = b.recv() => { picked = v; }
    }
    // Drain whichever value the losing case left behind.
    select {
        let _ = a.recv() => { }
        let _ = b.recv() => { }
        default => { }
    }
    return picked;
}
async fn main() {
    let a = Channel<i64>::new();
    let b = Channel<i64>::new();
    let mut saw_first = false;
    let mut saw_second = false;
    let mut i = 0;
    while i < 20 {
        let w = round(a, b);
        let picked = await w;
        if picked == 10 { saw_first = true; }
        if picked == 2 { saw_second = true; }
        i = i + 1;
    }
    println(saw_first);
    println(saw_second);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\n");
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

// ── LIR-walking backend (willow-0g8j) ────────────────────────────────────────
// Functions in the supported scalar subset compile from the lowered IR instead
// of the AST. Differential tests: the SAME program must produce identical
// output with the LIR path enabled (default) and disabled
// (WILLOW_LIR_BACKEND=0). 8 perspectives: recursion, loops (range-for +
// while), f64 arithmetic, bool logic + unary, nested calls, early returns,
// assignment-heavy bodies, and panic call-chain instrumentation parity.

/// The LIR path is on by default, but these tests must never silently compare
/// the AST path against itself, so the "on" side sets `WILLOW_LIR_BACKEND=1`
/// explicitly rather than relying on the ambient environment.
///
/// `WILLOW_LIR_BACKEND=1` alone is not enough: a function outside the walker's
/// supported subset falls back to the AST emitter, so a lowering or eligibility
/// regression would still leave both sides on the AST path and the comparison
/// would pass vacuously. `WILLOW_LIR_REQUIRE=1` makes that fallback a compile
/// error, so every function in these programs is pinned to the LIR path
/// (willow-0g8j.4 review). Programs that deliberately mix paths use
/// [`LIR_ON_MIXED`] instead.
const LIR_ON: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")];

/// The "on" side for a program that intentionally contains a function the
/// walker cannot compile — the point of the test is that the two paths still
/// agree, so a fallback must stay allowed.
const LIR_ON_MIXED: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "1")];
const LIR_OFF: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];

fn assert_lir_differential(source: &str, expected: &str) {
    let (with_lir, ok_on) = compile_with_env_and_run(source, &LIR_ON);
    assert!(ok_on, "LIR-enabled run failed: {with_lir}");
    let (without_lir, ok_off) = compile_with_env_and_run(source, &LIR_OFF);
    assert!(ok_off, "LIR-disabled run failed: {without_lir}");
    assert_eq!(with_lir, without_lir, "LIR and AST paths must agree");
    assert_eq!(with_lir, expected);
}

#[test]
fn lir_diff_01_recursion_fib() {
    assert_lir_differential(
        r#"
fn fib(n: i64) -> i64 {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fn main() { println(fib(10)); }
"#,
        "55\n",
    );
}

#[test]
fn lir_diff_02_loops() {
    assert_lir_differential(
        r#"
fn sum_to(n: i64) -> i64 {
    let mut t = 0;
    for i in 0..n { t = t + i; }
    while t > 100 { t = t - 100; }
    return t;
}
fn main() { println(sum_to(20)); }
"#,
        "90\n",
    );
}

#[test]
fn lir_diff_03_f64_arithmetic() {
    assert_lir_differential(
        r#"
fn area(r: f64) -> f64 { return r * r * 3.14159; }
fn big(x: f64) -> bool { return x > 10.0; }
fn main() { println(big(area(2.0))); println(big(area(1.0))); }
"#,
        "true\nfalse\n",
    );
}

#[test]
fn lir_diff_04_bool_and_unary() {
    assert_lir_differential(
        r#"
fn flip(b: bool) -> bool { return !b; }
fn neg(n: i64) -> i64 { return -n; }
fn main() { println(flip(false)); println(neg(-42)); }
"#,
        "true\n42\n",
    );
}

#[test]
fn lir_diff_05_nested_calls() {
    assert_lir_differential(
        r#"
fn double(n: i64) -> i64 { return n * 2; }
fn add(a: i64, b: i64) -> i64 { return a + b; }
fn main() { println(add(double(3), double(4))); }
"#,
        "14\n",
    );
}

#[test]
fn lir_diff_06_early_returns() {
    assert_lir_differential(
        r#"
fn sign(n: i64) -> i64 {
    if n > 0 { return 1; }
    if n < 0 { return -1; }
    return 0;
}
fn main() { println(sign(9)); println(sign(-9)); println(sign(0)); }
"#,
        "1\n-1\n0\n",
    );
}

#[test]
fn lir_diff_07_prints_inside_lir_fn() {
    assert_lir_differential(
        r#"
fn show(n: i64) {
    print(n);
    println(n % 2 == 0);
}
fn main() { show(4); show(7); }
"#,
        "4true\n7false\n",
    );
}

#[test]
fn lir_diff_08_panic_call_chain_parity() {
    // The panic call-chain must include the LIR-compiled frame (`boom` called
    // from `outer`), identically to the AST path.
    let source = r#"
fn boom(n: i64) -> i64 {
    if n > 2 { panic("too big"); }
    return n;
}
fn outer(n: i64) -> i64 { return boom(n + 2); }
fn main() { println(outer(5)); }
"#;
    let (with_lir, ok_on) = compile_with_env_and_run(source, &LIR_ON);
    let (without_lir, ok_off) = compile_with_env_and_run(source, &LIR_OFF);
    assert!(!ok_on && !ok_off, "both paths must panic");
    assert_eq!(with_lir, without_lir, "panic traces must agree");
}

#[test]
fn lir_callstack_static_method_panic_has_frame_on_both_backends() {
    let source = r#"
class Crash {
    pub static fn explode() { panic("static boom"); }
}
fn invoke() { Crash::explode(); }
fn main() { invoke(); }
"#;
    let (with_lir, ok_on) = compile_with_env_and_run_combined(source, &LIR_ON);
    let (without_lir, ok_off) = compile_with_env_and_run_combined(source, &LIR_OFF);
    assert!(!ok_on && !ok_off, "both paths must panic");
    for (backend, out) in [("LIR", with_lir), ("AST", without_lir)] {
        let method = out
            .find("0: explode")
            .unwrap_or_else(|| panic!("{backend} trace has no static-method frame: {out}"));
        let caller = out
            .find("1: invoke")
            .unwrap_or_else(|| panic!("{backend} trace has no caller frame: {out}"));
        assert!(method < caller, "{backend} trace is out of order: {out}");
    }
}

#[test]
fn lir_callstack_constructor_panic_has_frame_on_both_backends() {
    let source = r#"
class Item {
    pub init(self) { panic("constructor boom"); }
}
fn build() { let item = new Item(); }
fn main() { build(); }
"#;
    let (with_lir, ok_on) = compile_with_env_and_run_combined(source, &LIR_ON);
    let (without_lir, ok_off) = compile_with_env_and_run_combined(source, &LIR_OFF);
    assert!(!ok_on && !ok_off, "both paths must panic");
    for (backend, out) in [("LIR", with_lir), ("AST", without_lir)] {
        let init = out
            .find("0: init")
            .unwrap_or_else(|| panic!("{backend} trace has no constructor frame: {out}"));
        let caller = out
            .find("1: build")
            .unwrap_or_else(|| panic!("{backend} trace has no caller frame: {out}"));
        assert!(init < caller, "{backend} trace is out of order: {out}");
    }
}

#[test]
fn lir_callstack_constructor_frame_starts_after_argument_evaluation() {
    let source = r#"
fn bad() -> i64 { return 1 / 0; }
class Item {
    pub init(self, value: i64) {}
}
fn build() { let item = new Item(bad()); }
fn main() { build(); }
"#;
    let (with_lir, ok_on) = compile_with_env_and_run_combined(source, &LIR_ON);
    let (without_lir, ok_off) = compile_with_env_and_run_combined(source, &LIR_OFF);
    assert!(!ok_on && !ok_off, "both paths must panic");
    for (backend, out) in [("LIR", with_lir), ("AST", without_lir)] {
        assert!(
            out.contains("0: bad"),
            "{backend} trace has no bad frame: {out}"
        );
        assert!(
            !out.contains("0: init") && !out.contains("1: init"),
            "{backend} trace attributed an argument panic to init: {out}"
        );
    }
}

#[test]
fn lir_diff_09_short_circuit_is_lazy() {
    // With eager evaluation `a / b` would trap on b == 0; `-1` proves the
    // short-circuit skipped the rhs on both paths.
    assert_lir_differential(
        r#"
fn safe_ratio(a: i64, b: i64) -> i64 {
    return b != 0 && a / b > 2 ? a / b : -1;
}
fn main() {
    println(safe_ratio(10, 2));
    println(safe_ratio(10, 0));
    println(false && true);
    println(true || false);
}
"#,
        "5\n-1\nfalse\ntrue\n",
    );
}

#[test]
fn lir_diff_10_ternary_branches_are_lazy() {
    assert_lir_differential(
        r#"
fn pick(c: bool, a: i64, b: i64) -> i64 { return c ? a * 2 : b * 3; }
fn main() { println(pick(true, 5, 100)); println(pick(false, 100, 5)); }
"#,
        "10\n15\n",
    );
}

#[test]
fn lir_diff_11_simple_main_compiles_from_lir() {
    // A parameterless void main in the scalar subset takes the LIR path too.
    assert_lir_differential(
        r#"
fn main() {
    let mut t = 0;
    for i in 1..6 { t = t + i; }
    println(t);
    println(t > 10 && t < 20);
}
"#,
        "15\ntrue\n",
    );
}

// ── LIR backend: GC-managed values and rooting (willow-0g8j.1) ──────────────
// `String` is the first GC-managed type the LIR walker emits. Because the LIR
// is a FLAT basic-block graph with no scopes, a GC-managed local cannot use
// the AST path's per-`let` root push/pop (that would grow the shadow root
// stack once per loop iteration). Instead each GC local gets one
// null-initialized, entry-rooted stack slot that is simultaneously its storage
// and its root, and every `return` pops the whole set.
//
// 20 perspectives, continuing the numbering of the scalar differential tests
// above (12-31). Each either compares LIR-on against LIR-off output, or runs
// the same program under WILLOW_GC_STRESS=alloc on both paths — the mode that
// collects at every allocation, so any unrooted live value is reclaimed and
// the printed text changes:
//
// 12 String param passed through and printed, 13 concat chain, 14 equality,
// 15 inequality, 16 String ternary, 17 loop accumulator, 18 mixed scalar/GC
// locals, 19 nested String calls, 20 a local live across a LATER allocation
// (the case the entry root exists for), 21 the same under GC stress, 22 loop
// accumulator under GC stress, 23 several GC locals live at once under stress,
// 24 a GC call argument rooted across a second argument's allocation,
// 25 concat lhs rooted across the rhs allocation, 26 equality operands survive
// a collection, 27 early return out of a loop leaves the root stack balanced,
// 28 deep recursion with GC locals (push/pop balance across frames), 29 a void
// function's implicit return pops its roots, 30 a GC slot still unassigned
// when a collection scans it (null-initialized, must not crash), 31 print vs
// println of a String.

/// Run `source` on both paths under GC stress and require identical, successful
/// output.
fn assert_lir_gc_stress_differential(source: &str, expected: &str) {
    let stress = [("WILLOW_GC_STRESS", "alloc")];
    let (with_lir, ok_on) = compile_with_env_and_run_under(source, &LIR_ON, &stress);
    assert!(ok_on, "LIR-enabled GC-stress run failed: {with_lir}");
    let (without_lir, ok_off) = compile_with_env_and_run_under(source, &LIR_OFF, &stress);
    assert!(ok_off, "LIR-disabled GC-stress run failed: {without_lir}");
    assert_eq!(
        with_lir, without_lir,
        "LIR and AST paths must agree under GC stress"
    );
    assert_eq!(with_lir, expected);
}

#[test]
fn lir_diff_12_string_param_roundtrip() {
    assert_lir_differential(
        r#"
fn shout(s: String) -> String { return s; }
fn main() { println(shout("hello")); }
"#,
        "hello\n",
    );
}

#[test]
fn lir_diff_13_string_concat_chain() {
    assert_lir_differential(
        r#"
fn tag(a: String, b: String) -> String { return "<" + a + "/" + b + ">"; }
fn main() { println(tag("x", "y")); }
"#,
        "<x/y>\n",
    );
}

#[test]
fn lir_diff_14_string_equality() {
    assert_lir_differential(
        r#"
fn same(a: String, b: String) -> bool { return a == b; }
fn main() {
    println(same("ab", "a" + "b"));
    println(same("ab", "ac"));
}
"#,
        "true\nfalse\n",
    );
}

#[test]
fn lir_diff_15_string_inequality() {
    assert_lir_differential(
        r#"
fn differs(a: String, b: String) -> bool { return a != b; }
fn main() {
    println(differs("ab", "a" + "b"));
    println(differs("ab", "ac"));
}
"#,
        "false\ntrue\n",
    );
}

#[test]
fn lir_diff_16_string_ternary() {
    assert_lir_differential(
        r#"
fn pick(c: bool) -> String { return c ? "yes" + "!" : "no" + "?"; }
fn main() { println(pick(true)); println(pick(false)); }
"#,
        "yes!\nno?\n",
    );
}

#[test]
fn lir_diff_17_string_loop_accumulator() {
    // The reassigned GC local: one entry root, not one per iteration.
    assert_lir_differential(
        r#"
fn rep(n: i64) -> String {
    let mut s = "";
    let mut i = 0;
    while i < n {
        s = s + "ab";
        i = i + 1;
    }
    return s;
}
fn main() { println(rep(4)); println(rep(0)); }
"#,
        "abababab\n\n",
    );
}

#[test]
fn lir_diff_18_mixed_scalar_and_gc_locals() {
    assert_lir_differential(
        r#"
fn describe(n: i64) -> String {
    let doubled = n * 2;
    let label = "n=";
    let big = doubled > 10;
    return big ? label + "big" : label + "small";
}
fn main() { println(describe(9)); println(describe(2)); }
"#,
        "n=big\nn=small\n",
    );
}

#[test]
fn lir_diff_19_nested_string_calls() {
    assert_lir_differential(
        r#"
fn wrap(s: String) -> String { return "[" + s + "]"; }
fn twice(s: String) -> String { return wrap(wrap(s)); }
fn main() { println(twice("q")); }
"#,
        "[[q]]\n",
    );
}

#[test]
fn lir_diff_20_local_live_across_later_allocation() {
    // `a` is only reachable through its slot while `b` and `c` allocate.
    assert_lir_differential(
        r#"
fn three() -> String {
    let a = "x" + "1";
    let b = "y" + "2";
    let c = "z" + "3";
    return a + b + c;
}
fn main() { println(three()); }
"#,
        "x1y2z3\n",
    );
}

#[test]
fn lir_diff_21_local_live_across_allocation_under_gc_stress() {
    // Same program as 20, but every allocation collects: without the entry
    // root `a` and `b` are reclaimed and the output is garbage.
    assert_lir_gc_stress_differential(
        r#"
fn three() -> String {
    let a = "x" + "1";
    let b = "y" + "2";
    let c = "z" + "3";
    return a + b + c;
}
fn main() {
    let mut i = 0;
    while i < 20 {
        println(three());
        i = i + 1;
    }
}
"#,
        &"x1y2z3\n".repeat(20),
    );
}

#[test]
fn lir_diff_22_loop_accumulator_under_gc_stress() {
    assert_lir_gc_stress_differential(
        r#"
fn rep(n: i64) -> String {
    let mut s = "";
    let mut i = 0;
    while i < n {
        s = s + "ab";
        i = i + 1;
    }
    return s;
}
fn main() {
    let mut k = 0;
    while k < 10 {
        println(rep(5));
        k = k + 1;
    }
}
"#,
        &"ababababab\n".repeat(10),
    );
}

#[test]
fn lir_diff_23_many_live_gc_locals_under_gc_stress() {
    assert_lir_gc_stress_differential(
        r#"
fn combine() -> String {
    let a = "a" + "1";
    let b = "b" + "2";
    let c = "c" + "3";
    let d = "d" + "4";
    let e = "e" + "5";
    return a + b + c + d + e;
}
fn main() {
    let mut i = 0;
    while i < 15 {
        println(combine());
        i = i + 1;
    }
}
"#,
        &"a1b2c3d4e5\n".repeat(15),
    );
}

#[test]
fn lir_diff_24_gc_call_argument_rooted_across_later_argument() {
    // The first argument is built, then the SECOND argument allocates: the
    // first must be a root while that happens.
    assert_lir_gc_stress_differential(
        r#"
fn join(a: String, b: String) -> String { return a + "|" + b; }
fn main() {
    let mut i = 0;
    while i < 15 {
        println(join("l" + "1", "r" + "2"));
        i = i + 1;
    }
}
"#,
        &"l1|r2\n".repeat(15),
    );
}

#[test]
fn lir_diff_25_concat_lhs_rooted_across_rhs_allocation() {
    assert_lir_gc_stress_differential(
        r#"
fn build() -> String { return ("p" + "q") + ("r" + "s"); }
fn main() {
    let mut i = 0;
    while i < 15 {
        println(build());
        i = i + 1;
    }
}
"#,
        &"pqrs\n".repeat(15),
    );
}

#[test]
fn lir_diff_26_equality_operands_survive_collection() {
    assert_lir_gc_stress_differential(
        r#"
fn matches() -> bool { return ("a" + "b") == ("a" + "b"); }
fn main() {
    let mut i = 0;
    while i < 15 {
        println(matches());
        i = i + 1;
    }
}
"#,
        &"true\n".repeat(15),
    );
}

#[test]
fn lir_diff_27_early_return_out_of_loop_balances_roots() {
    // A `return` from inside the loop must pop exactly the roots this frame
    // pushed; if it popped too few, the caller's own roots would leak and the
    // repeated calls below would drift.
    assert_lir_gc_stress_differential(
        r#"
fn first_long(n: i64) -> String {
    let prefix = "p" + "-";
    let mut i = 0;
    let mut acc = "";
    while i < n {
        acc = acc + "z";
        if i == 2 {
            return prefix + acc;
        }
        i = i + 1;
    }
    return prefix + "none";
}
fn main() {
    let mut k = 0;
    while k < 15 {
        println(first_long(10));
        println(first_long(1));
        k = k + 1;
    }
}
"#,
        &"p-zzz\np-none\n".repeat(15),
    );
}

#[test]
fn lir_diff_28_deep_recursion_with_gc_locals() {
    // Every frame pushes roots at entry and pops them at return; an imbalance
    // would either exhaust the root stack or free a live parent value.
    assert_lir_gc_stress_differential(
        r#"
fn chain(n: i64) -> String {
    let here = "n" + "!";
    if n <= 0 { return here; }
    let rest = chain(n - 1);
    return here + rest;
}
fn main() { println(chain(60)); }
"#,
        &format!("{}\n", "n!".repeat(61)),
    );
}

#[test]
fn lir_diff_29_void_fn_with_gc_locals_pops_roots() {
    // No `return` statement at all: the implicit void return has to pop.
    assert_lir_gc_stress_differential(
        r#"
fn emit(tag: String) {
    let body = tag + "-body";
    let full = body + "!";
    println(full);
}
fn main() {
    let mut i = 0;
    while i < 15 {
        emit("t");
        i = i + 1;
    }
}
"#,
        &"t-body!\n".repeat(15),
    );
}

#[test]
fn lir_diff_30_unassigned_gc_slot_is_scanned_safely() {
    // `late` is rooted from entry but only assigned after several collections
    // have already scanned its slot; the null initialization is what keeps
    // that scan from reading stack garbage.
    assert_lir_gc_stress_differential(
        r#"
fn late_binding(n: i64) -> String {
    let mut i = 0;
    let mut churn = "";
    while i < n {
        churn = churn + "c";
        i = i + 1;
    }
    let late = churn + "-late";
    return late;
}
fn main() {
    let mut k = 0;
    while k < 10 {
        println(late_binding(3));
        k = k + 1;
    }
}
"#,
        &"ccc-late\n".repeat(10),
    );
}

#[test]
fn lir_diff_31_print_and_println_of_strings() {
    assert_lir_differential(
        r#"
fn show(s: String) {
    print(s);
    print("|");
    println(s + s);
}
fn main() { show("ab"); }
"#,
        "ab|abab\n",
    );
}

// ── LIR backend: Array<T> (willow-0g8j.4) ───────────────────────────────────
// The walker now emits array literals, indexing, index-assignment and the
// builtin len/push/pop/toString. An array is a GC handle whose pointer is
// stable across growth, so an array local uses the same entry-rooted slot a
// String local does; temporaries are rooted around any sub-expression that can
// collect. Eligibility perspectives 1-15 are unit tests in
// `src/backend/cranelift/lir_gen.rs`; these are perspectives 16-38, all of them
// differential (LIR vs AST must agree), the stress-tagged ones additionally
// under `WILLOW_GC_STRESS=alloc`: 16 literal + len + index, 17 element kinds
// round-trip through the 64-bit word ABI (i64/f64/bool), 18 push past the
// initial capacity, 19 pop, 20 index assignment, 21 for-over-array, 22 array
// passed to and returned from a function, 23 toString, 24 nested arrays,
// 25 array of String built by concatenation, 26 array local live across later
// allocations (stress), 27 array of String under stress, 28 array literal whose
// elements allocate (stress), 29 index expression that allocates while the
// array is a temporary (stress), 30 index-assignment with an allocating value
// (stress), 31 push with an allocating value (stress), 32 loop that grows an
// array of String (stress), 33 early return out of a loop holding array locals
// (stress), 34 array argument rooted across a later allocating argument
// (stress), 35 out-of-bounds read panics identically on both paths,
// 36 index-assignment whose *receiver* is a temporary (stress), 37 push whose
// receiver is a temporary (stress). The last two are what actually pin the
// index-assign and push roots: with a named local receiver the entry slot
// already keeps the handle alive, so only a temporary can prove them.
// 38 `for` over an array observes a `push`/`pop` made inside the body — the
// length is re-read on every header entry, not snapshot on entry.

#[test]
fn lir_diff_32_array_literal_len_index() {
    assert_lir_differential(
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs = [10, 20, 30];
    return xs.len() + xs[0] + xs[2];
}
fn main() { println(f()); }
"#,
        "43\n",
    );
}

#[test]
fn lir_diff_33_array_element_kinds() {
    assert_lir_differential(
        r#"
import std::collections::Array;

fn ints() -> i64 { let xs = [1, 2]; return xs[1]; }
fn floats() -> f64 { let xs = [1.5, 2.5]; return xs[0] + xs[1]; }
fn bools() -> bool { let xs = [false, true]; return xs[1]; }
fn main() {
    println(ints());
    println(floats());
    println(bools());
}
"#,
        "2\n4\ntrue\n",
    );
}

#[test]
fn lir_diff_34_push_grows_past_capacity() {
    assert_lir_differential(
        r#"
import std::collections::Array;

fn build(n: i64) -> i64 {
    let mut xs = [0];
    let mut i = 0;
    while i < n {
        xs.push(i);
        i = i + 1;
    }
    return xs.len();
}
fn main() { println(build(0)); println(build(1)); println(build(9)); }
"#,
        "1\n2\n10\n",
    );
}

#[test]
fn lir_diff_35_pop() {
    assert_lir_differential(
        r#"
import std::collections::Array;

fn drain() -> i64 {
    let mut xs = [1, 2, 3];
    let a = xs.pop();
    let b = xs.pop();
    return a * 10 + b + xs.len();
}
fn main() { println(drain()); }
"#,
        "33\n",
    );
}

#[test]
fn lir_diff_36_index_assignment() {
    assert_lir_differential(
        r#"
import std::collections::Array;

fn f() -> i64 {
    let mut xs = [1, 2, 3];
    xs[0] = 100;
    xs[2] = xs[0] + xs[1];
    return xs[0] + xs[1] + xs[2];
}
fn main() { println(f()); }
"#,
        "204\n",
    );
}

#[test]
fn lir_diff_37_for_over_array() {
    assert_lir_differential(
        r#"
import std::collections::Array;

fn total(xs: Array<i64>) -> i64 {
    let mut t = 0;
    for x in xs {
        t = t + x;
    }
    return t;
}
fn main() { println(total([1, 2, 3, 4])); }
"#,
        "10\n",
    );
}

#[test]
fn lir_diff_38_array_argument_and_return() {
    assert_lir_differential(
        r#"
import std::collections::Array;

fn doubled(xs: Array<i64>) -> Array<i64> {
    let mut out = [xs[0] * 2];
    let mut i = 1;
    while i < xs.len() {
        out.push(xs[i] * 2);
        i = i + 1;
    }
    return out;
}
fn main() {
    let ys = doubled([1, 2, 3]);
    println(ys[0] + ys[1] + ys[2]);
}
"#,
        "12\n",
    );
}

#[test]
fn lir_diff_39_array_to_string() {
    assert_lir_differential(
        r#"
import std::collections::Array;

fn f() -> String {
    let mut xs = [1, 2];
    xs.push(3);
    return xs.toString();
}
fn g() -> String { let xs = ["a", "b"]; return xs.toString(); }
fn main() { println(f()); println(g()); }
"#,
        "[1, 2, 3]\n[\"a\", \"b\"]\n",
    );
}

#[test]
fn lir_diff_40_nested_arrays() {
    assert_lir_differential(
        r#"
import std::collections::Array;

fn f() -> i64 {
    let grid = [[1, 2], [3, 4]];
    return grid[0][1] + grid[1][0] + grid.len() + grid[1].len();
}
fn main() { println(f()); }
"#,
        "9\n",
    );
}

#[test]
fn lir_diff_41_array_of_built_strings() {
    assert_lir_differential(
        r#"
import std::collections::Array;

fn f() -> String {
    let mut xs = ["a" + "1", "b" + "2"];
    xs.push("c" + "3");
    return xs[0] + xs[1] + xs[2];
}
fn main() { println(f()); }
"#,
        "a1b2c3\n",
    );
}

#[test]
fn lir_diff_42_array_local_live_across_allocation_stress() {
    assert_lir_gc_stress_differential(
        r#"
import std::collections::Array;

fn churn(n: i64) -> String {
    let mut s = "";
    let mut i = 0;
    while i < n {
        s = s + "x";
        i = i + 1;
    }
    return s;
}

fn f() -> i64 {
    let xs = [1, 2, 3];
    // Every one of these allocates while `xs` must stay live.
    let a = churn(4);
    let b = churn(5);
    let ok_a = a == "xxxx" ? 10 : 0;
    let ok_b = b == "xxxxx" ? 100 : 0;
    return xs[0] + xs[1] + xs[2] + ok_a + ok_b;
}
fn main() { println(f()); }
"#,
        "116\n",
    );
}

#[test]
fn lir_diff_43_array_of_strings_stress() {
    assert_lir_gc_stress_differential(
        r#"
import std::collections::Array;

fn f(n: i64) -> String {
    let mut xs = ["seed"];
    let mut i = 0;
    while i < n {
        xs.push("v" + "0");
        i = i + 1;
    }
    let mut out = xs[0];
    let mut j = 1;
    while j < xs.len() {
        out = out + "/" + xs[j];
        j = j + 1;
    }
    return out;
}
fn main() { println(f(3)); }
"#,
        "seed/v0/v0/v0\n",
    );
}

#[test]
fn lir_diff_44_allocating_literal_elements_stress() {
    assert_lir_gc_stress_differential(
        r#"
import std::collections::Array;

fn make(tag: String) -> String { return "<" + tag + ">"; }

fn f() -> String {
    // Every element allocates, so the fresh array must be rooted for the
    // whole fill.
    let xs = [make("a"), make("b"), make("c")];
    return xs[0] + xs[1] + xs[2];
}
fn main() { println(f()); }
"#,
        "<a><b><c>\n",
    );
}

#[test]
fn lir_diff_45_allocating_index_on_temporary_stress() {
    assert_lir_gc_stress_differential(
        r#"
import std::collections::Array;

fn pick(n: i64) -> i64 { let s = "i" + "dx"; return s == "idx" ? n : n + 1; }
fn build() -> Array<i64> { return [7, 8, 9]; }

fn f() -> i64 {
    // The array is a temporary and the index expression allocates: the handle
    // has to be rooted across the index evaluation.
    return build()[pick(1)];
}
fn main() { println(f()); }
"#,
        "8\n",
    );
}

#[test]
fn lir_diff_46_index_assign_allocating_value_stress() {
    assert_lir_gc_stress_differential(
        r#"
import std::collections::Array;

fn f() -> String {
    let mut xs = ["a", "b"];
    xs[1] = "c" + "d";
    return xs[0] + xs[1];
}
fn main() { println(f()); }
"#,
        "acd\n",
    );
}

#[test]
fn lir_diff_47_push_allocating_value_stress() {
    assert_lir_gc_stress_differential(
        r#"
import std::collections::Array;

fn f(n: i64) -> i64 {
    let mut xs = ["s"];
    let mut i = 0;
    while i < n {
        xs.push("p" + "q");
        i = i + 1;
    }
    let mut total = 0;
    let mut j = 0;
    while j < xs.len() {
        if xs[j] == "pq" {
            total = total + 1;
        }
        j = j + 1;
    }
    return total;
}
fn main() { println(f(4)); }
"#,
        "4\n",
    );
}

#[test]
fn lir_diff_48_many_live_arrays_stress() {
    assert_lir_gc_stress_differential(
        r#"
import std::collections::Array;

fn f() -> i64 {
    let a = ["a" + "1"];
    let b = ["b" + "2"];
    let c = [1, 2, 3];
    let d = ["d" + "4"];
    let e = [c[0] + 1, c[2] + 1];
    let mut t = e[0] + e[1];
    if a[0] == "a1" { t = t + 1; }
    if b[0] == "b2" { t = t + 10; }
    if d[0] == "d4" { t = t + 100; }
    return t;
}
fn main() { println(f()); }
"#,
        "117\n",
    );
}

#[test]
fn lir_diff_49_early_return_with_array_locals_stress() {
    assert_lir_gc_stress_differential(
        r#"
import std::collections::Array;

fn find(limit: i64) -> String {
    let names = ["a" + "a", "b" + "b", "c" + "c"];
    let mut i = 0;
    while i < limit {
        if i == 1 {
            // Early return out of a loop: exactly this frame's roots pop.
            return names[i] + "!";
        }
        i = i + 1;
    }
    return names[0];
}
fn main() { println(find(5)); println(find(1)); }
"#,
        "bb!\naa\n",
    );
}

#[test]
fn lir_diff_50_array_argument_rooted_across_later_argument_stress() {
    assert_lir_gc_stress_differential(
        r#"
import std::collections::Array;

fn combine(xs: Array<String>, tag: String) -> String { return xs[0] + tag; }
fn make(n: i64) -> Array<String> { return ["e" + "0"]; }

fn f() -> String {
    // The first argument is a fresh array; building the second one allocates
    // before the call, so the array has to be rooted meanwhile.
    return combine(make(1), "t" + "1");
}
fn main() { println(f()); }
"#,
        "e0t1\n",
    );
}

#[test]
fn lir_diff_54_for_observes_mutation_during_iteration() {
    // The loop bound is re-read on every header entry on both paths: a `push`
    // in the body extends the walk, a `pop` cuts it short.
    assert_lir_differential(
        r#"
import std::collections::Array;

fn grow() -> i64 {
    let xs = [1];
    let mut n = 0;
    for x in xs {
        n = n + 1;
        if n < 3 {
            xs.push(n * 10);
        }
    }
    return n * 100 + xs.len();
}

fn shrink() -> i64 {
    let xs = [1, 2, 3, 4, 5, 6];
    let mut n = 0;
    for x in xs {
        n = n + 1;
        xs.pop();
    }
    return n;
}
fn main() { println(grow()); println(shrink()); }
"#,
        "303\n3\n",
    );
}

#[test]
fn lir_diff_52_index_assign_on_temporary_array_stress() {
    assert_lir_gc_stress_differential(
        r#"
import std::collections::Array;

fn build() -> Array<String> { return ["a", "b"]; }
fn mk() -> String { return "c" + "d"; }

fn f() -> i64 {
    // The assigned-to array is a temporary nothing else roots, and the value
    // expression allocates: without a root the write lands in freed memory.
    build()[0] = mk();
    return 1;
}
fn main() { println(f()); }
"#,
        "1\n",
    );
}

#[test]
fn lir_diff_53_push_on_temporary_array_stress() {
    assert_lir_gc_stress_differential(
        r#"
import std::collections::Array;

fn build() -> Array<String> { return ["a", "b"]; }
fn mk() -> String { return "c" + "d"; }

fn f() -> i64 {
    // Same shape for `push`: the receiver is a temporary and the pushed value
    // allocates before the runtime call takes over rooting.
    build().push(mk());
    return 1;
}
fn main() { println(f()); }
"#,
        "1\n",
    );
}

#[test]
fn lir_diff_51_out_of_bounds_panics_identically() {
    let source = r#"
import std::collections::Array;

fn f(i: i64) -> i64 { let xs = [1, 2]; return xs[i]; }
fn main() { println(f(5)); }
"#;
    let (with_lir, ok_on) = compile_with_env_and_run_combined(source, &LIR_ON);
    let (without_lir, ok_off) = compile_with_env_and_run_combined(source, &LIR_OFF);
    assert!(
        !ok_on,
        "out-of-bounds must fail on the LIR path: {with_lir}"
    );
    assert!(
        !ok_off,
        "out-of-bounds must fail on the AST path: {without_lir}"
    );
    // The panic text names the (temporary) source file, so compare everything
    // else: the message, and the call-stack frame the panic is attributed to.
    for out in [&with_lir, &without_lir] {
        assert!(
            out.contains("array index out of bounds: the length is 2 but the index is 5"),
            "unexpected panic text: {out}"
        );
        assert!(out.contains("0: f at"), "missing call frame: {out}");
    }
}

// ── Class objects in the LIR walker (willow-0g8j.5) ─────────────────────────
// The walker claims functions that create, read, mutate and pass "simple"
// class objects: no base class, not itself a base, no interface or enum field.
// Class METHOD bodies still compile through `compile_class_method_inner`, which
// the `WILLOW_LIR_REQUIRE` gate does not police — so in the programs below the
// gate pins the free functions and `main`, which are the ones exercising the
// new object code paths.
//
// Perspectives 27-46 (1-26 are the eligibility unit tests in `lir_gen.rs`):
// 25 memberwise `new` + field read, 26 explicit `init` constructor, 27 field
// assignment is observed through an alias, 28 chained field access, 29
// instance method call, 30 static method call returning an object, 31 a method
// with arguments and a class-typed return, 32 a `String` field roundtrip, 33
// an `Array<T>` field, 34 objects as call arguments and return values, 35 an
// array of objects driven by `for`, 36 an object-typed field re-pointed at
// another object, 37 an object local live across a later allocation under GC
// stress, 38 a `String` field rewritten under GC stress (object-field write
// path), 39 many live objects under GC stress, 40 objects allocated in a loop
// under GC stress, 41 an object argument rooted across a later allocating
// argument, 42 an early return with class locals balances the root stack, 43 a
// field read feeding an allocating concatenation under GC stress, 44 a base
// class imported by NAME (`import zoo::Animal;`) still dispatches virtually,
// 45 the same through a module-qualified receiver, 46 an imported LEAF class
// keeps working (the identity check must not over-reject).

#[test]
fn lir_diff_55_new_and_field_read() {
    assert_lir_differential(
        r#"
class Point { pub x: i64; pub y: i64; }
fn sum(p: Point) -> i64 { return p.x + p.y; }
fn main() { println(sum(new Point(3, 4))); }
"#,
        "7\n",
    );
}

#[test]
fn lir_diff_56_explicit_init_constructor() {
    assert_lir_differential(
        r#"
class Counter {
    pub n: i64;
    pub init(self, start: i64) { self.n = start + 1; }
}
fn value(c: Counter) -> i64 { return c.n; }
fn main() { println(value(new Counter(41))); }
"#,
        "42\n",
    );
}

#[test]
fn lir_diff_57_field_assignment_seen_through_alias() {
    // Objects are handles: mutating through one binding must be visible
    // through the other, i.e. the walker must store into the object, not into
    // a copy of it.
    assert_lir_differential(
        r#"
class Point { pub x: i64; pub y: i64; }
fn main() {
    let p = new Point(1, 2);
    let q = p;
    p.x = 10;
    q.y = 20;
    println(p.x + p.y);
    println(q.x + q.y);
}
"#,
        "30\n30\n",
    );
}

#[test]
fn lir_diff_58_chained_field_access() {
    assert_lir_differential(
        r#"
class Inner { pub v: i64; }
class Outer { pub inner: Inner; }
fn deep(o: Outer) -> i64 { return o.inner.v; }
fn main() {
    let o = new Outer(new Inner(5));
    println(deep(o));
    o.inner.v = 9;
    println(deep(o));
}
"#,
        "5\n9\n",
    );
}

#[test]
fn lir_diff_59_instance_method_call() {
    assert_lir_differential(
        r#"
class Counter {
    pub n: i64;
    pub fn get(self) -> i64 { return self.n; }
    pub fn plus(self, k: i64) -> i64 { return self.n + k; }
}
fn call(c: Counter) -> i64 { return c.get() + c.plus(10); }
fn main() { println(call(new Counter(1))); }
"#,
        "12\n",
    );
}

#[test]
fn lir_diff_60_static_method_call_returning_object() {
    assert_lir_differential(
        r#"
class Counter {
    pub n: i64;
    pub static fn zero() -> Counter { return new Counter(0); }
    pub fn get(self) -> i64 { return self.n; }
}
fn make() -> Counter { return Counter::zero(); }
fn main() { println(make().get()); }
"#,
        "0\n",
    );
}

#[test]
fn lir_diff_61_method_with_args_and_object_return() {
    assert_lir_differential(
        r#"
class Point {
    pub x: i64;
    pub y: i64;
    pub fn shifted(self, dx: i64, dy: i64) -> Point {
        return new Point(self.x + dx, self.y + dy);
    }
}
fn move_twice(p: Point) -> Point { return p.shifted(1, 2).shifted(3, 4); }
fn main() {
    let p = move_twice(new Point(0, 0));
    println(p.x);
    println(p.y);
}
"#,
        "4\n6\n",
    );
}

#[test]
fn lir_diff_62_string_field_roundtrip() {
    assert_lir_differential(
        r#"
class Item { pub name: String; }
fn label(i: Item) -> String { return i.name; }
fn main() {
    let i = new Item("alpha");
    println(label(i));
    i.name = "beta" + "!";
    println(label(i));
}
"#,
        "alpha\nbeta!\n",
    );
}

#[test]
fn lir_diff_63_array_field() {
    assert_lir_differential(
        r#"
import std::collections::Array;

class Bag { pub xs: Array<i64>; }
fn total(b: Bag) -> i64 {
    let mut t = 0;
    for x in b.xs { t = t + x; }
    return t;
}
fn main() {
    let b = new Bag([1, 2, 3]);
    println(total(b));
    b.xs.push(4);
    println(total(b));
}
"#,
        "6\n10\n",
    );
}

#[test]
fn lir_diff_64_object_argument_and_return() {
    assert_lir_differential(
        r#"
class Point { pub x: i64; pub y: i64; }
fn swap(p: Point) -> Point { return new Point(p.y, p.x); }
fn show(p: Point) { println(p.x); println(p.y); }
fn main() { show(swap(new Point(1, 2))); }
"#,
        "2\n1\n",
    );
}

#[test]
fn lir_diff_65_array_of_objects_for_loop() {
    assert_lir_differential(
        r#"
import std::collections::Array;

class Point { pub x: i64; pub y: i64; }
fn total(ps: Array<Point>) -> i64 {
    let mut t = 0;
    for p in ps { t = t + p.x * p.y; }
    return t;
}
fn main() { println(total([new Point(1, 2), new Point(3, 4)])); }
"#,
        "14\n",
    );
}

#[test]
fn lir_diff_66_object_field_repointed() {
    // Storing an OBJECT into an object field goes through the same heap-store
    // path as a string field, and re-pointing must be visible on the next read.
    assert_lir_differential(
        r#"
class Point { pub x: i64; pub y: i64; }
class Pair { pub a: Point; pub b: Point; }
fn total(p: Pair) -> i64 { return p.a.x + p.b.x; }
fn main() {
    let p = new Pair(new Point(1, 2), new Point(3, 4));
    println(total(p));
    p.a = p.b;
    println(total(p));
    p.b = new Point(9, 9);
    println(total(p));
}
"#,
        "4\n6\n12\n",
    );
}

#[test]
fn lir_diff_67_object_local_live_across_allocation_stress() {
    // `p` must survive the collections triggered by the later `new`s: its entry
    // root slot is the only thing keeping it reachable.
    assert_lir_gc_stress_differential(
        r#"
class Point { pub x: i64; pub y: i64; }
fn build() -> i64 {
    let p = new Point(1, 2);
    let q = new Point(3, 4);
    let r = new Point(5, 6);
    return p.x + q.y + r.x;
}
fn main() {
    let mut i = 0;
    while i < 20 { println(build()); i = i + 1; }
}
"#,
        &"10\n".repeat(20),
    );
}

#[test]
fn lir_diff_68_string_field_rewritten_under_stress() {
    // Storing a GC value into an object field goes through the heap-store path
    // (write barrier + rooting), not a bare store.
    assert_lir_gc_stress_differential(
        r#"
class Item { pub name: String; }
fn main() {
    let i = new Item("a");
    let mut n = 0;
    while n < 20 {
        i.name = i.name + "b";
        n = n + 1;
    }
    println(i.name);
}
"#,
        "abbbbbbbbbbbbbbbbbbbb\n",
    );
}

#[test]
fn lir_diff_69_many_live_objects_under_stress() {
    assert_lir_gc_stress_differential(
        r#"
class Item { pub name: String; }
fn six() -> String {
    let a = new Item("a");
    let b = new Item("b");
    let c = new Item("c");
    let d = new Item("d");
    let e = new Item("e");
    let f = new Item("f");
    return a.name + b.name + c.name + d.name + e.name + f.name;
}
fn main() {
    let mut i = 0;
    while i < 10 { println(six()); i = i + 1; }
}
"#,
        &"abcdef\n".repeat(10),
    );
}

#[test]
fn lir_diff_70_objects_allocated_in_a_loop_under_stress() {
    // One entry root slot per local, reused every iteration: the shadow stack
    // must not grow with the loop.
    assert_lir_gc_stress_differential(
        r#"
class Point { pub x: i64; pub y: i64; }
fn main() {
    let mut i = 0;
    let mut t = 0;
    while i < 50 {
        let p = new Point(i, i + 1);
        t = t + p.y - p.x;
        i = i + 1;
    }
    println(t);
}
"#,
        "50\n",
    );
}

#[test]
fn lir_diff_71_object_argument_rooted_across_later_argument_stress() {
    // The object is evaluated first, then the string argument allocates: the
    // already-built object must be rooted while that happens.
    assert_lir_gc_stress_differential(
        r#"
class Item { pub name: String; }
fn tag(i: Item, s: String) -> String { return s + i.name; }
fn main() {
    let mut i = 0;
    while i < 20 {
        println(tag(new Item("p"), "q" + "="));
        i = i + 1;
    }
}
"#,
        &"q=p\n".repeat(20),
    );
}

#[test]
fn lir_diff_72_early_return_with_object_locals_stress() {
    // Every `return` pops the whole entry root frame, including the returns
    // taken from inside a loop.
    assert_lir_gc_stress_differential(
        r#"
class Item { pub name: String; }
fn find(n: i64) -> String {
    let a = new Item("a");
    let b = new Item("b");
    let mut i = 0;
    while i < 10 {
        if i == n { return a.name + b.name + "!"; }
        i = i + 1;
    }
    return a.name;
}
fn main() {
    let mut k = 0;
    while k < 12 { println(find(k)); k = k + 1; }
}
"#,
        &format!("{}{}", "ab!\n".repeat(10), "a\n".repeat(2)),
    );
}

#[test]
fn lir_diff_73_field_read_feeding_concat_under_stress() {
    // The loaded field value is a fresh temporary with no home: it must be
    // rooted across the allocation performed by the concatenation.
    assert_lir_gc_stress_differential(
        r#"
class Item { pub name: String; }
fn join(a: Item, b: Item) -> String { return a.name + ("-" + b.name); }
fn main() {
    let mut i = 0;
    while i < 20 {
        println(join(new Item("x"), new Item("y")));
        i = i + 1;
    }
}
"#,
        &"x-y\n".repeat(20),
    );
}

/// Both backends must agree on a multi-file project. `LIR_ON_MIXED` rather than
/// `LIR_ON`: these programs deliberately contain functions the walker refuses,
/// and that refusal is the thing under test.
fn assert_lir_project_differential(files: &[(&str, &str)], entry: &str, expected: &str) {
    let (with_lir, ok_on) = compile_temp_project_with_env_and_run(files, entry, &LIR_ON_MIXED);
    assert!(ok_on, "LIR-enabled run failed: {with_lir}");
    let (without_lir, ok_off) = compile_temp_project_with_env_and_run(files, entry, &LIR_OFF);
    assert!(ok_off, "LIR-disabled run failed: {without_lir}");
    assert_eq!(with_lir, without_lir, "LIR and AST paths must agree");
    assert_eq!(with_lir, expected);
}

/// A module whose public `Animal` is `open` and extended by `Dog`.
const ZOO_MODULE: &str = r#"
module zoo;

pub open class Animal {
    pub value: i64;
    pub open fn speak(self) -> i64 { return self.value; }
}

pub class Dog extends Animal {
    pub override fn speak(self) -> i64 { return self.value + 1000; }
}
"#;

#[test]
fn lir_diff_74_imported_base_class_still_dispatches_virtually() {
    // `import zoo::Animal;` registers the class under the bare name `Animal`
    // too, while `class_base` keeps canonical names (`zoo::Dog` -> `zoo::Animal`
    // ). A name-only "is anything extending me?" test misses that, calls
    // `Animal` a leaf and emits a DIRECT `Animal__speak` — so a `Dog` passed as
    // an `Animal` would print 5 instead of 1005.
    assert_lir_project_differential(
        &[
            ("zoo.wi", ZOO_MODULE),
            (
                "main.wi",
                r#"
import zoo::Animal;
import zoo::Dog;

fn speak(a: Animal) -> i64 { return a.speak(); }

fn main() {
    println(speak(new Dog(5)));
    println(speak(new Animal(7)));
}
"#,
            ),
        ],
        "main.wi",
        "1005\n7\n",
    );
}

#[test]
fn lir_diff_75_module_qualified_base_class_dispatches_virtually() {
    // Same class, reached under its canonical name instead of an alias.
    assert_lir_project_differential(
        &[
            ("zoo.wi", ZOO_MODULE),
            (
                "main.wi",
                r#"
import zoo;

fn speak(a: zoo::Animal) -> i64 { return a.speak(); }

fn main() {
    println(speak(new zoo::Dog(5)));
    println(speak(new zoo::Animal(7)));
}
"#,
            ),
        ],
        "main.wi",
        "1005\n7\n",
    );
}

#[test]
fn lir_diff_76_imported_leaf_class_still_works() {
    // The identity check must reject only classes that really take part in an
    // `extends` edge: an imported LEAF class stays eligible and keeps working.
    assert_lir_project_differential(
        &[
            (
                "shapes.wi",
                r#"
module shapes;

pub class Point {
    pub x: i64;
    pub y: i64;
    pub fn sum(self) -> i64 { return self.x + self.y; }
}
"#,
            ),
            (
                "main.wi",
                r#"
import shapes::Point;

fn total(p: Point) -> i64 { return p.x + p.y + p.sum(); }

fn main() { println(total(new Point(3, 4))); }
"#,
            ),
        ],
        "main.wi",
        "14\n",
    );
}

// ── LIR walker: class -> interface boxing (willow-j260) ─────────────────────
// An interface-typed slot does not hold a class pointer, it holds a 16-byte GC
// box `[object | vtable]`. So every store of a class value into an interface
// position is a conversion the walker has to emit, and — because the box
// allocates AFTER the value expression has already produced a live object —
// every such site also has to count as allocating for rooting purposes.
//
// Perspectives j01-j21 are the eligibility half, in
// `src/backend/cranelift/lir_gen.rs`. j22-j36 below are the emitted-code half:
// one store site per test, first as an LIR-on/LIR-off differential and then
// under WILLOW_GC_STRESS=alloc, where a value left unrooted across the box
// allocation is reclaimed and the printed text changes.
//
// j22 widening `let`, j23 widening assignment, j24 boxed call argument,
// j25 boxed `return`, j26 boxed field store, j27 memberwise `new`,
// j28 explicit `init`, j29 boxed index-assign, j30 boxed `push`,
// j31 an interface value re-stored (must NOT be boxed twice), then under GC
// stress: j32 boxes built in a loop, j33 the owner rooted across a field
// store's box, j34 a boxed argument rooted across a later allocating argument,
// j35 the array handle rooted across a pushed box, j36 early return with live
// boxes leaves the root stack balanced.
//
// j37-j50 (willow-j260.1) cover the case the tests above never reach: ONE
// object boxed into TWO DIFFERENT interfaces. That splits into two properties
// nothing else pins down.
//
//   * vtable SELECTION. `resolve_vtable_id` is keyed `(class, interface)`, so
//     one class has as many vtables as it has interfaces. A regression that
//     resolved per-CLASS would still satisfy every j22-j36 test, because each
//     of those classes is behind exactly one interface. `Ends` below makes the
//     mistake observable: `Front` and `Back` declare the SAME two methods in
//     OPPOSITE order, so the wrong vtable answers `head()` with the tail.
//
//   * shared IDENTITY. A box copies a pointer, it does not copy the object, so
//     two boxes over one object must still see one set of fields. Mutating
//     through one box and reading back through the other is the proof; run
//     under GC stress it also proves the object survived the SECOND box's
//     allocation, which happens while the first box and the half-initialized
//     owner are already live.
//
// j37 two interfaces in one memberwise `new`, j38 mutate through one box and
// read through the other, j39 reversed slot order, j40 boxing order reversed,
// j41 two interface parameters at one call site, j42 two widening `let`s,
// j43 the same object boxed TWICE into the SAME interface, j44 re-pointing one
// field leaves the other's object alone, j45 one object in two differently
// typed arrays, j46 two interface-returning functions over one object; then
// under WILLOW_GC_STRESS=alloc: j47 identity survives the second box's
// allocation in a loop, j48 the first box and the half-initialized owner stay
// rooted across the second box, j49 the reversed-slot pair built in a loop,
// j50 two boxes of one object as two allocating call arguments.

/// Shared shape for the boxing tests. The only way to OBSERVE a box is to read
/// back through it, and when these tests were written a virtual `name()` call
/// was still outside the walker — so every read happens inside a class METHOD,
/// which `WILLOW_LIR_REQUIRE` does not police. Dispatch has since joined the
/// subset (willow-0g8j.6, the k24+ block below); the reads stay in methods here
/// so these tests keep isolating the STORE side.
const BOXING_PRELUDE: &str = r##"
import std::collections::Array;

interface Named { fn name(self) -> String; }

class Item implements Named {
    pub label: String;
    pub fn name(self) -> String { return self.label; }
}

class Tag implements Named {
    pub n: i64;
    pub fn name(self) -> String { return "#" + self.n.toString(); }
}

class Cell {
    pub v: Named;
    pub fn read(self) -> String { return self.v.name(); }
}

class Row {
    pub xs: Array<Named>;
    pub fn joined(self) -> String {
        let mut o = "";
        let mut i = 0;
        while i < self.xs.len() { o = o + self.xs[i].name(); i = i + 1; }
        return o;
    }
}

fn named(s: String) -> Named { return new Item(s); }

// One class behind TWO interfaces (j37+). `Ticker` slot 0 is `tick`, `Counted`
// slot 0 is `count`, so a box that carried the wrong vtable would call the
// wrong function through the same slot index.
interface Ticker { fn tick(self); }
interface Counted { fn count(self) -> i64; }

class Meter implements Ticker, Counted {
    pub hits: i64;
    pub fn tick(self) { self.hits = self.hits + 1; }
    pub fn count(self) -> i64 { return self.hits; }
}

// Holds ONE object twice, once behind each interface.
class TwoWay {
    pub ticker: Ticker;
    pub reader: Counted;

    // Mutate through one box, read back through the OTHER. The answer is only
    // right if both boxes hold the same concrete object.
    pub fn bumpThenRead(self, times: i64) -> i64 {
        let mut i = 0;
        while i < times { self.ticker.tick(); i = i + 1; }
        return self.reader.count();
    }

    pub fn readOnly(self) -> i64 { return self.reader.count(); }
}

// The same two methods declared in OPPOSITE order, so the two interfaces
// disagree about which slot holds which method.
interface Front { fn head(self) -> String; fn tail(self) -> String; }
interface Back { fn tail(self) -> String; fn head(self) -> String; }

class Ends implements Front, Back {
    pub a: String;
    pub b: String;
    pub fn head(self) -> String { return self.a; }
    pub fn tail(self) -> String { return self.b; }
}

class Pair {
    pub front: Front;
    pub back: Back;

    // Reads each method through BOTH interfaces: a per-class vtable would make
    // the two halves disagree.
    pub fn crossed(self) -> String {
        return self.front.head() + self.back.head()
             + self.front.tail() + self.back.tail();
    }
}

class Meters {
    pub tickers: Array<Ticker>;
    pub readers: Array<Counted>;

    // Bump every element of one array, then total the OTHER array — which
    // holds the same objects behind different boxes.
    pub fn bumpAllThenTotal(self) -> i64 {
        let mut i = 0;
        while i < self.tickers.len() { self.tickers[i].tick(); i = i + 1; }
        let mut total = 0;
        let mut k = 0;
        while k < self.readers.len() { total = total + self.readers[k].count(); k = k + 1; }
        return total;
    }
}
"##;

fn boxing_source(body: &str) -> String {
    format!("{BOXING_PRELUDE}{body}")
}

#[test]
fn lir_diff_j22_widening_let_init() {
    // `let x: Named = new Item(..)` — the annotation widens, so the initializer
    // is boxed before it reaches the local's slot.
    assert_lir_differential(
        &boxing_source(
            r#"
fn wrap(s: String) -> String {
    let x: Named = new Item(s);
    return new Cell(x).read();
}
fn main() { println(wrap("a")); }
"#,
        ),
        "a\n",
    );
}

#[test]
fn lir_diff_j23_widening_assignment() {
    // The slot already exists with the interface type; the assignment must be
    // boxed against the SLOT's type, not the value's.
    assert_lir_differential(
        &boxing_source(
            r#"
fn swap(s: String, n: i64) -> String {
    let mut x: Named = new Item(s);
    let first = new Cell(x).read();
    x = new Tag(n);
    return first + "/" + new Cell(x).read();
}
fn main() { println(swap("a", 7)); }
"#,
        ),
        "a/#7\n",
    );
}

#[test]
fn lir_diff_j24_boxed_call_argument() {
    // The callee declares `Named`; the caller passes an `Item`. The box happens
    // at the call site, per argument.
    assert_lir_differential(
        &boxing_source(
            r#"
fn show(n: Named) -> String { return new Cell(n).read(); }
fn main() { println(show(new Item("a"))); println(show(new Tag(7))); }
"#,
        ),
        "a\n#7\n",
    );
}

#[test]
fn lir_diff_j25_boxed_return() {
    // `return new Item(..)` out of an interface-returning function boxes on the
    // way out, while the function's own roots are still live.
    assert_lir_differential(
        &boxing_source(
            r#"
fn make(s: String) -> Named {
    let keep = s + "!";
    return new Item(keep);
}
fn main() { println(new Cell(make("a")).read()); }
"#,
        ),
        "a!\n",
    );
}

#[test]
fn lir_diff_j26_boxed_field_store() {
    // Storing into an interface-typed field goes through the object-field write
    // path with the value boxed first.
    assert_lir_differential(
        &boxing_source(
            r#"
fn rewrite(s: String, n: i64) -> String {
    let c = new Cell(new Item(s));
    let before = c.read();
    c.v = new Tag(n);
    return before + "/" + c.read();
}
fn main() { println(rewrite("a", 7)); }
"#,
        ),
        "a/#7\n",
    );
}

#[test]
fn lir_diff_j27_memberwise_new_widens_each_field() {
    // The implicit memberwise constructor boxes each argument into its declared
    // field type.
    assert_lir_differential(
        &boxing_source(
            r#"
fn build(s: String) -> String { return new Cell(new Item(s)).read(); }
fn main() { println(build("a")); println(new Cell(new Tag(7)).read()); }
"#,
        ),
        "a\n#7\n",
    );
}

#[test]
fn lir_diff_j28_explicit_init_widens_parameter() {
    // Same store, reached through a declared `init` instead of the memberwise
    // constructor: the box now happens at the init call's argument.
    assert_lir_differential(
        &boxing_source(
            r#"
class Wrapped {
    pub v: Named;
    pub init(self, v: Named) { self.v = v; }
    pub fn read(self) -> String { return self.v.name(); }
}
fn build(s: String) -> String { return new Wrapped(new Item(s)).read(); }
fn main() { println(build("a")); }
"#,
        ),
        "a\n",
    );
}

#[test]
fn lir_diff_j29_boxed_index_assign() {
    // An `Array<Named>` slot holds boxes, so an index-assign of a class value
    // boxes per ELEMENT.
    assert_lir_differential(
        &boxing_source(
            r#"
fn replace(a: String, b: String) -> String {
    let xs: Array<Named> = [named(a), named(b)];
    xs[0] = new Tag(7);
    return new Row(xs).joined();
}
fn main() { println(replace("a", "b")); }
"#,
        ),
        "#7b\n",
    );
}

#[test]
fn lir_diff_j30_boxed_push() {
    // `push` takes the array's element type, so the same coercion applies.
    assert_lir_differential(
        &boxing_source(
            r#"
fn grow(a: String) -> String {
    let xs: Array<Named> = [named(a)];
    xs.push(new Item("b"));
    xs.push(new Tag(7));
    return new Row(xs).joined();
}
fn count(a: String) -> i64 {
    let xs: Array<Named> = [named(a)];
    xs.push(new Tag(7));
    return xs.len();
}
fn main() { println(grow("a")); println(count("a")); }
"#,
        ),
        "ab#7\n2\n",
    );
}

#[test]
fn lir_diff_j31_interface_value_restored_is_not_reboxed() {
    // A value that ALREADY has the interface representation must be stored as
    // is — boxing it again would produce a box whose payload is a box, and the
    // virtual call would then dispatch on the wrong object.
    assert_lir_differential(
        &boxing_source(
            r#"
fn passthrough(n: Named) -> Named {
    let x: Named = n;
    let c = new Cell(x);
    c.v = x;
    return c.v;
}
fn main() { println(new Cell(passthrough(named("a"))).read()); }
"#,
        ),
        "a\n",
    );
}

#[test]
fn lir_diff_j32_boxes_built_in_a_loop_under_stress() {
    // One entry root slot per local covers every box the loop re-points it at,
    // so the shadow stack does not grow and no box is left unrooted.
    assert_lir_gc_stress_differential(
        &boxing_source(
            r#"
fn cycle(times: i64) -> String {
    let mut x: Named = new Item("seed");
    let mut i = 0;
    while i < times {
        x = new Tag(i);
        i = i + 1;
    }
    return new Cell(x).read();
}
fn main() {
    let mut k = 0;
    while k < 10 { println(cycle(5)); k = k + 1; }
}
"#,
        ),
        &"#4\n".repeat(10),
    );
}

#[test]
fn lir_diff_j33_field_owner_rooted_across_box_allocation_stress() {
    // The owner object is produced BEFORE the box allocates. Rooting decided
    // from the value expression alone would miss this: `new Tag(n)` is the
    // value, but the box is a second allocation layered on top of it.
    assert_lir_gc_stress_differential(
        &boxing_source(
            r#"
fn rewrite(n: i64) -> String {
    let c = new Cell(new Item("a"));
    c.v = new Tag(n);
    return c.read();
}
fn main() {
    let mut i = 0;
    while i < 20 { println(rewrite(i)); i = i + 1; }
}
"#,
        ),
        &(0..20)
            .map(|i| format!("#{i}\n"))
            .collect::<Vec<_>>()
            .join(""),
    );
}

#[test]
fn lir_diff_j34_boxed_argument_rooted_across_later_argument_stress() {
    // The first argument's box is built first, then the second argument
    // allocates: the box itself — not the object inside it — has to be rooted
    // while that happens.
    assert_lir_gc_stress_differential(
        &boxing_source(
            r#"
fn pair(a: Named, s: String) -> String { return new Cell(a).read() + s; }
fn main() {
    let mut i = 0;
    while i < 20 {
        println(pair(new Item("x"), "-" + "y"));
        i = i + 1;
    }
}
"#,
        ),
        &"x-y\n".repeat(20),
    );
}

#[test]
fn lir_diff_j35_array_handle_rooted_across_pushed_box_stress() {
    // The array handle is live across the push's box allocation, and so is the
    // handle held by the `Row` built afterwards.
    assert_lir_gc_stress_differential(
        &boxing_source(
            r#"
fn fill(times: i64) -> String {
    let xs: Array<Named> = [named("s")];
    let mut i = 0;
    while i < times {
        xs.push(new Tag(i));
        i = i + 1;
    }
    return new Row(xs).joined();
}
fn main() {
    let mut k = 0;
    while k < 10 { println(fill(3)); k = k + 1; }
}
"#,
        ),
        &"s#0#1#2\n".repeat(10),
    );
}

#[test]
fn lir_diff_j36_early_return_with_live_boxes_stress() {
    // Boxes are ordinary GC locals: a `return` taken from inside a loop pops
    // the whole entry root frame, boxed slots included.
    assert_lir_gc_stress_differential(
        &boxing_source(
            r#"
fn pick(n: i64) -> String {
    let a: Named = new Item("a");
    let b: Named = new Tag(9);
    let mut i = 0;
    while i < 10 {
        if i == n { return new Cell(a).read() + new Cell(b).read() + "!"; }
        i = i + 1;
    }
    return new Cell(a).read();
}
fn main() {
    let mut k = 0;
    while k < 12 { println(pick(k)); k = k + 1; }
}
"#,
        ),
        &format!("{}{}", "a#9!\n".repeat(10), "a\n".repeat(2)),
    );
}

// ── one object, two interfaces (willow-j260.1) ─────────────────────────────

#[test]
fn lir_diff_j37_one_object_boxed_into_two_interfaces_in_one_new() {
    // `new TwoWay(m, m)` boxes the SAME `Meter` twice inside ONE construction,
    // once as a `Ticker` and once as a `Counted`. The two boxes must carry
    // DIFFERENT vtables: slot 0 of `Ticker` is `tick`, slot 0 of `Counted` is
    // `count`, so a vtable resolved per-class instead of per-(class,interface)
    // would send one of the two calls into the wrong function.
    assert_lir_differential(
        &boxing_source(
            r#"
fn twoWays(start: i64) -> TwoWay {
    let m = new Meter(start);
    return new TwoWay(m, m);
}
fn main() {
    println(twoWays(0).bumpThenRead(3));
    println(twoWays(10).bumpThenRead(1));
}
"#,
        ),
        "3\n11\n",
    );
}

#[test]
fn lir_diff_j38_two_boxes_share_one_concrete_object() {
    // Identity, checked from OUTSIDE the boxes: the caller keeps the concrete
    // `Meter`, the ticks go through an interface box, and the direct field read
    // has to see them. A box that copied the object would leave `m.hits` at 0.
    // The two readings are packed as `interface * 100 + direct` so one number
    // shows both; `.toString()` on a free function's value is outside the LIR
    // subset, so the tests here compare integers rather than formatted text.
    assert_lir_differential(
        &boxing_source(
            r#"
fn checkIdentity(times: i64) -> i64 {
    let m = new Meter(0);
    let w = new TwoWay(m, m);
    let through = w.bumpThenRead(times);
    return through * 100 + m.hits;
}
fn main() {
    let mut i = 0;
    while i < 4 { println(checkIdentity(i)); i = i + 1; }
}
"#,
        ),
        "0\n101\n202\n303\n",
    );
}

#[test]
fn lir_diff_j39_reversed_slot_order_between_the_two_interfaces() {
    // `Front` and `Back` declare the same two methods in OPPOSITE order, so the
    // two vtables for `Ends` differ only in their entry order. This is the
    // sharpest form of the selection question: pick the wrong one and `head()`
    // returns the tail. Expected `HHTT`, not `HTHT` or `HTTH`.
    assert_lir_differential(
        &boxing_source(
            r#"
fn ends(a: String, b: String) -> Pair {
    let e = new Ends(a, b);
    return new Pair(e, e);
}
fn main() {
    println(ends("H", "T").crossed());
    println(ends("x", "y").crossed());
}
"#,
        ),
        "HHTT\nxxyy\n",
    );
}

#[test]
fn lir_diff_j40_boxing_order_reversed_then_passed_through() {
    // The `Counted` box is built FIRST here, so the two boxes are created in
    // the opposite order from j37. Both are already interface values by the
    // time `new TwoWay` sees them, which also pins j31's rule for this shape:
    // neither gets boxed a second time.
    assert_lir_differential(
        &boxing_source(
            r#"
fn reversedOrder(times: i64) -> i64 {
    let m = new Meter(0);
    let r: Counted = m;
    let t: Ticker = m;
    return new TwoWay(t, r).bumpThenRead(times);
}
fn main() { println(reversedOrder(2)); println(reversedOrder(5)); }
"#,
        ),
        "2\n5\n",
    );
}

#[test]
fn lir_diff_j41_one_object_into_two_interface_parameters_at_one_call_site() {
    // Two boxes over one object built as two ARGUMENTS of a single call, each
    // against a different declared parameter type. The first box is live while
    // the second one allocates.
    assert_lir_differential(
        &boxing_source(
            r#"
fn wire(t: Ticker, c: Counted, times: i64) -> i64 {
    return new TwoWay(t, c).bumpThenRead(times);
}
fn main() {
    let m = new Meter(4);
    println(wire(m, m, 3));
}
"#,
        ),
        "7\n",
    );
}

#[test]
fn lir_diff_j42_one_object_into_two_widening_lets() {
    // The same split across two `let` slots instead of two arguments: the
    // annotation on each local picks the interface, and therefore the vtable.
    assert_lir_differential(
        &boxing_source(
            r#"
fn twoLets(times: i64) -> i64 {
    let m = new Meter(100);
    let t: Ticker = m;
    let c: Counted = m;
    return new TwoWay(t, c).bumpThenRead(times);
}
fn main() { println(twoLets(0)); println(twoLets(5)); }
"#,
        ),
        "100\n105\n",
    );
}

#[test]
fn lir_diff_j43_same_object_boxed_twice_into_the_same_interface() {
    // Four boxes over one object, two of them into the SAME interface. Boxes
    // are not interned, so these really are four distinct 16-byte objects —
    // and every one of them still has to reach the same fields.
    assert_lir_differential(
        &boxing_source(
            r#"
fn shared(times: i64) -> i64 {
    let m = new Meter(0);
    let first = new TwoWay(m, m);
    let second = new TwoWay(m, m);
    let a = first.bumpThenRead(times);
    let b = second.readOnly();
    return a + b;
}
fn main() { println(shared(0)); println(shared(3)); }
"#,
        ),
        "0\n6\n",
    );
}

#[test]
fn lir_diff_j44_repointing_one_interface_field_leaves_the_other_alone() {
    // The two fields start out aliasing one object; re-pointing only the
    // `Ticker` field must not drag the `Counted` field along. So the ticks land
    // on `b` and the read still reports `a` — packed as `a * 100 + b`, where
    // `a` stays 7 and `b` climbs from 50.
    assert_lir_differential(
        &boxing_source(
            r#"
fn repoint(times: i64) -> i64 {
    let a = new Meter(7);
    let b = new Meter(50);
    let w = new TwoWay(a, a);
    w.ticker = b;
    let read = w.bumpThenRead(times);
    return read * 100 + b.hits;
}
fn main() {
    let mut k = 0;
    while k < 3 { println(repoint(k)); k = k + 1; }
}
"#,
        ),
        "750\n751\n752\n",
    );
}

#[test]
fn lir_diff_j45_one_object_in_two_differently_typed_arrays() {
    // Element stores pick the vtable from the ARRAY's element type, so putting
    // one object into `Array<Ticker>` and into `Array<Counted>` produces two
    // differently-vtabled boxes per object. Bumping every element of one array
    // and totalling the other is the identity check at array scale. (The seed
    // is widened by a helper: a literal element that widens is still outside
    // the walker's subset, and `LIR_ON` sets WILLOW_LIR_REQUIRE.)
    assert_lir_differential(
        &boxing_source(
            r#"
fn asTicker(m: Meter) -> Ticker { return m; }
fn asCounted(m: Meter) -> Counted { return m; }
fn arrays(n: i64) -> i64 {
    let seed = new Meter(0);
    let ts: Array<Ticker> = [asTicker(seed)];
    let rs: Array<Counted> = [asCounted(seed)];
    let mut i = 1;
    while i < n {
        let m = new Meter(i);
        ts.push(m);
        rs.push(m);
        i = i + 1;
    }
    return new Meters(ts, rs).bumpAllThenTotal();
}
fn main() { println(arrays(1)); println(arrays(4)); }
"#,
        ),
        "1\n10\n",
    );
}

#[test]
fn lir_diff_j46_two_interface_returning_functions_over_one_object() {
    // The boxes are built at two different `return` sites, in two different
    // functions, from the same object — so the vtable comes from each
    // function's RETURN type rather than from anything at the use site.
    assert_lir_differential(
        &boxing_source(
            r#"
fn asTicker(m: Meter) -> Ticker { return m; }
fn asCounted(m: Meter) -> Counted { return m; }
fn viaReturns(times: i64) -> i64 {
    let m = new Meter(1);
    return new TwoWay(asTicker(m), asCounted(m)).bumpThenRead(times);
}
fn main() { println(viaReturns(0)); println(viaReturns(6)); }
"#,
        ),
        "1\n7\n",
    );
}

#[test]
fn lir_diff_j47_identity_survives_the_second_box_allocation_stress() {
    // The whole point of the review that asked for this block: the SECOND box
    // is an allocation that happens while the object is reachable only from the
    // first box and from the half-initialized `TwoWay`. Under
    // WILLOW_GC_STRESS=alloc that allocation collects, so an object left
    // unrooted here is reclaimed and the two halves of the printed pair stop
    // agreeing (or the program crashes).
    assert_lir_gc_stress_differential(
        &boxing_source(
            r#"
fn round(times: i64) -> i64 {
    let m = new Meter(0);
    let w = new TwoWay(m, m);
    return w.bumpThenRead(times) * 100 + m.hits;
}
fn main() {
    let mut i = 0;
    while i < 10 { println(round(i)); i = i + 1; }
}
"#,
        ),
        &(0..10)
            .map(|i| format!("{}\n", i * 100 + i))
            .collect::<Vec<_>>()
            .join(""),
    );
}

#[test]
fn lir_diff_j48_first_box_and_half_built_owner_rooted_across_second_box_stress() {
    // Same shape as j47 with a whole CALL between the two boxes: `viaAlloc`
    // allocates a string and boxes on the way out. At its call the first box,
    // the object inside it and the half-initialized `TwoWay` are all live, and
    // all three have to survive.
    assert_lir_gc_stress_differential(
        &boxing_source(
            r#"
fn viaAlloc(m: Meter, s: String) -> Counted {
    if (s + "!") == "never" { return new Meter(0); }
    return m;
}
fn nested(n: i64) -> i64 {
    let m = new Meter(n);
    let w = new TwoWay(m, viaAlloc(m, "x"));
    return w.bumpThenRead(2) * 100 + m.hits;
}
fn main() {
    let mut i = 0;
    while i < 15 { println(nested(i)); i = i + 1; }
}
"#,
        ),
        &(0..15)
            .map(|i| format!("{}\n", (i + 2) * 100 + (i + 2)))
            .collect::<Vec<_>>()
            .join(""),
    );
}

#[test]
fn lir_diff_j49_reversed_slot_pair_built_in_a_loop_stress() {
    // The reversed-slot pair from j39, rebuilt on every iteration with freshly
    // allocated strings, so each `Ends` is collectable the moment it stops
    // being rooted. Getting `H!H!T?T?` fifteen times means both vtables stayed
    // correct and the object stayed alive through both boxes.
    assert_lir_gc_stress_differential(
        &boxing_source(
            r#"
fn ends(a: String, b: String) -> String {
    let e = new Ends(a + "!", b + "?");
    return new Pair(e, e).crossed();
}
fn main() {
    let mut i = 0;
    while i < 15 { println(ends("H", "T")); i = i + 1; }
}
"#,
        ),
        &"H!H!T?T?\n".repeat(15),
    );
}

#[test]
fn lir_diff_j50_two_boxes_of_one_object_beside_an_allocating_argument_stress() {
    // Argument order: box #1 (`Ticker`), box #2 (`Counted`), then a string
    // concatenation. Each step allocates, so by the last one both boxes have to
    // be rooted — not just the object they share.
    assert_lir_gc_stress_differential(
        &boxing_source(
            r#"
fn wireAlloc(t: Ticker, c: Counted, s: String) -> i64 {
    if (s + "!") == "never" { return -1; }
    return new TwoWay(t, c).bumpThenRead(2);
}
fn runAlloc(n: i64) -> i64 {
    let m = new Meter(n);
    return wireAlloc(m, m, "-" + "z") * 100 + m.hits;
}
fn main() {
    let mut i = 0;
    while i < 15 { println(runAlloc(i)); i = i + 1; }
}
"#,
        ),
        &(0..15)
            .map(|i| format!("{}\n", (i + 2) * 100 + (i + 2)))
            .collect::<Vec<_>>()
            .join(""),
    );
}

#[test]
fn lirreq_51_boxing_example_is_fully_lir() {
    // Same contract as the other examples (willow-j260): every FREE function in
    // the boxing example must be claimed by the walker. Its class methods hold
    // the virtual calls and compile through `compile_class_method_inner`, which
    // the mode does not police, so this pins exactly what the header claims.
    let source = include_str!("../../example/lir_interface_boxing.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
    assert!(
        ok,
        "example/lir_interface_boxing.wi must compile with every free function \
         on the LIR path: {stderr}"
    );
}

// ── LIR walker: interface dispatch (willow-0g8j.6) ─────────────────────────
// Reading back THROUGH an interface: `s.area()` loads `[object | vtable]` out
// of the box, indexes the vtable by the method's declaration-order slot and
// issues an indirect call with the concrete object as the receiver. Nothing in
// the emitted code knows the class, so the only thing standing between a
// correct call and a jump into the wrong function is the slot — which
// eligibility resolves the same way the emitter does.
//
// Perspectives k01-k23 are the eligibility half, in
// `src/backend/cranelift/lir_gen.rs`. k24-k40 below are the emitted-code half:
//
// k24 two classes behind one interface pick different implementations,
// k25 a later slot with arguments, k26 an INHERITED slot (`extends`),
// k27 a DEFAULT body vs a class override, k28 a void method in statement
// position, k29 an array element receiver in a loop, k30 a field-read
// receiver, k31 a temporary receiver, k32 dispatch feeding dispatch,
// k33 an interface-returning free function's result dispatched on,
// k34 every slot of one interface called in one function, k35 recursion
// through the interface; then under WILLOW_GC_STRESS=alloc: k36 the receiver
// rooted across an allocating argument, k37 boxes built and dispatched in a
// loop, k38 a temporary receiver rooted across its own argument; and for the
// debug call chain: k39 a panic inside a dispatched method carries the method
// frame on both backends, k40 an argument panic is NOT attributed to it.

/// Shared shape for the dispatch tests: one inherited slot (`name`), one
/// required slot with an argument (`scaled`), one default body (`twice`) and a
/// void slot (`stamp`), across two implementing classes. Unlike
/// [`BOXING_PRELUDE`], the reads here are FREE functions — that is the point.
const DISPATCH_PRELUDE: &str = r##"
import std::collections::Array;

interface Named { fn name(self) -> String; }

interface Shape extends Named {
    fn area(self) -> i64;
    fn scaled(self, factor: i64) -> i64;
    fn tagged(self, extra: String) -> String;
    fn stamp(self);
    fn twice(self) -> i64 { return self.area() + self.area(); }
}

class Square implements Shape {
    pub side: i64;
    pub fn name(self) -> String { return "square"; }
    pub fn area(self) -> i64 { return self.side * self.side; }
    pub fn scaled(self, factor: i64) -> i64 { return self.area() * factor; }
    pub fn tagged(self, extra: String) -> String { return "square:" + extra; }
    pub fn stamp(self) { println("sq"); }
}

class Rect implements Shape {
    pub w: i64;
    pub h: i64;
    pub fn name(self) -> String { return "rect"; }
    pub fn area(self) -> i64 { return self.w * self.h; }
    pub fn scaled(self, factor: i64) -> i64 { return self.area() * factor; }
    pub fn tagged(self, extra: String) -> String { return "rect:" + extra; }
    pub fn stamp(self) { println("rect"); }
    pub fn twice(self) -> i64 { return self.area() * 2; }
}

class Holder {
    pub shape: Shape;
}
"##;

fn dispatch_source(body: &str) -> String {
    format!("{DISPATCH_PRELUDE}{body}")
}

#[test]
fn lir_diff_k24_dispatch_picks_the_concrete_implementation() {
    // One call site, two classes: the vtable in each box decides.
    assert_lir_differential(
        &dispatch_source(
            r#"
fn area_of(s: Shape) -> i64 { return s.area(); }
fn main() {
    println(area_of(new Square(3)));
    println(area_of(new Rect(2, 5)));
}
"#,
        ),
        "9\n10\n",
    );
}

#[test]
fn lir_diff_k25_later_slot_with_arguments() {
    // `scaled` is not the first slot and it takes an argument: a wrong slot
    // would call `name`/`area` with an extra parameter.
    assert_lir_differential(
        &dispatch_source(
            r#"
fn scale(s: Shape, factor: i64) -> i64 { return s.scaled(factor); }
fn main() {
    println(scale(new Square(3), 4));
    println(scale(new Rect(2, 5), 3));
}
"#,
        ),
        "36\n30\n",
    );
}

#[test]
fn lir_diff_k26_inherited_slot_from_extends() {
    // `name` is declared by `Named`; desugaring composes it into `Shape`'s slot
    // list, and dispatch on a `Shape` box must find it there.
    assert_lir_differential(
        &dispatch_source(
            r#"
fn label(s: Shape) -> String { return "[" + s.name() + "]"; }
fn main() {
    println(label(new Square(1)));
    println(label(new Rect(1, 1)));
}
"#,
        ),
        "[square]\n[rect]\n",
    );
}

#[test]
fn lir_diff_k27_default_body_and_override() {
    // `Square` inherits the interface's default `twice`; `Rect` overrides it.
    // Both are the same slot, filled with different function pointers.
    assert_lir_differential(
        &dispatch_source(
            r#"
fn twice_of(s: Shape) -> i64 { return s.twice(); }
fn main() {
    println(twice_of(new Square(3)));
    println(twice_of(new Rect(2, 5)));
}
"#,
        ),
        "18\n20\n",
    );
}

#[test]
fn lir_diff_k28_void_slot_in_statement_position() {
    // A void method produces no Cranelift result; the walker must not read one,
    // and the side effect must land in order relative to its neighbours.
    assert_lir_differential(
        &dispatch_source(
            r#"
fn announce(s: Shape) -> i64 {
    println("before");
    s.stamp();
    println("after");
    return s.area();
}
fn main() { println(announce(new Rect(2, 5))); }
"#,
        ),
        "before\nrect\nafter\n10\n",
    );
}

#[test]
fn lir_diff_k29_array_element_receiver_in_a_loop() {
    // Every element is a box; the receiver is re-loaded each iteration.
    assert_lir_differential(
        &dispatch_source(
            r#"
fn total(xs: Array<Shape>) -> i64 {
    let mut i = 0;
    let mut sum = 0;
    while i < xs.len() { sum = sum + xs[i].area(); i = i + 1; }
    return sum;
}
fn main() {
    let first: Shape = new Square(3);
    let xs: Array<Shape> = [first];
    xs.push(new Rect(2, 5));
    xs.push(new Square(4));
    println(total(xs));
}
"#,
        ),
        "35\n",
    );
}

#[test]
fn lir_diff_k30_field_read_receiver() {
    // The receiver is an interface-typed FIELD, so the box comes out of an
    // object load rather than a variable.
    assert_lir_differential(
        &dispatch_source(
            r#"
fn held(h: Holder) -> String { return h.shape.name(); }
fn main() {
    println(held(new Holder(new Square(1))));
    println(held(new Holder(new Rect(1, 1))));
}
"#,
        ),
        "square\nrect\n",
    );
}

#[test]
fn lir_diff_k31_temporary_receiver() {
    // Nothing but the box holds the concrete object across the call.
    assert_lir_differential(
        &dispatch_source(
            r#"
fn fresh(side: i64, factor: i64) -> i64 {
    let s: Shape = new Square(side);
    return s.scaled(factor);
}
fn main() { println(fresh(6, 2)); }
"#,
        ),
        "72\n",
    );
}

#[test]
fn lir_diff_k32_dispatch_feeding_dispatch() {
    // A dispatch result chooses the receiver of the next dispatch, and the
    // chosen box is returned as the interface — no concrete class anywhere.
    assert_lir_differential(
        &dispatch_source(
            r#"
fn bigger(a: Shape, b: Shape) -> Shape {
    if a.area() > b.area() { return a; }
    return b;
}
fn main() {
    println(bigger(new Square(3), new Rect(2, 5)).name());
    println(bigger(new Square(3), new Rect(1, 1)).name());
}
"#,
        ),
        "rect\nsquare\n",
    );
}

#[test]
fn lir_diff_k33_interface_returning_function_result() {
    // The receiver is the result of a call whose return type is the interface.
    assert_lir_differential(
        &dispatch_source(
            r#"
fn make(kind: i64) -> Shape {
    if kind == 0 { return new Square(3); }
    return new Rect(2, 5);
}
fn main() {
    println(make(0).area());
    println(make(1).area());
}
"#,
        ),
        "9\n10\n",
    );
}

#[test]
fn lir_diff_k34_every_slot_in_one_function() {
    // All four slots called on one receiver: any off-by-one in the slot index
    // would show up as the wrong answer for at least one of them.
    assert_lir_differential(
        &dispatch_source(
            r#"
fn all(s: Shape) -> String {
    s.stamp();
    let a = s.area();
    let b = s.scaled(2);
    let c = s.twice();
    let d = s.tagged("t");
    if a == 10 && b == 20 && c == 20 && d == "rect:t" { return s.name() + " ok"; }
    return s.name() + " bad";
}
fn main() { println(all(new Rect(2, 5))); }
"#,
        ),
        "rect\nrect ok\n",
    );
}

#[test]
fn lir_diff_k35_recursion_through_the_interface() {
    // The recursive call re-enters the dispatching function with a new box, so
    // the receiver root and the call frame have to balance per level.
    assert_lir_differential(
        &dispatch_source(
            r#"
fn shrink(s: Shape, depth: i64) -> i64 {
    if depth == 0 { return s.area(); }
    return s.area() + shrink(new Square(depth), depth - 1);
}
fn main() { println(shrink(new Rect(2, 5), 3)); }
"#,
        ),
        "24\n",
    );
}

#[test]
fn lir_diff_k36_receiver_rooted_across_allocating_argument() {
    // The receiver object is reachable only through the box while the argument
    // expression allocates. An unrooted receiver is collected here and the
    // callee dereferences freed memory.
    assert_lir_gc_stress_differential(
        &dispatch_source(
            r#"
fn repeat(tag: String, n: i64) -> String {
    let mut out = "";
    let mut i = 0;
    while i < n { out = out + tag; i = i + 1; }
    return out;
}
fn grow(s: Shape) -> String { return s.tagged(repeat("xy", 6)); }
fn main() { println(grow(new Rect(2, 5))); }
"#,
        ),
        "rect:xyxyxyxyxyxy\n",
    );
}

#[test]
fn lir_diff_k37_boxes_built_and_dispatched_in_a_loop() {
    // A fresh box per iteration, dispatched on immediately: the loop must not
    // leak roots, and each box must survive its own call.
    assert_lir_gc_stress_differential(
        &dispatch_source(
            r#"
fn run(n: i64) -> i64 {
    let mut i = 1;
    let mut sum = 0;
    while i <= n {
        let s: Shape = new Square(i);
        sum = sum + s.scaled(2);
        i = i + 1;
    }
    return sum;
}
fn main() { println(run(6)); }
"#,
        ),
        "182\n",
    );
}

#[test]
fn lir_diff_k38_temporary_receiver_rooted_across_its_own_argument() {
    // Receiver and argument both allocate, in that order, with only the box
    // holding the object in between.
    assert_lir_gc_stress_differential(
        &dispatch_source(
            r#"
fn make(side: i64) -> Shape { return new Square(side); }
fn cost(n: i64) -> i64 {
    let mut acc = 0;
    let mut i = 0;
    while i < 20 { acc = acc + new Square(i).area(); i = i + 1; }
    return acc + n;
}
fn main() { println(make(3).scaled(cost(1))); }
"#,
        ),
        "22239\n",
    );
}

#[test]
fn lir_callstack_interface_dispatch_panic_has_frame_on_both_backends() {
    // Debug builds record a call-chain frame for the dispatched method, pushed
    // before the arguments are evaluated — the same order the AST emitter uses
    // for an instance method.
    let source = dispatch_source(
        r#"
class Bomb implements Shape {
    pub k: i64;
    pub fn name(self) -> String { return "bomb"; }
    pub fn area(self) -> i64 { panic("dispatch boom"); return 0; }
    pub fn scaled(self, factor: i64) -> i64 { return factor; }
    pub fn tagged(self, extra: String) -> String { return extra; }
    pub fn stamp(self) {}
}
fn measure(s: Shape) -> i64 { return s.area(); }
fn main() { println(measure(new Bomb(1))); }
"#,
    );
    let (with_lir, ok_on) = compile_with_env_and_run_combined(&source, &LIR_ON);
    let (without_lir, ok_off) = compile_with_env_and_run_combined(&source, &LIR_OFF);
    assert!(!ok_on && !ok_off, "both paths must panic");
    for (backend, out) in [("LIR", with_lir), ("AST", without_lir)] {
        let method = out
            .find("0: area")
            .unwrap_or_else(|| panic!("{backend} trace has no dispatched-method frame: {out}"));
        let caller = out
            .find("1: measure")
            .unwrap_or_else(|| panic!("{backend} trace has no caller frame: {out}"));
        assert!(method < caller, "{backend} trace is out of order: {out}");
    }
}

#[test]
fn lir_callstack_interface_argument_panic_reports_the_argument_as_the_top_frame() {
    // A dispatched method installs its frame BEFORE its arguments are
    // evaluated (matching the AST emitter), so an argument that panics does not
    // replace that frame — it stacks on top of it. The whole chain is therefore
    // pinned: the argument, then the method it was being passed to, then the
    // caller — identically on both backends.
    let source = dispatch_source(
        r#"
fn bad() -> i64 { return 1 / 0; }
fn measure(s: Shape) -> i64 { return s.scaled(bad()); }
fn main() { println(measure(new Square(3))); }
"#,
    );
    let (with_lir, ok_on) = compile_with_env_and_run_combined(&source, &LIR_ON);
    let (without_lir, ok_off) = compile_with_env_and_run_combined(&source, &LIR_OFF);
    assert!(!ok_on && !ok_off, "both paths must panic");
    for (backend, out) in [("LIR", with_lir), ("AST", without_lir)] {
        let argument = out
            .find("0: bad")
            .unwrap_or_else(|| panic!("{backend} trace has no argument frame: {out}"));
        let method = out
            .find("1: scaled")
            .unwrap_or_else(|| panic!("{backend} trace has no dispatched-method frame: {out}"));
        let caller = out
            .find("2: measure")
            .unwrap_or_else(|| panic!("{backend} trace has no caller frame: {out}"));
        assert!(
            argument < method && method < caller,
            "{backend} trace is out of order: {out}"
        );
    }
}

#[test]
fn lirreq_52_dispatch_example_is_fully_lir() {
    // Every free function in the dispatch example — `main` included — must be
    // claimed by the walker, which is what makes the example's own header claim
    // ("built with WILLOW_LIR_REQUIRE=1 must succeed") a checked one.
    let source = include_str!("../../example/lir_interface_dispatch.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
    assert!(
        ok,
        "example/lir_interface_dispatch.wi must compile with every free function \
         on the LIR path: {stderr}"
    );
}

// ── Interface dispatch with REFERENCE parameters (willow-0g8j.9) ────────────
//
// A `&`/`&mut` parameter is passed as a POINTER. The interface tables used to
// keep parameter TYPES only, so `emit_interface_dispatch` built its
// `call_indirect` signature from types and passed every argument by value —
// the concrete method then dereferenced an integer and the program crashed, on
// both backends. These tests run the real thing end to end.
//
// Perspectives 19..30 of willow-0g8j.9 (1..18 are the type-checker tests):
// 19 `&mut i64` mutates the caller's local · 20 `& i64` reads it ·
// 21 a `&mut String` place (GC-managed) · 22 the same under GC stress ·
// 23 a field place · 24 an array element · 25 an inherited (`extends`) slot ·
// 26 mixed value and reference parameters in one signature · 27 two `&mut`
// parameters, distinct places · 28 a default-bodied slot · 29 two classes
// behind one call site · 30 the caller falls back while its siblings stay on
// the walker.

/// Interface slots that differ in how their parameter is passed, not in its
/// type: `nudge` takes `&mut i64`, `peek` takes `& i64`, `weigh` takes a plain
/// `i64`. Any confusion between the three is a pointer/value confusion.
const REFMODE_PRELUDE: &str = r##"
import std::collections::Array;

interface Base { fn nudge(self, value: &mut i64); }

interface Scale extends Base {
    fn peek(self, value: & i64) -> i64;
    fn weigh(self, value: i64) -> i64;
    fn rename(self, label: &mut String);
    fn spread(self, lo: &mut i64, hi: &mut i64);
    fn double(self, value: &mut i64) { self.nudge(&value); self.nudge(&value); }
}

class Step implements Scale {
    pub by: i64;
    pub fn nudge(self, value: &mut i64) { value = value + self.by; }
    pub fn peek(self, value: & i64) -> i64 { return value + self.by; }
    pub fn weigh(self, value: i64) -> i64 { return value * self.by; }
    pub fn rename(self, label: &mut String) { label = label + "!"; }
    pub fn spread(self, lo: &mut i64, hi: &mut i64) { lo = lo - self.by; hi = hi + self.by; }
}

class Jump implements Scale {
    pub by: i64;
    pub fn nudge(self, value: &mut i64) { value = value * self.by; }
    pub fn peek(self, value: & i64) -> i64 { return value * self.by; }
    pub fn weigh(self, value: i64) -> i64 { return value + self.by; }
    pub fn rename(self, label: &mut String) { label = "<" + label + ">"; }
    pub fn spread(self, lo: &mut i64, hi: &mut i64) { lo = lo * self.by; hi = hi * self.by; }
    pub fn double(self, value: &mut i64) { value = value * self.by * self.by; }
}

class Cell { pub n: i64; }
"##;

fn refmode_source(body: &str) -> String {
    format!("{REFMODE_PRELUDE}{body}")
}

/// A reference ARGUMENT is outside the LIR subset (HIR lowering refuses it), so
/// the function holding the call falls back — `LIR_ON_MIXED` rather than
/// `LIR_ON`. What is being compared is that the fallback and the walker's own
/// callers still produce the same answer.
fn assert_refmode_differential(source: &str, expected: &str) {
    let (with_lir, ok_on) = compile_with_env_and_run(source, &LIR_ON_MIXED);
    assert!(ok_on, "LIR-enabled run failed: {with_lir}");
    let (without_lir, ok_off) = compile_with_env_and_run(source, &LIR_OFF);
    assert!(ok_off, "LIR-disabled run failed: {without_lir}");
    assert_eq!(with_lir, without_lir, "LIR and AST paths must agree");
    assert_eq!(with_lir, expected);
}

// 19. The whole point: a `&mut` argument dispatched through a vtable must
// write back into the CALLER's local, not into a copy — and must not
// dereference the value 10 as an address.
#[test]
fn refmode_19_mut_reference_through_dispatch_mutates_caller_local() {
    assert_refmode_differential(
        &refmode_source(
            r#"
fn main() {
    let s: Scale = new Step(5);
    let mut x = 10;
    s.nudge(&x);
    println(x);
}
"#,
        ),
        "15\n",
    );
}

// 20. A shared `&` parameter is a pointer too; the callee reads through it.
#[test]
fn refmode_20_shared_reference_through_dispatch_reads_caller_local() {
    assert_refmode_differential(
        &refmode_source(
            r#"
fn main() {
    let s: Scale = new Step(5);
    let x = 10;
    println(s.peek(&x));
    println(x);
}
"#,
        ),
        "15\n10\n",
    );
}

// 21. A GC-managed place: the pointer names a String slot, and the callee
// stores a freshly allocated String into it.
#[test]
fn refmode_21_mut_reference_to_string_place() {
    assert_refmode_differential(
        &refmode_source(
            r#"
fn main() {
    let s: Scale = new Step(1);
    let mut label = "name";
    s.rename(&label);
    println(label);
}
"#,
        ),
        "name!\n",
    );
}

/// [`assert_lir_gc_stress_differential`] for a program that intentionally
/// falls back (see [`assert_refmode_differential`]).
fn assert_refmode_gc_stress_differential(source: &str, expected: &str) {
    let stress = [("WILLOW_GC_STRESS", "alloc")];
    let (with_lir, ok_on) = compile_with_env_and_run_under(source, &LIR_ON_MIXED, &stress);
    assert!(ok_on, "LIR-enabled GC-stress run failed: {with_lir}");
    let (without_lir, ok_off) = compile_with_env_and_run_under(source, &LIR_OFF, &stress);
    assert!(ok_off, "LIR-disabled GC-stress run failed: {without_lir}");
    assert_eq!(
        with_lir, without_lir,
        "LIR and AST paths must agree under GC stress"
    );
    assert_eq!(with_lir, expected);
}

// 22. The same under GC stress: the receiver box is only reachable through the
// interface value while the callee allocates.
#[test]
fn refmode_22_string_place_under_gc_stress() {
    assert_refmode_gc_stress_differential(
        &refmode_source(
            r#"
fn main() {
    let s: Scale = new Jump(2);
    let mut label = "name";
    s.rename(&label);
    s.rename(&label);
    println(label);
}
"#,
        ),
        "<<name>>\n",
    );
}

// 23. The referenced place may be a FIELD of a live object.
#[test]
fn refmode_23_mut_reference_to_field_place() {
    assert_refmode_differential(
        &refmode_source(
            r#"
fn main() {
    let s: Scale = new Step(7);
    let c = new Cell(1);
    s.nudge(&c.n);
    println(c.n);
}
"#,
        ),
        "8\n",
    );
}

// 24. …or an ARRAY ELEMENT, whose address is computed from the buffer.
#[test]
fn refmode_24_mut_reference_to_array_element() {
    assert_refmode_differential(
        &refmode_source(
            r#"
fn main() {
    let s: Scale = new Step(3);
    let mut xs: Array<i64> = [1, 2];
    s.nudge(&xs[1]);
    println(xs[0]);
    println(xs[1]);
}
"#,
        ),
        "1\n5\n",
    );
}

// 25. `nudge` is INHERITED from `Base`, so it lives in a slot desugaring
// composed in — the mode has to survive that composition.
#[test]
fn refmode_25_inherited_slot_keeps_reference_mode() {
    assert_refmode_differential(
        &refmode_source(
            r#"
fn main() {
    let b: Base = new Jump(3);
    let mut x = 4;
    b.nudge(&x);
    println(x);
}
"#,
        ),
        "12\n",
    );
}

// 26. Value and reference parameters side by side on one interface: picking
// the wrong slot, or the wrong mode within a slot, changes the answer.
#[test]
fn refmode_26_value_and_reference_slots_side_by_side() {
    assert_refmode_differential(
        &refmode_source(
            r#"
fn main() {
    let s: Scale = new Step(4);
    let mut x = 6;
    println(s.weigh(x));
    s.nudge(&x);
    println(s.peek(&x));
    println(x);
}
"#,
        ),
        "24\n14\n10\n",
    );
}

// 27. Two `&mut` parameters in one call, naming distinct places: both pointers
// must reach the callee in the right order.
#[test]
fn refmode_27_two_mut_references_in_one_call() {
    assert_refmode_differential(
        &refmode_source(
            r#"
fn main() {
    let s: Scale = new Step(2);
    let mut lo = 10;
    let mut hi = 20;
    s.spread(&lo, &hi);
    println(lo);
    println(hi);
}
"#,
        ),
        "8\n22\n",
    );
}

// 28. A DEFAULT body forwards its own `&mut` parameter to another slot, so the
// pointer is passed on twice — and an override replaces the whole thing.
#[test]
fn refmode_28_default_body_forwards_a_reference_parameter() {
    assert_refmode_differential(
        &refmode_source(
            r#"
fn main() {
    let step: Scale = new Step(5);
    let mut a = 1;
    step.double(&a);
    println(a);

    let jump: Scale = new Jump(3);
    let mut b = 2;
    jump.double(&b);
    println(b);
}
"#,
        ),
        "11\n18\n",
    );
}

// 29. One call site, two classes: the vtable decides which reference-taking
// method runs.
#[test]
fn refmode_29_one_call_site_two_implementations() {
    assert_refmode_differential(
        &refmode_source(
            r#"
fn apply(s: Scale, start: i64) -> i64 {
    let mut x = start;
    s.nudge(&x);
    return x;
}
fn main() {
    println(apply(new Step(5), 10));
    println(apply(new Jump(5), 10));
}
"#,
        ),
        "15\n50\n",
    );
}

// 30. The eligibility contract itself: the function that writes the reference
// argument falls back, and `WILLOW_LIR_REQUIRE=1` says so by NAME — while a
// sibling that only dispatches by value stays on the walker.
#[test]
fn refmode_30_reference_call_falls_back_by_name() {
    let source = refmode_source(
        r#"
fn shifted(s: Scale, start: i64) -> i64 {
    let mut x = start;
    s.nudge(&x);
    return x;
}
fn weighed(s: Scale, v: i64) -> i64 { return s.weigh(v); }
fn main() {
    println(shifted(new Step(5), 10));
    println(weighed(new Step(5), 10));
}
"#,
    );
    let (ok, stderr) = compile_with_compiler_env(&source, &LIR_ON);
    assert!(
        !ok,
        "a reference-argument call must not be claimed: {stderr}"
    );
    assert!(
        stderr.contains("`shifted`"),
        "the fallback must name the function holding the reference call: {stderr}"
    );
    assert!(
        !stderr.contains("`weighed`"),
        "a by-value dispatch must stay on the walker: {stderr}"
    );
}

// ── WILLOW_LIR_REQUIRE: no silent fallback (willow-0g8j.4 review) ───────────
// A differential test only proves something if the "LIR on" side really used
// the LIR path. `WILLOW_LIR_BACKEND=1` alone cannot guarantee that: a function
// outside the walker's supported subset falls back to the AST emitter, so a
// lowering or eligibility regression would leave both sides on the AST path and
// the comparison would still pass. `WILLOW_LIR_REQUIRE=1` turns that fallback
// into a compile error naming the function, and `LIR_ON` sets it for every
// differential test above.
//
// Scope: the mode polices the sync AST-vs-LIR dispatch in
// `compile_function_named`. `async fn`s return earlier, to their own state
// machine emitter, and are unaffected — they never had an LIR path to lose.
//
// Perspectives 39-48:
// 39 an all-eligible program compiles AND runs under the mode, 40 an
// ineligible function makes compilation fail, 41 the diagnostic names the
// offending function, 42 the diagnostic gives the eligibility reason, 43 only
// the ineligible function is named when eligible ones sit beside it, 44 a
// `main` outside the supported shape gets its own reason, 45 `REQUIRE=0`
// restores the silent fallback, 46 the mode is off when the variable is unset,
// 47 the `WILLOW_LIR_BACKEND=0` kill switch wins over the mode (so the "LIR
// off" side of a differential test can never trip it), 48 an async function is
// out of scope, 49 the shipped array example has every function on the LIR
// path.

/// A function the walker cannot compile, plus an eligible one, so a test can
/// check exactly which name is reported.
///
/// Every earlier stand-in here kept getting promoted into the subset — plain
/// class field access (willow-0g8j.5), class-to-interface widening
/// (willow-j260), dispatch through an interface box (willow-0g8j.6), then
/// `Option`/`Result` themselves (willow-0g8j.2.1). So the ineligible function
/// now shadows a binding in a nested block, which is not a missing feature but
/// the one thing LIR's flat scopes structurally cannot express: both `total`s
/// would be the same variable.
const LIR_MIXED_SOURCE: &str = r#"
fn eligible(a: i64) -> i64 { return a + 1; }

fn nullable(n: i64) -> i64 {
    let total = n;
    if n > 0 {
        let total = n * 2;
        return total;
    }
    return total;
}

fn main() { println(eligible(1)); println(nullable(2)); }
"#;

#[test]
fn lirreq_39_eligible_program_compiles_and_runs() {
    // The mode must be transparent when nothing falls back: same output as the
    // AST path, which is what every `assert_lir_differential` above relies on.
    assert_lir_differential(
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs = [1, 2, 3];
    xs.push(4);
    return xs.len() + xs[3];
}
fn main() { println(f()); }
"#,
        "8\n",
    );
}

#[test]
fn lirreq_40_ineligible_function_fails_compilation() {
    let (ok, _stderr) = compile_with_compiler_env(LIR_MIXED_SOURCE, &LIR_ON);
    assert!(
        !ok,
        "a fallback must fail the build under WILLOW_LIR_REQUIRE"
    );
}

#[test]
fn lirreq_41_diagnostic_names_the_function() {
    let (_ok, stderr) = compile_with_compiler_env(LIR_MIXED_SOURCE, &LIR_ON);
    assert!(
        stderr.contains("`nullable`"),
        "diagnostic must name the function that fell back: {stderr}"
    );
    assert!(
        stderr.contains("WILLOW_LIR_REQUIRE"),
        "diagnostic must name the mode that caused the failure: {stderr}"
    );
}

#[test]
fn lirreq_42_diagnostic_gives_the_reason() {
    let (_ok, stderr) = compile_with_compiler_env(LIR_MIXED_SOURCE, &LIR_ON);
    // The reason names the construct that blocked it, not just that something
    // did (willow-0g8j.2): `nullable` rebinds `total` in a nested block.
    assert!(
        stderr.contains("`let total` reuses a name already bound in this function"),
        "diagnostic must say which construct fell back: {stderr}"
    );
    assert!(
        stderr.contains("flat scopes"),
        "diagnostic must say why the function fell back: {stderr}"
    );
}

#[test]
fn lirreq_43_eligible_neighbour_is_not_reported() {
    let (_ok, stderr) = compile_with_compiler_env(LIR_MIXED_SOURCE, &LIR_ON);
    assert!(
        !stderr.contains("`eligible`"),
        "a function that did compile from LIR must not be reported: {stderr}"
    );
}

#[test]
fn lirreq_44_unsupported_main_shape_has_its_own_reason() {
    let (ok, stderr) = compile_with_compiler_env(
        r#"
import std::collections::Array;

fn main(args: Array<String>) { println(args.len()); }
"#,
        &LIR_ON,
    );
    assert!(!ok, "a `main` taking args must not pass the mode silently");
    assert!(
        stderr.contains("`main`") && stderr.contains("parameterless"),
        "expected the main-shape reason: {stderr}"
    );
}

#[test]
fn lirreq_45_require_zero_allows_fallback() {
    let (ok, stderr) = compile_with_compiler_env(
        LIR_MIXED_SOURCE,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "0")],
    );
    assert!(ok, "WILLOW_LIR_REQUIRE=0 must keep the fallback: {stderr}");
}

#[test]
fn lirreq_46_unset_allows_fallback() {
    // The default has to stay "fall back quietly": most real programs contain
    // functions the walker does not support yet.
    let (ok, stderr) = compile_with_compiler_env(LIR_MIXED_SOURCE, &LIR_ON_MIXED);
    assert!(ok, "the mode must be off by default: {stderr}");
}

#[test]
fn lirreq_47_backend_kill_switch_wins() {
    // Otherwise the "LIR off" side of a differential test would fail the moment
    // the mode leaked into the ambient environment.
    let (ok, stderr) = compile_with_compiler_env(
        LIR_MIXED_SOURCE,
        &[("WILLOW_LIR_BACKEND", "0"), ("WILLOW_LIR_REQUIRE", "1")],
    );
    assert!(
        ok,
        "WILLOW_LIR_BACKEND=0 must disable the requirement too: {stderr}"
    );
}

#[test]
fn lirreq_48_async_functions_are_out_of_scope() {
    let (ok, stderr) = compile_with_compiler_env(
        r#"
async fn work(n: i64) -> i64 { return n + 1; }

async fn main() {
    let v = await work(1);
    println(v);
}
"#,
        &LIR_ON,
    );
    assert!(
        ok,
        "async functions use their own emitter, not the AST fallback: {stderr}"
    );
}

#[test]
fn lirreq_49_array_example_is_fully_lir() {
    // The example claims in its header that every function it declares is
    // compiled from the lowered IR; this is what keeps that claim honest.
    let source = include_str!("../../example/lir_gc_arrays.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
    assert!(
        ok,
        "example/lir_gc_arrays.wi must compile with every function on the LIR path: {stderr}"
    );
}

#[test]
fn lirreq_50_object_example_is_fully_lir() {
    // Same contract for the class-object example (willow-0g8j.5): the free
    // functions it declares must all be claimed by the walker. Its class
    // methods compile through `compile_class_method_inner`, which the mode does
    // not police, so this pins exactly what the header claims.
    let source = include_str!("../../example/lir_gc_objects.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
    assert!(
        ok,
        "example/lir_gc_objects.wi must compile with every free function on the LIR path: {stderr}"
    );
}

#[test]
fn lirreq_51_collections_example_is_fully_lir() {
    // Same contract for the collections example (willow-0g8j.7): its header
    // claims every function is compiled from the lowered IR, and the mode is
    // what keeps that claim honest.
    let source = include_str!("../../example/lir_gc_collections.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
    assert!(
        ok,
        "example/lir_gc_collections.wi must compile with every function on the LIR path: {stderr}"
    );
}

// ── Maps and frozen collections on the LIR path (willow-0g8j.7) ─────────────
// The emitted-code half of willow-0g8j.7; the eligibility half is the c01-c24
// perspectives in `src/backend/cranelift/lir_gen.rs`. Every program below runs
// on both paths and must print the same thing, and c42-c46 repeat that under
// `WILLOW_GC_STRESS=alloc` — the mode that collects at every allocation, so a
// receiver or value left unrooted across an allocating call is reclaimed and
// the output changes.
//
// c25 an empty map filled by `insert`, c26 `contains` both ways, c27
// `toString` (sorted by key, so the text is deterministic), c28 an i64-keyed
// map with String values, c29 a map as a parameter, c30 a map as a RETURN
// value, c31 `freeze` into a `FrozenMap` read through `len`/`contains`,
// c32 an array frozen and then indexed, c33 a `FrozenArray` parameter summed
// in a loop, c34 a map whose values are arrays, c35 a map of maps, c36 inserts
// inside a loop, c37 a map built and frozen with only the frozen copy kept,
// c38 the receiver rooted across an allocating VALUE expression, c39 the same
// across an allocating KEY expression, c40 a map local live across a later
// allocation, c41 map/array/String locals live at once; then under GC stress:
// c42 the insert loop, c43 freeze, c44 arrays as map values, c45 a map handed
// between frames, c46 a frozen array indexed in a loop.

const MAP_IMPORTS: &str = "import std::collections::Map;\nimport std::collections::Array;\n";

/// A collection program with both collection imports prepended.
fn map_source(body: &str) -> String {
    format!("{MAP_IMPORTS}{body}")
}

#[test]
fn lir_diff_c25_empty_map_filled_by_insert() {
    assert_lir_differential(
        &map_source(
            r#"
fn build() -> i64 {
    let m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    m.insert("b", 2);
    m.insert("a", 3);
    return m.len();
}
fn main() { println(build()); }
"#,
        ),
        "2\n",
    );
}

#[test]
fn lir_diff_c26_contains_hit_and_miss() {
    assert_lir_differential(
        &map_source(
            r#"
fn has(key: String) -> bool {
    let m: Map<String, i64> = Map::new();
    m.insert("here", 1);
    return m.contains(key);
}
fn main() { println(has("here")); println(has("gone")); }
"#,
        ),
        "true\nfalse\n",
    );
}

#[test]
fn lir_diff_c27_map_to_string() {
    // The runtime sorts by key, so this text is stable on both paths.
    assert_lir_differential(
        &map_source(
            r#"
fn render() -> String {
    let m: Map<String, i64> = Map::new();
    m.insert("b", 2);
    m.insert("a", 1);
    return m.toString();
}
fn main() { println(render()); }
"#,
        ),
        "{a: 1, b: 2}\n",
    );
}

#[test]
fn lir_diff_c28_int_keyed_map_with_string_values() {
    // The other admitted key type, and a value that is a GC handle rather than
    // a word — both halves of the `MapKey` ABI in one program.
    assert_lir_differential(
        &map_source(
            r#"
fn render() -> String {
    let m: Map<i64, String> = Map::new();
    m.insert(2, "two");
    m.insert(1, "one");
    return m.toString();
}
fn main() { println(render()); }
"#,
        ),
        "{1: \"one\", 2: \"two\"}\n",
    );
}

#[test]
fn lir_diff_c29_map_parameter() {
    assert_lir_differential(
        &map_source(
            r#"
fn size(m: Map<String, i64>) -> i64 { return m.len(); }
fn main() {
    let m: Map<String, i64> = Map::new();
    m.insert("x", 1);
    m.insert("y", 2);
    println(size(m));
}
"#,
        ),
        "2\n",
    );
}

#[test]
fn lir_diff_c30_map_returned_across_frames() {
    // The callee's map must survive the return — it is live only through the
    // caller's slot once the callee's roots are popped.
    assert_lir_differential(
        &map_source(
            r#"
fn make(n: i64) -> Map<i64, i64> {
    let m: Map<i64, i64> = Map::new();
    let mut i = 0;
    while i < n { m.insert(i, i * i); i = i + 1; }
    return m;
}
fn main() {
    let m = make(4);
    println(m.len());
    println(m.contains(3));
}
"#,
        ),
        "4\ntrue\n",
    );
}

#[test]
fn lir_diff_c31_map_freeze_and_read() {
    assert_lir_differential(
        &map_source(
            r#"
fn frozen() -> FrozenMap<String, i64> {
    let m: Map<String, i64> = Map::new();
    m.insert("k", 7);
    return m.freeze();
}
fn main() {
    let g = frozen();
    println(g.len());
    println(g.contains("k"));
    println(g.contains("nope"));
}
"#,
        ),
        "1\ntrue\nfalse\n",
    );
}

#[test]
fn lir_diff_c32_array_freeze_and_index() {
    assert_lir_differential(
        &map_source(
            r#"
fn frozen() -> FrozenArray<i64> {
    let xs: Array<i64> = [10, 20, 30];
    return xs.freeze();
}
fn main() {
    let ys = frozen();
    println(ys.len());
    println(ys[0]);
    println(ys[2]);
}
"#,
        ),
        "3\n10\n30\n",
    );
}

#[test]
fn lir_diff_c33_frozen_array_parameter_in_a_loop() {
    assert_lir_differential(
        &map_source(
            r#"
fn total(xs: FrozenArray<i64>) -> i64 {
    let mut i = 0;
    let mut sum = 0;
    while i < xs.len() { sum = sum + xs[i]; i = i + 1; }
    return sum;
}
fn main() {
    let xs: Array<i64> = [1, 2, 3, 4];
    println(total(xs.freeze()));
}
"#,
        ),
        "10\n",
    );
}

#[test]
fn lir_diff_c34_map_of_arrays() {
    // The value is a GC handle: the store goes through the same discipline as
    // any other reference, and the array must outlive the insert.
    assert_lir_differential(
        &map_source(
            r#"
fn build() -> i64 {
    let m: Map<String, Array<i64>> = Map::new();
    m.insert("a", [1, 2]);
    m.insert("b", [3]);
    return m.len();
}
fn main() { println(build()); }
"#,
        ),
        "2\n",
    );
}

#[test]
fn lir_diff_c35_map_of_maps() {
    assert_lir_differential(
        &map_source(
            r#"
fn inner(v: i64) -> Map<String, i64> {
    let m: Map<String, i64> = Map::new();
    m.insert("v", v);
    return m;
}
fn build() -> i64 {
    let outer: Map<String, Map<String, i64>> = Map::new();
    outer.insert("one", inner(1));
    outer.insert("two", inner(2));
    return outer.len();
}
fn main() { println(build()); }
"#,
        ),
        "2\n",
    );
}

#[test]
fn lir_diff_c36_inserts_in_a_loop() {
    // The map local is live across every iteration's allocations, and the keys
    // come out of a frozen array — both collection kinds in one loop.
    assert_lir_differential(
        &map_source(
            r#"
fn build(keys: FrozenArray<String>) -> i64 {
    let m: Map<String, i64> = Map::new();
    let mut i = 0;
    while i < keys.len() {
        m.insert(keys[i] + "!", i);
        i = i + 1;
    }
    return m.len();
}
fn main() {
    let keys: Array<String> = ["a", "b", "c", "d", "e"];
    println(build(keys.freeze()));
}
"#,
        ),
        "5\n",
    );
}

#[test]
fn lir_diff_c37_only_the_frozen_copy_is_kept() {
    // The mutable map goes out of scope; the frozen copy is an independent
    // object, so it must not observe anything the source did afterwards.
    assert_lir_differential(
        &map_source(
            r#"
fn snapshot() -> FrozenMap<String, i64> {
    let m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    let g = m.freeze();
    m.insert("b", 2);
    return g;
}
fn main() {
    let g = snapshot();
    println(g.len());
    println(g.contains("b"));
}
"#,
        ),
        "1\nfalse\n",
    );
}

#[test]
fn lir_diff_c38_receiver_rooted_across_allocating_value() {
    // `a + b` allocates a String between the receiver's evaluation and the
    // insert call. An unrooted receiver would be collected under stress.
    assert_lir_differential(
        &map_source(
            r#"
fn build(a: String, b: String) -> String {
    let m: Map<String, String> = Map::new();
    m.insert("k", a + b);
    return m.toString();
}
fn main() { println(build("x", "y")); }
"#,
        ),
        "{k: \"xy\"}\n",
    );
}

#[test]
fn lir_diff_c39_receiver_rooted_across_allocating_key() {
    assert_lir_differential(
        &map_source(
            r#"
fn build(a: String, b: String) -> String {
    let m: Map<String, i64> = Map::new();
    m.insert(a + b, 1);
    return m.toString();
}
fn main() { println(build("x", "y")); }
"#,
        ),
        "{xy: 1}\n",
    );
}

#[test]
fn lir_diff_c40_map_live_across_a_later_allocation() {
    assert_lir_differential(
        &map_source(
            r#"
fn build() -> i64 {
    let m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    let s = "unrelated" + " allocation";
    let xs: Array<i64> = [1, 2, 3];
    xs.push(1);
    return m.len() + xs.len();
}
fn main() { println(build()); }
"#,
        ),
        "5\n",
    );
}

#[test]
fn lir_diff_c41_map_array_and_string_locals_at_once() {
    assert_lir_differential(
        &map_source(
            r#"
fn mixed() -> String {
    let m: Map<String, i64> = Map::new();
    let xs: Array<i64> = [1, 2];
    let s = "n=";
    m.insert("count", xs.len());
    return s + m.toString() + xs.toString();
}
fn main() { println(mixed()); }
"#,
        ),
        "n={count: 2}[1, 2]\n",
    );
}

#[test]
fn lir_diff_c42_insert_loop_under_gc_stress() {
    assert_lir_gc_stress_differential(
        &map_source(
            r#"
fn build(keys: FrozenArray<String>) -> String {
    let m: Map<String, i64> = Map::new();
    let mut i = 0;
    while i < keys.len() {
        m.insert(keys[i] + "!", i);
        i = i + 1;
    }
    return m.toString();
}
fn main() {
    let keys: Array<String> = ["a", "b", "c"];
    println(build(keys.freeze()));
}
"#,
        ),
        "{a!: 0, b!: 1, c!: 2}\n",
    );
}

#[test]
fn lir_diff_c43_freeze_under_gc_stress() {
    // `freeze` copies, so the source is rooted across the copy's allocation.
    assert_lir_gc_stress_differential(
        &map_source(
            r#"
fn snapshot() -> i64 {
    let m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    m.insert("b", 2);
    let g = m.freeze();
    let xs: Array<i64> = [1, 2, 3];
    return g.len() + xs.len();
}
fn main() { println(snapshot()); }
"#,
        ),
        "5\n",
    );
}

#[test]
fn lir_diff_c44_arrays_as_map_values_under_gc_stress() {
    assert_lir_gc_stress_differential(
        &map_source(
            r#"
fn build(n: i64) -> i64 {
    let m: Map<i64, Array<i64>> = Map::new();
    let mut i = 0;
    while i < n {
        m.insert(i, [i, i + 1]);
        i = i + 1;
    }
    return m.len();
}
fn main() { println(build(4)); }
"#,
        ),
        "4\n",
    );
}

#[test]
fn lir_diff_c45_map_handed_between_frames_under_gc_stress() {
    assert_lir_gc_stress_differential(
        &map_source(
            r#"
fn make() -> Map<String, String> {
    let m: Map<String, String> = Map::new();
    m.insert("a", "alpha");
    m.insert("b", "beta");
    return m;
}
fn grow(m: Map<String, String>) -> Map<String, String> {
    m.insert("c", "gamma");
    return m;
}
fn main() { println(grow(make()).toString()); }
"#,
        ),
        "{a: \"alpha\", b: \"beta\", c: \"gamma\"}\n",
    );
}

#[test]
fn lir_diff_c46_frozen_array_loop_under_gc_stress() {
    assert_lir_gc_stress_differential(
        &map_source(
            r#"
fn total(xs: FrozenArray<String>) -> String {
    let mut i = 0;
    let mut out = "";
    while i < xs.len() { out = out + xs[i]; i = i + 1; }
    return out;
}
fn main() {
    let xs: Array<String> = ["a", "b", "c"];
    println(total(xs.freeze()));
}
"#,
        ),
        "abc\n",
    );
}

// ── Debug-build integer division guards (willow-l9lx) ───────────────────────
// `/` and `%` used to die with a raw hardware signal; debug builds now emit a
// located runtime panic, consistent with the array bounds panics. 20
// perspectives: 1 div-by-zero message+location (LIR path), 2 same on the AST
// path, 3 rem-by-zero, 4 MIN/-1 overflow, 5 MIN%-1 overflow, 6 non-zero exit,
// 7 call-stack frame present, 8-9 normal div/rem unaffected on both paths,
// 10 runtime-value divisor, 11 guard inside a loop, 12 guard in a class
// method, 13 guard in an async fn, 14 f64 division by zero is NOT trapped,
// 15 constant operands still guarded, 16 computed-zero divisor, 17 rem in a
// LIR loop, 18 message names the source file, 19 zero mid-chain, 20 negative
// dividend unaffected.

fn div_panic_output(source: &str) -> String {
    let (out, ok) = compile_and_run_check_exit(source);
    assert!(!ok, "expected a runtime panic, got success: {out}");
    out
}

#[test]
fn divguard_01_div_zero_message_lir() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { println(f(1, 0)); }",
    );
    assert!(out.contains("runtime panic: division by zero at"), "{out}");
}

#[test]
fn divguard_02_div_zero_message_ast_path() {
    let source = "fn f(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { println(f(1, 0)); }";
    let (out, ok) = compile_with_env_and_run(source, &[("WILLOW_LIR_BACKEND", "0")]);
    assert!(!ok, "expected panic");
    let _ = out; // stdout empty; the message goes to stderr (checked via exit path below)
    let (all, ok2) = compile_and_run_check_exit(source);
    assert!(!ok2);
    assert!(all.contains("division by zero"), "{all}");
}

#[test]
fn divguard_03_rem_zero_message() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64) -> i64 { return a % b; }\nfn main() { println(f(1, 0)); }",
    );
    assert!(out.contains("runtime panic: remainder by zero at"), "{out}");
}

#[test]
fn divguard_04_min_div_neg1_overflow() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { let a = -9223372036854775807 - 1; println(f(a, -1)); }",
    );
    assert!(out.contains("integer overflow: `i64::MIN / -1`"), "{out}");
}

#[test]
fn divguard_05_min_rem_neg1_overflow() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64) -> i64 { return a % b; }\nfn main() { let a = -9223372036854775807 - 1; println(f(a, -1)); }",
    );
    assert!(out.contains("integer overflow: `i64::MIN % -1`"), "{out}");
}

#[test]
fn divguard_06_nonzero_exit() {
    let (_, ok) = compile_and_run_check_exit(
        "fn f(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { println(f(1, 0)); }",
    );
    assert!(!ok);
}

#[test]
fn divguard_07_call_stack_frame() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { println(f(1, 0)); }",
    );
    assert!(out.contains("call stack"), "{out}");
}

#[test]
fn divguard_08_normal_div_unaffected_lir() {
    let (out, ok) = compile_and_run("fn main() { println(10 / 3); println(10 % 3); }");
    assert!(ok);
    assert_eq!(out, "3\n1\n");
}

#[test]
fn divguard_09_normal_div_unaffected_ast() {
    let (out, ok) = compile_with_env_and_run(
        "fn main() { println(10 / 3); println(10 % 3); }",
        &[("WILLOW_LIR_BACKEND", "0")],
    );
    assert!(ok);
    assert_eq!(out, "3\n1\n");
}

#[test]
fn divguard_10_runtime_divisor() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut d = 5; let mut t = 0; while d > 0 { t = t + 100 / d; d = d - 1; } println(t); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "228\n"); // 20+25+33+50+100
}

#[test]
fn divguard_11_zero_inside_loop() {
    let out = div_panic_output(
        "fn main() { let mut d = 2; while d >= 0 { println(10 / d); d = d - 1; } }",
    );
    assert!(out.contains("division by zero"), "{out}");
    assert!(
        out.contains("5\n10\n"),
        "loop iterations before the panic: {out}"
    );
}

#[test]
fn divguard_12_guard_in_class_method() {
    let out = div_panic_output(
        "class C { pub fn ratio(self, a: i64, b: i64) -> i64 { return a / b; } }\nfn main() { let c = new C(); println(c.ratio(1, 0)); }",
    );
    assert!(out.contains("division by zero"), "{out}");
}

#[test]
fn divguard_13_guard_in_async_fn() {
    let out = div_panic_output(
        "async fn f(a: i64, b: i64) -> i64 { return a / b; }\nasync fn main() { println(await f(1, 0)); }",
    );
    assert!(out.contains("division by zero"), "{out}");
}

#[test]
fn divguard_14_f64_div_zero_not_trapped() {
    let (out, ok) = compile_and_run("fn main() { let x = 1.0 / 0.0; println(x > 100.0); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn divguard_15_constant_operands_guarded() {
    let out = div_panic_output("fn main() { let z = 0; println(1 / z); }");
    assert!(out.contains("division by zero"), "{out}");
}

#[test]
fn divguard_16_computed_zero_divisor() {
    let out = div_panic_output(
        "fn f(b: i64) -> i64 { return 10 / (b - b); }\nfn main() { println(f(3)); }",
    );
    assert!(out.contains("division by zero"), "{out}");
}

#[test]
fn divguard_17_rem_in_lir_loop() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut t = 0; for i in 1..5 { t = t + 10 % i; } println(t); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n"); // 0+0+1+2
}

#[test]
fn divguard_18_message_names_source_file() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { println(f(1, 0)); }",
    );
    assert!(
        out.contains(".wi:"),
        "location with file name expected: {out}"
    );
}

#[test]
fn divguard_19_zero_mid_chain() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64, c: i64) -> i64 { return a / b / c; }\nfn main() { println(f(100, 0, 5)); }",
    );
    assert!(out.contains("division by zero"), "{out}");
}

#[test]
fn divguard_20_negative_dividend_unaffected() {
    let (out, ok) = compile_and_run("fn main() { println(-7 / 2); println(-7 % 2); }");
    assert!(ok, "{out}");
    assert_eq!(out, "-3\n-1\n");
}

// ── Nested-place field assignment (willow-qzxg) ──────────────────────────────
// 10 runtime perspectives completing the 20 with the parser tests: 11 two-level
// write, 12 three-level write, 13 array-element field write, 14 call-result
// field write (mutates the returned object), 15 write then read back through
// the same path, 16 nested write inside a loop, 17 nested write in a method
// body via self, 18 mixed with one-level writes, 19 optional intermediate is
// rejected before a nested write, 20 checker still rejects a private field.

#[test]
fn nestassign_11_two_level_write() {
    let (out, ok) = compile_and_run(
        "class B { pub v: i64; } class A { pub b: B; }\nfn main() { let a = new A(new B(1)); a.b.v = 2; println(a.b.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn nestassign_12_three_level_write() {
    let (out, ok) = compile_and_run(
        "class C { pub v: i64; } class B { pub c: C; } class A { pub b: B; }\nfn main() { let a = new A(new B(new C(1))); a.b.c.v = 9; println(a.b.c.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn nestassign_13_array_element_field_write() {
    let (out, ok) = compile_and_run(
        "class P { pub x: i64; }\nfn main() { let ps = [new P(1), new P(2)]; ps[1].x = 7; println(ps[0].x); println(ps[1].x); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n7\n");
}

#[test]
fn nestassign_14_call_result_field_write() {
    let (out, ok) = compile_and_run(
        "class P { pub x: i64; }\nfn pick(p: P) -> P { return p; }\nfn main() { let p = new P(1); pick(p).x = 5; println(p.x); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn nestassign_15_write_then_read_same_path() {
    let (out, ok) = compile_and_run(
        "class B { pub v: i64; } class A { pub b: B; }\nfn main() { let a = new A(new B(0)); a.b.v = 3; a.b.v = a.b.v + 4; println(a.b.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn nestassign_16_write_inside_loop() {
    let (out, ok) = compile_and_run(
        "class B { pub v: i64; } class A { pub b: B; }\nfn main() { let a = new A(new B(0)); for i in 0..4 { a.b.v = a.b.v + i; } println(a.b.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn nestassign_17_write_via_self_in_method() {
    let (out, ok) = compile_and_run(
        "class B { pub v: i64; } class A { pub b: B; pub fn set(self, n: i64) { self.b.v = n; } }\nfn main() { let a = new A(new B(1)); a.set(42); println(a.b.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn nestassign_18_mixed_with_one_level() {
    let (out, ok) = compile_and_run(
        "class B { pub v: i64; } class A { pub b: B; pub n: i64; }\nfn main() { let a = new A(new B(1), 10); a.n = 20; a.b.v = 30; println(a.n + a.b.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "50\n");
}

#[test]
fn nestassign_19_nil_intermediate_rejected_by_checker() {
    // An Option intermediate must be explicitly opened before nested access.
    let (ok, stderr) = compile_with_compiler_env(
        "class B { pub v: i64; } class A { pub b: Option<B>; }\nfn main() { let a = new A(None); a.b.v = 2; }",
        &[],
    );
    assert!(!ok, "nullable intermediate must be rejected");
    assert!(
        stderr.contains("E0201") && stderr.contains("handling absence"),
        "{stderr}"
    );
}

#[test]
fn nestassign_20_private_field_still_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "class B { v: i64; } class A { pub b: B; }\nfn main() { let a = new A(new B(1)); a.b.v = 2; }",
        &[],
    );
    assert!(!ok, "private nested field write must be rejected");
    assert!(!stderr.is_empty());
}

// ── Trap contract sweep (willow-l9lx bug CLASS detector) ────────────────────
// Every aborting runtime failure in a DEBUG build must present as a located
// `runtime panic:` message — never a silent raw hardware signal (which prints
// nothing). One table; new trappable constructs must join it. A row failing
// with EMPTY output means a raw SIGILL/SIGFPE regression of the l9lx class.
#[test]
fn trap_contract_all_aborts_have_panic_messages() {
    let scenarios: &[(&str, &str)] = &[
        (
            // The sleep must outlast ANY stall the spawner can suffer between
            // the spawn and the `cancel()`. The task is published to the shared
            // run queues and polled by a peer worker immediately, so with a
            // short sleep it can reach `return 1` before `h.cancel()` lands and
            // the await then succeeds instead of aborting — a load-sensitive
            // flake that only showed up when the whole suite ran in parallel
            // (willow-fqzz). An hour is not a wait: `cancel()` re-queues the
            // task and the await aborts in milliseconds. If cancellation ever
            // stops landing, the scenario parks instead of finishing, which is
            // why every row below runs under a hard deadline.
            "await of a cancelled task",
            "async fn t() -> i64 { await sleep(3600000); return 1; } async fn main() { let h = t(); h.cancel(); println(await h); }",
        ),
        (
            "int division by zero",
            "fn f(a: i64, b: i64) -> i64 { return a / b; } fn main() { println(f(1, 0)); }",
        ),
        (
            "int remainder by zero",
            "fn f(a: i64, b: i64) -> i64 { return a % b; } fn main() { println(f(1, 0)); }",
        ),
        (
            "i64::MIN / -1 overflow",
            "fn f(a: i64, b: i64) -> i64 { return a / b; } fn main() { let a = -9223372036854775807 - 1; println(f(a, -1)); }",
        ),
        (
            "i64::MIN % -1 overflow",
            "fn f(a: i64, b: i64) -> i64 { return a % b; } fn main() { let a = -9223372036854775807 - 1; println(f(a, -1)); }",
        ),
        (
            "array index out of bounds",
            "import std::collections::Array; fn main() { let xs: Array<i64> = [1]; println(xs[5]); }",
        ),
        (
            "array negative index",
            "import std::collections::Array; fn main() { let xs: Array<i64> = [1]; println(xs[0 - 1]); }",
        ),
        (
            "pop from empty array",
            "import std::collections::Array; fn main() { let mut xs: Array<i64> = [1]; xs.pop(); xs.pop(); }",
        ),
        (
            "array element write out of bounds",
            "import std::collections::Array; fn main() { let xs: Array<i64> = [1]; xs[5] = 9; }",
        ),
        // (invalid reference field access is CHECKER-prevented in every reachable
        // form — direct, aliased, nested, narrowing-then-mutate — so it has no
        // runtime row; the backend guard remains defense-in-depth.)
        ("explicit panic()", "fn main() { panic(\"boom\"); }"),
    ];
    for (what, source) in scenarios {
        // Guard against a silently-uncompilable row: a compile failure would
        // otherwise masquerade as the expected abort.
        let (compiles, stderr) = compile_with_compiler_env(source, &[]);
        assert!(compiles, "{what}: scenario must compile, got: {stderr}");
        // Under a deadline: an aborting row takes milliseconds, so a row that
        // parks instead has failed the contract just as surely as one that
        // exits 0, and it must say so rather than hang the suite.
        let (out, ok, timed_out) =
            compile_and_run_with_env_timeout(source, &[], std::time::Duration::from_secs(60));
        assert!(
            !timed_out,
            "{what}: never aborted — the scenario parked instead. output: {out:?}"
        );
        assert!(!ok, "{what}: expected an abort, got success: {out}");
        assert!(
            out.contains("runtime panic:") || out.contains("panic:"),
            "{what}: aborted with NO panic message (raw signal — l9lx-class \
             regression). output: {out:?}"
        );
    }
}

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

// ── Formatted panic + generalized format (willow-csax) ──────────────────────
// 20 perspectives (plus 10 spec-parser unit tests in src/interpolate.rs):
// 1 i64 {}, 2 f64 {}, 3 bool {}, 4 String {}, 5 multiple args in order,
// 6 brace escapes at runtime, 7 f64 precision placeholders still work,
// 8 formatted panic message + location, 9 panic call stack + non-zero exit,
// 10 one-arg panic with braces stays literal (back-compat), 11 formatted
// panic in a method, 12 in an async fn, 13 nested format inside panic args,
// 14 too many args rejected, 15 too few args rejected, 16 non-literal spec
// rejected, 17 unknown placeholder rejected, 18 non-printable arg rejected,
// 19 f64 placeholder with i64 arg rejected, 20 GC stress over many pieces
// (intermediate concat results stay rooted).

#[test]
fn interp_01_i64_display() {
    let (out, ok) = compile_and_run("fn main() { println(format(\"x = {}\", 42)); }");
    assert!(ok, "{out}");
    assert_eq!(out, "x = 42\n");
}

#[test]
fn interp_02_f64_display() {
    let (out, ok) = compile_and_run("fn main() { println(format(\"v = {}\", 1.5)); }");
    assert!(ok, "{out}");
    assert_eq!(out, "v = 1.5\n");
}

#[test]
fn interp_03_bool_display() {
    let (out, ok) = compile_and_run("fn main() { println(format(\"b = {}\", 1 < 2)); }");
    assert!(ok, "{out}");
    assert_eq!(out, "b = true\n");
}

#[test]
fn interp_04_string_display() {
    let (out, ok) =
        compile_and_run("fn main() { let name = \"willow\"; println(format(\"hi {}\", name)); }");
    assert!(ok, "{out}");
    assert_eq!(out, "hi willow\n");
}

#[test]
fn interp_05_multiple_args_in_order() {
    let (out, ok) = compile_and_run("fn main() { println(format(\"{} < {} < {}\", 1, 2, 3)); }");
    assert!(ok, "{out}");
    assert_eq!(out, "1 < 2 < 3\n");
}

#[test]
fn interp_06_brace_escapes() {
    let (out, ok) = compile_and_run("fn main() { println(format(\"{{{}}}\", 5)); }");
    assert!(ok, "{out}");
    assert_eq!(out, "{5}\n");
}

#[test]
fn interp_07_f64_precision_placeholders() {
    let (out, ok) = compile_and_run(
        "fn main() { println(format(\"{:.6f}\", 3.14159265)); println(format(\"~{:.16f}~\", 1.5)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3.141593\n~1.5000000000000000~\n");
}

#[test]
fn interp_08_formatted_panic_message_and_location() {
    let (out, ok) = compile_and_run_check_exit("fn main() { panic(\"bad value: {}\", 42); }");
    assert!(!ok);
    assert!(out.contains("runtime panic: bad value: 42 at"), "{out}");
}

#[test]
fn interp_09_panic_stack_and_exit() {
    let (out, ok) = compile_and_run_check_exit(
        "fn boom(n: i64) { panic(\"n = {}\", n); }\nfn main() { boom(7); }",
    );
    assert!(!ok);
    assert!(out.contains("n = 7"), "{out}");
    assert!(out.contains("call stack"), "{out}");
}

#[test]
fn interp_10_one_arg_panic_braces_stay_literal() {
    // Back-compat: a single-argument panic never interpolates.
    let (out, ok) = compile_and_run_check_exit("fn main() { panic(\"100% {weird}\"); }");
    assert!(!ok);
    assert!(out.contains("runtime panic: 100% {weird}"), "{out}");
}

#[test]
fn interp_11_formatted_panic_in_method() {
    let (out, ok) = compile_and_run_check_exit(
        "class C { pub fn go(self, n: i64) { panic(\"C says {}\", n); } }\nfn main() { let c = new C(); c.go(3); }",
    );
    assert!(!ok);
    assert!(out.contains("C says 3"), "{out}");
}

#[test]
fn interp_12_formatted_panic_in_async_fn() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn f(n: i64) -> i64 { panic(\"async {}\", n); }\nasync fn main() { println(await f(9)); }",
    );
    assert!(!ok);
    assert!(out.contains("async 9"), "{out}");
}

#[test]
fn interp_13_nested_format_inside_panic() {
    let (out, ok) =
        compile_and_run_check_exit("fn main() { panic(\"outer {}\", format(\"inner {}\", 1)); }");
    assert!(!ok);
    assert!(out.contains("outer inner 1"), "{out}");
}

#[test]
fn interp_14_too_many_args_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { panic(\"x = {}\", 1, 2); }", &[]);
    assert!(!ok);
    assert!(stderr.contains("E1401"), "{stderr}");
}

#[test]
fn interp_15_too_few_args_rejected() {
    let (ok, stderr) =
        compile_with_compiler_env("fn main() { let s = format(\"{} {}\", 1); }", &[]);
    assert!(!ok);
    assert!(stderr.contains("E1401"), "{stderr}");
}

#[test]
fn interp_16_non_literal_spec_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn main() { let spec = \"x = {}\"; let s = format(spec, 1); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("string literal"), "{stderr}");
}

#[test]
fn interp_17_unknown_placeholder_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { let s = format(\"{:x}\", 1); }", &[]);
    assert!(!ok);
    assert!(stderr.contains("E1401"), "{stderr}");
}

#[test]
fn interp_18_non_printable_arg_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1]; let s = format(\"{}\", xs); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("cannot format"), "{stderr}");
}

#[test]
fn interp_19_f64_placeholder_i64_arg_rejected() {
    let (ok, stderr) =
        compile_with_compiler_env("fn main() { let s = format(\"{:.6f}\", 1); }", &[]);
    assert!(!ok);
    assert!(stderr.contains("expected `f64`"), "{stderr}");
}

#[test]
fn interp_20_many_pieces_under_gc_stress() {
    // Every concat allocates; under alloc-stress each one collects — the
    // rooted intermediates must survive.
    let (out, ok) = compile_and_run_gc_stress(
        "fn main() { println(format(\"{} {} {} {} {} {}\", 1, true, 2.5, \"s\", 3, false)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1 true 2.5 s 3 false\n");
}

// ── Collection debug display: .toString() (willow-vwn6) ─────────────────────
// 20 perspectives: 1 i64 array, 2 String array (quoted), 3 f64 array
// (shortest repr), 4 bool array, 5 empty array, 6 single element, 7 f64
// specials inside (Infinity/NaN), 8 negative numbers, 9 map sorted by key,
// 10 empty map, 11 i64-keyed map, 12 map String values quoted, 13 format {}
// interop, 14 println of the result, 15 class-element array rejected,
// 16 nested array rejected, 17 map with class values rejected, 18 toString
// with arguments rejected, 19 result usable in concatenation, 20 GC stress.

#[test]
fn tostr_01_i64_array() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[1, 2, 3]\n");
}

#[test]
fn tostr_02_string_array_quoted() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<String> = [\"a\", \"b c\"]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[\"a\", \"b c\"]\n");
}

#[test]
fn tostr_03_f64_array() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<f64> = [0.5, 2.25]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[0.5, 2.25]\n");
}

#[test]
fn tostr_04_bool_array() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<bool> = [true, false]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[true, false]\n");
}

#[test]
fn tostr_05_empty_array() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = []; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[]\n");
}

#[test]
fn tostr_06_single_element() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [7]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[7]\n");
}

#[test]
fn tostr_07_f64_specials_inside() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let inf = 1.0 / 0.0; let nan = 0.0 / 0.0; let xs: Array<f64> = [inf, 0.0 - inf, nan, 0.0]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[Infinity, -Infinity, NaN, 0.0]\n");
}

#[test]
fn tostr_08_negative_numbers() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [0 - 5, 0, 5]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[-5, 0, 5]\n");
}

#[test]
fn tostr_09_map_sorted_by_key() {
    let (out, ok) = compile_and_run(
        "import std::collections::Map;\nfn main() { let m: Map<String, i64> = Map::new(); m.insert(\"b\", 2); m.insert(\"a\", 1); m.insert(\"c\", 3); println(m.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "{a: 1, b: 2, c: 3}\n");
}

#[test]
fn tostr_10_empty_map() {
    let (out, ok) = compile_and_run(
        "import std::collections::Map;\nfn main() { let m: Map<String, i64> = Map::new(); println(m.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "{}\n");
}

#[test]
fn tostr_11_i64_keyed_map() {
    let (out, ok) = compile_and_run(
        "import std::collections::Map;\nfn main() { let m: Map<i64, String> = Map::new(); m.insert(2, \"b\"); m.insert(1, \"a\"); println(m.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "{1: \"a\", 2: \"b\"}\n");
}

#[test]
fn tostr_12_map_string_values_quoted() {
    let (out, ok) = compile_and_run(
        "import std::collections::Map;\nfn main() { let m: Map<String, String> = Map::new(); m.insert(\"k\", \"v w\"); println(m.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "{k: \"v w\"}\n");
}

#[test]
fn tostr_13_format_interop() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2]; println(format(\"xs = {}\", xs.toString())); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "xs = [1, 2]\n");
}

#[test]
fn tostr_14_println_direct() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { println([10, 20].toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[10, 20]\n");
}

#[test]
fn tostr_15_class_element_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::collections::Array;\nclass P { pub x: i64; }\nfn main() { let ps: Array<P> = [new P(1)]; println(ps.toString()); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E1402"), "{stderr}");
}

#[test]
fn tostr_16_nested_array_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::collections::Array;\nfn main() { let xs: Array<Array<i64>> = [[1]]; println(xs.toString()); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E1402"), "{stderr}");
}

#[test]
fn tostr_17_map_class_values_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::collections::Map;\nclass P { pub x: i64; }\nfn main() { let m: Map<String, P> = Map::new(); println(m.toString()); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E1402"), "{stderr}");
}

#[test]
fn tostr_18_arguments_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1]; println(xs.toString(2)); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("no arguments"), "{stderr}");
}

#[test]
fn tostr_19_concatenation() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1]; println(\"xs: \" + xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "xs: [1]\n");
}

#[test]
fn tostr_20_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        "import std::collections::Array;\nfn main() { let xs: Array<String> = [\"a\", \"b\", \"c\"]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[\"a\", \"b\", \"c\"]\n");
}

// ── Array for-loop inline element access (willow-pcoy) ──────────────────────
// The loop header now loads len from the handle and the body loads the
// element through a re-read buffer pointer (no willow_array_len/get calls).
// 20 perspectives: 1 i64 sum, 2 String elements (GC-managed), 3 f64
// elements, 4 bool elements, 5 empty array body never runs, 6 single
// element, 7 push DURING iteration is observed (len re-read), 8 pop DURING
// iteration shrinks the walk, 9 growth reallocation mid-iteration (buffer
// pointer re-read), 10 nested loops over the same array, 11 `_` binding
// (no element read), 12 loop variable is a copy (mutating array after read
// does not change it), 13 class elements, 14 large array, 15 two sequential
// loops same array, 16 for inside async fn, 17 GC stress with string
// elements, 18 GC stress with growth mid-iteration, 19 element order
// preserved, 20 loop over freshly returned array expression.

#[test]
fn afor_01_i64_sum() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3, 4]; let mut s = 0; for x in xs { s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}

#[test]
fn afor_02_string_elements() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<String> = [\"a\", \"b\"]; for x in xs { println(x); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a\nb\n");
}

#[test]
fn afor_03_f64_elements() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<f64> = [0.5, 1.25]; let mut s = 0.0; for x in xs { s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1.75\n");
}

#[test]
fn afor_04_bool_elements() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<bool> = [true, false, true]; let mut n = 0; for b in xs { if b { n = n + 1; } } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn afor_05_empty_never_runs() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = []; for x in xs { println(x); } println(9); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn afor_06_single_element() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [7]; for x in xs { println(x); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn afor_07_push_during_iteration_observed() {
    // len is re-read each entry: pushing while below 3 extends the walk.
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1]; let mut n = 0; for x in xs { n = n + 1; if n < 3 { xs.push(n * 10); } } println(n); println(xs.len()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n3\n");
}

#[test]
fn afor_08_pop_during_iteration_shrinks() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3, 4, 5, 6]; let mut n = 0; for x in xs { n = n + 1; xs.pop(); } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn afor_09_growth_realloc_mid_iteration() {
    // Start at cap 1; pushes force buffer reallocation while iterating —
    // subsequent element reads must go through the NEW buffer.
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [10]; let mut i = 0; for x in xs { if i < 7 { xs.push(x + 1); } i = i + 1; } println(xs.len()); let mut s = 0; for x in xs { s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n108\n");
}

#[test]
fn afor_10_nested_same_array() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2]; let mut s = 0; for a in xs { for b in xs { s = s + a * b; } } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn afor_11_underscore_binding() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3]; let mut n = 0; for _ in xs { n = n + 1; } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn afor_12_loop_var_is_copy() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [5, 6]; for x in xs { xs[0] = 99; println(x); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n6\n");
}

#[test]
fn afor_13_class_elements() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nclass P { pub v: i64; }\nfn main() { let xs: Array<P> = [new P(1), new P(2)]; let mut s = 0; for p in xs { s = s + p.v; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn afor_14_large_array() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = []; let mut i = 0; while i < 10000 { xs.push(i); i = i + 1; } let mut s = 0; for x in xs { s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "49995000\n");
}

#[test]
fn afor_15_two_sequential_loops() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3]; let mut a = 0; for x in xs { a = a + x; } let mut b = 0; for x in xs { b = b + x * 2; } println(a + b); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "18\n");
}

#[test]
fn afor_16_inside_async_fn() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nasync fn work() -> i64 { let xs: Array<i64> = [1, 2, 3]; let mut s = 0; for x in xs { s = s + x; } return s; }\nasync fn main() { println(await work()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn afor_17_gc_stress_strings() {
    let (out, ok) = compile_and_run_gc_stress(
        "import std::collections::Array;\nfn main() { let xs: Array<String> = [\"x\", \"y\", \"z\"]; let mut out = \"\"; for s in xs { out = out + s; } println(out); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "xyz\n");
}

#[test]
fn afor_18_gc_stress_growth() {
    let (out, ok) = compile_and_run_gc_stress(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1]; let mut i = 0; for x in xs { if i < 20 { xs.push(x + 1); } i = i + 1; } println(xs.len()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "21\n");
}

#[test]
fn afor_19_order_preserved() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [3, 1, 2]; for x in xs { println(x); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n1\n2\n");
}

#[test]
fn afor_20_fresh_array_expression() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn make() -> Array<i64> { let xs: Array<i64> = [4, 5]; return xs; }\nfn main() { let mut s = 0; for x in make() { s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

// ── break / continue (willow-kzka) ──────────────────────────────────────────
// 20 perspectives: 1 break in while, 2 break in range-for, 3 break in
// array-for, 4 continue in while (cond re-evaluated), 5 continue in
// range-for STILL INCREMENTS, 6 continue in array-for still advances,
// 7 nested loops: break exits inner only, 8 nested loops: continue targets
// inner, 9 break outside loop = E0904, 10 continue outside loop = E0904,
// 11 break inside lambda body does not see enclosing loop = E0904,
// 12 break under if/else, 13 break inside match arm inside loop, 14 `while
// true` terminated only by break, 15 break on first iteration (body once),
// 16 GC-managed temps + break (root balance under stress), 17 async
// range-for break+continue across awaits, 18 async while break across
// awaits, 19 three-level nesting inner break/continue, 20 mixed
// break+return in the same loop body.

#[test]
fn brk_01_while() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut n = 0; while n < 100 { n = n + 1; if n == 5 { break; } } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn brk_02_range_for() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut s = 0; for i in 0..100 { if i == 4 { break; } s = s + i; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn brk_03_array_for() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3, 4]; let mut s = 0; for x in xs { if x == 3 { break; } s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn brk_04_continue_while() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut n = 0; let mut s = 0; while n < 6 { n = n + 1; if n == 3 { continue; } s = s + n; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "18\n");
}

#[test]
fn brk_05_continue_range_for_increments() {
    // Skipping i==2 must still advance the induction variable (no hang).
    let (out, ok) = compile_and_run(
        "fn main() { let mut s = 0; for i in 0..5 { if i == 2 { continue; } s = s + i; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn brk_06_continue_array_for_advances() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3, 4]; let mut s = 0; for x in xs { if x == 2 { continue; } s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn brk_07_nested_break_inner_only() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut c = 0; for i in 0..3 { for j in 0..10 { if j == 2 { break; } c = c + 1; } } println(c); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn brk_08_nested_continue_inner() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut c = 0; for i in 0..3 { for j in 0..4 { if j == 1 { continue; } c = c + 1; } } println(c); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn brk_09_break_outside_loop_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { break; }", &[]);
    assert!(!ok);
    assert!(stderr.contains("E0904"), "{stderr}");
}

#[test]
fn brk_10_continue_outside_loop_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { if true { continue; } }", &[]);
    assert!(!ok);
    assert!(stderr.contains("E0904"), "{stderr}");
}

#[test]
fn brk_11_lambda_is_a_loop_boundary() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn main() { for i in 0..3 { let f = || { break; 1 }; } }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E0904"), "{stderr}");
}

#[test]
fn brk_12_under_if_else() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut n = 0; while true { if n > 3 { break; } else { n = n + 1; } } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn brk_13_inside_match_arm() {
    let (out, ok) = compile_and_run(
        "enum Sig { Go, Stop, }\nfn main() { let mut n = 0; for i in 0..10 { let s = i < 3 ? Sig::Go : Sig::Stop; match s { Go => { n = n + 1; } Stop => { break; } } } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn brk_14_while_true_break_only_exit() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut n = 1; while true { n = n * 2; if n > 50 { break; } } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "64\n");
}

#[test]
fn brk_15_first_iteration() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut c = 0; for i in 0..100 { c = c + 1; break; } println(c); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn brk_16_gc_roots_balanced_on_break() {
    let (out, ok) = compile_and_run_gc_stress(
        "import std::collections::Array;\nfn main() { let xs: Array<String> = [\"a\", \"b\", \"c\", \"d\"]; let mut n = 0; for s in xs { let t = s + \"!\"; println(t); n = n + 1; if n >= 2 { break; } } println(\"end\" + \"!\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!\nb!\nend!\n");
}

#[test]
fn brk_17_async_range_for() {
    let (out, ok) = compile_and_run(
        "async fn main() { let mut n = 0; for i in 0..10 { await sleep(1); if i == 2 { continue; } if i == 5 { break; } n = n + i; } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
}

#[test]
fn brk_18_async_while() {
    let (out, ok) = compile_and_run(
        "async fn main() { let mut m = 0; while m < 100 { await sleep(1); m = m + 1; if m == 4 { break; } } println(m); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn brk_19_three_level_nesting() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut c = 0; for a in 0..2 { for b in 0..3 { for d in 0..10 { if d == 1 { break; } if b == 1 { continue; } c = c + 1; } } } println(c); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn brk_20_break_and_return_same_loop() {
    let (out, ok) = compile_and_run(
        "fn f(stop_early: bool) -> i64 { for i in 0..10 { if stop_early { return 100; } if i == 3 { break; } } return 1; }\nfn main() { println(f(true)); println(f(false)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "100\n1\n");
}

// ── String content equality (willow-rpxh) ───────────────────────────────────
// == / != on String compare CONTENT via willow_string_eq, never pointers.
// 20 perspectives: 1 concat==literal (the original bug), 2 != inverse,
// 3 different strings unequal, 4 empty==empty, 5 empty vs non-empty,
// 6 literal==literal, 7 same variable both sides, 8 case sensitivity,
// 9 multibyte UTF-8, 10 prefix is not equal, 11 Option::None predicate,
// 12 Option::Some predicate pair, 13 comparison drives if, 14 comparison
// drives while exit, 15 == inside lambda, 16 interpolated/format result ==
// literal, 17 GC stress (rhs allocation during comparison, lhs rooted),
// 18 chained comparisons via bools, 19 array element == literal (break
// scenario from willow-kzka), 20 long strings differing at the last byte.

#[test]
fn streq_01_concat_vs_literal() {
    let (out, ok) = compile_and_run("fn main() { let a = \"c\" + \"!\"; println(a == \"c!\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_02_ne_inverse() {
    let (out, ok) = compile_and_run(
        "fn main() { let a = \"c\" + \"!\"; println(a != \"c!\"); println(a != \"c?\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\ntrue\n");
}

#[test]
fn streq_03_different_unequal() {
    let (out, ok) = compile_and_run("fn main() { println(\"x\" == \"y\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn streq_04_empty_eq_empty() {
    let (out, ok) = compile_and_run("fn main() { let e = \"\"; println(e == \"\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_05_empty_vs_nonempty() {
    let (out, ok) = compile_and_run("fn main() { println(\"\" == \"a\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn streq_06_literal_literal() {
    let (out, ok) = compile_and_run("fn main() { println(\"abc\" == \"abc\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_07_same_variable() {
    let (out, ok) = compile_and_run("fn main() { let s = \"q\" + \"r\"; println(s == s); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_08_case_sensitive() {
    let (out, ok) = compile_and_run("fn main() { println(\"Abc\" == \"abc\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn streq_09_multibyte_utf8() {
    let (out, ok) = compile_and_run(
        "fn main() { let s = \"日\" + \"本\"; println(s == \"日本\"); println(s == \"日体\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn streq_10_prefix_not_equal() {
    let (out, ok) = compile_and_run("fn main() { let s = \"ab\" + \"\"; println(s == \"abc\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn streq_11_option_none_predicate() {
    let (out, ok) =
        compile_and_run("fn main() { let s: Option<String> = None; println(s.is_none()); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_12_option_some_predicates() {
    let (out, ok) = compile_and_run(
        "fn main() { let t: Option<String> = Some(\"hi\"); println(t.is_none()); println(t.is_some()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\ntrue\n");
}

#[test]
fn streq_13_drives_if() {
    let (out, ok) = compile_and_run(
        "fn main() { let w = \"wil\" + \"low\"; if w == \"willow\" { println(1); } else { println(0); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn streq_14_drives_while_exit() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut s = \"\"; let mut n = 0; while s != \"aaa\" { s = s + \"a\"; n = n + 1; } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn streq_15_inside_lambda() {
    let (out, ok) = compile_and_run(
        "fn main() { let f = |s: String| s == \"ok\"; println(f(\"o\" + \"k\")); println(f(\"no\")); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn streq_16_format_result() {
    let (out, ok) =
        compile_and_run("fn main() { let s = format(\"n = {}\", 5); println(s == \"n = 5\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_17_gc_stress_rhs_allocates() {
    // rhs concat allocates during the comparison; lhs must stay rooted.
    let (out, ok) = compile_and_run_gc_stress(
        "fn main() { let a = \"x\" + \"y\"; println(a == (\"x\" + \"y\")); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_18_chained_bools() {
    let (out, ok) = compile_and_run(
        "fn main() { let a = \"p\" + \"q\"; let b = a == \"pq\" && \"r\" == \"r\"; println(b); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_19_array_element_break_scenario() {
    // The willow-kzka discovery case: break on a content match now fires.
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<String> = [\"a\", \"b\", \"c\", \"d\"]; for s in xs { let t = s + \"!\"; if t == \"c!\" { break; } println(t); } println(\"done\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!\nb!\ndone\n");
}

#[test]
fn streq_20_long_last_byte_differs() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut a = \"\"; let mut b = \"\"; let mut i = 0; while i < 200 { a = a + \"z\"; b = b + \"z\"; i = i + 1; } println(a == b); println((a + \"1\") == (b + \"2\")); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\nfalse\n");
}

// Review fix for brk_18's loop shape: in an async while, `continue` must go
// through the safepoint back edge (dedicated cont block), not straight to
// the header — a continue BEFORE any await in the body must still let the
// scheduler run (no busy-loop past suspension points).

#[test]
fn brk_21_async_while_continue_before_await() {
    let (out, ok) = compile_and_run(
        "async fn side() -> i64 { await sleep(5); return 7; }\nasync fn main() { let t = side(); let mut i = 0; let mut s = 0; while i < 500 { i = i + 1; if i % 2 == 0 { s = s + 1; continue; } await sleep(0); } println(s); println(await t); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "250\n7\n");
}

#[test]
fn brk_22_async_while_continue_only_still_terminates() {
    // EVERY iteration continues before the await: the loop must still
    // terminate and the safepoint edge must not corrupt the frame state.
    let (out, ok) = compile_and_run(
        "async fn main() { let mut i = 0; while i < 100 { i = i + 1; if true { continue; } await sleep(1); } println(i); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "100\n");
}

// ── defer (willow-vynv.2, sync Phase 2) ─────────────────────────────────────
// Scope-based (Zig-style, per-iteration in loops), LIFO, operands evaluated
// at registration. Runs on: fallthrough, return, `?`, break, continue. Not
// on panic (abort, no unwind). 20 perspectives: 1 runs at scope end, 2 LIFO
// order, 3 runs before return's caller observes (value computed first),
// 4 args evaluated at registration, 5 receiver evaluated at registration,
// 6 method-call defer, 7 print defer, 8 `?` propagation runs defers,
// 9 Ok-path `?` does NOT early-run them, 10 break flushes loop-scope
// defers, 11 continue flushes per-iteration defers, 12 loop body defer runs
// EACH iteration, 13 nested scopes flush inner-first, 14 inner-scope defer
// runs at inner exit not fn end, 15 non-call rejected E0905, 16 async fn
// defer rejected E0905, 17 GC stress with String operands (rooted slots),
// 18 defer in main with Result main, 19 class method named `close` resolves
// to the class (not the channel builtin) — found via defer, 20 defer does
// NOT run on panic.

#[test]
fn dfr_01_scope_end() {
    let (out, ok) = compile_and_run("fn c() { println(2); }\nfn main() { defer c(); println(1); }");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn dfr_02_lifo() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nfn main() { defer c(1); defer c(2); defer c(3); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n2\n1\n");
}

#[test]
fn dfr_03_return_value_computed_first() {
    let (out, ok) = compile_and_run(
        "fn c() { println(8); }\nfn f() -> i64 { defer c(); return 5; }\nfn main() { println(f()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n5\n");
}

#[test]
fn dfr_04_args_evaluated_at_registration() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nfn main() { let mut x = 1; defer c(x); x = 99; println(x); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n1\n");
}

#[test]
fn dfr_05_receiver_evaluated_at_registration() {
    let (out, ok) = compile_and_run(
        "class R { pub v: i64; pub fn show(self) { println(self.v); } }\nfn main() { let mut r = new R(1); defer r.show(); r = new R(2); println(r.v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n1\n");
}

#[test]
fn dfr_06_method_call() {
    let (out, ok) = compile_and_run(
        "class Res { pub name: String; pub fn close(self) { println(\"closed \" + self.name); } }\nfn main() { let r = new Res(\"db\"); defer r.close(); println(\"work\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "work\nclosed db\n");
}

#[test]
fn dfr_07_print_defer() {
    let (out, ok) = compile_and_run("fn main() { let x = 5; defer println(x); println(1); }");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n5\n");
}

#[test]
fn dfr_08_try_propagation_runs_defers() {
    let (out, ok) = compile_and_run(
        "fn c() { println(77); }\nfn bad() -> Result<i64, String> { return Err(\"e\"); }\nfn f() -> Result<i64, String> { defer c(); let x = bad()?; return Ok(x); }\nfn main() { match f() { Ok(v) => println(v), Err(e) => println(e), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "77\ne\n");
}

#[test]
fn dfr_09_ok_path_no_early_run() {
    let (out, ok) = compile_and_run(
        "fn c() { println(77); }\nfn good() -> Result<i64, String> { return Ok(3); }\nfn f() -> Result<i64, String> { defer c(); let x = good()?; println(x); return Ok(x); }\nfn main() { match f() { Ok(v) => println(v), Err(e) => println(e), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n77\n3\n");
}

#[test]
fn dfr_10_break_flushes() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nfn main() { for i in 0..5 { defer c(i); if i == 1 { break; } } println(9); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n9\n");
}

#[test]
fn dfr_11_continue_flushes() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n * 10); }\nfn main() { for i in 0..3 { defer c(i); if i == 1 { continue; } println(i); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n0\n10\n2\n20\n");
}

#[test]
fn dfr_12_loop_body_per_iteration() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nfn main() { for i in 0..3 { defer c(i); } println(9); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n9\n");
}

#[test]
fn dfr_13_nested_scopes_inner_first_on_return() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nfn f() { defer c(1); if true { defer c(2); return; } }\nfn main() { f(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n1\n");
}

#[test]
fn dfr_14_inner_scope_exits_early() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nfn main() { if true { defer c(1); println(0); } println(2); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n");
}

#[test]
fn dfr_15_non_call_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { defer 1 + 2; }", &[]);
    assert!(!ok);
    assert!(stderr.contains("E0905"), "{stderr}");
}

#[test]
fn dfr_16_async_defer_now_allowed() {
    // Phase 3 (willow-vynv.3) lifted the async restriction: defer in an
    // async fn registers into the frame and flushes on exit.
    let (out, ok) = compile_and_run("fn c() { println(2); }\nfn main() { defer c(); println(1); }");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn dfr_17_gc_stress_string_operands() {
    let (out, ok) = compile_and_run_gc_stress(
        "fn c(s: String) { println(s); }\nfn main() { let name = \"a\" + \"b\"; defer c(name + \"!\"); println(\"work\" + \"s\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "works\nab!\n");
}

#[test]
fn dfr_18_result_main() {
    let (out, ok) = compile_and_run(
        "fn c() { println(1); }\nfn main() -> Result<void, String> { defer c(); println(0); return Result::Ok(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n");
}

#[test]
fn dfr_19_class_close_not_channel_builtin() {
    let (out, ok) = compile_and_run(
        "class C { pub fn close(self) { println(4); } }\nfn main() { let c = new C(); defer c.close(); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n4\n");
}

#[test]
fn dfr_20_runs_before_unhandled_panic_finishes() {
    let (out, ok) = compile_and_run_check_exit(
        "fn c() { println(111); }\nfn main() { defer c(); panic(\"stop\"); }",
    );
    assert!(!ok);
    assert!(
        out.contains("111"),
        "defer must run during panic unwind: {out}"
    );
    assert!(out.contains("runtime panic: stop"), "{out}");
}

// Review fixes on sync defer (willow-vynv.2): two silent-misbehavior holes
// closed as compile errors. 21 reference args would mutate the hidden
// registration-time COPY, not the caller's variable; 22 an async callee
// would only SPAWN a task at scope exit (cleanup body never driven);
// 23 same for Future-returning runtime calls (sleep).

#[test]
fn dfr_21_reference_arg_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn inc(n: &mut i64) { n = n + 1; }\nfn main() { let mut n = 1; if true { defer inc(&n); } println(n); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E0905"), "{stderr}");
    assert!(stderr.contains("reference arguments"), "{stderr}");
}

#[test]
fn dfr_22_async_callee_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn cleanup() { println(9); }\nfn main() { defer cleanup(); println(1); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("async call"), "{stderr}");
}

#[test]
fn dfr_23_future_callee_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { defer sleep(5); }", &[]);
    assert!(!ok);
    assert!(stderr.contains("async call"), "{stderr}");
}

fn assert_dfr_runs(source: &str, expected: &str) {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

fn assert_dfr_runs_gc_stress(source: &str, expected: &str) {
    let (out, ok) = compile_and_run_gc_stress(source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

fn assert_dfr_compile_fails(source: &str) {
    let (ok, stderr) = compile_with_compiler_env(source, &[]);
    assert!(!ok, "expected compile failure, stderr:\n{stderr}");
}

fn assert_dfr_compile_error(source: &str, needle: &str) {
    let (ok, stderr) = compile_with_compiler_env(source, &[]);
    assert!(!ok, "expected compile failure, stderr:\n{stderr}");
    assert!(stderr.contains(needle), "{stderr}");
}

fn assert_dfr_exit_contains(source: &str, needles: &[&str]) {
    let (out, ok) = compile_and_run_check_exit(source);
    assert!(!ok, "{out}");
    for needle in needles {
        assert!(out.contains(needle), "{out}");
    }
}

#[test]
fn dfr_24_missing_semicolon_rejected() {
    assert_dfr_compile_error("fn c() {}\nfn main() { defer c() }", "E0101");
}

#[test]
fn dfr_25_empty_defer_rejected() {
    assert_dfr_compile_fails("fn main() { defer; }");
}

#[test]
fn dfr_26_variable_defer_rejected() {
    assert_dfr_compile_error("fn main() { let x = 1; defer x; }", "E0905");
}

#[test]
fn dfr_27_match_expr_defer_allowed() {
    assert_dfr_runs(
        "fn main() { defer match 1 { _ => println(2), }; println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_28_unknown_function_rejected() {
    assert_dfr_compile_error("fn main() { defer missing_cleanup(); }", "E0350");
}

#[test]
fn dfr_29_wrong_arg_count_rejected() {
    assert_dfr_compile_error("fn c(n: i64) {}\nfn main() { defer c(); }", "E0201");
}

#[test]
fn dfr_30_wrong_arg_type_rejected() {
    assert_dfr_compile_error("fn c(n: i64) {}\nfn main() { defer c(true); }", "E0201");
}

#[test]
fn dfr_31_method_not_found_rejected() {
    assert_dfr_compile_error(
        "class C {}\nfn main() { let c = new C(); defer c.close(); }",
        "E0502",
    );
}

#[test]
fn dfr_32_print_without_newline() {
    assert_dfr_runs("fn main() { defer print(5); println(1); }", "1\n5");
}

#[test]
fn dfr_33_sync_lambda_body() {
    assert_dfr_runs(
        "fn main() { let f = || { defer println(2); println(1); }; f(); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_34_constructor_body() {
    assert_dfr_runs(
        "class R { pub init(self) { defer println(2); println(1); } }\nfn main() { let r = new R(); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_35_static_method_body() {
    assert_dfr_runs(
        "class R { pub static fn run() { defer println(2); println(1); } }\nfn main() { R::run(); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_36_empty_block_no_effect() {
    assert_dfr_runs("fn main() { if true {} println(1); }", "1\n");
}

#[test]
fn dfr_37_if_then_only() {
    assert_dfr_runs(
        "fn main() { if true { defer println(1); } else { defer println(2); } println(3); }",
        "1\n3\n",
    );
}

#[test]
fn dfr_38_if_else_only() {
    assert_dfr_runs(
        "fn main() { if false { defer println(1); } else { defer println(2); } println(3); }",
        "2\n3\n",
    );
}

#[test]
fn dfr_39_branch_selection_does_not_register_other_side() {
    assert_dfr_runs(
        "fn main() { let flag = false; if flag { defer println(1); } else { println(2); } println(3); }",
        "2\n3\n",
    );
}

#[test]
fn dfr_40_branch_defer_before_outer_defer() {
    assert_dfr_runs(
        "fn main() { defer println(9); if true { defer println(1); } println(2); }",
        "1\n2\n9\n",
    );
}

#[test]
fn dfr_41_nested_if_lifo() {
    assert_dfr_runs(
        "fn main() { defer println(9); if true { defer println(1); if true { defer println(2); } } }",
        "2\n1\n9\n",
    );
}

#[test]
fn dfr_42_bare_return_flushes() {
    assert_dfr_runs(
        "fn f() { defer println(1); return; println(9); }\nfn main() { f(); println(2); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_43_void_return_flushes() {
    assert_dfr_runs(
        "fn f() { defer println(1); if true { return; } println(9); }\nfn main() { f(); println(2); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_44_gc_return_value_survives_defer() {
    assert_dfr_runs_gc_stress(
        "fn cleanup() { println(\"cleanup\"); }\nfn f() -> String { let s = \"a\" + \"b\"; defer cleanup(); return s + \"!\"; }\nfn main() { println(f()); }",
        "cleanup\nab!\n",
    );
}

#[test]
fn dfr_45_result_return_err_flushes() {
    assert_dfr_runs(
        "fn cleanup() { println(1); }\nfn f() -> Result<i64, String> { defer cleanup(); return Result::Err(\"bad\"); }\nfn main() { match f() { Result::Ok(v) => println(v), Result::Err(e) => println(e), } }",
        "1\nbad\n",
    );
}

#[test]
fn dfr_46_result_main_err_flushes() {
    assert_dfr_exit_contains(
        "fn main() -> Result<void, String> { defer println(1); return Result::Err(\"bad\"); }",
        &["1\n", "bad"],
    );
}

#[test]
fn dfr_47_option_none_try_flushes() {
    assert_dfr_runs(
        "fn cleanup() { println(1); }\nfn none() -> Option<i64> { return Option::None; }\nfn f() -> Option<i64> { defer cleanup(); let v = none()?; return Option::Some(v); }\nfn main() { match f() { Option::Some(v) => println(v), Option::None => println(0), } }",
        "1\n0\n",
    );
}

#[test]
fn dfr_48_option_some_try_no_early_flush() {
    assert_dfr_runs(
        "fn cleanup() { println(1); }\nfn some() -> Option<i64> { return Option::Some(3); }\nfn f() -> Option<i64> { defer cleanup(); let v = some()?; println(v); return Option::Some(v); }\nfn main() { match f() { Option::Some(v) => println(v), Option::None => println(0), } }",
        "3\n1\n3\n",
    );
}

#[test]
fn dfr_49_callee_try_does_not_flush_caller() {
    assert_dfr_runs(
        "fn cleanup(n: i64) { println(n); }\nfn bad() -> Result<i64, String> { return Result::Err(\"e\"); }\nfn inner() -> Result<i64, String> { defer cleanup(1); let v = bad()?; return Result::Ok(v); }\nfn outer() { defer cleanup(9); match inner() { Result::Ok(v) => println(v), Result::Err(e) => println(e), } println(0); }\nfn main() { outer(); }",
        "1\ne\n0\n9\n",
    );
}

#[test]
fn dfr_50_match_arm_return_flushes() {
    assert_dfr_runs(
        "fn f(n: i64) { defer println(9); match n { 0 => { defer println(1); return; }, _ => println(3), } }\nfn main() { f(0); }",
        "1\n9\n",
    );
}

#[test]
fn dfr_51_match_arm_try_flushes() {
    assert_dfr_runs(
        "fn bad() -> Result<i64, String> { return Result::Err(\"e\"); }\nfn f(n: i64) -> Result<i64, String> { defer println(9); match n { 0 => { defer println(1); let x = bad()?; return Result::Ok(x); } _ => { } } return Result::Ok(2); }\nfn main() { match f(0) { Result::Ok(v) => println(v), Result::Err(e) => println(e), } }",
        "1\n9\ne\n",
    );
}

#[test]
fn dfr_52_match_normal_path_waits_for_scope_exit() {
    assert_dfr_runs(
        "fn main() { match 1 { 1 => { defer println(1); println(7); } _ => { } } println(2); }",
        "7\n1\n2\n",
    );
}

#[test]
fn dfr_53_try_error_conversion_runs_defer() {
    assert_dfr_runs(
        "class HighErr { pub code: i64; }\nclass LowErr implements Into<HighErr> { pub n: i64; pub fn into(self) -> HighErr { return new HighErr(self.n + 10); } }\nfn low() -> Result<i64, LowErr> { return Result::Err(new LowErr(5)); }\nfn high() -> Result<i64, HighErr> { defer println(1); let v = low()?; return Result::Ok(v); }\nfn main() { match high() { Result::Ok(v) => println(v), Result::Err(e) => println(e.code), } }",
        "1\n15\n",
    );
}

#[test]
fn dfr_54_deferred_function_panic_reports_after_prior_output() {
    assert_dfr_exit_contains(
        "fn cleanup() { panic(\"boom\"); }\nfn main() { defer cleanup(); println(1); }",
        &["1\n", "boom"],
    );
}

#[test]
fn dfr_55_registration_panic_prevents_later_statements() {
    assert_dfr_exit_contains(
        "fn arg() -> i64 { panic(\"arg\"); return 0; }\nfn cleanup(n: i64) { println(n); }\nfn main() { defer cleanup(arg()); println(1); }",
        &["arg"],
    );
}

#[test]
fn dfr_56_try_in_direct_defer_argument_rejected() {
    assert_dfr_compile_error(
        "fn cleanup(n: i64) { println(n); }\nfn bad() -> Result<i64, String> { return Result::Err(\"e\"); }\nfn f() -> Result<void, String> { defer cleanup(1); defer cleanup(bad()?); return Result::Ok(); }\nfn main() { match f() { Result::Ok(_) => println(0), Result::Err(e) => println(e), } }",
        "E0905",
    );
}

#[test]
fn dfr_57_for_wildcard_flushes_each_iteration() {
    assert_dfr_runs(
        "fn main() { for _ in 0..2 { defer println(7); } println(9); }",
        "7\n7\n9\n",
    );
}

#[test]
fn dfr_58_while_body_per_iteration() {
    assert_dfr_runs(
        "fn main() { let mut i = 0; while i < 2 { defer println(i); i = i + 1; } println(9); }",
        "0\n1\n9\n",
    );
}

#[test]
fn dfr_59_while_break_flushes() {
    assert_dfr_runs(
        "fn main() { let mut i = 0; while true { defer println(i); if i == 1 { break; } i = i + 1; } println(9); }",
        "0\n1\n9\n",
    );
}

#[test]
fn dfr_60_while_continue_flushes() {
    assert_dfr_runs(
        "fn main() { let mut i = 0; while i < 3 { defer println(i); i = i + 1; if i == 2 { continue; } println(8); } }",
        "8\n0\n1\n8\n2\n",
    );
}

#[test]
fn dfr_61_nested_loop_break_flushes_inner_only() {
    assert_dfr_runs(
        "fn c(n: i64) { println(n); }\nfn main() { defer c(9); for i in 0..2 { defer c(10 + i); for j in 0..2 { defer c(j); break; } println(5); } }",
        "0\n5\n10\n0\n5\n11\n9\n",
    );
}

#[test]
fn dfr_62_nested_loop_continue_flushes_inner_only() {
    assert_dfr_runs(
        "fn c(n: i64) { println(n); }\nfn main() { for i in 0..1 { defer c(9); for j in 0..2 { defer c(j); if j == 0 { continue; } println(7); } println(8); } }",
        "0\n7\n1\n8\n9\n",
    );
}

#[test]
fn dfr_63_inner_block_defer_runs_before_loop_defer() {
    assert_dfr_runs(
        "fn main() { for _ in 0..1 { defer println(1); if true { defer println(2); } } }",
        "2\n1\n",
    );
}

#[test]
fn dfr_64_range_for_captures_iteration_value() {
    assert_dfr_runs(
        "fn main() { for i in 0..3 { defer println(i); } println(9); }",
        "0\n1\n2\n9\n",
    );
}

#[test]
fn dfr_65_array_for_defer() {
    assert_dfr_runs(
        "fn main() { let a = [4, 5]; for x in a { defer println(x); } println(9); }",
        "4\n5\n9\n",
    );
}

#[test]
fn dfr_66_array_for_continue_defer() {
    assert_dfr_runs(
        "fn main() { let a = [1, 2]; for x in a { defer println(x); continue; } println(9); }",
        "1\n2\n9\n",
    );
}

#[test]
fn dfr_67_string_argument() {
    assert_dfr_runs(
        "fn c(s: String) { println(s); }\nfn main() { let s = \"x\"; defer c(s + \"y\"); println(\"z\"); }",
        "z\nxy\n",
    );
}

#[test]
fn dfr_68_array_argument_len() {
    assert_dfr_runs(
        "import std::collections::Array;\nfn c(a: Array<i64>) { println(a.len()); }\nfn main() { let a: Array<i64> = [1, 2, 3]; defer c(a); println(0); }",
        "0\n3\n",
    );
}

#[test]
fn dfr_69_class_object_argument() {
    assert_dfr_runs(
        "class C { pub v: i64; }\nfn show(c: C) { println(c.v); }\nfn main() { let c = new C(4); defer show(c); println(1); }",
        "1\n4\n",
    );
}

#[test]
fn dfr_70_enum_payload_argument() {
    assert_dfr_runs(
        "enum Kind { Small, Big(i64) }\nfn show(k: Kind) { let out = match k { Kind::Big(v) => v, Kind::Small => 0, }; println(out); }\nfn main() { defer show(Kind::Big(7)); println(1); }",
        "1\n7\n",
    );
}

#[test]
fn dfr_71_option_object_after_match() {
    assert_dfr_runs(
        "class C { pub fn show(self) { println(2); } }\nfn main() { let x: Option<C> = Some(new C()); match x { Some(value) => { defer value.show(); println(1); }, None => {} } }",
        "1\n2\n",
    );
}

#[test]
fn dfr_72_interface_dispatch() {
    assert_dfr_runs(
        "interface I { fn show(self); }\nclass C implements I { pub fn show(self) { println(2); } }\nfn main() { let x: I = new C(); defer x.show(); println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_73_inherited_method_dispatch() {
    assert_dfr_runs(
        "open class Base { pub fn show(self) { println(2); } }\nclass Child extends Base {}\nfn main() { let x = new Child(); defer x.show(); println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_74_overridden_method_dispatch() {
    assert_dfr_runs(
        "open class Base { pub open fn show(self) { println(2); } }\nclass Child extends Base { pub override fn show(self) { println(3); } }\nfn main() { let x = new Child(); defer x.show(); println(1); }",
        "1\n3\n",
    );
}

#[test]
fn dfr_75_static_call_rejected() {
    assert_dfr_compile_error(
        "class C { pub static fn cleanup() {} }\nfn main() { defer C::cleanup(); }",
        "E0905",
    );
}

#[test]
fn dfr_76_static_constructor_call_rejected() {
    assert_dfr_compile_error("fn main() { defer Channel<i64>::new(); }", "E0905");
}

#[test]
fn dfr_77_indirect_function_value() {
    assert_dfr_runs(
        "fn c() { println(2); }\nfn main() { let f = c; defer f(); println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_78_lambda_value() {
    assert_dfr_runs(
        "fn main() { let f = || { println(2); }; defer f(); println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_79_multiple_argument_types() {
    assert_dfr_runs(
        "fn c(n: i64, b: bool, f: f64, s: String) { println(n); println(b); println(f); println(s); }\nfn main() { defer c(1, true, 2.5, \"x\"); println(0); }",
        "0\n1\ntrue\n2.5\nx\n",
    );
}

#[test]
fn dfr_80_method_argument_captured() {
    assert_dfr_runs(
        "class C { pub fn show(self, n: i64) { println(n); } }\nfn main() { let c = new C(); let mut n = 1; defer c.show(n); n = 2; println(n); }",
        "2\n1\n",
    );
}

#[test]
fn dfr_81_receiver_object_field_mutation_visible() {
    assert_dfr_runs(
        "class C { pub v: i64; pub fn show(self) { println(self.v); } }\nfn main() { let c = new C(1); defer c.show(); c.v = 2; println(0); }",
        "0\n2\n",
    );
}

#[test]
fn dfr_82_argument_side_effect_at_registration() {
    assert_dfr_runs(
        "fn mark(n: i64) -> i64 { println(n); return n; }\nfn c(n: i64) { println(n * 10); }\nfn main() { defer c(mark(1)); println(2); }",
        "1\n2\n10\n",
    );
}

#[test]
fn dfr_83_call_side_effect_at_exit() {
    assert_dfr_runs(
        "fn c() { println(2); }\nfn main() { defer c(); println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_84_deferred_return_value_ignored() {
    assert_dfr_runs(
        "fn c() -> i64 { println(2); return 9; }\nfn main() { defer c(); println(1); }",
        "1\n2\n",
    );
}

#[test]
fn dfr_85_print_bool_and_float() {
    assert_dfr_runs(
        "fn main() { defer println(true); defer println(2.5); println(1); }",
        "1\n2.5\ntrue\n",
    );
}

#[test]
fn dfr_86_none_branch_does_not_register_defer() {
    assert_dfr_runs(
        "class C { pub fn show(self) { println(2); } }\nfn main() { let x: Option<C> = None; match x { Some(value) => { defer value.show(); }, None => {} } println(1); }",
        "1\n",
    );
}

#[test]
fn dfr_87_to_string_argument() {
    assert_dfr_runs(
        "fn c(s: String) { println(s); }\nfn main() { let x = 5; defer c(x.toString()); println(1); }",
        "1\n5\n",
    );
}

#[test]
fn dfr_88_inner_scope_gc_stress_after_flush() {
    assert_dfr_runs_gc_stress(
        "fn c(s: String) { println(s); }\nfn main() { if true { let s = \"a\" + \"b\"; defer c(s); } println(\"c\" + \"d\"); }",
        "ab\ncd\n",
    );
}

#[test]
fn dfr_89_many_defers_lifo() {
    assert_dfr_runs(
        "fn main() { defer println(1); defer println(2); defer println(3); defer println(4); defer println(5); println(0); }",
        "0\n5\n4\n3\n2\n1\n",
    );
}

#[test]
fn dfr_90_hidden_name_collision_does_not_shadow_user_var() {
    assert_dfr_runs(
        "fn c(n: i64) { println(n); }\nfn main() { let __defer0_a0 = 7; defer c(1); println(__defer0_a0); }",
        "7\n1\n",
    );
}

#[test]
fn dfr_91_nested_function_scope_separation() {
    assert_dfr_runs(
        "fn c(n: i64) { println(n); }\nfn inner() { defer c(1); }\nfn main() { defer c(9); inner(); println(0); }",
        "1\n0\n9\n",
    );
}

#[test]
fn dfr_92_defer_in_if_then_return_flushes_inner_first() {
    assert_dfr_runs(
        "fn c(n: i64) { println(n); }\nfn f() { defer c(9); if true { defer c(1); return; } }\nfn main() { f(); }",
        "1\n9\n",
    );
}

#[test]
fn dfr_93_defer_after_loop_runs_after_loop_defers() {
    assert_dfr_runs(
        "fn main() { for i in 0..2 { defer println(i); } defer println(9); println(8); }",
        "0\n1\n8\n9\n",
    );
}

#[test]
fn dfr_94_for_break_outer_scope_defer_later() {
    assert_dfr_runs(
        "fn main() { defer println(9); for i in 0..3 { defer println(i); break; } println(8); }",
        "0\n8\n9\n",
    );
}

#[test]
fn dfr_95_continue_flushes_before_loop_increment_visible() {
    assert_dfr_runs(
        "fn main() { for i in 0..2 { defer println(i); continue; } println(9); }",
        "0\n1\n9\n",
    );
}

#[test]
fn dfr_96_result_ok_payload_return_value_computed_first() {
    assert_dfr_runs_gc_stress(
        "fn cleanup() { println(8); }\nfn f() -> Result<String, String> { let s = \"a\" + \"b\"; defer cleanup(); return Result::Ok(s + \"!\"); }\nfn main() { match f() { Result::Ok(v) => println(v), Result::Err(e) => println(e), } }",
        "8\nab!\n",
    );
}

#[test]
fn dfr_97_defer_argument_binary_expression_captured() {
    assert_dfr_runs(
        "fn c(n: i64) { println(n); }\nfn main() { let mut n = 1; defer c(n + 4); n = 9; println(n); }",
        "9\n5\n",
    );
}

#[test]
fn dfr_98_defer_method_receiver_expression_captured_once() {
    assert_dfr_runs(
        "class C { pub v: i64; pub fn show(self) { println(self.v); } }\nfn make(n: i64) -> C { println(n); return new C(n); }\nfn main() { defer make(1).show(); println(2); }",
        "1\n2\n1\n",
    );
}

#[test]
fn dfr_99_constructor_field_visible_to_defer() {
    assert_dfr_runs(
        "class C { pub v: i64; pub init(self, v: i64) { self.v = v; defer self.show(); println(1); } pub fn show(self) { println(self.v); } }\nfn main() { let c = new C(5); }",
        "1\n5\n",
    );
}

#[test]
fn dfr_100_reference_method_arg_rejected() {
    assert_dfr_compile_error(
        "class C { pub fn set(self, n: &mut i64) {} }\nfn main() { let c = new C(); let mut n = 1; defer c.set(&n); }",
        "reference arguments",
    );
}

// Explicit Result handling inside defer bodies (willow-oorh).
//
// 22 perspectives:
// 01 plain-call Err is discarded without panic/propagation
// 02 plain-call Ok is discarded
// 03 cleanup Err does not replace a parent Result return
// 04 deferred match handles Err after the body
// 05 deferred match selects Ok
// 06 deferred block can bind and match a Result
// 07 the value of the whole deferred match is discarded
// 08 call/match/block entries retain one shared LIFO order
// 09 direct-call argument side effects still happen at registration
// 10 match scrutinee evaluation is delayed until scope exit
// 11 `defer return ...` is E0905
// 12 return nested in a deferred block is E0905
// 13 return in a deferred match arm is E0905
// 14 `?` in a deferred block is E0905
// 15 `?` in a deferred match scrutinee is E0905
// 16 break in a deferred body is E0905 even inside an outer loop
// 17 continue in a deferred body is E0905 even inside an outer loop
// 18 await/select-style suspension in a deferred body is E0905
// 19 an async cleanup call nested in a body is E0905
// 20 async normal-exit and cancellation paths execute a registered body once
// 21 async cancellation can read a lexical value from its task frame
// 22 `?` in a direct-call receiver/argument is also E0905

const DEFER_RESULT_HELPER: &str = r#"
fn cleanup(ok: bool) -> Result<void, String> {
    if ok { return Ok(); }
    return Err("cleanup failed");
}
"#;

#[test]
fn defer_result_01_plain_err_is_ignored() {
    assert_dfr_runs(
        &format!("{DEFER_RESULT_HELPER}\nfn main() {{ defer cleanup(false); println(\"body\"); }}"),
        "body\n",
    );
}

#[test]
fn defer_result_02_plain_ok_is_ignored() {
    assert_dfr_runs(
        &format!("{DEFER_RESULT_HELPER}\nfn main() {{ defer cleanup(true); println(\"body\"); }}"),
        "body\n",
    );
}

#[test]
fn defer_result_03_cleanup_err_does_not_replace_parent_result() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn work() -> Result<i64, String> {{ defer cleanup(false); return Ok(7); }}\nfn main() {{ match work() {{ Ok(value) => println(value), Err(error) => println(error), }} }}"
        ),
        "7\n",
    );
}

#[test]
fn defer_result_04_match_handles_err_after_body() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn main() {{ defer match cleanup(false) {{ Ok(_) => println(\"ok\"), Err(error) => println(error), }} println(\"body\"); }}"
        ),
        "body\ncleanup failed\n",
    );
}

#[test]
fn defer_result_05_match_handles_ok() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn main() {{ defer match cleanup(true) {{ Ok(_) => println(\"ok\"), Err(error) => println(error), }} println(\"body\"); }}"
        ),
        "body\nok\n",
    );
}

#[test]
fn defer_result_06_block_binds_and_matches_result() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn main() {{ defer {{ let result = cleanup(false); match result {{ Ok(_) => println(\"ok\"), Err(error) => println(error), }} }} println(\"body\"); }}"
        ),
        "body\ncleanup failed\n",
    );
}

#[test]
fn defer_result_07_match_value_is_discarded() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn main() {{ defer match cleanup(false) {{ Ok(_) => 1, Err(_) => 2, }} println(\"body\"); }}"
        ),
        "body\n",
    );
}

#[test]
fn defer_result_08_mixed_forms_share_lifo_order() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn main() {{ defer println(1); defer match cleanup(false) {{ Ok(_) => println(2), Err(_) => println(3), }} defer {{ println(4); }} println(0); }}"
        ),
        "0\n4\n3\n1\n",
    );
}

#[test]
fn defer_result_09_direct_call_argument_still_evaluates_at_registration() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn mark() -> bool {{ println(\"register\"); return false; }}\nfn main() {{ defer cleanup(mark()); println(\"body\"); }}"
        ),
        "register\nbody\n",
    );
}

#[test]
fn defer_result_10_match_scrutinee_is_evaluated_at_exit() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn run() -> Result<void, String> {{ println(\"cleanup\"); return cleanup(false); }}\nfn main() {{ defer match run() {{ Ok(_) => {{}}, Err(error) => println(error), }} println(\"body\"); }}"
        ),
        "body\ncleanup\ncleanup failed\n",
    );
}

#[test]
fn defer_result_11_direct_return_is_e0905() {
    assert_dfr_compile_error(
        &format!("{DEFER_RESULT_HELPER}\nfn main() {{ defer return cleanup(false); }}"),
        "E0905",
    );
}

#[test]
fn defer_result_12_block_return_is_e0905() {
    assert_dfr_compile_error(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn work() -> Result<void, String> {{ defer {{ return cleanup(false); }} return Ok(); }}"
        ),
        "E0905",
    );
}

#[test]
fn defer_result_13_match_arm_return_is_e0905() {
    assert_dfr_compile_error(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn work() -> Result<void, String> {{ defer match cleanup(false) {{ Ok(_) => {{}}, Err(_) => return cleanup(true), }} return Ok(); }}"
        ),
        "E0905",
    );
}

#[test]
fn defer_result_14_block_try_is_e0905() {
    assert_dfr_compile_error(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn work() -> Result<void, String> {{ defer {{ cleanup(false)?; }} return Ok(); }}"
        ),
        "E0905",
    );
}

#[test]
fn defer_result_15_match_scrutinee_try_is_e0905() {
    assert_dfr_compile_error(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn work() -> Result<void, String> {{ defer match cleanup(false)? {{ _ => {{}} }} return Ok(); }}"
        ),
        "E0905",
    );
}

#[test]
fn defer_result_16_break_is_e0905_inside_outer_loop() {
    assert_dfr_compile_error("fn main() { for _ in 0..1 { defer { break; } } }", "E0905");
}

#[test]
fn defer_result_17_continue_is_e0905_inside_outer_loop() {
    assert_dfr_compile_error(
        "fn main() { for _ in 0..1 { defer { continue; } } }",
        "E0905",
    );
}

#[test]
fn defer_result_18_await_is_e0905() {
    assert_dfr_compile_error(
        "async fn value() -> i64 { return 1; }\nasync fn main() { defer { let n = await value(); println(n); } }",
        "E0905",
    );
}

#[test]
fn defer_result_19_nested_async_cleanup_call_is_e0905() {
    assert_dfr_compile_error(
        "async fn cleanup() {}\nfn main() { defer { cleanup(); } }",
        "E0905",
    );
}

#[test]
fn defer_result_20_async_normal_exit_and_cancel_run_once() {
    assert_dfr_runs(
        &format!(
            "{DEFER_RESULT_HELPER}\nasync fn normal() {{ defer {{ match cleanup(false) {{ Ok(_) => {{}}, Err(_) => println(\"normal cleanup\"), }} }} }}\nasync fn waiting() {{ defer {{ match cleanup(false) {{ Ok(_) => {{}}, Err(_) => println(\"cancel cleanup\"), }} }} await sleep(5000); }}\nasync fn main() {{ await normal(); let task = waiting(); await sleep(20); task.cancel(); await sleep(50); println(task.is_cancelled()); }}"
        ),
        "normal cleanup\ncancel cleanup\ntrue\n",
    );
}

#[test]
fn defer_result_21_async_cancel_body_reads_lexical_value() {
    assert_dfr_runs(
        "async fn waiting() { let label = \"cleanup\" + \" value\"; defer { println(label); } await sleep(5000); }\nasync fn main() { let task = waiting(); await sleep(20); task.cancel(); await sleep(50); println(task.is_cancelled()); }",
        "cleanup value\ntrue\n",
    );
}

#[test]
fn defer_result_22_direct_call_argument_try_is_e0905() {
    assert_dfr_compile_error(
        &format!(
            "{DEFER_RESULT_HELPER}\nfn value() -> Result<bool, String> {{ return Ok(false); }}\nfn work() -> Result<void, String> {{ defer cleanup(value()?); return Ok(); }}"
        ),
        "E0905",
    );
}

// ── std::fs v1 (willow-2s3 Stage 5 slice) ───────────────────────────────────
// Unsuffixed operations are synchronous compatibility APIs. Their `_async`
// counterparts execute in the scheduler blocking pool and return Tasks;
// all fallible ops return Result<_, IoError> with the
// failing path + OS message in IoError::Failed. 20 perspectives: 1 write+
// read roundtrip, 2 exists true/false, 3 read of missing file is Err with
// path in message, 4 write to unwritable dir is Err, 5 remove_file Ok +
// exists false, 6 remove of missing file is Err, 7 overwrite replaces
// contents, 8 empty file roundtrip, 9 multibyte UTF-8 contents, 10 newlines
// preserved, 11 `?` propagation of IoError, 12 `?` on the void write form,
// 13 fs in async fn, 14 GC stress roundtrip, 15 usable without import (builtin module, like env), 16 wrong arg count rejected, 17 wrong arg type
// rejected, 18 result must be matched (println of it rejected E1402),
// 19 large-ish contents (10k), 20 two files independent.

#[test]
fn fs_01_roundtrip() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t01\"); fs::write_string(p, \"abc\"); match fs::read_to_string(p) { Ok(t) => println(t), Err(e) => println(\"no\"), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "abc\n");
}

#[test]
fn fs_02_exists() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t02\"); println(fs::exists(p)); fs::write_string(p, \"x\"); println(fs::exists(p)); fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\ntrue\n");
}

#[test]
fn fs_03_missing_read_err_with_path() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t03_missing\"); match fs::read_to_string(p) { Ok(t) => println(t), Err(e) => { match e { Failed(m) => println(m), } } } }",
    );
    assert!(ok, "{out}");
    assert!(out.contains("willow_t03_missing"), "{out}");
}

#[test]
fn fs_04_unwritable_err() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t04_missing_dir\") + \"/t.txt\"; match fs::write_string(p, \"x\") { Ok(v) => println(\"ok\"), Err(e) => println(\"err\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "err\n");
}

#[test]
fn fs_05_remove_ok() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t05\"); fs::write_string(p, \"x\"); match fs::remove_file(p) { Ok(v) => println(\"gone\"), Err(e) => println(\"err\"), } println(fs::exists(p)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "gone\nfalse\n");
}

#[test]
fn fs_06_remove_missing_err() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t06_missing\"); match fs::remove_file(p) { Ok(v) => println(\"ok\"), Err(e) => println(\"err\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "err\n");
}

#[test]
fn fs_07_overwrite() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t07\"); fs::write_string(p, \"first\"); fs::write_string(p, \"second\"); match fs::read_to_string(p) { Ok(t) => println(t), Err(e) => println(\"no\"), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "second\n");
}

#[test]
fn fs_08_empty_file() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t08\"); fs::write_string(p, \"\"); match fs::read_to_string(p) { Ok(t) => println(t == \"\"), Err(e) => println(false), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn fs_09_multibyte() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t09\"); fs::write_string(p, \"日本語\"); match fs::read_to_string(p) { Ok(t) => println(t), Err(e) => println(\"no\"), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "日本語\n");
}

#[test]
fn fs_10_newlines_preserved() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t10\"); fs::write_string(p, \"a\\nb\"); match fs::read_to_string(p) { Ok(t) => println(t), Err(e) => println(\"no\"), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a\nb\n");
}

#[test]
fn fs_11_question_mark_read() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn load(p: String) -> Result<String, IoError> { let t = fs::read_to_string(p)?; return Ok(t + \"!\"); }\nfn main() { match load(fs::temp_path(\"willow_t11_missing\")) { Ok(t) => println(t), Err(e) => println(\"propagated\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "propagated\n");
}

#[test]
fn fs_12_question_mark_write() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn save(p: String) -> Result<void, IoError> { fs::write_string(p, \"x\")?; fs::remove_file(p)?; return Result::Ok(); }\nfn main() { match save(fs::temp_path(\"willow_t12\")) { Ok(v) => println(\"saved\"), Err(e) => println(\"err\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "saved\n");
}

#[test]
fn fs_13_in_async_fn() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nasync fn work() -> i64 { let p = fs::temp_path(\"willow_t13\"); fs::write_string(p, \"async\"); await sleep(1); match fs::read_to_string(p) { Ok(t) => println(t), Err(e) => println(\"no\"), } fs::remove_file(p); return 1; }\nasync fn main() { await work(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "async\n");
}

#[test]
fn fs_14_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t14\"); fs::write_string(p, \"g\" + \"c\"); match fs::read_to_string(p) { Ok(t) => println(t + \"!\"), Err(e) => println(\"no\"), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "gc!\n");
}

#[test]
fn fs_15_usable_without_import_like_env() {
    // Builtin schema modules (env, fs) are always visible; `import std::fs`
    // is stylistic — consistent with std::env.
    let (out, ok) = compile_and_run(
        "fn main() { let p = fs::temp_path(\"willow_t15_missing\"); println(fs::exists(p)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn fs_16_wrong_arg_count() {
    let (ok, stderr) =
        compile_with_compiler_env("import std::fs;\nfn main() { fs::read_to_string(); }", &[]);
    assert!(!ok);
    assert!(!stderr.is_empty());
}

#[test]
fn fs_17_wrong_arg_type() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::fs;\nfn main() { fs::read_to_string(42); }",
        &[],
    );
    assert!(!ok);
    assert!(!stderr.is_empty());
}

#[test]
fn fs_18_result_not_printable() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::fs;\nfn main() { println(fs::read_to_string(\"/tmp/x\")); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E1402"), "{stderr}");
}

#[test]
fn fs_19_large_contents() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t19\"); let mut s = \"\"; let mut i = 0; while i < 1000 { s = s + \"0123456789\"; i = i + 1; } fs::write_string(p, s); match fs::read_to_string(p) { Ok(t) => println(t == s), Err(e) => println(false), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn fs_20_two_files_independent() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let a = fs::temp_path(\"willow_t20a\"); let b = fs::temp_path(\"willow_t20b\"); fs::write_string(a, \"A\"); fs::write_string(b, \"B\"); match fs::read_to_string(a) { Ok(t) => println(t), Err(e) => println(\"no\"), } match fs::read_to_string(b) { Ok(t) => println(t), Err(e) => println(\"no\"), } fs::remove_file(a); fs::remove_file(b); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "A\nB\n");
}

// Review fixes on std::fs dispatch (willow-2s3): 21 a USER module imported
// as `fs` must win over the builtin (the emit arm used to fire on the bare
// string name before user-module resolution — the user's exists() was
// silently replaced by willow_fs_exists); 22 `import std::fs as files;`
// resolves the alias to the builtin (used to pass import validation then die
// E0350 at the call); 23 aliased env module works the same way; 24 aliases
// are normalized independently inside imported module bodies.

#[test]
fn fs_21_user_module_named_fs_wins() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import mine as fs;\nfn main() { println(fs::exists(\"/definitely/not/there\")); }\n",
            ),
            (
                "mine.wi",
                "module mine;\npub fn exists(p: String) -> bool { return true; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n", "user module must shadow the builtin fs");
}

#[test]
fn fs_22_std_fs_alias() {
    let (out, ok) = compile_and_run(
        "import std::fs as files;\nfn main() { let p = files::temp_path(\"willow_t22_missing\"); println(files::exists(p)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn fs_23_std_env_alias() {
    let (out, ok) = compile_and_run(
        "import std::env as environment;\nfn main() { println(environment::args_len()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn fs_24_std_fs_alias_inside_imported_module() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import helper;\nfn main() { println(helper::missing()); }\n",
            ),
            (
                "helper.wi",
                "module helper;\nimport std::fs as files;\npub fn missing() -> bool { let p = files::temp_path(\"willow_t24_missing\"); return files::exists(p); }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn fs_25_async_blocking_pool_roundtrip() {
    let (out, ok) = compile_and_run(
        r#"
import std::fs;

async fn main() {
    let path = fs::temp_path("willow_t25");
    match await fs::write_string_async(path, "pool") {
        Ok(v) => println("written"),
        Err(e) => println("write-error"),
    }
    match await fs::read_to_string_async(path) {
        Ok(text) => println(text),
        Err(e) => println("read-error"),
    }
    println(await fs::exists_async(path));
    match await fs::remove_file_async(path) {
        Ok(v) => println("removed"),
        Err(e) => println("remove-error"),
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "written\npool\ntrue\nremoved\n");
}

#[test]
fn fs_26_awaiting_sync_api_has_async_migration_diagnostic() {
    assert_compile_error_contains(
        "import std::fs;\nasync fn main() { let value = await fs::read_to_string(\"x\"); }",
        &[
            "error[E0803]",
            "synchronous filesystem operations cannot be awaited",
            "await fs::read_to_string_async(...)",
            "blocking pool",
        ],
    );
}

#[test]
fn fs_27_awaiting_sync_api_through_alias_preserves_alias_in_help() {
    assert_compile_error_contains(
        "import std::fs as files;\nasync fn main() { let value = await files::exists(\"x\"); }",
        &["error[E0803]", "await files::exists_async(...)"],
    );
}

#[test]
fn fs_28_sync_compatibility_and_async_api_coexist() {
    let (out, ok) = compile_and_run(
        r#"
import std::fs;

async fn main() {
    let path = fs::temp_path("willow_t28_missing");
    println(fs::exists(path));
    println(await fs::exists_async(path));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\nfalse\n");
}

// ── std::net v1 (willow-2s3.1) ───────────────────────────────────────
// Numeric-address bind is synchronous; connect/accept/read/write return Tasks
// whose poll functions park on the platform netpoll backend.

const NET_ECHO_SOURCE: &str = r#"
import std::net;

async fn exchange() -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    (await net::write_async(client, "ping"))?;
    let server = (await accepting)?;
    let text = (await net::read_async(server, 1024))?;
    net::shutdown(client)?;
    net::shutdown(server)?;
    return Ok(text);
}

async fn main() {
    match await exchange() {
        Ok(text) => println(text),
        Err(error) => println("network error"),
    }
}
"#;

#[test]
fn net_01_loopback_echo_uses_async_socket_operations() {
    // Exercise the runtime's minimum five-worker pool. Non-blocking behavior
    // is pinned separately by readiness parking/wake and cancellation tests;
    // WILLOW_WORKERS values below five are intentionally clamped to five.
    let (out, ok) = compile_and_run_with_env(NET_ECHO_SOURCE, &[("WILLOW_WORKERS", "5")]);
    assert!(ok, "{out}");
    assert_eq!(out, "ping\n");
}

#[test]
fn net_02_loopback_echo_survives_gc_stress() {
    let (out, ok) = compile_and_run_with_env(
        NET_ECHO_SOURCE,
        &[
            ("WILLOW_WORKERS", "5"),
            ("WILLOW_GC_STRESS", "alloc"),
            ("WILLOW_TASK_BUDGET", "1"),
        ],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "ping\n");
}

#[test]
fn net_03_module_alias_dispatches_to_builtin() {
    let source = NET_ECHO_SOURCE
        .replace("import std::net;", "import std::net as network;")
        .replace("net::", "network::");
    let (out, ok) = compile_and_run(&source);
    assert!(ok, "{out}");
    assert_eq!(out, "ping\n");
}

#[test]
fn net_04_invalid_address_is_typed_error() {
    let (out, ok) = compile_and_run(
        "import std::net;\nasync fn main() { match await net::connect_async(\"localhost:80\") { Ok(stream) => println(\"connected\"), Err(error) => println(\"invalid\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "invalid\n");
}

#[test]
fn net_05_cancelled_accept_deregisters_and_listener_can_be_reused() {
    let (out, ok) = compile_and_run_with_env(
        r#"
import std::net;

async fn run() -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let cancelled = net::accept_async(listener);
    await sleep(5);
    cancelled.cancel();
    match await cancelled.result() {
        Ok(stream) => println("unexpected"),
        Err(Cancelled) => println("cancelled"),
    }

    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    (await net::write_async(client, "after-cancel"))?;
    let server = (await accepting)?;
    return await net::read_async(server, 1024);
}

async fn main() {
    match await run() {
        Ok(text) => println(text),
        Err(error) => println("network error"),
    }
}
"#,
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cancelled\nafter-cancel\n");
}

#[test]
fn net_06_timer_wakes_while_accept_is_parked() {
    let (out, ok) = compile_and_run(
        r#"
import std::net;

async fn run() -> Result<void, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let accepting = net::accept_async(listener);
    await sleep(5);
    println("timer");
    accepting.cancel();
    return Ok();
}

async fn main() {
    match await run() {
        Ok(value) => println("done"),
        Err(error) => println("network error"),
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "timer\ndone\n");
}

#[test]
fn net_07_read_bound_validation_is_result_error() {
    let source = NET_ECHO_SOURCE.replace(
        "net::read_async(server, 1024)",
        "net::read_async(server, 0)",
    );
    let (out, ok) = compile_and_run(&source);
    assert!(ok, "{out}");
    assert_eq!(out, "network error\n");
}

#[test]
fn net_08_empty_write_completes_successfully() {
    let (out, ok) = compile_and_run(
        r#"
import std::net;

async fn run() -> Result<String, IoError> {
    let listener = net::bind("127.0.0.1:0")?;
    let address = net::local_addr(listener)?;
    let accepting = net::accept_async(listener);
    let client = (await net::connect_async(address))?;
    (await net::write_async(client, ""))?;
    net::shutdown(client)?;
    let server = (await accepting)?;
    return await net::read_async(server, 1);
}

async fn main() {
    match await run() {
        Ok(text) => println(text),
        Err(error) => println("network error"),
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "\n");
}

#[test]
fn net_09_imported_handle_types_accept_annotations() {
    let (out, ok) = compile_and_run(
        r#"
import std::net;
import std::net::TcpListener;

fn check_listener() -> Result<bool, IoError> {
    let listener: TcpListener = net::bind("127.0.0.1:0")?;
    let value = net::local_addr(listener)?;
    return Ok(value != "");
}

fn main() {
    match check_listener() {
        Ok(value) => println(value),
        Err(error) => println(false),
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn net_10_wrong_handle_type_is_rejected() {
    assert_compile_error_contains(
        "import std::net;\nasync fn main() { let task = net::accept_async(1); }",
        &["error[E0201]", "expected `TcpListener`, found `i64`"],
    );
}

#[test]
fn net_11_utf8_payload_roundtrips() {
    let source = NET_ECHO_SOURCE.replace("\"ping\"", "\"日本語🌱\"");
    let (out, ok) = compile_and_run_with_env(&source, &[("WILLOW_WORKERS", "5")]);
    assert!(ok, "{out}");
    assert_eq!(out, "日本語🌱\n");
}

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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_gc_stress_differential(
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
    assert_lir_gc_stress_differential(
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
    assert_lir_gc_stress_differential(
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
    assert_lir_gc_stress_differential(
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
    let source = include_str!("../../example/lir_enum_match.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
    assert!(
        ok,
        "example/lir_enum_match.wi must compile with every free function on the LIR path: {stderr}"
    );
}

#[test]
fn lirreq_54_generic_enums_are_claimed() {
    // The boundary moved in willow-0g8j.2.1: `Option` and `Result` are ordinary
    // prelude enums, so a function that matches one is compiled by the walker
    // rather than handed to the AST emitter. Both representations appear here —
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
        let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
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
    // so both the caller and the lifted body are on the LIR path and this mode
    // must accept the program rather than report a fallback.
    let source = "fn f(x: Option<i64>) -> i64 { return x.map(|v: i64| v * 2).unwrap_or(-1); }\n\
                  fn main() { println(f(Some(2))); }";
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
    assert!(
        ok,
        "a closure combinator over a supported lambda must stay on the LIR path: {stderr}"
    );
}

#[test]
fn lirreq_55b_an_unsupported_lambda_body_still_falls_back() {
    // The boundary moved but did not disappear. A lambda is a FUNCTION, so it
    // faces eligibility on its own terms; a static property read is outside the
    // walker's subset (willow-0g8j.2.6), and the lifted body is what the mode
    // must report even though the function that takes it is perfectly eligible.
    // (Scalar `toString` served as the unsupported body here until it joined
    // the subset in willow-0g8j.2.5.)
    let source = "class Config { pub static version: i64 = 7; }\n\
                  fn f(x: Option<i64>) -> i64 { return x.map(|v: i64| v + Config::version).unwrap_or(-1); }\n\
                  fn main() { println(f(Some(2))); }";
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
    assert!(
        !ok,
        "a lambda whose body leaves the subset must fall back, not be claimed by the walker"
    );
    assert!(
        stderr.contains("fell back to the AST backend"),
        "the refusal must be the fallback diagnostic: {stderr}"
    );
}

#[test]
fn lirreq_56_option_result_example_is_fully_lir() {
    // Same contract as the other examples: the header claims every free
    // function in the example is compiled from the lowered IR, and this mode is
    // what keeps that claim honest. Its class method compiles through
    // `compile_class_method_inner`, which the mode does not police.
    let source = include_str!("../../example/lir_option_result.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
    assert!(
        ok,
        "example/lir_option_result.wi must compile with every free function on the LIR path: {stderr}"
    );
}

#[test]
fn lirreq_58_divergence_example_is_fully_lir() {
    // The divergence example (willow-0g8j.2.5) exists to be compiled in this
    // mode: statement panics, formatted panics, `return`-diverging match arms
    // and an all-arms-returning match must every one of them stay on the LIR
    // path, or the mode turns the fallback into a compile error.
    let source = include_str!("../../example/lir_divergence.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
    assert!(
        ok,
        "example/lir_divergence.wi must compile with every free function on the LIR path: {stderr}"
    );
}

// ── divergence runtime differentials (willow-0g8j.2.5) ───────────────────────
//
// The `d*` unit tests in `src/backend/cranelift/lir_gen.rs` pin the
// eligibility boundary. These pin the BEHAVIOUR: a `panic` is an unwind, not a
// value, so what has to match across the two backends is the message on
// stderr, the frames under it, and the fact that the process does not exit
// cleanly.

/// Replace the compiled program's source path with a placeholder.
///
/// A panic message and every stack frame under it carry the path of the file
/// they were compiled from, and each backend gets its own temp file, so the two
/// runs can only be compared once that path is out of the way. Line and column
/// survive — they are the part a backend can get wrong.
fn without_source_paths(out: &str) -> String {
    out.lines()
        .map(|line| {
            let mut rest = line;
            let mut normalized = String::new();
            while let Some(end) = rest.find(".wi:") {
                let start = rest[..end].rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
                normalized.push_str(&rest[..start]);
                normalized.push_str("<src>.wi:");
                rest = &rest[end + ".wi:".len()..];
            }
            normalized.push_str(rest);
            normalized
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn lir_div_01_statement_panic_matches_the_ast_backend() {
    // The base case: `panic(...)` as a whole statement. Both backends must
    // print the same message and neither may exit successfully.
    let source = r#"
fn check(n: i64) -> i64 {
    if n < 0 { panic("negative input"); }
    return n;
}
fn main() { println(check(1)); println(check(-1)); }
"#;
    let (with_lir, ok_on) = compile_with_env_and_run_combined(source, &LIR_ON);
    let (without_lir, ok_off) = compile_with_env_and_run_combined(source, &LIR_OFF);
    assert!(!ok_on && !ok_off, "both paths must panic");
    assert!(
        with_lir.contains("negative input"),
        "LIR output lost the message: {with_lir}"
    );
    assert_eq!(
        without_source_paths(&with_lir),
        without_source_paths(&without_lir),
        "LIR and AST paths must agree"
    );
    assert!(
        with_lir.starts_with("1\n"),
        "the statements before the panic must still run: {with_lir}"
    );
}

#[test]
fn lir_div_02_formatted_panic_message_matches() {
    // The interpolated form goes through the same operand rendering as
    // `format`, so a crossed operand would show up as a wrong message rather
    // than as a crash.
    let source = r#"
fn div(a: i64, b: i64) -> i64 {
    if b == 0 { panic("cannot divide {} by {}", a, b); }
    return a / b;
}
fn main() { println(div(10, 2)); println(div(7, 0)); }
"#;
    let (with_lir, ok_on) = compile_with_env_and_run_combined(source, &LIR_ON);
    let (without_lir, ok_off) = compile_with_env_and_run_combined(source, &LIR_OFF);
    assert!(!ok_on && !ok_off, "both paths must panic");
    assert!(
        with_lir.contains("cannot divide 7 by 0"),
        "LIR output has the wrong message: {with_lir}"
    );
    assert_eq!(
        without_source_paths(&with_lir),
        without_source_paths(&without_lir),
        "LIR and AST paths must agree"
    );
}

#[test]
fn lir_div_03_panicking_match_arm_matches() {
    // A panic in ARM position ends the arm's block instead of jumping to the
    // merge. The arms that do produce values must be unaffected.
    let source = r#"
fn level(n: i64) -> String {
    return match n {
        1 => "low",
        2 => "high",
        _ => panic("no level {}", n),
    };
}
fn main() { println(level(1)); println(level(2)); println(level(9)); }
"#;
    let (with_lir, ok_on) = compile_with_env_and_run_combined(source, &LIR_ON);
    let (without_lir, ok_off) = compile_with_env_and_run_combined(source, &LIR_OFF);
    assert!(!ok_on && !ok_off, "both paths must panic");
    assert!(
        with_lir.starts_with("low\nhigh\n") && with_lir.contains("no level 9"),
        "LIR output is wrong: {with_lir}"
    );
    assert_eq!(
        without_source_paths(&with_lir),
        without_source_paths(&without_lir),
        "LIR and AST paths must agree"
    );
}

#[test]
fn lir_div_04_panic_frames_agree_with_the_ast_backend() {
    // Divergence must not cost the call stack: the frame for the panicking
    // function and the frame for its caller have to appear, in that order, on
    // both backends.
    let source = r#"
fn inner(n: i64) -> i64 { panic("boom {}", n); }
fn outer(n: i64) -> i64 { return inner(n); }
fn main() { println(outer(3)); }
"#;
    let (with_lir, ok_on) = compile_with_env_and_run_combined(source, &LIR_ON);
    let (without_lir, ok_off) = compile_with_env_and_run_combined(source, &LIR_OFF);
    assert!(!ok_on && !ok_off, "both paths must panic");
    for (backend, out) in [("LIR", &with_lir), ("AST", &without_lir)] {
        let callee = out
            .find("0: inner")
            .unwrap_or_else(|| panic!("{backend} trace has no callee frame: {out}"));
        let caller = out
            .find("1: outer")
            .unwrap_or_else(|| panic!("{backend} trace has no caller frame: {out}"));
        assert!(callee < caller, "{backend} trace is out of order: {out}");
    }
}

#[test]
fn lir_div_05_returning_match_arms_match() {
    // Every arm leaves, so the match is typed `!` and its merge block is
    // unreachable. Reading the result variable there would be undefined; the
    // outputs must still agree exactly.
    assert_lir_differential(
        r#"
fn classify(n: i64) -> String {
    match n {
        0 => return "zero",
        1 => return "one",
        _ => return "many",
    }
}
fn main() { println(classify(0)); println(classify(1)); println(classify(7)); }
"#,
        "zero\none\nmany\n",
    );
}

#[test]
fn lir_div_06_diverging_and_value_arms_in_one_match() {
    // A `return` arm beside a value arm: the value arm still has to reach the
    // merge and flow out of the match.
    assert_lir_differential(
        r#"
fn describe(n: i64) -> String {
    return match n {
        0 => "nothing",
        _ => { println("saw " + n.toString()); return "something"; }
    };
}
fn main() { println(describe(0)); println(describe(4)); }
"#,
        "nothing\nsaw 4\nsomething\n",
    );
}

#[test]
fn lir_div_07_nested_diverging_arms() {
    // Divergence nests: the outer arm's tail is itself an all-returning match,
    // so one block must acquire exactly one terminator.
    assert_lir_differential(
        r#"
fn grid(row: i64, col: i64) -> String {
    match row {
        0 => match col {
            0 => return "origin",
            _ => return "top",
        },
        _ => return "body",
    }
}
fn main() { println(grid(0, 0)); println(grid(0, 3)); println(grid(2, 0)); }
"#,
        "origin\ntop\nbody\n",
    );
}

#[test]
fn lir_div_08_scalar_to_string_and_format_match() {
    // The string machinery the panics build their messages from, exercised on
    // its own so a rendering difference is not mistaken for a divergence bug.
    assert_lir_differential(
        r#"
fn main() {
    println((42).toString());
    println((2.5).toString());
    println(true.toString());
    println("willow".toString());
    println(format("{} items", 3));
    println(format("{} and {}", "left", "right"));
    println(format("{:.6f}", 3.14159265));
    println(format("{{literal}} {}", 9));
}
"#,
        "42\n2.5\ntrue\nwillow\n3 items\nleft and right\n3.141593\n{literal} 9\n",
    );
}

#[test]
fn lir_div_09_operand_position_panic_still_falls_back() {
    // The negative control for the position rule. In an operand position the
    // panic's terminator would strand the call that consumes its value, so the
    // walker must decline the function — and this mode reports that as an
    // error instead of silently producing wrong code.
    let source = "fn f() -> i64 { println(panic(\"no\")); return 1; }\n\
                  fn main() { println(f()); }";
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
    assert!(
        !ok,
        "an operand-position panic must not be claimed by the walker"
    );
    assert!(
        stderr.contains("has type `!`"),
        "the fallback reason must name the diverging type: {stderr}"
    );
}

#[test]
fn lir_div_10_a_panic_that_is_not_taken_exits_cleanly() {
    // The guard shape in real code: the panic is compiled but never reached,
    // so the program must exit normally and print nothing extra.
    assert_lir_differential(
        r#"
fn require_positive(n: i64) -> i64 {
    if n <= 0 { panic("expected a positive value, got " + n.toString()); }
    return n;
}
fn main() { println(require_positive(5)); println(require_positive(1)); }
"#,
        "5\n1\n",
    );
}

#[test]
fn lirreq_57_function_values_example_is_fully_lir() {
    // Same contract again for the function-value example: named functions used
    // as values, lifted lambdas, indirect calls and the callable-taking
    // combinators all have to survive the mode that turns any fallback to the
    // AST emitter into a compile error.
    let source = include_str!("../../example/lir_function_values.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &LIR_ON);
    assert!(
        ok,
        "example/lir_function_values.wi must compile with every free function on the LIR path: {stderr}"
    );
}

// ── Option/Result runtime differentials (willow-0g8j.2.1) ────────────────────
//
// The eligibility boundary is pinned by the `p*` unit tests in
// `src/backend/cranelift/lir_gen.rs`. These pin the OUTPUT: for each shape the
// walker now claims, the program it produces must print exactly what the AST
// emitter's does. The representation split is what makes that non-trivial —
// `Option<String>` is a pointer niche and `Option<i64>` is a boxed
// `[tag | payload]`, and only `option_repr` knows which.

#[test]
fn lir_diff_46_boxed_option_roundtrip() {
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_differential(
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
    assert_lir_gc_stress_differential(
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
    assert_lir_gc_stress_differential(
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

// ── Function values, lambdas and indirect calls (willow-0g8j.2.2) ────────────
//
// The eligibility boundary is pinned by the `f*` unit tests in
// `src/backend/cranelift/lir_gen.rs`. These pin the OUTPUT. What makes the
// shape non-trivial is that a lambda is a SEPARATE function compiled under a
// symbol the walker never invented — the backend's span-keyed table names it —
// and that a call through a value has no statically known target, so the
// panic-depth protocol stays conservative.

#[test]
fn lir_diff_58_named_function_values() {
    assert_lir_differential(
        r#"
fn double(x: i64) -> i64 { return x * 2; }
fn square(x: i64) -> i64 { return x * x; }
fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
fn pick(want_square: bool) -> fn(i64) -> i64 {
    if want_square { return square; }
    return double;
}
fn main() {
    println(apply(double, 21));
    println(apply(square, 7));
    let g: fn(i64) -> i64 = pick(true);
    println(g(9));
    println(apply(pick(false), 5));
}
"#,
        "42\n49\n81\n10\n",
    );
}

#[test]
fn lir_diff_59_lambda_values_including_nested() {
    // The inner lambda is lifted too, and its body is not part of the outer
    // one's block graph — lowering it inline would put the blocks in the wrong
    // function.
    assert_lir_differential(
        r#"
fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
fn main() {
    println(apply(|x: i64| x + 1, 41));
    let times: fn(i64) -> i64 = |x: i64| -> i64 { return x * 10; };
    println(times(4));
    println(apply(|x: i64| apply(|y: i64| y * 3, x + 1), 2));
}
"#,
        "42\n40\n9\n",
    );
}

#[test]
fn lir_diff_60_shadowing_and_void_returning_function_values() {
    // `weigh` the local value shadows `weigh` the top-level function, so the
    // callee resolution order decides which one runs. The `void` cases are the
    // other signature an indirect call has: no result to merge.
    assert_lir_differential(
        r#"
fn weigh(n: i64) -> i64 { return n * 3; }
fn shout(s: String) { println("say " + s); }
fn run(f: fn(String) -> void, s: String) { f(s); }
fn main() {
    let weigh: fn(i64) -> i64 = |n: i64| n + 100;
    println(weigh(2));
    run(shout, "hi");
    let quiet: fn(String) -> void = |s: String| println("[" + s + "]");
    run(quiet, "there");
}
"#,
        "102\nsay hi\n[there]\n",
    );
}

#[test]
fn lir_diff_61_array_of_function_values() {
    // The callee comes out of an array element, so the value reaching the call
    // is loaded rather than named — and a lambda sits in the same array as two
    // named functions.
    assert_lir_differential(
        r#"
import std::collections::Array;
fn double(x: i64) -> i64 { return x * 2; }
fn negate(x: i64) -> i64 { return 0 - x; }
fn main() {
    let fs: Array<fn(i64) -> i64> = [double, negate, |x: i64| x + 7];
    let mut i = 0;
    while i < fs.len() {
        let g = fs[i];
        println(g(10));
        i = i + 1;
    }
}
"#,
        "20\n-10\n17\n",
    );
}

#[test]
fn lir_diff_62_gc_managed_values_cross_an_indirect_call() {
    // Each call allocates a new string, and the argument to the second call is
    // the first call's result — so a missing root would free a live string.
    assert_lir_differential(
        r#"
fn shout(s: String) -> String { return s + "!"; }
fn twice(f: fn(String) -> String, s: String) -> String { return f(f(s)); }
fn main() {
    println(twice(shout, "hi"));
    println(twice(|s: String| "[" + s + "]", "core"));
}
"#,
        "hi!!\n[[core]]\n",
    );
}

#[test]
fn lir_diff_63_option_combinators() {
    // `map`/`and_then`/`or_else` are the methods that CALL their operand, with
    // both spellings of a function value, and across both option
    // representations: `Option<i64>` is boxed, `Option<String>` is the niche.
    assert_lir_differential(
        r#"
fn label(v: i64) -> String {
    if v > 3 { return "big"; }
    return "small";
}
fn main() {
    let some: Option<i64> = Some(4);
    let none: Option<i64> = None;
    println(some.map(|v: i64| v * 10).unwrap_or(-1));
    println(none.map(|v: i64| v * 10).unwrap_or(-1));
    println(some.map(label).unwrap_or("?"));
    println(none.map(label).unwrap_or("?"));
    println(some.and_then(|v: i64| Option::Some(v + 1)).unwrap_or(-1));
    println(none.and_then(|v: i64| Option::Some(v + 1)).unwrap_or(-1));
    println(some.or_else(|| Option::Some(99)).unwrap_or(-1));
    println(none.or_else(|| Option::Some(99)).unwrap_or(-1));
}
"#,
        "40\n-1\nbig\n?\n5\n-1\n4\n99\n",
    );
}

#[test]
fn lir_diff_64_result_combinators() {
    // The `Result` side, including `map_err` — the only combinator that
    // rebuilds the error slot — and the two merges that pass the receiver
    // through one arm and the callable's own box through the other.
    assert_lir_differential(
        r#"
fn parse_even(v: i64) -> Result<i64, String> {
    if v % 2 == 0 { return Ok(v / 2); }
    return Err("odd");
}
fn label(v: i64) -> String {
    if v > 2 { return "big"; }
    return "small";
}
fn main() {
    let ok: Result<i64, String> = parse_even(8);
    let bad: Result<i64, String> = parse_even(7);
    println(ok.map(|v: i64| v * 10).unwrap_or(-1));
    println(bad.map(|v: i64| v * 10).unwrap_or(-1));
    println(ok.map(label).unwrap_or("?"));
    println(bad.map(label).unwrap_or("?"));
    println(ok.map_err(|e: String| "e:" + e).unwrap_or(-1));
    println(bad.map_err(|e: String| "e:" + e).unwrap_err());
    println(ok.and_then(|v: i64| parse_even(v)).unwrap_or(-1));
    println(bad.and_then(|v: i64| parse_even(v)).unwrap_err());
    println(ok.or_else(|e: String| Result::Ok(0)).unwrap());
    println(bad.or_else(|e: String| Result::Ok(0)).unwrap());
}
"#,
        "40\n-1\nbig\n?\n4\ne:odd\n2\nodd\n4\n0\n",
    );
}

#[test]
fn lir_diff_65_recursion_through_a_function_value() {
    // The recursion's step comes from a parameter, so the call graph is not
    // statically known — the panic-depth protocol has to stay conservative and
    // the frame push/pop still has to balance.
    assert_lir_differential(
        r#"
fn step(n: i64) -> i64 { return n - 1; }
fn walk(f: fn(i64) -> i64, n: i64) -> i64 {
    if n <= 0 { return 0; }
    return 1 + walk(f, f(n));
}
fn compose(f: fn(i64) -> i64, g: fn(i64) -> i64, v: i64) -> i64 {
    return f(g(v));
}
fn main() {
    println(walk(step, 5));
    println(walk(|n: i64| n - 2, 9));
    println(compose(step, |n: i64| n * 2, 10));
}
"#,
        "5\n5\n19\n",
    );
}

#[test]
fn lir_diff_66_function_value_into_a_class_method() {
    // The receiver's field is written through the callable's result, so the
    // method's own `self` has to survive the indirect call.
    assert_lir_differential(
        r#"
class Counter {
    pub total: i64;
    pub fn bump(self, by: fn(i64) -> i64) -> i64 {
        self.total = by(self.total);
        return self.total;
    }
}
fn triple(x: i64) -> i64 { return x * 3; }
fn main() {
    let c = new Counter(2);
    println(c.bump(triple));
    println(c.bump(|x: i64| x + 4));
    println(c.total);
}
"#,
        "6\n10\n10\n",
    );
}

#[test]
fn lir_diff_67_indirect_calls_and_combinators_under_gc_stress() {
    // Every iteration allocates: the argument string, the callee's result, and
    // the `Option` the combinator rebuilds. Collecting at every allocation is
    // what turns a missing root into a wrong answer rather than a lucky one.
    assert_lir_gc_stress_differential(
        r#"
fn wrap(s: String) -> String { return "<" + s + ">"; }
fn apply(f: fn(String) -> String, s: String) -> String { return f(s); }
fn main() {
    let mut i = 0;
    let mut last = "";
    while i < 40 {
        last = apply(wrap, "x" + "y");
        i = i + 1;
    }
    println(last);
    let mut j = 0;
    let mut seen = 0;
    while j < 40 {
        let o: Option<String> = Some("s" + "t");
        let got: String = o.map(|s: String| s + "!").unwrap_or("");
        if got != "" { seen = seen + 1; }
        j = j + 1;
    }
    println(seen);
}
"#,
        "<xy>\n40\n",
    );
}
