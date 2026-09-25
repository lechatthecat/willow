use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("willow-git-cli-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let mut stack = vec![self.0.clone()];
        while let Some(path) = stack.pop() {
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() || (cfg!(windows) && metadata.is_file()) {
                let mut permissions = metadata.permissions();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    permissions.set_mode(permissions.mode() | 0o700);
                }
                #[cfg(windows)]
                {
                    // Windows clears the read-only attribute, not Unix access bits.
                    #[allow(clippy::permissions_set_readonly_false)]
                    permissions.set_readonly(false);
                }
                fs::set_permissions(&path, permissions).unwrap();
            }
            if metadata.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    stack.push(entry.unwrap().path());
                }
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn git(path: &Path, args: &[&str]) {
    success(
        Command::new("git")
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
            .unwrap(),
    );
}

#[test]
fn git_project_compiles_runs_reuses_lock_and_ignores_hooks_and_filters() {
    let f = Fixture::new();
    f.write(
        "remote/project.toml",
        "[project]\nname='actual_name'\nversion='1.0.0'\n[willow]\nmanifest-version=1\n[dependencies]\ninner={path='helper'}\n",
    );
    f.write(
        "remote/src/value.wi",
        "module value; import inner::number; pub fn get() -> i64 { return number::get(); }",
    );
    f.write(
        "remote/helper/project.toml",
        "[project]\nname='helper'\nversion='1.0.0'\n[willow]\nmanifest-version=1\n",
    );
    f.write(
        "remote/helper/src/number.wi",
        "module number; pub fn get() -> i64 { return 42; }",
    );
    f.write("remote/.gitattributes", "*.wi filter=probe\n");
    let repo = f.0.join("remote");
    for args in [
        vec!["init", "--initial-branch=main", "--template="],
        vec!["config", "user.name", "Fixture"],
        vec!["config", "user.email", "fixture@example.invalid"],
        vec!["add", "project.toml", "src", "helper", ".gitattributes"],
        vec!["commit", "-m", "fixture"],
        vec!["tag", "1.0.0"],
    ] {
        git(&repo, &args);
    }
    f.write("app/project.toml", &format!("[project]\nname='app'\nversion='1.0.0'\n[willow]\nmanifest-version=1\n[dependencies]\nalias={{git='file://{}',version='1'}}\n", repo.to_string_lossy().replace('\\', "/")));
    f.write(
        "app/src/main.wi",
        "import alias::value; fn main() { println(value::get()); }",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = "#!/bin/sh\necho executed >> \"$WILLOW_GIT_SENTINEL\"\n";
        f.write("hooks/post-checkout", script);
        fs::set_permissions(
            f.0.join("hooks/post-checkout"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        success(
            Command::new("sh")
                .arg(f.0.join("hooks/post-checkout"))
                .env("WILLOW_GIT_SENTINEL", f.0.join("sentinel"))
                .output()
                .unwrap(),
        );
        assert!(f.0.join("sentinel").exists());
        fs::remove_file(f.0.join("sentinel")).unwrap();
    }
    #[cfg(unix)]
    let instrumented_path = {
        use std::os::unix::fs::PermissionsExt;
        let original = std::env::var_os("PATH").unwrap();
        let real_git = std::env::split_paths(&original)
            .map(|p| p.join("git"))
            .find(|p| p.is_file())
            .unwrap();
        let quoted = real_git.to_string_lossy().replace('\'', "'\\''");
        f.write(
            "tools/git",
            &format!(
                "#!/bin/sh\necho \"$*\" >> \"$WILLOW_GIT_OPERATIONS\"\nexec '{quoted}' \"$@\"\n"
            ),
        );
        fs::set_permissions(f.0.join("tools/git"), fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![f.0.join("tools")];
        paths.extend(std::env::split_paths(&original));
        std::env::join_paths(paths).unwrap()
    };
    let app = f.0.join("app");
    let invoke = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_willow"));
        command
            .current_dir(&app)
            .args(args)
            .env("WILLOW_GIT_SENTINEL", f.0.join("sentinel"))
            .env("WILLOW_HOME", f.0.join("cache"))
            .env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_0", f.0.join("hooks"))
            .env("GIT_CONFIG_KEY_1", "filter.probe.smudge")
            .env(
                "GIT_CONFIG_VALUE_1",
                "echo filter >> \"$WILLOW_GIT_SENTINEL\"; cat",
            )
            .env("GIT_TEMPLATE_DIR", f.0.join("hooks"));
        #[cfg(unix)]
        command
            .env("PATH", &instrumented_path)
            .env("WILLOW_GIT_OPERATIONS", f.0.join("operations"));
        success(command.output().unwrap())
    };
    assert_eq!(invoke(&["run", "."]), "42\n");
    let lock = fs::read(app.join("project.lock")).unwrap();
    assert!(String::from_utf8_lossy(&lock).contains("revision"));
    f.write(
        "remote/src/value.wi",
        "module value; pub fn get() -> i64 { return 99; }",
    );
    git(&repo, &["add", "src"]);
    git(&repo, &["commit", "-m", "moved"]);
    git(&repo, &["tag", "-f", "1.0.0"]);
    #[cfg(unix)]
    fs::write(f.0.join("operations"), "").unwrap();
    assert_eq!(invoke(&["run", ".", "--locked"]), "42\n");
    assert_eq!(invoke(&["run", "."]), "42\n");
    assert_eq!(fs::read(app.join("project.lock")).unwrap(), lock);
    assert!(!f.0.join("sentinel").exists(), "Git hook/filter executed");
    #[cfg(unix)]
    assert!(
        !fs::read_to_string(f.0.join("operations"))
            .unwrap()
            .lines()
            .any(|line| line.contains("for-each-ref")
                || line.contains(" fetch ")
                || line.contains(" checkout ")),
        "pinned builds performed fetch, checkout, or version enumeration"
    );
}
