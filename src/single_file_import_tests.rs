use super::*;
use diagnostics::{Diagnostic, DiagnosticEmitter, ErrorCode, SourceMap};
use std::sync::atomic::{AtomicU64, Ordering};

const HINT: &str = "external dependency requires a project.toml";

pub(super) struct Fixture(pub(super) PathBuf);
impl Fixture {
    pub(super) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-single-file-import-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        Self(root)
    }
    pub(super) fn file(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn check(&self, source: &str, project: bool) -> (bool, Vec<Diagnostic>) {
        let (result, diagnostics) = self.frontend(source, project);
        (result.is_ok(), diagnostics)
    }
    pub(super) fn frontend(
        &self,
        source: &str,
        project: bool,
    ) -> (Result<Frontend>, Vec<Diagnostic>) {
        self.file("src/main.wi", source);
        let root = self.0.join("src");
        let inputs =
            compiler_db::inputs::CompilerInputs::native(CompilerOptions::debug(), root.clone())
                .resolve_project(project.then_some(self.0.as_path()))
                .unwrap();
        let map = SourceMap::new(root.join("main.wi").to_string_lossy().into_owned(), source);
        let mut emitter = Capture::default();
        let result = run_frontend_with_inputs(source, &root, &map, inputs, &mut emitter);
        (result, emitter.0)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
pub(super) struct Capture(pub(super) Vec<Diagnostic>);
impl DiagnosticEmitter for Capture {
    fn emit(
        &mut self,
        diagnostic: &Diagnostic,
        _: &dyn diagnostics::source_map::SourceLookup,
    ) -> std::io::Result<()> {
        self.0.push(diagnostic.clone());
        Ok(())
    }
}
fn has_hint(d: &Diagnostic) -> bool {
    d.notes.iter().any(|note| note.contains(HINT))
}

#[test]
fn single_file_missing_qualified_import_explains_project_requirement() {
    for import in [
        "dep::api",
        "dep::api as api",
        "dep::api::value",
        "dep::api::value as v",
        "local::typo",
    ] {
        let f = Fixture::new();
        let (ok, diagnostics) = f.check(&format!("import {import}; fn main() {{}}"), false);
        assert!(!ok);
        let error = diagnostics
            .iter()
            .find(|d| d.code == ErrorCode::E0401)
            .unwrap();
        assert!(has_hint(error), "{diagnostics:?}");
        assert!(error.message.starts_with("unresolved import"));
        assert!(error.notes[0].contains("tried to find module"));
        assert!(!f.0.join("project.toml").exists());
        assert!(!f.0.join("src/project.toml").exists());
    }
}

#[test]
fn single_file_local_module_item_alias_and_std_imports_still_work() {
    for import in [
        "local",
        "local as l",
        "local::value",
        "local::value as v",
        "nested::local",
        "nested::local as l",
        "nested::local::value",
        "nested::local::value as v",
        "std::io",
    ] {
        for layout in ["nested/local.wi", "nested/local/mod.wi"] {
            let f = Fixture::new();
            f.file(
                "src/local.wi",
                "module local; pub fn value() -> i64 { return 1; }",
            );
            f.file(
                &format!("src/{layout}"),
                "module nested::local; pub fn value() -> i64 { return 1; }",
            );
            let (ok, diagnostics) = f.check(&format!("import {import}; fn main() {{}}"), false);
            assert!(ok, "{import}: {diagnostics:?}");
            assert!(!diagnostics.iter().any(has_hint));
        }
    }
}

#[test]
fn single_file_keeps_bare_missing_and_builtin_diagnostics() {
    for import in ["missing", "std::nonexistent", "std::nonexistent::item"] {
        let f = Fixture::new();
        let (ok, diagnostics) = f.check(&format!("import {import}; fn main() {{}}"), false);
        assert!(!ok);
        assert!(!diagnostics.iter().any(has_hint), "{diagnostics:?}");
    }
}

#[test]
fn single_file_does_not_discover_adjacent_or_ancestor_manifests() {
    for path in ["project.toml", "src/project.toml"] {
        let f = Fixture::new();
        // Invalid on purpose: an implicit manifest read would fail before imports.
        f.file(path, "not a valid manifest");
        let (ok, diagnostics) = f.check("import dep::api; fn main() {}", false);
        assert!(!ok);
        assert!(diagnostics.iter().any(has_hint));
        assert_eq!(
            std::fs::read_to_string(f.0.join(path)).unwrap(),
            "not a valid manifest"
        );
    }
}

#[test]
fn explicit_legacy_and_versioned_projects_keep_their_own_diagnostics() {
    for marker in ["", "\n[willow]\nmanifest-version = 1\n"] {
        let f = Fixture::new();
        f.file(
            "project.toml",
            &format!("[project]\nname = 'app'\nversion = '1.0.0'\n{marker}"),
        );
        let (ok, diagnostics) = f.check("import dep::api; fn main() {}", true);
        assert!(!ok);
        assert!(diagnostics.iter().any(|d| d.code == ErrorCode::E0401));
        assert!(!diagnostics.iter().any(has_hint), "{diagnostics:?}");
    }
}

#[test]
fn single_file_nested_failure_retains_imported_file_span() {
    let f = Fixture::new();
    f.file("src/local.wi", "module local; import dep::api;");
    let (ok, diagnostics) = f.check("import local; fn main() {}", false);
    assert!(!ok);
    let error = diagnostics.iter().find(|d| has_hint(d)).unwrap();
    assert_ne!(
        error.primary_span().unwrap().file_id,
        diagnostics::FileId::ENTRY
    );
}

#[test]
fn single_file_missing_import_diagnostics_scale_per_occurrence() {
    for size in [16, 64, 256, 1024] {
        let f = Fixture::new();
        let source = (0..size)
            .map(|i| format!("import dep::m{i};"))
            .collect::<String>();
        let program = parser::Parser::new(lexer::Lexer::new(&source).tokenize().unwrap())
            .parse()
            .0;
        let resolution = module::resolve_imports(&program, &f.0.join("src"));
        assert_eq!(resolution.graph.import_routes, size);
        assert_eq!(resolution.graph.source_loads, 0);
        assert_eq!(resolution.diagnostics.len(), size);
        assert_eq!(
            resolution
                .diagnostics
                .iter()
                .filter(|d| has_hint(d))
                .count(),
            size
        );
        eprintln!("single-file imports={size} routes={size} hints={size} source_loads=0");
    }
}

#[test]
fn package_entry_uses_consumer_alias_for_modules_and_items() {
    for (imports, expression) in [
        ("import dep::api;", "api::value()"),
        ("import dep::api as a;", "a::value()"),
        ("import dep::api::value;", "value()"),
        ("import dep::api::value as v;", "v()"),
        (
            "import dep::api as a; import other::api as b;",
            "a::value() + b::value()",
        ),
        (
            "import dep::api::value as a; import other::api::value as b;",
            "a() + b()",
        ),
    ] {
        let f = Fixture::new();
        f.file("project.toml", "[project]\nname = 'app'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n[dependencies]\ndep = { path = 'dep' }\nother = { path = 'dep' }\n");
        f.file(
            "dep/project.toml",
            "[project]\nname = 'dependency'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n",
        );
        f.file(
            "dep/src/api.wi",
            "module api; pub fn value() -> i64 { return 7; }",
        );
        let (ok, diagnostics) = f.check(
            &format!("{imports} fn main() {{ let x: i64 = {expression}; println(x); }}"),
            true,
        );
        assert!(ok, "{imports}: {diagnostics:?}");
    }
}

pub(super) fn package_fixture() -> Fixture {
    let f = Fixture::new();
    f.file("project.toml", "[project]\nname = 'app'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n[dependencies]\na = { path = 'a' }\nsame = { path = 'a' }\nb = { path = 'b' }\n");
    for name in ["a", "b"] {
        std::fs::create_dir_all(f.0.join(name).join("src")).unwrap();
        f.file(
            &format!("{name}/project.toml"),
            &format!(
                "[project]\nname = '{name}'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n"
            ),
        );
    }
    f
}

#[test]
fn package_entry_item_lookup_keeps_same_named_packages_separate() {
    for imports in [
        "import a::api::value as number; import b::api::value as text;",
        "import b::api::value as text; import a::api::value as number;",
    ] {
        let f = package_fixture();
        f.file(
            "a/src/api.wi",
            "module api; pub fn value() -> i64 { return 7; }",
        );
        f.file(
            "b/src/api.wi",
            "module api; pub fn value() -> String { return \"text\"; }",
        );
        let (ok, diagnostics) = f.check(&format!("{imports} fn main() {{ let n: i64 = number(); let s: String = text(); println(n); println(s); }}"), true);
        assert!(ok, "{diagnostics:?}");
        let (ok, diagnostics) = f.check(
            &format!("{imports} fn main() {{ let n: i64 = text(); println(n); }}"),
            true,
        );
        assert!(!ok);
        assert!(
            diagnostics.iter().any(|d| d.code == ErrorCode::E0201),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn package_entry_checks_item_visibility_through_aliases() {
    for declaration in [
        "fn hidden() {}",
        "class Hidden {}",
        "interface Hidden {}",
        "enum Hidden { Value }",
    ] {
        let item = if declaration.starts_with("fn ") {
            "hidden"
        } else {
            "Hidden"
        };
        for alias in ["a", "same"] {
            let f = package_fixture();
            f.file("a/src/api.wi", &format!("module api; {declaration}"));
            let (ok, diagnostics) = f.check(
                &format!("import {alias}::api::{item} as Local; fn main() {{}}"),
                true,
            );
            assert!(!ok);
            assert!(
                diagnostics.iter().any(|d| d.message.contains("private")),
                "{diagnostics:?}"
            );
        }
    }
}

#[test]
fn package_entry_reuses_types_across_package_aliases() {
    for declaration in [
        "pub class Value {}",
        "pub interface Value {}",
        "pub enum Value { V }",
    ] {
        for (imports, left, right) in [
            (
                "import a::api as first; import same::api as second;",
                "first::Value",
                "second::Value",
            ),
            (
                "import same::api as second; import a::api as first;",
                "first::Value",
                "second::Value",
            ),
            (
                "import a::api::Value as Left; import same::api::Value as Right;",
                "Left",
                "Right",
            ),
            (
                "import same::api::Value as Right; import a::api::Value as Left;",
                "Left",
                "Right",
            ),
        ] {
            let f = package_fixture();
            f.file("a/src/api.wi", &format!("module api; {declaration}"));
            let (ok, diagnostics) = f.check(
                &format!(
                    "{imports} fn accept(x: {left}) -> {right} {{ return x; }} fn main() {{}}"
                ),
                true,
            );
            assert!(ok, "{declaration}: {diagnostics:?}");
        }
    }
}

#[test]
fn package_entry_handles_dependency_first_alias_and_hides_transitive_names() {
    let f = package_fixture();
    f.file(
        "a/src/api.wi",
        "module api; pub fn value() -> i64 { return 7; }",
    );
    f.file("b/project.toml", "[project]\nname = 'b'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n[dependencies]\ninner = { path = '../a' }\n");
    f.file("b/src/wrapper.wi", "module wrapper; import inner::api as hidden; pub fn value() -> i64 { return hidden::value(); }");
    for imports in [
        "import b::wrapper; import a::api;",
        "import a::api; import b::wrapper;",
    ] {
        let (ok, diagnostics) = f.check(
            &format!("{imports} fn main() {{ println(api::value()); println(wrapper::value()); }}"),
            true,
        );
        assert!(ok, "{diagnostics:?}");
    }
    for name in ["hidden", "api", "inner"] {
        let (ok, diagnostics) = f.check(
            &format!("import b::wrapper; fn main() {{ println({name}::value()); }}"),
            true,
        );
        assert!(!ok, "{name}");
        assert!(
            diagnostics.iter().any(|d| d.code == ErrorCode::E0350),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn package_entry_binding_work_scales_with_imports_not_module_product() {
    use crate::package::*;
    for size in [16, 64, 256, 1024] {
        let packages = PackageGraph {
            root: PackageId(0),
            packages: (0..2)
                .map(|id| ResolvedPackage {
                    checksum: None,
                    id: PackageId(id),
                    identity: PackageIdentity {
                        name: format!("p{id}"),
                        version: "1.0.0".into(),
                        source: PackageSourceIdentity::Path {
                            path: format!("p{id}").into(),
                        },
                        revision: None,
                    },
                    root: format!("p{id}").into(),
                    dependencies: if id == 0 {
                        vec![ResolvedDependency {
                            selector: None,
                            alias: "dep".into(),
                            package: PackageId(1),
                        }]
                    } else {
                        vec![]
                    },
                })
                .collect(),
            stats: Default::default(),
        };
        let parse = |s: &str| {
            parser::Parser::new(lexer::Lexer::new(s).tokenize().unwrap())
                .parse()
                .0
        };
        let modules: Vec<_> = (0..size)
            .map(|i| module::ResolvedModule {
                id: module::ModuleId(i as u32 + 1),
                package: PackageId(1),
                symbol_module: None,
                name: format!("m{i}"),
                canonical_path: format!("m{i}"),
                path: format!("m{i}.wi").into(),
                source: String::new(),
                program: parse(""),
            })
            .collect();
        let dependencies = ModuleDependencies::with_packages(&modules, Some(&packages));
        for imports in [1, 8, size] {
            for repeated in [false, true] {
                let source = (0..imports).map(|i| {
                    let m = if repeated { 0 } else { i };
                    format!("import dep::m{m} as a{i}; import dep::m{m} as a{i}; import dep::m{m}::value as v{i};")
                }).collect::<String>();
                let program = parse(&source);
                let bindings = ModuleImportBindings::new(&program, &dependencies);
                assert_eq!(bindings.path_lookups, 4 * imports);
                assert_eq!(bindings.items.len(), imports);
                assert_eq!(bindings.imported.len(), if repeated { 1 } else { imports });
                assert_eq!(
                    bindings.imported.values().map(Vec::len).sum::<usize>(),
                    if repeated { imports + 1 } else { 2 * imports }
                );
                eprintln!(
                    "entry-bindings modules={size} imports={} repeated={repeated} lookups={} stored_spellings={}",
                    3 * imports,
                    bindings.path_lookups,
                    bindings.imported.values().map(Vec::len).sum::<usize>()
                );
            }
        }
    }
}

#[test]
fn package_entry_binding_prefers_module_to_same_named_item() {
    let f = package_fixture();
    f.file(
        "a/src/api.wi",
        "module api; pub fn value() -> i64 { return 1; }",
    );
    f.file(
        "a/src/api/value.wi",
        "module api::value; pub fn run() -> i64 { return 2; }",
    );
    let (ok, diagnostics) = f.check("import a::api; import a::api::value as nested; fn main() { println(api::value()); println(nested::run()); }", true);
    assert!(ok, "{diagnostics:?}");
}
