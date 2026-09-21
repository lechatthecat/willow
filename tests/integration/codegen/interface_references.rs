use super::*;

// 19. The whole point: a `&mut` argument dispatched through a vtable must
// write back into the CALLER's local, not into a copy — and must not
// dereference the value 10 as an address.
#[test]
fn refmode_19_mut_reference_through_dispatch_mutates_caller_local() {
    assert_refmode_output(
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
    assert_refmode_output(
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
    assert_refmode_output(
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

// 22. The same under GC stress: the receiver box is only reachable through the
// interface value while the callee allocates.
#[test]
fn refmode_22_string_place_under_gc_stress() {
    assert_refmode_output_under_gc_stress(
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
    assert_refmode_output(
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
    assert_refmode_output(
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
    assert_refmode_output(
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
    assert_refmode_output(
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
    assert_refmode_output(
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
    assert_refmode_output(
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
    assert_refmode_output(
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

// 30. A reference call through an interface, in a debug build, where the
// reference diagnostic hook and its post-call clear are emitted. The walker
// compiles it (willow-0g8j.2.17), and the by-value sibling stays independently
// eligible.
#[test]
fn refmode_30_reference_call_is_walker_owned() {
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
    assert_program_output(&source, "15\n50\n");
}
