use super::*;

// ── Direct type imports: interface dispatch + module-body internal enum use ──
//    (willow-64gs.1)
//
// Two latent bugs are fixed and pinned here. (A) `import mod::Iface` binds the
// bare interface name, but a class records its `implements` entry under the
// qualified name `mod::Iface`; boxing/dispatch must canonicalize so the bare
// alias matches (was E0201 then a vtable-miss segfault). (B) A module function
// that uses its OWN enum internally must resolve the bare `Color::Variant` to
// `mod::Color` during module codegen, instead of silently falling back to
// variant tag 0 — this only manifests when the entry does NOT separately import
// the enum.
//
// 20 test perspectives (each pinned below; a test may cover several):
//   P1  direct iface import: concrete arg to bare `Iface` param dispatches
//   P2  direct iface import: `let x: Iface = new Cls()` then `x.method()`
//   P3  direct iface import: two classes dispatch to their own methods
//   P4  direct iface import: interface with two methods dispatches each slot
//   P5  direct iface import: `Array<Iface>` of mixed classes dispatches per elem
//   P6  negative: non-implementing class to bare `Iface` param → E0201
//   P7  regression: qualified `import mod;` + `mod::Iface` param still works
//   P8  direct iface import: iface method result used in arithmetic
//   P9  negative: direct import of a PRIVATE interface → E0419
//   P10 module fn constructs its own fieldless enum internally (not tag 0)
//   P11 module fn matches its own enum passed as a param
//   P12 entry does NOT import the enum at all; module still correct (bug cond.)
//   P13 regression: entry DOES import the enum; behavior unchanged
//   P14 module fn constructs a PAYLOAD variant internally and binds the payload
//   P15 module CLASS METHOD constructs/matches the module's own enum internally
//   P16 two enums in one module used internally — no tag cross-talk
//   P17 every variant constructed internally (Red/Green/Blue) — all tags correct
//   P18 module helper-to-helper: build enum in one fn, match it in another
//   P19 module-body interface boxing: box a local class to the module's iface
//   P20 end-to-end: direct iface dispatch + module-internal enum together

// P1, P3, P8: direct interface import, two classes, result used in arithmetic.
#[test]
fn dti_iface_01_direct_import_dispatch_two_classes() {
    let shapes = r#"
module shapes;
pub interface Area {
    fn area(self) -> i64;
}
pub class Square implements Area {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
pub class Rect implements Area {
    pub w: i64;
    pub h: i64;
    pub fn area(self) -> i64 { return self.w * self.h; }
}
"#;
    let main = r#"
import shapes::Area;
import shapes::Square;
import shapes::Rect;
fn describe(a: Area) -> i64 { return a.area() + 1; }
fn main() {
    println(describe(new Square(5)));
    println(describe(new Rect(3, 4)));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "direct interface import dispatch failed: {out}");
    assert_eq!(out, "26\n13\n");
}

// P2: direct interface import bound to a local `let` of the interface type.
#[test]
fn dti_iface_02_direct_import_let_binding_dispatch() {
    let shapes = r#"
module shapes;
pub interface Greeter {
    fn hello(self) -> String;
}
pub class En implements Greeter {
    pub fn hello(self) -> String { return "hi"; }
}
"#;
    let main = r#"
import shapes::Greeter;
import shapes::En;
fn main() {
    let g: Greeter = new En();
    println(g.hello());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "direct interface import let-binding failed: {out}");
    assert_eq!(out, "hi\n");
}

// P4: a directly-imported interface with TWO methods dispatches each method
// through the correct vtable slot (method_order indexing under the bare alias).
#[test]
fn dti_iface_03_direct_import_multi_method_dispatch() {
    let shapes = r#"
module shapes;
pub interface Pair {
    fn first(self) -> i64;
    fn second(self) -> i64;
}
pub class Point implements Pair {
    pub x: i64;
    pub y: i64;
    pub fn first(self) -> i64 { return self.x; }
    pub fn second(self) -> i64 { return self.y; }
}
"#;
    let main = r#"
import shapes::Pair;
import shapes::Point;
fn diff(p: Pair) -> i64 { return p.second() - p.first(); }
fn main() {
    println(diff(new Point(3, 10)));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "direct import multi-method dispatch failed: {out}");
    assert_eq!(out, "7\n");
}

// P5: an Array of the directly-imported interface dispatches per element.
#[test]
fn dti_iface_04_direct_import_array_dispatch() {
    let shapes = r#"
module shapes;
pub interface Area {
    fn area(self) -> i64;
}
pub class Square implements Area {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
pub class Rect implements Area {
    pub w: i64;
    pub h: i64;
    pub fn area(self) -> i64 { return self.w * self.h; }
}
"#;
    let main = r#"
import std::collections::Array;
import shapes::Area;
import shapes::Square;
import shapes::Rect;
fn main() {
    let xs: Array<Area> = [new Square(2), new Rect(3, 5)];
    let mut sum = 0;
    let mut i = 0;
    while i < xs.len() {
        sum = sum + xs[i].area();
        i = i + 1;
    }
    println(sum);
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "direct import Array<Iface> dispatch failed: {out}");
    assert_eq!(out, "19\n");
}

// P6: a class that does NOT implement the directly-imported interface is
// rejected when passed to that interface parameter (E0201 still fires).
#[test]
fn dti_iface_05_direct_import_non_impl_rejected() {
    let shapes = r#"
module shapes;
pub interface Area {
    fn area(self) -> i64;
}
pub class Square implements Area {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
pub class Tag {
    pub n: i64;
}
"#;
    let main = r#"
import shapes::Area;
import shapes::Tag;
fn describe(a: Area) -> i64 { return a.area(); }
fn main() {
    println(describe(new Tag(1)));
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("error[E0201]"), "stderr: {stderr}");
}

// P7: the qualified form (module import + `mod::Iface`) is unaffected.
#[test]
fn dti_iface_06_qualified_form_regression() {
    let shapes = r#"
module shapes;
pub interface Area {
    fn area(self) -> i64;
}
pub class Square implements Area {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
"#;
    let main = r#"
import shapes;
fn describe(a: shapes::Area) -> i64 { return a.area(); }
fn main() {
    println(describe(new shapes::Square(6)));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "qualified interface form regressed: {out}");
    assert_eq!(out, "36\n");
}

// P9: directly importing a PRIVATE interface is rejected (E0419).
#[test]
fn dti_iface_07_private_interface_rejected() {
    let shapes = r#"
module shapes;
interface Secret {
    fn area(self) -> i64;
}
pub class Square {
    pub side: i64;
}
"#;
    let main = r#"
import shapes::Secret;
fn main() {
    println(1);
}
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("error[E0419]"), "stderr: {stderr}");
}

// P10, P12, P17: a module function constructs each of its own enum's variants
// internally; the entry imports only the function (NOT the enum). Every tag must
// be correct (the bug returned tag 0 for all of them).
#[test]
fn dti_enum_01_module_internal_construction_all_variants() {
    let pal = r#"
module pal;
pub enum Color { Red, Green, Blue }
pub fn rank(c: Color) -> i64 {
    return match c {
        Color::Red => 1,
        Color::Green => 2,
        Color::Blue => 3,
    };
}
pub fn red() -> i64 { return rank(Color::Red); }
pub fn green() -> i64 { return rank(Color::Green); }
pub fn blue() -> i64 { return rank(Color::Blue); }
"#;
    let main = r#"
import pal::red;
import pal::green;
import pal::blue;
fn main() {
    println(red());
    println(green());
    println(blue());
}
"#;
    let (out, ok) = compile_temp_project_and_run(&[("pal.wi", pal), ("main.wi", main)], "main.wi");
    assert!(ok, "module-internal enum construction failed: {out}");
    assert_eq!(out, "1\n2\n3\n");
}

// P13: regression — when the entry DOES import the enum and constructs values
// itself, dispatch into the module's match is unchanged.
#[test]
fn dti_enum_02_entry_imports_enum_regression() {
    let pal = r#"
module pal;
pub enum Color { Red, Green, Blue }
pub fn rank(c: Color) -> i64 {
    return match c {
        Color::Red => 1,
        Color::Green => 2,
        Color::Blue => 3,
    };
}
"#;
    let main = r#"
import pal::Color;
import pal::rank;
fn main() {
    println(rank(Color::Red));
    println(rank(Color::Green));
    println(rank(Color::Blue));
}
"#;
    let (out, ok) = compile_temp_project_and_run(&[("pal.wi", pal), ("main.wi", main)], "main.wi");
    assert!(ok, "entry-imported enum regressed: {out}");
    assert_eq!(out, "1\n2\n3\n");
}

// P14: a module fn constructs a PAYLOAD variant internally and binds the payload
// in a match, all without the entry importing the enum.
#[test]
fn dti_enum_03_module_internal_payload_variant() {
    let pal = r#"
module pal;
pub enum Kind { Small, Big(i64) }
pub fn weigh(k: Kind) -> i64 {
    return match k {
        Kind::Small => 1,
        Kind::Big(n) => n,
    };
}
pub fn heavy() -> i64 { return weigh(Kind::Big(77)); }
pub fn light() -> i64 { return weigh(Kind::Small); }
"#;
    let main = r#"
import pal::heavy;
import pal::light;
fn main() {
    println(heavy());
    println(light());
}
"#;
    let (out, ok) = compile_temp_project_and_run(&[("pal.wi", pal), ("main.wi", main)], "main.wi");
    assert!(ok, "module-internal payload variant failed: {out}");
    assert_eq!(out, "77\n1\n");
}

// P15: a module CLASS METHOD constructs/matches the module's own enum internally
// (method bodies are compiled within the module alias scope too).
#[test]
fn dti_enum_04_module_class_method_internal_enum() {
    let pal = r#"
module pal;
pub enum Color { Red, Green, Blue }
pub class Painter {
    pub fn pick(self) -> i64 {
        let c = Color::Green;
        return match c {
            Color::Red => 1,
            Color::Green => 2,
            Color::Blue => 3,
        };
    }
}
"#;
    let main = r#"
import pal::Painter;
fn main() {
    println(new Painter().pick());
}
"#;
    let (out, ok) = compile_temp_project_and_run(&[("pal.wi", pal), ("main.wi", main)], "main.wi");
    assert!(ok, "module class-method internal enum failed: {out}");
    assert_eq!(out, "2\n");
}

// P16, P18: two enums in one module, with one fn building a value passed to
// another fn that matches it — no tag cross-talk between the two enums.
#[test]
fn dti_enum_05_two_enums_no_crosstalk() {
    let pal = r#"
module pal;
pub enum Color { Red, Green, Blue }
pub enum Size { S, M, L }
pub fn color_rank(c: Color) -> i64 {
    return match c {
        Color::Red => 1,
        Color::Green => 2,
        Color::Blue => 3,
    };
}
pub fn size_rank(s: Size) -> i64 {
    return match s {
        Size::S => 10,
        Size::M => 20,
        Size::L => 30,
    };
}
pub fn combined() -> i64 {
    let c = Color::Blue;
    let s = Size::M;
    return color_rank(c) + size_rank(s);
}
"#;
    let main = r#"
import pal::combined;
fn main() {
    println(combined());
}
"#;
    let (out, ok) = compile_temp_project_and_run(&[("pal.wi", pal), ("main.wi", main)], "main.wi");
    assert!(ok, "two-enum module no-crosstalk failed: {out}");
    // Blue (3) + M (20) = 23
    assert_eq!(out, "23\n");
}

// dti_enum_06: the willow-favj repro. A module's plain type import constructs
// each variant of another module's enum, and the entry never names the enum.
#[test]
fn dti_enum_06_module_plain_type_import_constructs_each_variant() {
    let main = r#"
import beta;
fn main() {
    println(beta::code_a());
    println(beta::code_b());
    println(beta::code_c());
}
"#;
    let (out, ok) = compile_temp_project_and_run(
        &[
            ("alpha.wi", FAVJ_ALPHA),
            ("beta.wi", FAVJ_BETA),
            ("main.wi", main),
        ],
        "main.wi",
    );
    assert!(ok, "module plain type import failed: {out}");
    assert_eq!(out, "1\n2\n3\n");
}

// dti_enum_07: the same enum crosses the module boundary in both directions —
// the module returns a value the entry hands straight back to it.
#[test]
fn dti_enum_07_imported_enum_round_trips_through_the_entry() {
    let main = r#"
import beta;
fn main() {
    println(beta::code(beta::nth(0)));
    println(beta::code(beta::nth(1)));
    println(beta::code(beta::nth(2)));
}
"#;
    let (out, ok) = compile_temp_project_and_run(
        &[
            ("alpha.wi", FAVJ_ALPHA),
            ("beta.wi", FAVJ_BETA),
            ("main.wi", main),
        ],
        "main.wi",
    );
    assert!(ok, "imported-enum round trip failed: {out}");
    assert_eq!(out, "1\n2\n3\n");
}

// dti_enum_08: a PAYLOAD variant of the imported enum, built and matched inside
// the importing module — the tag and the payload must both survive.
#[test]
fn dti_enum_08_imported_payload_variant_keeps_its_tag() {
    let main = r#"
import beta;
import alpha;
fn main() {
    println(beta::value(beta::payload(41)));
    println(beta::value(alpha::Tag::Plain));
}
"#;
    let (out, ok) = compile_temp_project_and_run(
        &[
            ("alpha.wi", FAVJ_ALPHA),
            ("beta.wi", FAVJ_BETA),
            ("main.wi", main),
        ],
        "main.wi",
    );
    assert!(ok, "imported payload variant failed: {out}");
    assert_eq!(out, "41\n0\n");
}

// dti_enum_09: one enum, three spellings — the owner's bare `Color`, the
// importer's bare `Color`, and the entry's `alpha::Color`. All three name the
// same identity, so a value built under any of them matches under the others.
#[test]
fn dti_enum_09_owner_importer_and_qualified_spellings_agree() {
    let main = r#"
import beta;
import alpha;
fn main() {
    println(beta::code(alpha::Color::B));
    println(alpha::own_code(beta::nth(2)));
    println(beta::code(beta::nth(1)));
}
"#;
    let (out, ok) = compile_temp_project_and_run(
        &[
            ("alpha.wi", FAVJ_ALPHA),
            ("beta.wi", FAVJ_BETA),
            ("main.wi", main),
        ],
        "main.wi",
    );
    assert!(ok, "cross-spelling identity failed: {out}");
    assert_eq!(out, "2\n3\n2\n");
}

// dti_enum_10: TWO modules plain-import the same enum, one of them also under
// its own second import. Neither unit's view of `Color` may leak into the
// other's, which is what a build-wide alias table could not express.
#[test]
fn dti_enum_10_two_importing_modules_do_not_cross_talk() {
    let gamma = r#"
module gamma;
import alpha::Color;
pub fn weight(c: Color) -> i64 {
    return match c {
        Color::A => 100,
        Color::B => 200,
        Color::C => 300,
    };
}
pub fn heaviest() -> Color { return Color::C; }
"#;
    let main = r#"
import beta;
import gamma;
fn main() {
    println(gamma::weight(beta::nth(0)));
    println(beta::code(gamma::heaviest()));
    println(gamma::weight(gamma::heaviest()));
}
"#;
    let (out, ok) = compile_temp_project_and_run(
        &[
            ("alpha.wi", FAVJ_ALPHA),
            ("beta.wi", FAVJ_BETA),
            ("gamma.wi", gamma),
            ("main.wi", main),
        ],
        "main.wi",
    );
    assert!(ok, "two importing modules cross-talked: {out}");
    assert_eq!(out, "100\n3\n300\n");
}

// P11, P19: a module function boxes a local class to the module's OWN interface
// and dispatches internally; the entry only calls the function. Exercises
// module-body interface boxing under the alias scope.
#[test]
fn dti_iface_08_module_internal_interface_boxing() {
    let shapes = r#"
module shapes;
pub interface Area {
    fn area(self) -> i64;
}
pub class Square implements Area {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
fn measure(a: Area) -> i64 { return a.area(); }
pub fn run() -> i64 {
    let a: Area = new Square(9);
    return measure(a);
}
"#;
    let main = r#"
import shapes::run;
fn main() {
    println(run());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(ok, "module-internal interface boxing failed: {out}");
    assert_eq!(out, "81\n");
}

// P20: end-to-end — direct interface dispatch from the entry AND a module
// function that uses its own enum internally, in one project.
#[test]
fn dti_combined_01_iface_dispatch_plus_internal_enum() {
    let shapes = r#"
module shapes;
pub interface Drawable {
    fn area(self) -> i64;
}
pub class Square implements Drawable {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
pub enum Color { Red, Green, Blue }
pub fn rank(c: Color) -> i64 {
    return match c {
        Color::Red => 1,
        Color::Green => 2,
        Color::Blue => 3,
    };
}
pub fn brightest() -> i64 { return rank(Color::Blue); }
"#;
    let main = r#"
import shapes::Drawable;
import shapes::Square;
import shapes::brightest;
fn total(a: Drawable) -> i64 { return a.area(); }
fn main() {
    println(total(new Square(5)));
    println(brightest());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", shapes), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "combined direct-import iface + internal enum failed: {out}"
    );
    assert_eq!(out, "25\n3\n");
}
