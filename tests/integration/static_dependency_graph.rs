//! willow-9tls.32: dependency ordering and cycle witnesses, including helper calls.
use super::support::*;

fn output(source: &str, expected: &str) {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}
macro_rules! runs {
    ($name:ident, $source:expr, $expected:expr) => {
        #[test]
        fn $name() {
            output($source, $expected);
        }
    };
}
macro_rules! cycle {
    ($name:ident, $source:expr, $($part:expr),+) => {
        #[test] fn $name() {
            assert_compile_error_contains($source, &["error[E0838]", "static initialization cycle", $($part),+]);
        }
    };
}

runs!(
    dependency_01_cross_class_forward,
    "class A { pub static x: i64 = B::y + 1; } class B { pub static y: i64 = 41; } fn main() { println(A::x); }",
    "42\n"
);
runs!(
    dependency_02_same_class_helper,
    "fn seed() -> i64 { return C::b + 1; } class C { pub static a: i64 = seed(); pub static b: i64 = 41; } fn main() { println(C::a); }",
    "42\n"
);
runs!(
    dependency_03_helper_chain,
    "fn first() -> i64 { return second(); } fn second() -> i64 { return B::y; } class A { pub static x: i64 = first(); } class B { pub static y: i64 = 42; } fn main() { println(A::x); }",
    "42\n"
);
runs!(
    dependency_04_static_method,
    "class A { pub static x: i64 = B::seed(); } class B { pub static y: i64 = 42; pub static fn seed() -> i64 { return Self::y; } } fn main() { println(A::x); }",
    "42\n"
);
runs!(
    dependency_05_instance_method,
    "class A { pub static x: i64 = new B().seed(); } class B { pub static y: i64 = 42; pub fn seed() -> i64 { return B::y; } } fn main() { println(A::x); }",
    "42\n"
);
runs!(
    dependency_06_constructor,
    "class A { pub static x: B = new B(); } class B { pub v: i64; pub static y: i64 = 42; pub init(self) { self.v = B::y; } } fn main() { println(A::x.v); }",
    "42\n"
);
runs!(
    dependency_07_helper_recursion,
    "fn seed(n: i64) -> i64 { if n == 0 { return B::y; } return seed(n - 1); } class A { pub static x: i64 = seed(3); } class B { pub static y: i64 = 42; } fn main() { println(A::x); }",
    "42\n"
);
runs!(
    dependency_08_shared_diamond,
    "fn seed() -> i64 { return D::x; } class A { pub static x: i64 = B::x + C::x; } class B { pub static x: i64 = seed(); } class C { pub static x: i64 = seed(); } class D { pub static x: i64 = 21; } fn main() { println(A::x); }",
    "42\n"
);
runs!(
    dependency_09_inherited_static,
    "class A { pub static x: i64 = Child::y; } open class Base { pub static y: i64 = 42; } class Child extends Base {} fn main() { println(A::x); }",
    "42\n"
);
runs!(
    dependency_10_self_forward,
    "class A { pub static x: i64 = Self::y + 1; pub static y: i64 = 41; } fn main() { println(A::x); }",
    "42\n"
);
runs!(
    dependency_11_independent_side_effect_order,
    "fn seed(n: i64) -> i64 { println(n); return n; } class A { pub static x: i64 = seed(1); pub static y: i64 = seed(2); } class B { pub static z: i64 = seed(3); } fn main() {}",
    "1\n2\n3\n"
);
runs!(
    dependency_12_unused_helper_does_not_impose_cycle,
    "fn unused() -> i64 { return A::x; } class A { pub static x: i64 = 42; } fn main() { println(A::x); }",
    "42\n"
);
runs!(
    dependency_13_unchosen_branch_is_conservative,
    "class A { pub static x: i64 = true ? 42 : B::y; } class B { pub static y: i64 = 99; } fn main() { println(A::x); }",
    "42\n"
);
runs!(
    dependency_14_mutable_static,
    "class A { pub static mut x: i64 = B::y; } class B { pub static y: i64 = 41; } fn main() { A::x = A::x + 1; println(A::x); }",
    "42\n"
);
cycle!(
    dependency_15_self_cycle,
    "class A { pub static x: i64 = A::x; } fn main() {}",
    "A::x -> A::x"
);
cycle!(
    dependency_16_cross_class_cycle,
    "class A { pub static x: i64 = B::y; } class B { pub static y: i64 = A::x; } fn main() {}",
    "A::x",
    "B::y"
);
cycle!(
    dependency_17_helper_cycle,
    "fn seed() -> i64 { return A::x; } class A { pub static x: i64 = seed(); } fn main() {}",
    "A::x",
    "seed"
);
cycle!(
    dependency_18_long_cycle,
    "class A { pub static x: i64 = B::y; } class B { pub static y: i64 = C::z; } class C { pub static z: i64 = A::x; } fn main() {}",
    "A::x",
    "B::y",
    "C::z"
);

#[test]
fn dependency_19_imported_helper_and_alias() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "values.wi",
                "pub class C { pub static a: i64 = seed(); pub static b: i64 = 41; } pub fn seed() -> i64 { return C::b + 1; }",
            ),
            (
                "main.wi",
                "import values as v; class A { pub static x: i64 = v::seed(); } fn main() { println(A::x); println(v::C::a); }",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n42\n");
}
#[test]
fn dependency_20_item_import_and_same_names() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "a.wi",
                "pub class C { pub static x: i64 = 20; } pub fn seed() -> i64 { return C::x; }",
            ),
            (
                "b.wi",
                "pub class C { pub static x: i64 = 22; } pub fn seed() -> i64 { return C::x; }",
            ),
            (
                "main.wi",
                "import a::seed as left; import b as right; class C { pub static x: i64 = left() + right::seed(); } fn main() { println(C::x); }",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}
#[test]
fn dependency_21_gc_roots_after_reordering() {
    let source = "class A { pub static s: String = B::s + \"!\"; } class B { pub static s: String = \"hello\" + \" world\"; } fn main() { println(A::s); println(B::s); }";
    let (out, ok) = compile_and_run_gc_stress_all(source);
    assert!(ok, "{out}");
    assert_eq!(out, "hello world!\nhello world\n");
}
#[test]
fn dependency_22_indirect_call_fails_closed() {
    assert_compile_error_contains(
        "fn apply(f: fn() -> i64) -> i64 { return f(); } fn seed() -> i64 { return B::y; } class A { pub static x: i64 = apply(seed); } class B { pub static y: i64 = 42; } fn main() {}",
        &["error[E0838]", "indirect call", "A::x", "apply"],
    );
}
#[test]
fn dependency_23_example() {
    output(
        include_str!("../../example/static_dependency_graph.wi"),
        "42\n",
    );
}

runs!(
    dependency_24_super_constructor,
    "class A { pub static x: Child = new Child(); } open class Base { pub v: i64; pub init(self) { self.v = B::y; } } class Child extends Base { pub init(self) { super.init(); } } class B { pub static y: i64 = 42; } fn main() { println(A::x.v); }",
    "42\n"
);
cycle!(
    dependency_25_constructor_cycle,
    "class A { pub static x: B = new B(); } class B { pub init(self) { let other = A::x; } } fn main() {}",
    "A::x",
    "B::init"
);
runs!(
    dependency_26_lambda_creation_does_not_execute_body,
    "fn seed() -> i64 { let unused = || { return A::x; }; return 42; } class A { pub static x: i64 = seed(); } fn main() { println(A::x); }",
    "42\n"
);

#[test]
fn dependency_27_initializer_body_count_is_linear() {
    for n in [8, 32, 128] {
        let mut source = String::new();
        for i in 0..n {
            let expr = if i + 1 == n {
                "42".to_string()
            } else {
                format!("C{}::x", i + 1)
            };
            source.push_str(&format!("class C{i} {{ pub static x: i64 = {expr}; }}\n"));
        }
        source.push_str("fn main() { println(C0::x); }");
        let (ok, log) = compile_with_compiler_env(&source, &[("WILLOW_LIR_LOG", "1")]);
        assert!(ok, "{log}");
        let bodies = log
            .lines()
            .filter(|line| line.contains("[lir] compiling") && line.contains("$static_init."))
            .count();
        assert_eq!(bodies, n);
        eprintln!("static-codegen statics={n} initializer_bodies={bodies}");
        output(&source, "42\n");
    }
}

cycle!(
    dependency_28_self_constructor_cycle,
    "class A { pub static x: A = new Self(); pub init(self) { let other = A::x; } } fn main() {}",
    "A::x",
    "A::init"
);

runs!(
    dependency_29_virtual_helper_override,
    "open class Base { pub open fn seed() -> i64 { return 0; } } class Derived extends Base { pub override fn seed() -> i64 { return B::y; } } fn seed(value: Base) -> i64 { return value.seed(); } class A { pub static x: i64 = seed(new Derived()); } class B { pub static y: i64 = 42; } fn main() { println(A::x); }",
    "42\n"
);

fn dispatch_case(lib: &str, entry: &str, cyclic: bool) {
    let files = [("lib.wi", lib), ("main.wi", entry)];
    if cyclic {
        let error = compile_temp_project_error_stderr(&files, "main.wi");
        for part in [
            "error[E0838]",
            "static initialization cycle:",
            "A::x",
            "seed",
        ] {
            assert!(error.contains(part), "missing {part}: {error}");
        }
        assert!(!error.contains("incomplete"), "{error}");
    } else {
        let (out, ok) = compile_temp_project_and_run(&files, "main.wi");
        assert!(ok, "{out}");
        assert_eq!(out, "42\n");
    }
}

#[test]
fn dependency_30_cross_module_virtual_reads() {
    for read in ["B::y", "A::x"] {
        let entry = format!(
            "import lib; class Derived extends lib::Base {{ pub override fn seed() -> i64 {{ return {read}; }} }} class A {{ pub static x: i64 = lib::read(new Derived()); }} class B {{ pub static y: i64 = 42; }} fn main() {{ println(A::x); }}"
        );
        dispatch_case(
            "pub open class Base { pub open fn seed() -> i64 { return 0; } } pub fn read(v: Base) -> i64 { return v.seed(); }",
            &entry,
            read == "A::x",
        );
    }
}

#[test]
fn dependency_31_cross_module_virtual_alias_and_transitive_base() {
    for (import, base, reader) in [
        ("import lib as l;", "l::Base", "l::read"),
        ("import lib::Base as Parent;", "Parent", "lib::read"),
    ] {
        for read in ["B::y", "A::x"] {
            let entry = format!(
                "{import} open class Middle extends {base} {{}} class Derived extends Middle {{ pub override fn seed() -> i64 {{ return {read}; }} }} class A {{ pub static x: i64 = {reader}(new Derived()); }} class B {{ pub static y: i64 = 42; }} fn main() {{ println(A::x); }}"
            );
            dispatch_case(
                "pub open class Base { pub open fn seed() -> i64 { return 0; } } pub fn read(v: Base) -> i64 { return v.seed(); }",
                &entry,
                read == "A::x",
            );
        }
    }
}

#[test]
fn dependency_32_cross_module_virtual_without_subclass_is_complete() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "lib.wi",
                "pub open class Base { pub open fn seed() -> i64 { return 42; } } pub fn read(v: Base) -> i64 { return v.seed(); }",
            ),
            (
                "main.wi",
                "import lib; class A { pub static x: i64 = lib::read(new lib::Base()); } fn main() { println(A::x); }",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn dependency_33_cross_module_virtual_outside_initialization_is_allowed() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "lib.wi",
                "pub open class Base { pub open fn seed() -> i64 { return 0; } pub static fn constant() -> i64 { return 42; } } pub fn read(v: Base) -> i64 { return v.seed(); }",
            ),
            (
                "main.wi",
                "import lib; class Derived extends lib::Base { pub override fn seed() -> i64 { return A::x; } } class A { pub static x: i64 = lib::Base::constant(); } fn main() { println(lib::read(new Derived())); }",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn dependency_34_cross_module_interface_reads() {
    for (import, iface, reader) in [
        ("import lib;", "lib::I", "lib::read"),
        ("import lib as l;", "l::I", "l::read"),
        ("import lib::I as Contract;", "Contract", "lib::read"),
    ] {
        for read in ["B::y", "A::x"] {
            let entry = format!(
                "{import} class C implements {iface} {{ pub fn seed() -> i64 {{ return {read}; }} }} class A {{ pub static x: i64 = {reader}(new C()); }} class B {{ pub static y: i64 = 42; }} fn main() {{ println(A::x); }}"
            );
            dispatch_case(
                "pub interface I { fn seed() -> i64; } pub fn read(v: I) -> i64 { return v.seed(); }",
                &entry,
                read == "A::x",
            );
        }
    }
}

#[test]
fn dependency_35_cross_module_interface_inherited_and_local_union() {
    for (lib, prefix, reader) in [
        (
            "pub interface I { fn seed() -> i64; } pub interface J extends I {} pub fn read(v: J) -> i64 { return v.seed(); }",
            "import lib; class C implements lib::J { pub fn seed() -> i64 { return READ; } }",
            "lib::read",
        ),
        (
            "pub interface I { fn seed() -> i64; } pub open class Base implements I { pub open fn seed() -> i64 { return 0; } } pub fn read(v: I) -> i64 { return v.seed(); }",
            "import lib; class C extends lib::Base { pub override fn seed() -> i64 { return READ; } }",
            "lib::read",
        ),
        (
            "pub interface I { fn seed() -> i64; }",
            "import lib::I as Contract; class C implements Contract { pub fn seed() -> i64 { return READ; } } fn read(v: Contract) -> i64 { return v.seed(); }",
            "read",
        ),
    ] {
        for read in ["B::y", "A::x"] {
            let prefix = prefix.replace("READ", read);
            let entry = format!(
                "{prefix} class A {{ pub static x: i64 = {reader}(new C()); }} class B {{ pub static y: i64 = 42; }} fn main() {{ println(A::x); }}"
            );
            dispatch_case(lib, &entry, read == "A::x");
        }
    }
}

#[test]
fn dependency_36_interface_dispatch_after_startup_is_allowed() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "lib.wi",
                "pub interface I { fn seed() -> i64; } pub fn read(v: I) -> i64 { return v.seed(); }",
            ),
            (
                "main.wi",
                "import lib; class C implements lib::I { pub fn seed() -> i64 { return A::x; } } class A { pub static x: i64 = 42; } fn main() { println(lib::read(new C())); }",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

runs!(
    dependency_37_same_unit_interface_dependencies_are_ordered,
    "interface I { fn seed() -> i64; } class C implements I { pub fn seed() -> i64 { return B::y; } } fn read(v: I) -> i64 { return v.seed(); } class A { pub static x: i64 = read(new C()); } class B { pub static y: i64 = 42; } fn main() { println(A::x); }",
    "42\n"
);

#[test]
fn dependency_38_interface_implementation_inherits_body() {
    for read in ["B::y", "A::x"] {
        let entry = format!(
            "import lib; open class Parent {{ pub fn seed() -> i64 {{ return {read}; }} }} class C extends Parent implements lib::I {{}} class A {{ pub static x: i64 = lib::read(new C()); }} class B {{ pub static y: i64 = 42; }} fn main() {{ println(A::x); }}"
        );
        dispatch_case(
            "pub interface I { fn seed() -> i64; } pub fn read(v: I) -> i64 { return v.seed(); }",
            &entry,
            read == "A::x",
        );
    }
}

#[test]
fn dependency_39_three_module_class_and_interface_chains() {
    for (lib, middle, declaration) in [
        (
            "pub open class Base { pub open fn seed() -> i64 { return 0; } } pub fn read(v: Base) -> i64 { return v.seed(); }",
            "import lib; pub open class Middle extends lib::Base {}",
            "class C extends m::Middle { pub override fn seed() -> i64 { return READ; } }",
        ),
        (
            "pub interface I { fn seed() -> i64; } pub fn read(v: I) -> i64 { return v.seed(); }",
            "import lib; pub interface J extends lib::I {}",
            "class C implements m::J { pub fn seed() -> i64 { return READ; } }",
        ),
    ] {
        for read in ["B::y", "A::x"] {
            let declaration = declaration.replace("READ", read);
            let entry = format!(
                "import lib; import middle as m; {declaration} class A {{ pub static x: i64 = lib::read(new C()); }} class B {{ pub static y: i64 = 42; }} fn main() {{ println(A::x); }}"
            );
            let files = [("lib.wi", lib), ("middle.wi", middle), ("main.wi", &entry)];
            if read == "A::x" {
                let error = compile_temp_project_error_stderr(&files, "main.wi");
                for part in ["E0838", "static initialization cycle:", "A::x", "seed"] {
                    assert!(error.contains(part), "{error}");
                }
            } else {
                let (out, ok) = compile_temp_project_and_run(&files, "main.wi");
                assert!(ok, "{out}");
                assert_eq!(out, "42\n");
            }
        }
    }
}

fn unsupported(source: &str, reason: &str) {
    let error = compile_error_stderr(source);
    for part in [
        "error[E0838]",
        "unsupported static initialization",
        reason,
        "directly resolved helper",
        "after startup in main",
    ] {
        assert!(error.contains(part), "missing {part}: {error}");
    }
    assert!(!error.contains("static initialization cycle:"), "{error}");
}

#[test]
fn dependency_40_runtime_callbacks_are_explicitly_unsupported() {
    for (import, api) in [
        ("import std::parallel;", "parallel::map"),
        ("import std::parallel as par;", "par::map"),
    ] {
        for read in ["B::y", "A::x"] {
            let source = format!(
                "import std::collections::Array; {import} fn mapper(v: i64) -> i64 {{ return v + {read}; }} fn start() -> i64 {{ let values: Array<i64> = [1]; let task = {api}(values.freeze(), mapper); return 42; }} class A {{ pub static x: i64 = start(); }} class B {{ pub static y: i64 = 42; }} fn main() {{}}"
            );
            unsupported(&source, "runtime callback API (parallel::map)");
        }
    }
}

#[test]
fn dependency_41_injected_defaults_have_their_own_reason() {
    unsupported(
        "interface Seed { fn seed(self) -> i64 { return 42; } } class C implements Seed {} class A { pub static x: i64 = new C().seed(); } fn main() {}",
        "injected interface default method",
    );
}

#[test]
fn dependency_42_indirect_diagnostics_are_not_cycle_diagnostics() {
    unsupported(
        "fn seed() -> i64 { return 42; } fn call(f: fn() -> i64) -> i64 { return f(); } class A { pub static x: i64 = call(seed); } fn main() {}",
        "indirect call through a function value",
    );
    let error = compile_error_stderr("class A { pub static x: i64 = A::x; } fn main() {}");
    assert!(error.contains("static initialization cycle:"), "{error}");
    assert!(
        !error.contains("unsupported static initialization"),
        "{error}"
    );
    assert!(!error.contains("after startup in main"), "{error}");
}

#[test]
fn dependency_43_example_negative_snippets_are_checked() {
    let example = include_str!("../../example/static_dependency_graph.wi");
    for (name, reason) in [
        ("callback", "runtime callback API"),
        ("function-value", "indirect call"),
        ("injected-default", "injected interface default method"),
    ] {
        let source = example
            .split(&format!("/* example: {name}\n"))
            .nth(1)
            .unwrap()
            .split("*/")
            .next()
            .unwrap();
        unsupported(source, reason);
    }
    let cycle = example
        .split("/* example: cycle\n")
        .nth(1)
        .unwrap()
        .split("*/")
        .next()
        .unwrap();
    assert_compile_error_contains(
        cycle,
        &["E0838", "static initialization cycle:", "A::x", "B::y"],
    );
}
