use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

struct Project(PathBuf);
impl Project {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = Self(std::env::temp_dir().join(format!(
            "willow-lock-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )));
        p.write("project.toml", "[project]\nname='app'\nversion='1.0.0'\n[willow]\nmanifest-version=1\n[dependencies]\nlib={path='lib'}");
        p.write(
            "lib/project.toml",
            "[project]\nname='lib'\nversion='1.0.0'\n[willow]\nmanifest-version=1",
        );
        p.write(
            "src/main.wi",
            "import lib::value; fn main() { println(value::get()); }",
        );
        p.write(
            "lib/src/value.wi",
            "module value; pub fn get() -> i64 { return 42; }",
        );
        p
    }
    fn write(&self, name: &str, text: &str) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_willowc"));
        c.current_dir(&self.0).args(args);
        c
    }
    fn invoke(&self, args: &[&str], success: bool, message: &str) -> Output {
        let out = self.command(args).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.success(), success, "{args:?}: {stderr}");
        assert!(stderr.contains(message), "{args:?}: {stderr}");
        out
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn package_lock_cli_build_run_and_stale_recovery() {
    let p = Project::new();
    for command in ["build", "run"] {
        p.invoke(&[command, "--locked"], false, "lockfile_missing");
    }
    p.invoke(&["build", "-o", "app"], true, "");
    let lock = fs::read(p.0.join("project.lock")).unwrap();
    p.invoke(&["build", ".", "--locked", "-o", "app"], true, "");
    let output = p.invoke(&["run", ".", "--locked"], true, "");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n");
    assert_eq!(fs::read(p.0.join("project.lock")).unwrap(), lock);
    p.write(
        "lib/project.toml",
        "[project]\nname='lib'\nversion='2.0.0'\n[willow]\nmanifest-version=1",
    );
    for command in ["build", "run"] {
        p.invoke(&[command, "--locked"], false, "lockfile_stale");
    }
    assert_eq!(fs::read(p.0.join("project.lock")).unwrap(), lock);
    p.invoke(&["build", "-o", "app"], true, "");
    p.invoke(&["build", "--locked", "-o", "app"], true, "");
    assert_ne!(fs::read(p.0.join("project.lock")).unwrap(), lock);
}

#[test]
fn package_lock_cli_preserves_legacy_custom_entry_and_single_file() {
    let p = Project::new();
    p.write(
        "project.toml",
        "[project]\nname='legacy'\nversion='1.0.0'\nentry='main.wi'",
    );
    fs::remove_dir_all(p.0.join("src")).unwrap();
    p.write("main.wi", "fn main() { println(7); }");
    p.invoke(&["build", "-o", "legacy"], true, "");
    p.invoke(&["build", "--locked", "-o", "legacy"], true, "");
    let output = p.invoke(&["run", "--locked"], true, "");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "7\n");
    fs::remove_file(p.0.join("project.lock")).unwrap();
    p.invoke(&["main.wi", "-o", "single"], true, "");
    p.invoke(&["run", "main.wi"], true, "");
    assert!(!p.0.join("project.lock").exists());
    for command in ["build", "run"] {
        p.invoke(
            &[command, "main.wi", "--locked"],
            false,
            "requires project mode",
        );
    }
}

#[cfg(unix)]
#[test]
fn package_lock_reuse_starts_zero_git_or_network_tools() {
    use std::os::unix::fs::PermissionsExt;
    let p = Project::new();
    p.invoke(&["build", "-o", "app"], true, "");
    for tool in ["git", "curl", "wget"] {
        p.write(
            &format!("tools/{tool}"),
            "#!/bin/sh\necho called >> \"$WILLOW_LOCK_PROCESS_LOG\"\nexit 91\n",
        );
        fs::set_permissions(
            p.0.join("tools").join(tool),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let log = p.0.join("process-count");
    let mut paths = vec![p.0.join("tools")];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let path = std::env::join_paths(paths).unwrap();
    // Verify the counter actually intercepts a tool launch.
    let probe = Command::new("git")
        .env("PATH", &path)
        .env("WILLOW_LOCK_PROCESS_LOG", &log)
        .output()
        .unwrap();
    assert_eq!(probe.status.code(), Some(91));
    assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
    fs::remove_file(&log).unwrap();
    for args in [
        vec!["build", "-o", "app"],
        vec!["build", "--locked", "-o", "app"],
    ] {
        let out = p
            .command(&args)
            .env("PATH", &path)
            .env("WILLOW_LOCK_PROCESS_LOG", &log)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(
        !log.exists(),
        "package resolution launched a Git/network tool"
    );
    println!("valid-lock normal+locked builds: git/curl/wget operations=0");
}
