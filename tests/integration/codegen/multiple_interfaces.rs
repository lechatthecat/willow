use super::*;

// ── one object, two interfaces (willow-j260.1) ─────────────────────────────

#[test]
fn lir_diff_j37_one_object_boxed_into_two_interfaces_in_one_new() {
    // `new TwoWay(m, m)` boxes the SAME `Meter` twice inside ONE construction,
    // once as a `Ticker` and once as a `Counted`. The two boxes must carry
    // DIFFERENT vtables: slot 0 of `Ticker` is `tick`, slot 0 of `Counted` is
    // `count`, so a vtable resolved per-class instead of per-(class,interface)
    // would send one of the two calls into the wrong function.
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    assert_program_output(
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
    // the walker's subset, so writing it inline would not compile.)
    assert_program_output(
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
    assert_program_output(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
    assert_output_under_gc_stress(
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
    let source = include_str!("../../../example/lir_interface_boxing.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        ok,
        "example/lir_interface_boxing.wi must compile with every free function \
         on the LIR path: {stderr}"
    );
}
