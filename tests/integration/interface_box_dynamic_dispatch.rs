//! Boxing a value into an interface keeps the RUNTIME class's overrides
//! (willow-tygf).
//!
//! An interface value is `[object | vtable]`, and the vtable is the static
//! `(class, interface)` table chosen at the boxing site. Every slot of that
//! table was filled with `resolve_class_method_func_id(class, method)` — the
//! body THAT class would run — so widening an expression typed on a base class
//! called the base's method even when the object was really a subclass that
//! overrode it:
//!
//! ```willow
//! fn describe(m: Measured) -> i64 { return m.size() * 10; }
//! fn describe_parcel(p: Parcel) -> i64 { return describe(p); }
//! describe_parcel(new Sub(3))   // printed 30; `Sub::size` says 100
//! ```
//!
//! Silent and wrong rather than a crash, and not module-specific: one file
//! reproduces it. It hit every widening position (parameter, local, array
//! element, return value, branch) and left class-typed dispatch alone, since
//! that already goes through the receiver's class descriptor.
//!
//! An `open`/`override` method's vtable slot now holds a thunk that loads the
//! receiver's descriptor and calls its virtual slot — the same table, and the
//! same slot index, a class-typed call uses. A method with no virtual slot is
//! neither `open` nor an `override`, so nothing can replace it and its slot
//! keeps the direct address.
//!
//! 30 perspectives:
//!   01 a base-typed parameter reaches the subclass override
//!   02 control: the subclass as the static type at the box site
//!   03 a base-typed local
//!   04 a three-level chain where only the leaf overrides
//!   05 a three-level chain where the middle overrides and the leaf inherits
//!   06 an `open override` middle that the leaf overrides again
//!   07 the base object itself still reaches the base body
//!   08 two sibling subclasses through one base-typed parameter
//!   09 a base-typed RETURN value widened at the call site
//!   10 an array element
//!   11 a branch that reassigns a base-typed local
//!   12 a non-`open` interface method keeps its direct address
//!   13 control: an injected interface default's own self-call
//!   14 arguments forwarded in order through the thunk
//!   15 mixed argument types (i64, f64, bool, String)
//!   16 a String-returning method
//!   17 an f64-returning method
//!   18 a bool-returning method
//!   19 a void-returning method
//!   20 two interfaces on one class; the override wins through both
//!   21 a super-interface widening
//!   22 the same value boxed twice in one expression
//!   23 control: class-typed dispatch is unchanged
//!   24 an `open` body that calls another `open` method on `self`
//!   25 recursion through the box
//!   26 a module base with an entry subclass, widened inside the module
//!   27 a module chain widened through a base-typed local in the entry
//!   28 the repro under `--release`
//!   29 the repro under GC stress
//!   30 the runnable example

use super::support::{
    compile_and_run, compile_and_run_gc_stress, compile_and_run_release,
    compile_temp_project_and_run,
};

/// Compile and run a single-file program expected to succeed.
fn assert_output(source: &str, expected: &str) {
    let (output, ok) = compile_and_run(source);
    assert!(ok, "expected the program to compile:\n{output}");
    assert_eq!(output, expected);
}

/// Compile and run a multi-file project expected to succeed.
fn assert_project_output(files: &[(&str, &str)], expected: &str) {
    let (output, ok) = compile_temp_project_and_run(files, "app.wi");
    assert!(ok, "expected the project to compile:\n{output}");
    assert_eq!(output, expected);
}

/// The bug's shape: an `open` method, a subclass that overrides it, and a
/// function that widens to the interface.
const CHAIN: &str = "
interface Measured {
    fn size(self) -> i64;
}

open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

class Sub extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}
";

/// The bead's repro, reused by the release and GC-stress perspectives.
const REPRO: &str = "
interface Measured {
    fn size(self) -> i64;
}

open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

class Sub extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}

fn describe_parcel(p: Parcel) -> i64 {
    return describe(p);
}

fn main() {
    println(describe_parcel(new Sub(3)));
    println(describe(new Sub(3)));
}
";

// 1. The bead: the argument's static type is `Parcel`, its runtime class is
//    `Sub`. Printed 30 (`Parcel::size`) where `Sub::size` gives 100.
#[test]
fn box_dispatch_01_base_typed_parameter_reaches_the_override() {
    assert_output(
        &format!(
            "{CHAIN}
fn through_base(p: Parcel) -> i64 {{
    return describe(p);
}}

fn main() {{
    println(through_base(new Sub(3)));
    println(through_base(new Parcel(3)));
}}
"
        ),
        "100\n30\n",
    );
}

// 2. Control: with `Sub` as the static type the box already picked the
//    `(Sub, Measured)` table, so this line was right even before the fix. It
//    must stay right.
#[test]
fn box_dispatch_02_subclass_static_type_is_unchanged() {
    assert_output(
        &format!(
            "{CHAIN}
fn main() {{
    println(describe(new Sub(3)));
}}
"
        ),
        "100\n",
    );
}

// 3. A base-typed LOCAL is the same widening without a call boundary, so the
//    fix cannot be a property of argument passing.
#[test]
fn box_dispatch_03_base_typed_local() {
    assert_output(
        &format!(
            "{CHAIN}
fn main() {{
    let p: Parcel = new Sub(3);
    println(describe(p));
}}
"
        ),
        "100\n",
    );
}

/// Three levels, with the interface on the root: the box is built from `Root`
/// but the object is two levels below it.
const DEEP: &str = "
interface Measured {
    fn size(self) -> i64;
}

open class Root implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

open class Mid extends Root {}

class Leaf extends Mid {
    pub override fn size(self) -> i64 {
        return self.side * 5;
    }
}

fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}

fn through_root(r: Root) -> i64 {
    return describe(r);
}
";

// 4. The override is two levels below the class the box was built from, and
//    `Mid` contributes nothing — the slot it inherits must not pin the leaf.
#[test]
fn box_dispatch_04_three_level_chain_leaf_overrides() {
    assert_output(
        &format!(
            "{DEEP}
fn main() {{
    println(through_root(new Leaf(3)));
    println(through_root(new Mid(3)));
}}
"
        ),
        "150\n30\n",
    );
}

// 5. The mirror image: the MIDDLE class overrides and the leaf inherits that
//    override, so the leaf's slot holds `Mid::size` and the root's table must
//    still reach it.
#[test]
fn box_dispatch_05_three_level_chain_middle_overrides() {
    assert_output(
        "
interface Measured {
    fn size(self) -> i64;
}

open class Root implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

open class Mid extends Root {
    pub override fn size(self) -> i64 {
        return self.side * 2;
    }
}

class Leaf extends Mid {}

fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}

fn through_root(r: Root) -> i64 {
    return describe(r);
}

fn main() {
    println(through_root(new Leaf(3)));
}
",
        "60\n",
    );
}

// 6. `open override` in the middle: the leaf overrides an override. Each level
//    rewrites the SAME virtual slot, so the box built from the root still ends
//    at the deepest body.
#[test]
fn box_dispatch_06_open_override_middle_is_overridden_again() {
    assert_output(
        "
interface Measured {
    fn size(self) -> i64;
}

open class Root implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

open class Mid extends Root {
    pub open override fn size(self) -> i64 {
        return self.side * 3;
    }
}

class Leaf extends Mid {
    pub override fn size(self) -> i64 {
        return self.side * 10;
    }
}

fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}

fn through_root(r: Root) -> i64 {
    return describe(r);
}

fn main() {
    println(through_root(new Root(2)));
    println(through_root(new Mid(2)));
    println(through_root(new Leaf(2)));
}
",
        "20\n60\n200\n",
    );
}

// 7. The base object itself: the thunk must land back on the body the direct
//    address used to name, or the fix would have traded one wrong answer for
//    another.
#[test]
fn box_dispatch_07_the_base_object_still_reaches_the_base_body() {
    assert_output(
        &format!(
            "{CHAIN}
fn through_base(p: Parcel) -> i64 {{
    return describe(p);
}}

fn main() {{
    println(through_base(new Parcel(4)));
}}
"
        ),
        "40\n",
    );
}

// 8. Two siblings through ONE base-typed parameter: the answer cannot be a
//    property of the call site, since the same site gives three answers.
#[test]
fn box_dispatch_08_sibling_subclasses_through_one_parameter() {
    assert_output(
        &format!(
            "{CHAIN}
class Twice extends Parcel {{
    pub override fn size(self) -> i64 {{
        return self.side * 2;
    }}
}}

fn through_base(p: Parcel) -> i64 {{
    return describe(p);
}}

fn main() {{
    println(through_base(new Sub(4)));
    println(through_base(new Twice(4)));
    println(through_base(new Parcel(4)));
}}
"
        ),
        "110\n80\n40\n",
    );
}

// 9. A base-typed RETURN value: the widening happens on the result of a call
//    whose declared type hides the runtime class.
#[test]
fn box_dispatch_09_base_typed_return_value() {
    assert_output(
        &format!(
            "{CHAIN}
fn make(flag: bool) -> Parcel {{
    if flag {{
        return new Sub(3);
    }}
    return new Parcel(3);
}}

fn main() {{
    println(describe(make(true)));
    println(describe(make(false)));
}}
"
        ),
        "100\n30\n",
    );
}

// 10. An array element: `Array<Parcel>` erases the element's runtime class the
//     same way a parameter does, and each element boxes on its own.
#[test]
fn box_dispatch_10_array_element() {
    assert_output(
        &format!(
            "import std::collections::Array;
{CHAIN}
fn main() {{
    let items: Array<Parcel> = [new Sub(2), new Parcel(2)];
    let mut total = 0;
    let mut i = 0;
    while i < items.len() {{
        total = total + describe(items[i]);
        i = i + 1;
    }}
    println(total);
}}
"
        ),
        "110\n",
    );
}

// 11. A branch reassigning a base-typed local: the class the box is built from
//     is only known at runtime, which is exactly the case a static table
//     cannot answer.
#[test]
fn box_dispatch_11_branch_reassigns_a_base_typed_local() {
    assert_output(
        &format!(
            "{CHAIN}
fn pick(flag: bool) -> i64 {{
    let mut p: Parcel = new Parcel(3);
    if flag {{
        p = new Sub(3);
    }}
    return describe(p);
}}

fn main() {{
    println(pick(true));
    println(pick(false));
}}
"
        ),
        "100\n30\n",
    );
}

// 12. A method that is neither `open` nor an `override` has no virtual slot, so
//     its vtable entry keeps the direct address — no subclass can replace it,
//     and the thunk would be pure cost.
#[test]
fn box_dispatch_12_non_open_method_keeps_its_direct_address() {
    assert_output(
        "
interface Measured {
    fn size(self) -> i64;
    fn tag(self) -> i64;
}

open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub fn tag(self) -> i64 {
        return 99;
    }
}

class Sub extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn describe(m: Measured) -> i64 {
    return m.size() * 10 + m.tag();
}

fn through_base(p: Parcel) -> i64 {
    return describe(p);
}

fn main() {
    println(through_base(new Sub(3)));
    println(through_base(new Parcel(3)));
}
",
        "199\n129\n",
    );
}

// 13. Control: an injected interface DEFAULT is a plain class method (no slot
//     of its own), and the `self.size()` inside it is a CLASS-typed call, so
//     this half already reached the override before the fix. It pins the
//     boundary — the default body was never the broken part.
#[test]
fn box_dispatch_13_interface_default_through_a_base_typed_box() {
    assert_output(
        "
interface Measured {
    fn size(self) -> i64;
    fn doubled(self) -> i64 {
        return self.size() * 2;
    }
}

open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

class Sub extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn doubled_of(m: Measured) -> i64 {
    return m.doubled();
}

fn through_base(p: Parcel) -> i64 {
    return doubled_of(p);
}

fn main() {
    println(through_base(new Sub(3)));
    println(through_base(new Parcel(3)));
}
",
        "20\n6\n",
    );
}

// 14. Arguments must reach the override in order and unchanged: the thunk
//     forwards its own parameters, so a reversed or dropped one would show up
//     as an arithmetic answer, not a crash.
#[test]
fn box_dispatch_14_arguments_are_forwarded_in_order() {
    assert_output(
        "
interface Measured {
    fn scaled(self, k: i64, off: i64) -> i64;
}

open class Parcel implements Measured {
    pub side: i64;

    pub open fn scaled(self, k: i64, off: i64) -> i64 {
        return self.side * k + off;
    }
}

class Sub extends Parcel {
    pub override fn scaled(self, k: i64, off: i64) -> i64 {
        return self.side * k - off;
    }
}

fn apply(m: Measured) -> i64 {
    return m.scaled(10, 3);
}

fn through_base(p: Parcel) -> i64 {
    return apply(p);
}

fn main() {
    println(through_base(new Sub(4)));
    println(through_base(new Parcel(4)));
}
",
        "37\n43\n",
    );
}

// 15. Mixed argument types: the thunk's signature is copied from the method's
//     own declaration, so an integer, a float, a bool and a pointer all have to
//     survive the extra frame.
#[test]
fn box_dispatch_15_mixed_argument_types() {
    assert_output(
        r#"
interface Mixed {
    fn blend(self, k: i64, f: f64, flag: bool, name: String) -> String;
}

open class Base implements Mixed {
    pub tag: String;

    pub open fn blend(self, k: i64, f: f64, flag: bool, name: String) -> String {
        if flag {
            return self.tag + name + k.toString() + f.toString();
        }
        return self.tag;
    }
}

class Sub extends Base {
    pub override fn blend(self, k: i64, f: f64, flag: bool, name: String) -> String {
        if flag {
            return "sub:" + name + k.toString() + f.toString();
        }
        return "sub";
    }
}

fn call(m: Mixed) -> String {
    return m.blend(7, 1.5, true, "-x-");
}

fn through_base(b: Base) -> String {
    return call(b);
}

fn main() {
    println(through_base(new Sub("base:")));
    println(through_base(new Base("base:")));
}
"#,
        "sub:-x-71.5\nbase:-x-71.5\n",
    );
}

/// Return kinds other than `i64`: the thunk returns whatever the method does,
/// so each width has to come back through the extra frame intact.
const RETURNS: &str = r#"
interface Weighed {
    fn mass(self) -> f64;
    fn heavy(self) -> bool;
    fn report(self);
}

open class Crate implements Weighed {
    pub kg: f64;

    pub open fn mass(self) -> f64 {
        return self.kg;
    }

    pub open fn heavy(self) -> bool {
        return self.kg > 10.0;
    }

    pub open fn report(self) {
        println("crate");
    }
}

class Steel extends Crate {
    pub override fn mass(self) -> f64 {
        return self.kg * 3.0;
    }

    pub override fn heavy(self) -> bool {
        return true;
    }

    pub override fn report(self) {
        println("steel");
    }
}
"#;

// 16. A String (pointer) return through the thunk, in a hierarchy where the
//     override returns a different string.
#[test]
fn box_dispatch_16_string_return() {
    assert_output(
        r#"
interface Named {
    fn name(self) -> String;
}

open class Parcel implements Named {
    pub side: i64;

    pub open fn name(self) -> String {
        return "parcel";
    }
}

class Sub extends Parcel {
    pub override fn name(self) -> String {
        return "sub";
    }
}

fn name_of(n: Named) -> String {
    return n.name();
}

fn through_base(p: Parcel) -> String {
    return name_of(p);
}

fn main() {
    println(through_base(new Sub(1)));
    println(through_base(new Parcel(1)));
}
"#,
        "sub\nparcel\n",
    );
}

// 17. An f64 return comes back in a float register, not the integer one the
//     receiver arrived in.
#[test]
fn box_dispatch_17_float_return() {
    assert_output(
        &format!(
            "{RETURNS}
fn mass_of(w: Weighed) -> f64 {{
    return w.mass();
}}

fn main() {{
    let steel: Crate = new Steel(4.0);
    println(mass_of(steel));
    println(mass_of(new Crate(4.0)));
}}
"
        ),
        "12\n4\n",
    );
}

// 18. A bool return.
#[test]
fn box_dispatch_18_bool_return() {
    assert_output(
        &format!(
            "{RETURNS}
fn heavy_of(w: Weighed) -> bool {{
    return w.heavy();
}}

fn main() {{
    let steel: Crate = new Steel(4.0);
    println(heavy_of(steel));
    println(heavy_of(new Crate(4.0)));
}}
"
        ),
        "true\nfalse\n",
    );
}

// 19. A void return: the thunk has no result to forward at all, so an
//     unconditional `return_` of the call's results would be a verifier error
//     if the arity were assumed.
#[test]
fn box_dispatch_19_void_return() {
    assert_output(
        &format!(
            "{RETURNS}
fn report_of(w: Weighed) {{
    w.report();
}}

fn main() {{
    let steel: Crate = new Steel(4.0);
    report_of(steel);
    report_of(new Crate(4.0));
}}
"
        ),
        "steel\ncrate\n",
    );
}

// 20. One class implementing TWO interfaces gets two vtables that name the same
//     method; both have to reach the override, and one thunk is shared by both.
#[test]
fn box_dispatch_20_two_interfaces_share_one_thunk() {
    assert_output(
        "
interface Measured {
    fn size(self) -> i64;
}

interface Tagged {
    fn size(self) -> i64;
    fn tag(self) -> i64;
}

open class Parcel implements Measured, Tagged {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub open fn tag(self) -> i64 {
        return 1;
    }
}

class Sub extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }

    pub override fn tag(self) -> i64 {
        return 2;
    }
}

fn by_measured(m: Measured) -> i64 {
    return m.size();
}

fn by_tagged(t: Tagged) -> i64 {
    return t.size() * 100 + t.tag();
}

fn through_base(p: Parcel) {
    println(by_measured(p));
    println(by_tagged(p));
}

fn main() {
    through_base(new Sub(3));
    through_base(new Parcel(3));
}
",
        "10\n1002\n3\n301\n",
    );
}

// 21. A super-interface's table is an EMBEDDED region of the sub-interface's,
//     reached by pointer arithmetic on a widening. Its slots carry thunks too,
//     so widening `Measured` to `Named` must not lose the override.
#[test]
fn box_dispatch_21_super_interface_widening() {
    assert_output(
        r#"
interface Named {
    fn name(self) -> String;
}

interface Measured extends Named {
    fn size(self) -> i64;
}

open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub open fn name(self) -> String {
        return "parcel";
    }
}

class Sub extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }

    pub override fn name(self) -> String {
        return "sub";
    }
}

fn name_of(n: Named) -> String {
    return n.name();
}

fn widen(m: Measured) -> String {
    return name_of(m);
}

fn through_base(p: Parcel) {
    println(name_of(p));
    println(widen(p));
}

fn main() {
    through_base(new Sub(3));
    through_base(new Parcel(3));
}
"#,
        "sub\nsub\nparcel\nparcel\n",
    );
}

// 22. The same value boxed twice in one expression: two boxes, one object, and
//     both must dispatch dynamically rather than the first result being cached
//     into the second.
#[test]
fn box_dispatch_22_the_same_value_boxed_twice() {
    assert_output(
        &format!(
            "{CHAIN}
fn through_base(p: Parcel) -> i64 {{
    return describe(p) + describe(p);
}}

fn main() {{
    println(through_base(new Sub(3)));
}}
"
        ),
        "200\n",
    );
}

// 23. Control: a class-typed call already went through the descriptor and was
//     never affected. It is the answer the boxed call now has to match.
#[test]
fn box_dispatch_23_class_typed_dispatch_is_unchanged() {
    assert_output(
        &format!(
            "{CHAIN}
fn direct(p: Parcel) -> i64 {{
    return p.size();
}}

fn main() {{
    println(direct(new Sub(3)));
    println(direct(new Parcel(3)));
}}
"
        ),
        "10\n3\n",
    );
}

// 24. An `open` body calling another `open` method on `self`: the thunk enters
//     the override, whose own `self.size()` must keep dispatching virtually
//     rather than returning to the base.
#[test]
fn box_dispatch_24_open_body_calls_another_open_method() {
    assert_output(
        "
interface Measured {
    fn size(self) -> i64;
}

open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub open fn label(self) -> i64 {
        return self.size() + 1;
    }
}

class Sub extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }

    pub override fn label(self) -> i64 {
        return self.size() + 2;
    }
}

fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}

fn through_base(p: Parcel) -> i64 {
    return describe(p) + p.label();
}

fn main() {
    println(through_base(new Sub(3)));
    println(through_base(new Parcel(3)));
}
",
        "112\n34\n",
    );
}

// 25. Recursion through the box: each level re-enters the thunk, so a thunk
//     that clobbered the receiver or leaked a frame would diverge here rather
//     than return an answer.
#[test]
fn box_dispatch_25_recursion_through_the_box() {
    assert_output(
        "
interface Measured {
    fn size(self) -> i64;
}

open class Node implements Measured {
    pub n: i64;

    pub open fn size(self) -> i64 {
        if self.n <= 0 {
            return 0;
        }
        let smaller: Node = new Node(self.n - 1);
        return 1 + describe(smaller) / 10;
    }
}

class Doubling extends Node {
    pub override fn size(self) -> i64 {
        if self.n <= 0 {
            return 0;
        }
        let smaller: Node = new Doubling(self.n - 1);
        return 2 + describe(smaller) / 10;
    }
}

fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}

fn main() {
    let d: Node = new Doubling(3);
    println(describe(d));
    let p: Node = new Node(3);
    println(describe(p));
}
",
        "60\n30\n",
    );
}

/// A module that owns the interface, the base, and a subclass of its own, plus
/// the function that does the widening.
const DEPOT: &str = "
pub interface Measured {
    fn size(self) -> i64;
}

pub open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

pub open class Crate extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side * 3;
    }
}

pub fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}

pub fn through_base(p: Parcel) -> i64 {
    return describe(p);
}
";

// 26. The widening happens INSIDE the module, on a class the module has never
//     seen: the entry's subclass. The module's own vtables are declared before
//     that class exists, so the thunk is the only thing that can reach it.
#[test]
fn box_dispatch_26_module_widens_an_entry_subclass() {
    assert_project_output(
        &[
            ("depot.wi", DEPOT),
            (
                "app.wi",
                "
import depot;
import depot::Parcel;

class EntrySub extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(depot::through_base(new EntrySub(3)));
    println(depot::through_base(new Parcel(3)));
}
",
            ),
        ],
        "100\n30\n",
    );
}

// 27. Both classes live in the module and the widening happens in the ENTRY,
//     through a base-typed local — the cross-unit mirror of perspective 3.
#[test]
fn box_dispatch_27_entry_widens_a_module_chain() {
    assert_project_output(
        &[
            ("depot.wi", DEPOT),
            (
                "app.wi",
                "
import depot;
import depot::Parcel;
import depot::Crate;

fn main() {
    println(depot::through_base(new Crate(3)));
    let p: Parcel = new Crate(2);
    println(depot::describe(p));
}
",
            ),
        ],
        "90\n60\n",
    );
}

// 28. `--release` carries none of the debug instrumentation and optimizes the
//     extra frame; the answer must not depend on the build mode.
#[test]
fn box_dispatch_28_release_build() {
    let (output, ok) = compile_and_run_release(REPRO);
    assert!(ok, "expected the program to compile:\n{output}");
    assert_eq!(output, "100\n100\n");
}

// 29. Under GC stress the box allocation collects, so the receiver the thunk
//     loads a descriptor from has been through the collector. Word 0 must still
//     be the class descriptor.
#[test]
fn box_dispatch_29_gc_stress() {
    let (output, ok) = compile_and_run_gc_stress(REPRO);
    assert!(ok, "expected the program to compile:\n{output}");
    assert_eq!(output, "100\n100\n");
}

// 30. The shipped example: every widening position in one program, with the
//     class-typed call printed beside the boxed one.
#[test]
fn box_dispatch_30_the_runnable_example() {
    let source = std::fs::read_to_string("example/interface_box_dynamic_dispatch.wi").unwrap();
    let (output, ok) = compile_and_run(&source);
    assert!(ok, "expected the example to compile:\n{output}");
    assert_eq!(output, "20\n60\n200\n90\n140\n18\n9\n");
}
