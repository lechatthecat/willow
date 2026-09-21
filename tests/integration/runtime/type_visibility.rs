use super::*;

// ── Module-qualified type visibility (willow-7ihl): E0419 for private types ──

#[test]
fn module_vis_01_private_class_annotation_rejected() {
    let m = "module animals;\nclass Secret { pub v: i64; }\npub class Dog {}\n";
    let main = "import animals;\nfn main() { let s: animals::Secret = new animals::Secret(5); println(s.v); }\n";
    let stderr =
        compile_temp_project_error_stderr(&[("animals.wi", m), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("E0419"), "expected E0419, got: {stderr}");
    assert!(
        stderr.contains("private"),
        "diagnostic should mention private: {stderr}"
    );
}

#[test]
fn module_vis_02_pub_class_accessible() {
    let m = "module animals;\nclass Secret { pub v: i64; }\npub class Dog { pub fn speak(self) -> i64 { return 1; } }\n";
    let main = "import animals;\nfn main() { let d: animals::Dog = new animals::Dog(); println(d.speak()); }\n";
    let (out, ok) =
        compile_temp_project_and_run(&[("animals.wi", m), ("main.wi", main)], "main.wi");
    assert!(ok, "pub module class must be accessible: {out}");
    assert_eq!(out, "1\n");
}

#[test]
fn module_vis_03_private_interface_rejected() {
    let m = "module animals;\ninterface Hidden { fn f(self) -> i64; }\npub interface Shown { fn f(self) -> i64; }\n";
    let main = "import animals;\nfn use_it(a: animals::Hidden) {}\nfn main() {}\n";
    let stderr =
        compile_temp_project_error_stderr(&[("animals.wi", m), ("main.wi", main)], "main.wi");
    assert!(stderr.contains("E0419"), "expected E0419, got: {stderr}");
    assert!(
        stderr.contains("interface"),
        "diagnostic should name the kind: {stderr}"
    );
}

#[test]
fn module_vis_04_private_class_static_call_rejected() {
    let m = "module animals;\nclass Secret { pub static fn make() -> i64 { return 9; } }\npub class Dog {}\n";
    let main = "import animals;\nfn main() { println(animals::Secret::make()); }\n";
    let stderr =
        compile_temp_project_error_stderr(&[("animals.wi", m), ("main.wi", main)], "main.wi");
    assert!(
        stderr.contains("E0419"),
        "expected E0419 on static call, got: {stderr}"
    );
}

#[test]
fn module_vis_05_pub_interface_accessible() {
    let m = "module shapes;\npub interface Shape { fn area(self) -> i64; }\npub class Sq implements Shape { pub side: i64; pub fn area(self) -> i64 { return self.side * self.side; } }\n";
    let main = "import shapes;\nfn describe(s: shapes::Shape) { println(s.area()); }\nfn main() { describe(new shapes::Sq(4)); }\n";
    let (out, ok) = compile_temp_project_and_run(&[("shapes.wi", m), ("main.wi", main)], "main.wi");
    assert!(ok, "pub module interface must be accessible: {out}");
    assert_eq!(out, "16\n");
}
