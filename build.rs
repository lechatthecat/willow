use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn source_hash(roots: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut files = Vec::new();
    let mut pending: Vec<_> = roots.iter().map(std::path::PathBuf::from).collect();
    while let Some(path) = pending.pop() {
        println!("cargo:rerun-if-changed={}", path.display());
        if path.is_dir() {
            for entry in std::fs::read_dir(path).expect("source directory") {
                pending.push(entry.expect("source entry").path());
            }
        } else {
            files.push(path);
        }
    }
    files.sort();
    let mut hash = Sha256::new();
    for file in files {
        let name = file.to_string_lossy();
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        let bytes = std::fs::read(file).expect("compiler source");
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn main() {
    println!(
        "cargo:rustc-env=WILLOW_COMPILER_SCHEMA_HASH={}",
        source_hash(&[
            "src",
            "crates/willow_abi",
            "crates/willow_continuations",
            "Cargo.toml",
            "Cargo.lock",
            "build.rs"
        ])
    );
    println!(
        "cargo:rustc-env=WILLOW_STDLIB_SCHEMA_HASH={}",
        source_hash(&["std"])
    );
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=WILLOW_BUILD_COMMIT");
    // Do not accidentally identify a source archive using an enclosing repository.
    // Watching the .git directory recursively would scan the entire object store.
    // A worktree's .git file (or a not-yet-created .git) is cheap to watch.
    if !Path::new(".git").is_dir() {
        println!("cargo:rerun-if-changed=.git");
    }
    let commit = if Path::new(".git").exists() {
        for name in ["HEAD", "packed-refs"] {
            if let Some(path) = git(&["rev-parse", "--git-path", name]) {
                println!("cargo:rerun-if-changed={path}");
            }
        }
        if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"])
            && let Some(path) = git(&["rev-parse", "--git-path", &reference])
        {
            println!("cargo:rerun-if-changed={path}");
        }
        git(&["rev-parse", "--verify", "HEAD"])
    } else {
        None
    };
    let commit = std::env::var("WILLOW_BUILD_COMMIT").ok().or(commit);
    let commit =
        commit.filter(|s| matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit()));
    println!(
        "cargo:rustc-env=WILLOW_BUILD_COMMIT={}",
        commit.as_deref().unwrap_or("unknown")
    );
}
