use super::*;

// ── Class objects in the LIR walker (willow-0g8j.5) ─────────────────────────
// The walker claims functions that create, read, mutate and pass "simple"
// class objects: no base class, not itself a base, no interface or enum field.
// Class METHOD bodies compile through `compile_class_method_inner`, which walks
// lowered IR of its own (willow-0g8j.3); the programs below exercise the object
// code paths from free functions and `main`.
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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

#[test]
fn lir_diff_74_imported_base_class_still_dispatches_virtually() {
    // `import zoo::Animal;` registers the class under the bare name `Animal`
    // too, while `class_base` keeps canonical names (`zoo::Dog` -> `zoo::Animal`
    // ). A name-only "is anything extending me?" test misses that, calls
    // `Animal` a leaf and emits a DIRECT `Animal__speak` — so a `Dog` passed as
    // an `Animal` would print 5 instead of 1005.
    assert_project_output(
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
    assert_project_output(
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
    assert_project_output(
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
