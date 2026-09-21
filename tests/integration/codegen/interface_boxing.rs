use super::*;

#[test]
fn lir_diff_j22_widening_let_init() {
    // `let x: Named = new Item(..)` — the annotation widens, so the initializer
    // is boxed before it reaches the local's slot.
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
