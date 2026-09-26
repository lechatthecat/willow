//! Refresh-boundary I/O. Query evaluation consumes these immutable captures.
use super::{
    inputs::CompilerInputs,
    tracked::{InputNode, QueryValue, ResultFingerprint},
};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub(crate) struct FileInput {
    pub path: PathBuf,
    pub source: String,
}
impl FileInput {
    pub(crate) fn capture(path: &Path) -> Result<Self> {
        super::tracked::assert_frozen_read();
        let path = std::fs::canonicalize(path)?;
        Ok(Self::read(path)?)
    }
    /// Resolution already selected a physical path. Capture once before lexing;
    /// the resolver spools this exact value for later semantic reads.
    pub(crate) fn read(path: PathBuf) -> std::io::Result<Self> {
        super::tracked::assert_frozen_read();
        let source = std::fs::read_to_string(&path)?;
        Ok(Self { path, source })
    }
}

pub(crate) fn bytes(value: Vec<u8>) -> QueryValue {
    let fingerprint = ResultFingerprint::bytes(&value);
    QueryValue::new(value, fingerprint)
}

/// Explicit configuration boundaries; no formatted aggregate equality gate.
/// Graph entries are sorted by persistent package identity, never resolver IDs.
pub(crate) fn configuration(
    inputs: &CompilerInputs,
    entry: &Path,
    project: Option<&Path>,
) -> Result<Vec<(InputNode, QueryValue)>> {
    super::tracked::assert_frozen_read();
    use serde_json::json;
    let options = &inputs.options;
    let mut values = vec![
        (InputNode::Entry, json!([entry, inputs.project_root])),
        (
            InputNode::Options,
            json!({
                "locked": options.locked, "offline": options.offline,
                "release": options.target.build_mode == crate::BuildMode::Release,
                "debug_info": options.target.emit_debug_info,
                "source_map": options.target.emit_source_map,
                "strip": options.target.strip_symbols,
                "runtime_lib": options.target.runtime_lib,
                "cargo_target_dir": options.target.cargo_target_dir,
                "workers": options.worker_count,
            }),
        ),
        (InputNode::Target, json!(target_lexicon::HOST.to_string())),
        (
            InputNode::RuntimeCapabilities,
            json!(inputs.target.sync_stack_preemption),
        ),
        (InputNode::Features, json!(options.enforce_send_sync)),
        (
            InputNode::CompilerStamp,
            json!([
                env!("CARGO_PKG_VERSION"),
                willow_abi::OBJECT_LAYOUT_REVISION
            ]),
        ),
        // These tables are compiled into the executable. A live revision cannot
        // outlive that executable; on-disk compatibility is a separate phase.
        (InputNode::StdlibStamp, json!(env!("CARGO_PKG_VERSION"))),
        (InputNode::ManifestMode, json!(inputs.project_mode)),
    ];
    let mut graph_entries = Vec::new();
    if let Some(graph) = &inputs.package_graph {
        for package in &graph.packages {
            let mut dependencies = package
                .dependencies
                .iter()
                .map(|dep| {
                    let target = graph.get(dep.package).expect("resolved package dependency");
                    json!([dep.alias, dep.selector, target.identity])
                })
                .collect::<Vec<_>>();
            dependencies.sort_by_cached_key(|v| v.to_string());
            graph_entries.push(json!([
                package.identity,
                package.root,
                package.checksum,
                dependencies
            ]));
        }
        graph_entries.sort_by_cached_key(|v| v.to_string());
        values.push((
            InputNode::PackageGraph,
            json!([
                graph.get(graph.root).expect("root package").identity,
                graph_entries
            ]),
        ));
    } else {
        values.push((InputNode::PackageGraph, json!(null)));
    }
    let mut result = values
        .into_iter()
        .map(|(node, v)| Ok((node, bytes(serde_json::to_vec(&v)?))))
        .collect::<Result<Vec<_>>>()?;
    let roots: std::collections::BTreeSet<_> = project
        .into_iter()
        .map(Path::to_path_buf)
        .chain(
            inputs
                .package_graph
                .iter()
                .flat_map(|g| g.packages.iter().map(|p| p.root.clone())),
        )
        .collect();
    for root in roots {
        for (name, manifest) in [("project.toml", true), ("project.lock", false)] {
            let path = root.join(name);
            let captured = match std::fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            let node = if manifest {
                InputNode::Manifest(path)
            } else {
                InputNode::Lock(path)
            };
            let mut encoded = Vec::with_capacity(1 + captured.as_ref().map_or(0, Vec::len));
            encoded.push(u8::from(captured.is_some()));
            if let Some(captured) = captured {
                encoded.extend(captured);
            }
            result.push((node, bytes(encoded)));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler_db::tracked::TrackedQueryTable;
    #[test]
    fn configuration_inputs_are_independent() {
        let mut inputs =
            CompilerInputs::native(crate::CompilerOptions::debug(), PathBuf::from("root"));
        let mut table = TrackedQueryTable::default();
        let before = configuration(&inputs, Path::new("root/main.wi"), None).unwrap();
        table.replace_inputs(before).unwrap();
        inputs.options.worker_count = Some(123);
        let after = configuration(&inputs, Path::new("root/main.wi"), None).unwrap();
        let changed: Vec<_> = after
            .iter()
            .filter(|entry| !table.matches_inputs(std::slice::from_ref(*entry)))
            .map(|(n, _)| n.clone())
            .collect();
        assert_eq!(changed, [InputNode::Options]);
        table.replace_inputs(after).unwrap();
        inputs.target.sync_stack_preemption = !inputs.target.sync_stack_preemption;
        let after = configuration(&inputs, Path::new("root/main.wi"), None).unwrap();
        let changed: Vec<_> = after
            .iter()
            .filter(|entry| !table.matches_inputs(std::slice::from_ref(*entry)))
            .map(|(n, _)| n.clone())
            .collect();
        assert_eq!(changed, [InputNode::RuntimeCapabilities]);
    }
    #[test]
    fn graph_fingerprints_ignore_resolver_ids_order_and_observation_counters() {
        use crate::package::{
            PackageGraph, PackageId, PackageIdentity, PackageSourceIdentity, ResolvedDependency,
            ResolvedPackage,
        };
        let make = |reverse: bool| {
            let package = |name: &str, id, dependency| ResolvedPackage {
                checksum: None,
                id: PackageId(id),
                identity: PackageIdentity {
                    name: name.into(),
                    version: "1.0.0".into(),
                    source: PackageSourceIdentity::Path {
                        path: PathBuf::from(name),
                    },
                    revision: None,
                },
                root: PathBuf::from(format!("__captured_test_{name}__")),
                dependencies: if name == "a" {
                    vec![ResolvedDependency {
                        selector: None,
                        alias: "dep".into(),
                        package: PackageId(dependency),
                    }]
                } else {
                    vec![]
                },
            };
            let packages = if reverse {
                vec![package("b", 0, 0), package("a", 1, 0)]
            } else {
                vec![package("a", 0, 1), package("b", 1, 1)]
            };
            let mut graph = PackageGraph {
                root: PackageId(u32::from(reverse)),
                packages,
                stats: Default::default(),
            };
            graph.stats.manifests_loaded = if reverse { 100 } else { 2 };
            CompilerInputs::native(crate::CompilerOptions::debug(), PathBuf::from("root"))
                .with_packages(std::sync::Arc::new(graph))
        };
        let mut table = TrackedQueryTable::default();
        table
            .replace_inputs(configuration(&make(false), Path::new("main.wi"), None).unwrap())
            .unwrap();
        assert!(
            table.matches_inputs(&configuration(&make(true), Path::new("main.wi"), None).unwrap())
        );
    }
}
