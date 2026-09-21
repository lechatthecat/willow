use super::*;

#[test]
fn lir_diff_k24_dispatch_picks_the_concrete_implementation() {
    // One call site, two classes: the vtable in each box decides.
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
fn lir_callstack_interface_dispatch_panic_has_its_own_frame() {
    // Debug builds record a call-chain frame for the dispatched method, pushed
    // before the arguments are evaluated, which is the same order an ordinary
    // instance method uses.
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
    let (out, ok) = compile_with_env_and_run_combined(&source, &PLAIN);
    assert!(!ok, "the program must panic: {out}");
    let method = out
        .find("0: area")
        .unwrap_or_else(|| panic!("trace has no dispatched-method frame: {out}"));
    let caller = out
        .find("1: measure")
        .unwrap_or_else(|| panic!("trace has no caller frame: {out}"));
    assert!(method < caller, "trace is out of order: {out}");
}

#[test]
fn lir_callstack_interface_argument_panic_reports_the_argument_as_the_top_frame() {
    // A dispatched method installs its frame BEFORE its arguments are
    // evaluated, so an argument that panics does not replace that frame — it
    // stacks on top of it. The whole chain is therefore pinned: the argument,
    // then the method it was being passed to, then the caller.
    let source = dispatch_source(
        r#"
fn bad() -> i64 { return 1 / 0; }
fn measure(s: Shape) -> i64 { return s.scaled(bad()); }
fn main() { println(measure(new Square(3))); }
"#,
    );
    let (out, ok) = compile_with_env_and_run_combined(&source, &PLAIN);
    assert!(!ok, "the program must panic: {out}");
    let argument = out
        .find("0: bad")
        .unwrap_or_else(|| panic!("trace has no argument frame: {out}"));
    let method = out
        .find("1: scaled")
        .unwrap_or_else(|| panic!("trace has no dispatched-method frame: {out}"));
    let caller = out
        .find("2: measure")
        .unwrap_or_else(|| panic!("trace has no caller frame: {out}"));
    assert!(
        argument < method && method < caller,
        "trace is out of order: {out}"
    );
}

#[test]
fn lirreq_52_dispatch_example_is_fully_lir() {
    // Every free function in the dispatch example — `main` included — must be
    // claimed by the walker, which is what makes the example's own header claim
    // ("every function here is compiled from the lowered IR") a checked one.
    let source = include_str!("../../../example/lir_interface_dispatch.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        ok,
        "example/lir_interface_dispatch.wi must compile with every free function \
         on the LIR path: {stderr}"
    );
}
