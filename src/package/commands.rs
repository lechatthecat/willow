//! Package mutations are planned against an in-memory manifest before publication.
use super::{GitSource, PackageGraph, PackageSource, PathSource, SystemGit};
use crate::project::{CanonicalGitUrl, DependencySource, GitSelector, ProjectManifest};
use anyhow::{Context, Result, ensure};
use std::{collections::HashMap, io::Write, path::Path};
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value};

mod display;
pub use display::display_dependencies;

#[derive(Debug)]
pub enum PackageMutation {
    Add {
        alias: Option<String>,
        git: Option<String>,
        path: Option<String>,
        version: Option<String>,
    },
    Remove {
        alias: String,
    },
    Update {
        alias: Option<String>,
        breaking: bool,
    },
}

fn derived_alias(name: &str) -> String {
    let mut alias: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if alias.is_empty() || alias.as_bytes()[0].is_ascii_digit() {
        alias.insert(0, '_');
    }
    if alias == "std" {
        alias.push('_');
    }
    alias
}

fn dependencies(doc: &mut DocumentMut) -> Result<&mut dyn toml_edit::TableLike> {
    if !doc.contains_key("dependencies") {
        // A dotted root entry disappears cleanly on removal. In particular, do
        // not invent an explicit empty table that cannot be distinguished from
        // a user's pre-existing empty [dependencies] table on a later remove.
        let mut table = Table::new();
        table.set_dotted(true);
        table.set_implicit(true);
        doc.insert("dependencies", Item::Table(table));
    }
    doc["dependencies"]
        .as_table_like_mut()
        .context("dependencies must be a table")
}

fn git_metadata<'a>(
    sources: &'a mut HashMap<String, GitSource>,
    url: &CanonicalGitUrl,
    dry_run: bool,
) -> Result<&'a GitSource> {
    if !sources.contains_key(url.as_str()) {
        sources.insert(
            url.as_str().into(),
            GitSource::cached_mode(url.clone(), SystemGit, false, false, None, dry_run)?,
        );
    }
    Ok(&sources[url.as_str()])
}

/// Dry-run is strictly read-only, including the global cache and its lock files.
/// Git previews use available cached metadata; a cold cache produces an error.
pub fn mutate_packages(
    root: &Path,
    mutation: PackageMutation,
    dry_run: bool,
    out: &mut impl Write,
) -> Result<()> {
    let mut source = PathSource::open(root, false)?;
    let manifest_path = source.root.join("project.toml");
    let before = std::fs::read_to_string(&manifest_path)?;
    let lock_path = source.root.join("project.lock");
    let lock_before = read_optional(&lock_path)?;
    let mut doc: DocumentMut = before.parse()?;
    let update = match &mutation {
        PackageMutation::Update { alias, .. } => Some(alias.as_deref()),
        _ => None,
    };
    let (mut pins, mut checksums) = super::lock::command_pins(
        &source.root,
        update,
        matches!(mutation, PackageMutation::Add { .. }),
    )?;
    let mut sources = HashMap::new();
    let mut descriptions = Vec::new();
    let mut manifest_changed = false;
    match mutation {
        PackageMutation::Add {
            alias,
            git,
            path,
            version,
        } => {
            ensure!(
                git.is_some() ^ path.is_some(),
                "add requires exactly one --git or --path source"
            );
            ensure!(
                path.is_none() || version.is_none(),
                "--version requires --git"
            );
            manifest_changed = true;
            let mut value = InlineTable::new();
            let name;
            if let Some(path) = path {
                let dependency = PathSource::open(&source.root.join(&path), true)?;
                name = dependency.manifest.project.name.clone();
                value.insert("path", Value::from(path));
            } else {
                let url = CanonicalGitUrl::new(git.as_deref().unwrap());
                let selector = GitSelector::Version(version.as_deref().unwrap_or("*").parse()?);
                let metadata = git_metadata(&mut sources, &url, dry_run)?;
                let identity = metadata.resolve(&selector)?;
                name = identity.name;
                let requirement = version.unwrap_or_else(|| format!("^{}", identity.version));
                value.insert("git", Value::from(url.as_str()));
                value.insert("version", Value::from(requirement));
                if pins.contains_key(url.as_str()) {
                    // Metadata discovery may examine newer tags. Resolve the
                    // pinned instance separately with its locked checksum.
                    sources.remove(url.as_str());
                }
            }
            let alias = alias.unwrap_or_else(|| derived_alias(&name));
            ensure!(
                !source.manifest.dependencies.contains_key(&alias),
                "dependency `{alias}` already exists"
            );
            value.fmt();
            descriptions.push(format!("Add {alias} = {value}"));
            let implicit_parent = doc
                .get("dependencies")
                .and_then(Item::as_table)
                .is_some_and(|table| table.is_implicit() && !table.is_dotted());
            let item = if implicit_parent {
                Item::Table(value.into_table())
            } else {
                Item::Value(Value::InlineTable(value))
            };
            dependencies(&mut doc)?.insert(&alias, item);
        }
        PackageMutation::Remove { alias } => {
            ensure!(
                source.manifest.dependencies.contains_key(&alias),
                "unknown dependency `{alias}`"
            );
            manifest_changed = true;
            dependencies(&mut doc)?.remove(&alias);
            if doc["dependencies"]
                .as_table()
                .is_some_and(|t| t.is_dotted() && t.is_empty())
            {
                doc.remove("dependencies");
            }
            descriptions.push(format!("Remove {alias}"));
        }
        PackageMutation::Update { alias, breaking } => {
            if let Some(alias) = &alias {
                ensure!(
                    source.manifest.dependencies.contains_key(alias),
                    "unknown dependency `{alias}`"
                );
            }
            // One pass over root edges. Unselected sources keep their exact pins.
            for (name, dependency) in &source.manifest.dependencies {
                if alias.as_ref().is_some_and(|alias| alias != name) {
                    continue;
                }
                if let DependencySource::Git { url, selector } = dependency {
                    pins.remove(url.as_str());
                    checksums.remove(url.as_str());
                    if breaking && matches!(selector, GitSelector::Version(_)) {
                        let metadata = git_metadata(&mut sources, url, dry_run)?;
                        let identity =
                            metadata.resolve(&GitSelector::Version(semver::VersionReq::STAR))?;
                        let requirement = format!("^{}", identity.version);
                        let item = dependencies(&mut doc)?
                            .get_mut(name)
                            .context("missing dependency")?;
                        let table = item
                            .as_table_like_mut()
                            .context("dependency must be a table")?;
                        let old = table
                            .get("version")
                            .and_then(Item::as_str)
                            .unwrap_or("*")
                            .to_owned();
                        if old == requirement {
                            continue;
                        }
                        manifest_changed = true;
                        if let Some(value) = table.get_mut("version").and_then(Item::as_value_mut) {
                            let decor = value.decor().clone();
                            *value = Value::from(requirement.clone());
                            *value.decor_mut() = decor;
                        } else {
                            table.insert("version", toml_edit::value(requirement.clone()));
                        }
                        descriptions.push(format!("Requirement {name}: {old} -> {requirement}"));
                    }
                }
            }
            descriptions.push(format!(
                "Update {}",
                alias.as_deref().unwrap_or("all dependencies")
            ));
        }
    }
    let after = if manifest_changed {
        render_manifest(&doc, &before)
    } else {
        before.clone()
    };
    let manifest: ProjectManifest = toml::from_str(&after)?;
    manifest.validate()?;
    source.replace_manifest(manifest)?;
    let graph =
        super::solve::resolve_prepared(source, pins, checksums, false, dry_run, sources, false)
            .with_context(|| {
                if dry_run {
                    "cannot resolve dry-run plan; fetch missing Git sources first"
                } else {
                    "cannot resolve package plan"
                }
            })?;
    let lock_after = super::lock::command_lock(&graph)?;
    if dry_run
        && graph
            .packages
            .iter()
            .any(|p| matches!(p.identity.source, super::PackageSourceIdentity::Git { .. }))
    {
        writeln!(
            out,
            "Dry run uses cached Git metadata; remote changes are not fetched."
        )?;
    }
    // Present breaking requirement changes before publishing either file.
    for description in descriptions {
        writeln!(out, "{}{description}", if dry_run { "Would " } else { "" })?;
    }
    writeln!(
        out,
        "{} {} dependency packages",
        if dry_run { "Would resolve" } else { "Resolved" },
        graph.packages.len() - 1
    )?;
    out.flush()?;
    if dry_run {
        return Ok(());
    }
    ensure!(
        std::fs::read_to_string(&manifest_path)? == before
            && read_optional(&lock_path)? == lock_before,
        "project changed while resolving; retry the command"
    );
    if before != after {
        replace_manifest(&manifest_path, &after)?;
    }
    if lock_before.as_deref() != Some(&lock_after)
        && let Err(error) = super::lock::write_command_lock(&lock_path, &lock_after)
    {
        if before != after {
            replace_manifest(&manifest_path, &before)
                .context("lock write failed and manifest rollback failed")?;
        }
        return Err(error);
    }
    Ok(())
}

fn render_manifest(doc: &DocumentMut, original: &str) -> String {
    let mut text = doc.to_string();
    // toml_edit emits LF and a final newline; retain the source file's style.
    let crlf = original.contains("\r\n")
        && original.as_bytes().iter().filter(|b| **b == b'\n').count()
            == original.matches("\r\n").count();
    if crlf {
        text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    if !original.ends_with('\n') && text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    text
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn replace_manifest(path: &Path, text: &str) -> Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let (temporary, mut file) = loop {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary =
            path.with_file_name(format!(".project.toml.{}.{n}.tmp", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    let result = (|| {
        file.set_permissions(std::fs::metadata(path)?.permissions())?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

/// Inspection preserves pins and does not generate/repair a project.lock.
pub fn inspect_packages(root: &Path) -> Result<PackageGraph> {
    let source = PathSource::open(root, false)?;
    let (pins, checksums) = super::lock::command_pins(&source.root, None, false)?;
    Ok(super::solve::resolve_prepared(
        source,
        pins,
        checksums,
        false,
        false,
        HashMap::new(),
        false,
    )?)
}
