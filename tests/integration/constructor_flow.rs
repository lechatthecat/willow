//! End-to-end proof of path-sensitive field initialization (willow-9tls.31).
use super::support::{
    TestProject, assert_compile_error_contains, compile_and_run, compile_and_run_gc_stress,
    compile_and_run_release, compile_file_and_run, compile_release_with_compiler_env,
    compile_temp_project_and_run, compile_temp_project_error_stderr, compile_with_compiler_env,
};

#[test]
fn constructor_flow_rejects_partial_branch_and_early_exit() {
    for body in [
        "if flag { self.name = \"set\"; }",
        "if flag { return; } self.name = \"set\";",
        "while flag { self.name = \"set\"; break; }",
    ] {
        assert_compile_error_contains(
            &format!(
                "class C {{ name: String; init(self, flag: bool) {{ {body} }} }} fn main() {{}}"
            ),
            &[
                "error[E0842]",
                "field `name` is not initialized",
                "every path that returns",
            ],
        );
    }
}

#[test]
fn constructor_flow_branch_values_survive_gc() {
    let source = include_str!("../../example/constructor_flow.wi");
    let (out, ok) = compile_and_run_gc_stress(source);
    assert!(ok, "{out}");
    assert_eq!(out, "zero\n0\none\n1\nmany\n2\n");
}

#[test]
fn constructor_flow_nested_breaks_and_continue_run() {
    let (out, ok) = compile_and_run(
        r#"
class C {
    pub x: i64;
    pub init(self, flag: bool) {
        while true {
            while true { break; }
            if flag { self.x = 42; break; }
            continue;
        }
    }
}
fn main() { println(new C(true).x); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn constructor_flow_imported_diagnostic_keeps_source_location() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "shapes.wi",
                "module shapes; pub class C { pub name: String; pub init(self, flag: bool) { if flag { self.name = \"set\"; } } }",
            ),
            (
                "main.wi",
                "import shapes; fn main() { let c = new shapes::C(false); }",
            ),
        ],
        "main.wi",
    );
    assert!(stderr.contains("E0842"), "{stderr}");
    assert!(stderr.contains("shapes.wi"), "{stderr}");
}

#[test]
fn constructor_flow_recovery_cannot_publish_default_fields() {
    assert_compile_error_contains(
        r#"
class C {
    pub x: i64;
    pub init(self) {
        defer match recover() { Some(_) => {}, None => {} };
        panic("recovered");
    }
}
fn main() { println(new C().x); }
"#,
        &["error[E0842]", "field `x` is not initialized"],
    );
}

#[test]
fn constructor_flow_inner_recovery_resumes_before_initialization() {
    let (out, ok) = compile_and_run(
        r#"
class C {
    pub x: i64;
    pub init(self) {
        if true {
            defer match recover() { Some(_) => {}, None => {} };
            panic("recovered");
        }
        self.x = 42;
    }
}
fn main() { println(new C().x); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

// willow-ssl7.19: checker-owned panic facts refine constructor recovery.

#[test]
fn constructor_flow_cleanup_only_defer_accepts_panicking_constructor() {
    let source = r#"class C { x: i64; init(self) { defer println("cleanup"); panic("stop"); } } fn main() {}"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "{out}");
}

#[test]
fn constructor_flow_effects_example_runs() {
    let expected = "guarded cleanup\n3\n0\ninner recovered\n7\n";
    let (out, ok) = compile_file_and_run("example/constructor_flow_effects.wi");
    assert!(ok, "{out}");
    assert_eq!(out, expected);
    let source = include_str!("../../example/constructor_flow_effects.wi");
    let (out, ok) = compile_and_run_gc_stress(source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

/// A deferred helper's own `recover()` is not eligible for the caller's
/// unwinding, so accepting the constructor is sound: the panic escapes.
#[test]
fn constructor_flow_deferred_recovering_helper_cannot_resume() {
    let (out, ok) = compile_and_run(
        r#"
fn rescue() { defer match recover() { Some(_) => { println("helper"); }, None => {} }; }
class C {
    pub x: i64;
    pub init(self, fail: bool) {
        defer rescue();
        if fail { panic("stop"); }
        self.x = 1;
    }
}
fn main() {
    println(new C(false).x);
    let c = new C(true);
    println("unreachable");
}
"#,
    );
    assert!(!ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn constructor_flow_real_recovery_still_rejects_panicking_calls() {
    for body in [
        // Direct recovery with a panicking helper.
        "defer match recover() { Some(_) => {}, None => {} }; boom(); self.x = 1;",
        // Recovery through a deferred block that calls recover() directly.
        "defer { match recover() { Some(_) => {}, None => {} }; }; boom(); self.x = 1;",
        // A cleanup panic during an inner scope reaches outer recovery.
        "defer match recover() { Some(_) => {}, None => {} }; if true { defer boom(); } self.x = 1;",
        // A recursive SCC that can panic.
        "defer match recover() { Some(_) => {}, None => {} }; spin(true); self.x = 1;",
    ] {
        assert_compile_error_contains(
            &format!(
                "fn boom() {{ panic(\"boom\"); }}
fn spin(go: bool) {{ if go {{ spun(false); }} }}
fn spun(go: bool) {{ if go {{ spin(false); }} else {{ boom(); }} }}
class C {{ pub x: i64; pub init(self) {{ {body} }} }}
fn main() {{ println(new C().x); }}"
            ),
            &["error[E0842]", "field `x` is not initialized"],
        );
    }
}

/// Module-qualified identities: `util::calm` is panic-free while the entry's
/// same-named `calm` panics. Each call site uses its own resolved target.
#[test]
fn constructor_flow_imported_helper_identity_is_module_qualified() {
    let util = "module util; pub fn calm(n: i64) -> i64 { return n; }";
    let ok_main = r#"import util;
fn calm(n: i64) -> i64 { panic("local"); }
class C {
    pub x: i64;
    pub init(self) {
        defer match recover() { Some(_) => {}, None => {} };
        let n = util::calm(5);
        self.x = n;
    }
}
fn main() { println(new C().x); }
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("util.wi", util), ("main.wi", ok_main)], "main.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
    let bad_main = ok_main.replace("util::calm(5)", "calm(5)");
    let stderr = compile_temp_project_error_stderr(
        &[("util.wi", util), ("main.wi", bad_main.as_str())],
        "main.wi",
    );
    assert!(stderr.contains("E0842"), "{stderr}");
}

/// An open imported method can be overridden by an importing unit, so its
/// own facts are not proof for a dynamic call.
#[test]
fn constructor_flow_open_imported_method_stays_conservative() {
    let shapes = "module shapes; pub open class Base { pub init(self) {} pub open fn ok(self) -> bool { return true; } pub fn sealed(self) -> bool { return true; } }";
    let main = |call: &str| {
        format!(
            "import shapes;
class C {{
    pub x: i64;
    pub init(self, b: shapes::Base) {{
        defer match recover() {{ Some(_) => {{}}, None => {{}} }};
        let ok = b.{call}();
        self.x = 1;
    }}
}}
fn main() {{ println(new C(new shapes::Base()).x); }}
"
        )
    };
    let stderr = compile_temp_project_error_stderr(
        &[("shapes.wi", shapes), ("main.wi", main("ok").as_str())],
        "main.wi",
    );
    assert!(stderr.contains("E0842"), "{stderr}");
    let (out, ok) = compile_temp_project_and_run(
        &[("shapes.wi", shapes), ("main.wi", main("sealed").as_str())],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

/// E0842 is a language rule, so it must not depend on the build profile.
/// Debug overflow checks make `n + 1` a recoverable panic. Release wraps it,
/// but the checker reads the profile-independent checked-panic summary and
/// rejects in both modes, including through a call chain and an import.
/// Wrapping-only or literal-shift arithmetic is accepted in both modes.
#[test]
fn constructor_flow_checked_arithmetic_is_profile_independent() {
    let program = |helpers: &str, call: &str| {
        format!(
            "{helpers}
class C {{
    pub x: i64;
    pub init(self) {{
        defer match recover() {{ Some(_) => {{}}, None => {{}} }};
        let n = {call};
        self.x = n;
    }}
}}
fn main() {{ println(new C().x); }}
"
        )
    };
    let rejected = [
        program("fn grow(n: i64) -> i64 { return n + 1; }", "grow(1)"),
        program(
            "fn grow(n: i64) -> i64 { return -n; } fn outer(n: i64) -> i64 { return grow(n); }",
            "outer(1)",
        ),
        program("fn grow(n: i64) -> i64 { return n << n; }", "grow(1)"),
    ];
    for source in &rejected {
        for release in [false, true] {
            let (ok, stderr) = if release {
                compile_release_with_compiler_env(source, &[])
            } else {
                compile_with_compiler_env(source, &[])
            };
            assert!(!ok, "release={release} accepted:\n{source}");
            assert!(stderr.contains("E0842"), "release={release}: {stderr}");
        }
    }
    let accepted = program("fn calm(n: i64) -> i64 { return n << 3; }", "calm(1)");
    let (out, ok) = compile_and_run(&accepted);
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");
    let (out, ok) = compile_and_run_release(&accepted);
    assert!(ok, "{out}");
    assert_eq!(out, "8\n");

    let util = "module util; pub fn grow(n: i64) -> i64 { return n * 2; }";
    let main = program("import util;", "util::grow(1)");
    let project = TestProject::new(
        "ctor_checked_import",
        &[("util.wi", util), ("main.wi", &main)],
    );
    for output in [
        project.compile("main.wi"),
        project.compile_release("main.wi"),
    ] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains("E0842"), "{stderr}");
    }
}

/// Field reads and `self` uses before full initialization are compile errors,
/// not null dereferences: the three crash shapes plus String/Array fields.
#[test]
fn constructor_self_use_crash_shapes_are_rejected() {
    const INNER: &str = "class Inner { pub v: i64; pub init(self, v: i64) { self.v = v; } pub fn get(self) -> i64 { return self.v; } }";
    let cases = [
        (
            format!(
                "{INNER} class C {{ i: Inner; init(self) {{ println(self.i.get()); self.i = new Inner(1); }} }} fn main() {{ let c = new C(); }}"
            ),
            ["error[E0852]", "field `i` is read before it is assigned"],
        ),
        (
            format!(
                "{INNER} class C {{ i: Inner; init(self) {{ self.peek(); self.i = new Inner(1); }} fn peek(self) {{ println(self.i.get()); }} }} fn main() {{ let c = new C(); }}"
            ),
            ["error[E0853]", "`self` is used before field `i` is assigned"],
        ),
        (
            format!(
                "{INNER} open class Base {{ pub tag: i64; pub init(self) {{ self.tag = self.describe(); }} pub open fn describe(self) -> i64 {{ return 0; }} }} \
                 class Sub extends Base {{ inner: Inner; pub init(self) {{ super.init(); self.inner = new Inner(7); }} \
                 pub override fn describe(self) -> i64 {{ return self.inner.get(); }} }} fn main() {{ println(new Sub().tag); }}"
            ),
            ["error[E0853]", "field `inner` must be assigned before `super.init(...)`"],
        ),
        (
            "class S { s: String; init(self) { println(self.s.len()); self.s = \"x\"; } } fn main() { let s = new S(); }".to_string(),
            ["error[E0852]", "field `s` is read before"],
        ),
        (
            "import std::collections::Array; class A { a: Array<i64>; init(self) { println(self.a[0]); self.a = [1]; } } fn main() { let a = new A(); }".to_string(),
            ["error[E0852]", "field `a` is read before"],
        ),
    ];
    for (source, expected) in cases {
        assert_compile_error_contains(&source, &expected);
    }
}

#[test]
fn constructor_self_use_example_runs() {
    let expected = "16\nshape:square(4)\n#box\n";
    let (out, ok) = compile_file_and_run("example/constructor_self_use.wi");
    assert!(ok, "{out}");
    assert_eq!(out, expected);
    let source = include_str!("../../example/constructor_self_use.wi");
    let (out, ok) = compile_and_run_gc_stress(source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
    // The example's subclass with its fields assigned after `super.init`.
    let reordered = source.replace(
        "        self.side = side;\n        self.name = \"square\";\n        super.init();",
        "        super.init();\n        self.side = side;\n        self.name = \"square\";",
    );
    assert_ne!(reordered, source);
    assert_compile_error_contains(
        &reordered,
        &[
            "error[E0853]",
            "field `side` must be assigned before `super.init(...)`",
            "base class `Shape`",
        ],
    );
}

/// The escaping-base fact crosses modules: the imported base's constructor
/// is summarized from its own file.
#[test]
fn constructor_self_use_imported_escaping_base_requires_fields_first() {
    let base = "module shapes; pub open class Base { pub tag: i64; pub init(self) { self.tag = 0; self.tag = self.describe(); } pub open fn describe(self) -> i64 { return 0; } }";
    let sub = |body: &str| {
        format!(
            "import shapes; class Sub extends shapes::Base {{ n: i64; pub init(self) {{ {body} }} \
             pub override fn describe(self) -> i64 {{ return self.n; }} }} fn main() {{ println(new Sub().tag); }}"
        )
    };
    let stderr = compile_temp_project_error_stderr(
        &[
            ("shapes.wi", base),
            ("main.wi", &sub("super.init(); self.n = 5;")),
        ],
        "main.wi",
    );
    assert!(stderr.contains("E0853"), "{stderr}");
    assert!(
        stderr.contains("field `n` must be assigned before"),
        "{stderr}"
    );
    let (out, ok) = compile_temp_project_and_run(
        &[
            ("shapes.wi", base),
            ("main.wi", &sub("self.n = 5; super.init();")),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}
