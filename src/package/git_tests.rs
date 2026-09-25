use super::*;
use crate::project::{CanonicalGitUrl, GitSelector};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Fixture {
    _session: super::git::GitSession,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let session = super::git::GitSession::new().unwrap();
        let root = session.directory(0);
        fs::create_dir_all(&root).unwrap();
        Self {
            _session: session,
            root,
        }
    }
    fn package(&self, name: &str, version: &str, dependencies: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(path.join("project.toml"), format!("[project]\nname={name:?}\nversion={version:?}\n[willow]\nmanifest-version=1\n[dependencies]\n{dependencies}\n")).unwrap();
        fs::write(
            path.join("src/value.wi"),
            "module value; pub fn get() -> i64 { return 42; }",
        )
        .unwrap();
        path
    }
    fn repo(&self, name: &str, version: &str, dependencies: &str) -> PathBuf {
        let path = self.package(name, version, dependencies);
        git(&path, &["init", "--initial-branch=main", "--template="]);
        git(&path, &["config", "user.name", "Fixture"]);
        git(&path, &["config", "user.email", "fixture@example.invalid"]);
        commit(&path);
        git(&path, &["tag", version]);
        path
    }
    fn root(&self, dependencies: &str) -> PathBuf {
        self.package("app", "1.0.0", dependencies)
    }
}
fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .arg("-c")
        .arg("tag.gpgsign=false")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn commit(path: &Path) -> String {
    git(path, &["add", "project.toml", "src"]);
    git(path, &["commit", "--allow-empty", "-m", "fixture"]);
    git(path, &["rev-parse", "HEAD"])
}
fn url(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy().replace('\\', "/"))
}
fn dep(alias: &str, path: &Path, selector: &str) -> String {
    format!("{alias}={{git={:?},{selector}}}\n", url(path))
}
fn version(graph: &PackageGraph, name: &str) -> String {
    graph
        .packages
        .iter()
        .find(|p| p.identity.name == name)
        .unwrap()
        .identity
        .version
        .clone()
}

#[test]
fn direct_alias_versions_prerelease_and_exact_selectors() {
    let f = Fixture::new();
    let lib = f.repo("real_name", "1.0.0", "");
    let first = git(&lib, &["rev-parse", "HEAD"]);
    for v in ["1.4.0", "2.0.0-beta.1"] {
        f.package("real_name", v, "");
        commit(&lib);
        git(&lib, &["tag", &format!("v{v}")]);
    }
    git(&lib, &["tag", "release-not-semver"]);
    for (selector, expected) in [
        ("version='*'", "1.4.0"),
        ("version='>=2.0.0-beta.1'", "2.0.0-beta.1"),
        ("tag='v1.4.0'", "1.4.0"),
        ("branch='main'", "2.0.0-beta.1"),
    ] {
        let root = f.root(&dep("alias", &lib, selector));
        let graph = resolve_packages(&root).unwrap();
        assert_eq!(version(&graph, "real_name"), expected);
        assert_eq!(graph.packages[0].dependencies[0].alias, "alias");
        assert_eq!(
            graph.packages[1].identity.revision.as_ref().unwrap().len(),
            40
        );
        let checkout = graph.packages[1].root.clone();
        assert!(checkout.exists());
        drop(graph);
        assert!(
            checkout.exists(),
            "shared cache must outlive a compiler session"
        );
    }
    let graph = resolve_packages(&f.root(&dep("alias", &lib, &format!("rev={first:?}")))).unwrap();
    assert_eq!(version(&graph, "real_name"), "1.0.0");
    assert_eq!(
        graph.packages[1].identity.revision.as_ref().unwrap(),
        &first
    );
}

#[test]
fn transitive_diamond_compatible_conflicting_and_backtracking() {
    let f = Fixture::new();
    let shared = f.repo("shared", "1.2.0", "");
    f.package("shared", "1.8.0", "");
    commit(&shared);
    git(&shared, &["tag", "1.8.0"]);
    f.package("shared", "2.0.0", "");
    commit(&shared);
    git(&shared, &["tag", "2.0.0"]);
    let left = f.repo("left", "1.0.0", &dep("inside", &shared, "version='^1.2'"));
    let right = f.repo("right", "1.0.0", &dep("another", &shared, "version='^1.5'"));
    let root = f.root(&(dep("left", &left, "version='1'") + &dep("right", &right, "version='1'")));
    let graph = resolve_packages(&root).unwrap();
    assert_eq!(graph.packages.len(), 4);
    assert_eq!(version(&graph, "shared"), "1.8.0");
    assert_eq!(graph.stats.manifests_loaded, 4);
    assert_eq!(graph.stats.git_sources_fetched, 3);
    // Newest left conflicts transitively; an older left yields a valid solution.
    f.package("left", "1.1.0", &dep("inside", &shared, "version='2'"));
    commit(&left);
    git(&left, &["tag", "1.1.0"]);
    let graph = resolve_packages(&root).unwrap();
    assert_eq!(version(&graph, "left"), "1.0.0");
    assert_eq!(version(&graph, "shared"), "1.8.0");
    assert!(graph.stats.backtracks > 0);
    let root = f.root(&(dep("a", &shared, "version='1'") + &dep("b", &shared, "version='2'")));
    let PackageError::VersionConflict { requirements, .. } = resolve_packages(&root).unwrap_err()
    else {
        panic!("expected conflict")
    };
    assert_eq!(requirements.len(), 2);
    assert!(requirements.iter().all(|r| r.required_by == "app"));
    let json = serde_json::to_value(requirements).unwrap();
    assert!(json[0].get("requirement").is_some());
}

#[test]
fn lock_pins_branch_moved_tag_and_only_explicit_update_advances() {
    for selector in ["branch='main'", "tag='1.0.0'", "version='1'"] {
        let f = Fixture::new();
        let lib = f.repo("lib", "1.0.0", "");
        let root = f.root(&dep("alias", &lib, selector));
        let original = resolve_locked_path_packages(&root, false).unwrap().packages[1]
            .identity
            .clone();
        let bytes = fs::read(root.join("project.lock")).unwrap();
        f.package("lib", "1.1.0", "");
        commit(&lib);
        git(&lib, &["tag", "1.1.0"]);
        if selector.starts_with("tag") {
            git(&lib, &["tag", "-f", "1.0.0"]);
        }
        for locked in [true, false] {
            let graph = resolve_locked_path_packages(&root, locked).unwrap();
            assert_eq!(graph.packages[1].identity, original);
            assert_eq!(fs::read(root.join("project.lock")).unwrap(), bytes);
        }
        if selector.starts_with("tag") {
            assert!(
                update_packages(&root)
                    .unwrap_err()
                    .to_string()
                    .contains("tag_manifest_version_mismatch")
            );
            assert_eq!(fs::read(root.join("project.lock")).unwrap(), bytes);
        } else {
            assert_eq!(version(&update_packages(&root).unwrap(), "lib"), "1.1.0");
        }
    }
}

#[test]
fn changed_branch_selector_is_stale_and_normal_build_resolves_again() {
    let f = Fixture::new();
    let lib = f.repo("lib", "1.0.0", "");
    let root = f.root(&dep("alias", &lib, "branch='main'"));
    resolve_locked_path_packages(&root, false).unwrap();
    git(&lib, &["checkout", "-b", "next"]);
    f.package("lib", "9.0.0", "");
    commit(&lib);
    f.root(&dep("alias", &lib, "branch='next'"));
    assert!(
        resolve_locked_path_packages(&root, true)
            .unwrap_err()
            .to_string()
            .contains("lockfile_stale")
    );
    assert_eq!(
        version(&resolve_locked_path_packages(&root, false).unwrap(), "lib"),
        "9.0.0"
    );
}

#[test]
fn invalid_packages_sources_versions_and_revisions() {
    let f = Fixture::new();
    let lib = f.repo("lib", "1.0.0", "");
    for (selector, error) in [
        ("version='9'", "version_not_found"),
        ("branch='missing'", "git_revision_not_found"),
        ("branch='main~0'", "git_revision_not_found"),
        ("rev='deadbeef'", "git_revision_not_found"),
    ] {
        assert!(
            resolve_packages(&f.root(&dep("lib", &lib, selector)))
                .unwrap_err()
                .to_string()
                .contains(error)
        );
    }
    assert!(
        resolve_packages(&f.root(&dep("lib", &f.root.join("missing"), "version='1'")))
            .unwrap_err()
            .to_string()
            .contains("source_unreachable")
    );
    f.package("lib", "1.1.0", "");
    commit(&lib);
    git(&lib, &["tag", "2.0.0"]);
    assert!(
        resolve_packages(&f.root(&dep("lib", &lib, "version='2'")))
            .unwrap_err()
            .to_string()
            .contains("tag_manifest_version_mismatch")
    );
    let manifest = fs::read_to_string(lib.join("project.toml"))
        .unwrap()
        .replace("[willow]\nmanifest-version=1\n", "");
    fs::write(lib.join("project.toml"), manifest).unwrap();
    let revision = commit(&lib);
    assert!(matches!(
        resolve_packages(&f.root(&dep("lib", &lib, &format!("rev={revision:?}")))).unwrap_err(),
        PackageError::NotWillowPackage(_)
    ));
}

#[test]
fn canonical_sources_and_source_interface() {
    assert_eq!(
        CanonicalGitUrl::new("https://github.com/a/b.git"),
        CanonicalGitUrl::new("https://github.com/a/b")
    );
    assert_ne!(
        CanonicalGitUrl::new("ssh://git@github.com/a/b.git"),
        CanonicalGitUrl::new("https://github.com/a/b")
    );
    let f = Fixture::new();
    let lib = f.repo("lib", "1.0.0", "");
    let source = GitSource::open(
        CanonicalGitUrl::new(&url(&lib)),
        &f.root.join("download"),
        SystemGit,
    )
    .unwrap();
    assert_eq!(
        source.versions().unwrap(),
        vec![semver::Version::new(1, 0, 0)]
    );
    let identity = source
        .resolve(&GitSelector::Version(semver::VersionReq::STAR))
        .unwrap();
    assert_eq!(source.candidate_checks.get(), 1);
    source
        .resolve(&GitSelector::Version(semver::VersionReq::STAR))
        .unwrap();
    assert_eq!(source.candidate_checks.get(), 1);
    let missing = GitSelector::Version(semver::VersionReq::parse("9").unwrap());
    assert!(source.resolve(&missing).is_err());
    assert!(source.resolve(&missing).is_err());
    assert_eq!(source.candidate_checks.get(), 2);
    assert!(
        source
            .materialize(&identity)
            .unwrap()
            .join("src/value.wi")
            .exists()
    );
}

#[test]
fn increasing_alias_fanout_has_one_fetch_and_manifest_per_source() {
    for n in [1, 16, 128] {
        let f = Fixture::new();
        let lib = f.repo("lib", "1.0.0", "");
        let dependencies: String = (0..n)
            .map(|i| dep(&format!("alias{i}"), &lib, "version='1'"))
            .collect();
        super::cache::CHECKSUM_RUNS.with(|count| count.set(0));
        let graph = resolve_packages(&f.root(&dependencies)).unwrap();
        assert_eq!(super::cache::CHECKSUM_RUNS.with(|count| count.get()), 1);
        assert_eq!(graph.stats.manifests_loaded, 2);
        assert_eq!(graph.stats.git_sources_fetched, 1);
        assert_eq!(graph.stats.constraint_checks, n);
        assert_eq!(graph.stats.candidate_attempts, 1);
        println!("git aliases={n}: manifests=2 fetches=1 checks={n} candidates=1 checksum_runs=1");
    }
}

#[test]
fn backjump_skips_unrelated_choices_and_reuses_candidate_manifests() {
    for n in [1, 4, 12] {
        let f = Fixture::new();
        let shared = f.repo("shared", "1.0.0", "");
        f.package("shared", "2.0.0", "");
        commit(&shared);
        git(&shared, &["tag", "2.0.0"]);
        let mut dependencies = String::new();
        for i in 0..n {
            let name = format!("p{i:02}");
            let old_dep = if i == 0 {
                dep("shared", &shared, "version='1'")
            } else {
                String::new()
            };
            let repo = f.repo(&name, "1.0.0", &old_dep);
            let new_dep = if i == 0 {
                dep("shared", &shared, "version='2'")
            } else {
                String::new()
            };
            f.package(&name, "1.1.0", &new_dep);
            commit(&repo);
            git(&repo, &["tag", "1.1.0"]);
            dependencies += &dep(&name, &repo, "version='1'");
        }
        dependencies += &dep("zshared", &shared, "version='1'");
        let graph = resolve_packages(&f.root(&dependencies)).unwrap();
        assert_eq!(version(&graph, "p00"), "1.0.0");
        for i in 1..n {
            assert_eq!(version(&graph, &format!("p{i:02}")), "1.1.0");
        }
        assert_eq!(graph.stats.version_candidates_checked, 2 * (n + 1));
        assert_eq!(graph.stats.backtracks, 1);
        assert_eq!(graph.stats.candidate_attempts, 2 * (n + 1));
        assert_eq!(graph.stats.manifests_loaded, n + 3);
        println!(
            "backjump independent={n}: attempts={} backtracks={} manifests={}",
            graph.stats.candidate_attempts, graph.stats.backtracks, graph.stats.manifests_loaded
        );
    }
}

#[test]
fn backtracking_accumulates_causes_from_exhausted_alternatives() {
    let f = Fixture::new();
    let shared = f.repo("shared", "1.0.0", "");
    f.package("shared", "2.0.0", "");
    commit(&shared);
    git(&shared, &["tag", "2.0.0"]);
    let early = f.repo("early", "1.0.0", &dep("s", &shared, "version='1'"));
    f.package("early", "1.1.0", &dep("s", &shared, "version='2'"));
    commit(&early);
    git(&early, &["tag", "1.1.0"]);
    let later = f.repo("later", "1.0.0", &dep("s", &shared, "version='1'"));
    f.package("later", "1.1.0", &dep("s", &shared, "version='1'"));
    commit(&later);
    git(&later, &["tag", "1.1.0"]);
    let graph = resolve_packages(
        &f.root(&(dep("a", &early, "version='1'") + &dep("b", &later, "version='1'"))),
    )
    .unwrap();
    assert_eq!(version(&graph, "early"), "1.0.0");
    assert_eq!(version(&graph, "later"), "1.1.0");
}

#[test]
fn unsatisfiable_odd_cycle_constraints_have_reproducible_search_counts() {
    // The package graph is acyclic: variables depend on edge packages. Each
    // edge represents an inequality between two binary version choices.
    for n in [3, 5, 7] {
        let f = Fixture::new();
        let edges: Vec<_> = (0..n)
            .map(|i| {
                let name = format!("edge{i}");
                let repo = f.repo(&name, "1.0.0", "");
                f.package(&name, "2.0.0", "");
                commit(&repo);
                git(&repo, &["tag", "2.0.0"]);
                repo
            })
            .collect();
        let mut dependencies = String::new();
        for i in 0..n {
            let name = format!("v{i}");
            let previous = (i + n - 1) % n;
            let one = dep("outgoing", &edges[i], "version='1'")
                + &dep("incoming", &edges[previous], "version='2'");
            let two = dep("outgoing", &edges[i], "version='2'")
                + &dep("incoming", &edges[previous], "version='1'");
            let repo = f.repo(&name, "1.0.0", &one);
            f.package(&name, "2.0.0", &two);
            commit(&repo);
            git(&repo, &["tag", "2.0.0"]);
            dependencies += &dep(&name, &repo, "version='*'");
        }
        assert!(matches!(
            resolve_packages(&f.root(&dependencies)),
            Err(PackageError::VersionConflict { .. })
        ));
        let stats = super::solve::FAILURE_STATS.with(|s| *s.borrow());
        assert!(stats.candidate_attempts > 2 * n);
        assert!(stats.manifests_loaded <= 4 * n + 1);
        assert_eq!(stats.git_sources_fetched, 2 * n);
        println!(
            "unsatisfiable odd-cycle n={n}: attempts={} backtracks={} checks={} manifests={} fetches={}",
            stats.candidate_attempts,
            stats.backtracks,
            stats.constraint_checks,
            stats.manifests_loaded,
            stats.git_sources_fetched
        );
    }
}

#[test]
fn deep_git_chain_visits_each_manifest_and_dependency_once() {
    for n in [1, 8, 32] {
        let f = Fixture::new();
        let mut next = None;
        for i in (0..n).rev() {
            let deps = next
                .as_ref()
                .map_or(String::new(), |p: &PathBuf| dep("next", p, "version='1'"));
            next = Some(f.repo(&format!("p{i}"), "1.0.0", &deps));
        }
        let graph =
            resolve_packages(&f.root(&dep("first", &next.unwrap(), "version='1'"))).unwrap();
        assert_eq!(graph.stats.manifests_loaded, n + 1);
        assert_eq!(graph.stats.dependencies_visited, n);
        assert_eq!(graph.stats.candidate_attempts, n);
        assert_eq!(graph.stats.git_sources_fetched, n);
        println!(
            "git chain={n}: manifests={} edges={} attempts={} fetches={}",
            n + 1,
            n,
            n,
            n
        );
    }
}

#[test]
fn contained_path_dependencies_have_stable_git_identity_and_lock() {
    let f = Fixture::new();
    let lib = f.repo("lib", "1.0.0", "");
    let child = lib.join("child");
    fs::create_dir_all(child.join("src")).unwrap();
    fs::write(
        child.join("project.toml"),
        "[project]\nname='child'\nversion='3.0.0'\n[willow]\nmanifest-version=1\n",
    )
    .unwrap();
    fs::write(
        child.join("src/value.wi"),
        "module value; pub fn get() -> i64 { return 42; }",
    )
    .unwrap();
    f.package("lib", "1.1.0", "child={path='child'}");
    git(&lib, &["add", "child"]);
    commit(&lib);
    git(&lib, &["tag", "1.1.0"]);
    let root = f.root(&dep("lib", &lib, "version='1'"));
    let first = resolve_locked_path_packages(&root, false).unwrap();
    let identity = first
        .packages
        .iter()
        .find(|p| p.identity.name == "child")
        .unwrap()
        .identity
        .clone();
    assert!(matches!(
        identity.source,
        PackageSourceIdentity::GitSubdirectory { .. }
    ));
    assert!(identity.revision.is_some());
    let bytes = fs::read(root.join("project.lock")).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("/checkouts/"));
    drop(first);
    let second = resolve_locked_path_packages(&root, true).unwrap();
    assert_eq!(
        second
            .packages
            .iter()
            .find(|p| p.identity.name == "child")
            .unwrap()
            .identity,
        identity
    );
    assert_eq!(fs::read(root.join("project.lock")).unwrap(), bytes);
}

#[test]
fn git_path_dependencies_cannot_escape_the_snapshot() {
    let f = Fixture::new();
    let outside = f.package("outside", "1.0.0", "");
    let lib = f.repo(
        "lib",
        "1.0.0",
        &format!("escape={{path={:?}}}", outside.to_string_lossy()),
    );
    let error = resolve_packages(&f.root(&dep("lib", &lib, "version='1'"))).unwrap_err();
    assert!(matches!(error, PackageError::PathEscape { .. }));
}

#[test]
fn git_package_metadata_and_layout_are_validated_before_graph_insertion() {
    for invalid in [
        "blank_name",
        "unsupported_marker",
        "invalid_version",
        "missing_src",
    ] {
        let f = Fixture::new();
        let lib = f.repo("lib", "1.0.0", "");
        let manifest = fs::read_to_string(lib.join("project.toml")).unwrap();
        let changed = match invalid {
            "blank_name" => manifest.replace("name=\"lib\"", "name=\" \""),
            "unsupported_marker" => manifest.replace("manifest-version=1", "manifest-version=99"),
            "invalid_version" => manifest.replace("1.0.0", "not-semver"),
            _ => {
                fs::remove_dir_all(lib.join("src")).unwrap();
                manifest
            }
        };
        fs::write(lib.join("project.toml"), changed).unwrap();
        let revision = commit(&lib);
        let error = resolve_packages(&f.root(&dep("alias", &lib, &format!("rev={revision:?}"))))
            .unwrap_err();
        assert!(
            matches!(
                error,
                PackageError::ManifestInvalid { .. } | PackageError::SourceMissing(_)
            ),
            "{invalid}: {error}"
        );
    }
}

#[test]
fn new_transitive_fixed_selectors_are_not_approved_by_an_existing_pin() {
    for selector in ["tag='1.0.0'", "branch='old'"] {
        let f = Fixture::new();
        let lib = f.repo("lib", "1.0.0", "");
        git(&lib, &["branch", "old"]);
        f.package("lib", "1.2.0", "");
        commit(&lib);
        let root = f.root(&dep("lib", &lib, "branch='main'"));
        resolve_locked_path_packages(&root, false).unwrap();
        let before = fs::read(root.join("project.lock")).unwrap();
        f.package("extra", "1.0.0", &dep("lib", &lib, selector));
        let manifest = fs::read(root.join("project.toml")).unwrap();
        assert!(
            mutate_packages(
                &root,
                PackageMutation::Add {
                    alias: Some("extra".into()),
                    git: None,
                    path: Some("../extra".into()),
                    version: None,
                },
                false,
                &mut Vec::new()
            )
            .is_err()
        );
        assert_eq!(fs::read(root.join("project.toml")).unwrap(), manifest);
        assert_eq!(fs::read(root.join("project.lock")).unwrap(), before);
        f.root(&(dep("lib", &lib, "branch='main'") + "extra={path='../extra'}"));
        assert!(fetch_packages(&root, true, true).is_err());
        assert!(fetch_packages(&root, false, false).is_err());
        assert_eq!(fs::read(root.join("project.lock")).unwrap(), before);
    }
}

#[test]
fn adding_and_changing_sources_preserves_unrelated_branch_pins() {
    let f = Fixture::new();
    let lib = f.repo("lib", "1.0.0", "");
    let other = f.repo("other", "1.0.0", "");
    let root = f.root(&dep("lib", &lib, "branch='main'"));
    let original = fetch_packages(&root, false, false).unwrap().packages[1]
        .identity
        .clone();
    f.package("lib", "1.1.0", "");
    commit(&lib);
    f.root(&(dep("lib", &lib, "branch='main'") + &dep("other", &other, "branch='main'")));
    let added = fetch_packages(&root, false, false).unwrap();
    assert_eq!(
        added
            .packages
            .iter()
            .find(|p| p.identity.name == "lib")
            .unwrap()
            .identity,
        original
    );
    git(&other, &["checkout", "-b", "next"]);
    f.package("other", "2.0.0", "");
    commit(&other);
    f.root(&(dep("lib", &lib, "branch='main'") + &dep("other", &other, "branch='next'")));
    let changed = fetch_packages(&root, false, false).unwrap();
    assert_eq!(version(&changed, "other"), "2.0.0");
    assert_eq!(
        changed
            .packages
            .iter()
            .find(|p| p.identity.name == "lib")
            .unwrap()
            .identity,
        original
    );
    fetch_packages(&root, true, true).unwrap();
}

#[test]
fn update_prunes_deleted_tags_and_branches_from_warm_cache() {
    let f = Fixture::new();
    let lib = f.repo("lib", "1.0.0", "");
    f.package("lib", "1.1.0", "");
    commit(&lib);
    git(&lib, &["tag", "1.1.0"]);
    git(&lib, &["branch", "removed"]);
    let root = f.root(&dep("lib", &lib, "version='1'"));
    assert_eq!(
        version(&fetch_packages(&root, false, false).unwrap(), "lib"),
        "1.1.0"
    );
    git(&lib, &["tag", "-d", "1.1.0"]);
    git(&lib, &["branch", "-D", "removed"]);
    assert_eq!(version(&update_packages(&root).unwrap(), "lib"), "1.0.0");
    f.root(&dep("lib", &lib, "branch='removed'"));
    assert!(
        update_packages(&root)
            .unwrap_err()
            .to_string()
            .contains("git_revision_not_found")
    );
    let cold = GitSource::open(
        CanonicalGitUrl::new(&url(&lib)),
        &f.root.join("cold"),
        SystemGit,
    )
    .unwrap();
    assert_eq!(
        cold.resolve(&GitSelector::Version("1".parse().unwrap()))
            .unwrap()
            .version,
        "1.0.0"
    );
    assert!(
        cold.resolve(&GitSelector::Branch("removed".into()))
            .is_err()
    );
}

#[test]
fn repeated_locked_selectors_have_linear_constraint_checks() {
    for n in [1, 16, 128] {
        let f = Fixture::new();
        let lib = f.repo("lib", "1.0.0", "");
        let deps: String = (0..n)
            .map(|i| dep(&format!("a{i}"), &lib, "branch='main'"))
            .collect();
        let root = f.root(&deps);
        fetch_packages(&root, false, false).unwrap();
        let graph = fetch_packages(&root, true, true).unwrap();
        assert_eq!(graph.stats.constraint_checks, n);
        assert_eq!(graph.stats.candidate_attempts, 1);
        assert_eq!(graph.stats.manifests_loaded, 2);
        assert_eq!(graph.stats.git_sources_fetched, 0);
        println!("locked aliases={n}: checks={n} assignments=1 manifests=2 fetches=0");
    }
}

#[test]
fn independent_pin_repairs_reuse_the_resolved_prefix() {
    for n in [1, 8, 32] {
        let f = Fixture::new();
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..n {
            let name = format!("p{i:03}");
            let lib = f.repo(&name, "1.0.0", "");
            old += &dep(&name, &lib, "tag='1.0.0'");
            f.package(&name, "2.0.0", "");
            commit(&lib);
            git(&lib, &["tag", "2.0.0"]);
            new += &dep(&name, &lib, "tag='2.0.0'");
        }
        let root = f.root(&old);
        fetch_packages(&root, false, false).unwrap();
        f.root(&new);
        let graph = fetch_packages(&root, false, false).unwrap();
        assert_eq!(graph.stats.dependencies_visited, n);
        assert_eq!(graph.stats.constraint_checks, n);
        assert_eq!(graph.stats.manifests_loaded, n + 1);
        assert_eq!(graph.stats.candidate_attempts, n);
        assert_eq!(graph.stats.backtracks, n);
        assert_eq!(graph.stats.git_sources_fetched, n);
        assert!(
            graph
                .packages
                .iter()
                .skip(1)
                .all(|p| p.identity.version == "2.0.0")
        );
        println!(
            "independent repairs={n}: edges={n} checks={n} manifests={} assignments={n} backtracks={n} fetches={n}",
            n + 1
        );
    }
}

#[test]
fn compatible_new_tag_is_checked_and_then_reused_as_a_lock_selector() {
    let f = Fixture::new();
    let lib = f.repo("lib", "1.0.0", "");
    let root = f.root(&dep("lib", &lib, "branch='main'"));
    fetch_packages(&root, false, false).unwrap();
    // This ref did not exist during the initial fetch.
    git(&lib, &["tag", "release"]);
    f.package("extra", "1.0.0", &dep("lib", &lib, "tag='release'"));
    mutate_packages(
        &root,
        PackageMutation::Add {
            alias: Some("extra".into()),
            git: None,
            path: Some("../extra".into()),
            version: None,
        },
        false,
        &mut Vec::new(),
    )
    .unwrap();
    fs::rename(&lib, f.root.join("hidden-lib")).unwrap();
    fetch_packages(&root, true, true).unwrap();
}

#[test]
fn late_transitive_conflict_repairs_only_the_related_pin() {
    let f = Fixture::new();
    let lib = f.repo("lib", "1.0.0", "");
    let unrelated = f.repo("unrelated", "1.0.0", "");
    let deps = dep("a", &lib, "version='*'") + &dep("b", &unrelated, "branch='main'");
    let root = f.root(&deps);
    fetch_packages(&root, false, false).unwrap();
    f.package("lib", "2.0.0", "");
    commit(&lib);
    git(&lib, &["tag", "2.0.0"]);
    f.package("unrelated", "9.0.0", "");
    commit(&unrelated);
    f.package("extra", "1.0.0", &dep("lib", &lib, "version='2'"));
    f.root(&(deps + "z={path='../extra'}"));
    let graph = fetch_packages(&root, false, false).unwrap();
    assert_eq!(version(&graph, "lib"), "2.0.0");
    assert_eq!(version(&graph, "unrelated"), "1.0.0");
    assert_eq!(graph.stats.git_sources_fetched, 1);
    assert_eq!(graph.stats.manifests_loaded, 5);
    fetch_packages(&root, true, true).unwrap();
}
