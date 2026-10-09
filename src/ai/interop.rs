//! Compiler-owned bridge boundaries. Rust internals are deliberately opaque.
use super::*;
use serde_json::{Value, json};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteropGraph {
    pub bridges: Vec<Bridge>,
    /// Shared conservative adapter dependency set, never duplicated per bridge.
    pub crates: Vec<RustCrateIdentity>,
    pub rust_adapter: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustCrateIdentity {
    pub name: String,
    pub version: Option<String>,
    pub source: Value,
}

impl InteropGraph {
    pub fn query(&self, selector: Option<&str>) -> Value {
        let bridges: Vec<_> = self
            .bridges
            .iter()
            .filter(|b| selector.is_none_or(|s| s == b.function || s == b.symbol.willow_function))
            .collect();
        let records: Vec<_> = bridges.iter().map(|b| json!({
            "schema":1,"kind":"rust-bridge","symbol":b.symbol.willow_function,
            "function":b.function,"identity":b.identity,
            "crate": if self.crates.len() == 1 { json!(self.crates[0]) } else { Value::Null },
            "rust_adapter":self.rust_adapter,
            "abi_symbol":b.symbol.abi_symbol,
            "signature":{"inputs":b.symbol.input_types,"output":b.symbol.output_type},
            "declaration":b.declaration,"declaration_text":b.declaration_text,
            "coverage":"conservative adapter dependency set; Rust internals are opaque"
        })).collect();
        json!({"schema":1,"kind":"rust-bridge","status":if selector.is_some() && records.is_empty() {"unknown"} else {"ok"},"bridges":records,"crate_candidates":self.crates})
    }

    pub(crate) fn load_project(&mut self, root: &Path) -> Result<()> {
        if self.bridges.is_empty() {
            return Ok(());
        }
        let manifest = crate::project::ProjectManifest::load(&root.join("project.toml"))?;
        self.rust_adapter = manifest.rust.as_ref().map(|r| r.bridge.clone());
        let input_hash = crate::package::lock::rust_input_hash(
            &manifest,
            crate::rust_bridge::WRAPPER_SCHEMA,
            &willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION.to_string(),
        )?;
        let lock = crate::package::lock::read_rust_lock(root)?
            .filter(|lock| lock.bridge_input_hash == input_hash);
        let lock = if let Some(lock) = lock {
            match std::fs::read(root.join(".willow/rust/Cargo.lock")) {
                Ok(bytes) if format!("{:x}", Sha256::digest(&bytes)) == lock.cargo_lock_hash => {
                    Some(lock)
                }
                Ok(_) => None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            }
        } else {
            None
        };
        self.crates = manifest.rust_dependencies.iter().map(|(name, spec)| RustCrateIdentity {
            name:name.clone(),
            version:lock.as_ref().and_then(|l| l.dependencies.get(name)).cloned(),
            source:json!({"path":spec.path.as_ref().map(|p| root.join(p)),"git":spec.git,"rev":spec.rev,"tag":spec.tag,"registry":spec.version.as_ref().map(|_| "crates.io")}),
        }).collect();
        Ok(())
    }

    pub fn resolved_crates(&mut self, dependencies: &Value) -> Result<()> {
        self.crates = dependencies
            .as_array()
            .context("Rust dependency array missing")?
            .iter()
            .map(|d| {
                Ok(RustCrateIdentity {
                    name: d["name"]
                        .as_str()
                        .context("Rust crate name missing")?
                        .into(),
                    version: Some(
                        d["version"]
                            .as_str()
                            .context("Rust crate version missing")?
                            .into(),
                    ),
                    source: d["source"].clone(),
                })
            })
            .collect::<Result<_>>()?;
        Ok(())
    }

    pub fn boundaries(&self, functions: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<Value> {
        let reached: std::collections::HashSet<String> =
            functions.into_iter().map(|f| f.as_ref().into()).collect();
        self.bridges
            .iter()
            .filter(|b| reached.contains(&b.function))
            .map(|b| {
                json!({"kind":"rust",
                "crate":if self.crates.len() == 1 { json!(self.crates[0]) } else { Value::Null },
                "crate_scope":"rust_dependencies",
                "bridge":b.symbol.willow_function,"function":b.function,"identity":b.identity,
                "coverage":"conservative adapter dependency set"})
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bridge {
    pub function: String,
    pub identity: Option<SymbolIdentity>,
    pub declaration: Vec<Location>,
    pub declaration_text: String,
    pub symbol: crate::rust_bridge::RustBridgeSymbol,
}

pub(super) fn attach(
    frontend: &crate::Frontend,
    paths: &HashMap<UnitId, String>,
    functions: &[Function],
    mut declarations: HashMap<(u32, String), String>,
    semantic: &mut SemanticFacts,
) -> Result<()> {
    if frontend.db.rust_bridge_symbols.is_empty() {
        return Ok(());
    }
    let by_name: HashMap<_, _> = functions
        .iter()
        .map(|f| ((f.module.as_str(), f.name.as_str()), f))
        .collect();
    for ((unit, name), symbol) in &frontend.db.rust_bridge_symbols {
        let path = paths
            .get(&crate::module::ModuleId(*unit))
            .context("bridge source missing")?;
        let function = by_name
            .get(&(path.as_str(), name.as_str()))
            .context("bridge function missing")?;
        semantic.interop.bridges.push(Bridge {
            function: function.id.clone(),
            identity: function.identity.clone(),
            declaration: function.locations.clone(),
            declaration_text: declarations
                .remove(&(*unit, name.clone()))
                .context("bridge declaration missing")?,
            symbol: symbol.clone(),
        });
    }
    Ok(())
}

/// One reverse index per immutable snapshot, shared by every selected bridge.
/// Multi-source traversal visits each caller/edge once, even for shared callers.
pub struct CallerIndex<'a> {
    snapshot: &'a Snapshot,
    functions: HashMap<String, usize>,
    callers: Vec<Vec<usize>>,
    pub indexed_edges: usize,
    unknown: bool,
}
impl<'a> CallerIndex<'a> {
    pub fn new(snapshot: &'a Snapshot) -> Self {
        let functions: HashMap<_, _> = snapshot
            .functions
            .iter()
            .enumerate()
            .map(|(i, f)| (f.id.clone(), i))
            .collect();
        let mut callers = vec![vec![]; snapshot.functions.len()];
        let mut indexed_edges = 0;
        for (i, function) in snapshot.functions.iter().enumerate() {
            for target in &function.callees {
                indexed_edges += 1;
                if let Some(&callee) = functions.get(target) {
                    callers[callee].push(i);
                }
            }
        }
        Self {
            snapshot,
            functions,
            callers,
            indexed_edges,
            unknown: snapshot.functions.iter().any(|f| f.unknown),
        }
    }

    pub fn affected(&self, bridges: &[&Bridge]) -> Value {
        let snapshot = self.snapshot;
        let mut seen = std::collections::HashSet::new();
        let mut pending = std::collections::VecDeque::new();
        for bridge in bridges {
            if let Some(&i) = self.functions.get(&bridge.function)
                && seen.insert(i)
            {
                pending.push_back(i);
            }
        }
        let seeds = seen.clone();
        let mut edge_visits = 0;
        let mut callers = Vec::new();
        while let Some(i) = pending.pop_front() {
            if !seeds.contains(&i) && !snapshot.functions[i].synthetic {
                callers.push(&snapshot.functions[i]);
            }
            for &caller in &self.callers[i] {
                edge_visits += 1;
                if seen.insert(caller) {
                    pending.push_back(caller);
                }
            }
        }
        json!({"affected_willow_callers": callers, "edge_visits": edge_visits,
            "coverage":"conservative loaded Willow call graph; Rust internals are opaque",
            "function_visits":seen.len(),"unknown":self.unknown})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_bridge_caller_index_counts_each_edge_once() {
        for n in [16usize, 64, 256, 1024] {
            for shape in ["chain", "fanout", "cycle", "many-bridges"] {
                let bridge_count = if shape == "many-bridges" { n / 2 } else { 1 };
                let mut functions: Vec<Function> = (0..2*n).map(|i| {
                    serde_json::from_value(json!({"id":format!("f{i}"),"module":"fixture.wi","name":format!("f{i}"),
                        "locations":[],"synthetic":false,"fingerprint":"","body_fingerprint":"",
                        "callees":[],"runtime_effects":0,"unknown":false,"unresolved":[]})).unwrap()
                }).collect();
                for (i, f) in functions.iter_mut().enumerate().take(n).skip(bridge_count) {
                    f.callees = match shape {
                        "chain" | "cycle" => vec![format!("f{}", i - 1)],
                        "many-bridges" if i == bridge_count => {
                            (0..bridge_count).map(|j| format!("f{j}")).collect()
                        }
                        "many-bridges" => vec![format!("f{}", i - 1)],
                        _ => vec!["f0".into()],
                    };
                }
                if shape == "cycle" {
                    functions[0].callees.push(format!("f{}", n - 1));
                }
                let graph = InteropGraph {
                    bridges: (0..bridge_count)
                        .map(|i| Bridge {
                            function: format!("f{i}"),
                            identity: None,
                            declaration: vec![],
                            declaration_text: String::new(),
                            symbol: crate::rust_bridge::RustBridgeSymbol::new(
                                format!("f{i}"),
                                vec![],
                                crate::rust_bridge::Scalar::Void,
                            ),
                        })
                        .collect(),
                    ..Default::default()
                };
                let snapshot = Snapshot {
                    edit_context: None,
                    version: 1,
                    compiler: String::new(),
                    compatibility: String::new(),
                    workspace: String::new(),
                    revision: String::new(),
                    sources: BTreeMap::new(),
                    functions,
                    semantic: SemanticFacts {
                        interop: graph,
                        ..Default::default()
                    },
                };
                let index = CallerIndex::new(&snapshot);
                let expected_edges = if shape == "cycle" { n } else { n - 1 };
                assert_eq!(index.indexed_edges, expected_edges);
                // Duplicate seeds and repeated requests must not multiply caller work.
                let bridges: Vec<_> = snapshot
                    .semantic
                    .interop
                    .bridges
                    .iter()
                    .chain(snapshot.semantic.interop.bridges.iter())
                    .collect();
                for _ in 0..8 {
                    let result = index.affected(&bridges);
                    assert_eq!(result["edge_visits"], expected_edges);
                    assert_eq!(result["function_visits"], n);
                    assert_eq!(
                        result["affected_willow_callers"].as_array().unwrap().len(),
                        n - bridge_count
                    );
                }
                println!(
                    "interop shape={shape} functions={} bridges={bridge_count} indexed_edges={} visited_functions={n} edge_visits={expected_edges} queries=8",
                    2 * n,
                    index.indexed_edges
                );
            }
        }
    }
}
