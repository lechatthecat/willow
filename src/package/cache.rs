//! Persistent source cache, independent of compiler analysis and artifact caches.
use super::{GitBackend, PackageError};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn home_from(
    willow: Option<PathBuf>,
    home: Option<PathBuf>,
    profile: Option<PathBuf>,
    windows: bool,
) -> Result<PathBuf, PackageError> {
    willow
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| (if windows { profile.or(home) } else { home }).map(|p| p.join(".willow")))
        .ok_or_else(|| {
            PackageError::GitMaterialization("cannot locate package cache; set WILLOW_HOME".into())
        })
}

pub(super) struct SourceCache {
    home: PathBuf,
    key: String,
    pub offline: bool,
    pub read_only: bool,
    pub expected: Option<String>,
}
impl SourceCache {
    pub fn new(url: &str, offline: bool, expected: Option<String>) -> Result<Self, PackageError> {
        let home = home_from(
            std::env::var_os("WILLOW_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
            std::env::var_os("USERPROFILE").map(PathBuf::from),
            cfg!(windows),
        )?;
        Ok(Self {
            home,
            key: hex(Sha256::digest(url.as_bytes())),
            offline,
            read_only: false,
            expected,
        })
    }
    pub fn repository(&self) -> PathBuf {
        self.home.join("git/db").join(&self.key)
    }
    pub fn tree(&self, revision: &str) -> PathBuf {
        self.entry(revision).join("tree")
    }
    fn entry(&self, revision: &str) -> PathBuf {
        self.home
            .join("packages")
            .join(format!("{}-{revision}", self.key))
    }
    pub fn lock(&self) -> Result<fs::File, PackageError> {
        let dir = self.home.join("git/locks");
        fs::create_dir_all(&dir)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(&self.key))?;
        file.lock()?;
        Ok(file)
    }
    pub fn materialize(
        &self,
        backend: &impl GitBackend,
        url: &str,
        revision: &str,
    ) -> Result<PathBuf, PackageError> {
        let _guard = (!self.read_only).then(|| self.lock()).transpose()?;
        let entry = self.entry(revision);
        let tree = self.tree(revision);
        let stored = fs::read_to_string(entry.join("sha256")).ok();
        if let Some(stored) = &stored
            && self
                .expected
                .as_ref()
                .is_none_or(|expected| expected == stored)
            && tree_checksum(&tree).is_ok_and(|sum| &sum == stored)
        {
            return Ok(tree);
        }
        if self.offline || self.read_only {
            return Err(PackageError::CacheMissingOffline(format!(
                "{url}@{revision} (absent or checksum mismatch)"
            )));
        }
        // A damaged tree is rebuilt from exact Git objects; refresh those objects
        // on corruption, never choose a new tag or revision.
        let repository = self.repository();
        if entry.exists() || backend.revision(&repository, revision).is_err() {
            backend.fetch_revision(url, &repository, revision)?;
        }
        let temporary = Temporary::new(&self.home.join("tmp"))?;
        let fresh = temporary.0.join("tree");
        backend.checkout(&repository, revision, &fresh)?;
        let checksum = tree_checksum_mode(&fresh, true)?;
        if self
            .expected
            .as_ref()
            .is_some_and(|expected| expected != &checksum)
        {
            return Err(PackageError::CacheChecksumMismatch(format!(
                "{url}@{revision}"
            )));
        }
        let mut file = fs::File::create(temporary.0.join("sha256"))?;
        file.write_all(checksum.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::create_dir_all(entry.parent().unwrap())?;
        if entry.exists() {
            remove_tree(&entry)?;
        }
        fs::rename(&temporary.0, &entry)?;
        Ok(tree)
    }
}

struct Temporary(PathBuf);
impl Temporary {
    fn new(parent: &Path) -> Result<Self, PackageError> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        fs::create_dir_all(parent)?;
        loop {
            let path = parent.join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = remove_tree(&self.0);
    }
}

fn remove_tree(root: &Path) -> std::io::Result<()> {
    // Restore owner access for removal; never follow cached symlinks.
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() || (cfg!(windows) && metadata.is_file()) {
            let mut permissions = metadata.permissions();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                permissions.set_mode(permissions.mode() | 0o700);
            }
            #[cfg(windows)]
            permissions.set_readonly(false);
            fs::set_permissions(&path, permissions)?;
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                stack.push(entry?.path());
            }
        }
    }
    fs::remove_dir_all(root)
}

// A directory is SHA256(domain || sorted(length, basename, child digest)).
// MSD radix ordering is linear in filename bytes, unlike comparison sorting.
// Explicit frames avoid call-stack overflow on deep directory trees.
struct Child {
    name: Vec<u8>,
    digest: [u8; 32],
}
fn radix(children: &mut [Child]) -> usize {
    let mut inspections = 0;
    let mut work = vec![(0, children.len(), 0)];
    while let Some((start, end, depth)) = work.pop() {
        if end - start < 2 {
            continue;
        }
        let bucket = |child: &Child| child.name.get(depth).map_or(0, |b| *b as usize + 1);
        let mut counts = [0; 257];
        for child in &children[start..end] {
            inspections += 1;
            counts[bucket(child)] += 1;
        }
        let mut bounds = [start; 258];
        for i in 0..257 {
            bounds[i + 1] = bounds[i] + counts[i];
        }
        let mut next = bounds;
        for i in 0..257 {
            while next[i] < bounds[i + 1] {
                inspections += 1;
                let target = bucket(&children[next[i]]);
                if target == i {
                    next[i] += 1;
                } else {
                    children.swap(next[i], next[target]);
                    next[target] += 1;
                }
            }
        }
        for i in 1..257 {
            if counts[i] > 1 {
                work.push((bounds[i], bounds[i + 1], depth + 1));
            }
        }
    }
    inspections
}

#[derive(Default, Debug)]
pub(super) struct ChecksumStats {
    pub entries: usize,
    pub bytes: usize,
    pub name_bytes: usize,
    pub radix_inspections: usize,
}
#[cfg(test)]
thread_local! { pub(super) static CHECKSUM_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
pub(super) fn tree_checksum(root: &Path) -> Result<String, PackageError> {
    tree_checksum_mode(root, false)
}
fn tree_checksum_mode(root: &Path, readonly: bool) -> Result<String, PackageError> {
    #[cfg(test)]
    CHECKSUM_RUNS.with(|count| count.set(count.get() + 1));
    Ok(checksum(root, readonly)?.0)
}
#[cfg(test)]
pub(super) fn checksum_counted(root: &Path) -> Result<(String, ChecksumStats), PackageError> {
    checksum(root, false)
}
fn checksum(root: &Path, readonly: bool) -> Result<(String, ChecksumStats), PackageError> {
    struct Frame {
        name: Vec<u8>,
        entries: std::vec::IntoIter<std::ffi::OsString>,
        children: Vec<Child>,
    }
    fn frame(path: &Path, name: Vec<u8>) -> Result<Frame, PackageError> {
        // Symlink directories must not be followed, including the cache root.
        if !fs::symlink_metadata(path)?.is_dir() {
            return Err(PackageError::GitMaterialization(
                "cache tree is not a directory".into(),
            ));
        }
        Ok(Frame {
            // Close ReadDir here. Frames retain basenames, not a full path and
            // OS directory handle for every ancestor of a deep tree.
            entries: fs::read_dir(path)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter(),
            name,
            children: Vec::new(),
        })
    }
    let mut stack = vec![frame(root, Vec::new())?];
    let mut path = root.to_path_buf();
    let mut stats = ChecksumStats::default();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let current = stack.last_mut().unwrap();
        if let Some(filename) = current.entries.next() {
            let name = filename
                .to_str()
                .ok_or_else(|| PackageError::GitMaterialization("non-UTF8 Git path".into()))?
                .as_bytes()
                .to_vec();
            stats.entries += 1;
            stats.name_bytes += name.len();
            path.push(&filename);
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                stack.push(frame(&path, name)?);
                continue;
            }
            let mut hash = Sha256::new();
            if metadata.file_type().is_symlink() {
                hash.update(b"willow-symlink-v1\0");
                let target = fs::read_link(&path)?;
                let target = target
                    .to_str()
                    .ok_or_else(|| PackageError::GitMaterialization("non-UTF8 symlink".into()))?;
                stats.bytes += target.len();
                hash.update(target.as_bytes());
            } else if metadata.is_file() {
                hash.update(b"willow-file-v1\0");
                let mut file = fs::File::open(&path)?;
                loop {
                    let n = file.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    stats.bytes += n;
                    hash.update(&buffer[..n]);
                }
                if readonly {
                    let mut permissions = metadata.permissions();
                    permissions.set_readonly(true);
                    fs::set_permissions(&path, permissions)?;
                }
            } else {
                return Err(PackageError::GitMaterialization(
                    "unsupported cache entry".into(),
                ));
            }
            path.pop();
            current.children.push(Child {
                name,
                digest: hash.finalize().into(),
            });
        } else {
            let mut finished = stack.pop().unwrap();
            if readonly {
                let mut permissions = fs::metadata(&path)?.permissions();
                permissions.set_readonly(true);
                fs::set_permissions(&path, permissions)?;
            }
            path.pop();
            stats.radix_inspections += radix(&mut finished.children);
            let mut hash = Sha256::new();
            hash.update(b"willow-directory-v1\0");
            for child in finished.children {
                hash.update((child.name.len() as u64).to_le_bytes());
                hash.update(child.name);
                hash.update(child.digest);
            }
            let digest = hash.finalize().into();
            if let Some(parent) = stack.last_mut() {
                parent.children.push(Child {
                    name: finished.name,
                    digest,
                });
            } else {
                return Ok((hex(digest), stats));
            }
        }
    }
}

#[cfg(test)]
mod tests;
