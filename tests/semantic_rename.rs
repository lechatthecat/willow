use serde_json::Value;
use std::{fs, path::PathBuf, process::Command};

struct Fixture(PathBuf);
impl Fixture {
    fn new(source: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-direct-rename-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("main.wi"), source).unwrap();
        Self(fs::canonicalize(root).unwrap())
    }
    fn write(&self, path: &str, text: &str) {
        fs::write(self.0.join(path), text).unwrap();
    }
    fn run(&self, args: &[&str], code: i32) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.0)
            .args(args)
            .args(["--source", "main.wi", "--format=json"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn human(&self, args: &[&str], code: i32) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.0)
            .args(args)
            .args(["--source", "main.wi"])
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.code(), Some(code), "{text}");
        text
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn direct_rename_previews_without_metadata_and_applies_cross_file_imports() {
    for import in [
        "import helper;",
        "import helper::value;",
        "import helper::value as alias;",
    ] {
        let call = if import.contains("alias") {
            "alias()"
        } else if import.contains("::value") {
            "value()"
        } else {
            "helper::value()"
        };
        let f = Fixture::new(&format!("{import} fn main() {{ println({call}); }}"));
        f.write("helper.wi", "pub fn value() -> i64 { return 2; }");
        let before = fs::read(f.0.join("main.wi")).unwrap();
        let preview = f.run(&["rename", "helper::value", "answer", "--dry-run"], 0);
        assert!(preview["result"]["changes"].as_array().unwrap().len() == 2);
        assert_eq!(fs::read(f.0.join("main.wi")).unwrap(), before);
        assert!(!f.0.join(".willow-edits").exists());
        let applied = f.run(&["rename", "helper::value", "answer"], 0);
        assert_eq!(applied["result"]["files_changed"], 2);
        assert_eq!(applied["result"]["validation"], "passed");
        assert!(applied["result"].get("transaction").is_none());
        assert!(
            fs::read_to_string(f.0.join("helper.wi"))
                .unwrap()
                .contains("fn answer")
        );
        f.run(&["refs", "helper::answer"], 0);
        let main = fs::read_to_string(f.0.join("main.wi")).unwrap();
        assert!(main.contains(if import.contains("alias") {
            "alias()"
        } else {
            "answer()"
        }));
    }
}

#[test]
fn rename_rejections_preserve_sources_and_report_collision_location() {
    for (selector, name) in [
        ("missing", "new_name"),
        ("value", "answer"),
        ("value", "if"),
        ("value", "value"),
    ] {
        let source = "fn value() -> i64 { return 1; } fn answer() {} fn main() { value(); }";
        let f = Fixture::new(source);
        let result = f.run(&["rename", selector, name], 1);
        assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
        if name == "answer" {
            assert_eq!(result["location"]["path"], "main.wi");
            assert_eq!(result["location"]["line"], 1);
            assert!(result["location"]["column"].as_u64().unwrap() > 1);
        }
    }
    let f = Fixture::new("import helper; fn value() {} fn main() { helper::value(); value(); }");
    f.write("helper.wi", "pub fn value() {}");
    assert_eq!(
        f.run(&["rename", "value", "answer"], 1)["status"],
        "ambiguous"
    );
    assert!(!f.0.join(".willow-edits").exists());
}

#[test]
fn binding_capture_and_validation_failure_preserve_source() {
    for (source, selector, name) in [
        (
            "fn main() { let local = 1; let other = 2; println(local + other); }",
            "local",
            "other",
        ),
        (
            "fn value() -> i64 { return 1; } fn main() { let answer = 3; println(value()); }",
            "value",
            "answer",
        ),
    ] {
        let f = Fixture::new(source);
        let result = f.run(&["rename", selector, name], 1);
        assert_eq!(result["status"], "error");
        assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
        assert!(!f.0.join(".willow-edits/active").exists());
    }
}

#[test]
fn source_race_after_resolution_is_rejected_before_application() {
    use willow_compiler::{
        CompilerOptions, CompilerSession,
        ai::direct::{DirectSession, SelectorFilter},
        diagnostics::HumanEmitter,
    };
    let f = Fixture::new("fn value() {} fn main() { value(); }");
    let entry = f.0.join("main.wi");
    let snapshot =
        CompilerSession::new(entry.to_str().unwrap(), "", &CompilerOptions::debug(), None)
            .analysis_for_edit_with_emitter(&mut HumanEmitter)
            .unwrap();
    let session = DirectSession::new(snapshot).unwrap();
    let edited = "fn value() {} fn main() { value(); value(); }";
    f.write("main.wi", edited);
    assert!(
        session
            .rename(
                &entry,
                false,
                "value",
                &SelectorFilter::default(),
                "answer",
                false
            )
            .is_err()
    );
    assert_eq!(fs::read_to_string(entry).unwrap(), edited);
}

#[test]
fn configuration_changes_after_analysis_reject_preview_and_apply() {
    use willow_compiler::{
        CompilerOptions, CompilerSession,
        ai::direct::{DirectSession, SelectorFilter},
        diagnostics::HumanEmitter,
    };
    for file in ["project.toml", "project.lock"] {
        for dry_run in [false, true] {
            let source = "fn value() {} fn main() { value(); }";
            let f = Fixture::new(source);
            let entry = f.0.join("main.wi");
            let snapshot =
                CompilerSession::new(entry.to_str().unwrap(), "", &CompilerOptions::debug(), None)
                    .analysis_for_edit_with_emitter(&mut HumanEmitter)
                    .unwrap();
            f.write(file, "changed configuration");
            let error = DirectSession::new(snapshot)
                .unwrap()
                .rename(
                    &entry,
                    false,
                    "value",
                    &SelectorFilter::default(),
                    "answer",
                    dry_run,
                )
                .unwrap_err();
            assert!(
                error.to_string().contains("stale analysis configuration"),
                "{error:#}"
            );
            assert_eq!(fs::read_to_string(entry).unwrap(), source);
        }
    }
}

#[test]
fn method_dispatch_family_rename_updates_both_implementations() {
    let f = Fixture::new(
        "open class Base { pub open fn value(self) -> i64 { return 1; } } class Child extends Base { pub override fn value(self) -> i64 { return 2; } } fn call(x: Base) -> i64 { return x.value(); } fn main() { println(call(new Child())); }",
    );
    let result = f.run(&["rename", "main::Base::value", "answer"], 0);
    assert_eq!(result["result"]["files_changed"], 1);
    assert_eq!(result["result"]["references_updated"], 1);
    let text = fs::read_to_string(f.0.join("main.wi")).unwrap();
    assert_eq!(text.matches("fn answer").count(), 2);
    assert!(text.contains("x.answer()"));
}

#[test]
fn project_rename_dry_run_and_failure_never_publish_a_lock() {
    for name in ["answer", "main"] {
        let source = "fn value() {} fn main() { value(); }";
        let f = Fixture::new(source);
        fs::create_dir(f.0.join("src")).unwrap();
        f.write("project.toml", "[willow]\nmanifest-version=1\n[project]\nname='demo'\nversion='0.1.0'\nentry='main.wi'\n");
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&f.0)
            .args(["rename", "main::value", name, "--dry-run"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            name == "answer",
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!f.0.join("project.lock").exists());
        assert!(!f.0.join(".willow-edits").exists());
        assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
    }
}

#[test]
fn direct_rename_rejects_entry_that_disagrees_with_current_manifest() {
    use willow_compiler::{
        CompilerOptions, CompilerSession,
        ai::direct::{DirectSession, SelectorFilter},
        diagnostics::HumanEmitter,
    };
    let f = Fixture::new("fn value() {} fn main() { value(); }");
    f.write("alternate.wi", "fn main() {}");
    f.write(
        "project.toml",
        "[project]\nname='demo'\nversion='0.1.0'\nentry='alternate.wi'\n",
    );
    let entry = f.0.join("main.wi");
    let snapshot = CompilerSession::new(
        entry.to_str().unwrap(),
        "",
        &CompilerOptions::debug(),
        Some(f.0.clone()),
    )
    .analysis_for_edit_with_emitter(&mut HumanEmitter)
    .unwrap();
    let error = DirectSession::new(snapshot)
        .unwrap()
        .rename(
            &entry,
            true,
            "value",
            &SelectorFilter::default(),
            "answer",
            true,
        )
        .unwrap_err();
    assert!(error.to_string().contains("entry differs"), "{error:#}");
    assert!(!f.0.join("project.lock").exists());
    // The ordinary analysis API still honors an explicitly selected alternate
    // source; manifest-entry consistency is required by direct project selection.
    CompilerSession::new(
        entry.to_str().unwrap(),
        "",
        &CompilerOptions::debug(),
        Some(f.0.clone()),
    )
    .analysis_with_emitter(&mut HumanEmitter)
    .unwrap();
}

#[test]
fn semantic_member_rename_covers_declarations_references_and_dispatch() {
    let cases = [
        (
            "field reads",
            "class A { pub old: i64; pub init(self) { self.old = 1; } pub fn get(self) -> i64 { return self.old; } } fn main() {}",
            "main::A::old",
        ),
        (
            "field writes",
            "class A { pub old: i64; pub init(self) { self.old = 1; } } fn main() { let a = new A(); a.old = 2; println(a.old); }",
            "main::A::old",
        ),
        (
            "field unrelated owner",
            "class A { pub old: i64; pub init(self) { self.old = 1; } } class B { pub old: i64; pub init(self) { self.old = 2; } } fn main() { let a = new A(); let b = new B(); println(a.old + b.old); }",
            "main::A::old",
        ),
        (
            "field same-name binding",
            "class A { pub old: i64; pub init(self) { self.old = 1; } } fn main() { let old = 2; let a = new A(); println(a.old + old); }",
            "main::A::old",
        ),
        (
            "inherited field",
            "open class A { pub old: i64; pub init(self) { self.old = 1; } } class B extends A {} fn read(b: B) -> i64 { return b.old; } fn main() {}",
            "main::A::old",
        ),
        (
            "variant value",
            "enum E { Old, Other } fn main() { let e = E::Old; }",
            "main::E::Old",
        ),
        (
            "variant payload",
            "enum E { Old(i64), Other } fn main() { let e = E::Old(1); }",
            "main::E::Old",
        ),
        (
            "variant pattern",
            "enum E { Old(i64), Other } fn main() { let e = E::Old(1); match e { E::Old(v) => { println(v); }, E::Other => {} }; }",
            "main::E::Old",
        ),
        (
            "variant bare pattern",
            "enum E { Old(i64), Other } fn main() { let e = E::Old(1); match e { Old(v) => { println(v); }, Other => {} }; }",
            "main::E::Old",
        ),
        (
            "variant unrelated owner",
            "enum E { Old, Other } enum F { Old, Other } fn main() { let e = E::Old; let f = F::Old; }",
            "main::E::Old",
        ),
        (
            "variant same as type",
            "enum Old { Old, Other } fn main() { let e = Old::Old; }",
            "main::Old::Old",
        ),
        (
            "generic variant",
            "enum E<T> { Old(T), Other } fn main() { let e = E<i64>::Old(1); }",
            "main::E::Old",
        ),
        (
            "interface contract",
            "interface I { fn old(self) -> i64; } class A implements I { pub fn old(self) -> i64 { return 1; } } fn call(x: I) -> i64 { return x.old(); } fn main() {}",
            "main::I::old",
        ),
        (
            "implementation entry",
            "interface I { fn old(self) -> i64; } class A implements I { pub fn old(self) -> i64 { return 1; } } fn call(x: I) -> i64 { return x.old(); } fn main() {}",
            "main::A::old",
        ),
        (
            "multiple implementations",
            "interface I { fn old(self) -> i64; } class A implements I { pub fn old(self) -> i64 { return 1; } } class B implements I { pub fn old(self) -> i64 { return 2; } } fn call(x: I) -> i64 { return x.old(); } fn main() {}",
            "main::A::old",
        ),
        (
            "unrelated method",
            "interface I { fn old(self) -> i64; } class A implements I { pub fn old(self) -> i64 { return 1; } } class B { pub fn old(self) -> i64 { return 2; } } fn call(x: I, y: B) -> i64 { return x.old() + y.old(); } fn main() {}",
            "main::I::old",
        ),
        (
            "generic interface",
            "interface I<T> { fn old(self, x: T) -> T; } class A implements I<i64> { pub fn old(self, x: i64) -> i64 { return x; } } fn call(x: I<i64>) -> i64 { return x.old(1); } fn main() {}",
            "main::I::old",
        ),
        (
            "interface inheritance",
            "interface I { fn old(self) -> i64; } interface J extends I {} class A implements J { pub fn old(self) -> i64 { return 1; } } fn call(x: J) -> i64 { return x.old(); } fn main() {}",
            "main::I::old",
        ),
        (
            "multiple contracts",
            "interface I { fn old(self) -> i64; } interface J { fn old(self) -> i64; } class A implements I, J { pub fn old(self) -> i64 { return 1; } } fn main() {}",
            "main::I::old",
        ),
        (
            "inherited implementation",
            "interface I { fn old(self) -> i64; } open class A { pub open fn old(self) -> i64 { return 1; } } class B extends A implements I {} fn call(x: I) -> i64 { return x.old(); } fn main() {}",
            "main::I::old",
        ),
        (
            "override family without calls",
            "open class A { pub open fn old(self) -> i64 { return 1; } } class B extends A { pub override fn old(self) -> i64 { return 2; } } fn main() {}",
            "main::B::old",
        ),
    ];
    for (label, source, selector) in cases {
        let f = Fixture::new(source);
        eprintln!("rename perspective: {label}");
        let new_name = if selector.rsplit("::").next().unwrap().starts_with('O') {
            "Renamed"
        } else {
            "renamed"
        };
        let preview = f.run(&["rename", selector, new_name, "--dry-run"], 0);
        assert!(
            preview["result"]["changes"]
                .as_array()
                .is_some_and(|c| !c.is_empty()),
            "{label}: {preview}"
        );
        assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
        assert!(!f.0.join(".willow-edits").exists());
        let applied = f.run(&["rename", selector, new_name], 0);
        assert_eq!(applied["result"]["validation"], "passed", "{label}");
        let after = fs::read_to_string(f.0.join("main.wi")).unwrap();
        assert!(after.contains(new_name), "{label}");
        if label == "unrelated method" {
            assert!(after.contains("y.old()"));
        }
        if label == "field unrelated owner" {
            assert!(after.contains("b.old"));
        }
        if label == "variant unrelated owner" {
            assert!(after.contains("F::Old"));
        }
        if label == "variant same as type" {
            assert!(after.contains("Old::Renamed"));
        }
    }
}

#[test]
fn member_rename_across_modules_and_aliases() {
    for (main, model, selector, name) in [
        (
            "import model; fn main() { let a = new model::A(); println(a.old); }",
            "pub class A { pub old: i64; pub init(self) { self.old = 1; } }",
            "model::A::old",
            "updated",
        ),
        (
            "import model::A as Item; fn main() { let a = new Item(); println(a.old); }",
            "pub class A { pub old: i64; pub init(self) { self.old = 1; } }",
            "model::A::old",
            "updated",
        ),
        (
            "import model; fn main() { let x = model::E::Old; }",
            "pub enum E { Old, Other }",
            "model::E::Old",
            "Updated",
        ),
        (
            "import model::E as Choice; fn main() { let x = Choice::Old(1); match x { Choice::Old(v) => { println(v); }, Choice::Other => {} }; }",
            "pub enum E { Old(i64), Other }",
            "model::E::Old",
            "Updated",
        ),
    ] {
        let f = Fixture::new(main);
        f.write("model.wi", model);
        let result = f.run(&["rename", selector, name], 0);
        assert_eq!(result["result"]["files_changed"], 2);
        assert!(
            fs::read_to_string(f.0.join("model.wi"))
                .unwrap()
                .contains(name)
        );
        assert!(
            fs::read_to_string(f.0.join("main.wi"))
                .unwrap()
                .contains(name)
        );
    }
    for selector in ["api::Search::run", "search::BestFirst::run"] {
        let f = Fixture::new(
            "import api; import search; fn call(x: api::Search) -> i64 { return x.run(); } fn main() { let x = new search::BestFirst(); println(call(x)); println(x.run()); }",
        );
        f.write("api.wi", "pub interface Search { fn run(self) -> i64; }");
        f.write("search.wi", "import api::Search as Contract; pub class BestFirst implements Contract { pub fn run(self) -> i64 { return 1; } }");
        let result = f.run(&["rename", selector, "execute"], 0);
        assert_eq!(result["result"]["files_changed"], 3);
        for path in ["main.wi", "api.wi", "search.wi"] {
            let after = fs::read_to_string(f.0.join(path)).unwrap();
            assert!(after.contains("execute"), "{path}: {after}");
            assert!(!after.contains("run("), "{path}: {after}");
        }
    }
}

#[test]
fn semantic_member_rename_failures_are_atomic() {
    for (source, selector, name) in [
        (
            "class A { pub old: i64; pub other: i64; pub init(self) { self.old = 1; self.other = 2; } } fn main() {}",
            "main::A::old",
            "other",
        ),
        (
            "enum E { Old, Other } fn main() { let e = E::Old; }",
            "main::E::Old",
            "Other",
        ),
        (
            "interface I { fn old(self) -> i64; fn other(self) -> i64; } class A implements I { pub fn old(self) -> i64 { return 1; } pub fn other(self) -> i64 { return 2; } } fn main() {}",
            "main::I::old",
            "other",
        ),
        (
            "class A { pub old: i64; pub init(self) { self.old = 1; } } fn main() {}",
            "main::A::old",
            "if",
        ),
        (
            "enum E { Old, Other } fn main() { let e = E::Old; }",
            "main::E::Old",
            "lowercase",
        ),
    ] {
        let f = Fixture::new(source);
        f.run(&["rename", selector, name], 1);
        assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
        assert!(!f.0.join(".willow-edits/active").exists());
    }
}

#[test]
fn static_members_and_constructor_parameter_shadowing() {
    for (source, selector) in [
        (
            "class A { pub static mut old: i64 = 1; } fn main() { A::old = 2; println(A::old); }",
            "main::A::old",
        ),
        (
            "class A { pub static fn old(x: i64) -> i64 { return x; } } fn main() { println(A::old(1)); }",
            "main::A::old",
        ),
        (
            "class A { pub old: i64; pub init(self, old: i64) { self.old = old; } } fn main() { let a = new A(1); println(a.old); }",
            "main::A::old",
        ),
    ] {
        let f = Fixture::new(source);
        f.run(&["rename", selector, "updated"], 0);
        let after = fs::read_to_string(f.0.join("main.wi")).unwrap();
        assert!(after.contains("updated"));
        if source.contains("init(self, old") {
            assert!(after.contains("self.updated = old"));
        }
    }
}

#[test]
fn default_and_nested_method_renames() {
    let cases = [
        (
            "default interface method",
            "interface I { fn old(self) -> i64 { return 1; } } class A implements I {} class B implements I {} fn call(x: I) -> i64 { return x.old(); } fn main() {}",
            "main::I::old",
        ),
        (
            "nested method calls",
            "class A { pub fn old(self) -> A { return self; } } fn call(x: A) -> A { return x.old().old(); } fn main() {}",
            "main::A::old",
        ),
    ];
    for (label, source, selector) in cases {
        let f = Fixture::new(source);
        eprintln!("{label}");
        f.run(&["rename", selector, "renamed", "--dry-run"], 0);
        f.run(&["rename", selector, "renamed"], 0);
        assert!(
            !fs::read_to_string(f.0.join("main.wi"))
                .unwrap()
                .contains("old(")
        );
    }
}

#[test]
fn project_default_contract_uses_source_owner_selector() {
    let f = Fixture::new(
        "import api; import implementation; fn call(x: api::I) -> i64 { return x.old(); } fn main() { let a = new implementation::A(); println(call(a)); println(a.old()); }",
    );
    f.write("project.toml", "[willow]\nmanifest-version=1\n[project]\nname='rename_test'\nversion='0.1.0'\nentry='src/main.wi'\n");
    f.write(
        "api.wi",
        "pub interface I { fn old(self) -> i64 { return 7; } }",
    );
    f.write(
        "implementation.wi",
        "import api; pub class A implements api::I {}",
    );
    fs::create_dir(f.0.join("src")).unwrap();
    for file in ["main.wi", "api.wi", "implementation.wi"] {
        fs::rename(f.0.join(file), f.0.join("src").join(file)).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(&f.0)
        .args(["rename", "api::I::old", "renamed", "--format=json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(f.0.join("src/api.wi"))
            .unwrap()
            .contains("fn renamed")
    );
    assert!(
        fs::read_to_string(f.0.join("src/main.wi"))
            .unwrap()
            .contains("a.renamed()")
    );
}

#[test]
fn member_rename_ignores_module_prefixes_and_unrelated_destinations() {
    for (name, extra) in [
        ("count", "class Pair { pub count: i64; }"),
        ("goals", "fn goals() -> i64 { return 0; }"),
        ("n", "fn unrelated() { let n = 1; println(n); }"),
        ("value", "fn unrelated(value: i64) { println(value); }"),
        ("Rating", "class Rating {}"),
        ("delta", ""),
    ] {
        for import in ["import elo::delta;", "import elo;"] {
            let source = format!(
                "{import} import team; {extra} fn main() {{ let t = new team::Team(2); println(t.elo); }}"
            );
            let f = Fixture::new(&source);
            f.write("elo.wi", "pub fn delta() -> i64 { return 1; }");
            f.write("team.wi", "pub class Team { pub elo: i64; }");
            let preview = f.run(&["rename", "team::Team::elo", name, "--dry-run"], 0);
            assert_eq!(preview["status"], "ok");
            assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
            f.run(&["rename", "team::Team::elo", name], 0);
            let after = fs::read_to_string(f.0.join("main.wi")).unwrap();
            assert!(after.contains(import), "{after}");
            assert!(after.contains(extra), "{after}");
            assert!(after.contains(&format!("t.{name}")), "{after}");
        }
    }
}

#[test]
fn member_rename_destination_respects_hierarchy_and_dispatch_scopes() {
    for (source, selector, reject) in [
        (
            "interface I { fn old(self) {} } class A implements I { pub fn next(self) {} } fn main() {}",
            "main::I::old",
            true,
        ),
        (
            "interface I { fn next(self); } interface J extends I { fn old(self); } fn main() {}",
            "main::J::old",
            true,
        ),
        (
            "open class A { pub old: i64; } class B extends A { pub next: i64; } fn main() {}",
            "main::A::old",
            true,
        ),
        (
            "open class A { pub next: i64; } class B extends A { pub old: i64; } fn main() {}",
            "main::B::old",
            true,
        ),
        (
            "open class Root {} class A extends Root { pub old: i64; } class B extends Root { pub next: i64; } fn main() {}",
            "main::A::old",
            false,
        ),
        (
            "interface I { fn old(self); fn next(self); } class A implements I { pub fn old(self) {} pub fn next(self) {} } fn main() {}",
            "main::I::old",
            true,
        ),
        (
            "class A { pub fn old(self) {} } class B { pub fn next(self) {} } fn main() {}",
            "main::A::old",
            false,
        ),
    ] {
        let f = Fixture::new(source);
        for dry in [true, false] {
            let mut args = vec!["rename", selector, "next"];
            if dry {
                args.push("--dry-run");
            }
            let result = f.run(&args, i32::from(reject));
            if reject {
                assert_eq!(result["location"]["path"], "main.wi", "{result}");
            }
            if reject || dry {
                assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
            }
        }
    }
}

#[test]
fn local_binding_rename_preserves_identity_and_previews() {
    for (label, source, selector, expected) in [
        (
            "closure capture",
            "fn main() { let old = 1; let f = |x: i64| { return x + old; }; println(f(2)); }",
            "old",
            "x + renamed",
        ),
        (
            "lambda parameter",
            "fn main() { let f = |old: i64| { return old + 1; }; println(f(2)); }",
            "old",
            "renamed + 1",
        ),
        (
            "match binding",
            "enum E { Some(i64), None } fn main() { let e = E::Some(1); match e { E::Some(old) => { println(old); }, E::None => {} }; }",
            "old",
            "println(renamed)",
        ),
        (
            "simple",
            "fn main() { let old = 1; println(old); }",
            "old",
            "println(renamed)",
        ),
        (
            "assignment",
            "fn main() { let mut old = 1; old = 2; println(old); }",
            "old",
            "renamed = 2",
        ),
        (
            "parameter",
            "fn f(old: i64) -> i64 { return old; } fn main() {}",
            "old",
            "return renamed",
        ),
        (
            "nested use",
            "fn main() { let old = 1; if true { println(old); } }",
            "old",
            "println(renamed)",
        ),
        (
            "unrelated destination",
            "fn other() { let renamed = 3; } fn main() { let old = 1; println(old); }",
            "old",
            "println(renamed)",
        ),
        (
            "same spelling field",
            "class A { pub old: i64; } fn main() { let old = 1; let a = new A(old); println(a.old); }",
            "main::main::old",
            "a.old",
        ),
        ("unused", "fn main() { let old = 1; }", "old", "let renamed"),
        (
            "string untouched",
            "fn main() { let old = 1; println(\"old\"); println(old); }",
            "old",
            "\"old\"",
        ),
        (
            "shadowed binding",
            "fn main() { let old = 1; if true { let old = 2; println(old); } println(old); }",
            "main.wi:1:17",
            "let old = 2; println(old)",
        ),
    ] {
        let f = Fixture::new(source);
        eprintln!("local rename: {label}");
        f.run(&["rename", selector, "renamed", "--dry-run"], 0);
        assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
        f.run(&["rename", selector, "renamed"], 0);
        let after = fs::read_to_string(f.0.join("main.wi")).unwrap();
        assert!(after.contains(expected), "{label}: {after}");
        assert!(after.contains("renamed"), "{label}: {after}");
    }
}

#[test]
fn rename_keywords_name_the_reserved_word_for_all_target_kinds() {
    for (source, selector) in [
        ("fn old() {} fn main() { old(); }", "old"),
        ("class A { pub old: i64; } fn main() {}", "main::A::old"),
        ("fn main() { let old = 1; println(old); }", "old"),
    ] {
        for keyword in ["open", "if", "class", "return", "i64"] {
            let f = Fixture::new(source);
            let result = f.run(&["rename", selector, keyword], 1);
            assert!(
                result["message"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("'{keyword}' is a reserved keyword")),
                "{result}"
            );
            assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
        }
    }
}

const BUILTIN_NAMED_VEC: &str = "pub class Vec3 {
    pub x: f64;
    pub fn len(self) -> f64 { return self.x; }
    pub fn add(self, o: Vec3) -> Vec3 { return new Vec3(self.x + o.x); }
    pub fn unwrap(self) -> f64 { return self.x; }
    pub fn toString(self) -> String { return \"v\" + self.x.toString(); }
}
pub interface Sized { fn len(self) -> f64; }
pub class Box implements Sized { pub n: f64; pub fn len(self) -> f64 { return self.n; } }
pub fn min(a: f64, b: f64) -> f64 { if a < b { return a; } return b; }
pub fn max(a: f64, b: f64) -> f64 { if a > b { return a; } return b; }
";
const BUILTIN_NAMED_MAIN: &str = "import vec::{Vec3, Sized, Box, max, min};
import std::collections::{Array, Map};
fn size(s: Sized) -> f64 { return s.len(); }
fn main() {
    let items: Array<i64> = [1, 2, 3];
    let frozen = items.freeze();
    let words: Map<String, i64> = Map::new();
    words.insert(\"a\", 1);
    let s = \"abc\";
    let o: Option<i64> = Some(4);
    let rays = AtomicI64::new(0);
    rays.add(items.len() + frozen.len() + words.len() + s.len() + o.unwrap());
    let v = new Vec3(1.0).add(new Vec3(2.0));
    println(v.len() + v.unwrap() + size(new Box(2.0)) + max(1.0, 2.0) + min(1.0, 2.0));
    println(v.toString() + items.len().toString() + s.toString());
    println(rays.load());
}
";

/// Builtin member calls (Array/FrozenArray/Map/String/Option/AtomicI64 and
/// primitive `toString`) are proven non-targets, so user methods sharing their
/// spelling rename without touching builtin calls and the program still runs.
#[test]
fn user_methods_named_like_builtins_rename_without_touching_builtin_calls() {
    for (selector, old, new, edits) in [
        ("vec::Vec3::len", "len", "length", 2),
        ("vec::Vec3::add", "add", "plus", 2),
        ("vec::Vec3::unwrap", "unwrap", "value", 2),
        ("vec::Vec3::toString", "toString", "show", 2),
        ("vec::Sized::len", "len", "measure", 3),
        ("vec::Box::len", "len", "measure", 3),
    ] {
        let f = Fixture::new(BUILTIN_NAMED_MAIN);
        f.write("vec.wi", BUILTIN_NAMED_VEC);
        let preview = f.run(&["rename", selector, new, "--dry-run"], 0);
        assert_eq!(preview["result"]["edits"], edits, "{selector}: {preview}");
        assert_eq!(
            fs::read_to_string(f.0.join("main.wi")).unwrap(),
            BUILTIN_NAMED_MAIN
        );
        let applied = f.run(&["rename", selector, new], 0);
        assert_eq!(applied["result"]["edits"], edits, "{selector}");
        assert_eq!(applied["result"]["validation"], "passed");
        let main = fs::read_to_string(f.0.join("main.wi")).unwrap();
        // Every builtin call keeps its spelling.
        for builtin in [
            "items.len()",
            "frozen.len()",
            "words.len()",
            "s.len()",
            "o.unwrap()",
            "rays.add(",
            "items.len().toString()",
            "s.toString()",
        ] {
            assert!(
                main.contains(builtin),
                "{selector}: {builtin} changed\n{main}"
            );
        }
        let changed = main.matches(&format!(".{new}(")).count()
            + fs::read_to_string(f.0.join("vec.wi"))
                .unwrap()
                .matches(&format!("fn {new}("))
                .count();
        assert_eq!(changed, edits, "{selector} {old}->{new}\n{main}");
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&f.0)
            .args(["run", "main.wi"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "11\nv33abc\n14\n",
            "{selector}"
        );
    }
}

#[test]
fn rename_collision_cites_conflicting_declaration_not_import() {
    let f = Fixture::new(BUILTIN_NAMED_MAIN);
    f.write("vec.wi", BUILTIN_NAMED_VEC);
    let result = f.run(&["rename", "vec::min", "max"], 1);
    assert_eq!(
        result["message"],
        "rename destination conflicts with an existing declaration at vec.wi:11:8"
    );
    assert_eq!(result["location"]["path"], "vec.wi");
    assert_eq!(
        (&result["location"]["line"], &result["location"]["column"]),
        (&11.into(), &8.into())
    );
    // A destination spelled only by builtin calls has no declaration to cite.
    let result = f.run(&["rename", "vec::min", "load"], 1);
    assert_eq!(
        result["message"],
        "rename destination already occurs in workspace at main.wi:16:18"
    );
    assert_eq!(
        fs::read_to_string(f.0.join("main.wi")).unwrap(),
        BUILTIN_NAMED_MAIN
    );
    assert_eq!(
        fs::read_to_string(f.0.join("vec.wi")).unwrap(),
        BUILTIN_NAMED_VEC
    );
}

#[test]
fn rename_dry_run_reports_counts_with_singular_and_plural_nouns() {
    let f = Fixture::new("fn value() -> i64 { return 1; } fn main() { println(value()); }");
    let preview = f.run(&["rename", "value", "answer", "--dry-run"], 0);
    for (key, expected) in [
        ("files_changed", 1),
        ("edits", 2),
        ("declarations_updated", 1),
        ("references_updated", 1),
    ] {
        assert_eq!(preview["result"][key], expected, "{key}");
    }
    let text = f.human(&["rename", "value", "answer", "--dry-run"], 0);
    assert!(text.contains("-fn value()"), "{text}");
    assert!(
        text.ends_with(
            "Dry run: rename value -> answer would change 1 file\n2 edits (1 declaration, 1 reference)\nNo files changed\n"
        ),
        "{text}"
    );
    let text = f.human(&["rename", "value", "answer"], 0);
    assert!(
        text.starts_with("Renamed value -> answer\n1 file changed\n2 edits (1 declaration, 1 reference)\nValidation: passed"),
        "{text}"
    );
    let f = Fixture::new(BUILTIN_NAMED_MAIN);
    f.write("vec.wi", BUILTIN_NAMED_VEC);
    let text = f.human(&["rename", "vec::Sized::len", "measure", "--dry-run"], 0);
    assert!(
        text.contains("would change 2 files\n3 edits (2 declarations, 1 reference)\n"),
        "{text}"
    );
}

#[test]
fn free_function_named_like_builtin_method_renames_past_builtin_calls() {
    let source = "import std::collections::Array;\nfn len(x: i64) -> i64 { return x + 1; }\nfn main() { let items: Array<i64> = [1]; println(len(items.len()) + \"ab\".len()); }";
    let f = Fixture::new(source);
    let applied = f.run(&["rename", "len", "size"], 0);
    assert_eq!(applied["result"]["edits"], 2);
    let main = fs::read_to_string(f.0.join("main.wi")).unwrap();
    assert!(main.contains("fn size(x: i64)"), "{main}");
    assert!(
        main.contains("println(size(items.len()) + \"ab\".len())"),
        "{main}"
    );
}

/// Deeply nested builtin calls in arguments, and builtin calls chained on a
/// user-method result, are each classified once without rescanning arguments.
#[test]
fn nested_builtin_arguments_and_chains_are_proven_non_targets() {
    for depth in [1usize, 16, 64] {
        let nested = format!("{}0{}", "rays.add(".repeat(depth), ")".repeat(depth));
        let source = format!(
            "class P {{ pub n: i64; pub fn add(self, x: i64) -> i64 {{ return self.n + x; }} }}\nfn main() {{ let rays = AtomicI64::new(0); let p = new P(1); println(p.add({nested}).toString() + p.add(1).toString().len().toString()); }}"
        );
        let f = Fixture::new(&source);
        let applied = f.run(&["rename", "main::P::add", "plus"], 0);
        assert_eq!(applied["result"]["edits"], 3, "depth={depth}");
        let main = fs::read_to_string(f.0.join("main.wi")).unwrap();
        assert_eq!(main.matches("rays.add(").count(), depth, "{main}");
        assert_eq!(main.matches("p.plus(").count(), 2, "{main}");
    }
}

#[test]
fn referenced_constant_rename_preserves_aliases_and_shadowed_locals() {
    let source = "import helper; import helper::{value, value as v}; fn main() { println(value + helper::value + v); if true { let value = 9; println(value); } }";
    let f = Fixture::new(source);
    f.write("helper.wi", "pub const value: i64 = 7;");
    let preview = f.run(&["rename", "helper::value", "answer", "--dry-run"], 0);
    assert_eq!(preview["result"]["changes"].as_array().unwrap().len(), 2);
    assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
    let applied = f.run(&["rename", "helper::value", "answer"], 0);
    assert_eq!(applied["result"]["validation"], "passed");
    assert_eq!(applied["result"]["files_changed"], 2);
    assert_eq!(
        fs::read_to_string(f.0.join("helper.wi")).unwrap(),
        "pub const answer: i64 = 7;"
    );
    assert_eq!(
        fs::read_to_string(f.0.join("main.wi")).unwrap(),
        "import helper; import helper::{answer, answer as v}; fn main() { println(answer + helper::answer + v); if true { let value = 9; println(value); } }"
    );
    f.run(&["refs", "helper::answer"], 0);
}

#[test]
fn repeated_import_path_and_item_rename_keeps_module_qualifier() {
    let source =
        "import value::value; import value::{value as v}; fn main() { println(value + v); }";
    let f = Fixture::new(source);
    f.write("value.wi", "pub const value: i64 = 7;");
    f.run(&["rename", "value::value", "answer", "--dry-run"], 0);
    assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
    let applied = f.run(&["rename", "value::value", "answer"], 0);
    assert_eq!(applied["result"]["validation"], "passed");
    assert_eq!(
        fs::read_to_string(f.0.join("value.wi")).unwrap(),
        "pub const answer: i64 = 7;"
    );
    assert_eq!(
        fs::read_to_string(f.0.join("main.wi")).unwrap(),
        "import value::answer; import value::{answer as v}; fn main() { println(answer + v); }"
    );
}
