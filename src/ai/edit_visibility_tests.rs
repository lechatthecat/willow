//! Process and inventory proofs for the structured-edit visibility contract.
use super::*;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);
impl Fixture {
    fn new(n: usize) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-visibility-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(
            root.join("helper.wi"),
            "pub fn value() -> i64 { return 1; }",
        )
        .unwrap();
        let mut main = String::from("import helper;\n");
        for i in 0..n {
            fs::write(
                root.join(format!("nested/m{i}.wi")),
                "import helper; pub fn use_value() -> i64 { return helper::value(); }",
            )
            .unwrap();
            main.push_str(&format!("import nested::m{i};\n"));
        }
        main.push_str("fn main() { println(helper::value());");
        for i in 0..n {
            main.push_str(&format!("println(m{i}::use_value());"));
        }
        main.push('}');
        fs::write(root.join("main.wi"), main).unwrap();
        Self(root)
    }
    fn workspace(&self) -> Workspace {
        Workspace::open(&self.0).unwrap()
    }
    fn prepare(&self, w: &Workspace) -> String {
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = w
            .analyze(&self.0.join("main.wi"), false, &mut emitter)
            .unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "value")
            .unwrap()
            .id
            .clone();
        let preview = w
            .prepare(
                &self.0.join("main.wi"),
                false,
                Request {
                    revision: snapshot.revision,
                    operations: vec![Operation::Rename {
                        function,
                        name: "answer".into(),
                    }],
                },
                ChangeFormat::Full,
                &mut emitter,
            )
            .unwrap();
        let id = preview["transaction"].as_str().unwrap().to_owned();
        w.validate(&id, &mut emitter).unwrap();
        id
    }
    fn worker(&self, id: &str, mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "ai::edit::visibility_tests::worker",
                "--nocapture",
            ])
            .env("WILLOW_VISIBILITY_ROOT", &self.0)
            .env("WILLOW_VISIBILITY_ID", id)
            .env("WILLOW_VISIBILITY_MODE", mode);
        command
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Only the isolated worker gets this environment. Parent tests never mutate it.
pub(super) fn checkpoint(stage: &str) {
    if std::env::var("WILLOW_VISIBILITY_STOP").as_deref() == Ok(stage) {
        let root = std::env::var("WILLOW_VISIBILITY_ROOT").unwrap();
        fs::write(Path::new(&root).join("ready"), stage).unwrap();
        loop {
            std::thread::park();
        }
    }
}
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn kill_at(f: &Fixture, id: &str, mode: &str, stage: &str) {
    let ready = f.0.join("ready");
    let _ = fs::remove_file(&ready);
    let mut child = Running(
        f.worker(id, mode)
            .env("WILLOW_VISIBILITY_STOP", stage)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "worker exited before {stage}"
        );
        assert!(Instant::now() < deadline, "worker stalled before {stage}");
        std::thread::sleep(Duration::from_millis(5));
    }
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
}

#[test]
fn worker() {
    let Ok(root) = std::env::var("WILLOW_VISIBILITY_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let id = std::env::var("WILLOW_VISIBILITY_ID").unwrap();
    match std::env::var("WILLOW_VISIBILITY_MODE").unwrap().as_str() {
        "locked" => assert!(Workspace::open(root).is_err()),
        "write" => fs::write(root.join("nested/m0.wi"), "external bytes").unwrap(),
        "move" => fs::rename(root.join("nested/m0.wi"), root.join("moved.wi")).unwrap(),
        "delete" => fs::remove_file(root.join("nested/m0.wi")).unwrap(),
        "recover" => {
            Workspace::open(root).unwrap().recover(&id).unwrap();
        }
        "apply" => {
            let w = Workspace::open(root).unwrap();
            let hook = |index: Option<usize>| {
                checkpoint(&index.map_or_else(|| "finished".into(), |i| format!("before-{i}")));
                Ok(())
            };
            w.apply_observed(
                &id,
                None,
                Some(&hook),
                &mut 0,
                &mut crate::diagnostics::HumanEmitter,
            )
            .unwrap();
        }
        other => panic!("unknown mode {other}"),
    }
}

#[test]
fn process_kill_at_every_apply_boundary_recovers_the_correct_state() {
    // 4 changed files: intent only, applying, every write prefix, committed and cleared.
    for stage in [
        "intent",
        "applying",
        "before-0",
        "before-1",
        "before-2",
        "before-3",
        "written",
        "committed",
        "finished",
    ] {
        let f = Fixture::new(2);
        let w = f.workspace();
        let id = f.prepare(&w);
        let plan = w.load(&id).unwrap();
        assert_eq!(plan.changes.len(), 4);
        fs::write(f.0.join("unrelated.txt"), "keep").unwrap();
        drop(w);
        kill_at(&f, &id, "apply", stage);
        let w = f.workspace();
        if stage == "finished" {
            w.ensure_recovered().unwrap();
        } else {
            assert!(w.preview(&id, ChangeFormat::Full).is_err());
            w.recover(&id).unwrap();
        }
        let committed = matches!(stage, "committed" | "finished");
        for change in &plan.changes {
            assert_eq!(
                fs::read_to_string(f.0.join(&change.path)).unwrap(),
                if committed {
                    change.after.as_str()
                } else {
                    change.before.as_str()
                },
                "{stage}: {}",
                change.path
            );
        }
        assert_eq!(
            fs::read_to_string(f.0.join("unrelated.txt")).unwrap(),
            "keep"
        );
        assert_eq!(
            w.load(&id).unwrap().state,
            if committed { "applied" } else { "aborted" }
        );
        assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
        println!("killed stage={stage} committed={committed} files=4");
    }
}

#[test]
fn process_kill_during_recovery_can_be_retried() {
    for prefix in 0..4 {
        let f = Fixture::new(2);
        let w = f.workspace();
        let id = f.prepare(&w);
        let plan = w.load(&id).unwrap();
        assert!(
            w.apply_inner(&id, Some(3), &mut crate::diagnostics::HumanEmitter)
                .is_err()
        );
        drop(w);
        kill_at(&f, &id, "recover", &format!("recovered-{prefix}"));
        let w = f.workspace();
        w.recover(&id).unwrap();
        for c in &plan.changes {
            assert_eq!(fs::read_to_string(f.0.join(&c.path)).unwrap(), c.before);
        }
    }
}

#[test]
fn another_process_is_excluded_but_raw_readers_see_a_mixed_prefix() {
    let f = Fixture::new(2);
    let w = f.workspace();
    let id = f.prepare(&w);
    let plan = w.load(&id).unwrap();
    let hook = |index| {
        if index == Some(1) {
            assert!(f.worker(&id, "locked").status().unwrap().success());
            for (i, c) in plan.changes.iter().enumerate() {
                assert_eq!(
                    fs::read_to_string(f.0.join(&c.path)).unwrap(),
                    if i == 0 {
                        c.after.as_str()
                    } else {
                        c.before.as_str()
                    }
                );
            }
        }
        Ok(())
    };
    w.apply_observed(
        &id,
        None,
        Some(&hook),
        &mut 0,
        &mut crate::diagnostics::HumanEmitter,
    )
    .unwrap();
    drop(w);
    let w = f.workspace();
    w.ensure_recovered().unwrap();
    for c in plan.changes {
        assert_eq!(fs::read_to_string(f.0.join(c.path)).unwrap(), c.after);
    }
}

#[test]
fn concurrent_process_write_move_and_delete_are_preserved() {
    for mode in ["write", "move", "delete"] {
        for during_apply in [false, true] {
            let f = Fixture::new(8);
            let w = f.workspace();
            let id = f.prepare(&w);
            let plan = w.load(&id).unwrap();
            let target = plan
                .changes
                .iter()
                .position(|c| Path::new(&c.path) == Path::new("nested/m0.wi"))
                .unwrap();
            if !during_apply {
                assert!(f.worker(&id, mode).status().unwrap().success());
            }
            let hook = |index| {
                if index == Some(target) {
                    assert!(f.worker(&id, mode).status().unwrap().success());
                }
                Ok(())
            };
            assert!(
                w.apply_rollback_observed(
                    &id,
                    during_apply.then_some(&hook as &dyn Fn(Option<usize>) -> Result<()>),
                    &mut crate::diagnostics::HumanEmitter
                )
                .is_err()
            );
            for c in &plan.changes {
                if Path::new(&c.path) != Path::new("nested/m0.wi") {
                    assert_eq!(fs::read_to_string(f.0.join(&c.path)).unwrap(), c.before);
                }
            }
            match mode {
                "write" => assert_eq!(
                    fs::read_to_string(f.0.join("nested/m0.wi")).unwrap(),
                    "external bytes"
                ),
                "move" => {
                    assert!(!f.0.join("nested/m0.wi").exists());
                    assert_eq!(
                        fs::read_to_string(f.0.join("moved.wi")).unwrap(),
                        plan.changes[target].before
                    );
                }
                "delete" => assert!(!f.0.join("nested/m0.wi").exists()),
                _ => unreachable!(),
            }
            w.ensure_recovered().unwrap();
        }
    }
}

#[test]
fn recovery_preflight_preserves_moved_deleted_and_modified_sources() {
    for mode in ["write", "move", "delete"] {
        let f = Fixture::new(8);
        let w = f.workspace();
        let id = f.prepare(&w);
        assert!(
            w.apply_inner(&id, Some(2), &mut crate::diagnostics::HumanEmitter)
                .is_err()
        );
        assert!(f.worker(&id, mode).status().unwrap().success());
        let before: BTreeMap<_, _> = w
            .load(&id)
            .unwrap()
            .changes
            .iter()
            .map(|c| (c.path.clone(), fs::read(f.0.join(&c.path)).ok()))
            .collect();
        assert!(w.recover(&id).is_err());
        for (path, bytes) in before {
            assert_eq!(fs::read(f.0.join(path)).ok(), bytes);
        }
        assert!(w.ensure_recovered().is_err());
    }
}

#[test]
fn large_nested_rename_has_one_change_per_file_and_rejects_path_churn() {
    for n in [8, 32, 128] {
        let f = Fixture::new(n);
        let w = f.workspace();
        let id = f.prepare(&w);
        let plan = w.load(&id).unwrap();
        assert_eq!(plan.changes.len(), n + 2);
        assert_eq!(plan.work.patches, n + 2);
        assert_eq!(plan.work.references_visited, n + 1);
        assert_eq!(plan.work.value_probes, n + 2);
        assert_eq!(plan.work.tokens_indexed, 34 * n + 32);
        assert!(plan.changes.iter().all(|c| c.after.contains("answer")
            && !c.after.contains("helper::value()")
            && !c.after.contains("fn value()")));
        w.apply(&id, &mut crate::diagnostics::HumanEmitter).unwrap();
        for c in &plan.changes {
            assert_eq!(fs::read_to_string(f.0.join(&c.path)).unwrap(), c.after);
        }
        // A new large candidate must not recreate a moved/deleted source.
        let snapshot = w
            .analyze(
                &f.0.join("main.wi"),
                false,
                &mut crate::diagnostics::HumanEmitter,
            )
            .unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "answer")
            .unwrap()
            .id
            .clone();
        let preview = w
            .prepare(
                &f.0.join("main.wi"),
                false,
                Request {
                    revision: snapshot.revision,
                    operations: vec![Operation::Rename {
                        function,
                        name: "final_answer".into(),
                    }],
                },
                ChangeFormat::Full,
                &mut crate::diagnostics::HumanEmitter,
            )
            .unwrap();
        let next = preview["transaction"].as_str().unwrap();
        w.validate(next, &mut crate::diagnostics::HumanEmitter)
            .unwrap();
        fs::rename(f.0.join("nested/m0.wi"), f.0.join("moved.wi")).unwrap();
        fs::remove_file(f.0.join("nested/m1.wi")).unwrap();
        assert!(
            w.apply(next, &mut crate::diagnostics::HumanEmitter)
                .is_err()
        );
        for c in plan.changes.iter().filter(|c| {
            let path = Path::new(&c.path);
            path != Path::new("nested/m0.wi") && path != Path::new("nested/m1.wi")
        }) {
            assert_eq!(fs::read_to_string(f.0.join(&c.path)).unwrap(), c.after);
        }
        assert!(!f.0.join("nested/m0.wi").exists());
        assert!(!f.0.join("nested/m1.wi").exists());
        assert_eq!(
            fs::read_to_string(f.0.join("moved.wi")).unwrap(),
            plan.changes
                .iter()
                .find(|c| Path::new(&c.path) == Path::new("nested/m0.wi"))
                .unwrap()
                .after
        );
        println!(
            "rename modules={n} files={} patches={} work={}",
            plan.changes.len(),
            plan.work.patches,
            serde_json::to_string(&plan.work).unwrap()
        );
    }
}

#[cfg(unix)]
#[test]
fn first_use_syncs_metadata_parent_before_journal_and_source_publication() {
    use crate::workspace_transaction::{DurabilityEvent, DurabilityTrace};
    for precreated in [false, true] {
        let f = Fixture::new(2);
        let root = fs::canonicalize(&f.0).unwrap();
        let metadata = root.join(".willow-edits");
        assert!(!metadata.exists(), "must exercise first use");
        if precreated {
            // A competing opener or interrupted initialization can leave an
            // existing directory whose parent entry has not been synchronized.
            fs::create_dir(&metadata).unwrap();
        }
        let trace = DurabilityTrace::start();
        let w = f.workspace();
        let id = f.prepare(&w);
        let plan = w.load(&id).unwrap();
        w.apply(&id, &mut crate::diagnostics::HumanEmitter).unwrap();
        let events = trace.finish();
        assert_eq!(
            events.first(),
            Some(&DurabilityEvent::DirectorySynced(root.clone())),
            "metadata directory entry must be durable before any journal publication"
        );
        let source_paths: std::collections::HashSet<_> =
            plan.changes.iter().map(|c| root.join(&c.path)).collect();
        let is_source_publication = |event: &DurabilityEvent| matches!(event, DurabilityEvent::Published(path) if source_paths.contains(path));
        let first_source = events.iter().position(is_source_publication).unwrap();
        let active = events
            .iter()
            .position(|event| *event == DurabilityEvent::Published(metadata.join("active")))
            .unwrap();
        assert!(active > 0 && active < first_source);
        assert_eq!(
            events[active + 1],
            DurabilityEvent::DirectorySynced(metadata.clone())
        );
        // Applying-state undo journal and its directory sync precede the first replacement.
        assert_eq!(
            events[first_source - 2],
            DurabilityEvent::Published(w.plan_path(&id).unwrap())
        );
        assert_eq!(
            events[first_source - 1],
            DurabilityEvent::DirectorySynced(metadata)
        );
        let publications = events
            .iter()
            .filter(|event| is_source_publication(event))
            .count();
        assert_eq!(publications, 4);
        println!(
            "first_use_order precreated={precreated} root_sync=0 active_publish={active} first_source_publish={first_source} source_publications={publications}"
        );
    }
}
