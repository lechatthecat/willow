use super::*;

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
