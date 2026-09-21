use super::*;

// ── User module declarations (willow-y0o, spec 4.1 / 8 / 20) ───────────────

// Perspective 1: a module declaration is accepted and the program runs (the
// declaration is otherwise inert for an entry file).
#[test]
fn test_module_decl_entry_compiles() {
    let (out, ok) = compile_and_run(
        r#"
module myapp;
fn main() { println(7); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// Perspective 2: `::`-separated module paths are accepted on the entry file.
#[test]
fn test_module_decl_colon_entry_compiles() {
    let (out, ok) = compile_and_run(
        r#"
module myapp::tools;
fn main() { println(8); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "8\n");
}

// Perspective 3: `module std...` is rejected (reserved namespace).
#[test]
fn test_module_decl_std_rejected() {
    assert_compile_error_contains(
        "module std::io;\nfn main() {}\n",
        &["error[E2010]", "reserved namespace"],
    );
}

// Perspective 4: a module declaration after an item is rejected.
#[test]
fn test_module_decl_after_item_rejected() {
    assert_compile_error_contains(
        "fn helper() {}\nmodule myapp;\nfn main() {}\n",
        &["error[E2008]", "must appear before imports and items"],
    );
}

// Perspective 5: a duplicate module declaration is rejected.
#[test]
fn test_module_decl_duplicate_rejected() {
    assert_compile_error_contains(
        "module a;\nmodule b;\nfn main() {}\n",
        &["error[E2009]", "duplicate module declaration"],
    );
}

// Perspective 6: programs without a module declaration still compile.
#[test]
fn test_no_module_decl_backward_compatible() {
    let (out, ok) = compile_and_run(r#"fn main() { println(1); }"#);
    assert!(ok);
    assert_eq!(out, "1\n");
}

// Perspective 7: an imported file whose declared module matches the import path
// resolves and runs.
#[test]
fn test_imported_module_matching_decl_runs() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math::add(2, 3)); }\n",
            ),
            (
                "math.wi",
                "module math;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

// Perspective 8: an imported file whose declared module does not match the
// import path is an error (E2011).
#[test]
fn test_imported_module_mismatched_decl_errors() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math::add(2, 3)); }\n",
            ),
            (
                "math.wi",
                "module other;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2011]"), "stderr: {stderr}");
    assert!(
        stderr.contains("does not match import path"),
        "stderr: {stderr}"
    );
}

// Perspective 9: an imported file with no module declaration still resolves
// (identity derived from the path — backward compatible).
#[test]
fn test_imported_module_no_decl_backward_compatible() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math::add(4, 5)); }\n",
            ),
            (
                "math.wi",
                "pub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "9\n");
}

// Perspective 10: a nested module path matches a declared nested module.
#[test]
fn test_nested_imported_module_matching_decl_runs() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import foo::bar;\nfn main() { println(bar::val()); }\n",
            ),
            (
                "foo/bar.wi",
                "module foo::bar;\npub fn val() -> i64 { return 77; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "77\n");
}

// Perspective 11: a nested module with a mismatched declaration is an error.
#[test]
fn test_nested_imported_module_mismatch_errors() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import foo::bar;\nfn main() { println(bar::val()); }\n",
            ),
            (
                "foo/bar.wi",
                "module foo::baz;\npub fn val() -> i64 { return 1; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2011]"), "stderr: {stderr}");
}

// Perspective 1: a directly imported function is callable unqualified.
#[test]
fn test_item_import_function_call() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::add;\nfn main() { println(add(2, 3)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

// Perspective 2: an item import with an alias.
#[test]
fn test_item_import_alias() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::add as plus;\nfn main() { println(plus(10, 20)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "30\n");
}

// Perspective 3: two item imports from the same module.
#[test]
fn test_item_import_two_items() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::add;\nimport math::mul;\nfn main() { println(add(2, 3)); println(mul(2, 3)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "5\n6\n");
}

// Perspective 4: importing a private item is rejected.
#[test]
fn test_item_import_private_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math::secret;\nfn main() { println(secret()); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2006]"), "stderr: {stderr}");
    assert!(stderr.contains("private"), "stderr: {stderr}");
}

// Perspective 5: importing a non-existent item is rejected.
#[test]
fn test_item_import_missing_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            ("main.wi", "import math::nope;\nfn main() { println(1); }\n"),
            math_module(),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E2006]"), "stderr: {stderr}");
    assert!(stderr.contains("no item `nope`"), "stderr: {stderr}");
}

// Perspective 6: a module import still works alongside item imports.
#[test]
fn test_item_import_with_module_import() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math;\nimport math::add;\nfn main() { println(add(1, 1)); println(math::mul(2, 4)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "2\n8\n");
}

// Perspective 7: an item import without any plain `import math;` still loads
// the module (no explicit module import required).
#[test]
fn test_item_import_loads_module_implicitly() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::mul;\nfn main() { println(mul(6, 7)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 8: an item-imported function used inside a helper.
#[test]
fn test_item_import_used_in_helper() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::add;\nfn twice(n: i64) -> i64 { return add(n, n); }\nfn main() { println(twice(21)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 9: the item-imported function's result in an expression.
#[test]
fn test_item_import_result_in_expression() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::add;\nfn main() { println(add(3, 4) * 2); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "14\n");
}

// Perspective 10: a nested-module item import (`import foo::bar::baz;`).
#[test]
fn test_item_import_nested_module() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import foo::bar::baz;\nfn main() { println(baz()); }\n",
            ),
            (
                "foo/bar.wi",
                "module foo::bar;\npub fn baz() -> i64 { return 88; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "88\n");
}

// Perspective 11: two item imports + an alias together.
#[test]
fn test_item_import_mixed() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math::add;\nimport math::mul as times;\nfn main() { println(add(1, 2)); println(times(3, 4)); }\n",
            ),
            math_module(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "3\n12\n");
}

// ── validate_type rejects unknown/module type annotations (willow-a7j) ─────

// A module name used as a type is rejected.
#[test]
fn test_module_name_as_param_type_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import calc;\nfn f(x: calc) -> i64 { return 0; }\nfn main() { println(1); }\n",
            ),
            (
                "calc.wi",
                "module calc;\npub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E0350]"), "stderr: {stderr}");
    assert!(
        stderr.contains("is a module, not a type"),
        "stderr: {stderr}"
    );
}

// An undefined type name in a parameter is rejected.
#[test]
fn test_unknown_param_type_rejected() {
    assert_compile_error_contains(
        "fn f(x: Bogus) -> i64 { return 0; }\nfn main() {}\n",
        &["error[E0350]", "cannot find type `Bogus`"],
    );
}

// An undefined type name in a return position is rejected.
#[test]
fn test_unknown_return_type_rejected() {
    assert_compile_error_contains(
        "fn f() -> Nope { return 0; }\nfn main() {}\n",
        &["error[E0350]", "cannot find type `Nope`"],
    );
}

// An undefined type name in a let annotation is rejected.
#[test]
fn test_unknown_let_type_rejected() {
    assert_compile_error_contains(
        "fn main() { let x: Whatever = 1; println(1); }\n",
        &["error[E0350]", "cannot find type `Whatever`"],
    );
}

// An undefined type name in a class field is rejected.
#[test]
fn test_unknown_field_type_rejected() {
    assert_compile_error_contains(
        "class C { pub v: Ghost; }\nfn main() {}\n",
        &["error[E0350]", "cannot find type `Ghost`"],
    );
}

// Regression guard: a real class type is still accepted.
#[test]
fn test_known_class_type_accepted() {
    let (out, ok) = compile_and_run(
        r#"
class P {
    pub v: i64;
    pub static fn new(v: i64) -> P { return new P(v); }
    pub fn get(self) -> i64 { return self.v; }
}
fn use_p(p: P) -> i64 { return p.get(); }
fn main() { println(use_p(P::new(42))); }
"#,
    );
    assert!(ok, "a known class type must validate");
    assert_eq!(out, "42\n");
}

// Regression guard: enum types (Option/Result) are still accepted.
#[test]
fn test_known_enum_type_accepted() {
    let (out, ok) = compile_and_run(
        r#"
fn pick(x: Option<i64>) -> Result<i64, String> {
    return match x { Option::Some(v) => Result::Ok(v), Option::None => Result::Err("none"), };
}
fn main() {
    let r = pick(Option::Some(5));
    println(match r { Result::Ok(v) => v, Result::Err(_) => -1, });
}
"#,
    );
    assert!(ok, "Option/Result types must validate");
    assert_eq!(out, "5\n");
}

// Regression guard: a module-qualified class type annotation is accepted.
#[test]
fn test_module_qualified_class_type_accepted() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import geom;\nfn show(p: geom::Point) -> i64 { return p.getx(); }\nfn main() { println(1); }\n",
            ),
            (
                "geom.wi",
                "module geom;\npub class Point {\n    pub x: i64;\n    pub fn getx(self) -> i64 { return self.x; }\n}\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "module-qualified class type must validate");
    assert_eq!(out, "1\n");
}

// Regression guard: a module-qualified class constructor parses, type-checks,
// links to the imported module's class method, and returns the qualified object.
#[test]
fn test_module_qualified_class_constructor_runs() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import geom;\nfn main() { let p = geom::Point::new(10, 32); println(p.sum()); }\n",
            ),
            (
                "geom.wi",
                "module geom;\npub class Point {\n    pub x: i64;\n    pub y: i64;\n    pub static fn new(x: i64, y: i64) -> Point { return new Point(x, y); }\n    pub fn sum(self) -> i64 { return self.x + self.y; }\n}\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "module-qualified class construction should run");
    assert_eq!(out, "42\n");
}

// Imported module bodies can still use their local class name while the entry
// module uses the qualified class name.
#[test]
fn test_module_class_body_can_call_local_constructor() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import geom;\nfn main() { println(geom::origin_sum()); }\n",
            ),
            (
                "geom.wi",
                "module geom;\npub class Point {\n    pub x: i64;\n    pub y: i64;\n    pub static fn new(x: i64, y: i64) -> Point { return new Point(x, y); }\n    pub fn sum(self) -> i64 { return self.x + self.y; }\n}\npub fn origin_sum() -> i64 { let p = Point::new(3, 4); return p.sum(); }\n",
            ),
        ],
        "main.wi",
    );
    assert!(
        ok,
        "module class methods should be available inside the module"
    );
    assert_eq!(out, "7\n");
}

#[test]
fn test_module_alias_class_constructor_uses_canonical_symbol() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import geom as g;\nfn main() { let p = g::Point::new(5, 6); println(p.sum()); }\n",
            ),
            (
                "geom.wi",
                "module geom;\npub class Point {\n    pub x: i64;\n    pub y: i64;\n    pub static fn new(x: i64, y: i64) -> Point { return new Point(x, y); }\n    pub fn sum(self) -> i64 { return self.x + self.y; }\n}\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "aliased module class construction should run");
    assert_eq!(out, "11\n");
}

#[test]
fn test_nested_item_imports_same_leaf_module_do_not_collide() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import left::math::value as left_value;\nimport right::math::value as right_value;\nfn main() { println(left_value()); println(right_value()); }\n",
            ),
            (
                "left/math.wi",
                "module left::math;\npub fn value() -> i64 { return 11; }\n",
            ),
            (
                "right/math.wi",
                "module right::math;\npub fn value() -> i64 { return 22; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(
        ok,
        "canonical module symbol names should avoid leaf-name collisions"
    );
    assert_eq!(out, "11\n22\n");
}

// A module imported under an alias is accessed with `alias::item`.
#[test]
fn test_module_alias_qualified_call() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math as m;\nfn main() { println(m::add(2, 3)); println(m::square(4)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "5\n16\n");
}

// The plain `module::item` form still works without an alias.
#[test]
fn test_module_qualified_call_no_alias() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math::add(10, 20)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "30\n");
}

// Accessing a module item with `.` is an error that points at `::`.
#[test]
fn test_module_dot_access_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math;\nfn main() { println(math.add(1, 2)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E0350]"), "stderr: {stderr}");
    assert!(stderr.contains("is a module; use `::`"), "stderr: {stderr}");
}

// `.` on an aliased module is likewise rejected.
#[test]
fn test_module_alias_dot_access_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math as m;\nfn main() { println(m.add(1, 2)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    assert!(stderr.contains("error[E0350]"), "stderr: {stderr}");
}

// After aliasing, the original module name is not in scope.
#[test]
fn test_module_alias_hides_original_name() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "main.wi",
                "import math as m;\nfn main() { println(math::add(1, 2)); }\n",
            ),
            aliasable_math(),
        ],
        "main.wi",
    );
    // `math` is not a known module under the alias import.
    assert!(
        !stderr.is_empty(),
        "expected an error using the original name"
    );
}

// Instance `.` method/field access is unaffected by the module-dot rule.
#[test]
fn test_instance_dot_access_still_works() {
    let (out, ok) = compile_and_run(
        r#"
class P {
    pub v: i64;
    pub static fn new(v: i64) -> P { return new P(v); }
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let p = P::new(9);
    println(p.get());
    println(p.v);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n9\n");
}

// Importing a private (non-pub) item is rejected.
#[test]
fn test_import_private_item_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a::hidden;\nfn main() { println(hidden()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2006]"), "stderr: {stderr}");
    assert!(stderr.contains("private"), "stderr: {stderr}");
}

// Two item imports binding the same local name collide.
#[test]
fn test_duplicate_item_import_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a::dup;\nimport b::dup;\nfn main() { println(dup()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2004]"), "stderr: {stderr}");
    assert!(
        stderr.contains("defined multiple times"),
        "stderr: {stderr}"
    );
}

// An item import colliding with a local function is rejected.
#[test]
fn test_item_import_vs_local_fn_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a::f;\nfn f() -> i64 { return 0; }\nfn main() { println(f()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2003]"), "stderr: {stderr}");
    assert!(
        stderr.contains("import and a local declaration"),
        "stderr: {stderr}"
    );
}

// An item import colliding with a local class is rejected.
#[test]
fn test_item_import_vs_local_class_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a::f;\nclass f { pub v: i64; }\nfn main() {}\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2003]"), "stderr: {stderr}");
}

// Two module imports aliased to the same name collide.
#[test]
fn test_module_alias_collision_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a as x;\nimport b as x;\nfn main() { println(x::f()); }\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2004]"), "stderr: {stderr}");
}

// A module access-name colliding with a local declaration is rejected.
#[test]
fn test_module_name_vs_local_fn_rejected() {
    let stderr = compile_temp_project_error_stderr(
        &s5_project("import a;\nfn a() -> i64 { return 0; }\nfn main() {}\n"),
        "main.wi",
    );
    assert!(stderr.contains("error[E2003]"), "stderr: {stderr}");
}

// Distinct imports and declarations compile and run.
#[test]
fn test_distinct_imports_and_decls_ok() {
    let (out, ok) = compile_temp_project_and_run(
        &s5_project(
            "import a::f;\nimport b::g;\nfn helper() -> i64 { return 100; }\nfn main() { println(f() + g() + helper()); }\n",
        ),
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "103\n");
}

// An alias disambiguates two otherwise-colliding item imports.
#[test]
fn test_alias_disambiguates_duplicate_item() {
    let (out, ok) = compile_temp_project_and_run(
        &s5_project(
            "import a::dup;\nimport b::dup as bdup;\nfn main() { println(dup() + bdup()); }\n",
        ),
        "main.wi",
    );
    assert!(ok);
    assert_eq!(out, "30\n");
}
