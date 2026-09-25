use super::*;
use crate::package::{
    PackageGraph, PackageIdentity, PackageSourceIdentity, ResolvedDependency, ResolvedPackage,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-package-loader-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        // Match resolved packages even when the OS temp directory is a symlink.
        Self(std::fs::canonicalize(root).unwrap())
    }
    fn file(&self, package: u32, path: &str, source: &str) {
        let file = self.0.join(format!("p{package}/src/{path}"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source).unwrap();
    }
    fn graph(&self, edges: &[&[(&str, u32)]]) -> Arc<PackageGraph> {
        Arc::new(PackageGraph {
            root: PackageId(0),
            packages: edges
                .iter()
                .enumerate()
                .map(|(i, edges)| {
                    let root = self.0.join(format!("p{i}"));
                    std::fs::create_dir_all(root.join("src")).unwrap();
                    ResolvedPackage {
                        checksum: None,
                        id: PackageId(i as u32),
                        identity: PackageIdentity {
                            name: format!("p{i}"),
                            version: "1.0.0".into(),
                            source: PackageSourceIdentity::Path { path: root.clone() },
                            revision: None,
                        },
                        root,
                        dependencies: edges
                            .iter()
                            .map(|(alias, id)| ResolvedDependency {
                                selector: None,
                                alias: (*alias).into(),
                                package: PackageId(*id),
                            })
                            .collect(),
                    }
                })
                .collect(),
            stats: Default::default(),
        })
    }
    fn resolve(&self, source: &str, graph: Arc<PackageGraph>, spooled: bool) -> ImportResolution {
        let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let root = graph.get(graph.root).unwrap().source_root();
        if spooled {
            resolve_imports_spooled_entry(
                &program,
                &root,
                super::super::artifacts::UnitArtifacts::new().unwrap(),
                None,
                Some(graph),
                true,
            )
        } else {
            let mut modules = ModuleGraph::new(root.clone());
            modules.package_graph = Some(graph);
            resolve_imports_in_graph(&program, &root, modules)
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn success(result: &ImportResolution) {
    assert!(
        result
            .diagnostics
            .iter()
            .all(|d| d.severity != Severity::Error),
        "{:?}",
        result.diagnostics
    );
}
fn key(package: u32, path: &str) -> ModuleKey {
    ModuleKey::new(PackageId(package), path)
}

#[test]
fn package_loader_same_names_aliases_items_and_spooling() {
    for spooled in [false, true] {
        let f = Fixture::new();
        for p in 0..3 {
            f.file(p, "util.wi", "module util; pub fn format() {} ");
        }
        let graph = f.graph(&[&[("a", 1), ("same", 1), ("b", 2)], &[], &[]]);
        let result = f.resolve("import util; import a::util as first; import same::util as second; import b::util as third; import a::util::format as fa; import b::util::format as fb;", graph, spooled);
        success(&result);
        assert_eq!(result.graph.files.len(), 3);
        let ids: std::collections::HashSet<_> = (0..3)
            .map(|p| result.graph.module_id_for(&key(p, "util")).unwrap())
            .collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(
            result
                .item_imports
                .iter()
                .map(|i| (i.local.as_str(), i.package, i.canonical_module.as_str()))
                .collect::<Vec<_>>(),
            [("fa", PackageId(1), "util"), ("fb", PackageId(2), "util")]
        );
        if spooled {
            assert!(result.graph.files.iter().all(|m| m.source.is_empty()));
        }
    }
}

#[test]
fn package_loader_uses_each_consumers_aliases_and_records_edges() {
    let f = Fixture::new();
    f.file(1, "api.wi", "module api; import shared::util; import own; ");
    f.file(1, "own.wi", "module own;");
    f.file(2, "util.wi", "module util;");
    f.file(3, "util.wi", "module util;");
    let graph = f.graph(&[&[("a", 1), ("shared", 2)], &[("shared", 3)], &[], &[]]);
    let result = f.resolve("import a::api;", graph, true);
    success(&result);
    assert_eq!(
        result.graph.dependencies_for(&key(1, "api")),
        [key(3, "util"), key(1, "own")]
    );
    assert!(result.graph.module_id_for(&key(2, "util")).is_none());
    assert_eq!(
        result
            .graph
            .files
            .iter()
            .map(|m| m.module_key())
            .collect::<Vec<_>>(),
        [key(3, "util"), key(1, "own"), key(1, "api")]
    );
}

#[test]
fn package_loader_module_first_nested_directory_and_declarations() {
    for path in ["nested/util.wi", "nested/util/mod.wi"] {
        let f = Fixture::new();
        f.file(1, "nested.wi", "module nested; pub fn util() {}");
        f.file(1, path, "module nested::util;");
        let graph = f.graph(&[&[("a", 1)], &[]]);
        let result = f.resolve("import a::nested::util;", graph, false);
        success(&result);
        assert_eq!(result.graph.files.len(), 1);
        assert_eq!(result.graph.files[0].module_key(), key(1, "nested::util"));
        assert!(result.item_imports.is_empty());
    }
}

#[test]
fn package_loader_rejects_alias_in_module_declaration() {
    let f = Fixture::new();
    f.file(1, "util.wi", "module a::util;");
    let result = f.resolve("import a::util;", f.graph(&[&[("a", 1)], &[]]), false);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code == ErrorCode::E2011)
    );
}

#[test]
fn package_loader_rejects_undeclared_transitive_and_alias_only_imports() {
    for source in ["import hidden::util;", "import a;"] {
        let f = Fixture::new();
        f.file(2, "util.wi", "module util;");
        let graph = f.graph(&[&[("a", 1)], &[("hidden", 2)], &[]]);
        let result = f.resolve(source, graph, false);
        assert_eq!(result.diagnostics.len(), 1);
        assert!(
            result.diagnostics[0]
                .message
                .contains("package_module_not_found")
        );
        assert!(result.graph.files.is_empty());
    }
}

#[test]
fn package_loader_validates_conflicts_before_loading_even_without_imports() {
    for path in ["a.wi", "a/child.wi"] {
        let f = Fixture::new();
        f.file(0, path, "");
        let result = f.resolve("", f.graph(&[&[("a", 1)], &[]]), false);
        assert_eq!(result.diagnostics.len(), 1);
        assert!(
            result.diagnostics[0]
                .message
                .contains("dependency_alias_conflict")
        );
        assert!(result.graph.files.is_empty());
    }
}

#[test]
fn package_loader_cycles_are_package_qualified() {
    let f = Fixture::new();
    f.file(1, "a.wi", "module a; import b;");
    f.file(1, "b.wi", "module b; import a;");
    let result = f.resolve("import dep::a;", f.graph(&[&[("dep", 1)], &[]]), true);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code == ErrorCode::E0403)
    );
}

#[test]
fn package_loader_std_and_unused_dependencies_do_not_load_files() {
    let f = Fixture::new();
    let result = f.resolve("import std::io;", f.graph(&[&[("dep", 1)], &[]]), false);
    success(&result);
    assert!(result.graph.files.is_empty());
}

#[test]
fn package_loader_retains_parse_error_source_identity() {
    let f = Fixture::new();
    f.file(1, "broken.wi", "module broken; pub fn (");
    let result = f.resolve("import dep::broken;", f.graph(&[&[("dep", 1)], &[]]), true);
    assert!(!result.diagnostics.is_empty());
    assert_eq!(result.graph.files.len(), 1);
    assert_eq!(result.graph.files[0].module_key(), key(1, "broken"));
    assert!(
        result
            .diagnostics
            .iter()
            .flat_map(|d| &d.labels)
            .any(|label| label.span.file_id == result.graph.files[0].id.file_id())
    );
}

#[test]
fn package_loader_work_scales_with_sources_and_import_occurrences() {
    for size in [16, 64, 256, 1024] {
        for chain in [false, true] {
            let f = Fixture::new();
            for i in 0..size {
                let imports = if i == 0 {
                    String::new()
                } else {
                    let target = if chain { i - 1 } else { 0 };
                    format!("import m{target} as first; import m{target} as second;")
                };
                f.file(1, &format!("m{i}.wi"), &format!("module m{i}; {imports}"));
            }
            let mut entry = String::new();
            // Reverse order exercises a deep chain before it is cached.
            for i in (0..size).rev() {
                entry.push_str(&format!(
                    "import dep::m{i} as a{i}; import same::m{i} as b{i};"
                ));
            }
            let graph = f.graph(&[&[("dep", 1), ("same", 1)], &[]]);
            let result = f.resolve(&entry, graph, true);
            success(&result);
            assert_eq!(result.graph.files.len(), size);
            assert_eq!(result.graph.source_loads, size);
            assert_eq!(result.graph.import_routes, 2 * size + 2 * (size - 1));
            let edge_count: usize = result
                .graph
                .files
                .iter()
                .map(|m| result.graph.dependencies_for(&m.module_key()).len())
                .sum();
            assert_eq!(edge_count, size - 1);
            eprintln!(
                "package-loader shape={} modules={size} source_loads={} routes={} stored_edges={edge_count}",
                if chain { "chain" } else { "fanout" },
                result.graph.source_loads,
                result.graph.import_routes
            );
        }
    }
}

#[test]
fn package_loader_failed_visits_unwind_before_later_imports() {
    for broken in ["pub fn (", "\"unterminated"] {
        let f = Fixture::new();
        f.file(1, "broken.wi", broken);
        f.file(1, "valid.wi", "module valid;");
        let result = f.resolve(
            "import dep::broken; import dep::valid;",
            f.graph(&[&[("dep", 1)], &[]]),
            true,
        );
        assert!(!result.diagnostics.is_empty());
        assert!(result.graph.contains_key(&key(1, "valid")));
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.code == ErrorCode::E0403)
        );
    }
}

#[test]
fn package_loader_real_manifests_reach_dependency_local_modules() {
    let f = Fixture::new();
    for (p, deps) in [(0, "[dependencies]\ndep = { path = '../p1' }"), (1, "")] {
        f.file(p, "lib.wi", "module lib;");
        std::fs::write(f.0.join(format!("p{p}/project.toml")), format!("[project]\nname = 'p{p}'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n{deps}\n")).unwrap();
    }
    f.file(1, "api.wi", "module api; import lib;");
    let graph = Arc::new(crate::package::resolve_path_packages(&f.0.join("p0")).unwrap());
    let result = f.resolve("import dep::api;", graph, true);
    success(&result);
    assert_eq!(
        result
            .graph
            .files
            .iter()
            .map(|m| m.module_key())
            .collect::<Vec<_>>(),
        [key(1, "lib"), key(1, "api")]
    );
}

#[test]
fn package_loader_lexer_errors_keep_diagnostic_source() {
    for spooled in [false, true] {
        let f = Fixture::new();
        let source = "\"unterminated";
        f.file(1, "broken.wi", source);
        let result = f.resolve(
            "import dep::broken;",
            f.graph(&[&[("dep", 1)], &[]]),
            spooled,
        );
        assert!(!result.diagnostics.is_empty());
        let file = &result.graph.files[0];
        assert_eq!(file.module_key(), key(1, "broken"));
        assert!(
            result
                .diagnostics
                .iter()
                .flat_map(|d| &d.labels)
                .any(|label| label.span.file_id == file.id.file_id())
        );
        if spooled {
            assert_eq!(
                result
                    .graph
                    .artifacts
                    .as_ref()
                    .unwrap()
                    .source(file.id.file_id())
                    .unwrap(),
                source
            );
        } else {
            assert_eq!(file.source, source);
        }
    }
}

#[test]
fn package_loader_nonzero_root_and_no_module_declaration() {
    let f = Fixture::new();
    f.file(1, "util.wi", "pub fn format() {}");
    let mut graph = f.graph(&[&[], &[]]);
    Arc::get_mut(&mut graph).unwrap().root = PackageId(1);
    let result = f.resolve("import util;", graph, false);
    success(&result);
    assert_eq!(result.graph.files[0].module_key(), key(1, "util"));
}

#[test]
fn package_loader_duplicate_binding_diagnostics_are_preserved() {
    for (source, code) in [
        ("import dep::util; import dep::util;", ErrorCode::W2002),
        (
            "import dep::util as u; import same::util as u;",
            ErrorCode::E2004,
        ),
    ] {
        let f = Fixture::new();
        f.file(1, "util.wi", "module util;");
        let result = f.resolve(source, f.graph(&[&[("dep", 1), ("same", 1)], &[]]), false);
        assert_eq!(result.graph.source_loads, 1);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, code);
    }
}

#[test]
fn package_loader_feeds_consumer_dependency_index_and_backend_classification() {
    for spooled in [false, true] {
        let f = Fixture::new();
        f.file(
            1,
            "util.wi",
            "module util; import shared::util; pub class Left {}",
        );
        f.file(2, "util.wi", "module util; pub class Right {}");
        f.file(3, "util.wi", "module util; pub class Transitive {}");
        let graph = f.graph(&[
            &[("a", 1), ("same", 1), ("b", 2)],
            &[("shared", 3)],
            &[],
            &[],
        ]);
        // First import fixes graph spelling; subsequent units use different aliases.
        let result = f.resolve(
            "import a::util as first; import b::util as second;",
            graph.clone(),
            spooled,
        );
        success(&result);
        let modules = &result.graph.files;
        let index = crate::compiler_db::dependencies::ModuleDependencies::with_packages(
            modules,
            Some(&graph),
        );
        let tokens = Lexer::new("import same::util as left; import b::util as right;")
            .tokenize()
            .unwrap();
        let (entry, errors) = Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let left = *index.paths_for(&entry).get("same::util").unwrap();
        let right = *index.paths_for(&entry).get("b::util").unwrap();
        assert_ne!(left, right);
        let transitive = index.direct(modules[left].id).unwrap();
        assert_eq!(transitive.len(), 1);
        assert_eq!(modules[transitive[0]].package, PackageId(3));
        assert_eq!(index.paths_for(&entry).get("shared::util"), None);
        let imports = crate::backend_unit_imports(&entry, modules, &index);
        assert_eq!(imports.module_spellings.len(), 2);
        assert_eq!(imports.module_spellings[0].access, "left");
        assert_eq!(imports.module_spellings[0].types, ["Left"]);
        assert_eq!(imports.module_spellings[1].access, "right");
        assert_eq!(imports.module_spellings[1].types, ["Right"]);
    }
}
