use super::*;
use crate::package::{PackageIdentity, PackageSourceIdentity, ResolvedDependency, ResolvedPackage};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-package-imports-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn file(&self, path: &str) {
        let file = self.0.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, "pub fn format() {}\n").unwrap();
    }
    fn package(&self, id: u32, aliases: &[(&str, u32)]) -> ResolvedPackage {
        let root = self.0.join(format!("p{id}"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        ResolvedPackage {
            checksum: None,
            id: PackageId(id),
            identity: PackageIdentity {
                name: format!("p{id}"),
                version: "1.0.0".into(),
                source: PackageSourceIdentity::Path { path: root.clone() },
                revision: None,
            },
            root,
            dependencies: aliases
                .iter()
                .map(|(alias, package)| ResolvedDependency {
                    selector: None,
                    alias: (*alias).into(),
                    package: PackageId(*package),
                })
                .collect(),
        }
    }
    fn graph(&self) -> PackageGraph {
        PackageGraph {
            root: PackageId(0),
            packages: vec![
                self.package(0, &[("a", 1), ("same", 1), ("b", 2)]),
                self.package(1, &[("b", 3)]),
                self.package(2, &[]),
                self.package(3, &[]),
            ],
            stats: Default::default(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn resolved(
    imports: &PackageImports<'_>,
    consumer: u32,
    path: &str,
) -> (ModuleKey, PathBuf, Option<String>) {
    match imports.resolve(PackageId(consumer), path).unwrap() {
        PackageImport::Module { key, file, item } => (key, file, item),
        PackageImport::Std => panic!("expected source module"),
    }
}

macro_rules! route_test {
    ($name:ident, $consumer:expr, $path:expr, $file:expr, $target:expr, $logical:expr, $item:expr) => {
        #[test]
        fn $name() {
            let fixture = Fixture::new();
            let graph = fixture.graph();
            fixture.file($file);
            let imports = PackageImports::new(&graph).unwrap();
            let (key, file, item) = resolved(&imports, $consumer, $path);
            assert_eq!(key, ModuleKey::new(PackageId($target), $logical));
            assert_eq!(file, fixture.0.join($file));
            assert_eq!(item.as_deref(), $item);
        }
    };
}
route_test!(local_file, 0, "util", "p0/src/util.wi", 0, "util", None);
route_test!(
    local_directory,
    0,
    "util",
    "p0/src/util/mod.wi",
    0,
    "util",
    None
);
route_test!(
    local_nested,
    0,
    "util::text",
    "p0/src/util/text.wi",
    0,
    "util::text",
    None
);
route_test!(
    local_item,
    0,
    "util::format",
    "p0/src/util.wi",
    0,
    "util",
    Some("format")
);
route_test!(
    dependency_file,
    0,
    "a::util",
    "p1/src/util.wi",
    1,
    "util",
    None
);
route_test!(
    dependency_directory,
    0,
    "a::util",
    "p1/src/util/mod.wi",
    1,
    "util",
    None
);
route_test!(
    dependency_nested,
    0,
    "a::util::text",
    "p1/src/util/text.wi",
    1,
    "util::text",
    None
);
route_test!(
    dependency_item,
    0,
    "a::util::format",
    "p1/src/util.wi",
    1,
    "util",
    Some("format")
);
route_test!(
    dependency_own_local,
    1,
    "util",
    "p1/src/util.wi",
    1,
    "util",
    None
);
route_test!(
    dependency_own_alias,
    1,
    "b::util",
    "p3/src/util.wi",
    3,
    "util",
    None
);
route_test!(
    unicode_module,
    0,
    "a::日本",
    "p1/src/日本.wi",
    1,
    "日本",
    None
);

#[test]
fn same_module_path_in_different_packages_has_distinct_identity() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    fixture.file("p1/src/util.wi");
    fixture.file("p2/src/util.wi");
    let imports = PackageImports::new(&graph).unwrap();
    let a = resolved(&imports, 0, "a::util");
    let b = resolved(&imports, 0, "b::util");
    assert_ne!(a.0, b.0);
    assert_eq!(a.0.path, b.0.path);
}

#[test]
fn multiple_aliases_share_identity_and_file() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    fixture.file("p1/src/util.wi");
    let imports = PackageImports::new(&graph).unwrap();
    assert_eq!(
        resolved(&imports, 0, "a::util"),
        resolved(&imports, 0, "same::util")
    );
}

#[test]
fn module_precedes_item_and_file_precedes_mod_file() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    for file in [
        "p1/src/util.wi",
        "p1/src/util/format.wi",
        "p1/src/util/format/mod.wi",
    ] {
        fixture.file(file);
    }
    let imports = PackageImports::new(&graph).unwrap();
    let (key, file, item) = resolved(&imports, 0, "a::util::format");
    assert_eq!(key.path.0, "util::format");
    assert_eq!(file, fixture.0.join("p1/src/util/format.wi"));
    assert!(item.is_none());
}

#[test]
fn alias_alone_does_not_import_package() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    fixture.file("p1/src/lib.wi");
    assert!(matches!(
        PackageImports::new(&graph)
            .unwrap()
            .resolve(graph.root, "a"),
        Err(PackageImportError::ModuleRequired { .. })
    ));
}

#[test]
fn missing_dependency_module_does_not_fall_back_to_local_item() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    fixture.file("p0/src/util.wi");
    assert!(matches!(
        PackageImports::new(&graph)
            .unwrap()
            .resolve(graph.root, "a::util"),
        Err(PackageImportError::ModuleNotFound { .. })
    ));
}

#[test]
fn transitive_dependency_is_not_visible_to_root() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    fixture.file("p3/src/util.wi");
    let imports = PackageImports::new(&graph).unwrap();
    assert!(matches!(
        imports.resolve(graph.root, "p3::util"),
        Err(PackageImportError::ModuleNotFound { .. })
    ));
    assert!(matches!(
        imports.resolve(graph.root, "b::util"),
        Err(PackageImportError::ModuleNotFound { .. })
    ));
    assert_eq!(resolved(&imports, 1, "b::util").0.package, PackageId(3));
}

#[test]
fn root_aliases_are_not_inherited_by_dependencies() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    fixture.file("p1/src/util.wi");
    let imports = PackageImports::new(&graph).unwrap();
    assert!(matches!(
        imports.resolve(PackageId(2), "a::util"),
        Err(PackageImportError::ModuleNotFound { .. })
    ));
}

#[test]
fn std_bypasses_filesystem_and_alias_lookups() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    let imports = PackageImports::new(&graph).unwrap();
    for path in ["std", "std::collections::Array", "std::missing"] {
        assert_eq!(
            imports.resolve(graph.root, path).unwrap(),
            PackageImport::Std
        );
    }
    assert_eq!(imports.alias_lookups.get(), 0);
    assert_eq!(imports.file_probes.get(), 0);
}

#[test]
fn local_file_alias_conflict_is_rejected_before_imports() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    fixture.file("p0/src/a.wi");
    assert!(
        matches!(PackageImports::new(&graph), Err(PackageImportError::AliasConflict { alias, .. }) if alias == "a")
    );
}

#[test]
fn local_directory_alias_conflict_is_rejected_before_imports() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    std::fs::create_dir(fixture.0.join("p0/src/a")).unwrap();
    assert!(matches!(
        PackageImports::new(&graph),
        Err(PackageImportError::AliasConflict { .. })
    ));
}

#[test]
fn conflicts_in_dependency_use_its_own_aliases() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    fixture.file("p1/src/b.wi");
    assert!(
        matches!(PackageImports::new(&graph), Err(PackageImportError::AliasConflict { alias, root }) if alias == "b" && root == fixture.0.join("p1/src"))
    );
}

#[test]
fn invalid_paths_cannot_escape_or_be_reinterpreted_as_filenames() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    let imports = PackageImports::new(&graph).unwrap();
    for path in [
        "",
        "::util",
        "a::",
        "a::::util",
        "a::..::secret",
        "a::/etc/passwd",
        "a::C:\\secret",
        "a::util.wi",
        "a::bad\0name",
        "a::bad name",
    ] {
        assert!(
            matches!(
                imports.resolve(graph.root, path),
                Err(PackageImportError::InvalidPath(_))
            ),
            "{path:?}"
        );
    }
    assert_eq!(imports.file_probes.get(), 0);
}

#[test]
fn unknown_consumer_is_an_error() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    assert!(matches!(
        PackageImports::new(&graph)
            .unwrap()
            .resolve(PackageId(99), "util"),
        Err(PackageImportError::UnknownPackage(_))
    ));
}

#[test]
fn directory_named_wi_is_not_a_module() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    std::fs::create_dir(fixture.0.join("p1/src/util.wi")).unwrap();
    fixture.file("p1/src/util/mod.wi");
    let imports = PackageImports::new(&graph).unwrap();
    assert_eq!(
        resolved(&imports, 0, "a::util").1,
        fixture.0.join("p1/src/util/mod.wi")
    );
}

#[cfg(unix)]
#[test]
fn escaping_symlink_is_not_followed_or_hidden_by_item_fallback() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    fixture.file("outside.wi");
    fixture.file("p1/src/util.wi");
    std::fs::create_dir(fixture.0.join("p1/src/util")).unwrap();
    std::os::unix::fs::symlink(
        fixture.0.join("outside.wi"),
        fixture.0.join("p1/src/util/format.wi"),
    )
    .unwrap();
    let imports = PackageImports::new(&graph).unwrap();
    assert!(matches!(
        imports.resolve(graph.root, "a::util::format"),
        Err(PackageImportError::Source(PackageError::PathEscape { .. }))
    ));
}

#[cfg(unix)]
#[test]
fn internal_symlink_retains_logical_identity() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    fixture.file("p1/src/actual.wi");
    std::os::unix::fs::symlink("actual.wi", fixture.0.join("p1/src/util.wi")).unwrap();
    let imports = PackageImports::new(&graph).unwrap();
    let (key, file, _) = resolved(&imports, 0, "a::util");
    assert_eq!(key.path.0, "util");
    assert_eq!(file, fixture.0.join("p1/src/actual.wi"));
}

#[cfg(unix)]
#[test]
fn dangling_symlink_still_conflicts_with_dependency_alias() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    std::os::unix::fs::symlink("missing", fixture.0.join("p0/src/a")).unwrap();
    assert!(matches!(
        PackageImports::new(&graph),
        Err(PackageImportError::AliasConflict { .. })
    ));
}

#[test]
fn resolved_manifest_graph_routes_aliases_to_package_local_paths() {
    let fixture = Fixture::new();
    for (id, dependencies) in [
        (
            0,
            "[dependencies]\na = { path = '../p1' }\nsame = { path = '../p1' }",
        ),
        (1, "[dependencies]\nb = { path = '../p2' }"),
        (2, ""),
    ] {
        fixture.file(&format!("p{id}/src/util.wi"));
        std::fs::write(
            fixture.0.join(format!("p{id}/project.toml")),
            format!("[project]\nname = 'p{id}'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n{dependencies}\n"),
        ).unwrap();
    }
    let graph = crate::package::resolve_path_packages(&fixture.0.join("p0")).unwrap();
    let imports = PackageImports::new(&graph).unwrap();
    let a = resolved(&imports, graph.root.0, "a::util");
    assert_eq!(a, resolved(&imports, graph.root.0, "same::util"));
    let b = resolved(&imports, a.0.package.0, "b::util");
    assert_ne!(a.0, b.0);
    assert_eq!(a.0.path.0, "util");
    assert_eq!(b.0.path.0, "util");
    assert!(matches!(
        imports.resolve(graph.root, "b::util"),
        Err(PackageImportError::ModuleNotFound { .. })
    ));
}

#[test]
fn item_and_missing_routes_have_bounded_filesystem_work() {
    let fixture = Fixture::new();
    let graph = fixture.graph();
    fixture.file("p1/src/util.wi");
    let imports = PackageImports::new(&graph).unwrap();
    assert_eq!(
        resolved(&imports, 0, "a::util::format").2.as_deref(),
        Some("format")
    );
    assert_eq!(imports.file_probes.get(), 3);
    assert!(matches!(
        imports.resolve(graph.root, "a::missing::item"),
        Err(PackageImportError::ModuleNotFound { .. })
    ));
    assert_eq!(imports.file_probes.get(), 7);
    assert_eq!(imports.alias_lookups.get(), 2);
}

#[test]
fn indexed_routing_work_scales_with_requests_not_package_or_alias_count() {
    for size in [16, 64, 256, 1024] {
        let fixture = Fixture::new();
        let mut graph = PackageGraph {
            root: PackageId(0),
            packages: vec![fixture.package(0, &[])],
            stats: Default::default(),
        };
        for id in 1..=size {
            graph.packages.push(fixture.package(id, &[]));
            fixture.file(&format!("p{id}/src/util.wi"));
            for prefix in ["dep", "alias"] {
                graph.packages[0].dependencies.push(ResolvedDependency {
                    selector: None,
                    alias: format!("{prefix}{id}"),
                    package: PackageId(id),
                });
            }
        }
        let imports = PackageImports::new(&graph).unwrap();
        let mut keys = std::collections::HashSet::new();
        for _ in 0..4 {
            for id in 1..=size {
                for prefix in ["dep", "alias"] {
                    keys.insert(resolved(&imports, 0, &format!("{prefix}{id}::util")).0);
                }
            }
        }
        assert_eq!(keys.len(), size as usize);
        assert_eq!(
            imports.aliases.iter().map(HashMap::len).sum::<usize>(),
            2 * size as usize
        );
        assert_eq!(imports.alias_lookups.get(), 8 * size as usize);
        assert_eq!(imports.file_probes.get(), 8 * size as usize);
        eprintln!(
            "packages={} aliases={} requests={} alias_lookups={} file_probes={}",
            size + 1,
            size * 2,
            size * 8,
            imports.alias_lookups.get(),
            imports.file_probes.get()
        );
    }
}
