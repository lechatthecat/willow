//! willow-jz15.87: 24 positive perspectives (two callable kinds, two caller
//! nominal kinds, six import/caller/build shapes), capturing closure, primitive
//! controls, invalid signature rejection, example, and increasing-size symbols.
use super::support::TestProject;

const NOMINALS: &str = "interface N { fn hit(self) -> bool; }
class B implements N { pub fn hit(self) -> bool { return true; } }";

fn assert_build(project: &TestProject, release: bool, expected: &str) {
    let checked = project.check("main.wi");
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let built = if release {
        project.compile_release("main.wi")
    } else {
        project.compile("main.wi")
    };
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let run = project.run();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), expected);
}

#[test]
fn imported_generic_callable_caller_nominals() {
    for callable in ["fn", "closure"] {
        for nominal in ["B", "N"] {
            for shape in ["entry", "module", "alias", "qualified", "shadow", "release"] {
                let util = format!(
                    "pub fn call<T>(x: T, f: {callable}(T) -> bool) -> bool {{ return f(x); }}{}",
                    if shape == "shadow" { NOMINALS } else { "" }
                );
                let (import, call) = match shape {
                    "alias" => ("import util::call as apply;", "apply"),
                    "qualified" => ("import util;", "util::call"),
                    _ => ("import util::call;", "call"),
                };
                let caller = format!(
                    "{import}\n{NOMINALS}\npub fn run() {{ let v: {nominal} = new B(); println({call}(v, |x| x.hit())); }}"
                );
                let main = if shape == "module" {
                    "import caller; fn main() { caller::run(); }".into()
                } else {
                    format!("{caller}\nfn main() {{ run(); }}")
                };
                let project = TestProject::new(
                    "imported_generic_callable",
                    &[
                        ("util.wi", &util),
                        ("main.wi", &main),
                        ("caller.wi", &caller),
                    ],
                );
                eprintln!("perspective {callable}/{nominal}/{shape}");
                assert_build(&project, shape == "release", "true\n");
            }
        }
    }
}

#[test]
fn imported_generic_callable_captures_and_controls() {
    let project = TestProject::new(
        "imported_generic_capture",
        &[
            (
                "util.wi",
                "pub fn call<T>(x: T, f: closure(T) -> bool) -> bool { return f(x); }",
            ),
            (
                "main.wi",
                &format!(
                    "import util::call; {NOMINALS}
fn main() {{
 let yes = true;
 let v: N = new B();
 println(call(v, |x| x.hit() && yes));
 println(call(new B(), |x| x.hit() && yes));
 println(call(42, |x| x == 42));
 println(call(\"ok\", |x| x == \"ok\"));
}}"
                ),
            ),
        ],
    );
    assert_build(&project, false, "true\ntrue\ntrue\ntrue\n");
}

#[test]
fn imported_generic_callable_rejects_wrong_result() {
    let project = TestProject::new(
        "imported_generic_invalid",
        &[
            (
                "util.wi",
                "pub fn call<T>(x: T, f: fn(T) -> bool) -> bool { return f(x); }",
            ),
            (
                "main.wi",
                &format!(
                    "import util::call; {NOMINALS} fn main() {{ println(call(new B(), |x| 42)); }}"
                ),
            ),
        ],
    );
    for result in [project.check("main.wi"), project.compile("main.wi")] {
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(error.contains("E0201"), "{error}");
        assert!(!error.contains("E0800"), "{error}");
    }
}

#[test]
fn imported_generic_callable_example() {
    let project = TestProject::new(
        "imported_generic_example",
        &[
            (
                "util.wi",
                include_str!("../../example/imported_generic_callable/util.wi"),
            ),
            (
                "main.wi",
                include_str!("../../example/imported_generic_callable/main.wi"),
            ),
        ],
    );
    assert_build(&project, false, "true\ntrue\ntrue\ntrue\n");
}

#[test]
fn imported_generic_callable_symbols_scale_linearly() {
    for count in [1, 4, 16] {
        let mut main = format!("import util::call; {NOMINALS} fn main() {{ let v: N = new B();");
        for _ in 0..count {
            main.push_str("println(call(v, |x| x.hit()));");
        }
        main.push('}');
        let project = TestProject::new(
            "imported_generic_scale",
            &[
                (
                    "util.wi",
                    "pub fn call<T>(x: T, f: fn(T) -> bool) -> bool { return f(x); }",
                ),
                ("main.wi", &main),
            ],
        );
        let built = project.compile_with_env("main.wi", &[("WILLOW_KEEP_OBJECT", "1")]);
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let run = project.run();
        assert!(run.status.success());
        assert_eq!(String::from_utf8_lossy(&run.stdout), "true\n".repeat(count));
        let symbols = project.defined_object_symbols();
        assert_eq!(
            symbols.iter().filter(|s| s.contains("$lambda.")).count(),
            count,
            "{symbols:?}"
        );
        assert_eq!(
            symbols.iter().filter(|s| s.contains("call$mono$")).count(),
            1,
            "{symbols:?}"
        );
    }
}
