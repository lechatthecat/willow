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
fn unsupported_binding_and_validation_failure_preserve_source() {
    for (source, selector, name) in [
        (
            "fn main() { let local = 1; println(local); }",
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
