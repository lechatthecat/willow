//! Human-owned native review record, deliberately outside lock regeneration.
use super::*;
use crate::rust_bridge_ir::RustBridgeSymbol;
use std::collections::BTreeMap;

mod dependency_inputs;

pub const RECORD: &str = "rust-review.toml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    fingerprint: String,
}

pub fn recorded(root: &Path) -> Result<Option<String>> {
    let path = root.join(RECORD);
    match fs::read_to_string(&path) {
        Ok(source) => {
            let review: Review = toml::from_str(&source).context("invalid rust-review.toml")?;
            anyhow::ensure!(
                review.fingerprint.len() == 64
                    && review
                        .fingerprint
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
                "invalid Rust review fingerprint"
            );
            Ok(Some(review.fingerprint))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn required<'a>(root: &Path, symbols: impl IntoIterator<Item = &'a RustBridgeSymbol>) -> bool {
    root.join(RECORD).exists() || symbols.into_iter().any(|s| s.no_io || s.no_block)
}

pub fn validate(root: &Path, symbols: &[RustBridgeSymbol], current: &str) -> Result<()> {
    if required(root, symbols) && recorded(root)?.as_deref() != Some(current) {
        bail!(
            "rust_nonblocking_review_required: review all Rust modules and dependencies, then explicitly write fingerprint = \"{current}\" to {RECORD}"
        );
    }
    Ok(())
}

/// Cache admission includes every source that can revoke a recorded review,
/// even sources with no imported declarations. Only names and digests escape
/// this capture; syntax validation remains the review gate's responsibility.
pub(crate) fn source_input_fingerprint(root: &Path) -> Result<Option<String>> {
    if !root.join(RECORD).exists() {
        return Ok(None);
    }
    let mut sources = BTreeMap::new();
    discover_source_hashes(root, &mut sources, hash_file)?;
    Ok(Some(hash(&serde_json::to_vec(&(
        "review-source-inputs-v1",
        sources,
    ))?)))
}

/// Cargo enumerates the complete resolved closure, including transitive path
/// dependencies. Their versions alone cannot identify their mutable sources.
pub(super) fn dependency_fingerprint(project_root: &Path, metadata: &Value) -> Result<String> {
    let packages = metadata["packages"]
        .as_array()
        .context("Cargo packages missing")?;
    let root_id = metadata["resolve"]["root"]
        .as_str()
        .context("Cargo root missing")?;
    let root = packages
        .iter()
        .find(|package| package["id"] == root_id)
        .context("Cargo root package missing")?;
    let root_manifest = Path::new(
        root["manifest_path"]
            .as_str()
            .context("Cargo manifest missing")?,
    );
    let injected = root_manifest
        .parent()
        .context("Cargo manifest directory missing")?
        .join("willow-assume/Cargo.toml");
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .context("Cargo resolved nodes missing")?;
    let mut features_by_id = std::collections::HashMap::new();
    for node in nodes {
        let id = node["id"]
            .as_str()
            .context("Cargo resolved package ID missing")?;
        let mut features = node["features"]
            .as_array()
            .context("Cargo resolved features missing")?
            .iter()
            .map(|feature| feature.as_str().context("invalid Cargo resolved feature"))
            .collect::<Result<Vec<_>>>()?;
        features.sort_unstable();
        features.dedup();
        anyhow::ensure!(
            features_by_id.insert(id, features).is_none(),
            "duplicate Cargo resolved package ID"
        );
    }
    let mut sources = DependencySources::default();
    let mut identities = Vec::new();
    // Visit outer package roots first so shared subtrees have one deterministic
    // reviewed boundary, independently of Cargo's package iteration order.
    let mut ordered: Vec<_> = packages.iter().collect();
    ordered.sort_by_key(|package| {
        package["manifest_path"]
            .as_str()
            .and_then(|path| Path::new(path).parent())
    });
    for package in ordered {
        if package["id"] == root_id {
            continue;
        }
        let manifest = Path::new(
            package["manifest_path"]
                .as_str()
                .context("Cargo manifest missing")?,
        );
        if package["name"] == "willow_assume" && manifest == injected {
            continue;
        }
        let id = package["id"].as_str().context("Cargo package ID missing")?;
        let features = features_by_id
            .get(id)
            .context("Cargo resolved package missing")?;
        let source = match &package["source"] {
            Value::Null => None,
            Value::String(source) => Some(source.as_str()),
            _ => bail!("invalid Cargo package source"),
        };
        let tree = if source.is_none() {
            let directory = manifest
                .parent()
                .context("Cargo manifest directory missing")?;
            validate_dependency_package(directory, package)?;
            Some((
                relative_dependency_path(project_root, directory),
                sources.digest(directory)?,
            ))
        } else {
            None
        };
        identities.push((
            package["name"]
                .as_str()
                .context("Cargo package name missing")?,
            package["version"]
                .as_str()
                .context("Cargo package version missing")?,
            source,
            tree,
            features,
        ));
    }
    identities.sort();
    Ok(hash(&serde_json::to_vec(&identities)?))
}

fn validate_dependency_package(root: &Path, package: &Value) -> Result<()> {
    // Reject private/generated roots before reading any dependency content.
    let excluded = |path: &Path| {
        path.components()
            .any(|component| excluded_dependency_name(&component.as_os_str().to_string_lossy()))
    };
    anyhow::ensure!(
        !excluded(root) && !excluded(&fs::canonicalize(root)?),
        "rust_review_source_layout: dependency root is private or generated"
    );
    if let Some(targets) = package["targets"].as_array() {
        for target in targets {
            let kinds = target["kind"]
                .as_array()
                .context("Cargo target kind missing")?;
            anyhow::ensure!(
                !kinds
                    .iter()
                    .any(|kind| kind == "custom-build" || kind == "proc-macro"),
                "rust_review_source_layout: mutable dependency build scripts and procedural macros have unbounded host inputs"
            );
            let path = Path::new(
                target["src_path"]
                    .as_str()
                    .context("Cargo target source missing")?,
            );
            anyhow::ensure!(
                dependency_inputs::contained(root, path)
                    && path.extension().is_some_and(|extension| extension == "rs"),
                "rust_review_source_layout: dependency target must be a reviewed in-tree Rust source"
            );
        }
    }
    Ok(())
}

fn excluded_dependency_name(name: &str) -> bool {
    [".git", "target", ".willow", ".willow-edits", "requirements"]
        .iter()
        .any(|excluded| name.eq_ignore_ascii_case(excluded))
}

// Bind feature assignments to path-package locations without pinning a review
// to an absolute checkout directory. Tree hashes alone cannot distinguish two
// identical same-name/version packages with different resolved features.
fn relative_dependency_path(root: &Path, directory: &Path) -> String {
    let root: Vec<_> = root.components().collect();
    let directory: Vec<_> = directory.components().collect();
    let common = root
        .iter()
        .zip(&directory)
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        return directory
            .iter()
            .collect::<PathBuf>()
            .to_string_lossy()
            .replace('\\', "/");
    }
    let mut relative = PathBuf::new();
    for _ in &root[common..] {
        relative.push("..");
    }
    for component in &directory[common..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        ".".into()
    } else {
        relative.to_string_lossy().replace('\\', "/")
    }
}

#[derive(Default)]
struct DependencySources {
    // Shared subtree digests avoid rescanning nested workspace package roots.
    directories: std::collections::HashMap<PathBuf, String>,
}
impl DependencySources {
    fn digest(&mut self, root: &Path) -> Result<String> {
        let mut pending = vec![(root.to_owned(), false)];
        let mut entries = std::collections::HashMap::<
            PathBuf,
            BTreeMap<String, (bool, PathBuf, Option<String>)>,
        >::new();
        while let Some((directory, finish)) = pending.pop() {
            if self.directories.contains_key(&directory) {
                continue;
            }
            if finish {
                let children = entries
                    .remove(&directory)
                    .expect("pending dependency directory");
                let children: Vec<_> = children
                    .into_iter()
                    .map(|(name, (is_directory, path, digest))| {
                        (
                            name,
                            is_directory,
                            digest.unwrap_or_else(|| self.directories[&path].clone()),
                        )
                    })
                    .collect();
                self.directories
                    .insert(directory, hash(&serde_json::to_vec(&children)?));
                continue;
            }
            anyhow::ensure!(
                fs::symlink_metadata(&directory)?.is_dir(),
                "rust_review_source_layout: dependency directory is not regular: {}",
                directory.display()
            );
            pending.push((directory.clone(), true));
            let mut children = BTreeMap::new();
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("non-UTF8 dependency input name"))?;
                if excluded_dependency_name(&name) {
                    continue;
                }
                let kind = entry.file_type()?;
                anyhow::ensure!(
                    kind.is_file() || kind.is_dir(),
                    "rust_review_source_layout: symbolic link or special dependency input: {}",
                    entry.path().display()
                );
                let path = entry.path();
                let digest = if kind.is_dir() {
                    pending.push((path.clone(), false));
                    None
                } else {
                    #[cfg(test)]
                    DEPENDENCY_FILES.set(DEPENDENCY_FILES.get() + 1);
                    Some(
                        if path
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
                        {
                            let bytes = fs::read(&path)?;
                            let source = std::str::from_utf8(&bytes).context(
                                "rust_review_source_layout: dependency Rust source is not UTF-8",
                            )?;
                            dependency_inputs::validate(root, &path, source)?;
                            hash(&bytes)
                        } else {
                            hash_file(&path)?
                        },
                    )
                };
                children.insert(name, (kind.is_dir(), path, digest));
            }
            entries.insert(directory, children);
        }
        Ok(self.directories[root].clone())
    }
}

#[cfg(test)]
thread_local! {
    static DEPENDENCY_FILES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn discover_source_hashes(
    root: &Path,
    sources: &mut BTreeMap<String, String>,
    file_hash: impl Fn(&Path) -> Result<String>,
) -> Result<()> {
    let mut pending = vec![root.join("src")];
    while let Some(directory) = pending.pop() {
        if !directory.exists() {
            continue;
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if entry.file_name() != ".willow-edits" {
                    pending.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let key = review_module_path(root, &path);
                if let std::collections::btree_map::Entry::Vacant(entry) = sources.entry(key) {
                    entry.insert(file_hash(&path)?);
                }
            }
        }
    }
    Ok(())
}

pub(super) fn fingerprint(
    root: &Path,
    symbols: &[RustBridgeSymbol],
    lock: &str,
    input: &str,
) -> Result<String> {
    // Captured modules avoid duplicate source reads; discover unimported modules
    // as well so check, build and query share one project-wide review boundary.
    let mut sources = BTreeMap::new();
    for native in symbols.iter().filter_map(|s| s.native.as_ref()) {
        if let Some(source) = &native.source
            && let std::collections::btree_map::Entry::Vacant(entry) =
                sources.entry(review_module_path(root, &native.source_path))
        {
            entry.insert(reviewed_source_hash(&native.source_path, source)?);
        }
    }
    discover_source_hashes(root, &mut sources, reviewed_file_hash)?;
    Ok(hash(&serde_json::to_vec(&(
        "native-review-v3",
        lock,
        input,
        sources,
    ))?))
}

/// Review has a closed source boundary: every Rust module must be inline or
/// independently loaded by Willow. Never issue a fingerprint for an unchecked
/// Rust file loader, even if cfg disables it on this host.
pub(super) fn reviewed_file_hash(path: &Path) -> Result<String> {
    reviewed_source_hash(path, &fs::read_to_string(path)?)
}

fn reviewed_source_hash(path: &Path, source: &str) -> Result<String> {
    // Root and dependency sources share one closed expansion policy. Do not
    // admit a new file-loading mechanism merely because a root denylist did
    // not anticipate its syntax.
    let _visits = dependency_inputs::validate_root(path, source)?;
    #[cfg(test)]
    LAYOUT_VISITS.set(LAYOUT_VISITS.get() + _visits);
    Ok(hash_source_bytes(source.as_bytes()))
}

#[cfg(test)]
thread_local! {
    static LAYOUT_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust_module::NativeBridge;

    #[test]
    fn unified_review_dependency_tree_scan_is_linear_for_nested_package_roots() {
        let root =
            std::env::temp_dir().join(format!("willow-path-review-counts-{}", std::process::id()));
        assert!(!root.exists());
        for count in [1, 8, 24, 48] {
            for chain in [false, true] {
                let mut roots = vec![root.clone()];
                for i in 0..count {
                    let directory = if chain {
                        roots.last().unwrap().join("d")
                    } else {
                        root.join(format!("d{i}"))
                    };
                    fs::create_dir_all(&directory).unwrap();
                    fs::write(directory.join("input.bin"), [i as u8]).unwrap();
                    roots.push(directory);
                }
                let mut expected = None;
                for reverse in [false, true] {
                    if reverse {
                        roots.reverse();
                    }
                    DEPENDENCY_FILES.set(0);
                    let mut sources = DependencySources::default();
                    for directory in &roots {
                        sources.digest(directory).unwrap();
                    }
                    let digest = sources.digest(&root).unwrap();
                    if let Some(expected) = &expected {
                        assert_eq!(&digest, expected);
                    } else {
                        expected = Some(digest);
                    }
                    assert_eq!(DEPENDENCY_FILES.get(), count);
                }
                println!(
                    "dependency trees chain={chain} packages={} file_reads={count}",
                    count + 1
                );
                fs::remove_dir_all(&root).unwrap();
            }
        }
    }

    #[test]
    fn unified_review_binds_path_dependency_sources_without_version_changes() {
        let root = std::env::temp_dir().join(format!("willow-path-review-{}", std::process::id()));
        assert!(!root.exists());
        for path in ["dependency/src", "dependency/nested/src", "transitive/src"] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        for package in ["dependency", "dependency/nested", "transitive"] {
            fs::write(
                root.join(package).join("Cargo.toml"),
                "[package]\nname='dependency'\nversion='1.0.0'",
            )
            .unwrap();
            fs::write(
                root.join(package).join("src/lib.rs"),
                "pub fn value() -> i64 { 1 }",
            )
            .unwrap();
        }
        let package = |name: &str, path: &str| {
            serde_json::json!({
                "id":name, "name":name, "version":"1.0.0", "source":null,
                "manifest_path":root.join(path).join("Cargo.toml"),
            })
        };
        let mut metadata = serde_json::json!({"resolve":{"root":"generated", "nodes":[
            {"id":"dependency","features":[]},
            {"id":"nested","features":[]},
            {"id":"transitive","features":[]}
        ]}, "packages":[
            package("generated", "generated"),
            package("willow_assume", "generated/willow-assume"),
            package("dependency", "dependency"),
            package("nested", "dependency/nested"),
            package("transitive", "transitive"),
        ]});
        let original = dependency_fingerprint(&root, &metadata).unwrap();
        // Including the nested package root never repeats its file reads.
        DEPENDENCY_FILES.set(0);
        assert_eq!(dependency_fingerprint(&root, &metadata).unwrap(), original);
        assert_eq!(DEPENDENCY_FILES.get(), 6);
        metadata["packages"].as_array_mut().unwrap().reverse();
        DEPENDENCY_FILES.set(0);
        assert_eq!(dependency_fingerprint(&root, &metadata).unwrap(), original);
        assert_eq!(DEPENDENCY_FILES.get(), 6);
        for relative in [
            "dependency/src/lib.rs",
            "transitive/src/lib.rs",
            "dependency/nested/Cargo.toml",
        ] {
            let path = root.join(relative);
            let before = fs::read(&path).unwrap();
            fs::write(&path, b"changed input, package version unchanged").unwrap();
            assert_ne!(
                dependency_fingerprint(&root, &metadata).unwrap(),
                original,
                "{relative}"
            );
            fs::write(path, before).unwrap();
        }
        for relative in ["dependency/build.rs", "dependency/arbitrary-input.bin"] {
            let path = root.join(relative);
            fs::write(&path, "fn main() {}\n").unwrap();
            assert_ne!(
                dependency_fingerprint(&root, &metadata).unwrap(),
                original,
                "{relative}"
            );
            fs::remove_file(path).unwrap();
        }
        for excluded in [".git", "target", ".willow", ".willow-edits", "requirements"] {
            let path = root.join("dependency").join(excluded);
            fs::create_dir(&path).unwrap();
            fs::write(path.join("ignored"), "excluded input").unwrap();
        }
        assert_eq!(dependency_fingerprint(&root, &metadata).unwrap(), original);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                root.join("transitive/src/lib.rs"),
                root.join("dependency/source-link"),
            )
            .unwrap();
            assert!(
                dependency_fingerprint(&root, &metadata)
                    .unwrap_err()
                    .to_string()
                    .contains("rust_review_source_layout")
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unified_review_rejects_unbounded_dependency_targets_and_roots() {
        let root =
            std::env::temp_dir().join(format!("willow-dependency-boundary-{}", std::process::id()));
        assert!(!root.exists());
        fs::create_dir_all(root.join("src")).unwrap();
        let target = |kind: &str, path: &str| {
            serde_json::json!({
                "targets": [{"kind":[kind], "src_path": root.join(path)}]
            })
        };
        assert!(validate_dependency_package(&root, &target("lib", "src/lib.rs")).is_ok());
        for (kind, path) in [
            ("custom-build", "build.rs"),
            ("proc-macro", "src/lib.rs"),
            ("lib", "../outside.rs"),
            ("lib", "src/lib.txt"),
            ("lib", "target/generated.rs"),
            ("lib", "requirements/private.rs"),
        ] {
            assert!(
                validate_dependency_package(&root, &target(kind, path)).is_err(),
                "{kind} {path}"
            );
        }
        for name in ["target", ".git", ".willow", ".willow-edits", "requirements"] {
            // Rejection occurs without traversing or reading protected contents.
            let directory = root.join(name).join("dependency");
            fs::create_dir_all(&directory).unwrap();
            assert!(validate_dependency_package(&directory, &serde_json::json!({})).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unified_review_binds_resolved_features_to_each_dependency_identity() {
        let root =
            std::env::temp_dir().join(format!("willow-feature-review-{}", std::process::id()));
        assert!(!root.exists());
        for package in ["first", "second"] {
            fs::create_dir_all(root.join(package)).unwrap();
            fs::write(
                root.join(package).join("Cargo.toml"),
                "[package]\nname='same'\nversion='1.0.0'\n",
            )
            .unwrap();
            fs::write(root.join(package).join("lib.rs"), "pub fn value() {}\n").unwrap();
        }
        fs::create_dir(root.join("generated")).unwrap();
        let lock = b"unchanged dependency versions and checksums";
        fs::write(root.join("generated/Cargo.lock"), lock).unwrap();
        let package = |id: &str, path: &str, source: Option<&str>| {
            serde_json::json!({
                "id":id,"name":"same","version":"1.0.0","source":source,
                "manifest_path":root.join(path).join("Cargo.toml")
            })
        };
        let original = serde_json::json!({
            "packages":[
                package("generated", "generated", None),
                package("first", "first", None),
                package("second", "second", None),
                package("registry", "absent-registry", Some("registry+https://example.invalid/index")),
                package("git", "absent-git", Some("git+https://example.invalid/dependency#1234"))
            ],
            "resolve":{"root":"generated","nodes":[
                {"id":"first","features":["alpha","default"]},
                {"id":"second","features":["beta"]},
                {"id":"registry","features":["default"]},
                {"id":"git","features":["default"]}
            ]}
        });
        let digest = dependency_fingerprint(&root, &original).unwrap();
        // Workspace-inherited feature changes may leave package trees and the
        // lockfile unchanged. Every resolved package must still revoke review.
        for index in 0..4 {
            let mut changed = original.clone();
            changed["resolve"]["nodes"][index]["features"] = serde_json::json!(["changed"]);
            assert_ne!(
                dependency_fingerprint(&root, &changed).unwrap(),
                digest,
                "package {index}"
            );
        }
        // These path packages have identical names, versions and file bytes.
        // Swapping their feature sets must not preserve the reviewed identity.
        let mut swapped = original.clone();
        swapped["resolve"]["nodes"][0]["features"] =
            original["resolve"]["nodes"][1]["features"].clone();
        swapped["resolve"]["nodes"][1]["features"] =
            original["resolve"]["nodes"][0]["features"].clone();
        assert_ne!(dependency_fingerprint(&root, &swapped).unwrap(), digest);
        let mut reordered = original.clone();
        reordered["resolve"]["nodes"][0]["features"] =
            serde_json::json!(["default", "alpha", "alpha"]);
        reordered["resolve"]["nodes"]
            .as_array_mut()
            .unwrap()
            .reverse();
        reordered["packages"].as_array_mut().unwrap().reverse();
        assert_eq!(dependency_fingerprint(&root, &reordered).unwrap(), digest);
        for index in [3, 4] {
            let mut changed = original.clone();
            changed["packages"][index]["source"] = serde_json::json!("different-source");
            assert_ne!(dependency_fingerprint(&root, &changed).unwrap(), digest);
        }
        let mut missing = original.clone();
        missing["resolve"]["nodes"].as_array_mut().unwrap().pop();
        assert!(dependency_fingerprint(&root, &missing).is_err());
        assert_eq!(fs::read(root.join("generated/Cargo.lock")).unwrap(), lock);
        assert_eq!(
            relative_dependency_path(&root.join("project"), &root.join("sibling")),
            "../sibling"
        );
        assert_eq!(
            relative_dependency_path(&root, &root.join("first")),
            "first"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unified_review_source_inputs_track_imported_and_unimported_modules() {
        let root = std::env::temp_dir().join(format!(
            "willow-review-source-inputs-{}",
            std::process::id()
        ));
        assert!(!root.exists());
        fs::create_dir_all(root.join("src/nested")).unwrap();
        // No review means no Rust scan or manifest parse, even for invalid text.
        fs::write(root.join("src/nested/unused.rs"), "{invalid").unwrap();
        assert_eq!(source_input_fingerprint(&root).unwrap(), None);
        fs::write(
            root.join("project.toml"),
            "[project]\nname='review_inputs'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(root.join("src/native.rs"), "pub fn answer() -> i64 { 1 }").unwrap();
        fs::write(
            root.join(RECORD),
            format!("fingerprint='{}'", "0".repeat(64)),
        )
        .unwrap();
        let initial = source_input_fingerprint(&root).unwrap().unwrap();
        // Cache admission hashes bytes without swallowing invalid-source errors
        // from the later semantic/review pass or publishing the source text.
        fs::write(root.join("src/nested/unused.rs"), "pub fn unused() {}").unwrap();
        let edited = source_input_fingerprint(&root).unwrap().unwrap();
        assert_ne!(edited, initial);
        fs::write(root.join("src/added.rs"), "pub fn added() {}").unwrap();
        let added = source_input_fingerprint(&root).unwrap().unwrap();
        assert_ne!(added, edited);
        fs::remove_file(root.join("src/added.rs")).unwrap();
        assert_eq!(source_input_fingerprint(&root).unwrap().unwrap(), edited);
        fs::write(root.join("src/native.rs"), "pub fn answer() -> i64 { 2 }").unwrap();
        let module = source_input_fingerprint(&root).unwrap().unwrap();
        assert_ne!(module, edited);
        fs::rename(
            root.join("src/nested/unused.rs"),
            root.join("src/renamed.rs"),
        )
        .unwrap();
        let renamed = source_input_fingerprint(&root).unwrap().unwrap();
        assert_ne!(renamed, module);
        fs::remove_file(root.join("src/renamed.rs")).unwrap();
        assert_ne!(source_input_fingerprint(&root).unwrap().unwrap(), renamed);
        fs::remove_file(root.join(RECORD)).unwrap();
        assert_eq!(source_input_fingerprint(&root).unwrap(), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unified_review_required_for_native_effect_claims() {
        let root = std::env::temp_dir().join(format!(
            "willow-unified-review-absent-{}",
            std::process::id()
        ));
        assert!(!root.exists());
        let mut symbol =
            RustBridgeSymbol::new("answer".into(), vec![], crate::rust_bridge_ir::Scalar::I64);
        symbol.native = Some(NativeBridge {
            module: "native".into(),
            source_path: root.join("src/native.rs"),
            source: None,
            adapter: String::new(),
            target: String::new(),
            definitions: vec![],
        });
        assert!(!required(&root, [&symbol]));
        symbol.declare_no_block();
        assert!(required(&root, [&symbol]));
        symbol.no_io = true;
        assert!(required(&root, [&symbol]));
        symbol.no_block = false;
        assert!(required(&root, [&symbol]));
        symbol.no_io = false;
        assert!(!required(&root, [&symbol]));
    }

    #[test]
    fn unified_review_hashes_captured_sources_once_per_module() {
        let root = std::env::temp_dir().join(format!(
            "willow-unified-review-scaling-{}",
            std::process::id()
        ));
        assert!(!root.exists());
        for count in [1, 16, 128, 512] {
            let source = (0..count)
                .map(|i| format!("pub fn operation_{i}() {{}}\n"))
                .collect::<String>();
            let mut symbol = RustBridgeSymbol::new(
                "operation".into(),
                vec![],
                crate::rust_bridge_ir::Scalar::Void,
            );
            symbol.native = Some(NativeBridge {
                module: "native".into(),
                source_path: root.join("src/native.rs"),
                source: Some(source),
                adapter: String::new(),
                target: String::new(),
                definitions: vec![],
            });
            LAYOUT_VISITS.set(0);
            let expected = fingerprint(&root, &[symbol.clone()], "lock", "input").unwrap();
            let once = LAYOUT_VISITS.get();
            LAYOUT_VISITS.set(0);
            let actual = fingerprint(&root, &vec![symbol; count], "lock", "input").unwrap();
            assert_eq!(actual, expected);
            assert_eq!(LAYOUT_VISITS.get(), once);
            assert_eq!(once, 5 * count);
            println!("review exports={count} source_token_visits={once}");
        }
    }

    #[test]
    fn native_review_source_layout_rejects_file_loading_in_all_contexts() {
        for source in [
            "mod helper;",
            "macro_rules! value { ($x:expr) => { $x + 1 }; } pub fn f() -> i64 { value!(41) }",
            "macro_rules! inline { () => { mod child { pub fn f() {} } }; } inline!();",
            "#[path = \"../helper.rs\"] mod helper;",
            "#[cfg(windows)] #[path = \"../helper.rs\"] mod helper;",
            "#[cfg_attr(windows, path = \"../helper.rs\")] mod helper;",
            "mod inline { mod child; }",
            "fn f() { #[path = \"../helper.rs\"] mod local; }",
            "include!(\"../helper.rs\");",
            "r#include!(\"../helper.rs\");",
            "use std::r#include as load; load!(\"../helper.rs\");",
            "fn f() { let _ = include_str!(\"../helper.rs\"); }",
            "fn f() { let _ = ::std::include_bytes!(\"../data.bin\"); }",
            "use std::include as load; load!(\"../helper.rs\");",
            "use std::{include_str as load}; fn f() { let _ = load!(\"../helper.rs\"); }",
            "macro_rules! load { ($name:ident) => { mod $name; }; } load!(helper);",
            "macro_rules! load { () => { include!(\"../helper.rs\"); }; } load!();",
            "fn f() { mod local { mod deeper; } }",
        ] {
            let error = reviewed_source_hash(Path::new("native.rs"), source).unwrap_err();
            assert!(
                error.to_string().contains("rust_review_source_layout"),
                "{source}: {error:#}"
            );
        }
        for source in [
            "mod helper { pub fn answer() -> i64 { 42 } }",
            "fn f() { mod local { pub fn value() {} } local::value(); }",
            "fn f() { println!(\"mod helper; include!(file)\"); let _ = vec![1, 2]; }",
            "fn f() { let include = 0; if include != 1 { let include = 2; } if !false { let include = 3; } }",
            "// mod helper;\nconst TEXT: &str = \"include!(file)\";",
        ] {
            assert_eq!(
                reviewed_source_hash(Path::new("native.rs"), source).unwrap(),
                hash_source_bytes(source.as_bytes())
            );
        }
    }

    #[test]
    fn native_review_source_layout_rejects_root_native_links() {
        for source in [
            "#[link(name=\"payload\", kind=\"static\")] unsafe extern \"C\" { fn value(); }",
            "#[r#link(name=\"payload\")] unsafe extern \"C\" {}",
            "#[link(name=\"payload\", kind=\"dylib\")] unsafe extern \"C\" {}",
            "#[cfg(windows)] #[link(name=\"payload\")] unsafe extern \"C\" {}",
            "#[cfg_attr(windows, link(name=\"payload\"))] unsafe extern \"C\" {}",
            "#[cfg_attr(unix, cfg_attr(windows, link(name=\"payload\")))] unsafe extern \"C\" {}",
            "mod inner { #[link(name=\"payload\")] unsafe extern \"C\" {} }",
            "macro_rules! native { () => { #[link(name=\"payload\")] unsafe extern \"C\" {} }; }",
            "macro_rules! attach { ($attribute:meta) => { #[$attribute] unsafe extern \"C\" {} }; } attach!(link(name=\"payload\"));",
        ] {
            let error = reviewed_source_hash(Path::new("bridge.rs"), source).unwrap_err();
            assert!(
                error.to_string().contains("rust_review_source_layout"),
                "{source}: {error:#}"
            );
        }
        for source in [
            "pub fn link() -> i64 { 42 }",
            "pub fn value() -> i64 { let link = 42; link }",
            "unsafe extern \"C\" { #[link_name=\"value\"] fn native_value(); }",
            "#[doc=\"link(name=payload)\"] pub fn value() {}",
        ] {
            assert_eq!(
                reviewed_source_hash(Path::new("bridge.rs"), source).unwrap(),
                hash_source_bytes(source.as_bytes())
            );
        }
    }

    #[test]
    fn native_review_source_layout_visits_scale_with_syntax() {
        for count in [1, 16, 128, 512] {
            let source: String = (0..count)
                .map(|i| format!("mod helper_{i} {{ pub fn answer() -> i64 {{ 42 }} }}\n"))
                .collect();
            LAYOUT_VISITS.set(0);
            reviewed_source_hash(Path::new("native.rs"), &source).unwrap();
            assert_eq!(LAYOUT_VISITS.get(), 12 * count);
            let source = format!("fn values() {{ vec![{}]; }}", "0,".repeat(count));
            LAYOUT_VISITS.set(0);
            reviewed_source_hash(Path::new("native.rs"), &source).unwrap();
            assert_eq!(LAYOUT_VISITS.get(), 2 * count + 8);
            println!(
                "layout count={count}: inline tokens={}, macro tokens={}",
                12 * count,
                2 * count + 8
            );
        }
    }

    #[test]
    fn native_review_source_layout_nested_modules_visit_once() {
        for depth in [1, 16, 64] {
            let source = format!(
                "{}fn leaf() {{}}{}",
                "mod nested {".repeat(depth),
                "}".repeat(depth)
            );
            LAYOUT_VISITS.set(0);
            reviewed_source_hash(Path::new("native.rs"), &source).unwrap();
            assert_eq!(LAYOUT_VISITS.get(), 3 * depth + 4);
            println!("layout depth={depth}: inline tokens={}", 3 * depth + 4);
        }
    }

    #[test]
    fn native_review_platform_paths_line_endings_and_order_are_portable() {
        let mut expected = None;
        // Model all four CI environments without executing foreign binaries.
        for (root, slash, newline) in [
            ("/nonexistent-linux/project", "/", "\n"),
            (r"Z:\nonexistent-windows\project", "\\", "\r\n"),
            ("/nonexistent-macos-arm/project", "/", "\n"),
            ("/nonexistent-macos-intel/project", "/", "\n"),
        ] {
            let symbols: Vec<_> = ["z", "a"]
                .iter()
                .map(|name| {
                    let mut s = RustBridgeSymbol::new(
                        (*name).into(),
                        vec![],
                        crate::rust_bridge_ir::Scalar::Void,
                    );
                    s.native = Some(NativeBridge {
                        module: format!("{root}_{name}"),
                        source_path: PathBuf::from(format!("{root}{slash}src{slash}{name}.rs")),
                        source: Some(format!(
                            "#[willow::assume(no_block)]{newline}pub fn f() {{}}{newline}"
                        )),
                        adapter: String::new(),
                        target: String::new(),
                        definitions: vec![],
                    });
                    s
                })
                .collect();
            let value =
                fingerprint(Path::new(root), &symbols, "same Cargo lock", "same inputs").unwrap();
            assert_eq!(
                value,
                fingerprint(
                    Path::new(root),
                    &symbols.iter().rev().cloned().collect::<Vec<_>>(),
                    "same Cargo lock",
                    "same inputs",
                )
                .unwrap()
            );
            if let Some(expected) = &expected {
                assert_eq!(&value, expected);
            } else {
                expected = Some(value);
            }
        }
    }
}
