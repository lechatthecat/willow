use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-lock-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn package(&self, name: &str, deps: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(path.join("src")).unwrap();
        std::fs::write(path.join("project.toml"), format!("[project]\nname={name:?}\nversion='1.0.0'\n[willow]\nmanifest-version=1\n[dependencies]\n{deps}")).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn missing_stale_reuse_and_live_path_sources() {
    let f = Fixture::new();
    let root = f.package("root", "lib={path='../lib'}");
    let lib = f.package("lib", "");
    assert!(
        resolve_locked_path_packages(&root, true)
            .unwrap_err()
            .to_string()
            .contains("lockfile_missing")
    );
    resolve_locked_path_packages(&root, false).unwrap();
    let path = root.join("project.lock");
    let bytes = std::fs::read(&path).unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    // A dependency lock is neither read nor written, even if invalid.
    std::fs::write(lib.join("project.lock"), "INVALID").unwrap();
    std::fs::write(lib.join("src/lib.wi"), "updated live source").unwrap();
    resolve_locked_path_packages(&root, true).unwrap();
    resolve_locked_path_packages(&root, false).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let manifest_path = lib.join("project.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap()
        .replace("1.0.0", "1.1.0");
    std::fs::write(&manifest_path, &manifest).unwrap();
    assert!(
        resolve_locked_path_packages(&root, true)
            .unwrap_err()
            .to_string()
            .contains("lockfile_stale")
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    resolve_locked_path_packages(&root, false).unwrap();
    assert_ne!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(std::fs::read_to_string(manifest_path).unwrap(), manifest);
    assert_eq!(
        std::fs::read_to_string(lib.join("project.lock")).unwrap(),
        "INVALID"
    );
}

#[test]
fn deterministic_relocatable_and_order_independent() {
    let f = Fixture::new();
    let root = f.package(
        "root",
        "z={path='../b'}\na={path='../a'}\nshared={path='../a'}",
    );
    f.package("a", "b={path='../b'}");
    f.package("b", "");
    let mut graph = resolve_path_packages(&root).unwrap();
    let first = Lock::from_graph(&graph).unwrap().canonical_text().unwrap();
    for p in &mut graph.packages {
        p.dependencies.reverse();
    }
    // Reverse the graph's storage/IDs as well as edge order.
    let n = graph.packages.len() as u32;
    graph.root.0 = n - 1 - graph.root.0;
    graph.packages.reverse();
    for p in &mut graph.packages {
        p.id.0 = n - 1 - p.id.0;
        for d in &mut p.dependencies {
            d.package.0 = n - 1 - d.package.0;
        }
    }
    let second = Lock::from_graph(&graph).unwrap().canonical_text().unwrap();
    assert_eq!(first, second);
    assert!(!first.contains(f.0.to_str().unwrap()));
    assert!(!first.contains("checksum"));
    assert!(
        toml::from_str::<Lock>(&first)
            .unwrap()
            .matches(&Lock::from_graph(&graph).unwrap())
    );
    // Moving the entire package tree must not invalidate the lock.
    resolve_locked_path_packages(&root, false).unwrap();
    let moved = f.0.join("moved");
    std::fs::create_dir(&moved).unwrap();
    for name in ["root", "a", "b"] {
        std::fs::rename(f.0.join(name), moved.join(name)).unwrap();
    }
    resolve_locked_path_packages(&moved.join("root"), true).unwrap();
}

#[test]
fn corrupt_lock_is_stale_and_recovers_without_manifest_changes() {
    let f = Fixture::new();
    let root = f.package("root", "");
    let manifest = std::fs::read(root.join("project.toml")).unwrap();
    for bad in ["broken", "lock-version=99", ""] {
        std::fs::write(root.join("project.lock"), bad).unwrap();
        assert!(
            resolve_locked_path_packages(&root, true)
                .unwrap_err()
                .to_string()
                .contains("lockfile_stale")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("project.lock")).unwrap(),
            bad
        );
        resolve_locked_path_packages(&root, false).unwrap();
        resolve_locked_path_packages(&root, true).unwrap();
    }
    assert_eq!(std::fs::read(root.join("project.toml")).unwrap(), manifest);
}

#[test]
fn atomic_failures_preserve_previous_files_and_remove_temporary() {
    let f = Fixture::new();
    let root = f.package("root", "");
    resolve_locked_path_packages(&root, false).unwrap();
    let path = root.join("project.lock");
    let original = std::fs::read(&path).unwrap();
    let manifest = std::fs::read(root.join("project.toml")).unwrap();
    assert!(atomic_write(&path, b"invalid", || Ok(())).is_err());
    assert!(atomic_write(&path, &original, || anyhow::bail!("injected before rename")).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::read(root.join("project.toml")).unwrap(), manifest);
    assert!(
        !std::fs::read_dir(&root).unwrap().any(|p| p
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp"))
    );
    // Rename failure also cleans up and leaves the destination intact.
    let destination = root.join("directory");
    std::fs::create_dir(&destination).unwrap();
    assert!(atomic_write(&destination, &original, || Ok(())).is_err());
    assert!(destination.is_dir());
    assert!(
        !std::fs::read_dir(&root).unwrap().any(|p| p
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp"))
    );
}

#[test]
fn growing_shared_fanout_and_deep_chains_validate_once() {
    for n in [1, 16, 128] {
        for chain in [false, true] {
            let f = Fixture::new();
            let mut root_deps = String::new();
            for i in 0..n {
                use std::fmt::Write;
                writeln!(root_deps, "a{i}={{path='../p{i}'}}").unwrap();
                writeln!(root_deps, "b{i}={{path='../p{i}'}}").unwrap();
                let deps = if chain && i + 1 < n {
                    format!("next={{path='../p{}'}}", i + 1)
                } else {
                    String::new()
                };
                f.package(&format!("p{i}"), &deps);
            }
            let root = f.package("root", &root_deps);
            resolve_locked_path_packages(&root, false).unwrap();
            VALIDATION_VISITS.with(|counts| counts.set((0, 0)));
            let graph = resolve_locked_path_packages(&root, true).unwrap();
            let edges = 2 * n + if chain { n - 1 } else { 0 };
            assert_eq!(
                VALIDATION_VISITS.with(|counts| counts.get()),
                (n + 1, edges)
            );
            assert_eq!(graph.stats.manifests_loaded, n + 1);
            assert_eq!(graph.stats.dependencies_visited, edges);
            assert_eq!(graph.stats.paths_canonicalized, edges + 1);
            println!(
                "lock n={n} chain={chain}: manifests={} edges={} canonicalizations={}",
                graph.stats.manifests_loaded,
                graph.stats.dependencies_visited,
                graph.stats.paths_canonicalized
            );
        }
    }
}

#[test]
fn edited_dependency_edges_and_tampered_records_are_stale() {
    let f = Fixture::new();
    let root = f.package("root", "a={path='../a'}");
    f.package("a", "");
    f.package("b", "");
    resolve_locked_path_packages(&root, false).unwrap();
    let path = root.join("project.lock");
    let original = std::fs::read_to_string(&path).unwrap();
    for replacement in [
        "alias_changed={path='../a'}",
        "a={path='../b'}",
        "a={path='../missing'}",
        "a={git='unavailable'}",
        "",
    ] {
        f.package("root", replacement);
        assert!(
            resolve_locked_path_packages(&root, true)
                .unwrap_err()
                .to_string()
                .contains("lockfile_stale")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
    f.package("root", "a={path='../a'}");
    for mutation in 0..5 {
        let mut lock: Lock = toml::from_str(&original).unwrap();
        match mutation {
            0 => lock.version = 99,
            1 => lock.packages[0].id = lock.packages[1].id.clone(),
            2 => lock.root.package = "wrong".into(),
            3 => lock.packages[0].checksum = Some("pin-path-contents".into()),
            _ => lock.packages[0].version = "9.0.0".into(),
        }
        std::fs::write(&path, lock.canonical_text().unwrap()).unwrap();
        assert!(
            resolve_locked_path_packages(&root, true)
                .unwrap_err()
                .to_string()
                .contains("lockfile_stale")
        );
    }
}

#[cfg(unix)]
#[test]
fn relative_path_preserves_literal_backslashes() {
    assert_eq!(
        relative_path(Path::new("/a/root"), Path::new("/a/lib\\name")).unwrap(),
        "../lib\\name"
    );
    assert_ne!(
        relative_path(Path::new("/a/root"), Path::new("/a/lib\\name")).unwrap(),
        relative_path(Path::new("/a/root"), Path::new("/a/lib/name")).unwrap()
    );
}

#[test]
fn selective_command_pins_visit_shared_descendants_once() {
    for n in [32, 128, 512, 2048] {
        let f = Fixture::new();
        let mut packages: Vec<_> = (0..n + 4)
            .map(|i| Package {
                id: format!("p{i}"),
                name: format!("p{i}"),
                version: "1.0.0".into(),
                source: Source::Git {
                    url: format!("https://example.invalid/p{i}"),
                },
                revision: Some("a".repeat(40)),
                checksum: None,
                dependencies: vec![],
            })
            .collect();
        let edge = |alias: String, to: usize| Dependency {
            alias,
            package: format!("p{to}"),
            selector: Some("version:^1".into()),
        };
        packages[0].dependencies = vec![edge("selected".into(), 1), edge("other".into(), n + 3)];
        for i in 2..n + 2 {
            packages[1].dependencies.push(edge(format!("a{i}"), i));
            packages[i].dependencies.push(edge("shared".into(), n + 2));
        }
        let lock = Lock {
            version: 1,
            root: Root {
                package: "p0".into(),
            },
            packages,
        };
        std::fs::write(f.0.join("project.lock"), lock.canonical_text().unwrap()).unwrap();
        COMMAND_UNLOCK_VISITS.with(|v| v.set(0));
        let (pins, _) = command_pins(&f.0, Some(Some("selected")), false).unwrap();
        assert_eq!(pins.len(), 2); // root and the unrelated source stay pinned
        assert!(pins.contains_key(&format!("https://example.invalid/p{}", n + 3)));
        let visits = COMMAND_UNLOCK_VISITS.with(|v| v.get());
        assert_eq!(visits, 2 * n + 1);
        eprintln!("n={n} selected_unlock_visits={visits}");
    }
}
