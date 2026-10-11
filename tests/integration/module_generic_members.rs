//! Cross-module specialization-origin regression coverage (willow-jz15.63).
use super::support::{TestProject, compile_file_and_run};

const BOXES: &str = r#"
module boxes;
pub class Box<T> {
    pub value: T;
    pub fn get(self) -> T { return self.value; }
}
pub class Holder {
    pub b: Box<i64>;
    pub init(self) { self.b = new Box<i64>(5); }
    pub fn box(self) -> Box<i64> { return self.b; }
    pub fn take(self, b: Box<i64>) -> i64 { return b.get(); }
    pub fn read(self, b: &Box<i64>) -> i64 { return b.get(); }
}
pub class Nested {
    pub b: Box<Box<i64>>;
    pub init(self) { self.b = new Box<Box<i64>>(new Box<i64>(7)); }
}
pub class Deep {
    pub b: Box<Box<Box<i64>>>;
    pub init(self) { self.b = new Box<Box<Box<i64>>>(new Box<Box<i64>>(new Box<i64>(17))); }
}
pub class Node { pub n: i64; }
pub class Nominal {
    pub b: Box<Node>;
    pub init(self) { self.b = new Box<Node>(new Node(19)); }
}
pub class Text {
    pub b: Box<String>;
    pub init(self) { self.b = new Box<String>("kept"); }
}
pub class Flag {
    pub b: Box<bool>;
    pub init(self) { self.b = new Box<bool>(true); }
}
pub fn make() -> Box<i64> { return new Box<i64>(11); }
pub class Hidden<T> {
    value: T;
    pub init(self, value: T) { self.value = value; }
    fn get(self) -> T { return self.value; }
}
pub class Secret {
    pub b: Hidden<i64>;
    pub init(self) { self.b = new Hidden<i64>(13); }
}
"#;

#[track_caller]
fn project_runs(files: &[(&str, &str)], expected: &str) {
    let project = TestProject::new("module_generic_members", files);
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
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), expected);
}

macro_rules! member_case {
    ($name:ident, $body:literal, $expected:literal) => {
        #[test]
        fn $name() {
            let main = concat!("import boxes::Holder; import boxes::Nested; import boxes::Text; import boxes::Flag; import boxes; fn main() { ", $body, " }");
            project_runs(&[("boxes.wi", BOXES), ("main.wi", main)], $expected);
        }
    };
}

member_case!(p01_direct_field, "println(boxes::make().value);", "11\n");
member_case!(p02_direct_method, "println(boxes::make().get());", "11\n");
member_case!(
    p03_holder_field,
    "let h = new Holder(); println(h.b.value);",
    "5\n"
);
member_case!(
    p04_holder_method,
    "let h = new Holder(); println(h.b.get());",
    "5\n"
);
member_case!(
    p05_local_field,
    "let b = new Holder().b; println(b.value);",
    "5\n"
);
member_case!(
    p06_local_method,
    "let b = new Holder().b; println(b.get());",
    "5\n"
);
member_case!(
    p07_nested_fields,
    "println(new Nested().b.value.value);",
    "7\n"
);
member_case!(
    p08_nested_methods,
    "println(new Nested().b.get().get());",
    "7\n"
);
member_case!(
    p09_nested_local,
    "let b = new Nested().b.get(); println(b.value);",
    "7\n"
);
member_case!(p10_string_field, "println(new Text().b.value);", "kept\n");
member_case!(p11_string_method, "println(new Text().b.get());", "kept\n");
member_case!(p12_bool, "println(new Flag().b.get());", "true\n");
member_case!(
    p13_method_return,
    "println(new Holder().box().get());",
    "5\n"
);
member_case!(
    p14_parameter,
    "let h = new Holder(); println(h.take(h.b));",
    "5\n"
);
member_case!(
    p15_reassignment,
    "let mut b = new Holder().b; b = boxes::make(); println(b.get());",
    "11\n"
);
member_case!(
    p16_array_index,
    "let a = [new Holder().b, boxes::make()]; println(a[0].get() + a[1].value);",
    "16\n"
);
member_case!(
    p17_lambda_return,
    "let make = || boxes::make(); println(make().get());",
    "11\n"
);
member_case!(
    p18_multiple_specializations,
    "println(new Holder().b.get()); println(new Text().b.get()); println(new Flag().b.get()); println(new Nested().b.get().get());",
    "5\nkept\ntrue\n7\n"
);
member_case!(
    p19_repeated_access,
    "let h = new Holder(); let mut sum = 0; for i in 0..4 { sum = sum + h.b.get(); } println(sum);",
    "20\n"
);

#[test]
fn p20_module_alias() {
    project_runs(
        &[
            ("boxes.wi", BOXES),
            (
                "main.wi",
                "import boxes as b; import boxes::Holder as H; fn main() { println(new H().b.get()); println(b::make().value); }",
            ),
        ],
        "5\n11\n",
    );
}

#[test]
fn p21_transitive_module() {
    project_runs(
        &[
            ("boxes.wi", BOXES),
            (
                "relay.wi",
                "module relay; import boxes::Holder; pub fn make() -> Holder { return new Holder(); }",
            ),
            (
                "main.wi",
                "import relay; fn main() { let h = relay::make(); println(h.b.get()); }",
            ),
        ],
        "5\n",
    );
}

#[test]
fn p22_same_named_templates() {
    project_runs(
        &[
            ("boxes.wi", BOXES),
            (
                "other.wi",
                "module other; pub class Box<T> { pub value: T; pub extra: i64; pub fn get(self) -> T { return self.value; } } pub fn make() -> Box<String> { return new Box<String>(\"other\", 99); }",
            ),
            (
                "main.wi",
                "import boxes; import other; fn main() { println(boxes::make().get()); println(other::make().get()); println(other::make().extra); }",
            ),
        ],
        "11\nother\n99\n",
    );
}

#[test]
fn p23_private_members_stay_private() {
    for member in ["value", "get()"] {
        let main =
            format!("import boxes::Secret; fn main() {{ println(new Secret().b.{member}); }}");
        let project = TestProject::new(
            "generic_private",
            &[("boxes.wi", BOXES), ("main.wi", &main)],
        );
        let checked = project.check("main.wi");
        assert!(!checked.status.success());
        let stderr = String::from_utf8_lossy(&checked.stderr);
        assert!(
            stderr.contains("error[E0501]") && stderr.contains("private"),
            "{stderr}"
        );
    }
}

#[test]
fn p24_example() {
    let (out, ok) = compile_file_and_run("example/module_generic_members/main.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "5\n5\n7\n7\nkept\n");
}

#[test]
fn p25_three_nested_specializations() {
    project_runs(
        &[
            ("boxes.wi", BOXES),
            (
                "main.wi",
                "import boxes::Deep; fn main() { println(new Deep().b.get().get().get()); }",
            ),
        ],
        "17\n",
    );
}

#[test]
fn p26_nominal_type_argument() {
    project_runs(
        &[
            ("boxes.wi", BOXES),
            (
                "main.wi",
                "import boxes::Nominal; fn main() { println(new Nominal().b.get().n); }",
            ),
        ],
        "19\n",
    );
}

#[test]
fn p27_field_assignment() {
    project_runs(
        &[
            ("boxes.wi", BOXES),
            (
                "main.wi",
                "import boxes; import boxes::Holder; fn main() { let h = new Holder(); h.b = boxes::make(); println(h.b.get()); }",
            ),
        ],
        "11\n",
    );
}

#[test]
fn p28_repeated_access_emits_one_method_body() {
    for uses in [1, 8, 64] {
        let main = format!(
            "import boxes; fn main() {{ let b = boxes::make(); {} }}",
            "println(b.get());".repeat(uses)
        );
        let boxes = "module boxes; pub class Box<T> { pub value: T; pub fn get(self) -> T { return self.value; } } pub fn make() -> Box<i64> { return new Box<i64>(5); }";
        let project = TestProject::new(
            "generic_member_symbols",
            &[("boxes.wi", boxes), ("main.wi", &main)],
        );
        let built = project.compile_with_env("main.wi", &[("WILLOW_KEEP_OBJECT", "1")]);
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let symbols = project.defined_object_symbols();
        let methods = symbols
            .iter()
            .filter(|name| name.contains("Box$mono$") && name.ends_with("get"))
            .count();
        assert_eq!(methods, 1, "{symbols:?}");
        println!("WILLOW_GENERIC_MEMBER_CODEGEN uses={uses} method_bodies={methods}");
    }
}

#[test]
fn p29_nested_members_survive_gc_stress() {
    let main = "import boxes::Deep; import boxes::Text; fn main() { let d = new Deep(); let t = new Text(); println(t.b.get() + t.b.value); println(d.b.get().get().get()); }";
    let project = TestProject::new(
        "generic_member_gc",
        &[("boxes.wi", BOXES), ("main.wi", main)],
    );
    let built = project.compile("main.wi");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    for mode in ["alloc", "minor"] {
        let run = project.run_with_env(&[("WILLOW_GC_STRESS", mode)]);
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&run.stdout), "keptkept\n17\n");
    }
}

member_case!(
    p30_reference_parameter,
    "let h = new Holder(); println(h.read(&h.b));",
    "5\n"
);
