//! Provider-owned types in injected default signatures (willow-srv6).
use super::support::*;

fn runs(provider: &str, main: &str, expected: &str) {
    let project = TestProject::new(
        "provider_signatures",
        &[("util.wi", provider), ("main.wi", main)],
    );
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let output = project.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
}

#[test]
fn provider_signature_ticket_repro() {
    runs(
        "module util; pub interface Get { fn n(self) -> i64; fn bad(self, g: Get) -> i64 { return g.n(); } }",
        "import util; class A implements util::Get { pub fn n(self) -> i64 { return 1; } } fn main() { println(new A().bad(new A())); }",
        "1\n",
    );
}

fn signature_matrix(imports: &str, contract: &str, generic: bool) {
    let decl = if generic { "Get<T>" } else { "Get" };
    let extra = if generic {
        "fn value(self, x: T) -> T { return x; }"
    } else {
        ""
    };
    let provider = format!(
        r#"
        module util;
        pub class Payload {{ pub value: i64; }}
        pub enum Kind {{ First, Second }}
        pub interface View {{ fn n(self) -> i64; }}
        pub interface {decl} {{
            fn n(self) -> i64;
            fn bad(self, g: View) -> i64 {{ return g.n(); }}
            fn view(self, g: View) -> View {{ return g; }}
            fn payload(self, p: Payload) -> Payload {{ return p; }}
            fn kind(self, k: Kind) -> Kind {{ return k; }}
            fn wrapped(self, p: Option<Payload>) -> Option<Payload> {{ return p; }}
            {extra}
        }}
    "#
    );
    let ty = if generic {
        format!("{contract}<i64>")
    } else {
        contract.into()
    };
    let extra = if generic { "println(a.value(9));" } else { "" };
    let prefix = if imports.contains("as u;") {
        "u::"
    } else if imports.contains("util::Get") {
        ""
    } else {
        "util::"
    };
    let type_imports = if prefix.is_empty() {
        "import util::{Payload as ForeignPayload, Kind as ForeignKind, View as ForeignView};"
    } else {
        ""
    };
    let main = format!(
        r#"
        {imports}
        {type_imports}
        class Payload {{ pub unused: String; }}
        enum Kind {{ Wrong }}
        interface View {{ fn different(self) -> String; }}
        class A implements {ty}, util::View {{ pub fn n(self) -> i64 {{ return 7; }} }}
        fn dynamic(g: {ty}, p: util::Payload) -> i64 {{ return g.payload(p).value; }}
        fn main() {{
            let a = new A();
            println(a.bad(a));
            println(a.view(a).n());
            println(a.payload(new util::Payload(3)).value);
            match a.kind(util::Kind::Second) {{ util::Kind::First => println(0), util::Kind::Second => println(4) }}
            match a.wrapped(Option::Some(new util::Payload(5))) {{ Option::Some(p) => println(p.value), Option::None => println(0) }}
            println(dynamic(a, new util::Payload(6)));
            {extra}
        }}
    "#
    );
    let main = if prefix.is_empty() {
        main.replace("util::Payload", "ForeignPayload")
            .replace("util::Kind", "ForeignKind")
            .replace("util::View", "ForeignView")
    } else {
        main.replace("util::Payload", &format!("{prefix}Payload"))
            .replace("util::Kind", &format!("{prefix}Kind"))
            .replace("util::View", &format!("{prefix}View"))
    };
    runs(
        &provider,
        &main,
        if generic {
            "7\n7\n3\n4\n5\n6\n9\n"
        } else {
            "7\n7\n3\n4\n5\n6\n"
        },
    );
}

#[test]
fn provider_signature_plain() {
    signature_matrix("import util;", "util::Get", false);
}
#[test]
fn provider_signature_generic() {
    signature_matrix("import util;", "util::Get", true);
}
#[test]
fn provider_signature_module_alias() {
    signature_matrix("import util as u;", "u::Get", false);
}
#[test]
fn provider_signature_generic_module_alias() {
    signature_matrix("import util as u;", "u::Get", true);
}
#[test]
fn provider_signature_item_import() {
    signature_matrix("import util::Get;", "Get", false);
}
#[test]
fn provider_signature_generic_item_import() {
    signature_matrix("import util::Get;", "Get", true);
}
#[test]
fn provider_signature_item_alias() {
    signature_matrix("import util::Get as Contract;", "Contract", false);
}
#[test]
fn provider_signature_generic_item_alias() {
    signature_matrix("import util::Get as Contract;", "Contract", true);
}

#[test]
fn provider_signature_generic_ticket_repro() {
    runs(
        "module util; pub interface Get { fn n(self) -> i64; } pub interface Use<T> { fn bad(self, g: Get) -> i64 { return g.n(); } }",
        "import util; class A implements util::Use<i64>, util::Get { pub fn n(self) -> i64 { return 1; } } fn main() { println(new A().bad(new A())); }",
        "1\n",
    );
}

#[test]
fn provider_signature_generic_receiver_and_local_argument() {
    runs(
        "module util; pub class Payload { pub value: i64; } pub interface Get<T> { fn echo(self, p: Payload) -> Payload { return p; } fn keep(self, t: T) -> T { return t; } }",
        "import util; class Local { pub n: i64; } class A<T> implements util::Get<T> {} fn main() { println(new A<i64>().echo(new util::Payload(8)).value); println(new A<Local>().echo(new util::Payload(9)).value); println(new A<Local>().keep(new Local(10)).n); }",
        "8\n9\n10\n",
    );
}

#[test]
fn provider_signature_inherited_from_third_module() {
    let project = TestProject::new(
        "provider_signature_inheritance",
        &[
            (
                "root.wi",
                "module root; pub class Payload { pub n: i64; } pub interface Base { fn echo(self, p: Payload) -> Payload { return p; } }",
            ),
            (
                "util.wi",
                "module util; import root as r; pub class Payload { pub wrong: String; } pub interface Get extends r::Base {}",
            ),
            (
                "consumer.wi",
                "module consumer; import util::Get as G; pub class A implements G {}",
            ),
            (
                "main.wi",
                "import consumer; import root; fn main() { println(new consumer::A().echo(new root::Payload(11)).n); }",
            ),
        ],
    );
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let output = project.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "11\n");
}

#[test]
fn provider_signature_wrong_importer_type_is_rejected() {
    let project = TestProject::new(
        "provider_signature_wrong_type",
        &[
            (
                "util.wi",
                "module util; pub class Payload { pub n: i64; } pub interface Get { fn echo(self, p: Payload) -> Payload { return p; } }",
            ),
            (
                "main.wi",
                "import util; class Payload { pub n: i64; } class A implements util::Get {} fn main() { new A().echo(new Payload(1)); }",
            ),
        ],
    );
    let compiled = project.compile("main.wi");
    let error = String::from_utf8_lossy(&compiled.stderr);
    assert!(!compiled.status.success());
    assert!(error.contains("E0704"), "{error}");
    assert!(!error.contains("E0416"), "{error}");
}

#[test]
fn provider_signature_example_runs() {
    let project = TestProject::new(
        "provider_signature_example",
        &[
            (
                "main.wi",
                include_str!("../../example/interface_default_provider_signatures.wi"),
            ),
            (
                "interface_default_provider_signatures_support/provider.wi",
                include_str!(
                    "../../example/interface_default_provider_signatures_support/provider.wi"
                ),
            ),
        ],
    );
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let output = project.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n7\n");
}

#[test]
fn provider_signature_function_closure_array_and_reference_types() {
    runs(
        r#"module util; import std::collections::Array;
        pub class Payload { pub value: i64; }
        pub interface Get {
            fn function(self, f: fn(Payload) -> Payload, p: Payload) -> Payload { return f(p); }
            fn closure(self, f: closure(Payload) -> Payload, p: Payload) -> Payload { return f(p); }
            fn array(self, ps: Array<Payload>) -> Array<Payload> { return ps; }
            fn reference(self, p: &Payload) -> i64 { return p.value; }
        }"#,
        r#"import util; import std::collections::Array;
        class A implements util::Get {}
        fn identity(p: util::Payload) -> util::Payload { return p; }
        fn main() {
            let a = new A();
            println(a.function(identity, new util::Payload(12)).value);
            let offset = 0; let f = |p: util::Payload| -> util::Payload { return new util::Payload(p.value + offset); };
            println(a.closure(f, new util::Payload(13)).value);
            let ps = a.array([new util::Payload(14)]); println(ps[0].value);
            let p = new util::Payload(15); println(a.reference(&p));
        }"#,
        "12\n13\n14\n15\n",
    );
}

#[test]
fn provider_signature_imported_type_aliases() {
    for import in ["import deep as d;", "import deep::Payload as Foreign;"] {
        let ty = if import.contains("as d;") {
            "d::Payload"
        } else {
            "Foreign"
        };
        let provider = format!(
            "module util; {import} pub interface Get {{ fn echo(self, p: {ty}) -> {ty} {{ return p; }} }}"
        );
        let project = TestProject::new(
            "provider_signature_aliases",
            &[
                (
                    "deep.wi",
                    "module deep; pub class Payload { pub n: i64; } pub fn make() -> Payload { return new Payload(16); }",
                ),
                ("util.wi", &provider),
                (
                    "main.wi",
                    "import util; import deep; class A implements util::Get {} fn main() { println(new A().echo(deep::make()).n); }",
                ),
            ],
        );
        let compiled = project.compile("main.wi");
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let output = project.run();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "16\n");
    }
}

#[test]
fn provider_signature_override_and_inherited_class_copy() {
    runs(
        "module util; pub class Payload { pub n: i64; } pub interface Get { fn echo(self, p: Payload) -> Payload { return p; } }",
        "import util; open class Base implements util::Get {} class Child extends Base {} class Override implements util::Get { pub fn echo(self, p: util::Payload) -> util::Payload { return new util::Payload(p.n + 1); } } fn main() { println(new Child().echo(new util::Payload(17)).n); println(new Override().echo(new util::Payload(17)).n); }",
        "17\n18\n",
    );
}

#[test]
fn provider_signature_generic_interface_names_itself() {
    runs(
        "module util; pub interface Get<T> { fn n(self) -> i64; fn bad(self, g: Get<T>) -> i64 { return g.n(); } fn peer(self, g: Get<T>) -> Get<T> { return g; } }",
        "import util::Get as G; class A implements G<i64> { pub fn n(self) -> i64 { return 19; } } fn main() { println(new A().bad(new A())); println(new A().peer(new A()).n()); }",
        "19\n19\n",
    );
}

#[test]
fn provider_signature_lir_keeps_one_default_per_receiver() {
    let project = TestProject::new(
        "provider_signature_lir",
        &[
            (
                "util.wi",
                "module util; pub class Payload { pub n: i64; } pub interface Get { fn echo(self, p: Payload) -> Payload { return p; } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get {} class B implements util::Get {} fn main() { println(new A().echo(new util::Payload(20)).n); println(new B().echo(new util::Payload(21)).n); }",
            ),
        ],
    );
    let output = project.emit_lir("main.wi");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lir = String::from_utf8_lossy(&output.stdout);
    assert_eq!(lir.matches("fn A::echo(").count(), 1, "{lir}");
    assert_eq!(lir.matches("fn B::echo(").count(), 1, "{lir}");
}

#[test]
fn provider_signature_tuple_keeps_structural_identity() {
    runs(
        "module util; pub class Payload { pub n: i64; } pub interface Get { fn pair(self, p: (Payload, i64)) -> (Payload, i64) { return p; } }",
        "import util; class A implements util::Get {} fn main() { let (p, n) = new A().pair((new util::Payload(20), 1)); println(p.n + n); }",
        "21\n",
    );
}
