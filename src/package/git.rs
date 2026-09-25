//! Git transport is isolated from source identity and dependency solving.
use super::{PackageError, PackageIdentity, PackageSource, PackageSourceIdentity, PathSource};
use crate::project::{CanonicalGitUrl, GitSelector};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
};

/// Backends must never run repository hooks, filters, or installation scripts.
pub trait GitBackend {
    fn fetch(&self, url: &str, repository: &Path) -> Result<(), PackageError>;
    fn fetch_revision(
        &self,
        url: &str,
        repository: &Path,
        revision: &str,
    ) -> Result<(), PackageError>;
    fn tags(&self, repository: &Path) -> Result<Vec<String>, PackageError>;
    fn revision(&self, repository: &Path, selector: &str) -> Result<String, PackageError>;
    fn checkout(
        &self,
        repository: &Path,
        revision: &str,
        destination: &Path,
    ) -> Result<(), PackageError>;
}

#[derive(Debug, Default)]
pub struct SystemGit;
impl SystemGit {
    fn command(repository: &Path) -> Command {
        let mut command = Command::new("git");
        // Do not inherit injected config, alternate object stores, templates or
        // repository paths. No user's credential/filter/helper code is run.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.file.allow=always",
                "-c",
                "protocol.https.allow=always",
                "-c",
                "protocol.http.allow=always",
                "-c",
                "protocol.git.allow=always",
                "-c",
                "protocol.ssh.allow=always",
                "-c",
                "credential.helper=",
                "-c",
                "core.autocrlf=false",
            ])
            .arg("--git-dir")
            .arg(repository);
        command
    }
    fn run(repository: &Path, args: &[&str]) -> Result<String, PackageError> {
        let output = Self::command(repository)
            .args(args)
            .output()
            .map_err(|e| PackageError::GitMaterialization(e.to_string()))?;
        if !output.status.success() {
            return Err(PackageError::GitMaterialization(
                String::from_utf8_lossy(&output.stderr).trim().into(),
            ));
        }
        String::from_utf8(output.stdout)
            .map_err(|e| PackageError::GitMaterialization(e.to_string()))
    }
}
impl GitBackend for SystemGit {
    fn fetch(&self, url: &str, repository: &Path) -> Result<(), PackageError> {
        std::fs::create_dir_all(repository)
            .map_err(|e| PackageError::GitMaterialization(e.to_string()))?;
        Self::run(repository, &["init", "--bare", "--template="])?;
        Self::run(
            repository,
            &[
                "fetch",
                "--force",
                "--no-recurse-submodules",
                "--prune",
                "--",
                url,
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ],
        )
        .map_err(|e| PackageError::SourceUnreachable {
            url: url.into(),
            message: e.to_string(),
        })?;
        Ok(())
    }
    fn fetch_revision(
        &self,
        url: &str,
        repository: &Path,
        revision: &str,
    ) -> Result<(), PackageError> {
        std::fs::create_dir_all(repository)?;
        Self::run(repository, &["init", "--bare", "--template="])?;
        Self::run(
            repository,
            &[
                "fetch",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                url,
                revision,
            ],
        )?;
        Ok(())
    }
    fn tags(&self, repository: &Path) -> Result<Vec<String>, PackageError> {
        Ok(Self::run(
            repository,
            &["for-each-ref", "--format=%(refname:strip=2)", "refs/tags/"],
        )?
        .lines()
        .map(str::to_owned)
        .collect())
    }
    fn revision(&self, repository: &Path, selector: &str) -> Result<String, PackageError> {
        if selector.starts_with("refs/") {
            Self::run(repository, &["check-ref-format", selector])?;
        } else if selector.is_empty() || !selector.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(PackageError::GitMaterialization(
                "expected a literal ref or commit ID".into(),
            ));
        }
        Ok(Self::run(
            repository,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{selector}^{{commit}}"),
            ],
        )?
        .trim()
        .into())
    }
    fn checkout(
        &self,
        repository: &Path,
        revision: &str,
        destination: &Path,
    ) -> Result<(), PackageError> {
        std::fs::create_dir_all(destination)
            .map_err(|e| PackageError::GitMaterialization(e.to_string()))?;
        // Fresh bare repository has no inherited filters; checkout does not
        // recurse into submodules and hooks are disabled for every invocation.
        let output = Self::command(repository)
            .arg("--work-tree")
            .arg(destination)
            .args(["checkout", "--force", revision, "--", "."])
            .output()
            .map_err(|e| PackageError::GitMaterialization(e.to_string()))?;
        if !output.status.success() {
            return Err(PackageError::GitMaterialization(
                String::from_utf8_lossy(&output.stderr).into(),
            ));
        }
        Ok(())
    }
}

type Candidate = (String, Option<String>);
type CandidateList = std::rc::Rc<Vec<Candidate>>;

pub(crate) struct GitCandidates {
    entries: CandidateList,
    position: usize,
}
impl Iterator for GitCandidates {
    type Item = Candidate;
    fn next(&mut self) -> Option<Self::Item> {
        let candidate = self.entries.get(self.position)?.clone();
        self.position += 1;
        Some(candidate)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.entries.len() - self.position;
        (remaining, Some(remaining))
    }
}
impl ExactSizeIterator for GitCandidates {}

pub struct GitSource<B: GitBackend = SystemGit> {
    url: CanonicalGitUrl,
    repository: PathBuf,
    checkouts: PathBuf,
    backend: B,
    cache: Option<super::cache::SourceCache>,
    refs: RefCell<HashMap<String, String>>,
    pub(crate) fetches: Cell<usize>,
    tags: RefCell<Option<Vec<(semver::Version, String)>>>,
    candidates: RefCell<HashMap<GitSelector, CandidateList>>,
    pub(crate) candidate_checks: Cell<usize>,
    packages: RefCell<HashMap<String, (PackageIdentity, std::rc::Rc<PathSource>)>>,
}
impl<B: GitBackend> GitSource<B> {
    /// `directory` is an empty, caller-owned session directory, not a cache.
    pub fn open(url: CanonicalGitUrl, directory: &Path, backend: B) -> Result<Self, PackageError> {
        let repository = directory.join("repository");
        backend.fetch(url.as_str(), &repository)?;
        Ok(Self {
            url,
            repository,
            checkouts: directory.join("checkouts"),
            backend,
            cache: None,
            refs: RefCell::new(HashMap::new()),
            fetches: Cell::new(1),
            tags: RefCell::new(None),
            candidates: RefCell::new(HashMap::new()),
            candidate_checks: Cell::new(0),
            packages: RefCell::new(HashMap::new()),
        })
    }
    pub(crate) fn cached_mode(
        url: CanonicalGitUrl,
        backend: B,
        offline: bool,
        pinned: bool,
        expected: Option<String>,
        read_only: bool,
    ) -> Result<Self, PackageError> {
        let mut cache = super::cache::SourceCache::new(url.as_str(), offline, expected)?;
        cache.read_only = read_only;
        let repository = cache.repository();
        {
            let _guard = (!read_only).then(|| cache.lock()).transpose()?;
            if !pinned {
                if offline || read_only {
                    if !repository.is_dir() {
                        return Err(PackageError::CacheMissingOffline(url.as_str().into()));
                    }
                } else {
                    backend.fetch(url.as_str(), &repository)?;
                }
            }
        }
        Ok(Self {
            url,
            repository,
            checkouts: PathBuf::new(),
            backend,
            cache: Some(cache),
            refs: RefCell::new(HashMap::new()),
            fetches: Cell::new(usize::from(!pinned && !offline && !read_only)),
            tags: RefCell::new(None),
            candidates: RefCell::new(HashMap::new()),
            candidate_checks: Cell::new(0),
            packages: RefCell::new(HashMap::new()),
        })
    }
    /// Newly introduced selectors must be checked against current remote refs.
    /// Established lock selectors never enter this path.
    pub(crate) fn refresh_refs(&self) -> Result<(), PackageError> {
        let Some(cache) = &self.cache else {
            return Ok(());
        };
        if self.fetches.get() != 0 || cache.offline || cache.read_only {
            return Ok(());
        }
        let _guard = cache.lock()?;
        self.backend.fetch(self.url.as_str(), &self.repository)?;
        self.refs.borrow_mut().clear();
        self.tags.borrow_mut().take();
        self.candidates.borrow_mut().clear();
        self.fetches.set(self.fetches.get() + 1);
        Ok(())
    }

    pub(crate) fn unlock(&mut self) -> Result<(), PackageError> {
        if let Some(cache) = &mut self.cache {
            cache.expected = None;
        }
        self.refresh_refs()
    }

    pub(crate) fn checksum(&self, revision: &str) -> Result<Option<String>, PackageError> {
        self.cache
            .as_ref()
            .map(|cache| {
                std::fs::read_to_string(cache.tree(revision).parent().unwrap().join("sha256"))
            })
            .transpose()
            .map_err(Into::into)
    }
    fn checkout_root(&self, revision: &str) -> PathBuf {
        self.cache.as_ref().map_or_else(
            || self.checkouts.join(revision),
            |cache| cache.tree(revision),
        )
    }
    fn load_tags(&self) -> Result<(), PackageError> {
        if self.tags.borrow().is_some() {
            return Ok(());
        }
        let _guard = self
            .cache
            .as_ref()
            .filter(|cache| !cache.read_only)
            .map(|cache| cache.lock())
            .transpose()?;
        let mut tags: Vec<_> = self
            .backend
            .tags(&self.repository)?
            .into_iter()
            .filter_map(|tag| Some((tag_version(&tag)?, tag)))
            .collect();
        tags.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        *self.tags.borrow_mut() = Some(tags);
        Ok(())
    }
    pub(crate) fn source(&self, revision: &str) -> std::rc::Rc<PathSource> {
        self.packages.borrow()[revision].1.clone()
    }
    pub(crate) fn revision(&self, selector: &str) -> Result<String, PackageError> {
        if let Some(revision) = self.refs.borrow().get(selector) {
            return Ok(revision.clone());
        }
        // Full commit IDs are validated on materialization, permitting cached
        // trees to work even when the Git metadata cache has been removed.
        if self.cache.is_some()
            && matches!(selector.len(), 40 | 64)
            && selector.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Ok(selector.to_ascii_lowercase());
        }
        let _guard = self
            .cache
            .as_ref()
            .filter(|cache| !cache.read_only)
            .map(|cache| cache.lock())
            .transpose()?;
        let mut resolved = self.backend.revision(&self.repository, selector);
        if resolved.is_err()
            && matches!(selector.len(), 40 | 64)
            && selector.bytes().all(|c| c.is_ascii_hexdigit())
        {
            if self
                .cache
                .as_ref()
                .is_some_and(|cache| cache.offline || cache.read_only)
            {
                return Err(PackageError::CacheMissingOffline(self.url.as_str().into()));
            }
            self.backend
                .fetch_revision(self.url.as_str(), &self.repository, selector)
                .map_err(|_| PackageError::GitRevisionNotFound {
                    url: self.url.as_str().into(),
                    revision: selector.into(),
                })?;
            resolved = self.backend.revision(&self.repository, selector);
        }
        let revision = resolved.map_err(|_| PackageError::GitRevisionNotFound {
            url: self.url.as_str().into(),
            revision: selector.into(),
        })?;
        if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(PackageError::GitMaterialization(
                "backend returned invalid commit ID".into(),
            ));
        }
        self.refs
            .borrow_mut()
            .insert(selector.into(), revision.clone());
        Ok(revision)
    }
    pub(crate) fn candidates(&self, selector: &GitSelector) -> Result<GitCandidates, PackageError> {
        let cached = self.candidates.borrow().get(selector).cloned();
        let entries = if let Some(entries) = cached {
            entries
        } else {
            let candidates = match selector {
                GitSelector::Version(req) => {
                    self.load_tags()?;
                    self.tags
                        .borrow()
                        .as_ref()
                        .unwrap()
                        .iter()
                        .filter(|(version, _)| {
                            self.candidate_checks.set(self.candidate_checks.get() + 1);
                            req.matches(version)
                        })
                        .map(|(_, tag)| (format!("refs/tags/{tag}"), Some(tag.clone())))
                        .collect()
                }
                GitSelector::Revision(rev) => {
                    if rev.is_empty() || !rev.bytes().all(|c| c.is_ascii_hexdigit()) {
                        return Err(PackageError::GitRevisionNotFound {
                            url: self.url.as_str().into(),
                            revision: rev.clone(),
                        });
                    }
                    vec![(rev.clone(), None)]
                }
                GitSelector::Branch(branch) => vec![(format!("refs/heads/{branch}"), None)],
                GitSelector::Tag(tag) => vec![(format!("refs/tags/{tag}"), Some(tag.clone()))],
            };
            let entries = std::rc::Rc::new(candidates);
            self.candidates
                .borrow_mut()
                .insert(selector.clone(), entries.clone());
            entries
        };
        if entries.is_empty() {
            let GitSelector::Version(req) = selector else {
                unreachable!()
            };
            return Err(PackageError::VersionNotFound {
                url: self.url.as_str().into(),
                requirement: req.to_string(),
            });
        }
        Ok(GitCandidates {
            entries,
            position: 0,
        })
    }
    pub(crate) fn at(
        &self,
        revision: &str,
        tag: Option<&str>,
    ) -> Result<PackageIdentity, PackageError> {
        let cached = self.packages.borrow().get(revision).cloned();
        let identity = if let Some((identity, _)) = cached {
            identity
        } else {
            let root = if let Some(cache) = &self.cache {
                cache.materialize(&self.backend, self.url.as_str(), revision)?
            } else {
                let root = self.checkout_root(revision);
                self.backend.checkout(&self.repository, revision, &root)?;
                root
            };
            let source = PathSource::open(&root, true)?;
            let mut identity = source.identity();
            identity.source = PackageSourceIdentity::Git {
                url: self.url.as_str().into(),
            };
            identity.revision = Some(revision.into());
            self.packages.borrow_mut().insert(
                revision.into(),
                (identity.clone(), std::rc::Rc::new(source)),
            );
            identity
        };
        if let Some(tag) = tag
            && let Some(version) = tag_version(tag)
            && version.to_string() != identity.version
        {
            return Err(PackageError::TagManifestVersionMismatch {
                tag: tag.into(),
                version: identity.version,
            });
        }
        Ok(identity)
    }
}
pub(crate) fn tag_version(tag: &str) -> Option<semver::Version> {
    semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}
impl<B: GitBackend> PackageSource for GitSource<B> {
    type Selector = GitSelector;
    fn versions(&self) -> Result<Vec<semver::Version>, PackageError> {
        self.load_tags()?;
        Ok(self
            .tags
            .borrow()
            .as_ref()
            .unwrap()
            .iter()
            .map(|(v, _)| v.clone())
            .collect())
    }
    fn resolve(&self, selector: &GitSelector) -> Result<PackageIdentity, PackageError> {
        let (reference, tag) = self.candidates(selector)?.next().unwrap();
        self.at(&self.revision(&reference)?, tag.as_deref())
    }
    fn materialize(&self, identity: &PackageIdentity) -> Result<PathBuf, PackageError> {
        if identity.source
            != (PackageSourceIdentity::Git {
                url: self.url.as_str().into(),
            })
        {
            return Err(PackageError::GitMaterialization(
                "source identity mismatch".into(),
            ));
        }
        let revision = identity
            .revision
            .as_ref()
            .ok_or_else(|| PackageError::GitMaterialization("missing revision".into()))?;
        let canonical = self.revision(revision)?;
        if canonical != *revision || self.at(revision, None)? != *identity {
            return Err(PackageError::GitMaterialization(
                "revision identity mismatch".into(),
            ));
        }
        Ok(self.checkout_root(revision))
    }
}

#[cfg(test)]
#[derive(Debug)]
pub struct GitSession(PathBuf);
#[cfg(test)]
impl GitSession {
    pub(crate) fn new() -> Result<Self, PackageError> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        loop {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("willow-git-{}-{n}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(PackageError::GitMaterialization(e.to_string())),
            }
        }
    }
    pub(crate) fn directory(&self, index: usize) -> PathBuf {
        self.0.join(index.to_string())
    }
}
#[cfg(test)]
impl Drop for GitSession {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
