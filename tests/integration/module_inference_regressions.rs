//! Package identities, imported contracts, and inferred match values (willow-5lz8).
use super::support::*;

const MANIFEST: &str =
    "[project]\nname='module-regressions'\nversion='0.1.0'\n[willow]\nmanifest-version=1\n";

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn package_lambda_perspectives_and_linear_symbol_count() {
    // 1 capture-free, 2 capture, 3 nested capture, 4 method, 5 returned closure,
    // 6 two modules, 7 entry lambda, 8 increasing independent declarations.
    for count in [1, 8, 32] {
        let mut calc = String::from(
            r#"
pub fn free(n: i64) -> i64 { let f = |x: i64| x + 1; return f(n); }
pub fn capture(n: i64) -> i64 { let f = |x: i64| x + n; return f(1); }
pub fn nested(n: i64) -> i64 {
    let f = |x: i64| { let g = |y: i64| y + x + n; return g(1); };
    return f(1);
}
pub class Calc { pub fn call(self) -> i64 { let f = |x: i64| x + 2; return f(40); } }
pub fn returned(n: i64) -> closure(i64) -> i64 { return |x: i64| x + n; }
"#,
        );
        for i in 0..count {
            calc.push_str(&format!(
                "pub fn f{i}() -> i64 {{ let f = |x: i64| x + {i}; return f(0); }}\n"
            ));
        }
        let main = format!(
            r#"
import calc;
import other;
fn main() {{
    println(calc::free(41)); println(calc::capture(41)); println(calc::nested(40));
    println(new calc::Calc().call());
    let f = calc::returned(40); println(f(2));
    println(other::go()); let entry = |x: i64| x; println(entry(42));
    println(calc::f{}());
}}
"#,
            count - 1
        );
        let project = TestProject::new(
            "package_lambdas",
            &[
                ("project.toml", MANIFEST),
                ("src/main.wi", &main),
                ("src/calc.wi", &calc),
                (
                    "src/other.wi",
                    "pub fn go() -> i64 { let f = |x: i64| x * 2; return f(21); }",
                ),
            ],
        );
        assert_success(&project.package_command("check"));
        let build = project.package_command("build");
        assert_success(&build);
        let output = project.run();
        assert_success(&output);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("42\n42\n42\n42\n42\n42\n42\n{}\n", count - 1)
        );
        let log = String::from_utf8_lossy(&build.stderr);
        let lambdas = log
            .lines()
            .filter(|line| line.contains("[lir] compiling") && line.contains("$lambda."))
            .count();
        assert_eq!(lambdas, count + 8, "{log}");
        eprintln!(
            "package lambdas: declarations={} emitted={lambdas}",
            count + 8
        );
    }
}

#[test]
fn imported_interface_contracts_and_enum_arguments() {
    // 9 direct import, 10 module alias, 11 item alias; each with
    // 12 Send, 13 Sync, 14 inherited Send; enum appears in param and return.
    for bound in ["Send", "Sync", "Base"] {
        for (imports, interface, ty, value) in [
            (
                "import iface::Source; import expr::Expr;",
                "Source",
                "Expr",
                "Expr::Value(42)",
            ),
            (
                "import iface as api; import expr as ast;",
                "api::Source",
                "ast::Expr",
                "ast::Expr::Value(42)",
            ),
            (
                "import iface::Source as Read; import expr::Expr as Node;",
                "Read",
                "Node",
                "Node::Value(42)",
            ),
        ] {
            let iface = format!(
                "pub interface Base extends Send {{}} pub interface Source<T> extends {bound} {{ fn get(self) -> T; fn echo(self, value: T) -> T; }}"
            );
            let main = format!(
                r#"{imports}
class Impl implements {interface}<{ty}> {{
    pub fn get(self) -> {ty} {{ return {value}; }}
    pub fn echo(self, value: {ty}) -> {ty} {{ return value; }}
}}
async fn main() {{ let s: {interface}<{ty}> = new Impl(); await yield();
    println(match s.echo(s.get()) {{ Value(n) => n }});
}}"#
            );
            // A namespace-only enum import requires a qualified pattern.
            let main = if ty.contains("::") {
                main.replace("Value(n)", "ast::Expr::Value(n)")
            } else {
                main
            };
            let project = TestProject::new(
                "imported_contract",
                &[
                    ("project.toml", MANIFEST),
                    ("src/main.wi", &main),
                    ("src/iface.wi", &iface),
                    ("src/expr.wi", "pub enum Expr { Value(i64) }"),
                ],
            );
            assert_success(&project.package_command("check"));
            assert_success(&project.package_command("build"));
            let output = project.run();
            assert_success(&output);
            assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n");
        }
    }
}

#[test]
fn inferred_result_match_perspectives() {
    // 15 Ok-first, 16 Err-first, 17 qualified/bare, 18 nested match,
    // 19 nested ternary, 20 all-Ok, 21 scope shadowing, 22 failed arm errors.
    for (arms, expected) in [
        ("true => Ok(42), false => Err(\"bad\")", "42\n"),
        ("true => Err(\"bad\"), false => Ok(42)", "3\n"),
        ("true => Result::Ok(42), false => Err(\"bad\")", "42\n"),
        (
            "true => std::result::Result::Ok(42), false => Err(\"bad\")",
            "42\n",
        ),
        ("true => res::Result::Ok(42), false => Err(\"bad\")", "42\n"),
        ("true => Ok(42), false => Result::Err(\"bad\")", "42\n"),
        (
            "true => (match false { true => Ok(1), false => Ok(42) }), false => Err(\"bad\")",
            "42\n",
        ),
        (
            "true => (false ? Ok(1) : Ok(42)), false => Err(\"bad\")",
            "42\n",
        ),
    ] {
        let source = format!(
            "import std::result as res; fn main() {{ let r = match true {{ {arms} }}; println(match r {{ Ok(n) => n, Err(e) => e.len() }}); }}"
        );
        let (out, ok) = compile_and_run(&source);
        assert!(ok, "{arms}: {out}");
        assert_eq!(out, expected);
    }
    for expression in [
        "true ? Ok(42) : Err(\"bad\")",
        "match true { true => Ok(42), false => Ok(0) }",
    ] {
        let source = format!(
            "fn main() {{ let r = {expression}; println(match r {{ Ok(n) => n, Err(e) => 0 }}); }}"
        );
        let (out, ok) = compile_and_run(&source);
        assert!(ok, "{out}");
        assert_eq!(out, "42\n");
    }
    let (out, ok) = compile_and_run(
        "fn Ok(x: i64) -> i64 { return x + 1; } fn main() { let r = match true { true => Ok(41), false => Ok(0) }; println(r); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn recovery_does_not_leak_internal_types_or_hide_independent_errors() {
    for source in [
        "fn main() { let r = missing(); println(match r { Ok(n) => n, Err(e) => 0 }); }",
        "fn main() { let r = match true { true => Ok(missing()), false => Err(\"bad\") }; println(match r { Ok(n) => n, Err(e) => 0 }); }",
    ] {
        let errors = compile_error_stderr(source);
        assert!(
            errors.contains("cannot find function `missing`"),
            "{errors}"
        );
        assert_eq!(errors.matches("error[").count(), 1, "{errors}");
        for leaked in ["diagnostic-error", "`T`", "`E`", "E0800"] {
            assert!(!errors.contains(leaked), "{errors}");
        }
    }
    let errors = compile_error_stderr(
        "fn main() { let r = missing(); match r { Ok(n) => other(), Err(e) => 0 }; }",
    );
    assert!(
        errors.contains("function `missing`") && errors.contains("function `other`"),
        "{errors}"
    );
}

#[test]
fn imported_contract_and_nominal_identity_negative_controls() {
    // 23 absent Send remains rejected; 24 identically-shaped enums stay distinct.
    let errors = compile_temp_project_error_stderr(
        &[
            ("iface.wi", "pub interface Source { fn get(self) -> i64; }"),
            (
                "main.wi",
                "import iface::Source; class Impl implements Source { pub fn get(self) -> i64 { return 1; } } async fn main() { let s: Source = new Impl(); await yield(); println(s.get()); }",
            ),
        ],
        "main.wi",
    );
    assert!(errors.contains("E2402"), "{errors}");
    let errors = compile_temp_project_error_stderr(
        &[
            ("expr.wi", "pub enum Expr { Value(i64) }"),
            ("other.wi", "pub enum Expr { Value(i64) }"),
            (
                "main.wi",
                "import expr; import other; interface Source<T> { fn get(self) -> T; } class Impl implements Source<expr::Expr> { pub fn get(self) -> other::Expr { return other::Expr::Value(1); } } fn main() {}",
            ),
        ],
        "main.wi",
    );
    assert!(errors.contains("E0417"), "{errors}");
}

#[test]
fn runnable_module_inference_example() {
    let project = TestProject::new(
        "module_inference_example",
        &[
            (
                "project.toml",
                include_str!("../../example/module_inference/project.toml"),
            ),
            (
                "src/main.wi",
                include_str!("../../example/module_inference/src/main.wi"),
            ),
            (
                "src/calc.wi",
                include_str!("../../example/module_inference/src/calc.wi"),
            ),
            (
                "src/expr.wi",
                include_str!("../../example/module_inference/src/expr.wi"),
            ),
            (
                "src/source.wi",
                include_str!("../../example/module_inference/src/source.wi"),
            ),
        ],
    );
    assert_success(&project.package_command("check"));
    assert_success(&project.package_command("build"));
    let output = project.run();
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n42\n42\n");
}

#[test]
fn nested_generic_enum_argument_and_transitive_contract() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            ("expr.wi", "pub enum Expr { Value(i64) }"),
            ("base.wi", "pub interface Safe extends Send {}"),
            (
                "iface.wi",
                "import base::Safe as Bound; pub interface Source<T> extends Bound { fn get(self) -> T; }",
            ),
            (
                "main.wi",
                "import expr::Expr; import iface::Source; class Impl implements Source<Option<Expr>> { pub fn get(self) -> Option<Expr> { return Some(Expr::Value(42)); } } async fn main() { let s: Source<Option<Expr>> = new Impl(); await yield(); let n = match s.get() { Some(e) => (match e { Value(n) => n }), None => 0 }; println(n); }",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn explicit_transitive_type_still_requires_its_own_import() {
    let errors = compile_temp_project_error_stderr(
        &[
            ("base.wi", "pub interface Safe extends Send {}"),
            (
                "iface.wi",
                "import base::Safe as Bound; pub interface Source extends Bound {}",
            ),
            (
                "main.wi",
                "import iface::Source; class Impl implements Source, base::Safe {} fn main() {}",
            ),
        ],
        "main.wi",
    );
    assert!(errors.contains("module `base` is not imported"), "{errors}");
}

#[test]
fn inferred_result_rejects_conflicting_payloads_and_shadowed_values() {
    for source in [
        "fn main() { let r = match true { true => Ok(1), false => Ok(true) }; }",
        "fn main() { let r = match true { true => Err(1), false => Err(true) }; }",
    ] {
        let errors = compile_error_stderr(source);
        assert!(errors.contains("E1201"), "{errors}");
    }
    let errors = compile_error_stderr(
        "fn main() { let Ok = 1; let r = match true { true => Ok(1), false => Ok(2) }; }",
    );
    assert!(errors.contains("cannot call value `Ok`"), "{errors}");
}

/// Module-qualified calls through a prefix the file only item-imported, in a
/// real `project.toml` layout where modules carry package identities
/// (willow-jz15.43). `willow check` and `willow build` must agree.
#[test]
fn project_item_import_keeps_module_prefix_callable() {
    const A: &str = "pub enum Op { Eq, Ne }\npub fn twice(n: i64) -> i64 { return n * 2; }\n\
pub fn name() -> String { return \"a\"; }\nfn secret() -> i64 { return 1; }\n\
pub class Box { pub v: i64; pub init(self, v: i64) { self.v = v; } \
pub fn get(self) -> i64 { return self.v; } pub static fn make() -> Box { return new Box(21); } }\n\
pub fn unbox(b: Box) -> i64 { return b.get() * 2; }\n";
    const NESTED: &str = "pub enum Kind { K }\npub fn deep(n: i64) -> i64 { return n + 1; }\n";
    let main = |imports: &str, body: &str| format!("{imports}\nfn main() {{\n{body}\n}}\n");
    // (perspective, extra files, main source, expected stdout)
    type Case = (
        &'static str,
        Vec<(&'static str, &'static str)>,
        String,
        &'static str,
    );
    let cases: Vec<Case> = vec![
        ("1 single enum item", vec![], main("import a::Op;", "println(a::twice(21));"), "42\n"),
        ("2 braced item", vec![], main("import a::{Op};", "println(a::twice(21));"), "42\n"),
        ("3 braced fn and enum", vec![], main("import a::{Op, twice};", "println(a::twice(20) + twice(1));"), "42\n"),
        ("4 aliased item", vec![], main("import a::twice as tw;", "println(a::twice(tw(3)) + 30);"), "42\n"),
        ("5 class item", vec![], main("import a::Box;", "println(a::twice(new Box(21).get()));"), "42\n"),
        ("6 string result", vec![], main("import a::Op;", "println(a::name());"), "a\n"),
        ("7 module class argument", vec![], main("import a::Box;", "println(a::unbox(new Box(21)));"), "42\n"),
        ("8 qualified static method", vec![], main("import a::Op;", "println(a::Box::make().get() * 2);"), "42\n"),
        ("9 qualified variant", vec![], main("import a::Op;", "let o = a::Op::Ne;\nprintln(o == Op::Ne);"), "true\n"),
        ("10 qualified new", vec![], main("import a::Op;", "println(new a::Box(42).get());"), "42\n"),
        ("11 lambda body", vec![], main("import a::Op;", "let f = |x: i64| a::twice(x);\nprintln(f(21));"), "42\n"),
        ("12 module import too", vec![], main("import a;\nimport a::Op;", "println(a::twice(21));"), "42\n"),
        (
            "13 entry method",
            vec![],
            "import a::Op;\nclass C { pub fn d(self) -> i64 { return a::twice(21); } }\nfn main() { println(new C().d()); }\n".into(),
            "42\n",
        ),
        (
            "14 async main",
            vec![],
            "import a::Op;\nasync fn main() { await yield(); println(a::twice(21)); }\n".into(),
            "42\n",
        ),
        (
            "15 module unit function",
            vec![("src/c.wi", "import a::Op;\npub fn go() -> i64 { return a::twice(20); }\n")],
            main("import c::go;", "println(c::go() + go() - 38);"),
            "42\n",
        ),
        (
            "16 module unit method",
            vec![("src/c.wi", "import a::Op;\npub class F { pub fn d(self) -> i64 { return a::twice(21); } }\n")],
            main("import c::F;", "println(new F().d());"),
            "42\n",
        ),
        (
            "17 module aliased by another file",
            vec![("src/c.wi", "import a as z;\npub fn go() -> i64 { return z::twice(1); }\n")],
            main("import c::go;\nimport a::Op;", "println(a::twice(go()) + 38);"),
            "42\n",
        ),
        (
            "18 nested module",
            vec![("src/x/y.wi", NESTED)],
            main("import x::y::Kind;", "println(x::y::deep(41));"),
            "42\n",
        ),
        (
            "19 two item-imported modules",
            vec![("src/x/y.wi", NESTED)],
            main("import a::Op;\nimport x::y::Kind;", "println(a::twice(x::y::deep(20)));"),
            "42\n",
        ),
    ];
    for (perspective, extra, main_source, expected) in cases {
        let mut files = vec![
            ("project.toml", MANIFEST),
            ("src/main.wi", main_source.as_str()),
            ("src/a.wi", A),
        ];
        files.extend(extra);
        let project = TestProject::new("item_prefix", &files);
        let check = project.package_command("check");
        assert!(
            check.status.success(),
            "{perspective}: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        let build = project.package_command("build");
        assert!(
            build.status.success(),
            "{perspective}: {}",
            String::from_utf8_lossy(&build.stderr)
        );
        let output = project.run();
        assert!(
            output.status.success(),
            "{perspective}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected,
            "{perspective}"
        );
    }
    // Rejections stay ordinary diagnostics in both commands, never E0800:
    // 20 private item, 21 missing item, 22 the module's bare names stay unbound.
    for (perspective, body, needle) in [
        ("20 private", "println(a::secret());", "secret"),
        ("21 missing", "println(a::nope());", "nope"),
        ("22 bare name", "println(twice(21));", "twice"),
    ] {
        let project = TestProject::new(
            "item_prefix_reject",
            &[
                ("project.toml", MANIFEST),
                ("src/main.wi", &main("import a::Op;", body)),
                ("src/a.wi", A),
            ],
        );
        for command in ["check", "build"] {
            let output = project.package_command(command);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !output.status.success(),
                "{perspective} {command}: {stderr}"
            );
            assert!(
                stderr.contains(needle) && !stderr.contains("E0800"),
                "{perspective} {command}: {stderr}"
            );
        }
    }
}

#[test]
fn runnable_item_import_prefix_example() {
    let project = TestProject::new(
        "item_import_prefix_example",
        &[
            (
                "project.toml",
                include_str!("../../example/item_import_prefix/project.toml"),
            ),
            (
                "src/main.wi",
                include_str!("../../example/item_import_prefix/src/main.wi"),
            ),
            (
                "src/ops.wi",
                include_str!("../../example/item_import_prefix/src/ops.wi"),
            ),
            (
                "src/report/text.wi",
                include_str!("../../example/item_import_prefix/src/report/text.wi"),
            ),
        ],
    );
    assert_success(&project.package_command("check"));
    assert_success(&project.package_command("build"));
    let output = project.run();
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\nx != y\n==\n");
}
