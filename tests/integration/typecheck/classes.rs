use super::super::support::*;

// ── Class / Object tests ───────────────────────────────────────────────────

// 1. Single field, single method
#[test]
fn test_class_single_field_and_getter() {
    let (out, ok) = compile_and_run(
        r#"
class Num {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let x = new Num(7);
    println(x.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// 2. Multiple fields accessed via methods
#[test]
fn test_class_multiple_fields() {
    let (out, ok) = compile_and_run(
        r#"
class Rect {
    pub init(self, w: i64, h: i64) {
        self.w = w;
        self.h = h;
    }
    w: i64;
    h: i64;
    pub fn width(self) -> i64  { return self.w; }
    pub fn height(self) -> i64 { return self.h; }
    pub fn area(self) -> i64   { return self.w * self.h; }
}
fn main() {
    let r = new Rect(6, 4);
    println(r.width());
    println(r.height());
    println(r.area());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n4\n24\n");
}

// 3. Method that takes extra argument
#[test]
fn test_class_method_with_extra_arg() {
    let (out, ok) = compile_and_run(
        r#"
class Adder {
    pub init(self, base: i64) {
        self.base = base;
    }
    base: i64;
    pub fn add(self, n: i64) -> i64 { return self.base + n; }
}
fn main() {
    let a = new Adder(10);
    println(a.add(5));
    println(a.add(90));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n100\n");
}

// 4. Method that calls another method on self
#[test]
fn test_class_method_calls_sibling_method() {
    let (out, ok) = compile_and_run(
        r#"
class Circle {
    pub init(self, r: i64) {
        self.r = r;
    }
    r: i64;
    pub fn radius(self) -> i64     { return self.r; }
    pub fn diameter(self) -> i64   { return self.r * 2; }
}
fn main() {
    let c = new Circle(5);
    println(c.diameter());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// 5. Object passed to a free function
#[test]
fn test_class_object_passed_to_free_function() {
    let (out, ok) = compile_and_run(
        r#"
class Val {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn double(x: Val) -> i64 { return x.get() * 2; }
fn main() {
    let x = new Val(21);
    println(double(x));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 6. Object returned from free function
#[test]
fn test_class_object_returned_from_function() {
    let (out, ok) = compile_and_run(
        r#"
class Point {
    pub init(self, x: i64, y: i64) {
        self.x = x;
        self.y = y;
    }
    x: i64;
    y: i64;
    pub fn x(self) -> i64 { return self.x; }
    pub fn y(self) -> i64 { return self.y; }
}
fn make(x: i64, y: i64) -> Point { return new Point(x, y); }
fn main() {
    let p = make(3, 4);
    println(p.x());
    println(p.y());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n4\n");
}

// 7. Bool field
#[test]
fn test_class_bool_field() {
    let (out, ok) = compile_and_run(
        r#"
class Flag {
    pub init(self, on: bool) {
        self.on = on;
    }
    on: bool;
    pub fn is_on(self) -> bool { return self.on; }
}
fn main() {
    let f = new Flag(true);
    println(f.is_on());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// 8. String field
#[test]
fn test_class_string_field() {
    let (out, ok) = compile_and_run(
        r#"
class Msg {
    pub init(self, text: String) {
        self.text = text;
    }
    text: String;
    pub fn get(self) -> String { return self.text; }
}
fn main() {
    let m = new Msg("hello");
    println(m.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 9. f64 field
#[test]
fn test_class_f64_field() {
    let (out, ok) = compile_and_run(
        r#"
class Temp {
    pub init(self, celsius: f64) {
        self.celsius = celsius;
    }
    celsius: f64;
    pub fn get(self) -> f64 { return self.celsius; }
}
fn main() {
    let t = new Temp(36.6);
    println(t.get());
}
"#,
    );
    assert!(ok);
    assert!(out.starts_with("36.6"));
}

// 10. Nested class fields
#[test]
fn test_class_nested_field() {
    let (out, ok) = compile_and_run(
        r#"
class Inner {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
class Outer {
    pub init(self, inner: Inner) {
        self.inner = inner;
    }
    pub inner: Inner;
    pub fn inner(self) -> Inner { return self.inner; }
}
fn main() {
    let o = new Outer(new Inner(99));
    println(o.inner().get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// 11. Multiple objects of same class
#[test]
fn test_class_multiple_instances() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let a = new Box(1);
    let b = new Box(2);
    let c = new Box(3);
    let va = a.get();
    let vb = b.get();
    let vc = c.get();
    println(va + vb + vc);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n");
}

// 12. Free-function constructor (factory pattern)
#[test]
fn test_class_static_constructor_method() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn make_counter(start: i64) -> Counter { return new Counter(start); }
fn main() {
    let c = make_counter(42);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 13. Method returning bool comparison
#[test]
fn test_class_method_returns_bool() {
    let (out, ok) = compile_and_run(
        r#"
class Score {
    pub init(self, points: i64) {
        self.points = points;
    }
    points: i64;
    pub fn passing(self) -> bool { return self.points >= 60; }
}
fn main() {
    let s1 = new Score(80);
    let s2 = new Score(40);
    println(s1.passing());
    println(s2.passing());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

// 14. Method with conditional logic
#[test]
fn test_class_method_with_if() {
    let (out, ok) = compile_and_run(
        r#"
class Abs {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn abs(self) -> i64 {
        if self.v < 0 {
            return self.v * -1;
        }
        return self.v;
    }
}
fn main() {
    let a = new Abs(-5);
    let b = new Abs(3);
    println(a.abs());
    println(b.abs());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n3\n");
}

// 15. Method with loop
#[test]
fn test_class_method_with_loop() {
    let (out, ok) = compile_and_run(
        r#"
class Pow {
    pub init(self, base: i64) {
        self.base = base;
    }
    base: i64;
    pub fn pow(self, exp: i64) -> i64 {
        let mut result = 1;
        let mut i = 0;
        while i < exp {
            result = result * self.base;
            i = i + 1;
        }
        return result;
    }
}
fn main() {
    let p = new Pow(2);
    println(p.pow(8));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "256\n");
}

// 16. Public field direct access
#[test]
fn test_class_public_field_direct_access() {
    let (out, ok) = compile_and_run(
        r#"
class Point {
    pub x: i64;
    pub y: i64;
}
fn main() {
    let p = new Point(10, 20);
    println(p.x);
    println(p.y);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n20\n");
}

// 17. Private field rejected outside class
#[test]
fn test_class_private_field_rejected_outside() {
    assert!(expect_compile_error(
        r#"
class Pair {
    a: i64;
    b: i64;
}
fn main() {
    let p = new Pair(1, 2);
    println(p.a);
}
"#
    ));
}

// 18. Private method rejected outside class
#[test]
fn test_class_private_method_rejected_outside() {
    assert!(expect_compile_error(
        r#"
class Pair {
    a: i64;
    fn sum(self) -> i64 { return self.a; }
}
fn main() {
    let p = new Pair(1);
    println(p.sum());
}
"#
    ));
}

// 19. Simple inheritance: child can be assigned to base variable (type check)
#[test]
fn test_class_inheritance_inherits_method() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub open fn value(self) -> i64 { return 42; }
}
pub class Child extends Base {
    pub override fn value(self) -> i64 { return 42; }
}
fn main() {
    let c = new Child();
    println(c.value());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 20. Override: derived class method called directly on derived type
#[test]
fn test_class_override_changes_value() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub open fn id(self) -> i64 { return 1; }
}
pub class Derived extends Base {
    pub override fn id(self) -> i64 { return 2; }
}
fn main() {
    let b = new Base();
    let d = new Derived();
    println(b.id());
    println(d.id());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

// 21. Two levels of inheritance, each called directly
#[test]
fn test_class_two_level_inheritance() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    pub open fn tag(self) -> i64 { return 1; }
}
pub open class B extends A {
    pub open override fn tag(self) -> i64 { return 2; }
}
pub class C extends B {
    pub override fn tag(self) -> i64 { return 3; }
}
fn main() {
    let a = new A();
    let b = new B();
    let c = new C();
    println(a.tag());
    println(b.tag());
    println(c.tag());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n3\n");
}

// 22. Child inherits field from base, override method accesses inherited field
#[test]
fn test_class_child_with_field_and_inherited_method() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Named {
    pub name: String;
    pub open fn greet(self) -> String { return self.name; }
}
pub class Employee extends Named {
    pub dept: String;
    pub override fn greet(self) -> String { return self.name; }
    pub fn dept(self) -> String { return self.dept; }
}
fn main() {
    let e = new Employee("Alice", "Eng");
    println(e.greet());
    println(e.dept());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Alice\nEng\n");
}

// 23. Override reads inherited public i64 field
#[test]
fn test_class_override_reads_base_field() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub age: i64;
    pub open fn describe(self) -> i64 { return self.age; }
}
pub class Cat extends Animal {
    pub override fn describe(self) -> i64 { return self.age * 2; }
}
fn main() {
    let base = new Animal(5);
    let cat = new Cat(3);
    println(base.describe());
    println(cat.describe());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n6\n");
}

// 24. Extending a non-open class reports error
#[test]
fn test_class_extend_non_open_is_error() {
    assert!(expect_compile_error(
        r#"
class Closed {}
class Child extends Closed {}
fn main() { println(1); }
"#
    ));
}

// 25. Override without keyword reports error
#[test]
fn test_class_override_without_keyword_is_error() {
    assert!(expect_compile_error(
        r#"
pub open class Base {
    pub open fn foo(self) -> i64 { return 1; }
}
pub class Child extends Base {
    pub fn foo(self) -> i64 { return 2; }
}
fn main() { println(1); }
"#
    ));
}

// 26. Override non-open method is error
#[test]
fn test_class_override_non_open_method_is_error() {
    assert!(expect_compile_error(
        r#"
pub open class Base {
    pub fn foo(self) -> i64 { return 1; }
}
pub class Child extends Base {
    pub override fn foo(self) -> i64 { return 2; }
}
fn main() { println(1); }
"#
    ));
}

// 27. Object stored in local variable, method called later
#[test]
fn test_class_stored_then_method_called() {
    let (out, ok) = compile_and_run(
        r#"
class Token {
    pub init(self, id: i64) {
        self.id = id;
    }
    id: i64;
    pub fn id(self) -> i64 { return self.id; }
}
fn main() {
    let t = new Token(77);
    let val = t.id();
    println(val);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "77\n");
}

// 28. Multiple method calls on same object
#[test]
fn test_class_multiple_method_calls_on_same_obj() {
    let (out, ok) = compile_and_run(
        r#"
class Stats {
    pub init(self, total: i64, count: i64) {
        self.total = total;
        self.count = count;
    }
    total: i64;
    count: i64;
    pub fn total(self) -> i64 { return self.total; }
    pub fn count(self) -> i64 { return self.count; }
    pub fn avg(self) -> i64   { return self.total / self.count; }
}
fn main() {
    let s = new Stats(90, 3);
    println(s.total());
    println(s.count());
    println(s.avg());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "90\n3\n30\n");
}

// 29. Method returns another class instance
#[test]
fn test_class_method_returns_class_instance() {
    let (out, ok) = compile_and_run(
        r#"
class Inner {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
class Outer {
    pub fn make_inner(self, v: i64) -> Inner { return new Inner(v); }
}
fn main() {
    let o = new Outer();
    let i = o.make_inner(55);
    println(i.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "55\n");
}

// 30. Class with Option field
#[test]
fn test_class_with_option_field() {
    let (out, ok) = compile_and_run(
        r#"
class MaybeNum {
    pub init(self, val: Option<i64>) {
        self.val = val;
    }
    val: Option<i64>;
    pub fn get_or(self, def: i64) -> i64 { return self.val.unwrap_or(def); }
    pub fn has_value(self) -> bool { return self.val.is_some(); }
}
fn main() {
    let a = new MaybeNum(Option::Some(10));
    let b = new MaybeNum(Option::None);
    println(a.get_or(0));
    println(b.get_or(99));
    println(a.has_value());
    println(b.has_value());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n99\ntrue\nfalse\n");
}

// 31. Class with Result field
#[test]
fn test_class_with_result_field() {
    let (out, ok) = compile_and_run(
        r#"
class Op {
    pub init(self, result: Result<i64, String>) {
        self.result = result;
    }
    result: Result<i64, String>;
    pub fn ok_or(self, def: i64) -> i64 { return self.result.unwrap_or(def); }
    pub fn succeeded(self) -> bool { return self.result.is_ok(); }
}
fn main() {
    let a = new Op(Result::Ok(7));
    let b = new Op(Result::Err("fail"));
    println(a.ok_or(0));
    println(b.ok_or(0));
    println(a.succeeded());
    println(b.succeeded());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n0\ntrue\nfalse\n");
}

// 32. Class method returns Option
#[test]
fn test_class_method_returns_option() {
    let (out, ok) = compile_and_run(
        r#"
class Lookup {
    pub init(self, key: i64, value: i64) {
        self.key = key;
        self.value = value;
    }
    key: i64;
    value: i64;
    pub fn find(self, k: i64) -> Option<i64> {
        if self.key == k {
            return Option::Some(self.value);
        }
        return Option::None;
    }
}
fn main() {
    let l = new Lookup(5, 100);
    println(l.find(5).unwrap());
    println(l.find(9).is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\ntrue\n");
}

// 33. Class method returns Result
#[test]
fn test_class_method_returns_result() {
    let (out, ok) = compile_and_run(
        r#"
class Divider {
    pub init(self, denom: i64) {
        self.denom = denom;
    }
    denom: i64;
    pub fn divide(self, n: i64) -> Result<i64, String> {
        if self.denom == 0 {
            return Result::Err("division by zero");
        }
        return Result::Ok(n / self.denom);
    }
}
fn main() {
    let d = new Divider(4);
    let z = new Divider(0);
    println(d.divide(20).unwrap());
    println(z.divide(1).is_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\ntrue\n");
}

// 34. Array-free accumulator via two class instances
#[test]
fn test_class_two_counters_independent() {
    let (out, ok) = compile_and_run(
        r#"
class Acc {
    pub init(self, start: i64) {
        self.start = start;
    }
    start: i64;
    pub fn sum_to(self, n: i64) -> i64 {
        let mut s = self.start;
        let mut i = 0;
        while i < n {
            s = s + i;
            i = i + 1;
        }
        return s;
    }
}
fn main() {
    let a = new Acc(0);
    let b = new Acc(100);
    println(a.sum_to(5));
    println(b.sum_to(5));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n110\n");
}

// 35. GC: class allocated inside function, returned as primitive
#[test]
fn test_class_gc_inner_alloc_returns_primitive() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn compute() -> i64 {
    let t = new Tmp(42);
    return t.get();
}
fn main() {
    gc_collect();
    let x = compute();
    gc_collect();
    println(x);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 36. GC: live object survives collect
#[test]
fn test_class_gc_live_object_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let b = new Box(123);
    gc_collect();
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "123\n");
}

// 37. GC: two objects, one goes out of scope
#[test]
fn test_class_gc_one_survives_one_collected() {
    let (out, ok) = compile_and_run(
        r#"
class Obj {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn make_and_drop() {
    let tmp = new Obj(999);
}
fn main() {
    let live = new Obj(7);
    make_and_drop();
    gc_collect();
    println(live.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// 38. Optional class: explicit None
#[test]
fn test_class_option_accepts_none() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let n: Option<Node> = None;
    println(n.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// 39. Option: open Some before using the object
#[test]
fn test_class_option_match_then_use() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn maybe_get(n: Option<Node>) -> i64 {
    match n {
        Some(value) => return value.get(),
        None => return -1,
    }
}
fn main() {
    let a: Option<Node> = Some(new Node(5));
    let b: Option<Node> = None;
    println(maybe_get(a));
    println(maybe_get(b));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n-1\n");
}

// 40. Class as function argument and return type
#[test]
fn test_class_as_fn_arg_and_return() {
    let (out, ok) = compile_and_run(
        r#"
class Vec2 {
    pub x: i64;
    pub y: i64;
    pub fn x(self) -> i64 { return self.x; }
    pub fn y(self) -> i64 { return self.y; }
}
fn add(a: Vec2, b: Vec2) -> Vec2 {
    return new Vec2(a.x() + b.x(), a.y() + b.y());
}
fn main() {
    let u = new Vec2(1, 2);
    let v = new Vec2(3, 4);
    let w = add(u, v);
    println(w.x());
    println(w.y());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n6\n");
}

// 41. Class with two String fields, each returned independently
#[test]
fn test_class_method_string_concat() {
    let (out, ok) = compile_and_run(
        r#"
class Person {
    pub init(self, first: String, last: String) {
        self.first = first;
        self.last = last;
    }
    first: String;
    last: String;
    pub fn first(self) -> String { return self.first; }
    pub fn last(self) -> String  { return self.last;  }
}
fn main() {
    let p = new Person("Jane", "Doe");
    println(p.first());
    println(p.last());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Jane\nDoe\n");
}

// 42. Class method used in boolean expression
#[test]
fn test_class_method_in_boolean_expr() {
    let (out, ok) = compile_and_run(
        r#"
class Range {
    pub init(self, lo: i64, hi: i64) {
        self.lo = lo;
        self.hi = hi;
    }
    lo: i64;
    hi: i64;
    pub fn contains(self, v: i64) -> bool { return v >= self.lo && v <= self.hi; }
}
fn main() {
    let r = new Range(10, 20);
    println(r.contains(15));
    println(r.contains(25));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

// 43. Class method used in while condition
#[test]
fn test_class_method_in_while_condition() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, limit: i64) {
        self.limit = limit;
    }
    limit: i64;
    pub fn below(self, n: i64) -> bool { return n < self.limit; }
}
fn main() {
    let c = new Counter(3);
    let mut i = 0;
    while c.below(i) {
        println(i);
        i = i + 1;
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n1\n2\n");
}

// 44. Two different class types in same function
#[test]
fn test_class_two_different_classes() {
    let (out, ok) = compile_and_run(
        r#"
class Width  {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class Height {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn area(w: Width, h: Height) -> i64 { return w.get() * h.get(); }
fn main() {
    let w = new Width(7);
    let h = new Height(3);
    println(area(w, h));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "21\n");
}

// 45. Method returning f64
#[test]
fn test_class_method_returning_f64() {
    let (out, ok) = compile_and_run(
        r#"
class Circle {
    pub init(self, radius: f64) {
        self.radius = radius;
    }
    radius: f64;
    pub fn area(self) -> f64 { return 3.14159 * self.radius * self.radius; }
}
fn main() {
    let c = new Circle(2.0);
    let a = c.area();
    println(a > 12.0);
    println(a < 13.0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// 46. Child accesses inherited public field via speed() method and own override
#[test]
fn test_class_inherited_base_field_via_method() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Vehicle {
    pub speed: i64;
    pub fn speed(self) -> i64 { return self.speed; }
    pub open fn describe(self) -> i64 { return self.speed; }
}
pub class Car extends Vehicle {
    pub override fn describe(self) -> i64 { return self.speed * 2; }
}
fn main() {
    let v = new Vehicle(30);
    let car = new Car(60);
    println(v.speed());
    println(car.speed());
    println(v.describe());
    println(car.describe());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "30\n60\n30\n120\n");
}

// 47. Object used in match expression (via method)
#[test]
fn test_class_method_result_used_in_match() {
    let (out, ok) = compile_and_run(
        r#"
class Tag {
    pub init(self, kind: i64) {
        self.kind = kind;
    }
    kind: i64;
    pub fn kind(self) -> i64 { return self.kind; }
}
fn describe(t: Tag) -> String {
    let k = t.kind();
    if k == 1 { return "one"; }
    if k == 2 { return "two"; }
    return "other";
}
fn main() {
    println(describe(new Tag(1)));
    println(describe(new Tag(2)));
    println(describe(new Tag(9)));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "one\ntwo\nother\n");
}

// 48. Object created in if-else branch
#[test]
fn test_class_created_in_if_else() {
    let (out, ok) = compile_and_run(
        r#"
class Signed {
    pub init(self, v: i64, neg: bool) {
        self.v = v;
        self.neg = neg;
    }
    v: i64;
    neg: bool;
    pub fn value(self) -> i64 { return self.v; }
    pub fn is_neg(self) -> bool { return self.neg; }
}
fn make(n: i64) -> Signed {
    if n < 0 {
        return new Signed(n * -1, true);
    }
    return new Signed(n, false);
}
fn main() {
    let a = make(-7);
    let b = make(3);
    println(a.value());
    println(a.is_neg());
    println(b.value());
    println(b.is_neg());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\ntrue\n3\nfalse\n");
}

// 49. Class missing field in literal is an error
#[test]
fn test_class_literal_missing_field_is_error() {
    assert!(expect_compile_error(
        r#"
class Point { x: i64; y: i64; }
fn main() {
    let p = new Point(1);
    println(1);
}
"#
    ));
}

// 50. Class literal with extra field is an error
#[test]
fn test_class_literal_extra_field_is_error() {
    assert!(expect_compile_error(
        r#"
class Point { x: i64; }
fn main() {
    let p = new Point(1, 2);
    println(1);
}
"#
    ));
}
