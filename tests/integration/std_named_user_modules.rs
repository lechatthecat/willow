//! A project module named like a std function module (`text`, `time`, ...).
//!
//! Every file sees the builtin function modules, but they declare no types, so
//! a qualified `text::Edit` must resolve to the user's module only where that
//! module is imported, and report the missing import everywhere else.
use super::support::*;

const FUNCTION_MODULES: [&str; 7] = ["text", "time", "env", "fs", "path", "net", "parallel"];

/// The declaring module, a file that re-exports a value of its enum, and a
/// file that matches on it with `user_head` in front.
fn project(module: &str, user_head: &str, user_body: &str) -> TestProject {
    let decl = "import std::collections::Array;\n\
        pub enum Edit { Keep(i64), Del }\n\
        pub fn one() -> Edit { return Edit::Keep(1); }\n\
        pub interface Animal { fn name(self) -> String; }\n\
        pub class Dog implements Animal { pub fn name(self) -> String { return \"d\"; } }\n\
        pub fn pet() -> Animal { return new Dog(); }\n";
    let other = format!(
        "import {module}::{{Edit, one, Animal, pet}};\n\
         pub fn get() -> Edit {{ return one(); }}\n\
         pub fn animal() -> Animal {{ return pet(); }}\n"
    );
    let user = format!("import other::{{get, animal}};\n{user_head}\n{user_body}\n");
    TestProject::new(
        "std_named_module",
        &[
            (&format!("{module}.wi"), decl),
            ("other.wi", &other),
            ("user.wi", &user),
            ("main.wi", "import user::f;\nfn main() { println(f()); }\n"),
        ],
    )
}

fn check(project: &TestProject) -> (bool, String) {
    let output = project.check("main.wi");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn error_codes(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix("error["))
        .filter_map(|rest| rest.split(']').next())
        .collect()
}

fn assert_only_missing_import(stderr: &str, module: &str, ty: &str) {
    assert_eq!(error_codes(stderr), ["E0350"], "{stderr}");
    assert!(
        stderr.contains(&format!(
            "cannot name type `{module}::{ty}`: module `{module}` is not imported"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("import `{module}` in this file")),
        "{stderr}"
    );
}

const TUPLE_MATCH: &str =
    "pub fn f() -> i64 { return match get() { M::Edit::Keep(k) => k, _ => 0 }; }";

#[test]
fn qualified_enum_pattern_in_non_main_file_reports_missing_import_for_every_function_module() {
    for module in FUNCTION_MODULES {
        let body = TUPLE_MATCH.replace("M::", &format!("{module}::"));
        let (ok, stderr) = check(&project(module, "", &body));
        assert!(!ok, "{module}: {stderr}");
        assert_only_missing_import(&stderr, module, "Edit");
    }
}

#[test]
fn qualified_enum_pattern_resolves_to_imported_user_module_for_every_function_module() {
    for module in FUNCTION_MODULES {
        let body = TUPLE_MATCH.replace("M::", &format!("{module}::"));
        let (ok, stderr) = check(&project(module, &format!("import {module};"), &body));
        assert!(ok, "{module}: {stderr}");
    }
}

#[test]
fn qualified_enum_pattern_in_main_file_reports_missing_import() {
    let project = TestProject::new(
        "std_named_module_main",
        &[
            (
                "text.wi",
                "pub enum Edit { Keep(i64), Del }\npub fn one() -> Edit { return Edit::Keep(1); }\n",
            ),
            (
                "other.wi",
                "import text::{Edit, one};\npub fn get() -> Edit { return one(); }\n",
            ),
            (
                "main.wi",
                "import other::get;\nfn main() { println(match get() { text::Edit::Keep(k) => k, _ => 0 }); }\n",
            ),
        ],
    );
    let (ok, stderr) = check(&project);
    assert!(!ok, "{stderr}");
    assert_only_missing_import(&stderr, "text", "Edit");
}

#[test]
fn qualified_unit_variant_pattern_reports_missing_import() {
    let body = "pub fn f() -> i64 { return match get() { text::Edit::Del => 1, _ => 0 }; }";
    let (ok, stderr) = check(&project("text", "", body));
    assert!(!ok, "{stderr}");
    assert_only_missing_import(&stderr, "text", "Edit");
}

#[test]
fn qualified_enum_pattern_through_ordinary_module_has_no_cascade() {
    let body = TUPLE_MATCH.replace("M::", "shape::");
    let (ok, stderr) = check(&project("shape", "", &body));
    assert!(!ok, "{stderr}");
    assert_only_missing_import(&stderr, "shape", "Edit");
}

#[test]
fn qualified_class_pattern_without_import_reports_missing_import() {
    for module in ["text", "shape"] {
        let body = format!(
            "pub fn f() -> String {{ return match animal() {{ {module}::Dog(d) => d.name(), _ => \"x\" }}; }}"
        );
        let (ok, stderr) = check(&project(module, "", &body));
        assert!(!ok, "{module}: {stderr}");
        assert_only_missing_import(&stderr, module, "Dog");
    }
}

#[test]
fn qualified_class_pattern_with_import_says_to_name_the_class_bare() {
    for module in ["text", "shape"] {
        let body = format!(
            "pub fn f() -> String {{ return match animal() {{ {module}::Dog(d) => d.name(), _ => \"x\" }}; }}"
        );
        let (ok, stderr) = check(&project(module, &format!("import {module};"), &body));
        assert!(!ok, "{module}: {stderr}");
        assert_eq!(error_codes(&stderr), ["E1205"], "{module}: {stderr}");
        assert!(
            stderr.contains(&format!(
                "class pattern `{module}::Dog(..)` cannot be module-qualified"
            )),
            "{module}: {stderr}"
        );
        assert!(
            stderr.contains(&format!("import {module}::Dog;")),
            "{module}: {stderr}"
        );
        assert!(!stderr.contains("enum pattern"), "{module}: {stderr}");
    }
}

#[test]
fn bare_class_pattern_after_class_import_is_accepted() {
    let body = "pub fn f() -> String { return match animal() { Dog(d) => d.name(), _ => \"x\" }; }";
    let (ok, stderr) = check(&project("text", "import text::Dog;", body));
    assert!(ok, "{stderr}");
}

#[test]
fn qualified_type_annotation_reports_missing_import() {
    let body = "pub fn f() -> i64 { let e: text::Edit = get(); return 0; }";
    let (ok, stderr) = check(&project("text", "", body));
    assert!(!ok, "{stderr}");
    assert_only_missing_import(&stderr, "text", "Edit");
}

#[test]
fn item_import_of_user_enum_named_like_function_module_is_accepted() {
    let body = "pub fn f() -> i64 { return match get() { Edit::Keep(k) => k, Edit::Del => 0 }; }";
    let (ok, stderr) = check(&project("text", "import text::Edit;", body));
    assert!(ok, "{stderr}");
}

#[test]
fn builtin_function_module_call_still_needs_no_user_module() {
    let project = TestProject::new(
        "std_function_module_call",
        &[(
            "main.wi",
            "import std::text;\nfn main() { println(text::from_scalar(65)); }\n",
        )],
    );
    let (ok, stderr) = check(&project);
    assert!(ok, "{stderr}");
}

#[test]
fn imported_user_module_runs_qualified_patterns() {
    let body = "pub fn f() -> i64 { return match get() { text::Edit::Keep(k) => k + 41, text::Edit::Del => 0 }; }";
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "text.wi",
                "pub enum Edit { Keep(i64), Del }\npub fn one() -> Edit { return Edit::Keep(1); }\n",
            ),
            (
                "other.wi",
                "import text::{Edit, one};\npub fn get() -> Edit { return one(); }\n",
            ),
            (
                "user.wi",
                &format!("import other::get;\nimport text;\n{body}\n"),
            ),
            ("main.wi", "import user::f;\nfn main() { println(f()); }\n"),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}
