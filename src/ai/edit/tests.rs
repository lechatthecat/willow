use super::*;
#[test]
fn rename_history_reads_legacy_and_ignores_invalid_metadata() {
    let f = Fixture::new();
    let w = f.workspace();
    let path = w.directory.join("last-local-rename.json");
    let hint = RenameHint {
        selector: "main::old".into(),
        path: "main.wi".into(),
        start: 17,
        digest: "digest".into(),
    };
    durable_write(&path, &serde_json::to_vec(&hint).unwrap()).unwrap();
    let hints = RenameHint::read_all(&f.0);
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].selector, "main::old");
    assert_eq!(hints[0].start, 17);
    for invalid in ["invalid", "{}", "[{}]", "null"] {
        durable_write(&path, invalid.as_bytes()).unwrap();
        assert!(RenameHint::read_all(&f.0).is_empty());
    }
}

#[test]
fn direct_apply_rolls_back_own_writes_on_late_failure_or_race() {
    for race in [0, 1, 2, 3] {
        let f = Fixture::new();
        fs::write(f.0.join("helper.wi"), "pub fn other() -> i64 { return 2; }").unwrap();
        fs::write(f.0.join("main.wi"),"import helper; fn value() -> i64 { return 1; } fn main() { println(value() + helper::other()); }").unwrap();
        let workspace = f.workspace();
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = workspace
            .analyze(&f.0.join("main.wi"), false, &mut emitter)
            .unwrap();
        let operations = snapshot
            .functions
            .iter()
            .filter(|f| matches!(f.name.as_str(), "value" | "other"))
            .map(|f| Operation::ReplaceBody {
                function: f.id.clone(),
                body: "{ return 9; }".into(),
            })
            .collect();
        let preview = workspace
            .prepare_analyzed(
                &f.0.join("main.wi"),
                false,
                Request {
                    revision: snapshot.revision.clone(),
                    operations,
                },
                ChangeFormat::Diff,
                snapshot,
            )
            .unwrap();
        let id = preview["transaction"].as_str().unwrap();
        workspace.validate(id, &mut emitter).unwrap();
        let plan = workspace.load(id).unwrap();
        assert_eq!(plan.changes.len(), 2);
        let second = workspace.root.join(&plan.changes[1].path);
        let third_party = if race == 2 {
            plan.changes[1].after.clone()
        } else {
            format!("{} // concurrent user edit", plan.changes[1].before)
        };
        let hook = |event: Option<usize>| -> Result<()> {
            if event.is_none() && race == 3 {
                anyhow::bail!("injected failure after active marker removal");
            }
            if matches!(event, Some(1)) && race != 3 {
                if race != 0 {
                    fs::write(&second, &third_party)?;
                } else {
                    anyhow::bail!("injected write failure");
                }
            }
            Ok(())
        };
        assert!(
            workspace
                .apply_rollback_observed(id, Some(&hook), &mut emitter)
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(workspace.root.join(&plan.changes[0].path)).unwrap(),
            plan.changes[0].before
        );
        assert_eq!(
            fs::read_to_string(second).unwrap(),
            if race == 1 || race == 2 {
                third_party
            } else {
                plan.changes[1].before.clone()
            }
        );
        assert!(!workspace.directory.join("active").exists());
        assert_eq!(workspace.load(id).unwrap().state, PlanState::Aborted);
    }
}
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-edit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(
            path.join("main.wi"),
            "fn value() -> i64 { return 1; } fn main() { println(value()); }",
        )
        .unwrap();
        Self(path)
    }
    fn workspace(&self) -> Workspace {
        Workspace::open(&self.0).unwrap()
    }
    fn prepare(&self, workspace: &Workspace, body: &str) -> String {
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = workspace
            .analyze(&self.0.join("main.wi"), false, &mut emitter)
            .unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "value")
            .unwrap()
            .id
            .clone();
        let result = workspace
            .prepare(
                &self.0.join("main.wi"),
                false,
                Request {
                    revision: snapshot.revision,
                    operations: vec![Operation::ReplaceBody {
                        function,
                        body: body.into(),
                    }],
                },
                ChangeFormat::Full,
                &mut emitter,
            )
            .unwrap();
        result["transaction"].as_str().unwrap().into()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn durable_write_creates_replaces_and_cleans_up_failed_publication() {
    let fixture = Fixture::new();
    let path = fixture.0.join("journal");
    durable_write(&path, b"prepared").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"prepared");
    durable_write(&path, b"applied").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"applied");
    let directory = fixture.0.join("directory");
    fs::create_dir(&directory).unwrap();
    assert!(durable_write(&directory, b"invalid").is_err());
    assert!(directory.is_dir());
    assert!(fs::read_dir(&fixture.0).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".willow-write-")
    }));
}
#[test]
fn isolated_preview_validate_apply_and_replay_rejection() {
    let f = Fixture::new();
    let w = f.workspace();
    let before = fs::read(f.0.join("main.wi")).unwrap();
    let id = f.prepare(&w, "{ return 2; }");
    assert_eq!(fs::read(f.0.join("main.wi")).unwrap(), before);
    assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
    w.validate(&id, &mut crate::diagnostics::HumanEmitter)
        .unwrap();
    w.apply(&id, &mut crate::diagnostics::HumanEmitter).unwrap();
    assert!(
        fs::read_to_string(f.0.join("main.wi"))
            .unwrap()
            .contains("return 2")
    );
    assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
}
#[test]
fn preview_formats_report_full_texts_or_unified_diffs() {
    let f = Fixture::new();
    let w = f.workspace();
    let id = f.prepare(&w, "{ return 2; }");
    let full = w.preview(&id, ChangeFormat::Full).unwrap();
    let diff = w.preview(&id, ChangeFormat::Diff).unwrap();
    assert_eq!(full["transaction"], diff["transaction"]);
    let full = &full["changes"][0];
    let change = &diff["changes"][0];
    assert_eq!(change["path"], full["path"]);
    assert!(change.get("before").is_none() && change.get("after").is_none());
    let expected = super::super::diff::unified(
        full["path"].as_str().unwrap(),
        full["before"].as_str().unwrap(),
        full["after"].as_str().unwrap(),
    );
    assert_eq!(change["diff"], expected);
    assert!(expected.starts_with("--- a/main.wi\n+++ b/main.wi\n@@ -1,1 +1,1 @@\n-"));
    assert!(expected.contains("+fn value() -> i64 { return 2; }"));
    // Diff previews do not change the transaction; it still validates.
    w.validate(&id, &mut crate::diagnostics::HumanEmitter)
        .unwrap();
}
#[derive(Default)]
struct Collect(Vec<(String, String, usize)>);
impl DiagnosticEmitter for Collect {
    fn emit(&mut self, d: &Diagnostic, sources: &dyn SourceLookup) -> std::io::Result<()> {
        for label in &d.labels {
            let path = sources.get(label.span.file_id).unwrap().path.clone();
            self.0.push((path, d.message.clone(), label.span.line));
        }
        Ok(())
    }
}
#[test]
fn batch_local_rename_rejects_new_destination_capture() {
    let f = Fixture::new();
    let source = "fn main() { let a = 1; let b = 2; println(a + b); }";
    fs::write(f.0.join("main.wi"), source).unwrap();
    let w = f.workspace();
    let mut emitter = crate::diagnostics::HumanEmitter;
    let snapshot = w
        .analyze(&f.0.join("main.wi"), false, &mut emitter)
        .unwrap();
    let operations = snapshot
        .semantic
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Binding)
        .map(|s| Operation::Rename {
            function: s.id.clone(),
            name: "fresh".into(),
        })
        .collect::<Vec<_>>();
    assert_eq!(operations.len(), 2);
    let error = w
        .prepare(
            &f.0.join("main.wi"),
            false,
            Request {
                revision: snapshot.revision.clone(),
                operations,
            },
            ChangeFormat::Full,
            &mut emitter,
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("distinct destination names"));
    assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
}

#[test]
fn validation_diagnostics_use_workspace_relative_paths() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("lib")).unwrap();
    fs::write(
            f.0.join("main.wi"),
            "import lib::helper;\nfn value() -> i64 { return 1; }\nfn main() { println(value() + helper::get()); }",
        )
        .unwrap();
    fs::write(
        f.0.join("lib/helper.wi"),
        "pub fn get() -> i64 {\n  return 2;\n}",
    )
    .unwrap();
    let w = f.workspace();
    let mut emitter = crate::diagnostics::HumanEmitter;
    let snapshot = w
        .analyze(&f.0.join("main.wi"), false, &mut emitter)
        .unwrap();
    let id = |name: &str| {
        snapshot
            .functions
            .iter()
            .find(|g| g.name == name)
            .unwrap()
            .id
            .clone()
    };
    let result = w
        .prepare(
            &f.0.join("main.wi"),
            false,
            Request {
                revision: snapshot.revision.clone(),
                operations: vec![
                    Operation::ReplaceBody {
                        function: id("value"),
                        body: "{ return false; }".into(),
                    },
                    Operation::ReplaceBody {
                        function: id("get"),
                        body: "{\n  return \"two\";\n}".into(),
                    },
                ],
            },
            ChangeFormat::Full,
            &mut emitter,
        )
        .unwrap();
    let tx = result["transaction"].as_str().unwrap();
    let mut collect = Collect::default();
    assert!(w.validate(tx, &mut collect).is_err());
    let paths: std::collections::BTreeSet<_> =
        collect.0.iter().map(|(p, _, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        [
            "lib/helper.wi"
                .replace('/', std::path::MAIN_SEPARATOR_STR)
                .as_str(),
            "main.wi"
        ]
        .into_iter()
        .collect()
    );
    assert!(
        collect
            .0
            .iter()
            .all(|(p, m, line)| !p.contains(".willow-edits")
                && !m.contains(".willow-edits")
                && *line > 0)
    );
}
#[test]
fn validation_failure_and_stale_source_preserve_existing_changes() {
    let f = Fixture::new();
    let w = f.workspace();
    let id = f.prepare(&w, "{ return false; }");
    assert!(
        w.validate(&id, &mut crate::diagnostics::HumanEmitter)
            .is_err()
    );
    assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
    let id = f.prepare(&w, "{ return 3; }");
    w.validate(&id, &mut crate::diagnostics::HumanEmitter)
        .unwrap();
    fs::write(f.0.join("main.wi"), "fn main() {} // user edit").unwrap();
    assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
    assert_eq!(
        fs::read_to_string(f.0.join("main.wi")).unwrap(),
        "fn main() {} // user edit"
    );
}
#[test]
fn candidate_tampering_and_added_files_are_rejected() {
    let f = Fixture::new();
    let w = f.workspace();
    let id = f.prepare(&w, "{ return 2; }");
    w.validate(&id, &mut crate::diagnostics::HumanEmitter)
        .unwrap();
    fs::write(w.directory.join(&id).join("extra.wi"), "fn extra() {}").unwrap();
    assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
    fs::remove_file(w.directory.join(&id).join("extra.wi")).unwrap();
    fs::write(w.directory.join(&id).join("main.wi"), "fn main() {}").unwrap();
    assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
}
#[test]
fn restart_rolls_back_all_files_and_preserves_unrelated_files() {
    let f = Fixture::new();
    let w = f.workspace();
    let id = f.prepare(&w, "{ return 2; }");
    // Exercise the same journal with two files, independently of semantic
    // edit selection. All rollback bytes predate the first workspace write.
    let mut plan = w.load(&id).unwrap();
    fs::write(f.0.join("second.wi"), "before").unwrap();
    fs::write(w.directory.join(&id).join("second.wi"), "after").unwrap();
    plan.inputs.insert("second.wi".into(), hash("before"));
    plan.changes.push(Change {
        path: "second.wi".into(),
        before: "before".into(),
        after: "after".into(),
        offsets: Vec::new(),
    });
    plan.state = PlanState::Validated;
    plan.candidate = candidate(&plan).unwrap();
    w.store(&id, &plan).unwrap();
    fs::write(f.0.join("unrelated.txt"), "keep me").unwrap();
    drop(w);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "ai::edit::tests::crash_worker", "--nocapture"])
        .env("WILLOW_EDIT_TEST_CRASH_ROOT", &f.0)
        .env("WILLOW_EDIT_TEST_CRASH_ID", &id)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(79));
    let w = f.workspace();
    assert!(w.preview(&id, ChangeFormat::Full).is_err());
    w.recover(&id).unwrap();
    for c in plan.changes {
        assert_eq!(fs::read_to_string(f.0.join(c.path)).unwrap(), c.before);
    }
    assert_eq!(
        fs::read_to_string(f.0.join("unrelated.txt")).unwrap(),
        "keep me"
    );
    assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
}
#[test]
fn recovery_conflicts_never_overwrite_user_bytes() {
    let f = Fixture::new();
    let w = f.workspace();
    let id = f.prepare(&w, "{ return 2; }");
    w.validate(&id, &mut crate::diagnostics::HumanEmitter)
        .unwrap();
    assert!(
        w.apply_inner(&id, Some(0), &mut crate::diagnostics::HumanEmitter)
            .is_err()
    );
    fs::write(f.0.join("main.wi"), "user change").unwrap();
    assert!(w.recover(&id).is_err());
    assert_eq!(
        fs::read_to_string(f.0.join("main.wi")).unwrap(),
        "user change"
    );
}
#[test]
fn rename_resolved_calls_and_reject_ambiguous_tokens() {
    let f = Fixture::new();
    let w = f.workspace();
    let mut emitter = crate::diagnostics::HumanEmitter;
    let snapshot = w
        .analyze(&f.0.join("main.wi"), false, &mut emitter)
        .unwrap();
    let function = snapshot
        .functions
        .iter()
        .find(|f| f.name == "value")
        .unwrap()
        .id
        .clone();
    let result = w
        .prepare(
            &f.0.join("main.wi"),
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
    let id = result["transaction"].as_str().unwrap();
    w.validate(id, &mut emitter).unwrap();
    w.apply(id, &mut emitter).unwrap();
    assert_eq!(
        fs::read_to_string(f.0.join("main.wi")).unwrap(),
        "fn answer() -> i64 { return 1; } fn main() { println(answer()); }"
    );
}
#[test]
fn dispatch_family_rename_preserves_overrides() {
    let f = Fixture::new();
    fs::write(f.0.join("main.wi"), "open class Base { pub open fn run(self) -> i64 { return 1; } } class Child extends Base { pub override fn run(self) -> i64 { return 2; } } fn call(x: Base) -> i64 { return x.run(); } fn main() {}").unwrap();
    let w = f.workspace();
    let mut emitter = crate::diagnostics::HumanEmitter;
    let snapshot = w
        .analyze(&f.0.join("main.wi"), false, &mut emitter)
        .unwrap();
    let function = snapshot
        .functions
        .iter()
        .find(|f| f.name == "Base::run")
        .unwrap()
        .id
        .clone();
    let result = w
        .prepare(
            &f.0.join("main.wi"),
            false,
            Request {
                revision: snapshot.revision,
                operations: vec![Operation::Rename {
                    function,
                    name: "execute".into(),
                }],
            },
            ChangeFormat::Full,
            &mut emitter,
        )
        .unwrap();
    let id = result["transaction"].as_str().unwrap();
    w.validate(id, &mut emitter).unwrap();
    w.apply(id, &mut emitter).unwrap();
    let source = fs::read_to_string(f.0.join("main.wi")).unwrap();
    assert_eq!(source.matches("execute").count(), 3);
    assert!(!source.contains("run("));
}
#[test]
fn rename_across_modules_and_replace_body_in_one_candidate() {
    let f = Fixture::new();
    fs::write(
        f.0.join("main.wi"),
        "import helper; fn main() { println(helper::value()); }",
    )
    .unwrap();
    fs::write(f.0.join("helper.wi"), "pub fn value() -> i64 { return 1; }").unwrap();
    let w = f.workspace();
    let mut emitter = crate::diagnostics::HumanEmitter;
    let snapshot = w
        .analyze(&f.0.join("main.wi"), false, &mut emitter)
        .unwrap();
    let function = snapshot
        .functions
        .iter()
        .find(|f| f.name == "value")
        .unwrap()
        .id
        .clone();
    let result = w
        .prepare(
            &f.0.join("main.wi"),
            false,
            Request {
                revision: snapshot.revision,
                operations: vec![
                    Operation::Rename {
                        function: function.clone(),
                        name: "answer".into(),
                    },
                    Operation::ReplaceBody {
                        function,
                        body: "{ return 42; }".into(),
                    },
                ],
            },
            ChangeFormat::Full,
            &mut emitter,
        )
        .unwrap();
    assert_eq!(result["changes"].as_array().unwrap().len(), 2);
    let id = result["transaction"].as_str().unwrap();
    w.validate(id, &mut emitter).unwrap();
    w.apply(id, &mut emitter).unwrap();
    assert!(
        fs::read_to_string(f.0.join("main.wi"))
            .unwrap()
            .contains("helper::answer()")
    );
    assert!(
        fs::read_to_string(f.0.join("helper.wi"))
            .unwrap()
            .contains("answer() -> i64 { return 42; }")
    );
}

/// Rename helper::value to `answer` in a two-or-more file fixture and apply.
fn rename_helper_value(files: &[(&str, &str)]) -> Result<Fixture> {
    let f = Fixture::new();
    for (path, source) in files {
        fs::write(f.0.join(path), source).unwrap();
    }
    let w = f.workspace();
    let mut emitter = crate::diagnostics::HumanEmitter;
    let snapshot = w.analyze(&f.0.join("main.wi"), false, &mut emitter)?;
    let function = snapshot
        .functions
        .iter()
        .find(|g| g.name == "value" && g.module.ends_with("helper.wi"))
        .unwrap()
        .id
        .clone();
    let result = w.prepare(
        &f.0.join("main.wi"),
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
    )?;
    let id = result["transaction"].as_str().unwrap().to_owned();
    w.validate(&id, &mut emitter)?;
    w.apply(&id, &mut emitter)?;
    drop(w);
    Ok(f)
}
const HELPER: (&str, &str) = (
    "helper.wi",
    "pub fn value() -> i64 { return 1; } pub fn other() -> i64 { return 2; }",
);

#[test]
fn rename_rewrites_grouped_item_import_and_bare_calls() {
    let f = rename_helper_value(&[
        HELPER,
        (
            "main.wi",
            "import helper::{other, value};\nfn main() { println(value() + other()); }",
        ),
    ])
    .unwrap();
    assert_eq!(
        fs::read_to_string(f.0.join("main.wi")).unwrap(),
        "import helper::{other, answer};\nfn main() { println(answer() + other()); }"
    );
    assert!(
        fs::read_to_string(f.0.join("helper.wi"))
            .unwrap()
            .starts_with("pub fn answer()")
    );
}

#[test]
fn rename_rewrites_single_item_import() {
    let f = rename_helper_value(&[
        HELPER,
        (
            "main.wi",
            "import helper::value;\nfn main() { println(value()); }",
        ),
    ])
    .unwrap();
    assert_eq!(
        fs::read_to_string(f.0.join("main.wi")).unwrap(),
        "import helper::answer;\nfn main() { println(answer()); }"
    );
}

#[test]
fn rename_keeps_import_alias_and_alias_uses() {
    let f = rename_helper_value(&[
            HELPER,
            (
                "main.wi",
                "import helper::{value as v};\nimport helper::value as w;\nfn main() { println(v() + w()); }",
            ),
        ])
        .unwrap();
    assert_eq!(
        fs::read_to_string(f.0.join("main.wi")).unwrap(),
        "import helper::{answer as v};\nimport helper::answer as w;\nfn main() { println(v() + w()); }"
    );
}

#[test]
fn rename_follows_item_imports_in_every_consumer_module() {
    let f = rename_helper_value(&[
        HELPER,
        (
            "mid.wi",
            "import helper::{value};\npub fn twice() -> i64 { return value() * 2; }",
        ),
        (
            "main.wi",
            "import helper;\nimport mid;\nfn main() { println(helper::value() + mid::twice()); }",
        ),
    ])
    .unwrap();
    assert_eq!(
        fs::read_to_string(f.0.join("mid.wi")).unwrap(),
        "import helper::{answer};\npub fn twice() -> i64 { return answer() * 2; }"
    );
    assert!(
        fs::read_to_string(f.0.join("main.wi"))
            .unwrap()
            .contains("helper::answer()")
    );
}

#[test]
fn rename_rejects_same_spelled_import_of_other_module_with_location() {
    let main =
        "import helper;\nimport other::{value};\nfn main() { println(helper::value() + value()); }";
    let error = rename_helper_value(&[
        HELPER,
        ("other.wi", "pub fn value() -> i64 { return 3; }"),
        ("main.wi", main),
    ])
    .err()
    .unwrap();
    let rejection = error.downcast_ref::<Rejection>().unwrap();
    let l = &rejection.location;
    assert_eq!(l.path, "main.wi");
    assert_eq!((l.line, l.column), (2, 16));
    assert_eq!(&main[l.start..l.end], "value");
    assert!(error.to_string().ends_with("at main.wi:2:16"));
}

#[test]
fn rename_destination_collision_reports_declaration() {
    for count in [1, 16, 64] {
        let helper = format!(
            "// answer is only a comment\npub fn answer() -> i64 {{ return 2; }}\npub fn value() -> i64 {{ {} return 1; }}",
            "answer();".repeat(count)
        );
        let error = rename_helper_value(&[
            ("helper.wi", &helper),
            (
                "main.wi",
                "import helper;\nfn main() { let answer = helper::value(); println(answer); }",
            ),
        ])
        .err()
        .unwrap();
        let rejection = error.downcast_ref::<Rejection>().unwrap();
        assert_eq!(
            rejection.message,
            "rename destination conflicts with an existing declaration"
        );
        let location = &rejection.location;
        let start = helper.find("fn answer").unwrap() + 3;
        assert_eq!(
            serde_json::to_value(location).unwrap(),
            serde_json::json!({
                "path": "helper.wi", "start": start, "end": start + 6,
                "line": 2, "column": 8
            })
        );
        assert_eq!(&helper[location.start..location.end], "answer");
        assert!(error.to_string().ends_with("at helper.wi:2:8"));
    }
}

#[test]
fn rename_destination_collision_prefers_declaration_over_earlier_import() {
    let lib = "pub fn answer() -> i64 { return 2; }";
    let error = rename_helper_value(&[
        HELPER,
        ("lib.wi", lib),
        (
            "app.wi",
            "import lib::{answer};\npub fn run() -> i64 { return answer(); }",
        ),
        (
            "main.wi",
            "import app;\nimport helper;\nfn main() { println(app::run() + helper::value()); }",
        ),
    ])
    .err()
    .unwrap();
    let rejection = error.downcast_ref::<Rejection>().unwrap();
    assert_eq!(
        rejection.message,
        "rename destination conflicts with an existing declaration"
    );
    let l = &rejection.location;
    assert_eq!((l.path.as_str(), l.line, l.column), ("lib.wi", 1, 8));
    assert_eq!(&lib[l.start..l.end], "answer");
}

#[test]
fn rename_supports_function_values() {
    for main in [
        "import helper::{value}; fn main() { let f = value; println(f()); }",
        "import helper::value as v; fn main() { let f = v; println(f()); }",
        "import helper::{value as v}; fn main() { let f = v; println(f()); }",
    ] {
        let f = rename_helper_value(&[HELPER, ("main.wi", main)]).unwrap();
        assert_eq!(
            fs::read_to_string(f.0.join("main.wi")).unwrap(),
            main.replace("value", "answer")
        );
    }
}

#[test]
fn rename_preserves_explicit_alias_equal_to_original_name() {
    for helper in [HELPER.1, "pub const value: i64 = 7;"] {
        for import in [
            "import helper::value as value;",
            "import helper::{value as value};",
        ] {
            let main = format!("{import} fn main() {{ let f = value; }}");
            let f = rename_helper_value(&[("helper.wi", helper), ("main.wi", &main)]).unwrap();
            assert_eq!(
                fs::read_to_string(f.0.join("main.wi")).unwrap(),
                main.replacen("value", "answer", 1)
            );
        }
    }
}

#[test]
fn rename_repeated_module_and_item_names() {
    for (import, usage, expected_import, expected_usage) in [
        (
            "import value::value;",
            "value",
            "import value::answer;",
            "answer",
        ),
        (
            "import value::{value};",
            "value",
            "import value::{answer};",
            "answer",
        ),
        (
            "import value::value as v;",
            "v",
            "import value::answer as v;",
            "v",
        ),
        (
            "import value::{value as v};",
            "v",
            "import value::{answer as v};",
            "v",
        ),
        (
            "import value::value as value;",
            "value",
            "import value::answer as value;",
            "value",
        ),
        (
            "import value::{value as value};",
            "value",
            "import value::{answer as value};",
            "value",
        ),
    ] {
        let f = Fixture::new();
        fs::write(f.0.join("value.wi"), "pub const value: i64 = 7;").unwrap();
        fs::write(
            f.0.join("main.wi"),
            format!("{import} fn main() {{ println({usage}); }}"),
        )
        .unwrap();
        let w = f.workspace();
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = w
            .analyze(&f.0.join("main.wi"), false, &mut emitter)
            .unwrap();
        let id = snapshot
            .functions
            .iter()
            .find(|f| f.name == "value")
            .unwrap()
            .id
            .clone();
        let result = w
            .prepare(
                &f.0.join("main.wi"),
                false,
                Request {
                    revision: snapshot.revision,
                    operations: vec![Operation::Rename {
                        function: id,
                        name: "answer".into(),
                    }],
                },
                ChangeFormat::Full,
                &mut emitter,
            )
            .unwrap();
        let transaction = result["transaction"].as_str().unwrap();
        w.validate(transaction, &mut emitter).unwrap();
        w.apply(transaction, &mut emitter).unwrap();
        assert_eq!(
            fs::read_to_string(f.0.join("value.wi")).unwrap(),
            "pub const answer: i64 = 7;"
        );
        assert_eq!(
            fs::read_to_string(f.0.join("main.wi")).unwrap(),
            format!("{expected_import} fn main() {{ println({expected_usage}); }}")
        );
    }
}

#[test]
fn rename_constant_across_consumers() {
    let f = rename_helper_value(&[
        ("helper.wi", "pub const value: i64 = 7;"),
        (
            "mid.wi",
            "import helper::{value}; pub fn read() -> i64 { return value; }",
        ),
        (
            "main.wi",
            "import helper; import mid; fn main() { println(helper::value + mid::read()); }",
        ),
    ])
    .unwrap();
    assert_eq!(
        fs::read_to_string(f.0.join("helper.wi")).unwrap(),
        "pub const answer: i64 = 7;"
    );
    assert_eq!(
        fs::read_to_string(f.0.join("mid.wi")).unwrap(),
        "import helper::{answer}; pub fn read() -> i64 { return answer; }"
    );
    assert_eq!(
        fs::read_to_string(f.0.join("main.wi")).unwrap(),
        "import helper; import mid; fn main() { println(helper::answer + mid::read()); }"
    );
}

#[test]
fn rename_constant_spellings_and_local_shadowing() {
    let helper = "pub const value: i64 = 7; pub fn read() -> i64 { return value; }";
    for main in [
        "import helper; fn main() { println(helper::value); }",
        "import helper as h; fn main() { println(h::value); }",
        "import helper::value; fn main() { println(value); }",
        "import helper::{value}; fn main() { println(value); }",
        "import helper::value as v; fn main() { println(v); }",
        "import helper::{value as v}; fn main() { println(v); }",
    ] {
        let f = rename_helper_value(&[("helper.wi", helper), ("main.wi", main)]).unwrap();
        assert_eq!(
            fs::read_to_string(f.0.join("main.wi")).unwrap(),
            main.replace("value", "answer")
        );
        assert_eq!(
            fs::read_to_string(f.0.join("helper.wi")).unwrap(),
            helper.replace("value", "answer")
        );
    }
    let main = "import helper::value; fn main() { println(value); if true { let value = 9; println(value); } println(value); } fn shadow(value: i64) -> i64 { return value; }";
    let f = rename_helper_value(&[("helper.wi", helper), ("main.wi", main)]).unwrap();
    assert_eq!(
        fs::read_to_string(f.0.join("main.wi")).unwrap(),
        "import helper::answer; fn main() { println(answer); if true { let value = 9; println(value); } println(answer); } fn shadow(value: i64) -> i64 { return value; }"
    );
}

#[test]
fn edit_scaling_indexes_tokens_once_and_visits_only_target_references() {
    for n in [16, 64, 256, 512] {
        let f = Fixture::new();
        fs::write(
            f.0.join("main.wi"),
            format!(
                "fn value() -> i64 {{ return 1; }} fn main() {{ {} }}",
                "println(value());".repeat(n)
            ),
        )
        .unwrap();
        let w = f.workspace();
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = w
            .analyze(&f.0.join("main.wi"), false, &mut emitter)
            .unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "value")
            .unwrap()
            .id
            .clone();
        let result = w
            .prepare(
                &f.0.join("main.wi"),
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
        assert_eq!(result["work"]["patches"], n + 1);
        assert_eq!(result["work"]["references_visited"], n);
        assert_eq!(result["work"]["tokens_indexed"], 7 * n + 18);
        let id = result["transaction"].as_str().unwrap();
        let plan = w.load(id).unwrap();
        let change = &plan.changes[0];
        assert_eq!(change.offsets.len(), n + 1);
        let path = w.root.join("main.wi").to_string_lossy().into_owned();
        let starts: Vec<_> = change
            .before
            .match_indices("value")
            .map(|(i, _)| i)
            .collect();
        let before_digest = hash(&change.before);
        let after_digest = hash(&change.after);
        let hints: Vec<_> = starts
            .iter()
            .enumerate()
            .map(|(i, &start)| RenameHint {
                selector: format!("main::old_{i}"),
                path: path.clone(),
                start,
                digest: before_digest.clone(),
            })
            .collect();
        durable_write(
            &w.directory.join("last-local-rename.json"),
            &serde_json::to_vec(&hints).unwrap(),
        )
        .unwrap();
        let location = super::super::Location {
            display: None,
            path,
            start: starts[0],
            end: starts[0] + 5,
        };
        w.remember_rename(id, "main::value", &location).unwrap();
        let updated = RenameHint::read_all(&f.0);
        assert_eq!(updated.len(), n + 2);
        let expected: Vec<_> = change
            .after
            .match_indices("answer")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            updated[..n + 1].iter().map(|h| h.start).collect::<Vec<_>>(),
            expected
        );
        assert!(updated.iter().all(|h| h.digest == after_digest));
        println!(
            "edit calls={n} offsets={} hints={} work={}",
            change.offsets.len(),
            updated.len(),
            result["work"]
        );
    }
}
#[test]
fn rename_value_scaling_counts_exact_facts_and_probes() {
    for n in [16, 64, 256] {
        let f = Fixture::new();
        let source = format!(
            "const value: i64 = 7; fn main() {{ {} }}",
            "println(value); if true { let value = 9; println(value); }".repeat(n)
        );
        fs::write(f.0.join("main.wi"), &source).unwrap();
        let w = f.workspace();
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = w
            .analyze(&f.0.join("main.wi"), false, &mut emitter)
            .unwrap();
        let id = snapshot
            .functions
            .iter()
            .find(|f| f.name == "value")
            .unwrap()
            .id
            .clone();
        let sources = BTreeMap::from([("main.wi".into(), source.clone())]);
        let (changes, work) = structured_changes(
            &snapshot,
            &w.root,
            &sources,
            vec![Operation::Rename {
                function: id,
                name: "answer".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            work.value_facts_indexed,
            snapshot.semantic.symbols.len() + snapshot.semantic.references.len()
        );
        assert_eq!(work.value_probes, 3 * n + 1);
        assert_eq!(work.patches, n + 1);
        assert_eq!(changes.len(), 1);
        println!(
            "value occurrences={n} work={}",
            serde_json::to_string(&work).unwrap()
        );
    }
}

#[test]
fn rejects_shadowing_and_declarations_outside_replacement() {
    let f = Fixture::new();
    let w = f.workspace();
    let mut emitter = crate::diagnostics::HumanEmitter;
    let snapshot = w
        .analyze(&f.0.join("main.wi"), false, &mut emitter)
        .unwrap();
    let function = snapshot
        .functions
        .iter()
        .find(|f| f.name == "value")
        .unwrap()
        .id
        .clone();
    for operation in [
        Operation::Rename {
            function: function.clone(),
            name: "main".into(),
        },
        Operation::ReplaceBody {
            function,
            body: "{} fn injected() {}".into(),
        },
    ] {
        assert!(
            w.prepare(
                &f.0.join("main.wi"),
                false,
                Request {
                    revision: snapshot.revision.clone(),
                    operations: vec![operation]
                },
                ChangeFormat::Full,
                &mut emitter
            )
            .is_err()
        );
    }
}

#[test]
fn crash_worker() {
    let Ok(root) = std::env::var("WILLOW_EDIT_TEST_CRASH_ROOT") else {
        return;
    };
    let id = std::env::var("WILLOW_EDIT_TEST_CRASH_ID").unwrap();
    let w = Workspace::open(Path::new(&root)).unwrap();
    assert!(
        w.apply_inner(&id, Some(1), &mut crate::diagnostics::HumanEmitter)
            .is_err()
    );
    // Deliberately skip Rust destructors: the OS releases the file lock.
    std::process::exit(79);
}
#[test]
fn workspace_lock_excludes_other_edit_observers() {
    let f = Fixture::new();
    let first = f.workspace();
    assert!(Workspace::open(&f.0).is_err());
    drop(first);
    assert!(Workspace::open(&f.0).is_ok());
}
#[test]
fn changed_compiler_configuration_rejects_validation_and_apply() {
    let f = Fixture::new();
    let w = f.workspace();
    let id = f.prepare(&w, "{ return 2; }");
    let mut plan = w.load(&id).unwrap();
    plan.configuration = "changed".into();
    plan.candidate = candidate(&plan).unwrap();
    w.store(&id, &plan).unwrap();
    assert!(
        w.validate(&id, &mut crate::diagnostics::HumanEmitter)
            .is_err()
    );
    plan.state = PlanState::Validated;
    w.store(&id, &plan).unwrap();
    assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
    assert!(
        fs::read_to_string(f.0.join("main.wi"))
            .unwrap()
            .contains("return 1;")
    );
}
