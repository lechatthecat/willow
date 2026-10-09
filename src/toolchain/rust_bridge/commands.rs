//! Rust dependency edits reuse the comment-preserving package editor. Cargo is
//! the only resolver; candidate manifests are resolved before publishing edits.
use super::*;
use anyhow::ensure;
use serde_json::json;
use toml_edit::{DocumentMut, Item};

#[derive(Debug)]
pub enum Mutation {
    Add {
        name: String,
        spec: crate::project::RustDependencySpec,
    },
    Remove {
        name: String,
    },
    Update {
        name: Option<String>,
        breaking: bool,
    },
}

fn table(doc: &mut DocumentMut) -> Result<&mut dyn toml_edit::TableLike> {
    if !doc.contains_key("rust-dependencies") {
        doc["rust-dependencies"] = Item::Table(toml_edit::Table::new());
    }
    doc["rust-dependencies"]
        .as_table_like_mut()
        .context("rust-dependencies must be a table")
}

fn set_version(item: &mut Item, version: &str) -> Result<()> {
    if let Some(value) = item.as_value_mut().filter(|v| v.is_str()) {
        let decor = value.decor().clone();
        *value = toml_edit::Value::from(version);
        *value.decor_mut() = decor;
    } else {
        let fields = item
            .as_table_like_mut()
            .context("invalid Rust dependency entry")?;
        let value = fields
            .get_mut("version")
            .context("registry version missing")?;
        set_version(value, version)?;
    }
    Ok(())
}

fn parsed(doc: &DocumentMut, original: &str) -> Result<(ProjectManifest, String)> {
    let text = crate::package::commands::render_manifest(doc, original);
    let project: ProjectManifest = toml::from_str(&text)?;
    project.validate()?;
    Ok((project, text))
}

/// Serialize edits from different cache roots too. Resolution holds the existing
/// bridge/project leases through publication; failed resolution leaves user files intact.
pub fn mutate(root: &Path, options: &BridgeOptions, mutation: Mutation) -> Result<Value> {
    ensure!(
        !options.locked,
        "Rust dependency edits cannot use --locked or --frozen"
    );
    fs::create_dir_all(root.join(".willow/rust"))?;
    let edit_lease = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".willow/rust/.edit.lock"))?;
    edit_lease.lock()?;
    let path = root.join("project.toml");
    let before = fs::read_to_string(&path)?;
    let mut doc: DocumentMut = before.parse()?;
    let original = ProjectManifest::load(&path)?;
    let (operation, name, update, breaking) = match &mutation {
        Mutation::Add { name, spec } => {
            spec.normalize(name)?;
            ensure!(
                !original.rust_dependencies.contains_key(name),
                "Rust dependency `{name}` already exists"
            );
            let mut entry = toml_edit::InlineTable::new();
            for (key, value) in [
                ("version", &spec.version),
                ("git", &spec.git),
                ("rev", &spec.rev),
                ("tag", &spec.tag),
                ("path", &spec.path),
            ] {
                if let Some(value) = value {
                    entry.insert(key, value.as_str().into());
                }
            }
            ensure!(
                spec.features.is_empty() && spec.default_features.is_none(),
                "feature options are not supported by rust add"
            );
            table(&mut doc)?.insert(name, toml_edit::value(entry));
            ("add", Some(name.as_str()), None, false)
        }
        Mutation::Remove { name } => {
            ensure!(
                table(&mut doc)?.remove(name).is_some(),
                "unknown Rust dependency `{name}`"
            );
            ("remove", Some(name.as_str()), None, false)
        }
        Mutation::Update { name, breaking } => {
            if let Some(name) = name {
                ensure!(
                    original.rust_dependencies.contains_key(name),
                    "unknown Rust dependency `{name}`"
                );
            }
            ("update", name.as_deref(), Some(name.as_deref()), *breaking)
        }
    };
    // Resolve a named selector against the original direct graph, before
    // relaxing requirements. Bare names can match multiple transitive versions.
    // Keep this candidate lock with its package ID so Cargo sees the same graph.
    let named_update = if let Some(Some(name)) = update {
        let build = prepare_bridge(
            &original,
            root,
            options,
            BridgeAction::Edit {
                update: None,
                lock: EditLock::Persisted,
            },
        )?
        .unwrap();
        let id = build
            .direct_dependencies
            .as_array()
            .unwrap()
            .iter()
            .find(|dependency| dependency["alias"] == name)
            .and_then(|dependency| dependency["id"].as_str())
            .context("Cargo direct dependency package ID missing")?
            .to_owned();
        Some((id, fs::read(build.directory.join("Cargo.lock"))?))
    } else {
        None
    };
    let update = match &named_update {
        Some((id, _)) => Some(Some(id.as_str())),
        None => update,
    };
    // Relax selected registry requirements only; Git selectors remain explicit.
    let selected: Vec<_> = original
        .rust_dependencies
        .iter()
        .filter(|(alias, spec)| {
            breaking && name.is_none_or(|name| name == alias.as_str()) && spec.version.is_some()
        })
        .map(|(alias, _)| alias.clone())
        .collect();
    for alias in &selected {
        set_version(table(&mut doc)?.get_mut(alias).unwrap(), "*")?;
    }
    if named_update.is_some() && !selected.is_empty() {
        // A wildcard plus a targeted Cargo update can reuse another locked
        // version of this package (even an older transitive one). Discover
        // Cargo's preferred compatible graph without pins, then update only
        // the selected original package against the original candidate lock.
        let (project, _) = parsed(&doc, &before)?;
        let preferred = prepare_bridge(
            &project,
            root,
            options,
            BridgeAction::Edit {
                update: None,
                lock: EditLock::Fresh,
            },
        )?
        .unwrap();
        let versions = direct_versions(&preferred.direct_dependencies)?;
        for alias in &selected {
            set_version(table(&mut doc)?.get_mut(alias).unwrap(), &versions[alias])?;
        }
    }
    let (project, _) = parsed(&doc, &before)?;
    let mut build = prepare_bridge(
        &project,
        root,
        options,
        BridgeAction::Edit {
            update,
            lock: named_update
                .as_ref()
                .map_or(EditLock::Persisted, |(_, bytes)| EditLock::Candidate(bytes)),
        },
    )?
    .unwrap();
    if named_update.is_none() && !selected.is_empty() {
        let versions = direct_versions(&build.direct_dependencies)?;
        for alias in &selected {
            set_version(table(&mut doc)?.get_mut(alias).unwrap(), &versions[alias])?;
        }
        // Snapshot under the lease, then explicitly seed the final pass. The
        // persisted lock still contains the old pins until the edit commits.
        let candidate_lock = fs::read(build.directory.join("Cargo.lock"))?;
        drop(build);
        let (project, _) = parsed(&doc, &before)?;
        build = prepare_bridge(
            &project,
            root,
            options,
            BridgeAction::Edit {
                update: None,
                lock: EditLock::Candidate(&candidate_lock),
            },
        )?
        .unwrap();
    }
    let (_, after) = parsed(&doc, &before)?;
    ensure!(
        fs::read_to_string(&path)? == before,
        "project changed while resolving; retry the command"
    );
    let lock_paths = [
        root.join("project.lock"),
        root.join(".willow/rust/Cargo.lock"),
    ];
    let saved = lock_paths
        .iter()
        .map(|p| match fs::read(p) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    if before != after {
        crate::package::commands::replace_manifest(&path, &after)?;
    }
    if let Err(error) = build.persist_resolution(root, options) {
        crate::package::commands::replace_manifest(&path, &before)
            .context("manifest rollback failed")?;
        for (path, bytes) in lock_paths.iter().zip(&saved) {
            if let Some(bytes) = bytes {
                crate::package::lock::atomic_write_validated(path, bytes, || Ok(()))?;
            } else if path.exists() {
                fs::remove_file(path)?;
            }
        }
        return Err(error);
    }
    // A candidate, deliberately not an assertion of semantic liveness: Rust
    // macros and generated code preclude proving unused declarations textually.
    let diagnostics = if operation == "remove" {
        json!([{"kind":"rust_bridge_unused_declaration_candidate", "dependency":name,
            "source":original.rust.as_ref().map(|r| &r.bridge),
            "message":"Review bridge declarations after removing this dependency; declarations may now be unused or refer to the removed crate."}])
    } else {
        json!([])
    };
    Ok(
        json!({"schema":1,"ok":true,"kind":format!("rust.{operation}"),"enabled":!build.direct_dependencies.as_array().unwrap().is_empty(),
        "dependency":name,"manifest_changed":before != after,"direct_dependencies":build.direct_dependencies,
        "bridge":{"manifest":build.directory.join("Cargo.toml")},"diagnostics":diagnostics}),
    )
}

/// Preserve Cargo's graph rather than recursively duplicating shared subgraphs.
/// Human output is an adjacency list: every package and edge appears once.
pub fn tree(metadata: &Value) -> Value {
    json!({"root":metadata["resolve"]["root"], "packages":metadata["packages"], "nodes":metadata["resolve"]["nodes"]})
}

pub fn display_tree(tree: &Value) -> Result<String> {
    use std::fmt::Write;
    let packages: std::collections::HashMap<_, _> = tree["packages"]
        .as_array()
        .context("missing Cargo packages")?
        .iter()
        .map(|p| (p["id"].as_str().unwrap_or_default(), p))
        .collect();
    let mut text = String::new();
    for node in tree["nodes"].as_array().context("missing Cargo nodes")? {
        let id = node["id"].as_str().context("missing Cargo node id")?;
        let p = packages.get(id).context("missing Cargo package")?;
        writeln!(
            text,
            "{} {} [{}]",
            p["name"].as_str().unwrap_or_default(),
            p["version"].as_str().unwrap_or_default(),
            id
        )?;
        for edge in node["deps"].as_array().context("missing Cargo edges")? {
            writeln!(
                text,
                "  {} -> {}",
                edge["name"].as_str().unwrap_or_default(),
                edge["pkg"].as_str().unwrap_or_default()
            )?;
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_output_counts_each_node_and_edge_once_on_adverse_graphs() {
        for count in [16, 64, 256, 1024] {
            for shape in ["chain", "fanout", "shared"] {
                let packages: Vec<_> = (0..count).map(|i| json!({"id":format!("p{i}"),"name":format!("dep{i}"),"version":"1.0.0"})).collect();
                let mut edges = 0;
                let nodes: Vec<_> = (0..count)
                    .map(|i| {
                        let targets: Vec<_> = match shape {
                            "fanout" if i == 0 => (1..count).collect(),
                            "chain" if i + 1 < count => vec![i + 1],
                            "shared" if i + 2 < count => vec![i + 1, count - 1],
                            _ => vec![],
                        };
                        edges += targets.len();
                        let deps: Vec<_> = targets
                            .iter()
                            .map(|j| json!({"name":format!("dep{j}"),"pkg":format!("p{j}")}))
                            .collect();
                        json!({"id":format!("p{i}"),"deps":deps})
                    })
                    .collect();
                let graph =
                    tree(&json!({"packages":packages,"resolve":{"root":"p0","nodes":nodes}}));
                let text = display_tree(&graph).unwrap();
                assert_eq!(graph["nodes"].as_array().unwrap().len(), count);
                assert_eq!(text.lines().filter(|l| l.starts_with("  ")).count(), edges);
                assert_eq!(text.lines().count(), count + edges);
                eprintln!(
                    "rust_tree shape={shape} packages={count} edges={edges} lines={}",
                    text.lines().count()
                );
            }
        }
    }

    #[test]
    fn version_edits_preserve_comments_features_crlf_and_no_final_newline() {
        for entry in [
            "dep = \"1\" # retained",
            "dep = { version = \"1\", features = [\"extra\"], default-features = false } # retained",
            "[rust-dependencies.dep]\nversion = \"1\" # retained\nfeatures = [\"extra\"]",
        ] {
            let source = if entry.starts_with('[') {
                entry.to_owned()
            } else {
                format!("[rust-dependencies]\n{entry}")
            };
            for crlf in [false, true] {
                let source = if crlf {
                    source.replace('\n', "\r\n")
                } else {
                    source.clone()
                };
                let mut doc: DocumentMut = source.parse().unwrap();
                set_version(table(&mut doc).unwrap().get_mut("dep").unwrap(), "2.0.0").unwrap();
                let result = crate::package::commands::render_manifest(&doc, &source);
                assert_eq!(result, source.replace("\"1\"", "\"2.0.0\""));
            }
        }
    }
}
