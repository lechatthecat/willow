use super::super::support::*;

// ── prot (protected) access modifier tests ────────────────────────────────

// 1. prot field accessible within own class method
#[test]
fn test_prot_field_accessible_in_own_class() {
    let (out, ok) = compile_and_run(
        r#"
class Bag {
    pub init(self, items: i64) {
        self.items = items;
    }
    prot items: i64;
    pub fn count(self) -> i64 { return self.items; }
}
fn main() {
    let b = new Bag(7);
    println(b.count());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// 2. prot method callable within own class
#[test]
fn test_prot_method_callable_in_own_class() {
    let (out, ok) = compile_and_run(
        r#"
class Calc {
    pub init(self, val: i64) {
        self.val = val;
    }
    val: i64;
    prot fn triple(self) -> i64 { return self.val * 3; }
    pub fn result(self) -> i64 { return self.triple(); }
}
fn main() {
    let c = new Calc(4);
    println(c.result());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

// 3. prot field accessible in direct subclass method
#[test]
fn test_prot_field_accessible_in_subclass() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, score: i64) {
        self.score = score;
    }
    prot score: i64;
    pub fn score(self) -> i64 { return self.score; }
}
pub class Child extends Base {
    pub init(self, score: i64) {
        super.init(score);
    }
    pub fn bonus(self) -> i64 { return self.score + 10; }
}
fn main() {
    let c = new Child(5);
    println(c.score());
    println(c.bonus());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n15\n");
}

// 4. prot method callable in direct subclass
#[test]
fn test_prot_method_callable_in_subclass() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Engine {
    pub init(self, power: i64) {
        self.power = power;
    }
    power: i64;
    prot fn raw_power(self) -> i64 { return self.power; }
    pub fn get_power(self) -> i64 { return self.power; }
}
pub class Turbo extends Engine {
    pub init(self, power: i64) {
        super.init(power);
    }
    pub fn boosted(self) -> i64 { return self.raw_power() * 2; }
}
fn main() {
    let t = new Turbo(50);
    println(t.get_power());
    println(t.boosted());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "50\n100\n");
}

// 5. prot field accessible two levels down the hierarchy
#[test]
fn test_prot_field_accessible_two_levels_deep() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    pub init(self, n: i64) {
        self.n = n;
    }
    prot n: i64;
    pub fn n(self) -> i64 { return self.n; }
}
pub open class B extends A {
    pub init(self, n: i64) {
        super.init(n);
    }
    pub fn double(self) -> i64 { return self.n * 2; }
}
pub class C extends B {
    pub init(self, n: i64) {
        super.init(n);
    }
    pub fn triple(self) -> i64 { return self.n * 3; }
}
fn main() {
    let c = new C(4);
    println(c.n());
    println(c.double());
    println(c.triple());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n8\n12\n");
}

// 6. prot field rejected from unrelated external function
#[test]
fn test_prot_field_rejected_from_external_function() {
    assert_compile_error_contains(
        r#"
pub open class Vault {
    prot secret: i64;
}
fn steal(v: Vault) -> i64 { return v.secret; }
fn main() { println(1); }
"#,
        &[
            "error[E0503]",
            "field `secret` of class `Vault` is protected",
        ],
    );
}

// 7. prot method rejected from unrelated external function
#[test]
fn test_prot_method_rejected_from_external_function() {
    assert_compile_error_contains(
        r#"
pub open class Vault {
    x: i64;
    prot fn secret(self) -> i64 { return self.x; }
}
fn steal(v: Vault) -> i64 { return v.secret(); }
fn main() { println(1); }
"#,
        &[
            "error[E0503]",
            "method `secret` of class `Vault` is protected",
        ],
    );
}

// 8. prot field rejected via direct access from main
#[test]
fn test_prot_field_rejected_from_main() {
    assert!(expect_compile_error(
        r#"
class Box {
    prot val: i64;
}
fn main() {
    let b = new Box(1);
    println(b.val);
}
"#
    ));
}

// 9. pub still allows access from anywhere
#[test]
fn test_pub_overrides_prot_restriction() {
    let (out, ok) = compile_and_run(
        r#"
class Mix {
    pub init(self, x: i64, y: i64) {
        self.x = x;
        self.y = y;
    }
    pub x: i64;
    prot y: i64;
}
fn read_x(m: Mix) -> i64 { return m.x; }
fn main() {
    let m = new Mix(10, 20);
    println(read_x(m));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// 10. private is stricter than prot (subclass cannot see private)
#[test]
fn test_private_stricter_than_prot() {
    assert_compile_error_contains(
        r#"
pub open class Base {
    secret: i64;
}
pub class Child extends Base {
    pub fn leak(self) -> i64 { return self.secret; }
}
fn main() { println(1); }
"#,
        &["error[E0501]", "field `secret` of class `Base` is private"],
    );
}

// 11. prot method diagnostic points to declaration site
#[test]
fn test_prot_method_diagnostic_points_to_declaration() {
    assert_compile_error_contains(
        r#"
pub open class Service {
    x: i64;
    prot fn internal(self) -> i64 { return self.x; }
}
fn call(s: Service) -> i64 { return s.internal(); }
fn main() { println(1); }
"#,
        &[
            "error[E0503]",
            "method `internal` of class `Service` is protected",
            "method defined here",
        ],
    );
}

// 12. prot field diagnostic includes help text
#[test]
fn test_prot_field_diagnostic_help_text() {
    assert_compile_error_contains(
        r#"
pub open class Secure {
    prot token: i64;
}
fn grab(s: Secure) -> i64 { return s.token; }
fn main() { println(1); }
"#,
        &[
            "error[E0503]",
            "field `token` of class `Secure` is protected",
            "prot members are accessible only within",
        ],
    );
}

// 13. override method can access prot field from base
#[test]
fn test_prot_override_method_accesses_base_field() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub init(self, energy: i64) {
        self.energy = energy;
    }
    prot energy: i64;
    pub open fn cost(self) -> i64 { return self.energy; }
}
pub class Dog extends Animal {
    pub init(self, energy: i64) {
        super.init(energy);
    }
    pub override fn cost(self) -> i64 { return self.energy * 2; }
}
fn main() {
    let d = new Dog(5);
    println(d.cost());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// 14. prot + override: override of a prot method callable directly on the subclass type
#[test]
fn test_prot_method_override_in_subclass() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Shape {
    pub init(self, sides: i64) {
        self.sides = sides;
    }
    prot sides: i64;
    prot open fn side_count(self) -> i64 { return self.sides; }
    pub fn info(self) -> i64 { return self.side_count(); }
}
pub class Triangle extends Shape {
    pub init(self, sides: i64) {
        super.init(sides);
    }
    pub override fn side_count(self) -> i64 { return self.sides * 3; }
}
fn main() {
    let s = new Shape(4);
    let t = new Triangle(2);
    println(s.info());
    println(t.side_count());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n6\n");
}

// 15. prot field not visible as pub field from outside
#[test]
fn test_prot_field_not_publicly_visible() {
    assert!(expect_compile_error(
        r#"
class Hidden {
    prot value: i64;
}
fn main() {
    let h = new Hidden(1);
    println(h.value);
}
"#
    ));
}

// 16. error code is E0503, not E0501 or E0502
#[test]
fn test_prot_uses_error_code_e0503() {
    assert_compile_error_contains(
        r#"
class C { prot x: i64; }
fn main() {
    let c = new C(1);
    println(c.x);
}
"#,
        &["error[E0503]"],
    );
}

// 17. prot keyword parses on fields without other modifiers
#[test]
fn test_prot_parses_on_field() {
    let (out, ok) = compile_and_run(
        r#"
class Wrapper {
    pub init(self, inner: i64) {
        self.inner = inner;
    }
    prot inner: i64;
    pub fn get(self) -> i64 { return self.inner; }
}
fn main() {
    let w = new Wrapper(42);
    println(w.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 18. prot keyword parses on methods without other modifiers
#[test]
fn test_prot_parses_on_method() {
    let (out, ok) = compile_and_run(
        r#"
class Worker {
    pub init(self, load: i64) {
        self.load = load;
    }
    load: i64;
    prot fn internal_load(self) -> i64 { return self.load; }
    pub fn public_load(self) -> i64 { return self.internal_load(); }
}
fn main() {
    let w = new Worker(9);
    println(w.public_load());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n");
}

// 19. prot + open: protected open method overrideable in subclass, called on concrete type
#[test]
fn test_prot_open_method_can_be_overridden() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Vehicle {
    pub init(self, speed: i64) {
        self.speed = speed;
    }
    prot speed: i64;
    pub fn get_speed(self) -> i64 { return self.speed; }
    prot open fn describe(self) -> i64 { return self.speed; }
    pub fn show(self) -> i64 { return self.describe(); }
}
pub class Car extends Vehicle {
    pub init(self, speed: i64) {
        super.init(speed);
    }
    pub override fn describe(self) -> i64 { return self.speed + 10; }
}
fn main() {
    let v = new Vehicle(30);
    let c = new Car(50);
    println(v.show());
    println(c.describe());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "30\n60\n");
}

// 20. prot field accessible within class and subclass, rejected elsewhere
#[test]
fn test_prot_complete_access_rules() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Counter {
    pub init(self, count: i64) {
        self.count = count;
    }
    prot count: i64;
    pub fn get(self) -> i64 { return self.count; }
    prot fn increment(self) -> i64 { return self.count + 1; }
}
pub class BoundedCounter extends Counter {
    pub init(self, count: i64) {
        super.init(count);
    }
    pub fn safe_inc(self, max: i64) -> i64 {
        let next = self.increment();
        if next > max {
            return self.count;
        }
        return next;
    }
}
fn main() {
    let c = new BoundedCounter(8);
    println(c.get());
    println(c.safe_inc(10));
    println(c.safe_inc(7));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "8\n9\n8\n");
}
