//! A static property's INITIALIZER expression on the LIR-walking backend
//! (willow-t0uy.3).
//!
//! An initializer is written outside every function, so the compiler gives it
//! one of its own — `Class::$static_init.field`, called from
//! `__willow_static_init` before `main`. Until this bead that synthetic
//! function was the last body still emitted by walking the AST: every other
//! checked body moved to lowered IR in willow-0g8j.3, and the AST emitter was
//! kept alive for this one caller. So an initializer quietly had a different
//! expression subset than the identical expression one line down in a
//! function, and nothing reported the difference.
//!
//! The initializer lowers to HIR now — a parameterless function returning the
//! property's type, whose single statement is `return <initializer>` — and
//! compiles from LIR like every other body. With the AST emitter gone there is
//! nothing left to fall back to, so an initializer the walker cannot take is a
//! compile error, and a run that prints the right answer is proof the walker
//! produced it.
//!
//! 26 perspectives:
//!   1 every initializer is logged      14 an instance method on a new value
//!   2 an integer literal               15 an earlier static of the class
//!   3 folded arithmetic                16 an inherited static, via subclass
//!   4 unary negation                   17 a ternary picks the value
//!   5 a comparison into a bool         18 an enum variant with a payload
//!   6 an f64 expression                19 an `Option::Some` payload
//!   7 a String concatenation           20 an interface-typed slot boxes
//!   8 an intrinsic method call         21 declaration order across classes
//!   9 an Array literal                 22 stores after initialization
//!  10 an empty Array literal           23 a String slot under alloc stress
//!  11 a `new` object                   24 an Array slot under minor stress
//!  12 a free function call             25 a module class's own initializer
//!  13 a static method with an effect   26 the example, end to end

use super::support::{
    compile_temp_project_with_env_and_run, compile_temp_project_with_env_stderr,
    compile_with_compiler_env, compile_with_env_and_run, compile_with_env_and_run_under,
};

const PLAIN: [(&str, &str); 0] = [];
const LIR_LOG: [(&str, &str); 1] = [("WILLOW_LIR_LOG", "1")];
const ALLOC_STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "alloc")];
const MINOR_STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "minor")];

/// The program builds and prints `expected`. A body the walker cannot take is
/// a compile error since willow-0g8j.3, so this also asserts the initializer
/// stayed inside the lowered subset.
#[track_caller]
fn assert_output(source: &str, expected: &str) {
    let (out, ok) = compile_with_env_and_run(source, &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// One class holding `field`, printed by `main` through `print`.
fn one_static(field: &str, print: &str) -> String {
    format!("class Cfg {{ pub static {field} }}\nfn main() {{ println({print}); }}\n")
}

/// Perspective 1: the walker claims every initializer, under the synthetic
/// name the class and field give it. A property with no initializer has no
/// such function at all — its slot simply stays zero.
#[test]
fn static_init_01_every_initializer_compiles_from_lowered_ir() {
    let src = "class Cfg {
    pub static a: i64 = 1;
    pub static b: String = \"two\";
    pub static c: bool = true;
}
fn main() { println(Cfg::a); println(Cfg::b); println(Cfg::c); }
";
    let (ok, log) = compile_with_compiler_env(src, &LIR_LOG);
    assert!(ok, "build failed: {log}");
    for field in ["a", "b", "c"] {
        let line = format!("[lir] compiling `Cfg::$static_init.{field}` from lowered IR");
        assert!(log.contains(&line), "missing `{line}` in:\n{log}");
    }
}

/// Perspectives 2-6: the scalar initializer forms. Each is the whole body of
/// its own synthetic function, so each exercises a different lowered return.
#[test]
fn static_init_02_scalar_initializers_run() {
    assert_output(&one_static("n: i64 = 5;", "Cfg::n"), "5\n");
    assert_output(&one_static("n: i64 = 40 + 2;", "Cfg::n"), "42\n");
    assert_output(&one_static("n: i64 = -5;", "Cfg::n"), "-5\n");
    assert_output(&one_static("b: bool = 1 < 2;", "Cfg::b"), "true\n");
    assert_output(&one_static("x: f64 = 1.5 * 2.0;", "Cfg::x"), "3\n");
}

/// Perspectives 7-8: a String initializer allocates before the global store,
/// so the value it writes is a fresh heap object rather than a static blob.
#[test]
fn static_init_03_string_initializers_allocate_then_store() {
    assert_output(
        &one_static("s: String = \"wil\" + \"low\";", "Cfg::s"),
        "willow\n",
    );
    assert_output(&one_static("s: String = 42.toString();", "Cfg::s"), "42\n");
}

/// Perspectives 9-11: GC-managed initializers. The slot starts null so a
/// collection during static init sees a safe slot, and the store goes through
/// the write barrier a permanent root needs.
#[test]
fn static_init_04_gc_values_are_stored_into_rooted_slots() {
    let arrays = "import std::collections::Array;
class Cfg {
    pub static list: Array<i64> = [1, 2, 3];
    pub static blank: Array<i64> = [];
}
fn main() { println(Cfg::list[0] + Cfg::list[2]); println(Cfg::blank.len()); }
";
    assert_output(arrays, "4\n0\n");
    let src = "class Point { pub x: i64; }
class Cfg { pub static origin: Point = new Point(3); }
fn main() { println(Cfg::origin.x); }
";
    assert_output(src, "3\n");
}

/// Perspectives 12-14: an initializer may CALL. A free function, a static
/// method, and an instance method on a receiver the initializer allocates
/// itself — all three are ordinary lowered calls out of the synthetic body.
#[test]
fn static_init_05_calls_run_during_initialization() {
    let src = "fn seed() -> i64 { return 7; }
class Point {
    pub x: i64;
    pub fn doubled(self) -> i64 { return self.x * 2; }
}
class Base {
    pub static mut hits: i64 = 0;
    pub static fn bump() -> i64 { Base::hits = Base::hits + 1; return Base::hits; }
}
class Cfg {
    pub static from_fn: i64 = seed();
    pub static from_static: i64 = Base::bump();
    pub static from_method: i64 = new Point(4).doubled();
}
fn main() { println(Cfg::from_fn); println(Cfg::from_static); println(Cfg::from_method); }
";
    assert_output(src, "7\n1\n8\n");
}

/// Perspectives 15-16: initializers run in declaration order, so one may read
/// a static declared before it — in its own class, or inherited through a
/// subclass name that resolves to the base's storage.
#[test]
fn static_init_06_reads_of_earlier_statics_see_their_values() {
    let src = "open class Base {
    pub static mut hits: i64 = 0;
    pub static fn bump() -> i64 { Base::hits = Base::hits + 1; return Base::hits; }
}
class Derived extends Base {}
class Cfg {
    pub static answer: i64 = 40 + 2;
    pub static prior: i64 = Cfg::answer + 1;
    pub static bumped: i64 = Base::bump();
    pub static inherited: i64 = Derived::hits;
}
fn main() { println(Cfg::prior); println(Cfg::inherited); }
";
    assert_output(src, "43\n1\n");
}

/// Perspective 17: a ternary inside an initializer. The synthetic body has the
/// same block graph any other function does, so a branch needs no special
/// case.
#[test]
fn static_init_07_a_ternary_picks_the_initial_value() {
    let src = "class Cfg {
    pub static answer: i64 = 42;
    pub static picked: i64 = Cfg::answer > 40 ? 10 : 20;
}
fn main() { println(Cfg::picked); }
";
    assert_output(src, "10\n");
}

/// Perspectives 18-19: payload-carrying values. Both are enum constructions,
/// so both allocate a `[tag | payload]` object the slot then roots.
#[test]
fn static_init_08_enum_and_option_payloads_survive_initialization() {
    let src = "enum Shade { Light, Dark(i64) }
class Cfg {
    pub static shade: Shade = Shade::Dark(3);
    pub static maybe: i64? = Option::Some(5);
}
fn main() {
    match Cfg::shade { Shade::Dark(v) => { println(v); } _ => { println(0); } }
    match Cfg::maybe { Option::Some(v) => { println(v); } Option::None => { println(-1); } }
}
";
    assert_output(src, "3\n5\n");
}

/// Perspective 20: an interface-typed slot. The initializer boxes the class
/// value, and the box is what a later virtual call dispatches on.
#[test]
fn static_init_09_an_interface_slot_holds_a_box() {
    let src = "interface Named { fn name(self) -> String; }
class Point implements Named {
    pub x: i64;
    pub fn name(self) -> String { return \"p\" + self.x.toString(); }
}
class Cfg { pub static named: Named = new Point(8); }
fn main() { println(Cfg::named.name()); }
";
    assert_output(src, "p8\n");
}

/// Perspective 21: declaration order holds ACROSS classes, so a later class's
/// initializer reads an earlier class's initialized slot.
#[test]
fn static_init_10_declaration_order_spans_classes() {
    let src = "class Cfg { pub static answer: i64 = 42; }
class Echo { pub static doubled: i64 = Cfg::answer * 2; }
fn main() { println(Echo::doubled); }
";
    assert_output(src, "84\n");
}

/// Perspective 22: initialization is the FIRST store, not a special one. A
/// `static mut` keeps taking ordinary stores afterwards, and the first of them
/// reads the initialized value.
#[test]
fn static_init_11_a_mutable_static_keeps_taking_stores() {
    let src = "class Cfg { pub static mut n: i64 = 1; }
fn main() { Cfg::n = Cfg::n + 10; println(Cfg::n); }
";
    assert_output(src, "11\n");
}

/// Perspectives 23-24: the GC slots again, this time with a collection forced
/// at every allocation and at every minor cycle. A missed root or a missed
/// barrier in the initializer path shows up here as a corrupt read.
#[test]
fn static_init_12_gc_slots_survive_collection_stress() {
    let src = "import std::collections::Array;
class Cfg {
    pub static label: String = \"wil\" + \"low\";
    pub static list: Array<i64> = [1, 2, 3];
}
fn main() {
    let mut i = 0;
    while i < 50 {
        let filler = [i, i + 1, i + 2];
        i = i + filler.len();
    }
    println(Cfg::label);
    println(Cfg::list[0] + Cfg::list[2]);
}
";
    for run_env in [&ALLOC_STRESS, &MINOR_STRESS] {
        let (out, ok) = compile_with_env_and_run_under(src, &PLAIN, run_env);
        assert!(ok, "run failed: {out}");
        assert_eq!(out, "willow\n4\n", "wrong output under {run_env:?}");
    }
}

/// Perspective 25: a MODULE class's initializer. It is compiled in that
/// module's body phase, under that module's aliases, so a bare class name and
/// a bare function call in the initializer resolve to the module's own — and
/// the synthetic function is keyed by the module-qualified class name.
#[test]
fn static_init_13_a_module_class_initializes_under_its_own_aliases() {
    let files = [
        (
            "holder.wi",
            "pub class Slot {
    pub static base: i64 = 40 + 2;
    pub static label: String = \"slot\";
}
pub fn seed() -> i64 { return Slot::base; }
",
        ),
        (
            "main.wi",
            "import holder;
fn main() { println(holder::seed()); println(holder::Slot::label); }
",
        ),
    ];
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, "42\nslot\n");

    let (ok, log) = compile_temp_project_with_env_stderr(&files, "main.wi", &LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("$static_init.base") && log.contains("$static_init.label"),
        "module initializers were not compiled from lowered IR:\n{log}"
    );
}

/// The type checker still owns an initializer's type rule, and rejects a
/// mismatch before lowering ever sees it — with the AST emitter gone there is
/// no second path that could have accepted it.
#[test]
fn static_init_14_a_mistyped_initializer_is_a_diagnostic() {
    let src = "class Cfg { pub static n: i64 = \"text\"; }\nfn main() { println(Cfg::n); }\n";
    let (ok, err) = compile_with_compiler_env(src, &PLAIN);
    assert!(!ok, "a mistyped initializer must not build");
    assert!(err.contains("E0301"), "unexpected diagnostic:\n{err}");
}

/// Perspective 26: the whole example, with every initializer form in one
/// program, and every one of them claimed by the walker.
#[test]
fn static_init_15_example_compiles_from_lowered_ir_and_runs() {
    let src = std::fs::read_to_string("example/lir_static_initializers.wi")
        .expect("example/lir_static_initializers.wi must exist");
    let (out, ok) = compile_with_env_and_run(&src, &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(
        out,
        "42\n-5\ntrue\n3\nwillow\n42\n4\n0\n3\n7\n1\n8\n43\n1\n10\n3\n5\np8\n84\n11\n"
    );

    let (ok, log) = compile_with_compiler_env(&src, &LIR_LOG);
    assert!(ok, "build failed: {log}");
    for field in [
        "answer",
        "negative",
        "ready",
        "ratio",
        "label",
        "digits",
        "list",
        "blank",
        "origin",
        "from_fn",
        "from_static",
        "from_method",
        "prior",
        "inherited",
        "picked",
        "shade",
        "maybe",
        "named",
    ] {
        let line = format!("[lir] compiling `Config::$static_init.{field}` from lowered IR");
        assert!(log.contains(&line), "missing `{line}`");
    }
    assert!(
        log.contains("[lir] compiling `Echo::$static_init.doubled` from lowered IR"),
        "the second class's initializer was not compiled from lowered IR"
    );
}
