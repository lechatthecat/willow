//! Imported Result error identity (willow-jz15.114).
//! 24 perspectives: class/enum × brace/single/alias import × entry header
//! present/absent × entry/helper caller. Every case exercises Ok and Err.
use super::support::*;

#[test]
fn imported_error_identity_matrix() {
    for kind in ["class", "enum"] {
        let declaration = if kind == "class" {
            "pub class E1 { pub msg: String; }"
        } else {
            "pub enum E1 { Bad(String), }"
        };
        let error = if kind == "class" {
            "new E1(\"neg\")"
        } else {
            "E1::Bad(\"neg\")"
        };
        let provider = format!(
            "module a; {declaration} pub fn get(x: i64) -> Result<i64, E1> {{ if x < 0 {{ return Err({error}); }} return Ok(x); }}"
        );
        for (style, imports, ty) in [
            ("brace", "import a::{E1, get};", "E1"),
            ("single", "import a::E1; import a::get;", "E1"),
            (
                "alias",
                "import a::E1 as Failure; import a::get;",
                "Failure",
            ),
        ] {
            for header in ["", "module main;"] {
                for helper in [false, true] {
                    let caller = format!(
                        "{imports} pub fn once(x: i64) -> Result<i64, {ty}> {{ let v = get(x)?; return Ok(v + 1); }}"
                    );
                    let inspect = if kind == "class" {
                        "e.msg"
                    } else {
                        "match e { Bad(msg) => msg }"
                    };
                    let main_body = format!(
                        "fn main() {{ println(match once(3) {{ Ok(v) => v, Err(e) => 0 }}); println(match once(-1) {{ Ok(v) => \"wrong\", Err(e) => {inspect} }}); }}"
                    );
                    let entry = if helper {
                        format!("{header} {imports} import helper::once; {main_body}")
                    } else {
                        format!("{header} {caller} {main_body}")
                    };
                    let helper_source = format!("module helper; {caller}");
                    let mut files = vec![("a.wi", provider.as_str()), ("main.wi", entry.as_str())];
                    if helper {
                        files.push(("helper.wi", helper_source.as_str()));
                    }
                    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &[]);
                    assert!(ok, "{kind}/{style}/header={header}/helper={helper}: {out}");
                    assert_eq!(out, "4\nneg\n");
                }
            }
        }
    }
}

#[test]
fn imported_error_identity_distinct_declarations_rejected() {
    for kind in ["class", "enum"] {
        let decl = if kind == "class" {
            "pub class E1 {}"
        } else {
            "pub enum E1 { Bad, }"
        };
        let value = if kind == "class" {
            "new E1()"
        } else {
            "E1::Bad"
        };
        let a =
            format!("module a; {decl} pub fn get() -> Result<i64, E1> {{ return Err({value}); }}");
        let b = format!("module b; {decl}");
        let stderr = compile_temp_project_error_stderr(
            &[
                ("a.wi", &a),
                ("b.wi", &b),
                (
                    "main.wi",
                    "import a::get; import b::E1; fn once() -> Result<i64, E1> { return Ok(get()?); } fn main() {}",
                ),
            ],
            "main.wi",
        );
        assert!(stderr.contains("E1805"), "{kind}: {stderr}");
    }
}

#[test]
fn imported_error_identity_example() {
    let (out, ok) = compile_temp_project_with_env_and_run(
        &[
            (
                "main.wi",
                include_str!("../../example/imported_error_identity/main.wi"),
            ),
            (
                "a.wi",
                include_str!("../../example/imported_error_identity/a.wi"),
            ),
        ],
        "main.wi",
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\nneg\n");
}

#[test]
fn imported_error_identity_does_not_allow_subtyping() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "a.wi",
                "module a; pub class Base {} pub class Child extends Base {} pub fn get() -> Result<i64, Child> { return Err(new Child()); }",
            ),
            (
                "main.wi",
                "import a::{Base, get}; fn once() -> Result<i64, Base> { return Ok(get()?); } fn main() {}",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("E1805"), "{stderr}");
}
