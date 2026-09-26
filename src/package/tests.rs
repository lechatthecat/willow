use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use super::*;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-package-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        // Identity and cycle diagnostics contain canonical package paths.
        Self(std::fs::canonicalize(path).unwrap())
    }

    fn package(&self, name: &str, marker: bool, extra: &str) -> PathBuf {
        let root = self.0.join(name);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.wi"), "fn value() -> i64 { return 1; }\n").unwrap();
        std::fs::write(
            root.join("project.toml"),
            format!(
                "[project]\nname = {name:?}\nversion = \"1.0.0\"\n{}\n{extra}\n",
                if marker {
                    "[willow]\nmanifest-version = 1"
                } else {
                    ""
                },
            ),
        )
        .unwrap();
        root
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn transitive_paths_and_aliases_share_canonical_identity() {
    let fixture = Fixture::new();
    let app = fixture.package("app", false, "[dependencies]\nb = { path = '../b' }\nother = { path = '../b/../b' }\nc = { path = '../c' }");
    fixture.package("b", true, "[dependencies]\nc = { path = '../c' }");
    fixture.package("c", true, "");
    let graph = resolve_path_packages(&app).unwrap();
    assert_eq!(graph.packages.len(), 3);
    let root = graph.get(graph.root).unwrap();
    assert_eq!(root.dependencies[0].package, root.dependencies[2].package);
    let b = graph.get(root.dependencies[0].package).unwrap();
    assert_eq!(b.dependencies[0].package, root.dependencies[1].package);
    assert_eq!(graph.stats.manifests_loaded, 3);
    assert_eq!(graph.stats.dependencies_visited, 4);
    assert_eq!(
        b.source_file(Path::new("lib.wi")).unwrap(),
        b.root.join("src/lib.wi")
    );
}

#[test]
fn dependency_marker_is_strict_but_entry_manifest_can_be_legacy() {
    let fixture = Fixture::new();
    let app = fixture.package("app", false, "[dependencies]\nb = { path = '../b' }");
    fixture.package("b", false, "");
    let error = resolve_path_packages(&app).unwrap_err();
    assert!(matches!(error, PackageError::NotWillowPackage(_)));
    assert!(error.to_string().contains("not a Willow package"));
}

#[test]
fn cycle_reports_only_the_active_cycle() {
    let fixture = Fixture::new();
    let app = fixture.package("app", false, "[dependencies]\nb = { path = '../b' }");
    let b = fixture.package("b", true, "[dependencies]\nc = { path = '../c' }");
    let c = fixture.package("c", true, "[dependencies]\nb = { path = '../b' }");
    let PackageError::Cycle(cycle) = resolve_path_packages(&app).unwrap_err() else {
        panic!("expected cycle")
    };
    assert_eq!(cycle, vec![b.clone(), c, b]);
}

#[test]
fn self_cycle_is_rejected() {
    let fixture = Fixture::new();
    let root = fixture.package("app", false, "[dependencies]\nself_ref = { path = '.' }");
    assert!(matches!(
        resolve_path_packages(&root),
        Err(PackageError::Cycle(_))
    ));
}

#[test]
fn discovery_errors_are_distinct() {
    let fixture = Fixture::new();
    let absent = fixture.0.join("absent");
    assert!(matches!(
        PathSource::open(&absent, true),
        Err(PackageError::NotFound { .. })
    ));
    std::fs::write(&absent, "file").unwrap();
    assert!(matches!(
        PathSource::open(&absent, true),
        Err(PackageError::NotDirectory(_))
    ));
    let empty = fixture.0.join("empty");
    std::fs::create_dir(&empty).unwrap();
    assert!(matches!(
        PathSource::open(&empty, true),
        Err(PackageError::ManifestMissing(_))
    ));
    std::fs::write(empty.join("project.toml"), "invalid = [").unwrap();
    assert!(matches!(
        PathSource::open(&empty, true),
        Err(PackageError::ManifestInvalid { .. })
    ));
    let root = fixture.package("valid", true, "");
    std::fs::remove_dir_all(root.join("src")).unwrap();
    assert!(matches!(
        PathSource::open(&root, true),
        Err(PackageError::SourceMissing(_))
    ));
}

#[test]
fn entry_and_source_paths_cannot_escape_the_package() {
    let fixture = Fixture::new();
    let root = fixture.package("app", true, "");
    let graph = resolve_path_packages(&root).unwrap();
    let package = graph.get(graph.root).unwrap();
    for path in [Path::new("../../outside.wi"), fixture.0.as_path()] {
        assert!(matches!(
            package.source_file(path),
            Err(PackageError::PathEscape { .. })
        ));
    }
    let manifest = root.join("project.toml");
    let text = std::fs::read_to_string(&manifest).unwrap().replace(
        "version = \"1.0.0\"",
        "version = \"1.0.0\"\nentry = '../outside.wi'",
    );
    std::fs::write(&manifest, text).unwrap();
    assert!(matches!(
        resolve_path_packages(&root),
        Err(PackageError::PathEscape { .. })
    ));
}

#[cfg(unix)]
#[test]
fn symlink_escapes_are_rejected_and_package_aliases_are_deduplicated() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let app = fixture.package(
        "app",
        true,
        "[dependencies]\na = { path = '../lib' }\nb = { path = '../alias' }",
    );
    let lib = fixture.package("lib", true, "");
    symlink(&lib, fixture.0.join("alias")).unwrap();
    let graph = resolve_path_packages(&app).unwrap();
    assert_eq!(graph.packages.len(), 2);
    let root = graph.get(graph.root).unwrap();
    assert_eq!(root.dependencies[0].package, root.dependencies[1].package);
    symlink(lib.join("src/lib.wi"), app.join("src/escape.wi")).unwrap();
    assert!(matches!(
        root.source_file(Path::new("escape.wi")),
        Err(PackageError::PathEscape { .. })
    ));
    std::fs::remove_dir_all(app.join("src")).unwrap();
    symlink(lib.join("src"), app.join("src")).unwrap();
    assert!(matches!(
        resolve_path_packages(&app),
        Err(PackageError::PathEscape { .. })
    ));
    std::fs::remove_file(app.join("src")).unwrap();
    std::fs::create_dir(app.join("src")).unwrap();
    std::fs::remove_file(app.join("project.toml")).unwrap();
    symlink(lib.join("project.toml"), app.join("project.toml")).unwrap();
    assert!(matches!(
        resolve_path_packages(&app),
        Err(PackageError::PathEscape { .. })
    ));
}

#[test]
fn source_contract_and_persistent_identity_do_not_expose_session_ids() {
    let fixture = Fixture::new();
    let root = fixture.package("app", true, "");
    let source = PathSource::open(&root, true).unwrap();
    assert_eq!(
        source.versions().unwrap(),
        vec![semver::Version::new(1, 0, 0)]
    );
    let identity = source.resolve(&semver::VersionReq::STAR).unwrap();
    assert_eq!(source.materialize(&identity).unwrap(), root);
    assert!(matches!(
        source.resolve(&semver::VersionReq::parse("^2").unwrap()),
        Err(PackageError::VersionUnavailable(_))
    ));
    let json = serde_json::to_value(&identity).unwrap();
    assert_eq!(json.as_object().unwrap().len(), 4);
    assert!(json.get("id").is_none());
    assert_eq!(
        serde_json::from_value::<PackageIdentity>(json).unwrap(),
        identity
    );
    let mut wrong = identity;
    wrong.revision = Some("different".into());
    assert!(matches!(
        source.materialize(&wrong),
        Err(PackageError::RevisionMismatch(_))
    ));
}

#[test]
fn git_dependencies_fail_explicitly_until_git_source_is_implemented() {
    let fixture = Fixture::new();
    let root = fixture.package(
        "app",
        true,
        "[dependencies]\na = { git = 'https://example.invalid/a' }",
    );
    assert!(matches!(
        resolve_path_packages(&root),
        Err(PackageError::UnsupportedSource { .. })
    ));
}

#[test]
fn graph_work_counts_scale_with_nodes_and_edges() {
    for count in [16, 64, 256] {
        let fixture = Fixture::new();
        // A chain plus repeated edges to one shared leaf. Each package is
        // parsed once even when referenced again after its traversal completes.
        fixture.package("leaf", true, "");
        for index in 0..count {
            let next = if index + 1 < count {
                format!("next = {{ path = '../p{}' }}\n", index + 1)
            } else {
                String::new()
            };
            fixture.package(
                &format!("p{index}"),
                true,
                &format!(
                    "[dependencies]\n{next}a = {{ path = '../leaf' }}\nb = {{ path = '../leaf' }}"
                ),
            );
        }
        let graph = resolve_path_packages(&fixture.0.join("p0")).unwrap();
        assert_eq!(graph.stats.manifests_loaded, count + 1);
        assert_eq!(graph.stats.dependencies_visited, 3 * count - 1);
        assert_eq!(graph.stats.paths_canonicalized, 3 * count);
        println!(
            "packages={} edges={} manifests={} canonicalizations={}",
            count + 1,
            3 * count - 1,
            graph.stats.manifests_loaded,
            graph.stats.paths_canonicalized
        );
    }
}

#[test]
fn wide_diamond_graph_loads_shared_leaf_once() {
    for count in [16, 64, 256] {
        let fixture = Fixture::new();
        fixture.package("leaf", true, "");
        let mut dependencies = String::from("[dependencies]\n");
        for index in 0..count {
            dependencies.push_str(&format!("p{index} = {{ path = '../p{index}' }}\n"));
            fixture.package(
                &format!("p{index}"),
                true,
                "[dependencies]\nleaf = { path = '../leaf' }",
            );
        }
        let root = fixture.package("app", true, &dependencies);
        let graph = resolve_path_packages(&root).unwrap();
        assert_eq!(graph.stats.manifests_loaded, count + 2);
        assert_eq!(graph.stats.dependencies_visited, count * 2);
        assert_eq!(graph.stats.paths_canonicalized, count * 2 + 1);
        println!(
            "diamond packages={} edges={} manifests={} canonicalizations={}",
            count + 2,
            count * 2,
            graph.stats.manifests_loaded,
            graph.stats.paths_canonicalized
        );
    }
}

#[test]
fn long_chain_does_not_use_recursive_resolution_or_destruction() {
    let fixture = Fixture::new();
    let count = 4096;
    for index in 0..count {
        let next = if index + 1 < count {
            format!("[dependencies]\nnext = {{ path = '../p{}' }}\n", index + 1)
        } else {
            String::new()
        };
        fixture.package(&format!("p{index}"), true, &next);
    }
    let root = fixture.0.join("p0");
    // A deliberately small stack makes accidental recursive graph walking
    // observable without relying on timing or a platform's default stack size.
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let graph = resolve_path_packages(&root).unwrap();
            assert_eq!(graph.packages.len(), count);
            assert_eq!(graph.stats.manifests_loaded, count);
            assert_eq!(graph.stats.dependencies_visited, count - 1);
            drop(graph);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn compiler_inputs_share_resolved_graph_without_loading_unused_modules() {
    let fixture = Fixture::new();
    let app = fixture.package(
        "app",
        true,
        "[dependencies]\nunused = { path = '../unused' }",
    );
    fixture.package("unused", true, "");
    let source = "fn main() {}";
    let root = app.join("src");
    let inputs = crate::compiler_db::inputs::CompilerInputs::native(
        crate::CompilerOptions::debug(),
        root.clone(),
    )
    .resolve_project(Some(&app))
    .unwrap();
    let packages = inputs.package_graph.clone().unwrap();
    assert_eq!(packages.packages.len(), 2);
    let map = crate::diagnostics::SourceMap::new("main.wi", source);
    let frontend = crate::run_frontend_with_inputs(
        source,
        &root,
        &map,
        inputs,
        &mut crate::diagnostics::HumanEmitter,
    )
    .unwrap();
    assert!(frontend.module_graph.files.is_empty());
    assert_eq!(frontend.db.inputs().root_package, Some(packages.root));
    assert!(std::sync::Arc::ptr_eq(
        frontend.db.inputs().package_graph.as_ref().unwrap(),
        &packages
    ));
    assert!(std::sync::Arc::ptr_eq(
        frontend.module_graph.package_graph.as_ref().unwrap(),
        &packages
    ));

    let legacy = fixture.package("legacy", false, "");
    std::fs::remove_dir_all(legacy.join("src")).unwrap();
    let inputs = crate::compiler_db::inputs::CompilerInputs::native(
        crate::CompilerOptions::debug(),
        legacy.clone(),
    )
    .resolve_project(Some(&legacy))
    .unwrap();
    assert!(inputs.package_graph.is_none());
    assert!(inputs.root_package.is_none());
}

#[test]
fn rust_dependencies_never_enter_the_package_graph() {
    let fixture = Fixture::new();
    let app = fixture.package(
        "app",
        false,
        "[dependencies]\nb = { path = '../b' }\n\n[rust-dependencies]\nregex = '1.12'\nmy_native = { path = '../my-native' }\n\n[rust]\nbridge = 'rust/bridge.rs'",
    );
    std::fs::create_dir_all(app.join("rust")).unwrap();
    std::fs::write(app.join("rust/bridge.rs"), "pub fn f() {}\n").unwrap();
    fixture.package("b", true, "");
    // `../my-native` does not exist as a Willow package and must never be visited.
    let graph = resolve_path_packages(&app).unwrap();
    assert_eq!(graph.packages.len(), 2);
    let root = graph.get(graph.root).unwrap();
    assert_eq!(root.dependencies.len(), 1);
    assert_eq!(root.dependencies[0].alias, "b");
    assert_eq!(graph.stats.dependencies_visited, 1);
}

#[test]
fn rust_dependencies_alone_do_not_make_a_project_package_mode() {
    let fixture = Fixture::new();
    let app = fixture.package("app", false, "[rust-dependencies]\nregex = '1.12'");
    assert!(
        super::lock::resolve_project_packages(&app, false, false)
            .unwrap()
            .is_none()
    );
}
