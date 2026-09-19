//! A module-qualified base class keeps its module in lowering (willow-ejg5).
//!
//! `src/ir/lower.rs` types member accesses from a table of the unit's own
//! classes, walking each class's base chain for inherited members. That table
//! recorded a base by the LAST segment of its path, so `class Sized extends
//! shapes::Sized` read as `Sized extends Sized`, and the walk had no guard
//! against a repeated class. Two failures followed:
//!
//!   * reading an inherited field through the local `Sized` spun the compiler
//!     forever at 100% CPU (the local class was its own base and never had
//!     the field);
//!   * with an unrelated local class named `Sized` beside `class Cube extends
//!     shapes::Sized`, `cube.width` took the LOCAL `Sized`'s field type and
//!     codegen aborted with E0800 ("the field `width` on a `Cube` ... has
//!     incompatible operands").
//!
//! The base now keeps its qualifier -- a qualified base is not in the
//! unit-local table, so the walk stops there and the checker tables supply the
//! type -- and the walk stops at any repeated class.
//!
//! Perspectives (each runs the program and checks what it prints):
//!   1. inherited field read through a same-named local subclass (the hang)
//!   2. inherited field written through it
//!   3. inherited method called through it
//!   4. an override in it dispatches through the module base type
//!   5. the module class constructed directly beside the local one
//!   6. own and inherited fields read in one expression
//!   7. two levels below the module root
//!   8. the inherited field read inside the local class's own method body
//!   9. `new` takes the module base's fields first
//!  10. an unrelated same-named local class beside a subclass of the module
//!      class: the inherited field has the MODULE class's type (the ICE)
//!  11. same, for an inherited method's return type
//!  12. the unrelated local class's own member keeps its own type
//!  13. an inherited static property through the qualified base
//!  14. a static property read through a same-named local subclass
//!  15. an element of a module-base-typed array reads the inherited field
//!  16. a lambda reads the inherited field through the local class
//!  17. a subclass declared BEFORE the same-named local base
//!  18. a module chain (`Rect extends Sized`) extended under a same-named
//!      local class
//!  19. an aliased direct import of the module class still works
//!  20. a local class extending its own short name is a genuine ring (E0426),
//!      module import or not
//!  21. the same-named local subclass builds in release mode
//!  22. the runnable example prints what it documents
//!
//! The walk's own guard has unit tests in `src/ir/lower.rs`.

use super::support::{
    TestProject, compile_file_and_run, compile_temp_project_and_run,
    compile_temp_project_error_stderr,
};

/// The module root every perspective extends as `shapes::Sized`.
const SHAPES: &str = r#"
pub open class Sized {
    pub width: i64;
    pub static mut made: i64 = 0;
    pub open fn area(self) -> i64 { return self.width; }
    pub fn widen(self, by: i64) { self.width = self.width + by; }
}
pub open class Rect extends Sized {
    pub height: i64;
    pub override fn area(self) -> i64 { return self.width * self.height; }
}
"#;

#[track_caller]
fn assert_runs(main: &str, expected: &str) {
    let (out, ok) =
        compile_temp_project_and_run(&[("shapes.wi", SHAPES), ("main.wi", main)], "main.wi");
    assert!(ok, "expected the project to compile");
    assert_eq!(out, expected);
}

// 1
#[test]
fn short_name_01_inherited_field_read_through_a_same_named_subclass() {
    assert_runs(
        "import shapes;\nclass Sized extends shapes::Sized { pub depth: i64; }\n\
         fn main() { let s = new Sized(3, 4); println(s.width); }\n",
        "3\n",
    );
}

// 2
#[test]
fn short_name_02_inherited_field_written_through_a_same_named_subclass() {
    assert_runs(
        "import shapes;\nclass Sized extends shapes::Sized { pub depth: i64; }\n\
         fn main() { let s = new Sized(3, 4); s.width = 10; println(s.width); }\n",
        "10\n",
    );
}

// 3
#[test]
fn short_name_03_inherited_method_called_through_a_same_named_subclass() {
    assert_runs(
        "import shapes;\nclass Sized extends shapes::Sized { pub depth: i64; }\n\
         fn main() { let s = new Sized(3, 4); s.widen(4); println(s.area()); }\n",
        "7\n",
    );
}

// 4
#[test]
fn short_name_04_override_dispatches_through_the_module_base_type() {
    assert_runs(
        "import shapes;\n\
         open class Sized extends shapes::Sized {\n\
             pub depth: i64;\n\
             pub override fn area(self) -> i64 { return self.width * self.depth; }\n\
         }\n\
         fn through_base(s: shapes::Sized) -> i64 { return s.area(); }\n\
         fn main() { println(through_base(new Sized(3, 4))); }\n",
        "12\n",
    );
}

// 5
#[test]
fn short_name_05_module_class_constructed_beside_the_local_one() {
    assert_runs(
        "import shapes;\nclass Sized extends shapes::Sized { pub depth: i64; }\n\
         fn main() { println(new shapes::Sized(7).width + new Sized(1, 2).width); }\n",
        "8\n",
    );
}

// 6
#[test]
fn short_name_06_own_and_inherited_fields_in_one_expression() {
    assert_runs(
        "import shapes;\nclass Sized extends shapes::Sized { pub depth: i64; }\n\
         fn main() { let s = new Sized(3, 4); println(s.width * 10 + s.depth); }\n",
        "34\n",
    );
}

// 7
#[test]
fn short_name_07_two_levels_below_the_module_root() {
    assert_runs(
        "import shapes;\nopen class Sized extends shapes::Sized { pub depth: i64; }\n\
         class Block extends Sized { pub tag: i64; }\n\
         fn main() { let b = new Block(2, 5, 9); println(b.width + b.depth + b.tag); }\n",
        "16\n",
    );
}

// 8
#[test]
fn short_name_08_inherited_field_read_inside_the_local_class_body() {
    assert_runs(
        "import shapes;\n\
         class Sized extends shapes::Sized {\n\
             pub depth: i64;\n\
             pub fn footprint(self) -> i64 { return self.width + self.depth; }\n\
         }\n\
         fn main() { println(new Sized(3, 4).footprint()); }\n",
        "7\n",
    );
}

// 9
#[test]
fn short_name_09_new_takes_the_module_base_fields_first() {
    assert_runs(
        "import shapes;\nclass Sized extends shapes::Sized { pub depth: i64; }\n\
         fn main() { let s = new Sized(1, 2); println(s.width); println(s.depth); }\n",
        "1\n2\n",
    );
}

// 10: the ICE repro. `Cube`'s `width` is `shapes::Sized`'s `i64`; the local
// `Sized`'s `width: String` must not be consulted.
#[test]
fn short_name_10_unrelated_same_named_local_class_does_not_retype_the_field() {
    assert_runs(
        "import shapes;\nclass Sized { pub width: String; }\n\
         class Cube extends shapes::Sized { pub depth: i64; }\n\
         fn main() { let c = new Cube(3, 4); println(c.width + c.depth); println(new Sized(\"x\").width); }\n",
        "7\nx\n",
    );
}

// 11
#[test]
fn short_name_11_unrelated_same_named_local_class_does_not_retype_the_method() {
    assert_runs(
        "import shapes;\nclass Sized { pub fn area(self) -> String { return \"local\"; } }\n\
         class Cube extends shapes::Sized { pub depth: i64; }\n\
         fn main() { let c = new Cube(3, 4); println(c.area() + 1); println(new Sized().area()); }\n",
        "4\nlocal\n",
    );
}

// 12
#[test]
fn short_name_12_unrelated_same_named_local_class_keeps_its_own_types() {
    assert_runs(
        "import shapes;\nclass Sized { pub width: String; pub fn area(self) -> String { return self.width + \"!\"; } }\n\
         class Cube extends shapes::Sized { pub depth: i64; }\n\
         fn main() { let s = new Sized(\"w\"); println(s.width); println(s.area()); println(new Cube(1, 2).area()); }\n",
        "w\nw!\n1\n",
    );
}

// 13
#[test]
fn short_name_13_inherited_static_property_through_the_qualified_base() {
    assert_runs(
        "import shapes;\nclass Cube extends shapes::Sized { pub depth: i64; }\n\
         fn main() { println(Cube::made); shapes::Sized::made = 5; println(Cube::made); }\n",
        "0\n5\n",
    );
}

// 14
#[test]
fn short_name_14_static_property_through_a_same_named_subclass() {
    assert_runs(
        "import shapes;\nclass Sized extends shapes::Sized { pub depth: i64; }\n\
         fn main() { Sized::made = 3; println(shapes::Sized::made); println(Sized::made); }\n",
        "3\n3\n",
    );
}

// 15
#[test]
fn short_name_15_module_base_typed_array_element_reads_the_inherited_field() {
    assert_runs(
        "import shapes;\nimport std::collections::Array;\n\
         class Sized extends shapes::Sized { pub depth: i64; }\n\
         fn main() {\n\
             let items: Array<shapes::Sized> = [new Sized(2, 3), new shapes::Sized(5)];\n\
             println(items[0].width + items[1].width);\n\
         }\n",
        "7\n",
    );
}

// 16
#[test]
fn short_name_16_lambda_reads_the_inherited_field() {
    assert_runs(
        "import shapes;\nclass Sized extends shapes::Sized { pub depth: i64; }\n\
         fn main() { let s = new Sized(6, 1); let read = || { return s.width; }; println(read()); }\n",
        "6\n",
    );
}

// 17
#[test]
fn short_name_17_subclass_declared_before_the_same_named_local_base() {
    assert_runs(
        "import shapes;\nclass Block extends Sized { pub tag: i64; }\n\
         open class Sized extends shapes::Sized { pub depth: i64; }\n\
         fn main() { let b = new Block(2, 5, 9); println(b.width); println(b.tag); }\n",
        "2\n9\n",
    );
}

// 18
#[test]
fn short_name_18_module_chain_extended_under_a_same_named_local_class() {
    assert_runs(
        "import shapes;\nclass Rect extends shapes::Rect { pub depth: i64; }\n\
         fn main() { let r = new Rect(2, 3, 4); println(r.width); println(r.height); println(r.area()); }\n",
        "2\n3\n6\n",
    );
}

// 19: the spelling that never looped, so it must keep working.
#[test]
fn short_name_19_aliased_direct_import_still_works() {
    assert_runs(
        "import shapes::Sized as Base;\nclass Sized extends Base { pub depth: i64; }\n\
         fn main() { let s = new Sized(3, 4); println(s.width + s.depth); println(new Base(9).width); }\n",
        "7\n9\n",
    );
}

// 20: with the module imported, `Sized extends Sized` still names the LOCAL
// class on both sides, so it is a ring and stays a compile error.
#[test]
fn short_name_20_extending_the_own_short_name_is_still_a_ring() {
    let stderr = compile_temp_project_error_stderr(
        &[
            ("shapes.wi", SHAPES),
            (
                "main.wi",
                "import shapes;\nopen class Sized extends Sized { pub depth: i64; }\n\
                 fn main() { println(new shapes::Sized(1).width); }\n",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E0426]"), "{stderr}");
    assert!(!stderr.contains("E0800"), "{stderr}");
    assert!(stderr.contains("aborting due to 1 error"), "{stderr}");
}

// 21
#[test]
fn short_name_21_same_named_subclass_builds_in_release_mode() {
    let project = TestProject::new(
        "short_name_release",
        &[
            ("shapes.wi", SHAPES),
            (
                "main.wi",
                "import shapes;\nclass Sized extends shapes::Sized { pub depth: i64; }\n\
                 fn main() { let s = new Sized(5, 6); println(s.width * s.depth); }\n",
            ),
        ],
    );
    let compiled = project.compile_release("main.wi");
    assert!(
        compiled.status.success(),
        "release compile failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let run = project.run();
    assert!(run.status.success(), "binary failed");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "30\n");
}

// 22: the exact output is also pinned by the runnable-examples table in
// `runtime.rs`.
#[test]
fn short_name_22_the_example_program_runs() {
    let (out, ok) = compile_file_and_run("example/module_base_short_name/main.wi");
    assert!(ok, "example failed: {out}");
    assert_eq!(out, "3\n12\n7\n5\n20\n50\n3\n12\n42\n1\n0\n");
}
