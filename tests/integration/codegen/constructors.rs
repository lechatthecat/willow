use super::*;

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
