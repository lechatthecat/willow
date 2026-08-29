//! A default interface method is injected ONCE per `extends` chain (willow-3eo1).
//!
//! `implements` is propagated down an `extends` chain so every subclass gets its
//! own (class, interface) vtable (willow-2s4i). Default-method injection then ran
//! for every one of those classes, so `Base`, `Mid` and `Leaf` each received a
//! private copy of the same interface default body. Two consequences, both
//! wrong:
//!
//!   * the copy on the subclass shadows the identical copy on the base, and the
//!     override rules reported `E0702: method `m` overrides `Base` but is missing
//!     `override`` against a method the user never wrote — there was no way to
//!     silence it, because writing `override` then hit `E0703` on a base copy
//!     that is not `open`;
//!   * the copies are neither `open` nor `override`, so none of them gets a
//!     vtable slot, and the backend hit `compiler invariant violated: method
//!     `C::m` has no virtual slot but 2 candidate implementations`.
//!
//! Injection now stops at the topmost class of the chain that implements the
//! interface; the subclasses INHERIT that method exactly as they inherit any
//! other base method. `self` inside the default still dispatches virtually, so
//! an override further down is what the default body calls.
//!
//! 22 perspectives:
//!   01 a subclass inherits its base's default instead of copying it
//!   02 the inherited default dispatches to the subclass's override
//!   03 a three-level chain reaches the default from the deepest class
//!   04 the default is reachable through an interface-typed parameter
//!   05 a class that implements directly still gets the default
//!   06 a class that declares the method itself wins over the default
//!   07 a subclass may declare the method itself, for that subclass only
//!   08 sibling subclasses both reach the one inherited default
//!   09 a default that calls another default
//!   10 a middle class with its own methods keeps the injection point
//!   11 a generic interface's default keeps its substituted types
//!   12 a default inherited from a SUPER-interface
//!   13 two independent interfaces with different defaults
//!   14 genuinely ambiguous defaults are still E0425
//!   15 a hand-written shadow without `override` is still E0702
//!   16 `override` with no inherited method is still E0702
//!   17 overriding a non-`open` base method is still E0703
//!   18 a module's own chain compiles and runs
//!   19 an entry subclass of a module base reaches the module's default
//!   20 the chain still works when the base itself overrides nothing
//!   21 an entry subclass that RE-STATES the module interface still compiles
//!   22 an imported sealed method wins over an entry interface default

use super::support::{compile_and_run, compile_error_stderr, compile_temp_project_and_run};

/// Compile and run a single-file program expected to succeed.
fn assert_output(source: &str, expected: &str) {
    let (output, ok) = compile_and_run(source);
    assert!(ok, "expected the program to compile:\n{output}");
    assert_eq!(output, expected);
}

/// The shape the bug was about: an interface with a default, an `open` base
/// that implements it, and a subclass that inherits the `implements`.
const CHAIN: &str = "
interface Measured {
    fn size(self) -> i64;
    fn label(self) -> i64 { return self.size() * 10; }
}

open class Parcel implements Measured {
    pub side: i64;
    pub open fn size(self) -> i64 { return self.side; }
}

class BigParcel extends Parcel {
    pub override fn size(self) -> i64 { return self.side * 2; }
}
";

// 1. The subclass has no copy of its own, so calling the default on it resolves
//    up the chain to the base's single copy. Before the fix this program did not
//    even reach codegen: E0702 rejected the injected copy.
#[test]
fn default_inheritance_01_subclass_inherits_the_base_default() {
    assert_output(
        &format!(
            "{CHAIN}
fn main() {{
    let p = new Parcel(3);
    println(p.label());
}}
"
        ),
        "30\n",
    );
}

// 2. `self.size()` inside the inherited default is a virtual call, so the copy
//    that lives on `Parcel` still reaches `BigParcel`'s override. This is the
//    reason inheriting (rather than copying) is safe.
#[test]
fn default_inheritance_02_the_default_reaches_the_subclass_override() {
    assert_output(
        &format!(
            "{CHAIN}
fn main() {{
    let b = new BigParcel(3);
    println(b.size());
    println(b.label());
}}
"
        ),
        "6\n60\n",
    );
}

// 3. Three levels: the injection belongs to the topmost implementing class and
//    the two below it inherit through it.
#[test]
fn default_inheritance_03_three_level_chain() {
    assert_output(
        "
interface Measured {
    fn size(self) -> i64;
    fn label(self) -> i64 { return self.size() + 1; }
}

open class A implements Measured {
    pub n: i64;
    pub open fn size(self) -> i64 { return self.n; }
}

open class B extends A {
}

class C extends B {
    pub override fn size(self) -> i64 { return self.n * 100; }
}

fn main() {
    println(new A(1).label());
    println(new B(2).label());
    println(new C(3).label());
}
",
        "2\n3\n301\n",
    );
}

// 4. The subclass is still usable as the interface (it keeps its own vtable),
//    and the slot for the default resolves to the base's implementation.
#[test]
fn default_inheritance_04_default_through_an_interface_parameter() {
    assert_output(
        &format!(
            "{CHAIN}
fn described(m: Measured) -> i64 {{
    return m.label();
}}

fn main() {{
    println(described(new Parcel(4)));
    println(described(new BigParcel(4)));
}}
"
        ),
        "40\n80\n",
    );
}

// 5. The no-inheritance case is untouched: a standalone class that implements
//    the interface gets the default injected as before.
#[test]
fn default_inheritance_05_standalone_class_still_gets_the_default() {
    assert_output(
        "
interface Named {
    fn base(self) -> i64;
    fn shout(self) -> i64 { return self.base() + 7; }
}

class Solo implements Named {
    pub n: i64;
    pub fn base(self) -> i64 { return self.n; }
}

fn main() {
    println(new Solo(5).shout());
}
",
        "12\n",
    );
}

// 6. An explicit method on the implementing class still wins: nothing is
//    injected over it.
#[test]
fn default_inheritance_06_explicit_method_wins_over_the_default() {
    assert_output(
        "
interface Named {
    fn base(self) -> i64;
    fn shout(self) -> i64 { return self.base() + 7; }
}

class Solo implements Named {
    pub n: i64;
    pub fn base(self) -> i64 { return self.n; }
    pub fn shout(self) -> i64 { return 999; }
}

fn main() {
    println(new Solo(5).shout());
}
",
        "999\n",
    );
}

// 7. A subclass may still declare the method itself. It then legitimately
//    shadows the base's inherited copy, so it needs `override` and the base copy
//    must be `open` — which the injected copy is not, so the subclass declares
//    it against an `open` method the base wrote by hand instead.
#[test]
fn default_inheritance_07_subclass_may_declare_the_method_itself() {
    assert_output(
        "
interface Measured {
    fn size(self) -> i64;
    fn label(self) -> i64 { return self.size() * 10; }
}

open class Parcel implements Measured {
    pub side: i64;
    pub open fn size(self) -> i64 { return self.side; }
    pub open fn label(self) -> i64 { return self.size() * 10; }
}

class Loud extends Parcel {
    pub override fn label(self) -> i64 { return 1; }
}

fn main() {
    println(new Parcel(3).label());
    println(new Loud(3).label());
}
",
        "30\n1\n",
    );
}

// 8. Two subclasses of the same base: one shared inherited default, two
//    different `size` overrides underneath it.
#[test]
fn default_inheritance_08_sibling_subclasses_share_the_default() {
    assert_output(
        "
interface Measured {
    fn size(self) -> i64;
    fn label(self) -> i64 { return self.size() * 10; }
}

open class Parcel implements Measured {
    pub side: i64;
    pub open fn size(self) -> i64 { return self.side; }
}

class Small extends Parcel {
    pub override fn size(self) -> i64 { return self.side - 1; }
}

class Large extends Parcel {
    pub override fn size(self) -> i64 { return self.side + 1; }
}

fn main() {
    println(new Small(5).label());
    println(new Large(5).label());
}
",
        "40\n60\n",
    );
}

// 9. One default calling another: both live on the base, and both see the
//    subclass through `self`.
#[test]
fn default_inheritance_09_a_default_calling_another_default() {
    assert_output(
        "
interface Measured {
    fn size(self) -> i64;
    fn doubled(self) -> i64 { return self.size() * 2; }
    fn quadrupled(self) -> i64 { return self.doubled() * 2; }
}

open class Parcel implements Measured {
    pub side: i64;
    pub open fn size(self) -> i64 { return self.side; }
}

class BigParcel extends Parcel {
    pub override fn size(self) -> i64 { return self.side + 10; }
}

fn main() {
    println(new Parcel(1).quadrupled());
    println(new BigParcel(1).quadrupled());
}
",
        "4\n44\n",
    );
}

// 10. The middle class of the chain may declare methods of its own without
//     moving where the default is injected.
#[test]
fn default_inheritance_10_middle_class_with_its_own_methods() {
    assert_output(
        "
interface Measured {
    fn size(self) -> i64;
    fn label(self) -> i64 { return self.size() * 10; }
}

open class Parcel implements Measured {
    pub side: i64;
    pub open fn size(self) -> i64 { return self.side; }
}

open class Tagged extends Parcel {
    pub fn tag(self) -> i64 { return self.side + 1; }
}

class BigTagged extends Tagged {
    pub override fn size(self) -> i64 { return self.side * 3; }
}

fn main() {
    println(new Tagged(2).label());
    println(new Tagged(2).tag());
    println(new BigTagged(2).label());
}
",
        "20\n3\n60\n",
    );
}

// 11. A generic interface's default is injected with its type arguments
//     substituted; the subclass inherits that one substituted copy.
#[test]
fn default_inheritance_11_generic_interface_default() {
    assert_output(
        "
interface Holder<T> {
    fn get(self) -> T;
    fn twice(self) -> T { return self.get(); }
}

open class IntBox implements Holder<i64> {
    pub v: i64;
    pub open fn get(self) -> i64 { return self.v; }
}

class BiggerBox extends IntBox {
    pub override fn get(self) -> i64 { return self.v + 5; }
}

fn main() {
    println(new IntBox(1).twice());
    println(new BiggerBox(1).twice());
}
",
        "1\n6\n",
    );
}

// 12. The default may come from a SUPER-interface of the one the base names.
#[test]
fn default_inheritance_12_default_from_a_super_interface() {
    assert_output(
        "
interface Base {
    fn size(self) -> i64;
    fn label(self) -> i64 { return self.size() * 10; }
}

interface Derived extends Base {
}

open class Parcel implements Derived {
    pub side: i64;
    pub open fn size(self) -> i64 { return self.side; }
}

class BigParcel extends Parcel {
    pub override fn size(self) -> i64 { return self.side * 2; }
}

fn main() {
    println(new Parcel(3).label());
    println(new BigParcel(3).label());
}
",
        "30\n60\n",
    );
}

// 13. Two independent interfaces contributing DIFFERENT defaults is not
//     ambiguous; both land on the base and both are inherited.
#[test]
fn default_inheritance_13_two_interfaces_with_different_defaults() {
    assert_output(
        "
interface Sized {
    fn size(self) -> i64;
    fn label(self) -> i64 { return self.size() * 10; }
}

interface Tagged {
    fn size(self) -> i64;
    fn tag(self) -> i64 { return self.size() + 1; }
}

open class Parcel implements Sized, Tagged {
    pub side: i64;
    pub open fn size(self) -> i64 { return self.side; }
}

class BigParcel extends Parcel {
    pub override fn size(self) -> i64 { return self.side * 2; }
}

fn main() {
    println(new BigParcel(3).label());
    println(new BigParcel(3).tag());
}
",
        "60\n7\n",
    );
}

// 14. Skipping the subclass injection must not swallow the real conflict: two
//     independent interfaces providing the SAME default name is still E0425.
#[test]
fn default_inheritance_14_ambiguous_defaults_are_still_rejected() {
    let stderr = compile_error_stderr(
        "
interface Left {
    fn label(self) -> i64 { return 1; }
}

interface Right {
    fn label(self) -> i64 { return 2; }
}

class Both implements Left, Right {
    pub n: i64;
}

fn main() {
    println(new Both(0).label());
}
",
    );
    assert!(
        stderr.contains("E0425"),
        "conflicting defaults must still be reported:\n{stderr}"
    );
}

// 15. The override rules are only relaxed for the SYNTHESIZED copies. A method
//     the user actually wrote that shadows a base method still needs `override`.
#[test]
fn default_inheritance_15_hand_written_shadow_still_needs_override() {
    let stderr = compile_error_stderr(
        "
open class Base {
    pub n: i64;
    pub open fn value(self) -> i64 { return self.n; }
}

class Child extends Base {
    pub fn value(self) -> i64 { return 1; }
}

fn main() {
    println(new Child(0).value());
}
",
    );
    assert!(
        stderr.contains("E0702") && stderr.contains("override"),
        "a hand-written shadow must still require `override`:\n{stderr}"
    );
}

// 16. The other half of E0702 — `override` with nothing to override — is
//     likewise untouched.
#[test]
fn default_inheritance_16_override_without_a_base_method_is_still_rejected() {
    let stderr = compile_error_stderr(
        "
open class Base {
    pub n: i64;
}

class Child extends Base {
    pub override fn value(self) -> i64 { return 1; }
}

fn main() {
    println(new Child(0).value());
}
",
    );
    assert!(
        stderr.contains("E0702"),
        "`override` with no base method must still be reported:\n{stderr}"
    );
}

// 17. E0703 still fires for a hand-written override of a base method that is
//     not `open`.
#[test]
fn default_inheritance_17_overriding_a_sealed_method_is_still_rejected() {
    let stderr = compile_error_stderr(
        "
open class Base {
    pub n: i64;
    pub fn value(self) -> i64 { return self.n; }
}

class Child extends Base {
    pub override fn value(self) -> i64 { return 1; }
}

fn main() {
    println(new Child(0).value());
}
",
    );
    assert!(
        stderr.contains("E0703"),
        "overriding a non-open method must still be reported:\n{stderr}"
    );
}

// 18. The chain inside an imported MODULE — the case that surfaced this, since
//     module bodies were not checked before willow-3eo1.
#[test]
fn default_inheritance_18_a_module_chain_compiles_and_runs() {
    let lib = "
pub interface Measured {
    fn size(self) -> i64;
    fn label(self) -> i64 { return self.size() * 10; }
}

pub open class Parcel implements Measured {
    pub side: i64;
    pub open fn size(self) -> i64 { return self.side; }
}

pub class BigParcel extends Parcel {
    pub override fn size(self) -> i64 { return self.side * 2; }
}
";
    let app = "
import lib::Parcel;
import lib::BigParcel;

fn main() {
    println(new Parcel(3).label());
    println(new BigParcel(3).label());
}
";
    let (output, ok) = compile_temp_project_and_run(&[("lib.wi", lib), ("app.wi", app)], "app.wi");
    assert!(ok, "expected the project to compile:\n{output}");
    assert_eq!(output, "30\n60\n");
}

// 19. An ENTRY subclass of a module base. Imported class shapes tell the entry
//     desugar that the default already lives on the base, so the subclass
//     inherits that one copy.
#[test]
fn default_inheritance_19_entry_subclass_of_a_module_base() {
    let lib = "
pub interface Measured {
    fn size(self) -> i64;
    fn label(self) -> i64 { return self.size() * 10; }
}

pub open class Parcel implements Measured {
    pub side: i64;
    pub open fn size(self) -> i64 { return self.side; }
}
";
    let app = "
import lib::Parcel;

class EntryParcel extends Parcel {
    pub override fn size(self) -> i64 { return self.side + 7; }
}

fn main() {
    println(new EntryParcel(3).label());
}
";
    let (output, ok) = compile_temp_project_and_run(&[("lib.wi", lib), ("app.wi", app)], "app.wi");
    assert!(ok, "expected the project to compile:\n{output}");
    assert_eq!(output, "100\n");
}

// 20. A subclass that overrides nothing at all still reaches the default, and
//     the base's own `size` is what it runs.
#[test]
fn default_inheritance_20_subclass_that_overrides_nothing() {
    assert_output(
        &format!(
            "{CHAIN}
class PlainParcel extends Parcel {{
}}

fn main() {{
    println(new PlainParcel(6).label());
}}
"
        ),
        "60\n",
    );
}

// 21. Re-stating the imported interface does not force a second default copy:
//     the imported ancestor shape still proves the base provides it.
#[test]
fn default_inheritance_21_entry_subclass_restating_the_module_interface() {
    let lib = "
pub interface Measured {
    fn size(self) -> i64;
    fn label(self) -> i64 { return self.size() * 10; }
}

pub open class Parcel implements Measured {
    pub side: i64;
    pub open fn size(self) -> i64 { return self.side; }
}
";
    let app = "
import lib::Parcel;
import lib::Measured;

class EntryParcel extends Parcel implements Measured {
    pub override fn size(self) -> i64 { return self.side + 7; }
}

fn main() {
    println(new EntryParcel(3).size());
    println(new EntryParcel(3).label());
}
";
    let (output, ok) = compile_temp_project_and_run(&[("lib.wi", lib), ("app.wi", app)], "app.wi");
    assert!(ok, "expected the project to compile:\n{output}");
    assert_eq!(output, "10\n100\n");
}

// 22. An imported ancestor's sealed method already satisfies the interface.
//     The entry class must inherit it instead of receiving a synthesized
//     default with the same name, which would silently replace sealed code.
#[test]
fn default_inheritance_22_imported_sealed_method_wins_over_default() {
    let lib = "
pub open class Base {
    pub n: i64;
    pub fn label(self) -> i64 { return self.n + 4; }
}
";
    let app = "
import lib::Base;

interface Labeled {
    fn label(self) -> i64 { return 99; }
}

class Child extends Base implements Labeled {
}

fn main() {
    println(new Child(3).label());
}
";
    let (output, ok) = compile_temp_project_and_run(&[("lib.wi", lib), ("app.wi", app)], "app.wi");
    assert!(ok, "expected the project to compile:\n{output}");
    assert_eq!(output, "7\n");
}
