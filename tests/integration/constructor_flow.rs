//! End-to-end proof of path-sensitive field initialization (willow-9tls.31).
use super::support::{
    assert_compile_error_contains, compile_and_run, compile_and_run_gc_stress,
    compile_file_and_run, compile_temp_project_error_stderr,
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
fn constructor_flow_example_runs() {
    let (out, ok) = compile_file_and_run("example/constructor_flow.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "zero\n0\none\n1\nmany\n2\n");
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
