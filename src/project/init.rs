//! Project scaffolding, independent of terminal and agent selection.
use super::{ProjectManifest, ProjectSection, WillowSection};
use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

pub const MAIN: &str = "fn main() {\n    println(\"Hello, Willow!\");\n}\n";

pub fn manifest(name: &str) -> Result<String> {
    ProjectManifest {
        project: ProjectSection {
            name: name.into(),
            version: "0.1.0".into(),
            entry: Some("src/main.wi".into()),
        },
        dependencies: BTreeMap::new(),
        willow: Some(WillowSection {
            manifest_version: 1,
        }),
    }
    .validate()
    .context("help: use --name with a valid project name")?;
    let mut chars = name.bytes();
    anyhow::ensure!(
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "invalid project name; help: use --name with letters, digits or underscores, starting with a letter or underscore"
    );
    Ok(format!(
        "[willow]\nmanifest-version = 1\n\n[project]\nname = \"{name}\"\nversion = \"0.1.0\"\nentry = \"src/main.wi\"\n\n[dependencies]\n"
    ))
}

/// Resolve existing symlinks before `..`, while allowing missing directories.
pub fn resolve_root(root: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(root)?;
    let mut resolved = PathBuf::new();
    let mut missing = 0usize;
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
                missing = missing.saturating_sub(1);
            }
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir => resolved.push(component),
            Component::Normal(_) => {
                resolved.push(component);
                if missing > 0 {
                    missing += 1;
                    continue;
                }
                match fs::symlink_metadata(&resolved) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() {
                            resolved = fs::canonicalize(&resolved)?;
                        }
                        anyhow::ensure!(resolved.is_dir(), "parent is not a directory");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = 1,
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    Ok(resolved)
}

/// Tracks only entries created by this invocation. Never recursively removes directories.
#[derive(Default)]
pub struct Scaffold {
    files: Vec<PathBuf>,
    directories: Vec<(PathBuf, usize)>,
    committed: bool,
}
impl Scaffold {
    pub fn create(root: &Path, name: Option<&str>) -> Result<Self> {
        let absolute = resolve_root(root)?;
        let default_name = absolute.file_name().and_then(|n| n.to_str());
        let text = manifest(
            name.or(default_name)
                .context("cannot derive project name; help: use --name")?,
        )?;
        anyhow::ensure!(
            !absolute.join("project.toml").try_exists()?,
            "project.toml already exists"
        );
        let mut result = Self::default();
        result.create_dirs(&absolute)?;
        result.write_new(&absolute.join("project.toml"), text.as_bytes())?;
        result.create_dirs(&absolute.join("src"))?;
        let main = absolute.join("src/main.wi");
        match fs::symlink_metadata(&main) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                result.write_new(&main, MAIN.as_bytes())?
            }
            Err(error) => return Err(error.into()),
        }
        Ok(result)
    }
    fn create_dirs(&mut self, path: &Path) -> Result<()> {
        let mut missing = Vec::new();
        let mut current = path;
        while !current.try_exists()? {
            missing.push(current);
            current = current.parent().context("directory has no parent")?;
        }
        anyhow::ensure!(current.is_dir(), "parent is not a directory");
        if !missing.is_empty() {
            self.directories.push((current.to_path_buf(), 0));
            let (last, count) = self.directories.last_mut().unwrap();
            for directory in missing.into_iter().rev() {
                fs::create_dir(directory)?;
                last.push(directory.file_name().context("directory has no name")?);
                *count += 1;
            }
        }
        Ok(())
    }
    pub fn write_new(&mut self, path: &Path, bytes: &[u8]) -> Result<()> {
        atomic_write(path, bytes, false)?;
        self.files.push(path.to_path_buf());
        Ok(())
    }
    pub fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for Scaffold {
    fn drop(&mut self) {
        if !self.committed {
            for file in self.files.iter().rev() {
                let _ = fs::remove_file(file);
            }
            for (directory, count) in self.directories.iter().rev() {
                for path in directory.ancestors().take(*count) {
                    let _ = fs::remove_dir(path);
                }
            }
        }
    }
}

pub fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write(path, bytes, true)
}

fn atomic_write(path: &Path, bytes: &[u8], replace: bool) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().context("file has no parent")?;
    let (temp, mut file) = loop {
        let temp = parent.join(format!(
            ".willow-init-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        if replace && let Ok(metadata) = fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        if replace {
            fs::rename(&temp, path)?;
        } else {
            // Same-directory hard link publishes complete bytes atomically and
            // fails if any destination entry exists (including dangling symlinks).
            fs::hard_link(&temp, path)?;
        }
        Ok(())
    })();
    let _ = fs::remove_file(temp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deep_directory_journal_is_linear_and_rollback_removes_only_owned_entries() {
        let base = std::env::temp_dir().join(format!("willow_init_depth_{}", std::process::id()));
        fs::create_dir(&base).unwrap();
        fs::write(base.join("keep"), "user").unwrap();
        for depth in [8, 32, 64] {
            let mut root = base.clone();
            for _ in 0..depth {
                root.push("d");
            }
            let scaffold = Scaffold::create(&root, Some("demo")).unwrap();
            assert_eq!(scaffold.directories.len(), 2);
            assert_eq!(
                scaffold.directories.iter().map(|(_, n)| n).sum::<usize>(),
                depth + 1
            );
            assert_eq!(scaffold.files.len(), 2);
            println!(
                "depth={depth} journal_paths=2 directories={} files=2",
                depth + 1
            );
            drop(scaffold);
            assert!(!base.join("d").exists());
            assert_eq!(fs::read_to_string(base.join("keep")).unwrap(), "user");
        }
        // Failed publication must remove its temporary file, preserve its target.
        assert!(atomic_replace(&base, b"cannot replace directory").is_err());
        assert_eq!(fs::read_dir(&base).unwrap().count(), 1);
        fs::remove_dir_all(base).unwrap();
    }
}
