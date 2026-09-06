//! An ENTRY-file subclass of a module class is usable as the interfaces that
//! module class implements (willow-himv).
//!
//! `implements` is propagated down an `extends` chain by
//! `resolve_interface_inheritance` (willow-2s4i), and that pass built its class
//! map out of `program.items` alone. Every ancestor that lived in an imported
//! module was therefore invisible: `class EntryParcel extends lib::Parcel`
//! inherited nothing, no `(EntryParcel, lib::Measured)` vtable was ever
//! declared, and the value the entry file handed to a module function typed on
//! the interface was a box with no methods in it. The AST emitter fell back to
//! the raw object and the program silently stopped mid-`main` with exit code 0;
//! since the LIR walker became mandatory the same program is rejected outright
//! with `error[E0800] ... the static call \`lib::describe\` ... is outside the
//! walker's subset`, because eligibility asks `can_box(class, iface)` and the
//! vtable is missing. Both spellings of the failure are one missing vtable.
//!
//! The fix hands that pass the module class shapes the default-method pass
//! already builds, so an ancestor in a module contributes its `implements` the
//! same way a local one does.
//!
//! 24 perspectives:
//!   1 the bead's repro: an entry subclass reaches a module interface function
//!   2 control: the module base itself still boxes
//!   3 a subclass that overrides nothing inherits the base body through the box
//!   4 a two-level entry chain reaches the leaf override
//!   5 an aliased import spells the base differently
//!   6 a qualified base path (`extends lib::Parcel`) works too
//!   7 two entry subclasses each get their own vtable
//!   8 an interface method returning a String
//!   9 a super-interface accepts the entry subclass
//!  10 the sub-interface accepts it in the same program
//!  11 an inherited default method is not injected twice
//!  12 the subclass adds an interface of its own alongside the inherited one
//!  13 a generic interface keeps its type argument
//!  14 two interfaces on one module base, both usable
//!  15 an array of the interface holds entry subclasses
//!  16 an entry function returns the module interface
//!  17 a module function dispatches in a loop
//!  18 a branch picks base or subclass and both box
//!  19 an interface-typed local in the entry file
//!  20 a two-level module chain then an entry subclass
//!  21 control: a module subclass of a module base still boxes
//!  22 two modules each contribute a base class and an interface
//!  23 control: an entry class implementing the module interface directly
//!  24 the runnable example program

use super::support::compile_temp_project_and_run;

/// Compile and run a project expected to succeed, asserting its stdout.
fn assert_output(files: &[(&str, &str)], expected: &str) {
    let (output, ok) = compile_temp_project_and_run(files, "app.wi");
    assert!(ok, "expected the project to compile:\n{output}");
    assert_eq!(output, expected);
}

/// The bead's module: one interface, one `open` class implementing it, and a
/// function typed on the interface — the boxing site that needs the vtable.
const LIB: &str = r#"
module lib;

pub interface Measured {
    fn size(self) -> i64;
}

pub open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

pub fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}
"#;

#[test]
fn mi_01_entry_subclass_boxes_into_the_module_interface() {
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(lib::describe(new Parcel(3)));
    println(lib::describe(new EntryParcel(3)));
}
"#,
            ),
        ],
        "30\n100\n",
    );
}

#[test]
fn mi_02_the_module_base_itself_still_boxes() {
    // Control: the pair that always had a vtable, with no subclass in the file
    // at all, so a regression here would mean the fix broke the ordinary case.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;

fn main() {
    println(lib::describe(new Parcel(5)));
}
"#,
            ),
        ],
        "50\n",
    );
}

#[test]
fn mi_03_a_subclass_that_overrides_nothing_inherits_through_the_box() {
    // The vtable slot is filled by walking to the ancestor that declares the
    // method, so a subclass adding nothing still dispatches to `Parcel::size`.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;

class Plain extends Parcel {}

fn main() {
    println(lib::describe(new Plain(4)));
}
"#,
            ),
        ],
        "40\n",
    );
}

#[test]
fn mi_04_a_two_level_entry_chain_reaches_the_leaf_override() {
    // `Leaf`'s base is a local class whose OWN base is the module class, so the
    // inherited interface has to survive a walk that leaves the entry file and
    // comes back with a module-qualified name.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;

open class Mid extends Parcel {}

class Leaf extends Mid {
    pub override fn size(self) -> i64 {
        return self.side + 1;
    }
}

fn main() {
    println(lib::describe(new Mid(2)));
    println(lib::describe(new Leaf(4)));
}
"#,
            ),
        ],
        "20\n50\n",
    );
}

#[test]
fn mi_05_an_aliased_import_spells_the_base_differently() {
    // `extends P` names an import alias; the class shapes are keyed by the
    // qualified name, so the alias spelling has to be bound too.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel as P;

class Boxy extends P {
    pub override fn size(self) -> i64 {
        return self.side * 2;
    }
}

fn main() {
    println(lib::describe(new Boxy(4)));
}
"#,
            ),
        ],
        "80\n",
    );
}

#[test]
fn mi_06_a_qualified_base_path_works_too() {
    // No direct import of the class at all: `extends lib::Parcel` is the
    // already-qualified spelling, which reaches the shape index by its key.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib;

class Crate extends lib::Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 6;
    }
}

fn main() {
    println(lib::describe(new Crate(4)));
}
"#,
            ),
        ],
        "100\n",
    );
}

#[test]
fn mi_07_two_entry_subclasses_each_get_their_own_vtable() {
    // One vtable per (class, interface) pair: the two subclasses must not share
    // one, or the second boxing would dispatch into the first's override.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;

class Plus extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 1;
    }
}

class Times extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side * 3;
    }
}

fn main() {
    println(lib::describe(new Plus(4)));
    println(lib::describe(new Times(4)));
}
"#,
            ),
        ],
        "50\n120\n",
    );
}

#[test]
fn mi_08_an_interface_method_returning_a_string() {
    // The slot's signature is the interface's, not `i64`-shaped by accident.
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub interface Named {
    fn tag(self) -> String;
}

pub open class Thing implements Named {
    pub id: i64;

    pub open fn tag(self) -> String {
        return "thing";
    }
}

pub fn name(n: Named) -> String {
    return n.tag();
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib;
import lib::Thing;

class Widget extends Thing {
    pub override fn tag(self) -> String {
        return "widget";
    }
}

fn main() {
    println(lib::name(new Thing(1)));
    println(lib::name(new Widget(2)));
}
"#,
            ),
        ],
        "thing\nwidget\n",
    );
}

/// A module whose interface inherits another: `Measured extends Named`, so a
/// `Parcel` is usable as either and the sub-interface's vtable embeds the
/// super's region.
const LIB_SUPER: &str = r#"
module lib;

pub interface Named {
    fn tag(self) -> String;
}

pub interface Measured extends Named {
    fn size(self) -> i64;
}

pub open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub open fn tag(self) -> String {
        return "parcel";
    }
}

pub fn name(n: Named) -> String {
    return n.tag();
}

pub fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}
"#;

#[test]
fn mi_09_a_super_interface_accepts_the_entry_subclass() {
    // The propagated `implements lib::Measured` also has to bring in the super
    // interfaces, which is the step that runs right after it in the same pass.
    assert_output(
        &[
            ("lib.wi", LIB_SUPER),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn tag(self) -> String {
        return "entry";
    }
}

fn main() {
    println(lib::name(new Parcel(3)));
    println(lib::name(new EntryParcel(3)));
}
"#,
            ),
        ],
        "parcel\nentry\n",
    );
}

#[test]
fn mi_10_the_sub_interface_accepts_it_in_the_same_program() {
    // Same class, both interfaces used in one program: the two vtables are
    // distinct symbols and both have to exist for this to link.
    assert_output(
        &[
            ("lib.wi", LIB_SUPER),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }

    pub override fn tag(self) -> String {
        return "entry";
    }
}

fn main() {
    println(lib::describe(new EntryParcel(3)));
    println(lib::name(new EntryParcel(3)));
}
"#,
            ),
        ],
        "100\nentry\n",
    );
}

/// A module interface carrying a DEFAULT body, so default-method injection has
/// an opinion about which class receives a copy.
const LIB_DEFAULT: &str = r#"
module lib;

pub interface Measured {
    fn size(self) -> i64;

    fn doubled(self) -> i64 {
        return self.size() * 2;
    }
}

pub open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

pub fn twice(m: Measured) -> i64 {
    return m.doubled() + 1;
}
"#;

#[test]
fn mi_11_an_inherited_default_method_is_not_injected_twice() {
    // The propagated interface TYPE is the very one the ancestor's shape lists,
    // so `ancestor_provides` matches it and the subclass inherits the single
    // injected copy instead of receiving a second one (which has no virtual
    // slot to disambiguate it and is rejected by the backend).
    assert_output(
        &[
            ("lib.wi", LIB_DEFAULT),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(lib::twice(new Parcel(3)));
    println(lib::twice(new EntryParcel(3)));
}
"#,
            ),
        ],
        "7\n21\n",
    );
}

#[test]
fn mi_12_the_subclass_adds_an_interface_of_its_own() {
    // The inherited interface is appended to a list the class also writes into
    // itself, so the two must coexist: `EntryParcel` boxes into the module's
    // `Measured` AND into an interface declared right here.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;

interface Tagged {
    fn tag(self) -> String;
}

class EntryParcel extends Parcel implements Tagged {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }

    pub fn tag(self) -> String {
        return "entry";
    }
}

fn label(t: Tagged) -> String {
    return t.tag();
}

fn main() {
    println(lib::describe(new EntryParcel(3)));
    println(label(new EntryParcel(3)));
}
"#,
            ),
        ],
        "100\nentry\n",
    );
}

#[test]
fn mi_13_a_generic_interface_keeps_its_type_argument() {
    // Propagation copies the interface TYPE, arguments included: inheriting
    // `Holder<i64>` as a bare `Holder` would neither type-check nor name the
    // vtable the boxing site looks for.
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub interface Holder<T> {
    fn get(self) -> T;
}

pub open class Cell implements Holder<i64> {
    pub value: i64;

    pub open fn get(self) -> i64 {
        return self.value;
    }
}

pub fn read(h: Holder<i64>) -> i64 {
    return h.get();
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib;
import lib::Cell;

class Doubler extends Cell {
    pub override fn get(self) -> i64 {
        return self.value * 2;
    }
}

fn main() {
    println(lib::read(new Cell(21)));
    println(lib::read(new Doubler(21)));
}
"#,
            ),
        ],
        "21\n42\n",
    );
}

#[test]
fn mi_14_two_interfaces_on_one_module_base_are_both_usable() {
    // Two independent interfaces (not a super/sub pair): the subclass needs one
    // vtable for each, and each has its own slot order.
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub interface Measured {
    fn size(self) -> i64;
}

pub interface Named {
    fn tag(self) -> String;
}

pub open class Parcel implements Measured, Named {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub open fn tag(self) -> String {
        return "parcel";
    }
}

pub fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}

pub fn name(n: Named) -> String {
    return n.tag();
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }

    pub override fn tag(self) -> String {
        return "entry";
    }
}

fn main() {
    println(lib::describe(new EntryParcel(3)));
    println(lib::name(new EntryParcel(3)));
}
"#,
            ),
        ],
        "100\nentry\n",
    );
}

#[test]
fn mi_15_an_array_of_the_interface_holds_entry_subclasses() {
    // Boxing happens at the element store, not only at a call argument.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import std::collections::Array;

import lib;
import lib::Parcel;
import lib::Measured;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    let items: Array<Measured> = [new Parcel(1), new EntryParcel(1)];
    let mut total = 0;
    let mut i = 0;
    while i < items.len() {
        total = total + items[i].size();
        i = i + 1;
    }
    println(total);
}
"#,
            ),
        ],
        "9\n",
    );
}

#[test]
fn mi_16_an_entry_function_returns_the_module_interface() {
    // A `return` is a widening position of its own: the coercion happens
    // against the declared return type, spelled here with the module
    // qualification the propagated `implements` carries.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn wrap(p: EntryParcel) -> lib::Measured {
    return p;
}

fn main() {
    println(wrap(new EntryParcel(3)).size());
    println(lib::describe(wrap(new EntryParcel(5))));
}
"#,
            ),
        ],
        "10\n120\n",
    );
}

#[test]
fn mi_17_a_module_function_dispatches_in_a_loop() {
    // The vtable is loaded once per call; a loop makes a wrong or absent one
    // show up as a repeated wrong answer rather than a single odd number.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    let mut total = 0;
    let mut i = 0;
    while i < 3 {
        total = total + lib::describe(new EntryParcel(i));
        i = i + 1;
    }
    println(total);
}
"#,
            ),
        ],
        "240\n",
    );
}

#[test]
fn mi_18_a_branch_picks_base_or_subclass_and_both_box() {
    // Two boxings of DIFFERENT classes reaching one call site: each arm has to
    // find its own vtable.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;
import lib::Measured;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn pick(entry: bool) -> i64 {
    if entry {
        return lib::describe(new EntryParcel(3));
    } else {
        return lib::describe(new Parcel(3));
    }
}

fn main() {
    println(pick(true));
    println(pick(false));
}
"#,
            ),
        ],
        "100\n30\n",
    );
}

#[test]
fn mi_19_an_interface_typed_local_in_the_entry_file() {
    // The entry file's own function typed on the imported interface: dispatch
    // does not have to cross the module boundary for the vtable to matter.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib::Parcel;
import lib::Measured;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    let m: Measured = new EntryParcel(3);
    println(m.size());
}
"#,
            ),
        ],
        "10\n",
    );
}

/// A module with its own two-level hierarchy, so the entry subclass sits three
/// deep and the interface is named only by the root.
const LIB_CHAIN: &str = r#"
module lib;

pub interface Measured {
    fn size(self) -> i64;
}

pub open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

pub open class Crate extends Parcel {}

pub fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}
"#;

#[test]
fn mi_20_a_two_level_module_chain_then_an_entry_subclass() {
    // `Big -> lib::Crate -> lib::Parcel`: the walk has to keep going after the
    // first module ancestor, which lists no interface of its own.
    assert_output(
        &[
            ("lib.wi", LIB_CHAIN),
            (
                "app.wi",
                r#"
import lib;
import lib::Crate;

class Big extends Crate {
    pub override fn size(self) -> i64 {
        return self.side * 3;
    }
}

fn main() {
    println(lib::describe(new Big(4)));
}
"#,
            ),
        ],
        "120\n",
    );
}

#[test]
fn mi_21_a_module_subclass_of_a_module_base_still_boxes() {
    // Control for the perspective above: the module-internal half of the same
    // propagation, which already worked because both classes are in one program.
    assert_output(
        &[
            ("lib.wi", LIB_CHAIN),
            (
                "app.wi",
                r#"
import lib;
import lib::Crate;

fn main() {
    println(lib::describe(new Crate(4)));
}
"#,
            ),
        ],
        "40\n",
    );
}

#[test]
fn mi_22_two_modules_each_contribute_a_base_and_an_interface() {
    // The shapes of every imported module go into one map: a program that
    // subclasses out of two different modules has to inherit each ancestor's
    // own interface, not whichever module was indexed last.
    assert_output(
        &[
            (
                "one.wi",
                r#"
module one;

pub interface Measured {
    fn size(self) -> i64;
}

pub open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

pub fn describe(m: Measured) -> i64 {
    return m.size() * 10;
}
"#,
            ),
            (
                "two.wi",
                r#"
module two;

pub interface Named {
    fn tag(self) -> String;
}

pub open class Thing implements Named {
    pub id: i64;

    pub open fn tag(self) -> String {
        return "thing";
    }
}

pub fn name(n: Named) -> String {
    return n.tag();
}
"#,
            ),
            (
                "app.wi",
                r#"
import one;
import one::Parcel;
import two;
import two::Thing;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

class Widget extends Thing {
    pub override fn tag(self) -> String {
        return "widget";
    }
}

fn main() {
    println(one::describe(new EntryParcel(3)));
    println(two::name(new Widget(1)));
}
"#,
            ),
        ],
        "100\nwidget\n",
    );
}

#[test]
fn mi_23_an_entry_class_implementing_the_module_interface_directly() {
    // Control: a written-out `implements lib::Measured` never depended on the
    // propagation and must keep working beside a subclass that does.
    assert_output(
        &[
            ("lib.wi", LIB),
            (
                "app.wi",
                r#"
import lib;
import lib::Parcel;
import lib::Measured;

class Direct implements Measured {
    pub n: i64;

    pub fn size(self) -> i64 {
        return self.n;
    }
}

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(lib::describe(new Direct(2)));
    println(lib::describe(new EntryParcel(3)));
}
"#,
            ),
        ],
        "20\n100\n",
    );
}

#[test]
fn mi_24_the_runnable_example_program() {
    // The shipped example is the end-to-end shape: entry subclasses of a module
    // class boxed into that module's interfaces from both sides of the boundary.
    let (output, ok) = compile_temp_project_and_run(
        &[
            (
                "depot.wi",
                &std::fs::read_to_string("example/module_inherited_interfaces/depot.wi").unwrap(),
            ),
            (
                "app.wi",
                &std::fs::read_to_string("example/module_inherited_interfaces/main.wi").unwrap(),
            ),
        ],
        "app.wi",
    );
    assert!(ok, "expected the example to compile:\n{output}");
    assert_eq!(output, "30\n100\n40\nparcel\nentry\n7\n21\n120\n");
}
