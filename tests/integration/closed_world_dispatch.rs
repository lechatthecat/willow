//! Closed-build interface dispatch (willow-8hq4.34).
use super::support::{TestProject, compile_and_run_release};

#[test]
fn closed_world_later_module_and_rebuild_invalidate_singleton() {
    let lib = r#"module lib;
        pub interface Value { fn get(self) -> i64; }
        pub class First implements Value { pub fn get(self) -> i64 { return 11; } }
        pub fn read(v: Value) -> i64 { return v.get(); }
    "#;
    let project = TestProject::new(
        "closed_world",
        &[
            ("lib.wi", lib),
            (
                "app.wi",
                "import lib; fn main() { println(lib::read(new lib::First())); }",
            ),
        ],
    );
    let check = |expected: &str| {
        let built = project.compile_with_args("app.wi", &["--release"]);
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let output = project.run();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    };
    check("11\n");
    project.write_file("other.wi", "module other; import lib; pub class Second implements lib::Value { pub fn get(self) -> i64 { return 29; } }");
    project.write_file("app.wi", "import lib; import other; fn main() { println(lib::read(new lib::First())); println(lib::read(new other::Second())); }");
    check("11\n29\n");
    project.write_file("other.wi", "module other; import lib; pub class Second implements lib::Value { pub fn get(self) -> i64 { return 31; } }");
    check("11\n31\n");
    project.write_file(
        "app.wi",
        "import lib; fn main() { println(lib::read(new lib::First())); }",
    );
    check("11\n");
}

#[test]
fn closed_world_later_override_through_base_box() {
    let project = TestProject::new(
        "closed_world_override",
        &[
            (
                "lib.wi",
                r#"module lib;
            pub interface Value { fn get(self) -> i64; }
            pub open class Base implements Value { pub open fn get(self) -> i64 { return 11; } }
            pub fn box(v: Base) -> Value { return v; }
            pub fn read(v: Value) -> i64 { return v.get(); }
        "#,
            ),
            (
                "app.wi",
                r#"import lib;
            class Child extends lib::Base { pub override fn get(self) -> i64 { return 29; } }
            fn main() { println(lib::read(lib::box(new Child()))); println(lib::read(lib::box(new lib::Base()))); }
        "#,
            ),
        ],
    );
    let built = project.compile_with_args("app.wi", &["--release"]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let output = project.run();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "29\n11\n");
}

#[test]
fn closed_world_singleton_abis_and_allocations() {
    let source = include_str!("../../example/closed_world_dispatch.wi");
    let (output, ok) = compile_and_run_release(source);
    assert!(ok, "{output}");
    assert_eq!(output, "12\n2\nitem!\n7\n");
}

#[test]
fn closed_world_singleton_panic_keeps_failure_edge() {
    let source = r#"
        interface Value { fn get(self) -> i64; }
        class Bomb implements Value { pub fn get(self) -> i64 { panic("singleton"); return 0; } }
        fn read(v: Value) { println(v.get()); println("unreachable"); }
        fn main() { read(new Bomb()); }
    "#;
    let (output, ok) = compile_and_run_release(source);
    assert!(!ok, "{output}");
    assert!(output.is_empty(), "{output}");
}

#[test]
fn closed_world_second_package_shares_interface_identity() {
    let project = TestProject::new(
        "closed_world_packages",
        &[
            (
                "project.toml",
                "[project]\nname = 'app'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n[dependencies]\na = { path = 'a' }\nb = { path = 'b' }\n",
            ),
            (
                "a/project.toml",
                "[project]\nname = 'alpha'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n",
            ),
            (
                "b/project.toml",
                "[project]\nname = 'beta'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n[dependencies]\ninner = { path = '../a' }\n",
            ),
            (
                "a/src/util.wi",
                "module util; pub interface Read { fn read(self) -> i64; } pub class First implements Read { pub fn read(self) -> i64 { return 11; } } pub fn get(v: Read) -> i64 { return v.read(); }",
            ),
            (
                "b/src/util.wi",
                "module util; import inner::util as base; pub class Second implements base::Read { pub fn read(self) -> i64 { return 29; } } pub fn make() -> base::Read { return new Second(); }",
            ),
            (
                "src/main.wi",
                "import a::util as first; import b::util as second; fn main() { println(first::get(new first::First())); println(first::get(second::make())); }",
            ),
        ],
    );
    let build = project.compile_with_args(".", &["--release"]);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let output = project.run();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "11\n29\n");
}

#[test]
fn closed_world_singleton_release_gc_and_debug_control() {
    let project = TestProject::new(
        "closed_world_gc",
        &[(
            "app.wi",
            include_str!("../../example/closed_world_dispatch.wi"),
        )],
    );
    for args in [&[][..], &["--release"][..]] {
        let built = project.compile_with_args("app.wi", args);
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let output = project.run_with_env(&[("WILLOW_GC_STRESS", "all")]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "12\n2\nitem!\n7\n");
    }
}

#[test]
fn closed_world_default_and_inherited_body_remain_singleton() {
    let source = r#"
        interface Parent { fn get(self) -> i64; }
        interface Value extends Parent { fn twice(self) -> i64 { return self.get() * 2; } }
        open class Base implements Value { pub fn get(self) -> i64 { return 7; } }
        class Child extends Base {}
        fn read(v: Value) { let p: Parent = v; println(p.get()); println(v.twice()); }
        fn main() { read(new Base()); read(new Child()); }
    "#;
    let (output, ok) = compile_and_run_release(source);
    assert!(ok, "{output}");
    assert_eq!(output, "7\n14\n7\n14\n");
}

#[test]
fn closed_world_thunk_proof_ignores_caller_local_class_shadow() {
    let project = TestProject::new(
        "closed_world_shadow",
        &[
            (
                "lib.wi",
                r#"module lib;
            pub interface Value { fn get(self) -> i64; }
            pub open class C { pub open fn get(self) -> i64 { return 99; } }
            pub fn read(v: Value) -> i64 { return v.get(); }
        "#,
            ),
            (
                "app.wi",
                r#"import lib;
            open class C implements lib::Value { pub open fn get(self) -> i64 { return 7; } }
            class Child extends C {}
            fn main() { println(lib::read(new C())); println(lib::read(new Child())); }
        "#,
            ),
        ],
    );
    let built = project.compile_with_args("app.wi", &["--release"]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let output = project.run();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "7\n7\n");
}
