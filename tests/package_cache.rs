use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-cache-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, content: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn command(&self, app: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_willow"));
        cmd.current_dir(self.0.join(app))
            .args(args)
            .env("WILLOW_HOME", self.0.join("cache"));
        #[cfg(unix)]
        if self.0.join("tools/git").exists() {
            let mut paths = vec![self.0.join("tools")];
            paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
            cmd.env("PATH", std::env::join_paths(paths).unwrap())
                .env("CACHE_GIT_SENTINEL", self.0.join("unexpected-git"));
        }
        cmd
    }
    fn invoke(&self, app: &str, args: &[&str]) -> Output {
        self.command(app, args).output().unwrap()
    }
}
fn writable(path: &Path) {
    let metadata = fs::symlink_metadata(path).unwrap();
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
    fs::set_permissions(path, permissions).unwrap();
}
impl Drop for Fixture {
    fn drop(&mut self) {
        {
            let mut stack = vec![self.0.clone()];
            while let Some(path) = stack.pop() {
                writable(&path);
                if path.is_dir() {
                    for e in fs::read_dir(&path).unwrap() {
                        stack.push(e.unwrap().path());
                    }
                } else {
                    writable(&path);
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
fn failure(output: Output, kind: &str) {
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(kind),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn git(path: &Path, args: &[&str]) {
    success(
        Command::new("git")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
                "-C",
            ])
            .arg(path)
            .args(args)
            .output()
            .unwrap(),
    );
}

#[test]
fn fetch_frozen_corruption_repair_shared_cache_and_parallel_publish() {
    let f = Fixture::new();
    f.write(
        "remote/project.toml",
        "[project]\nname='lib'\nversion='1.0.0'\n[willow]\nmanifest-version=1\n",
    );
    f.write(
        "remote/src/value.wi",
        "module value; pub fn get() -> i64 { return 42; }",
    );
    let remote = f.0.join("remote");
    for args in [
        vec!["init", "--initial-branch=main", "--template="],
        vec!["config", "user.name", "Fixture"],
        vec!["config", "user.email", "fixture@example.invalid"],
        vec!["add", "project.toml", "src"],
        vec!["commit", "-m", "fixture"],
        vec!["tag", "1.0.0"],
    ] {
        git(&remote, &args);
    }
    let manifest = format!(
        "[project]\nname='app'\nversion='1.0.0'\n[willow]\nmanifest-version=1\n[dependencies]\na={{git='file://{}',version='1'}}\nb={{git='file://{}',version='1'}}\n",
        remote.to_string_lossy().replace('\\', "/"),
        remote.to_string_lossy().replace('\\', "/")
    );
    for app in ["app", "second", "third"] {
        f.write(&format!("{app}/project.toml"), &manifest);
        // Fetch must succeed without parsing/compiling this intentionally invalid file.
        f.write(&format!("{app}/src/main.wi"), "not valid Willow source");
    }
    failure(f.invoke("app", &["fetch", "--frozen"]), "lockfile_missing");
    failure(
        f.invoke("app", &["fetch", "--offline"]),
        "cache_missing_offline",
    );
    assert!(!f.0.join("app/project.lock").exists());
    // Separate processes publish the same source/revision concurrently.
    let first = f
        .command("app", &["fetch", "--format", "json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let second = f
        .command("second", &["fetch"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&success(first.wait_with_output().unwrap())).unwrap();
    assert_eq!(parsed["dependencies"], 1);
    success(second.wait_with_output().unwrap());
    let lock = fs::read(f.0.join("app/project.lock")).unwrap();
    assert!(String::from_utf8_lossy(&lock).contains("checksum = "));
    assert_eq!(fs::read(f.0.join("second/project.lock")).unwrap(), lock);
    assert_eq!(fs::read_dir(f.0.join("cache/packages")).unwrap().count(), 1);
    assert_eq!(fs::read_dir(f.0.join("cache/tmp")).unwrap().count(), 0);
    let entry = fs::read_dir(f.0.join("cache/packages"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let source = entry.join("tree/src/value.wi");
    assert!(fs::metadata(&source).unwrap().permissions().readonly());
    let original = fs::read(&source).unwrap();
    // Unavailable remote: normal pinned fetch and frozen builds still succeed.
    fs::rename(&remote, f.0.join("hidden-remote")).unwrap();
    success(f.invoke("app", &["fetch", "--locked"]));
    success(f.invoke("app", &["fetch", "--frozen", "--format=ndjson"]));
    // Cached refs allow offline resolution even without a lock.
    success(f.invoke("third", &["fetch", "--offline"]));
    assert_eq!(fs::read(f.0.join("third/project.lock")).unwrap(), lock);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.write(
            "tools/git",
            "#!/bin/sh\necho invoked >> \"$CACHE_GIT_SENTINEL\"\nexit 71\n",
        );
        fs::set_permissions(f.0.join("tools/git"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    f.write(
        "app/src/main.wi",
        "import a::value; fn main() { println(value::get()); }",
    );
    assert_eq!(success(f.invoke("app", &["run", "--frozen"])), "42\n");
    success(f.invoke("app", &["build", "--frozen", "-o", "frozen-app"]));
    assert_eq!(fs::read(f.0.join("app/project.lock")).unwrap(), lock);
    // Pinning uses only the tree; cached Git metadata is not required.
    fs::rename(f.0.join("cache/git/db"), f.0.join("saved-db")).unwrap();
    success(f.invoke("app", &["fetch", "--frozen"]));
    fs::rename(f.0.join("saved-db"), f.0.join("cache/git/db")).unwrap();
    writable(&source);
    fs::write(&source, "damaged").unwrap();
    failure(
        f.invoke("app", &["build", "--frozen"]),
        "cache_missing_offline",
    );
    assert_eq!(fs::read(f.0.join("app/project.lock")).unwrap(), lock);
    #[cfg(unix)]
    {
        assert!(!f.0.join("unexpected-git").exists(), "frozen invoked Git");
        fs::remove_file(f.0.join("tools/git")).unwrap();
    }
    fs::rename(f.0.join("hidden-remote"), &remote).unwrap();
    success(f.invoke("app", &["fetch", "--locked"]));
    assert_eq!(fs::read(&source).unwrap(), original);
    assert!(fs::metadata(&source).unwrap().permissions().readonly());
    // A forged lock checksum must never be silently accepted or rewritten.
    let text = String::from_utf8(lock.clone()).unwrap();
    let digest = fs::read_to_string(entry.join("sha256")).unwrap();
    let forged = text.replace(&digest, &"0".repeat(64));
    f.write("app/project.lock", &forged);
    failure(f.invoke("app", &["fetch"]), "cache_checksum_mismatch");
    assert_eq!(
        fs::read_to_string(f.0.join("app/project.lock")).unwrap(),
        forged
    );
    f.write("app/project.lock", &text);
    // Cache miss with an otherwise valid frozen lock fails without modifying it.
    fs::rename(&entry, f.0.join("saved-entry")).unwrap();
    failure(
        f.invoke("app", &["fetch", "--frozen"]),
        "cache_missing_offline",
    );
    assert_eq!(fs::read(f.0.join("app/project.lock")).unwrap(), lock);
    success(f.invoke("app", &["fetch", "--locked"]));
    assert_eq!(fs::read(f.0.join("app/project.lock")).unwrap(), lock);
}

#[test]
fn frozen_short_revision_reuses_full_pin_without_git_metadata() {
    let f = Fixture::new();
    f.write(
        "remote/project.toml",
        "[project]\nname='lib'\nversion='1.0.0'\n[willow]\nmanifest-version=1\n",
    );
    f.write("remote/src/value.wi", "module value;");
    let remote = f.0.join("remote");
    for args in [
        vec!["init", "--initial-branch=main", "--template="],
        vec!["config", "user.name", "Fixture"],
        vec!["config", "user.email", "fixture@example.invalid"],
        vec!["add", "project.toml", "src"],
        vec!["commit", "-m", "fixture"],
    ] {
        git(&remote, &args);
    }
    let full = success(
        Command::new("git")
            .arg("-C")
            .arg(&remote)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap(),
    );
    let manifest = format!(
        "[project]\nname='app'\nversion='1.0.0'\n[willow]\nmanifest-version=1\n[dependencies]\na={{git='file://{}',rev='{}'}}\n",
        remote.to_string_lossy().replace('\\', "/"),
        &full[..12]
    );
    f.write("app/project.toml", &manifest);
    f.write("app/src/main.wi", "fn main() {}");
    success(f.invoke("app", &["fetch"]));
    let lock = fs::read(f.0.join("app/project.lock")).unwrap();
    assert!(String::from_utf8_lossy(&lock).contains(full.trim()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.write(
            "tools/git",
            "#!/bin/sh\necho invoked >> \"$CACHE_GIT_SENTINEL\"\nexit 71\n",
        );
        fs::set_permissions(f.0.join("tools/git"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    success(f.invoke("app", &["fetch", "--frozen"]));
    fs::rename(f.0.join("cache/git/db"), f.0.join("saved-db")).unwrap();
    for args in [
        vec!["fetch", "--frozen"],
        vec!["fetch", "--locked"],
        vec!["fetch"],
    ] {
        success(f.invoke("app", &args));
        assert_eq!(fs::read(f.0.join("app/project.lock")).unwrap(), lock);
    }
    #[cfg(unix)]
    assert!(
        !f.0.join("unexpected-git").exists(),
        "locked short rev invoked Git"
    );
    // A changed abbreviated revision is not authorized by the old selector.
    f.write(
        "app/project.toml",
        &manifest.replace(&full[..12], "deadbeef"),
    );
    failure(f.invoke("app", &["fetch", "--frozen"]), "lockfile_stale");
    assert_eq!(fs::read(f.0.join("app/project.lock")).unwrap(), lock);
}
