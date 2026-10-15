//! Project-mode downcast payload identity (willow-jz15.79).
use super::support::*;

const SHAPES: &str = r#"
pub interface C { fn k(self) -> i64; }
pub class E implements C {
    pub id: i64;
    pub fn k(self) -> i64 { return self.id; }
}
"#;

#[test]
fn module_downcast_payload_defining_module_field() {
    let shapes =
        format!("{SHAPES}\npub fn d(c: C) -> i64 {{ return match c {{ E(e) => e.id, _ => 0 }}; }}");
    let files = [
        ("shapes.wi", shapes.as_str()),
        (
            "main.wi",
            "import shapes::{C, E, d}; fn main() { println(d(new E(2))); }",
        ),
    ];
    let project = TestProject::new("module_downcast", &files);
    let checked = project.check("main.wi");
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let built = project.compile("main.wi");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let run = project.run();
    assert!(run.status.success());
    assert_eq!(String::from_utf8_lossy(&run.stdout), "2\n");
}

// Seven payload uses in each of three independently checked scopes = 21
// explicit perspectives. Each program also exercises the nonmatching class.
#[test]
fn module_downcast_payload_scope_matrix() {
    let cases = [
        ("field", "return match c { E(e) => e.id, _ => 0 };", "2"),
        ("method", "return match c { E(e) => e.k(), _ => 0 };", "2"),
        (
            "guard",
            "return match c { E(e) if e.k() == 2 => e.id, _ => 0 };",
            "2",
        ),
        (
            "block",
            "match c { E(e) => { let n = e.id; return n; } _ => { return 0; } }",
            "2",
        ),
        (
            "local_copy",
            "match c { E(e) => { let copy = e; return copy.k() + copy.id; } _ => { return 0; } }",
            "4",
        ),
        (
            "repeated_use",
            "return match c { E(e) => e.id + e.k() + e.id, _ => 0 };",
            "6",
        ),
        (
            "shadow",
            "let e = 100; let n = match c { E(e) => e.id, _ => 0 }; return n + e - 100;",
            "2",
        ),
    ];
    for scope in ["entry", "defining", "importing"] {
        for (case, body, expected) in cases {
            let shapes = format!(
                "{SHAPES}\npub class Other implements C {{ pub fn k(self) -> i64 {{ return 9; }} }}\n{}",
                if scope == "defining" {
                    format!("pub fn d(c: C) -> i64 {{ {body} }}")
                } else {
                    String::new()
                }
            );
            let consumer = format!("import shapes::{{C, E}}; pub fn d(c: C) -> i64 {{ {body} }}");
            let entry = format!(
                "import shapes::{{C, E, Other}}; {} fn main() {{ println(d(new E(2))); println(d(new Other())); }}",
                match scope {
                    "entry" => format!("fn d(c: C) -> i64 {{ {body} }}"),
                    "defining" => "import shapes::d;".into(),
                    _ => "import consumer::d;".into(),
                }
            );
            let files = [
                ("shapes.wi", shapes.as_str()),
                ("consumer.wi", consumer.as_str()),
                ("main.wi", entry.as_str()),
            ];
            let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &[]);
            assert!(ok, "{scope}/{case}: {out}");
            assert_eq!(out, format!("{expected}\n0\n"), "{scope}/{case}");
        }
    }
}

#[test]
fn module_downcast_payload_example() {
    let (out, ok) = compile_file_and_run("example/module_downcast_payload/main.wi");
    assert!(ok, "example failed: {out}");
    assert_eq!(out, "2\n2\n2\n");
}

#[test]
fn module_downcast_payload_aliases_and_class_identity() {
    let files = [
        ("shapes.wi", SHAPES),
        (
            "other.wi",
            "import shapes::C; pub class E implements C { pub id: i64; pub fn k(self) -> i64 { return self.id + 10; } }",
        ),
        (
            "consumer.wi",
            r#"
import shapes::C;
import shapes::E as First;
import other::E as Second;
pub fn d(c: C) -> i64 {
    return match c { First(e) => e.id, Second(e) => e.k(), _ => 0 };
}
"#,
        ),
        (
            "main.wi",
            "import shapes::E as First; import other::E as Second; import consumer::d; fn main() { println(d(new First(2))); println(d(new Second(3))); }",
        ),
    ];
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &[]);
    assert!(ok, "{out}");
    assert_eq!(out, "2\n13\n");
}

#[test]
fn module_downcast_payload_release_and_gc_stress() {
    let files = [
        (
            "shapes.wi",
            include_str!("../../example/module_downcast_payload/shapes.wi"),
        ),
        (
            "consumer.wi",
            include_str!("../../example/module_downcast_payload/consumer.wi"),
        ),
        (
            "main.wi",
            include_str!("../../example/module_downcast_payload/main.wi"),
        ),
    ];
    let project = TestProject::new("module_downcast_release", &files);
    let built = project.compile_release("main.wi");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    for mode in ["alloc", "minor"] {
        let run = project.run_with_env_timeout(
            &[("WILLOW_GC_STRESS", mode)],
            std::time::Duration::from_secs(30),
        );
        assert!(
            run.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&run.stdout), "2\n2\n2\n");
    }
}
