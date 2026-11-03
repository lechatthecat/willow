//! Immutable Git/worktree input capture. Git is only used for local object reads.
use super::*;
use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
};

const LIMIT: usize = 64 * 1024 * 1024;
pub(super) type Tree = BTreeMap<String, Vec<u8>>;
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .output()?;
    ensure!(output.status.success(), "local Git input read failed");
    Ok(output.stdout)
}
pub fn resolve_base(root: &Path, revision: &str) -> Result<String> {
    let object = git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{tree}}"),
        ],
    )?;
    let object = String::from_utf8(object)?.trim().to_owned();
    ensure!(
        !object.is_empty() && object.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid revision"
    );
    Ok(object)
}
fn relevant(path: &str, lock: bool) -> bool {
    if lock && path == "contracts.lock" {
        return true;
    }
    path.ends_with(".wi")
        || path
            .rsplit('/')
            .next()
            .is_some_and(|s| matches!(s, "project.toml" | "project.lock"))
}
fn safe(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
        && Path::new(path)
            .components()
            .all(|p| matches!(p, std::path::Component::Normal(_)))
}
fn insert(tree: &mut Tree, total: &mut usize, path: String, bytes: Vec<u8>) -> Result<()> {
    *total = total
        .checked_add(bytes.len())
        .context("input size overflow")?;
    ensure!(*total <= LIMIT, "verification inputs exceed 64 MiB");
    ensure!(safe(&path), "invalid input path");
    ensure!(tree.insert(path, bytes).is_none(), "duplicate input path");
    Ok(())
}
pub(super) fn base(root: &Path, object: &str) -> Result<Tree> {
    base_inputs(root, object, false)
}
pub(super) fn base_inputs(root: &Path, object: &str, lock: bool) -> Result<Tree> {
    let prefix = String::from_utf8(git(root, &["rev-parse", "--show-prefix"])?)?;
    let prefix = prefix.trim_end_matches('\n');
    let listing = git(root, &["ls-tree", "-rz", "--full-tree", object])?;
    let mut objects = Vec::new();
    for record in listing.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let record = std::str::from_utf8(record)?;
        let (header, path) = record.split_once('\t').context("invalid Git tree")?;
        let Some(path) = path.strip_prefix(prefix) else {
            continue;
        };
        if !relevant(path, lock) {
            continue;
        }
        let parts: Vec<_> = header.split_whitespace().collect();
        ensure!(
            parts.len() == 3 && matches!(parts[0], "100644" | "100755") && parts[1] == "blob",
            "non-regular source input"
        );
        objects.push((path.to_owned(), parts[2].to_owned()));
    }
    // One batch process, with a bounded read per object. Request and consume one
    // object at a time so neither pipe can deadlock on a large repository.
    let mut child = Command::new("git")
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
        .arg("-C")
        .arg(root)
        .args(["cat-file", "--batch"])
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        use std::io::BufRead;
        let mut input = child.stdin.take().context("Git stdin")?;
        let mut output = std::io::BufReader::new(child.stdout.take().context("Git stdout")?);
        let mut tree = Tree::new();
        let mut total = 0;
        for (path, object) in objects {
            writeln!(input, "{object}")?;
            input.flush()?;
            let mut header = String::new();
            output.read_line(&mut header)?;
            let parts: Vec<_> = header.split_whitespace().collect();
            ensure!(
                parts.len() == 3 && parts[0] == object && parts[1] == "blob",
                "missing Git blob"
            );
            let size: usize = parts[2].parse()?;
            ensure!(size <= LIMIT - total, "verification inputs exceed 64 MiB");
            let mut bytes = vec![0; size];
            output.read_exact(&mut bytes)?;
            let mut newline = [0];
            output.read_exact(&mut newline)?;
            ensure!(newline == [b'\n'], "invalid Git blob framing");
            insert(&mut tree, &mut total, path, bytes)?;
        }
        Ok(tree)
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    ensure!(status.success(), "Git blob read failed");
    result
}
pub(super) fn target(root: &Path) -> Result<Tree> {
    target_inputs(root, false)
}
pub(super) fn target_inputs(root: &Path, lock: bool) -> Result<Tree> {
    let listing = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut paths: BTreeSet<_> = listing
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(std::str::from_utf8)
        .collect::<std::result::Result<_, _>>()?;
    if lock {
        match fs::symlink_metadata(root.join("contracts.lock")) {
            Ok(_) => {
                paths.insert("contracts.lock");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut tree = Tree::new();
    let mut total = 0;
    for path in paths {
        if !relevant(path, lock) {
            continue;
        }
        ensure!(safe(path), "invalid input path");
        let full = root.join(path);
        let metadata = match fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        ensure!(
            metadata.is_file() && fs::canonicalize(&full)? == full,
            "non-regular source input"
        );
        ensure!(
            metadata.len() <= (LIMIT - total) as u64,
            "verification inputs exceed 64 MiB"
        );
        let mut bytes = Vec::new();
        fs::File::open(full)?
            .take((LIMIT - total + 1) as u64)
            .read_to_end(&mut bytes)?;
        insert(&mut tree, &mut total, path.to_owned(), bytes)?;
    }
    Ok(tree)
}
pub(super) struct Scratch(pub PathBuf);
impl Scratch {
    pub fn new() -> Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "willow-semantic-input-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(fs::canonicalize(path)?)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!("cannot reserve isolated verification inputs")
    }
    pub fn write(&self, before: &Tree, after: &Tree) -> Result<()> {
        for path in before.keys().filter(|p| !after.contains_key(*p)) {
            fs::remove_file(self.0.join(path))?;
        }
        for (path, bytes) in after {
            let full = self.0.join(path);
            fs::create_dir_all(full.parent().context("input parent")?)?;
            fs::write(full, bytes)?;
        }
        Ok(())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn protected_commit(root: &Path, commit: &str) -> Result<String> {
    ensure!(
        matches!(commit.len(), 40 | 64) && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "protected base must be a full immutable commit ID"
    );
    let resolved = String::from_utf8(git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{commit}^{{commit}}"),
        ],
    )?)?;
    ensure!(
        resolved.trim() == commit,
        "protected base must identify a commit directly"
    );
    Ok(commit.to_owned())
}
