use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::SystemTime,
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-package-commands-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn package(&self, dir: &str, name: &str, version: &str, deps: &str) {
        self.write(&format!("{dir}/project.toml"), &format!("# Package comment\n[project]\nname = '{name}'\nversion = '{version}' # Version comment\n[willow]\nmanifest-version = 1\n{deps}"));
        self.write(&format!("{dir}/src/main.wi"), "fn main() { println(42); }");
    }
    fn cli(&self, args: &[&str]) -> Output {
        self.cli_from("app", args)
    }
    fn cli_from(&self, dir: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_willowc"))
            .current_dir(self.0.join(dir))
            .env("WILLOW_HOME", self.0.join("cache"))
            .args(args)
            .output()
            .unwrap()
    }
    fn git(&self, dir: &str, args: &[&str]) -> String {
        success(
            Command::new("git")
                .current_dir(self.0.join(dir))
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "tag.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap(),
        )
    }
    fn remote(&self, dir: &str, name: &str) -> String {
        self.package(dir, name, "1.0.0", "");
        self.git(dir, &["init", "--initial-branch=main", "--template="]);
        self.git(dir, &["config", "user.name", "Fixture"]);
        self.git(dir, &["config", "user.email", "fixture@example.invalid"]);
        self.release(dir, name, "1.0.0");
        format!("file://{}", self.0.join(dir).display())
    }
    fn release(&self, dir: &str, name: &str, version: &str) {
        self.package(dir, name, version, "");
        self.git(dir, &["add", "project.toml", "src/main.wi"]);
        self.git(dir, &["commit", "-m", version]);
        self.git(dir, &["tag", &format!("v{version}")]);
    }
    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.0.join(path)).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn failure(output: Output, expected: &str) {
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains(expected), "{text}");
}
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (SystemTime, Option<Vec<u8>>)> {
    let mut result = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::metadata(&path).unwrap();
        let bytes = if metadata.is_file() {
            Some(fs::read(&path).unwrap())
        } else {
            pending.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            None
        };
        result.insert(path, (metadata.modified().unwrap(), bytes));
    }
    result
}

#[test]
fn path_add_remove_round_trips_comments_tables_and_dry_run_is_read_only() {
    let f = Fixture::new();
    f.package("lib", "my-library", "1.0.0", "");
    for deps in [
        "",
        "\n# Dependencies comment\n[dependencies]\n# Trailing comment\n",
        "\n[dependencies]\nbase={path='../lib'} # Keep this\n\n[unrelated]\nfoo = 'bar' # untouched\n",
    ] {
        f.package("app", "app", "1.0.0", deps);
        let before = f.read("app/project.toml");
        let state = snapshot(&f.0);
        assert_eq!(
            success(f.cli(&["add", "--path", "../lib", "--dry-run"])),
            "Would Add my_library = { path = \"../lib\" }\nWould resolve 1 dependency packages\n"
        );
        assert_eq!(snapshot(&f.0), state);
        assert_eq!(
            success(f.cli(&["add", "--path", "../lib"])),
            "Add my_library = { path = \"../lib\" }\nResolved 1 dependency packages\n"
        );
        let state = snapshot(&f.0);
        assert!(
            success(f.cli(&["remove", "my_library", "--dry-run"]))
                .starts_with("Would Remove my_library\n")
        );
        assert_eq!(snapshot(&f.0), state);
        assert!(
            success(f.cli(&["update", "--dry-run"])).starts_with("Would Update all dependencies\n")
        );
        assert_eq!(snapshot(&f.0), state);
        assert!(success(f.cli(&["remove", "my_library"])).starts_with("Remove my_library\n"));
        assert_eq!(f.read("app/project.toml"), before);
    }
}

#[test]
fn tree_why_all_diamond_paths_and_remove_prunes_lock() {
    let f = Fixture::new();
    f.package("common", "shared", "1.0.0", "");
    f.package(
        "left",
        "left",
        "1.0.0",
        "[dependencies]\nc={path='../common'}\n",
    );
    f.package(
        "right",
        "right",
        "1.0.0",
        "[dependencies]\nc={path='../common'}\n",
    );
    f.package(
        "app",
        "app",
        "1.0.0",
        "[dependencies]\nl={path='../left'}\nr={path='../right'}\n",
    );
    assert_eq!(
        success(f.cli(&["deps", "why", "shared"])),
        "app v1.0.0 -> l: left v1.0.0 -> c: shared v1.0.0\napp v1.0.0 -> r: right v1.0.0 -> c: shared v1.0.0\n"
    );
    assert_eq!(
        success(f.cli(&["deps", "tree"])),
        "app v1.0.0\n  l: left v1.0.0\n    c: shared v1.0.0\n  r: right v1.0.0\n    c: shared v1.0.0 (*)\n"
    );
    assert!(!f.0.join("app/project.lock").exists());
    failure(f.cli(&["deps", "why", "missing"]), "not found");
    success(f.cli(&["remove", "l"]));
    assert!(!f.read("app/project.lock").contains("name = \"left\""));
    assert!(f.read("app/project.lock").contains("name = \"shared\""));
    success(f.cli(&["remove", "r"]));
    assert!(!f.read("app/project.lock").contains("name = \"shared\""));
}

#[test]
fn git_shorthand_stable_update_bounds_breaking_and_cache_preservation() {
    let f = Fixture::new();
    let url = f.remote("remote", "1-web.client");
    f.release("remote", "1-web.client", "1.1.0");
    f.release("remote", "1-web.client", "2.0.0-beta.1");
    f.package("app", "app", "1.0.0", "");
    // Cold-cache dry-run fails without creating even a cache lock or directory.
    let state = snapshot(&f.0);
    failure(f.cli(&["add", &url, "--dry-run"]), "cache_missing_offline");
    assert_eq!(snapshot(&f.0), state);
    let original = f.read("app/project.toml");
    let output = success(f.cli(&["add", &url]));
    assert!(output.contains("Add _1_web_client"));
    assert!(output.contains("version = \"^1.1.0\""));
    let added = f.read("app/project.toml");
    assert!(!added.contains("branch"));
    f.release("remote", "1-web.client", "1.2.0");
    f.release("remote", "1-web.client", "2.0.0");
    assert_eq!(
        success(f.cli(&["update"])),
        "Update all dependencies\nResolved 1 dependency packages\n"
    );
    assert_eq!(f.read("app/project.toml"), added);
    assert!(f.read("app/project.lock").contains("version = \"1.2.0\""));
    // Seed the exact candidate tree using a separate consumer. Preview must
    // neither fetch nor materialize missing Git trees into the shared cache.
    f.package("preview_seed", "seed", "1.0.0", "");
    success(f.cli_from(
        "preview_seed",
        &["add", "candidate", "--git", &url, "--version", "2"],
    ));
    let state = snapshot(&f.0);
    let preview = success(f.cli(&["update", "_1_web_client", "--breaking", "--dry-run"]));
    assert!(preview.contains("Would Requirement _1_web_client: ^1.1.0 -> ^2.0.0"));
    assert_eq!(snapshot(&f.0), state);
    let output = success(f.cli(&["update", "_1_web_client", "--breaking"]));
    assert!(output.starts_with("Requirement _1_web_client: ^1.1.0 -> ^2.0.0\n"));
    assert!(f.read("app/project.lock").contains("version = \"2.0.0\""));
    let cache = snapshot(&f.0.join("cache"));
    success(f.cli(&["remove", "_1_web_client"]));
    assert_eq!(snapshot(&f.0.join("cache")), cache);
    assert_eq!(f.read("app/project.toml"), original);
    let state = snapshot(&f.0);
    assert!(success(f.cli(&["add", &url, "--dry-run"])).contains("version = \"^2.0.0\""));
    assert_eq!(snapshot(&f.0), state);
}

#[test]
fn targeted_update_keeps_other_branches_pinned_and_regular_fetch_never_follows() {
    let f = Fixture::new();
    let a = f.remote("a", "a");
    let b = f.remote("b", "b");
    f.package(
        "app",
        "app",
        "1.0.0",
        &format!("[dependencies]\na={{git='{a}',branch='main'}}\nb={{git='{b}',branch='main'}}\n"),
    );
    success(f.cli(&["fetch"]));
    let old = f.read("app/project.lock");
    f.release("a", "a", "1.1.0");
    f.release("b", "b", "1.1.0");
    success(f.cli(&["fetch"]));
    assert_eq!(f.read("app/project.lock"), old);
    success(f.cli(&["update", "a"]));
    let lock: toml::Value = toml::from_str(&f.read("app/project.lock")).unwrap();
    let versions: BTreeMap<_, _> = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["name"].as_str().unwrap(), p["version"].as_str().unwrap()))
        .collect();
    assert_eq!(versions["a"], "1.1.0");
    assert_eq!(versions["b"], "1.0.0");
    success(f.cli(&["update"]));
    assert_eq!(
        f.read("app/project.lock")
            .matches("version = \"1.1.0\"")
            .count(),
        2
    );
}

#[test]
fn failed_plan_preserves_manifest_and_lock_and_run_discovers_project_from_children() {
    let f = Fixture::new();
    f.package("app", "app", "1.0.0", "");
    f.package(
        "lib",
        "lib",
        "1.0.0",
        "[dependencies]\nmissing={path='../absent'}\n",
    );
    success(f.cli(&["fetch"]));
    let before = snapshot(&f.0);
    failure(
        f.cli(&["add", "x", "--path", "../lib"]),
        "package_not_found",
    );
    assert_eq!(snapshot(&f.0), before);
    failure(f.cli(&["remove", "absent"]), "unknown dependency");
    assert_eq!(snapshot(&f.0), before);
    f.write("app/src/custom.wi", "fn main() { println(73); }");
    let configured = f
        .read("app/project.toml")
        .replace("[project]", "[project]\nentry = 'src/custom.wi'");
    f.write("app/project.toml", &configured);
    assert_eq!(success(f.cli(&["run", "."])), "73\n");
    assert_eq!(success(f.cli_from("app/src", &["run"])), "73\n");
    assert_eq!(success(f.cli(&["run", "src/main.wi"])), "42\n");
}

#[test]
fn targeted_update_refreshes_reachable_transitives_and_keeps_other_sources() {
    let f = Fixture::new();
    let shared = f.remote("shared", "shared");
    let other = f.remote("other", "other");
    f.package(
        "path",
        "path",
        "1.0.0",
        &format!("[dependencies]\nshared={{git='{shared}',version='1'}}\n"),
    );
    f.package(
        "app",
        "app",
        "1.0.0",
        &format!("[dependencies]\np={{path='../path'}}\no={{git='{other}',version='1'}}\n"),
    );
    success(f.cli(&["fetch"]));
    f.release("shared", "shared", "1.1.0");
    f.release("other", "other", "1.1.0");
    success(f.cli(&["update", "p"]));
    let lock: toml::Value = toml::from_str(&f.read("app/project.lock")).unwrap();
    let versions: BTreeMap<_, _> = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["name"].as_str().unwrap(), p["version"].as_str().unwrap()))
        .collect();
    assert_eq!(versions["shared"], "1.1.0");
    assert_eq!(versions["other"], "1.0.0");
}

#[test]
fn editor_preserves_inline_dotted_and_nested_dependency_forms() {
    let f = Fixture::new();
    f.package("lib", "lib", "1.0.0", "");
    for prefix in [
        "dependencies = { base = {path='../lib'} } # Inline\n",
        "dependencies.base = {path='../lib'} # Dotted\n",
        "",
    ] {
        let suffix = if prefix.is_empty() {
            "[dependencies.base] # Named table\npath = '../lib' # Source comment\n\n[extra]\nanswer=42\n"
        } else {
            ""
        };
        f.package("app", "app", "1.0.0", suffix);
        let original = format!("{prefix}{}", f.read("app/project.toml"));
        f.write("app/project.toml", &original);
        success(f.cli(&["add", "added", "--path=../lib"]));
        success(f.cli(&["remove", "added"]));
        assert_eq!(f.read("app/project.toml"), original);
    }
}

#[test]
fn editor_preserves_line_endings_and_final_newline() {
    let f = Fixture::new();
    f.package("lib", "lib", "1.0.0", "");
    f.package("app", "app", "1.0.0", "");
    let base = f.read("app/project.toml");
    for original in [base.trim_end().to_string(), base.replace('\n', "\r\n")] {
        f.write("app/project.toml", &original);
        success(f.cli(&["add", "added", "--path=../lib"]));
        success(f.cli(&["remove", "added"]));
        assert_eq!(f.read("app/project.toml"), original);
    }
}

#[test]
fn add_resolves_new_transitive_constraints_without_following_locked_branches() {
    let f = Fixture::new();
    let versioned = f.remote("versioned", "versioned");
    let moving = f.remote("moving", "moving");
    f.package("app", "app", "1.0.0", &format!("[dependencies]\nv={{git='{versioned}',version='1'}}\nb={{git='{moving}',branch='main'}}\n"));
    success(f.cli(&["fetch"]));
    f.release("versioned", "versioned", "1.1.0");
    f.release("moving", "moving", "1.1.0");
    f.package(
        "new",
        "new",
        "1.0.0",
        &format!("[dependencies]\nv={{git='{versioned}',version='1.1'}}\n"),
    );
    success(f.cli(&["add", "new", "--path=../new"]));
    let lock: toml::Value = toml::from_str(&f.read("app/project.lock")).unwrap();
    let versions: BTreeMap<_, _> = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["name"].as_str().unwrap(), p["version"].as_str().unwrap()))
        .collect();
    assert_eq!(versions["versioned"], "1.1.0");
    assert_eq!(versions["moving"], "1.0.0");
}
