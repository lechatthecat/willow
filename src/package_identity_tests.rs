use super::*;
use compiler_db::ids::BodyOwner;
use semantic::ids::{FunctionId, SymbolId, TypeId};

#[test]
fn package_identity_checked_declarations_and_bodies_preserve_origins() {
    let fixture = single_file_import_tests::package_fixture();
    for package in ["a", "b"] {
        fixture.file(&format!("{package}/src/util.wi"), "module util; pub class Value { pub static n: i64 = 1; pub init(self) {} pub fn read(self) -> i64 { return 1; } } pub fn value() -> i64 { return 2; }");
    }
    let mut previous = None;
    for imports in [
        "import a::util as first; import b::util as second; import same::util as again;",
        "import same::util as again; import b::util as second; import a::util as first;",
    ] {
        let (frontend, diagnostics) = fixture.frontend(&format!("{imports} fn main() {{}}"), true);
        let frontend = frontend.unwrap_or_else(|e| panic!("{e}: {diagnostics:?}"));
        let graph = &frontend.module_graph;
        assert_eq!(graph.files.len(), 2);
        let checked = frontend
            .db
            .unit_declarations(module::UnitId::ENTRY, graph.artifacts.as_ref().unwrap())
            .unwrap();
        let type_of = |access: &str| {
            TypeId::from_source_name(&checked.symbols.lookup_class(access).unwrap().name)
        };
        let first = type_of("first::Value");
        let second = type_of("second::Value");
        assert_eq!(first, type_of("again::Value"));
        assert_ne!(first, second);
        assert!(first.module().is_some());
        assert!(second.module().is_some());
        let function_of = |access: &str| {
            checked
                .symbols
                .lookup_module(access)
                .unwrap()
                .functions
                .scope()
                .lookup_id("value")
        };
        assert_eq!(function_of("first"), function_of("again"));
        assert_ne!(function_of("first"), function_of("second"));
        assert_eq!(function_of("first").module(), first.module());

        let mut functions = std::collections::BTreeMap::new();
        for module in &graph.files {
            let origin = module.symbol_module.unwrap();
            for item in &module.program.items {
                if let parser::ast::Item::Function(function) = item {
                    let (unit, BodyOwner::Function(id)) =
                        frontend.db.bodies().owner(function.body.id).unwrap()
                    else {
                        panic!("function owner");
                    };
                    assert_eq!(unit, module.id);
                    assert_eq!(id.module(), Some(origin));
                    assert_eq!(id, FunctionId::free("value").in_module(origin));
                    functions.insert(origin.package().name.clone(), id);
                }
                if let parser::ast::Item::Class(class) = item {
                    let (_, BodyOwner::Function(id)) = frontend
                        .db
                        .bodies()
                        .owner(class.methods[0].body.id)
                        .unwrap()
                    else {
                        panic!("method owner");
                    };
                    assert_eq!(
                        id.owner_type(),
                        Some(TypeId::local("Value").in_module(origin))
                    );
                    let static_id = frontend
                        .db
                        .bodies()
                        .static_id(class.fields[0].initializer.as_ref().unwrap().id())
                        .unwrap();
                    assert_eq!(static_id.owner.module(), Some(origin));
                }
            }
        }
        assert_ne!(functions["a"], functions["b"]);
        for symbol in [
            SymbolId::Type(first),
            SymbolId::Type(second),
            SymbolId::Function(functions["a"]),
            SymbolId::Function(functions["b"]),
        ] {
            let serialized = serde_json::to_string(&symbol).unwrap();
            assert_eq!(
                serde_json::from_str::<SymbolId>(&serialized).unwrap(),
                symbol
            );
            assert!(!serialized.contains("first"));
            assert!(!serialized.contains("again"));
        }
        let identities = (first, second, functions);
        if let Some(previous) = &previous {
            assert_eq!(&identities, previous);
        }
        previous = Some(identities);
    }
}

#[test]
fn package_identity_helper_effects_follow_module_and_item_aliases() {
    let fixture = single_file_import_tests::package_fixture();
    fixture.file("a/src/util.wi", "module util; pub fn heavy() -> i64 { while true {} return 1; } pub class Work { pub fn heavy(self) -> i64 { while true {} return 1; } }");
    fixture.file("b/src/util.wi", "module util; pub fn heavy() -> i64 { return 2; } pub class Work { pub fn heavy(self) -> i64 { return 2; } }");
    for (package, expected_error) in [("a", true), ("b", false), ("same", true)] {
        for (imports, call) in [
            (
                format!("import {package}::util as jobs;"),
                "jobs::heavy();".to_string(),
            ),
            (
                format!("import {package}::util::heavy as run;"),
                "run();".to_string(),
            ),
            (
                format!("import {package}::util::Work as Job;"),
                "let w: Job = new Job(); w.heavy();".to_string(),
            ),
        ] {
            let source = format!("{imports} async fn main() {{ {call} }}");
            fixture.file("src/main.wi", &source);
            let root = fixture.0.join("src");
            let mut inputs =
                compiler_db::inputs::CompilerInputs::native(CompilerOptions::debug(), root.clone())
                    .resolve_project(Some(&fixture.0))
                    .unwrap();
            inputs.target.sync_stack_preemption = false;
            let map = diagnostics::SourceMap::new(
                root.join("main.wi").to_string_lossy().into_owned(),
                &source,
            );
            let mut emitter = single_file_import_tests::Capture::default();
            let result = run_frontend_with_inputs(&source, &root, &map, inputs, &mut emitter);
            assert_eq!(
                result.is_err(),
                expected_error,
                "{imports}: {:?}",
                emitter.0
            );
            if expected_error {
                assert!(
                    emitter
                        .0
                        .iter()
                        .any(|d| d.code == diagnostics::ErrorCode::E0810),
                    "{:?}",
                    emitter.0
                );
            }
        }
    }
}

#[test]
fn package_identity_effect_queries_resolve_each_consumer() {
    let fixture = single_file_import_tests::package_fixture();
    fixture.file("a/src/util.wi", "module util; pub fn value() -> i64 { panic(\"expected\"); return 1; } pub class Work { pub fn value(self) -> i64 { panic(\"expected\"); return 1; } }");
    fixture.file("b/src/util.wi", "module util; pub fn value() -> i64 { return 2; } pub class Work { pub fn value(self) -> i64 { return 2; } }");
    for (package, may_panic) in [("a", true), ("b", false), ("same", true)] {
        for (imports, params, body) in [
            (
                format!("import {package}::util as jobs;"),
                "",
                "return jobs::value();",
            ),
            (
                format!("import {package}::util::value as run;"),
                "",
                "return run();",
            ),
            (
                format!("import {package}::util::Work as Job;"),
                "w: Job",
                "return w.value();",
            ),
        ] {
            let (frontend, diagnostics) = fixture.frontend(
                &format!("{imports} fn call({params}) -> i64 {{ {body} }} fn main() {{}}"),
                true,
            );
            let frontend = frontend.unwrap_or_else(|e| panic!("{e}: {diagnostics:?}"));
            assert_eq!(
                frontend
                    .db
                    .effects
                    .panic(module::UnitId::ENTRY, FunctionId::free("call")),
                may_panic,
                "{imports}"
            );
        }
    }
}
