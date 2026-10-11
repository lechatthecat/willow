#![cfg(feature = "packaged")]

use std::path::PathBuf;
use std::process::{Command, Output};

struct Package(PathBuf);

impl Package {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("willow packaged-{}", std::process::id()));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::copy(
            env!("CARGO_BIN_EXE_willow"),
            root.join("bin").join(binary_name()),
        )
        .unwrap();
        std::fs::write(root.join("hello.wi"), "fn main() { print(\"hello\"); }\n").unwrap();
        Self(root)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(self.0.join("bin").join(binary_name()));
        command
            .current_dir(&self.0)
            .env_remove("WILLOW_RUNTIME_LIB")
            .env("CARGO_TARGET_DIR", self.0.join("forbidden-target"));
        command
    }

    fn build(&self, args: &[&str]) -> Output {
        self.command()
            .args(["build", "hello.wi", "-o", "hello"])
            .args(args)
            .output()
            .unwrap()
    }
}

impl Drop for Package {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "willow.exe"
    } else {
        "willow"
    }
}

fn archive_name() -> &'static str {
    if cfg!(target_env = "msvc") {
        "willow_runtime.lib"
    } else {
        "libwillow_runtime.a"
    }
}

#[test]
fn relocated_packaged_binary_runtime_discovery() {
    let package = Package::new();
    for args in [&[][..], &["--release"][..]] {
        let output = package.build(args);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        for expected in [
            "E0700",
            "searched bundled runtime",
            "WILLOW_RUNTIME_LIB",
            "--runtime-lib",
            "release archive",
        ] {
            assert!(stderr.contains(expected), "{stderr}");
        }
        assert!(
            stderr.contains(
                &package
                    .0
                    .join("lib")
                    .join(archive_name())
                    .display()
                    .to_string()
            ),
            "{stderr}"
        );
        for forbidden in [
            "Permission denied",
            env!("CARGO_MANIFEST_DIR"),
            "build willow_runtime with Cargo",
        ] {
            assert!(!stderr.contains(forbidden), "{stderr}");
        }
        assert!(!package.0.join("forbidden-target").exists());
        assert!(!package.0.join("hello.o").exists());
        assert!(!package.0.join("hello.obj").exists());
    }
    let output = package.build(&["--runtime-lib", "missing-runtime"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("missing-runtime"), "{stderr}");
    assert!(!stderr.contains("searched bundled"), "{stderr}");
    let output = package
        .command()
        .env("WILLOW_RUNTIME_LIB", "environment-runtime")
        .args(["build", "hello.wi", "-o", "hello"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("environment-runtime"));
    let output = package
        .command()
        .env("WILLOW_RUNTIME_LIB", "environment-runtime")
        .args([
            "build",
            "hello.wi",
            "-o",
            "hello",
            "--runtime-lib",
            "explicit-runtime",
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("explicit-runtime"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("environment-runtime"));

    let runtime = PathBuf::from(env!("CARGO_BIN_EXE_willow"))
        .parent()
        .unwrap()
        .join(archive_name());
    assert!(
        runtime.is_file(),
        "build willow_runtime before running this test"
    );
    std::fs::create_dir_all(package.0.join("lib")).unwrap();
    std::fs::create_dir(package.0.join("lib").join(archive_name())).unwrap();
    let output = package.build(&[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("searched bundled"));
    std::fs::remove_dir(package.0.join("lib").join(archive_name())).unwrap();
    std::fs::copy(&runtime, package.0.join("lib").join(archive_name())).unwrap();
    let output = package
        .command()
        .args(["run", "hello.wi"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    assert!(!package.0.join("forbidden-target").exists());
    let output = package.build(&["--runtime-lib", "still-missing"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("still-missing"));
    std::fs::remove_dir_all(package.0.join("lib")).unwrap();
    let output = package
        .command()
        .args(["run", "hello.wi", "--runtime-lib"])
        .arg(&runtime)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    let output = package
        .command()
        .env("WILLOW_RUNTIME_LIB", &runtime)
        .args(["run", "hello.wi"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
}
