use super::support::TestProject;

const ROOT: &str = "[project]\nname = 'app'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n[dependencies]\na = { path = 'a' }\nb = { path = 'b' }\nsame = { path = 'a' }\n";
const A: &str = "[project]\nname = 'alpha'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n";
const B: &str = "[project]\nname = 'beta'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n";

fn run_case(a: &str, b: &str, entry: &str, expected: &str) {
    let project = TestProject::new(
        "package_identity",
        &[
            ("project.toml", ROOT),
            ("a/project.toml", A),
            ("b/project.toml", B),
            ("a/src/util.wi", a),
            ("b/src/util.wi", b),
            ("src/main.wi", entry),
        ],
    );
    let build = project.compile(".");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
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
fn package_identity_distinct_functions_and_repeated_aliases_execute() {
    for imports in [
        "import a::util as first; import b::util as second; import same::util as again;",
        "import same::util as again; import b::util as second; import a::util as first;",
    ] {
        run_case(
            "module util; pub fn value() -> i64 { return 11; }",
            "module util; pub fn value() -> i64 { return 22; }",
            &format!(
                "{imports} fn main() {{ println(first::value()); println(second::value()); println(again::value()); }}"
            ),
            "11\n22\n11\n",
        );
    }
}

#[test]
fn package_identity_distinct_item_functions_execute() {
    run_case(
        "module util; pub fn value() -> i64 { return 11; }",
        "module util; pub fn value() -> i64 { return 22; }",
        "import a::util::value as first; import b::util::value as second; fn main() { println(first()); println(second()); }",
        "11\n22\n",
    );
}

#[test]
fn package_identity_distinct_class_layouts_execute() {
    run_case(
        "module util; pub class Value { pub n: i64; pub init(self, n: i64) { self.n = n; } pub fn read(self) -> i64 { return self.n; } }",
        "module util; pub class Value { pub pad: i64; pub n: i64; pub init(self, pad: i64, n: i64) { self.pad = pad; self.n = n; } pub fn read(self) -> i64 { return self.pad + self.n; } }",
        "import a::util as first; import b::util as second; fn main() { let x = new first::Value(11); let y = new second::Value(20, 2); println(x.read()); println(y.read()); }",
        "11\n22\n",
    );
}

#[test]
fn package_identity_same_named_enums_and_interfaces_execute() {
    let a = "module util; pub enum Kind { Small, Large } pub interface Read { fn read(self) -> i64; } pub class Value implements Read { pub fn read(self) -> i64 { return 11; } } pub fn make() -> Read { return new Value(); } pub fn kind() -> Kind { return Kind::Small; }";
    let b = "module util; pub enum Kind { Large, Small } pub interface Read { fn read(self) -> i64; } pub class Value implements Read { pub fn read(self) -> i64 { return 22; } } pub fn make() -> Read { return new Value(); } pub fn kind() -> Kind { return Kind::Small; }";
    run_case(
        a,
        b,
        "import a::util as first; import b::util as second; fn main() { let x: first::Read = first::make(); let y: second::Read = second::make(); println(x.read()); println(y.read()); match first::kind() { first::Kind::Small => println(1), first::Kind::Large => println(0) }; match second::kind() { second::Kind::Small => println(2), second::Kind::Large => println(0) }; }",
        "11\n22\n1\n2\n",
    );
}

#[test]
fn package_identity_same_named_static_storage_execute() {
    run_case(
        "module util; pub class Value { pub static n: i64 = 11; pub static fn read() -> i64 { return Value::n; } }",
        "module util; pub class Value { pub static n: i64 = 22; pub static fn read() -> i64 { return Value::n; } }",
        "import a::util::Value as First; import b::util::Value as Second; fn main() { println(First::read()); println(Second::read()); }",
        "11\n22\n",
    );
}

#[test]
fn package_identity_consumer_local_transitive_aliases_execute() {
    let b = "[project]\nname = 'beta'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n[dependencies]\ninner = { path = '../a' }\n";
    let project = TestProject::new(
        "package_transitive",
        &[
            ("project.toml", ROOT),
            ("a/project.toml", A),
            ("b/project.toml", b),
            (
                "a/src/util.wi",
                "module util; pub class Value { pub n: i64; pub init(self, n: i64) { self.n = n; } } pub fn value() -> i64 { return 11; }",
            ),
            (
                "b/src/util.wi",
                "module util; import inner::util as own; import inner::util::Value as V; pub fn value() -> i64 { return own::value() + 11; } pub fn make() -> V { return new V(33); }",
            ),
            (
                "src/main.wi",
                "import b::util as second; import a::util as first; fn main() { println(first::value()); println(second::value()); let x: first::Value = second::make(); println(x.n); }",
            ),
        ],
    );
    let build = project.compile(".");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let run = project.run();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), "11\n22\n33\n");
}

#[test]
fn package_identity_generic_interfaces_and_gc_execute() {
    let module = "module util; pub interface Holder<T> { fn get(self) -> T; } pub class Cell implements Holder<i64> { pub n: i64; pub text: String; pub init(self, n: i64) { self.n = n; self.text = \"kept\"; } pub fn get(self) -> i64 { println(self.text); return self.n; } } pub fn make(n: i64) -> Holder<i64> { return new Cell(n); }";
    let project = TestProject::new(
        "package_generic_gc",
        &[
            ("project.toml", ROOT),
            ("a/project.toml", A),
            ("b/project.toml", B),
            ("a/src/util.wi", module),
            ("b/src/util.wi", module),
            (
                "src/main.wi",
                "import a::util as first; import b::util as second; fn main() { let x: first::Holder<i64> = first::make(11); let y: second::Holder<i64> = second::make(22); println(x.get()); println(y.get()); }",
            ),
        ],
    );
    for release in [false, true] {
        let build = if release {
            project.compile_release(".")
        } else {
            project.compile(".")
        };
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        let run = project.run_with_env(&[("WILLOW_GC_STRESS", "1")]);
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&run.stdout), "kept\n11\nkept\n22\n");
    }
}

#[test]
fn package_identity_default_methods_across_dependency_execute() {
    run_case(
        "module util; pub interface Read { fn read(self) -> i64 { return 11; } }",
        "module util; pub interface Read { fn read(self) -> i64 { return 22; } }",
        "import a::util::Read as First; import b::util::Read as Second; class Left implements First {} class Right implements Second {} fn main() { let a: First = new Left(); let b: Second = new Right(); println(a.read()); println(b.read()); }",
        "11\n22\n",
    );
}

#[test]
fn package_identity_native_symbols_ignore_alias_and_import_order() {
    let module = "module util; pub class Value { pub static n: i64 = 11; pub fn read(self) -> i64 { return 11; } } pub fn value() -> i64 { return 11; }";
    let project = TestProject::new(
        "package_symbols",
        &[
            ("project.toml", ROOT),
            ("a/project.toml", A),
            ("b/project.toml", B),
            ("a/src/util.wi", module),
            ("b/src/util.wi", module),
            ("src/main.wi", ""),
        ],
    );
    let mut previous = None;
    for (imports, expression) in [
        (
            "import a::util as first; import b::util as second;",
            "first::value() + second::value()",
        ),
        (
            "import b::util as two; import same::util as one;",
            "one::value() + two::value()",
        ),
        (
            "import same::util as one; import a::util as again; import b::util as two;",
            "again::value() + two::value()",
        ),
    ] {
        project.write_file(
            "src/main.wi",
            &format!("{imports} fn main() {{ println({expression}); }}"),
        );
        let build = project.compile_with_env(".", &[("WILLOW_KEEP_OBJECT", "1")]);
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        let symbols: Vec<_> = project
            .defined_object_symbols()
            .into_iter()
            .filter(|name| name.starts_with("$pkg"))
            .collect();
        assert!(
            symbols.iter().any(|name| name.ends_with(".util.value")),
            "{symbols:?}"
        );
        assert!(symbols.iter().all(|name| !name.contains("first")
            && !name.contains("second")
            && !name.contains("http")
            && !name.contains("/")));
        if let Some(previous) = &previous {
            assert_eq!(&symbols, previous);
        }
        previous = Some(symbols);
    }
}

#[test]
fn package_identity_cross_package_type_confusion_is_rejected() {
    for (declaration, function) in [
        (
            "pub class Value {}",
            "fn wrong(x: first::Value) -> second::Value { return x; }",
        ),
        (
            "pub interface Value { fn read(self) -> i64; }",
            "fn wrong(x: first::Value) -> second::Value { return x; }",
        ),
        (
            "pub enum Value { A }",
            "fn wrong(x: first::Value) -> second::Value { return x; }",
        ),
    ] {
        let module = format!("module util; {declaration}");
        let entry =
            format!("import a::util as first; import b::util as second; {function} fn main() {{}}");
        let project = TestProject::new(
            "package_confusion",
            &[
                ("project.toml", ROOT),
                ("a/project.toml", A),
                ("b/project.toml", B),
                ("a/src/util.wi", &module),
                ("b/src/util.wi", &module),
                ("src/main.wi", &entry),
            ],
        );
        let build = project.compile(".");
        let stderr = String::from_utf8_lossy(&build.stderr);
        assert!(!build.status.success(), "{declaration} silently merged");
        assert!(stderr.contains("mismatched types"), "{stderr}");
    }
}

#[test]
fn package_identity_inherited_interface_dispatch_executes() {
    run_case(
        "module util; pub interface Read { fn read(self) -> i64; } pub open class Value implements Read { pub open fn read(self) -> i64 { return 11; } }",
        "module util; pub interface Read { fn read(self) -> i64; } pub open class Value implements Read { pub open fn read(self) -> i64 { return 22; } }",
        "import a::util as first; import b::util as second; class Left extends first::Value {} class Right extends second::Value {} fn main() { let a: first::Read = new Left(); let b: second::Read = new Right(); println(a.read()); println(b.read()); }",
        "11\n22\n",
    );
}

#[test]
fn package_identity_initializers_run_once_per_package() {
    run_case(
        "module util; fn seed() -> i64 { println(1); return 11; } pub class Value { pub static n: i64 = seed(); pub static fn read() -> i64 { return Value::n; } }",
        "module util; fn seed() -> i64 { println(2); return 22; } pub class Value { pub static n: i64 = seed(); pub static fn read() -> i64 { return Value::n; } }",
        "import a::util::Value as First; import same::util::Value as Again; import b::util::Value as Second; fn main() { println(First::read()); println(Again::read()); println(Second::read()); }",
        "1\n2\n11\n11\n22\n",
    );
}
