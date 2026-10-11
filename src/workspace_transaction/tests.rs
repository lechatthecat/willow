use super::*;
use std::process::Command;
thread_local! { pub(super) static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-workspace-journal-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        let fixture = Self(path);
        for name in ["app", "a", "b", "c", "d"] {
            fixture.package(name);
        }
        fixture
    }
    fn package(&self, name: &str) {
        let root = self.0.join(name);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("project.toml"), format!("# preserved\n[project]\nname = '{name}'\nversion = '1.0.0'\n[willow]\nmanifest-version = 1\n")).unwrap();
        fs::write(root.join("src/main.wi"), "fn main() { println(42); }").unwrap();
    }
    fn root(&self) -> PathBuf {
        self.0.join("app")
    }
    fn worker(&self, mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "workspace_transaction::tests::worker",
                "--nocapture",
            ])
            .env("WILLOW_TRANSACTION_TEST_ROOT", self.root())
            .env("WILLOW_TRANSACTION_TEST_MODE", mode);
        command
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn crash_after(count: usize) {
    if std::env::var("WILLOW_TRANSACTION_TEST_CRASH_AFTER")
        .ok()
        .as_deref()
        == Some(count.to_string().as_str())
    {
        std::process::exit(79); // No destructors: OS releases the lease.
    }
}

#[test]
fn worker() {
    let Ok(root) = std::env::var("WILLOW_TRANSACTION_TEST_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    match std::env::var("WILLOW_TRANSACTION_TEST_MODE")
        .unwrap()
        .as_str()
    {
        "package" => {
            let alias =
                std::env::var("WILLOW_TRANSACTION_TEST_ALIAS").unwrap_or_else(|_| "a".into());
            crate::package::mutate_packages(
                root,
                crate::package::PackageMutation::Add {
                    alias: Some(alias.clone()),
                    git: None,
                    path: Some(format!("../{alias}")),
                    version: None,
                },
                false,
                &mut Vec::new(),
            )
            .unwrap();
        }
        "rust" | "rust-remove" => {
            let mut options =
                crate::toolchain::rust_bridge::BridgeOptions::new(crate::BuildMode::Debug).unwrap();
            options.offline = true;
            options.cache_root = root.parent().unwrap().join("cache");
            let mutation =
                if std::env::var("WILLOW_TRANSACTION_TEST_MODE").unwrap() == "rust-remove" {
                    crate::toolchain::rust_bridge::commands::Mutation::Remove {
                        name: "fixture".into(),
                    }
                } else {
                    crate::toolchain::rust_bridge::commands::Mutation::Add {
                        name: "fixture".into(),
                        spec: crate::project::RustDependencySpec {
                            path: Some("../rust-dep".into()),
                            ..Default::default()
                        },
                    }
                };
            crate::toolchain::rust_bridge::commands::mutate(root, &options, mutation).unwrap();
        }
        "relative-manifest" => {
            let manifest =
                crate::project::ProjectManifest::load(Path::new("project.toml")).unwrap();
            assert_eq!(manifest.project.name, "app");
        }
        "recover" => {
            let _tx = WorkspaceFileTransaction::open(root).unwrap();
        }
        "try-lease" => {
            assert!(WorkspaceFileTransaction::try_open(root).is_err());
            std::process::exit(80);
        }
        mode => panic!("unknown worker mode: {mode}"),
    }
}

#[test]
fn package_process_death_recovers_before_locked_offline_resolution() {
    for phase in 0..=2 {
        let f = Fixture::new();
        crate::package::fetch_packages(&f.root(), false, true).unwrap();
        let manifest = fs::read(f.root().join("project.toml")).unwrap();
        let lock = fs::read(f.root().join("project.lock")).unwrap();
        let output = f
            .worker("package")
            .env("WILLOW_TRANSACTION_TEST_CRASH_AFTER", phase.to_string())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(79),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            f.root()
                .join(".willow-edits/workspace-journal.json")
                .exists()
        );
        crate::package::fetch_packages(&f.root(), true, true).unwrap();
        assert_eq!(fs::read(f.root().join("project.toml")).unwrap(), manifest);
        assert_eq!(fs::read(f.root().join("project.lock")).unwrap(), lock);
        assert!(
            !f.root()
                .join(".willow-edits/workspace-journal.json")
                .exists()
        );
    }
}

fn rust_fixture(f: &Fixture) {
    fs::write(
        f.root().join("bridge.rs"),
        "// intentionally empty bridge\n",
    )
    .unwrap();
    let path = f.root().join("project.toml");
    let mut manifest = fs::read_to_string(&path).unwrap();
    manifest.push_str("[rust]\nbridge = 'bridge.rs'\n");
    fs::write(path, manifest).unwrap();
    let dep = f.0.join("rust-dep");
    fs::create_dir_all(dep.join("src")).unwrap();
    fs::write(
        dep.join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='1.0.0'\nedition='2024'\n[workspace]\n",
    )
    .unwrap();
    fs::write(dep.join("src/lib.rs"), "pub fn value() -> i64 { 42 }\n").unwrap();
}

#[test]
fn rust_process_death_recovers_manifest_and_both_locks() {
    for phase in 0..=3 {
        let f = Fixture::new();
        rust_fixture(&f);
        crate::package::fetch_packages(&f.root(), false, true).unwrap();
        let manifest = fs::read(f.root().join("project.toml")).unwrap();
        let lock = fs::read(f.root().join("project.lock")).unwrap();
        let output = f
            .worker("rust")
            .env("WILLOW_TRANSACTION_TEST_CRASH_AFTER", phase.to_string())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(79),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        crate::project::ProjectManifest::load(&f.root().join("project.toml")).unwrap();
        assert_eq!(fs::read(f.root().join("project.toml")).unwrap(), manifest);
        assert_eq!(fs::read(f.root().join("project.lock")).unwrap(), lock);
        assert!(!f.root().join(".willow/rust/Cargo.lock").exists());
        crate::package::fetch_packages(&f.root(), true, true).unwrap();
    }
}

#[test]
fn concurrent_package_processes_keep_every_update() {
    let f = Fixture::new();
    let mut children: Vec<_> = ["a", "b", "c", "d"]
        .into_iter()
        .map(|alias| {
            f.worker("package")
                .env("WILLOW_TRANSACTION_TEST_ALIAS", alias)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    let manifest = crate::project::ProjectManifest::load(&f.root().join("project.toml")).unwrap();
    assert_eq!(manifest.dependencies.len(), 4);
    assert_eq!(
        crate::package::fetch_packages(&f.root(), true, true)
            .unwrap()
            .packages
            .len(),
        5
    );
}

#[test]
fn common_lease_excludes_package_process_during_ai_edit() {
    let f = Fixture::new();
    let ai = crate::ai::edit::Workspace::open(&f.root()).unwrap();
    let output = f.worker("try-lease").output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(80),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(ai);
    assert!(f.worker("package").status().unwrap().success());
}

#[test]
fn recovery_preflights_conflicts_and_restarts_idempotently() {
    let f = Fixture::new();
    let tx = WorkspaceFileTransaction::open(&f.root()).unwrap();
    fs::write(f.root().join("one"), b"old").unwrap();
    fs::write(f.root().join("two"), b"old").unwrap();
    tx.begin(vec![
        FileChange::new("one", Some(b"old".to_vec()), Some(b"new".to_vec())),
        FileChange::new("two", Some(b"old".to_vec()), Some(b"new".to_vec())),
    ])
    .unwrap();
    durable_write(&f.root().join("one"), b"new").unwrap();
    fs::write(f.root().join("two"), b"third-party").unwrap();
    assert!(tx.recover().is_err());
    assert_eq!(fs::read(f.root().join("one")).unwrap(), b"new");
    assert_eq!(fs::read(f.root().join("two")).unwrap(), b"third-party");
    fs::write(f.root().join("two"), b"old").unwrap();
    tx.recover().unwrap();
    tx.recover().unwrap();
    assert_eq!(fs::read(f.root().join("one")).unwrap(), b"old");
}

#[test]
fn journal_restores_created_deleted_binary_and_unchanged_files() {
    let f = Fixture::new();
    let tx = WorkspaceFileTransaction::open(&f.root()).unwrap();
    fs::write(f.root().join("deleted"), [0, 255, 1]).unwrap();
    fs::write(f.root().join("same"), b"same").unwrap();
    let journal = tx
        .begin(vec![
            FileChange::new("created", None, Some(vec![255, 0])),
            FileChange::new("deleted", Some(vec![0, 255, 1]), None),
            FileChange::new("same", Some(b"same".to_vec()), Some(b"same".to_vec())),
        ])
        .unwrap();
    for c in &journal.changes {
        if c.before != c.after {
            replace_optional(&tx.file(&c.path).unwrap(), c.after.as_deref()).unwrap();
        }
    }
    drop(tx);
    let _tx = WorkspaceFileTransaction::open(&f.root()).unwrap();
    assert!(!f.root().join("created").exists());
    assert_eq!(fs::read(f.root().join("deleted")).unwrap(), [0, 255, 1]);
    assert_eq!(fs::read(f.root().join("same")).unwrap(), b"same");
}

#[test]
fn stale_duplicate_and_escaping_candidates_never_publish() {
    let f = Fixture::new();
    let tx = WorkspaceFileTransaction::open(&f.root()).unwrap();
    for changes in [
        vec![FileChange::new(
            "project.toml",
            Some(b"stale".to_vec()),
            Some(b"new".to_vec()),
        )],
        vec![
            FileChange::new("x", None, None),
            FileChange::new("x", None, None),
        ],
        vec![FileChange::new("../outside", None, None)],
        vec![FileChange::new(".willow-edits/lock", None, None)],
    ] {
        assert!(tx.publish(changes).is_err());
        assert!(!tx.journal_path().exists());
    }
}

#[test]
fn failed_write_rolls_back_prior_files() {
    let f = Fixture::new();
    let tx = WorkspaceFileTransaction::open(&f.root()).unwrap();
    let before = fs::read(f.root().join("project.toml")).unwrap();
    assert!(
        tx.publish(vec![
            FileChange::new("project.toml", Some(before.clone()), Some(b"new".to_vec())),
            FileChange::new("missing-parent/lock", None, Some(b"new".to_vec())),
        ])
        .is_err()
    );
    assert_eq!(fs::read(f.root().join("project.toml")).unwrap(), before);
    assert!(!tx.journal_path().exists());
}

#[test]
fn transaction_scaling_serializes_each_file_once_per_journal() {
    for count in [16, 64, 256, 1024] {
        let f = Fixture::new();
        let tx = WorkspaceFileTransaction::open(&f.root()).unwrap();
        let changes = (0..count)
            .map(|i| FileChange::new(&format!("file{i:04}"), None, Some(vec![b'x'; 32])))
            .collect();
        READS.with(|n| n.set(0));
        let journal = tx.begin(changes).unwrap();
        assert_eq!(READS.with(|n| n.get()), count);
        let persisted: Journal =
            serde_json::from_slice(&fs::read(tx.journal_path()).unwrap()).unwrap();
        assert_eq!(persisted.changes.len(), count);
        assert_eq!(
            persisted
                .changes
                .iter()
                .map(|c| c.after.as_ref().unwrap().len())
                .sum::<usize>(),
            count * 32
        );
        let bytes = fs::metadata(tx.journal_path()).unwrap().len();
        eprintln!(
            "workspace_journal files={count} payload={} journal_bytes={bytes}",
            count * 32
        );
        assert!(bytes < (count * 256 + 128) as u64);
        assert_eq!(journal.changes.len(), count);
        tx.recover().unwrap();
        assert_eq!(READS.with(|n| n.get()), 3 * count + 1);
        eprintln!(
            "workspace_journal files={count} begin_and_recovery_reads={}",
            READS.with(|n| n.get())
        );
    }
}

#[test]
fn rust_removal_crash_restores_existing_cargo_lock_and_frozen_metadata() {
    for phase in 0..=3 {
        let f = Fixture::new();
        rust_fixture(&f);
        assert!(f.worker("rust").output().unwrap().status.success());
        let paths = ["project.toml", "project.lock", ".willow/rust/Cargo.lock"];
        let before: Vec<_> = paths
            .iter()
            .map(|path| fs::read(f.root().join(path)).unwrap())
            .collect();
        let output = f
            .worker("rust-remove")
            .env("WILLOW_TRANSACTION_TEST_CRASH_AFTER", phase.to_string())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(79),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let project =
            crate::project::ProjectManifest::load(&f.root().join("project.toml")).unwrap();
        for (path, expected) in paths.iter().zip(before) {
            assert_eq!(fs::read(f.root().join(path)).unwrap(), expected);
        }
        let mut options =
            crate::toolchain::rust_bridge::BridgeOptions::new(crate::BuildMode::Debug).unwrap();
        options.locked = true;
        options.offline = true;
        options.cache_root = f.0.join("cache");
        assert!(
            crate::toolchain::rust_bridge::inspect_bridge(&project, &f.root(), &options)
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn concurrent_rust_and_package_commands_preserve_both_graphs() {
    let f = Fixture::new();
    rust_fixture(&f);
    let mut children = vec![
        f.worker("rust")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    ];
    for alias in ["a", "b", "c", "d"] {
        children.push(
            f.worker("package")
                .env("WILLOW_TRANSACTION_TEST_ALIAS", alias)
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
    }
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    let project = crate::project::ProjectManifest::load(&f.root().join("project.toml")).unwrap();
    assert_eq!(project.dependencies.len(), 4);
    assert_eq!(project.rust_dependencies.len(), 1);
    crate::package::fetch_packages(&f.root(), true, true).unwrap();
    let mut options =
        crate::toolchain::rust_bridge::BridgeOptions::new(crate::BuildMode::Debug).unwrap();
    options.locked = true;
    options.offline = true;
    options.cache_root = f.0.join("cache");
    assert!(
        crate::toolchain::rust_bridge::inspect_bridge(&project, &f.root(), &options)
            .unwrap()
            .is_some()
    );
}

#[test]
fn journal_payload_size_scales_without_repeated_candidates() {
    for size in [32, 128, 512, 2048] {
        let f = Fixture::new();
        let tx = WorkspaceFileTransaction::open(&f.root()).unwrap();
        let changes = (0..16)
            .map(|i| FileChange::new(&format!("file{i:04}"), None, Some(vec![b'x'; size])))
            .collect();
        let journal = tx.begin(changes).unwrap();
        assert_eq!(
            journal
                .changes
                .iter()
                .map(|c| c.after.as_ref().unwrap().len())
                .sum::<usize>(),
            size * 16
        );
        let bytes = fs::metadata(tx.journal_path()).unwrap().len();
        eprintln!(
            "workspace_journal files=16 bytes_per_file={size} payload={} journal_bytes={bytes}",
            size * 16
        );
        assert!(bytes < (size * 16 * 4 + 16 * 128 + 128) as u64);
        tx.recover().unwrap();
    }
}

pub(super) fn recovery_crash_after(count: usize) {
    if std::env::var("WILLOW_TRANSACTION_TEST_RECOVERY_CRASH_AFTER")
        .ok()
        .as_deref()
        == Some(count.to_string().as_str())
    {
        std::process::exit(81);
    }
}

#[test]
fn recovery_itself_can_die_and_restart() {
    let f = Fixture::new();
    let tx = WorkspaceFileTransaction::open(&f.root()).unwrap();
    let mut changes = Vec::new();
    for name in ["one", "two", "three"] {
        fs::write(f.root().join(name), b"old").unwrap();
        changes.push(FileChange::new(
            name,
            Some(b"old".to_vec()),
            Some(b"new".to_vec()),
        ));
    }
    let journal = tx.begin(changes).unwrap();
    for c in &journal.changes {
        durable_write(&tx.file(&c.path).unwrap(), c.after.as_deref().unwrap()).unwrap();
    }
    drop(tx);
    let output = f
        .worker("recover")
        .env("WILLOW_TRANSACTION_TEST_RECOVERY_CRASH_AFTER", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(81));
    assert_eq!(fs::read(f.root().join("one")).unwrap(), b"old");
    assert_eq!(fs::read(f.root().join("two")).unwrap(), b"new");
    let _tx = WorkspaceFileTransaction::open(&f.root()).unwrap();
    for name in ["one", "two", "three"] {
        assert_eq!(fs::read(f.root().join(name)).unwrap(), b"old");
    }
}

#[test]
fn missing_original_project_lock_stays_missing_after_crash() {
    for phase in 0..=2 {
        let f = Fixture::new();
        let before = fs::read(f.root().join("project.toml")).unwrap();
        let output = f
            .worker("package")
            .env("WILLOW_TRANSACTION_TEST_CRASH_AFTER", phase.to_string())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(79));
        crate::project::ProjectManifest::load(&f.root().join("project.toml")).unwrap();
        assert_eq!(fs::read(f.root().join("project.toml")).unwrap(), before);
        assert!(!f.root().join("project.lock").exists());
        assert!(
            crate::package::fetch_packages(&f.root(), true, true)
                .unwrap_err()
                .to_string()
                .contains("lockfile_missing")
        );
    }
}

#[test]
fn no_op_publication_does_not_create_an_intent_or_write_targets() {
    let f = Fixture::new();
    let tx = WorkspaceFileTransaction::open(&f.root()).unwrap();
    let path = f.root().join("project.toml");
    let before = fs::read(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    tx.publish(vec![FileChange::new(
        "project.toml",
        Some(before.clone()),
        Some(before),
    )])
    .unwrap();
    assert!(!tx.journal_path().exists());
    assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
}

#[test]
fn opposing_dependency_reads_fail_without_a_cross_workspace_deadlock() {
    let f = Fixture::new();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let mut threads = Vec::new();
        for (own, other) in [("app", "a"), ("a", "app")] {
            let own = f.0.join(own);
            let other = f.0.join(other);
            let barrier = &barrier;
            threads.push(scope.spawn(move || {
                let _tx = WorkspaceFileTransaction::open(&own).unwrap();
                barrier.wait();
                let result = crate::project::ProjectManifest::load(&other.join("project.toml"));
                assert!(result.is_err());
                barrier.wait(); // Keep both leases alive until both attempts finish.
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
    });
}

#[test]
fn relative_manifest_loading_with_existing_metadata() {
    let f = Fixture::new();
    rust_fixture(&f);
    drop(WorkspaceFileTransaction::open(&f.root()).unwrap());
    let before = fs::read(f.root().join("project.toml")).unwrap();
    let output = f
        .worker("relative-manifest")
        .current_dir(f.root())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(f.root().join("project.toml")).unwrap(), before);
}

#[test]
fn relative_manifest_loading_recovers_pending_journal() {
    let f = Fixture::new();
    rust_fixture(&f);
    crate::package::fetch_packages(&f.root(), false, true).unwrap();
    let manifest = fs::read(f.root().join("project.toml")).unwrap();
    let lock = fs::read(f.root().join("project.lock")).unwrap();
    let tx = WorkspaceFileTransaction::open(&f.root()).unwrap();
    // Invalid intermediate contents prove that recovery precedes parsing.
    let journal = tx
        .begin(vec![
            FileChange::new(
                "project.toml",
                Some(manifest.clone()),
                Some(b"partial manifest".to_vec()),
            ),
            FileChange::new(
                "project.lock",
                Some(lock.clone()),
                Some(b"partial lock".to_vec()),
            ),
        ])
        .unwrap();
    for change in &journal.changes {
        durable_write(
            &tx.file(&change.path).unwrap(),
            change.after.as_deref().unwrap(),
        )
        .unwrap();
    }
    drop(tx);
    let output = f
        .worker("relative-manifest")
        .current_dir(f.root())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(f.root().join("project.toml")).unwrap(), manifest);
    assert_eq!(fs::read(f.root().join("project.lock")).unwrap(), lock);
    assert!(
        !f.root()
            .join(".willow-edits/workspace-journal.json")
            .exists()
    );
}
