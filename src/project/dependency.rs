use serde::{Deserialize, Deserializer, de::Error};

#[cfg(test)]
thread_local! {
    pub(super) static NORMALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// HTTP(S) source identity strips a terminal `.git` suffix.
/// Local and other transport locators are preserved exactly.
/// It deliberately does not equate SSH and HTTPS or case-fold repository paths.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalGitUrl(String);

impl CanonicalGitUrl {
    pub fn new(url: &str) -> Self {
        let canonical = if url.starts_with("https://") || url.starts_with("http://") {
            url.strip_suffix(".git").unwrap_or(url)
        } else {
            url
        };
        Self(canonical.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Normalized selectors: conflicting raw fields cannot reach a resolver.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GitSelector {
    Version(semver::VersionReq),
    Revision(String),
    Branch(String),
    Tag(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencySource {
    Path {
        path: String,
    },
    Git {
        url: CanonicalGitUrl,
        selector: GitSelector,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDependency {
    path: Option<String>,
    git: Option<String>,
    version: Option<String>,
    rev: Option<String>,
    branch: Option<String>,
    tag: Option<String>,
}

impl<'de> Deserialize<'de> for DependencySource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawDependency::deserialize(deserializer)?;
        #[cfg(test)]
        NORMALIZATIONS.with(|count| count.set(count.get() + 1));
        let selectors = [&raw.version, &raw.rev, &raw.branch, &raw.tag];
        if selectors.iter().filter(|value| value.is_some()).count() > 1 {
            return Err(D::Error::custom(
                "only one of version/rev/branch/tag is allowed",
            ));
        }
        if selectors
            .iter()
            .filter_map(|value| value.as_ref())
            .any(|value| value.trim().is_empty())
        {
            return Err(D::Error::custom("dependency selector must not be empty"));
        }
        match (raw.path, raw.git) {
            (Some(path), None) if !path.trim().is_empty() => {
                if selectors.iter().any(|value| value.is_some()) {
                    return Err(D::Error::custom(
                        "path dependencies cannot have Git selectors",
                    ));
                }
                Ok(Self::Path { path })
            }
            (None, Some(url)) if !url.trim().is_empty() => {
                let selector = if let Some(version) = raw.version {
                    GitSelector::Version(
                        semver::VersionReq::parse(&version).map_err(D::Error::custom)?,
                    )
                } else if let Some(revision) = raw.rev {
                    GitSelector::Revision(revision)
                } else if let Some(branch) = raw.branch {
                    GitSelector::Branch(branch)
                } else if let Some(tag) = raw.tag {
                    GitSelector::Tag(tag)
                } else {
                    GitSelector::Version(semver::VersionReq::STAR)
                };
                Ok(Self::Git {
                    url: CanonicalGitUrl::new(&url),
                    selector,
                })
            }
            _ => Err(D::Error::custom(
                "exactly one nonempty path or git source is required",
            )),
        }
    }
}
