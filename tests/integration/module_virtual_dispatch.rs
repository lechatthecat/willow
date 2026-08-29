//! A module body dispatches virtually against the WHOLE program (willow-4zt8).
//!
//! `plan_virtual_call` devirtualizes a call whose receiver has exactly one
//! implementation in the class hierarchy — and the hierarchy it consulted was
//! whatever `Codegen` had been told about so far. The driver compiled each
//! imported module completely (declare + lower bodies) before it even started
//! on the entry program, so while a module body was being lowered the entry
//! file's subclasses did not exist yet. A module method calling one of its own
//! `open` methods therefore saw a single candidate, bound the base body
//! directly, and quietly ignored the override the entry file went on to
//! declare. The call that the user could see was virtual — `new Sub(3).size()`
//! from the entry file — dispatched correctly, which is what made this look
//! like an inheritance bug rather than an ordering one.
//!
//! The fix splits codegen into a declaration sweep and a body sweep: every unit
//! (each module AND the entry program) is declared before any body is lowered,
//! so the candidate set is computed against the complete class hierarchy.
//!
//! 21 perspectives:
//!   1 the base case: a module method sees an entry-file override
//!   2 control: with no override the module still runs its own body
//!   3 a module FREE function dispatches to an entry override
//!   4 a three-level chain rooted in a module reaches the entry leaf
//!   5 two entry subclasses of one module base each get their own override
//!   6 the override is reached on every call in one module expression
//!   7 dispatch survives a module method calling another module method
//!   8 an `open` method taking parameters dispatches to the override
//!   9 the override may read a field the entry subclass added
//!  10 a module function typed on the base accepts and dispatches a subclass
//!  11 the override is reached from inside a loop in the module
//!  12 the override is reached from a branch in the module
//!  13 a module overriding another module's class is reached cross-module
//!  14 module base, module override and entry override: the deepest wins
//!  15 a module interface default body reaches an entry class's method
//!  17 control: a non-`open` module method is unaffected
//!  18 a subclass that overrides nothing still runs the module's body
//!  19 dispatch works for a method returning a string
//!  20 an override calling a second `open` method dispatches both
//!  21 the override is reached through a base-typed local in the module
//!  22 a four-level chain spanning two modules and the entry file

use super::support::compile_temp_project_and_run;

/// Compile and run a project expected to succeed, asserting its stdout.
fn assert_output(files: &[(&str, &str)], expected: &str) {
    let (output, ok) = compile_temp_project_and_run(files, "app.wi");
    assert!(ok, "expected the project to compile:\n{output}");
    assert_eq!(output, expected);
}

/// The module of the bead's repro: `label` is an ordinary method whose body
/// calls the `open` `size`, so lowering `label` is what has to stay virtual.
const PARCEL: &str = r#"
module lib;

pub open class Parcel {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub fn label(self) -> i64 {
        return self.size() * 10;
    }
}
"#;

#[test]
fn module_dispatch_01_entry_override_is_reached_from_a_module_method() {
    assert_output(
        &[
            ("lib.wi", PARCEL),
            (
                "app.wi",
                r#"
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(new EntryParcel(3).label());
}
"#,
            ),
        ],
        "100\n",
    );
}

#[test]
fn module_dispatch_02_without_an_override_the_module_body_still_runs() {
    assert_output(
        &[
            ("lib.wi", PARCEL),
            (
                "app.wi",
                r#"
import lib::Parcel;

fn main() {
    println(new Parcel(3).label());
}
"#,
            ),
        ],
        "30\n",
    );
}

#[test]
fn module_dispatch_03_module_free_function_reaches_the_override() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Parcel {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

pub fn label_of(p: Parcel) -> i64 {
    return p.size() * 10;
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Parcel;
import lib::label_of;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(label_of(new EntryParcel(3)));
}
"#,
            ),
        ],
        "100\n",
    );
}

#[test]
fn module_dispatch_04_three_level_chain_reaches_the_entry_leaf() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Base {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub fn label(self) -> i64 {
        return self.size() * 10;
    }
}

pub open class Mid extends Base {
    pub open override fn size(self) -> i64 {
        return self.side + 1;
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Base;
import lib::Mid;

class Leaf extends Mid {
    pub override fn size(self) -> i64 {
        return self.side + 100;
    }
}

fn main() {
    println(new Base(3).label());
    println(new Mid(3).label());
    println(new Leaf(3).label());
}
"#,
            ),
        ],
        "30\n40\n1030\n",
    );
}

#[test]
fn module_dispatch_05_two_entry_subclasses_each_get_their_override() {
    assert_output(
        &[
            ("lib.wi", PARCEL),
            (
                "app.wi",
                r#"
import lib::Parcel;

class Plus extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

class Times extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side * 7;
    }
}

fn main() {
    println(new Plus(3).label());
    println(new Times(3).label());
}
"#,
            ),
        ],
        "100\n210\n",
    );
}

#[test]
fn module_dispatch_06_every_call_in_one_module_expression_dispatches() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Parcel {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub fn twice(self) -> i64 {
        return self.size() + self.size();
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(new EntryParcel(3).twice());
}
"#,
            ),
        ],
        "20\n",
    );
}

#[test]
fn module_dispatch_07_indirect_through_a_second_module_method() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Parcel {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub fn inner(self) -> i64 {
        return self.size() * 10;
    }

    pub fn outer(self) -> i64 {
        return self.inner() + 1;
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(new EntryParcel(3).outer());
}
"#,
            ),
        ],
        "101\n",
    );
}

#[test]
fn module_dispatch_08_open_method_with_parameters() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Scaler {
    pub side: i64;

    pub open fn scale(self, factor: i64) -> i64 {
        return self.side * factor;
    }

    pub fn apply(self) -> i64 {
        return self.scale(4);
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Scaler;

class Shifted extends Scaler {
    pub override fn scale(self, factor: i64) -> i64 {
        return self.side * factor + 1;
    }
}

fn main() {
    println(new Scaler(3).apply());
    println(new Shifted(3).apply());
}
"#,
            ),
        ],
        "12\n13\n",
    );
}

#[test]
fn module_dispatch_09_override_reads_a_field_the_subclass_added() {
    assert_output(
        &[
            ("lib.wi", PARCEL),
            (
                "app.wi",
                r#"
import lib::Parcel;

class Padded extends Parcel {
    pub pad: i64;

    pub override fn size(self) -> i64 {
        return self.side + self.pad;
    }
}

fn main() {
    println(new Padded(3, 5).label());
}
"#,
            ),
        ],
        "80\n",
    );
}

#[test]
fn module_dispatch_10_base_typed_module_parameter_accepts_a_subclass() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Parcel {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

pub fn combine(a: Parcel, b: Parcel) -> i64 {
    return a.size() * 100 + b.size();
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Parcel;
import lib::combine;

class Big extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side * 3;
    }
}

fn main() {
    println(combine(new Parcel(4), new Big(4)));
}
"#,
            ),
        ],
        "412\n",
    );
}

#[test]
fn module_dispatch_11_override_is_reached_from_a_loop_in_the_module() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Parcel {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub fn total(self) -> i64 {
        let mut sum = 0;
        for i in 0..3 {
            sum = sum + self.size();
        }
        return sum;
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(new EntryParcel(3).total());
}
"#,
            ),
        ],
        "30\n",
    );
}

#[test]
fn module_dispatch_12_override_is_reached_from_a_branch_in_the_module() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Parcel {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub fn pick(self, take: bool) -> i64 {
        if take {
            return self.size();
        }
        return 0;
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(new EntryParcel(3).pick(true));
    println(new EntryParcel(3).pick(false));
}
"#,
            ),
        ],
        "10\n0\n",
    );
}

#[test]
fn module_dispatch_13_one_module_overrides_another_modules_class() {
    assert_output(
        &[
            ("lib.wi", PARCEL),
            (
                "heavy.wi",
                r#"
module heavy;

import lib;

pub class HeavyParcel extends lib::Parcel {
    pub side: i64;

    pub override fn size(self) -> i64 {
        return self.side * 5;
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import heavy::HeavyParcel;
import lib::Parcel;

fn main() {
    println(new HeavyParcel(3).label());
}
"#,
            ),
        ],
        "150\n",
    );
}

#[test]
fn module_dispatch_14_module_and_entry_overrides_of_one_module_base() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Parcel {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub fn label(self) -> i64 {
        return self.size() * 10;
    }
}
"#,
            ),
            (
                "heavy.wi",
                r#"
module heavy;

import lib;

pub open class HeavyParcel extends lib::Parcel {
    pub side: i64;

    pub open override fn size(self) -> i64 {
        return self.side * 5;
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import heavy::HeavyParcel;
import lib::Parcel;

class HugeParcel extends HeavyParcel {
    pub override fn size(self) -> i64 {
        return self.side * 50;
    }
}

fn main() {
    println(new Parcel(3).label());
    println(new HeavyParcel(3).label());
    println(new HugeParcel(3).label());
}
"#,
            ),
        ],
        "30\n150\n1500\n",
    );
}

#[test]
fn module_dispatch_15_module_interface_default_reaches_an_entry_class() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub interface Measured {
    fn size(self) -> i64;

    fn label(self) -> i64 {
        return self.size() * 10;
    }
}

pub open class Parcel implements Measured {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(new Parcel(3).label());
    println(new EntryParcel(3).label());
}
"#,
            ),
        ],
        "30\n100\n",
    );
}

#[test]
fn module_dispatch_17_a_non_open_module_method_is_unaffected() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Parcel {
    pub side: i64;

    pub fn fixed(self) -> i64 {
        return self.side * 2;
    }

    pub fn label(self) -> i64 {
        return self.fixed() * 10;
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Parcel;

class EntryParcel extends Parcel {
    pub fn extra(self) -> i64 {
        return 1;
    }
}

fn main() {
    println(new EntryParcel(3).label());
    println(new EntryParcel(3).extra());
}
"#,
            ),
        ],
        "60\n1\n",
    );
}

#[test]
fn module_dispatch_18_a_subclass_overriding_nothing_runs_the_module_body() {
    assert_output(
        &[
            ("lib.wi", PARCEL),
            (
                "app.wi",
                r#"
import lib::Parcel;

class Plain extends Parcel {
    pub fn tag(self) -> i64 {
        return 1;
    }
}

class Plus extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(new Plain(3).label());
    println(new Plus(3).label());
}
"#,
            ),
        ],
        "30\n100\n",
    );
}

#[test]
fn module_dispatch_19_dispatch_for_a_string_returning_method() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Parcel {
    pub side: i64;

    pub open fn name(self) -> String {
        return "parcel";
    }

    pub fn label(self) -> String {
        return self.name();
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn name(self) -> String {
        return "entry parcel";
    }
}

fn main() {
    println(new Parcel(3).label());
    println(new EntryParcel(3).label());
}
"#,
            ),
        ],
        "parcel\nentry parcel\n",
    );
}

#[test]
fn module_dispatch_20_an_override_calling_a_second_open_method() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Parcel {
    pub side: i64;

    pub open fn unit(self) -> i64 {
        return 1;
    }

    pub open fn size(self) -> i64 {
        return self.side;
    }

    pub fn label(self) -> i64 {
        return self.size() * 10;
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn unit(self) -> i64 {
        return 2;
    }

    pub override fn size(self) -> i64 {
        return self.side * self.unit();
    }
}

fn main() {
    println(new EntryParcel(3).label());
}
"#,
            ),
        ],
        "60\n",
    );
}

#[test]
fn module_dispatch_21_override_is_reached_through_a_base_typed_local() {
    assert_output(
        &[
            (
                "lib.wi",
                r#"
module lib;

pub open class Parcel {
    pub side: i64;

    pub open fn size(self) -> i64 {
        return self.side;
    }
}

pub fn relabel(p: Parcel) -> i64 {
    let copy: Parcel = p;
    return copy.size() * 10;
}
"#,
            ),
            (
                "app.wi",
                r#"
import lib::Parcel;
import lib::relabel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 {
        return self.side + 7;
    }
}

fn main() {
    println(relabel(new EntryParcel(3)));
}
"#,
            ),
        ],
        "100\n",
    );
}

#[test]
fn module_dispatch_22_four_level_chain_across_two_modules_and_the_entry() {
    assert_output(
        &[
            (
                "base.wi",
                r#"
module base;

pub open class Level {
    pub side: i64;

    pub open fn depth(self) -> i64 {
        return 1;
    }

    pub fn report(self) -> i64 {
        return self.depth() * 1000 + self.side;
    }
}
"#,
            ),
            (
                "mid.wi",
                r#"
module mid;

import base;

pub open class Second extends base::Level {
    pub side: i64;

    pub open override fn depth(self) -> i64 {
        return 2;
    }
}

pub open class Third extends Second {
    pub open override fn depth(self) -> i64 {
        return 3;
    }
}
"#,
            ),
            (
                "app.wi",
                r#"
import base::Level;
import mid::Second;
import mid::Third;

class Fourth extends Third {
    pub override fn depth(self) -> i64 {
        return 4;
    }
}

fn main() {
    println(new Level(7).report());
    println(new Second(7).report());
    println(new Third(7).report());
    println(new Fourth(7).report());
}
"#,
            ),
        ],
        "1007\n2007\n3007\n4007\n",
    );
}
