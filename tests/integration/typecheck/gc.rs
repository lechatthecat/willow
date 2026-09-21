use super::super::support::*;

// ── GC tests ──────────────────────────────────────────────────────────────

// GC-01: single object freed after scope exit
#[test]
fn test_gc_01_single_object_freed_after_scope() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make() -> i64 {
    let b = new Box(1);
    return b.get();
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-02: two objects freed after scope exit
#[test]
fn test_gc_02_two_objects_freed_after_scope() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make() -> i64 {
    let a = new Box(1);
    let b = new Box(2);
    return a.get() + b.get();
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-03: live object NOT freed
#[test]
fn test_gc_03_live_object_not_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let b = new Box(42);
    gc_collect();
    println(b.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\ntrue\n");
}

// GC-04: gc_allocated_bytes increases with each allocation
#[test]
fn test_gc_04_allocated_bytes_grows_per_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; }
fn main() {
    let before = gc_allocated_bytes();
    let _a = new Box(1);
    let mid = gc_allocated_bytes();
    let _b = new Box(2);
    let after = gc_allocated_bytes();
    println(mid > before);
    println(after > mid);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// GC-05: explicit gc_collect returns zero after all freed
#[test]
fn test_gc_05_explicit_collect_returns_zero() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn alloc_and_drop() -> i64 {
    let t = new Tmp(99);
    return t.get();
}
fn main() {
    let r1 = alloc_and_drop();
    let r2 = alloc_and_drop();
    println(r1 + r2);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "198\n0\n");
}

// GC-06: object allocated in loop, freed after loop
#[test]
fn test_gc_06_objects_in_loop_freed_after() {
    let (out, ok) = compile_and_run(
        r#"
class Item {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn process(n: i64) -> i64 {
    let mut i = 0;
    let mut sum = 0;
    while i < n {
        let item = new Item(i);
        sum = sum + item.get();
        i = i + 1;
    }
    return sum;
}
fn main() {
    let result = process(5);
    gc_collect();
    println(result);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n0\n");
}

// GC-07: nested function allocation, inner freed
#[test]
fn test_gc_07_nested_function_alloc_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn inner() -> i64 {
    let n = new Node(5);
    return n.get();
}
fn outer() -> i64 { return inner() + inner(); }
fn main() {
    let r = outer();
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n0\n");
}

// GC-08: object field holding i64 doesn't prevent GC
#[test]
fn test_gc_08_i64_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Point {
    pub init(self, x: i64, y: i64) {
        self.x = x;
        self.y = y;
    } x: i64; y: i64; pub fn sum(self) -> i64 { return self.x + self.y; } }
fn make_sum() -> i64 {
    let p = new Point(3, 4);
    return p.sum();
}
fn main() {
    let _ = make_sum();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-09: bool field object freed
#[test]
fn test_gc_09_bool_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Flag {
    pub init(self, on: bool) {
        self.on = on;
    } on: bool; pub fn get(self) -> bool { return self.on; } }
fn check() -> bool {
    let f = new Flag(true);
    return f.get();
}
fn main() {
    let _ = check();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-10: multiple collect cycles — already-freed objects stay at zero
#[test]
fn test_gc_10_multiple_collect_cycles() {
    let (out, ok) = compile_and_run(
        r#"
class Obj {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_obj() -> i64 { let o = new Obj(1); return o.get(); }
fn main() {
    let _ = drop_obj();
    gc_collect();
    let after1 = gc_allocated_bytes();
    gc_collect();
    let after2 = gc_allocated_bytes();
    println(after1);
    println(after2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n0\n");
}

// GC-11: object reachable through local variable survives
#[test]
fn test_gc_11_local_var_keeps_alive() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let n = new Node(7);
    gc_collect();
    gc_collect();
    println(n.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// GC-12: two live objects both survive collect
#[test]
fn test_gc_12_two_live_objects_both_survive() {
    let (out, ok) = compile_and_run(
        r#"
class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class B {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let a = new A(10);
    let b = new B(20);
    gc_collect();
    println(a.get());
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n20\n");
}

// GC-13: live and dead objects — only dead freed
#[test]
fn test_gc_13_live_and_dead_objects_mixed() {
    let (out, ok) = compile_and_run(
        r#"
class Live {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class Dead {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_dead() -> i64 { let d = new Dead(0); return d.get(); }
fn main() {
    let live = new Live(5);
    let _ = drop_dead();
    gc_collect();
    println(live.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\ntrue\n");
}

// GC-14: object passed to function and returned as i64, original freed
#[test]
fn test_gc_14_passed_to_fn_extract_primitive_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Wrap {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn extract(w: Wrap) -> i64 { return w.get(); }
fn main() {
    let val = extract(new Wrap(99));
    gc_collect();
    println(val);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n0\n");
}

// GC-15: object allocated before and after collect
#[test]
fn test_gc_15_alloc_collect_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_one() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let _ = drop_one();
    gc_collect();
    let b2 = new Box(2);
    println(b2.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\ntrue\n");
}

// GC-16: object with string field — string GC-managed too
#[test]
fn test_gc_16_string_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Msg {
    pub init(self, text: String) {
        self.text = text;
    } text: String; pub fn get(self) -> String { return self.text; } }
fn drop_msg() -> String {
    let m = new Msg("hello");
    return m.get();
}
fn main() {
    let s = drop_msg();
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// GC-17: optional field pointing to live object keeps it alive
#[test]
fn test_gc_17_nullable_field_keeps_child_alive() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub next: Option<Node>; }
fn main() {
    let tail = new Node(2, None);
    let head = new Node(1, Some(tail));
    gc_collect();
    println(head.v);
    let n = head.next;
    match n { Some(value) => println(value.v), None => println(0) }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

// GC-18: None field — object still freed when out of scope
#[test]
fn test_gc_18_nil_nullable_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64, next: Option<Node>) {
        self.v = v;
        self.next = next;
    } v: i64; next: Option<Node>; pub fn get(self) -> i64 { return self.v; } }
fn make() -> i64 {
    let n = new Node(3, None);
    return n.get();
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-19: chain of nullable nodes — all freed together
#[test]
fn test_gc_19_chain_of_nodes_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub next: Option<Node>; }
fn make_chain() -> i64 {
    let c = new Node(3, None);
    let b = new Node(2, Some(c));
    let a = new Node(1, Some(b));
    return a.v;
}
fn main() {
    let _ = make_chain();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-20: chain of nullable nodes — head kept, rest freed (not possible to free partial chain while head live)
#[test]
fn test_gc_20_live_chain_all_survive() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub next: Option<Node>; }
fn main() {
    let c = new Node(3, None);
    let b = new Node(2, Some(c));
    let a = new Node(1, Some(b));
    gc_collect();
    println(a.v);
    let n1 = a.next;
    match n1 {
        Some(first) => {
            println(first.v);
            match first.next { Some(second) => println(second.v), None => println(0) }
        },
        None => println(0),
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n3\n");
}

// GC-21: inherited class object freed
#[test]
fn test_gc_21_inherited_class_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn drop_child() -> i64 { let c = new Child(5); return c.get(); }
fn main() {
    let _ = drop_child();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-22: inherited class object live, survives
#[test]
fn test_gc_22_inherited_class_live_survives() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn main() {
    let c = new Child(11);
    gc_collect();
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "11\n");
}

// GC-23: object with prot field freed
#[test]
fn test_gc_23_prot_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Secret {
    pub init(self, key: i64) {
        self.key = key;
    } prot key: i64; pub fn get(self) -> i64 { return self.key; } }
fn drop_it() -> i64 { let s = new Secret(7); return s.get(); }
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-24: object allocated inside if-branch, freed after branch
#[test]
fn test_gc_24_object_in_if_branch_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn conditional(flag: bool) -> i64 {
    if flag {
        let t = new Tmp(3);
        return t.get();
    }
    return 0;
}
fn main() {
    let _ = conditional(true);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-25: object alive across if-branch
#[test]
fn test_gc_25_object_alive_across_if() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let b = new Box(9);
    if b.get() > 0 {
        gc_collect();
    }
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n");
}

// GC-26: object allocated inside while loop, freed each iteration
#[test]
fn test_gc_26_loop_object_freed_each_iteration() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let mut i = 0;
    while i < 3 {
        let t = new Tmp(i);
        let _ = t.get();
        i = i + 1;
    }
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-27: collect before allocation — zero
#[test]
fn test_gc_27_collect_before_any_alloc() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-28: allocated_bytes zero at start of program
#[test]
fn test_gc_28_bytes_zero_at_start() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-29: object size proportional to field count
#[test]
fn test_gc_29_larger_object_uses_more_bytes() {
    let (out, ok) = compile_and_run(
        r#"
class Small {
    pub init(self, a: i64) {
        self.a = a;
    } a: i64; }
class Large {
    pub init(self, a: i64, b: i64, c: i64, d: i64) {
        self.a = a;
        self.b = b;
        self.c = c;
        self.d = d;
    } a: i64; b: i64; c: i64; d: i64; }
fn main() {
    let before = gc_allocated_bytes();
    let _s = new Small(1);
    let after_small = gc_allocated_bytes();
    let _l = new Large(1, 2, 3, 4);
    let after_large = gc_allocated_bytes();
    println(after_small > before);
    println(after_large > after_small);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// GC-30: GC-managed object returned from function, caller holds it
#[test]
fn test_gc_30_object_returned_and_held_by_caller() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make(v: i64) -> Node { return new Node(v); }
fn main() {
    let n = make(55);
    gc_collect();
    println(n.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "55\n");
}

// GC-31: object passed to function, function holds local copy
#[test]
fn test_gc_31_object_alive_while_in_called_function() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn use_box(b: Box) -> i64 {
    gc_collect();
    return b.get();
}
fn main() {
    let b = new Box(7);
    println(use_box(b));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// GC-32: two separate collect calls
#[test]
fn test_gc_32_two_separate_collects() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_one() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let r1 = drop_one();
    gc_collect();
    let b = new Box(2);
    let r2 = b.get();
    gc_collect();
    println(r1 + r2);
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\ntrue\n");
}

// GC-33: Option<T> with class payload — freed when out of scope
#[test]
fn test_gc_33_option_class_payload_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_opt() -> i64 {
    let opt = Option::Some(new Node(42));
    return match opt {
        Option::Some(n) => n.get(),
        Option::None => 0,
    };
}
fn main() {
    let _ = make_opt();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-34: Option::Some class payload survives when held
#[test]
fn test_gc_34_option_class_payload_survives_when_held() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let opt = Option::Some(new Node(13));
    gc_collect();
    let v = match opt {
        Option::Some(n) => n.get(),
        Option::None => 0,
    };
    println(v);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "13\n");
}

// GC-35: Result::Ok with class payload freed when out of scope
#[test]
fn test_gc_35_result_ok_payload_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_res() -> i64 {
    let r: Result<Node, String> = Result::Ok(new Node(7));
    return match r {
        Result::Ok(n) => n.get(),
        Result::Err(_) => 0,
    };
}
fn main() {
    let _ = make_res();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-36: Result::Ok class payload survives when held
#[test]
fn test_gc_36_result_ok_payload_survives_when_held() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let r: Result<Node, String> = Result::Ok(new Node(17));
    gc_collect();
    let v = match r {
        Result::Ok(n) => n.get(),
        Result::Err(_) => 0,
    };
    println(v);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "17\n");
}

// GC-37: gc_collect does not corrupt live i64 variables
#[test]
fn test_gc_37_collect_does_not_corrupt_i64_vars() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = 12345;
    gc_collect();
    println(x);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12345\n");
}

// GC-38: gc_collect does not corrupt live bool variables
#[test]
fn test_gc_38_collect_does_not_corrupt_bool_vars() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let b = true;
    gc_collect();
    println(b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// GC-39: gc_collect does not corrupt live string variables
#[test]
fn test_gc_39_collect_does_not_corrupt_string_vars() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s = "hello gc";
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello gc\n");
}

// GC-40: object with multiple i64 fields freed correctly
#[test]
fn test_gc_40_multi_i64_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Quad {
    pub init(self, a: i64, b: i64, c: i64, d: i64) {
        self.a = a;
        self.b = b;
        self.c = c;
        self.d = d;
    } a: i64; b: i64; c: i64; d: i64;
    pub fn sum(self) -> i64 { return self.a + self.b + self.c + self.d; }
}
fn make() -> i64 {
    let q = new Quad(1, 2, 3, 4);
    return q.sum();
}
fn main() {
    let r = make();
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n0\n");
}

// GC-41: object allocated in deeply nested function freed
#[test]
fn test_gc_41_deep_nested_alloc_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn f3() -> i64 { let n = new Node(3); return n.get(); }
fn f2() -> i64 { return f3() + f3(); }
fn f1() -> i64 { return f2() + f2(); }
fn main() {
    let _ = f1();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-42: recursive function allocating objects — all freed after recursion
#[test]
fn test_gc_42_recursive_alloc_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn sum(n: i64) -> i64 {
    if n <= 0 { return 0; }
    let node = new Node(n);
    return node.get() + sum(n - 1);
}
fn main() {
    let _ = sum(5);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-43: live object in recursive function survives
#[test]
fn test_gc_43_live_object_in_recursive_fn_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn fib(n: i64) -> i64 {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fn main() {
    let b = new Box(fib(5));
    gc_collect();
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

// GC-44: object stored in multiple variables (aliases) — freed when all out of scope
#[test]
fn test_gc_44_alias_both_out_of_scope_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_two() -> i64 {
    let a = new Node(1);
    let b = a;
    return b.get();
}
fn main() {
    let _ = make_two();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-45: object returned from if-else — retained by caller
#[test]
fn test_gc_45_conditional_returned_object_retained() {
    let (out, ok) = compile_and_run(
        r#"
class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class B {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_a(flag: bool) -> i64 {
    if flag {
        let a = new A(10);
        return a.get();
    }
    let b = new B(20);
    return b.get();
}
fn main() {
    let ra = make_a(true);
    let rb = make_a(false);
    gc_collect();
    println(ra);
    println(rb);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n20\n0\n");
}

// GC-46: enum payload (non-class) — no GC impact expected
#[test]
fn test_gc_46_i64_enum_payload_no_gc_impact() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let before = gc_allocated_bytes();
    let opt = Option::Some(42);
    let after = gc_allocated_bytes();
    println(after > before);
    let _ = match opt { Option::Some(v) => v, Option::None => 0 };
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// GC-47: gc_collect called with no allocations is safe
#[test]
fn test_gc_47_collect_with_no_allocs_safe() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    gc_collect();
    gc_collect();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-48: large number of objects freed in one collect
#[test]
fn test_gc_48_many_objects_freed_together() {
    let (out, ok) = compile_and_run(
        r#"
class Obj {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_many() -> i64 {
    let mut sum = 0;
    let mut i = 0;
    while i < 20 {
        let o = new Obj(i);
        sum = sum + o.get();
        i = i + 1;
    }
    return sum;
}
fn main() {
    let _ = make_many();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-49: objects allocated in separate scopes both freed
#[test]
fn test_gc_49_two_scopes_both_freed() {
    let (out, ok) = compile_and_run(
        r#"
class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class B {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn scope1() -> i64 { let a = new A(1); return a.get(); }
fn scope2() -> i64 { let b = new B(2); return b.get(); }
fn main() {
    let r1 = scope1();
    let r2 = scope2();
    println(r1 + r2);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n0\n");
}

// GC-50: gc_allocated_bytes is monotonically increasing without collect
#[test]
fn test_gc_50_bytes_monotonically_increasing() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; }
fn main() {
    let b0 = gc_allocated_bytes();
    let _a = new Box(1);
    let b1 = gc_allocated_bytes();
    let _b = new Box(2);
    let b2 = gc_allocated_bytes();
    let _c = new Box(3);
    let b3 = gc_allocated_bytes();
    println(b1 >= b0);
    println(b2 >= b1);
    println(b3 >= b2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\ntrue\n");
}

// GC-51: object with only public fields freed
#[test]
fn test_gc_51_all_public_fields_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Point { pub x: i64; pub y: i64; }
fn drop_it() -> i64 {
    let p = new Point(3, 4);
    return p.x + p.y;
}
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-52: inherited object freed (child)
#[test]
fn test_gc_52_child_class_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64, extra: i64) {
        super.init(v);
        self.extra = extra;
    } extra: i64; pub fn extra(self) -> i64 { return self.extra; } }
fn drop_child() -> i64 {
    let c = new Child(1, 2);
    return c.get() + c.extra();
}
fn main() {
    let r = drop_child();
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n0\n");
}

// GC-53: inherited object survives when live
#[test]
fn test_gc_53_child_class_survives_when_live() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64, extra: i64) {
        super.init(v);
        self.extra = extra;
    } extra: i64; }
fn main() {
    let c = new Child(10, 5);
    gc_collect();
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// GC-54: three-level hierarchy object freed
#[test]
fn test_gc_54_three_level_hierarchy_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub open class B extends A {
    pub init(self, v: i64) {
        super.init(v);
    }}
pub class C extends B {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn drop_c() -> i64 { let c = new C(7); return c.get(); }
fn main() {
    let _ = drop_c();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-55: three-level hierarchy object survives when live
#[test]
fn test_gc_55_three_level_hierarchy_survives() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub open class B extends A {
    pub init(self, v: i64) {
        super.init(v);
    }}
pub class C extends B {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn main() {
    let c = new C(22);
    gc_collect();
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "22\n");
}

// GC-56: object holding child type — all freed
#[test]
fn test_gc_56_object_holding_child_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn process() -> i64 {
    let c = new Child(3);
    let b: Base = c;
    return b.get();
}
fn main() {
    let _ = process();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-57: object method returns new object (both freed after scope)
#[test]
fn test_gc_57_method_returning_new_object_both_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Outer {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn val(self) -> i64 { return self.v; } }
class Inner {
    pub init(self, w: i64) {
        self.w = w;
    } w: i64; pub fn val(self) -> i64 { return self.w; } }
fn compute() -> i64 {
    let o = new Outer(5);
    let i = new Inner(o.val() * 2);
    return i.val();
}
fn main() {
    let _ = compute();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-58: gc_allocated_bytes after one collect then one alloc equals one object
#[test]
fn test_gc_58_bytes_after_collect_then_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class B {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_a() -> i64 { let a = new A(1); return a.get(); }
fn main() {
    let r = drop_a();
    println(r);
    gc_collect();
    let zero = gc_allocated_bytes();
    let b = new B(2);
    let one = gc_allocated_bytes();
    println(zero);
    println(one > 0);
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n0\ntrue\n2\n");
}

// GC-59: object with f64 field freed
#[test]
fn test_gc_59_f64_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Flt {
    pub init(self, v: f64) {
        self.v = v;
    } v: f64; pub fn get(self) -> f64 { return self.v; } }
fn drop_it() -> f64 { let f = new Flt(1.5); return f.get(); }
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-60: object with f64 field survives collect
#[test]
fn test_gc_60_f64_field_object_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Flt {
    pub init(self, v: f64) {
        self.v = v;
    } v: f64; pub fn get(self) -> f64 { return self.v; } }
fn main() {
    let f = new Flt(2.5);
    gc_collect();
    let r = f.get();
    println(r > 2.0);
    println(r < 3.0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// GC-61: optional object freed when matched scope exits
#[test]
fn test_gc_61_nullable_freed_when_scope_exits() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn extract(n: Option<Node>) -> i64 {
    match n { Some(value) => return value.get(), None => return 0 }
}
fn make_and_extract() -> i64 {
    let n: Option<Node> = Some(new Node(5));
    return extract(n);
}
fn main() {
    let r = make_and_extract();
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n0\n");
}

// GC-62: Option<GCRef>::None uses the niche and allocates nothing
#[test]
fn test_gc_62_nullable_nil_no_extra_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; }
fn main() {
    let before = gc_allocated_bytes();
    let n: Option<Node> = None;
    let after = gc_allocated_bytes();
    println(n.is_none());
    println(before);
    println(after);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n0\n0\n");
}

// GC-63: two optional nodes, one None — only Some payload is freed
#[test]
fn test_gc_63_nullable_one_nil_one_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make() -> i64 {
    let a: Option<Node> = Some(new Node(1));
    let b: Option<Node> = None;
    return match a { Some(value) => value.get(), None => 0 };
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-64: object reachable from function parameter — not freed during call
#[test]
fn test_gc_64_object_not_freed_while_in_param() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn consume(b: Box) -> i64 {
    gc_collect();
    return b.get();
}
fn main() {
    println(consume(new Box(77)));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "77\n");
}

// GC-65: object in Option::Some survives gc when option is live
#[test]
fn test_gc_65_option_some_live_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let opt = Option::Some(new Node(8));
    gc_collect();
    let v = opt.unwrap();
    println(v.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "8\n");
}

// GC-66: gc_collect after empty loop still zero
#[test]
fn test_gc_66_collect_after_zero_iter_loop() {
    let (out, ok) = compile_and_run(
        r#"
class Obj {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; }
fn main() {
    let mut i = 0;
    while i < 0 {
        let _ = new Obj(i);
        i = i + 1;
    }
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-67: object freed after being passed by value and returned as i64
#[test]
fn test_gc_67_pass_by_value_extract_i64_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Wrap {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn extract(w: Wrap) -> i64 { return w.get(); }
fn main() {
    let r = extract(new Wrap(100));
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\n0\n");
}

// GC-68: class with inherited prot field freed
#[test]
fn test_gc_68_inherited_prot_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } prot v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64) {
        super.init(v);
    } pub fn doubled(self) -> i64 { return self.v * 2; } }
fn drop_child() -> i64 {
    let c = new Child(4);
    return c.doubled();
}
fn main() {
    let _ = drop_child();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-69: object alive through method chain
#[test]
fn test_gc_69_object_alive_through_method_chain() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } pub fn doubled(self) -> i64 { return self.v * 2; } }
fn main() {
    let n = new Node(5);
    gc_collect();
    let a = n.get();
    let b = n.doubled();
    println(a);
    println(b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n10\n");
}

// GC-70: allocate, collect, verify zero, allocate again, verify positive
#[test]
fn test_gc_70_alloc_collect_zero_alloc_positive() {
    let (out, ok) = compile_and_run(
        r#"
class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_a() -> i64 { let a = new A(1); return a.get(); }
fn main() {
    let r = drop_a();
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
    let b = new A(2);
    println(gc_allocated_bytes() > 0);
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n0\ntrue\n2\n");
}

// GC-71: deeply nested optional reference — all alive while root is live
#[test]
fn test_gc_71_nested_nullable_chain_all_alive() {
    let (out, ok) = compile_and_run(
        r#"
class N { pub v: i64; pub n: Option<N>; }
fn main() {
    let d = new N(3, None);
    let c = new N(2, Some(d));
    let b = new N(1, Some(c));
    gc_collect();
    println(b.v);
    let bc = b.n;
    match bc {
        Some(first) => {
            println(first.v);
            match first.n { Some(second) => println(second.v), None => println(0) }
        },
        None => println(0),
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n3\n");
}

// GC-72: object used in conditional — survives both branches
#[test]
fn test_gc_72_object_survives_across_condition() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let b = new Box(3);
    let flag = b.get() > 2;
    gc_collect();
    if flag {
        println(b.get());
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

// GC-73: gc does not affect i64 arithmetic result
#[test]
fn test_gc_73_gc_does_not_affect_arithmetic() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let t = new Tmp(10);
    let tv = t.get();
    let x = tv * 3;
    gc_collect();
    println(x);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "30\n");
}

// GC-74: object allocated after collect has fresh identity
#[test]
fn test_gc_74_post_collect_object_fresh() {
    let (out, ok) = compile_and_run(
        r#"
class V {
    pub init(self, val: i64) {
        self.val = val;
    } val: i64; pub fn get(self) -> i64 { return self.val; } }
fn drop_v() -> i64 { let v = new V(1); return v.get(); }
fn main() {
    let _ = drop_v();
    gc_collect();
    let v2 = new V(99);
    println(v2.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// GC-75: multiple classes, mixed live and dead
#[test]
fn test_gc_75_mixed_live_dead_multiple_classes() {
    let (out, ok) = compile_and_run(
        r#"
class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class B {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class C {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_bc() -> i64 {
    let b = new B(2);
    let c = new C(3);
    return b.get() + c.get();
}
fn main() {
    let a = new A(1);
    let _ = drop_bc();
    gc_collect();
    println(a.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\ntrue\n");
}

// GC-76: object with bool field freed
#[test]
fn test_gc_76_bool_field_object_freed_correctly() {
    let (out, ok) = compile_and_run(
        r#"
class Toggle {
    pub init(self, flag: bool) {
        self.flag = flag;
    } flag: bool; pub fn get(self) -> bool { return self.flag; } }
fn drop_toggle() -> bool {
    let t = new Toggle(false);
    return t.get();
}
fn main() {
    let _ = drop_toggle();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-77: object survives across multiple function calls
#[test]
fn test_gc_77_object_survives_multiple_fn_calls() {
    let (out, ok) = compile_and_run(
        r#"
class Acc {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn use_acc(a: Acc) -> i64 { return a.get(); }
fn main() {
    let a = new Acc(5);
    let r1 = use_acc(a);
    gc_collect();
    let r2 = use_acc(a);
    gc_collect();
    println(r1 + r2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// GC-78: interleaved alloc/collect/alloc/collect stays consistent
#[test]
fn test_gc_78_interleaved_alloc_collect() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_box(v: i64) -> i64 { let b = new Box(v); return b.get(); }
fn main() {
    let r1 = drop_box(1);
    gc_collect();
    let r2 = drop_box(2);
    gc_collect();
    let r3 = drop_box(3);
    gc_collect();
    println(r1 + r2 + r3);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n0\n");
}

// GC-79: object created inside match arm freed
#[test]
fn test_gc_79_object_in_match_arm_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn wrap(v: i64) -> i64 {
    let t = new Tmp(v);
    return t.get();
}
fn main() {
    let r = match Option::Some(9) {
        Option::Some(v) => wrap(v),
        Option::None => 0,
    };
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n0\n");
}

// GC-80: subtype object used as base type — GC still works
#[test]
fn test_gc_80_subtype_as_base_gc_works() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn process(b: Base) -> i64 { return b.get(); }
fn make() -> i64 {
    let c = new Child(6);
    return process(c);
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-81: object method call does not prevent GC after scope
#[test]
fn test_gc_81_method_call_then_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, n: i64) {
        self.n = n;
    } n: i64; pub fn next(self) -> i64 { return self.n + 1; } }
fn run() -> i64 {
    let c = new Counter(0);
    return c.next() + c.next();
}
fn main() {
    let _ = run();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-82: object with optional class field — field freed with owner
#[test]
fn test_gc_82_optional_class_field_freed_with_owner() {
    let (out, ok) = compile_and_run(
        r#"
class Inner {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class Outer {
    pub init(self, child: Option<Inner>) {
        self.child = child;
    } pub child: Option<Inner>; }
fn make() -> i64 {
    let i = new Inner(3);
    let o = new Outer(Some(i));
    let c = o.child;
    return match c { Some(value) => value.get(), None => 0 };
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-83: object with None optional field is freed safely
#[test]
fn test_gc_83_nil_optional_field_freed_safely() {
    let (out, ok) = compile_and_run(
        r#"
class Outer {
    pub init(self, child: Option<Inner>, v: i64) {
        self.child = child;
        self.v = v;
    } child: Option<Inner>; v: i64; pub fn get(self) -> i64 { return self.v; } }
class Inner {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; }
fn make() -> i64 {
    let o = new Outer(None, 7);
    return o.get();
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-84: gc does not corrupt i64 return value from function
#[test]
fn test_gc_84_gc_does_not_corrupt_return_value() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn compute() -> i64 {
    let b = new Box(42);
    let result = b.get();
    gc_collect();
    return result;
}
fn main() {
    println(compute());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// GC-85: function allocating then calling gc_collect internally
#[test]
fn test_gc_85_gc_inside_allocating_function() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn alloc_and_get() -> i64 {
    let n = new Node(3);
    return n.get();
}
fn main() {
    let r1 = alloc_and_get();
    let r2 = alloc_and_get();
    gc_collect();
    println(r1 + r2);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n0\n");
}

// GC-86: zero-field class object allocated and freed
#[test]
fn test_gc_86_zero_field_class_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Empty {}
fn drop_it() -> i64 {
    let _e = new Empty();
    return 1;
}
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-87: zero-field class object alive survives collect
#[test]
fn test_gc_87_zero_field_class_survives() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Empty { pub fn tag(self) -> i64 { return 0; } }
fn main() {
    let e = new Empty();
    gc_collect();
    println(gc_allocated_bytes() > 0);
    println(e.tag());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n0\n");
}

// GC-88: child zero-field class extends parent with field — freed
#[test]
fn test_gc_88_child_inherits_field_both_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Empty extends Base {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn drop_it() -> i64 { let e = new Empty(9); return e.get(); }
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-89: object surviving ternary expression
#[test]
fn test_gc_89_object_survives_ternary() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let b = new Box(7);
    let x = b.get() > 5 ? 1 : 0;
    gc_collect();
    println(x);
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n7\n");
}

// GC-90: object with four fields — all freed when dead
#[test]
fn test_gc_90_four_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Quad {
    pub init(self, a: i64, b: i64, c: i64, d: i64) {
        self.a = a;
        self.b = b;
        self.c = c;
        self.d = d;
    } a: i64; b: i64; c: i64; d: i64;
    pub fn sum(self) -> i64 { return self.a + self.b + self.c + self.d; }
}
fn drop_quad() -> i64 {
    let q = new Quad(1, 2, 3, 4);
    return q.sum();
}
fn main() {
    let _ = drop_quad();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-91: object as function return — freed when caller doesn't store it
#[test]
fn test_gc_91_returned_object_not_stored_freed() {
    let (out, ok) = compile_and_run(
        r#"
class V {
    pub init(self, val: i64) {
        self.val = val;
    } val: i64; pub fn get(self) -> i64 { return self.val; } }
fn make_v() -> V { return new V(5); }
fn main() {
    let _ = make_v().get();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-92: object stored temporarily in variable then dropped
#[test]
fn test_gc_92_temp_stored_then_dropped() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn use_temp() -> i64 {
    let tmp = new Box(3);
    let tv = tmp.get();
    let result = tv * 2;
    return result;
}
fn main() {
    let r = use_temp();
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n0\n");
}

// GC-93: child and parent object both allocated, child freed first
#[test]
fn test_gc_93_parent_child_child_freed_parent_live() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn drop_child() -> i64 { let c = new Child(2); return c.get(); }
fn main() {
    let parent = new Base(1);
    let _ = drop_child();
    gc_collect();
    println(parent.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\ntrue\n");
}

// GC-94: allocate inside while body, free each iteration via scope
#[test]
fn test_gc_94_while_body_alloc_freed_each_iter() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let mut i = 0;
    let mut acc = 0;
    while i < 4 {
        let t = new Tmp(i * i);
        acc = acc + t.get();
        i = i + 1;
    }
    gc_collect();
    println(acc);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "14\n0\n");
}

// GC-95: object with both pub and prot fields freed
#[test]
fn test_gc_95_mixed_visibility_fields_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Mixed {
    pub init(self, a: i64, b: i64) {
        self.a = a;
        self.b = b;
    } pub a: i64; prot b: i64; pub fn sum(self) -> i64 { return self.a + self.b; } }
fn drop_mixed() -> i64 { let m = new Mixed(3, 4); return m.sum(); }
fn main() {
    let _ = drop_mixed();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-96: object alive when used as argument to function that gc_collects
#[test]
fn test_gc_96_alive_during_fn_that_collects() {
    let (out, ok) = compile_and_run(
        r#"
class Key {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn expensive(k: Key) -> i64 {
    gc_collect();
    return k.get();
}
fn main() {
    let k = new Key(13);
    println(expensive(k));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "13\n");
}

// GC-97: collect between two allocs — second survives
#[test]
fn test_gc_97_collect_between_allocs_second_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_one() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let _ = drop_one();
    gc_collect();
    let b2 = new Box(50);
    println(b2.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "50\n");
}

// GC-98: gc_collect idempotent — calling twice is safe
#[test]
fn test_gc_98_double_collect_idempotent() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_box() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let _ = drop_box();
    gc_collect();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-99: live object across gc_collect in a loop
#[test]
fn test_gc_99_live_across_collect_in_loop() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, n: i64) {
        self.n = n;
    } n: i64; pub fn get(self) -> i64 { return self.n; } }
fn main() {
    let c = new Counter(42);
    let mut i = 0;
    while i < 3 {
        gc_collect();
        i = i + 1;
    }
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// GC-100: gc_allocated_bytes tracks only live bytes after collect
#[test]
fn test_gc_100_bytes_tracks_only_live_after_collect() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_box() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let r = drop_box();
    println(r);
    gc_collect();
    let zero = gc_allocated_bytes();
    let live = new Box(2);
    let nonzero = gc_allocated_bytes();
    gc_collect();
    let still_nonzero = gc_allocated_bytes();
    println(zero);
    println(nonzero > 0);
    println(still_nonzero > 0);
    println(live.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n0\ntrue\ntrue\n2\n");
}
