//! Compiler-owned bridge boundaries. Rust internals are deliberately opaque.
use super::*;
use crate::rust_bridge_ir::{Record, RustBridgeSymbol, Scalar};
use serde_json::{Value, json};
use std::borrow::Cow;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteropGraph {
    pub bridges: Vec<Bridge>,
    /// Shared conservative adapter dependency set, never duplicated per bridge.
    pub crates: Vec<RustCrateIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustCrateIdentity {
    pub name: String,
    pub version: Option<String>,
    pub source: RustCrateSource,
}

/// Known dependency source formats are typed; unknown externally supplied source
/// metadata stays opaque and is returned without interpreting its contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum RustCrateSource {
    Project(ProjectSource),
    Local { kind: LocalSourceKind, path: String },
    Remote(String),
    Other(Value),
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalSourceKind {
    Local,
}
fn required_optional<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSource {
    #[serde(deserialize_with = "required_optional")]
    pub git: Option<String>,
    #[serde(deserialize_with = "required_optional")]
    pub path: Option<String>,
    #[serde(deserialize_with = "required_optional")]
    pub registry: Option<String>,
    #[serde(deserialize_with = "required_optional")]
    pub rev: Option<String>,
    #[serde(deserialize_with = "required_optional")]
    pub tag: Option<String>,
}
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BoundaryKind {
    Rust,
}
#[derive(Debug, Clone, Serialize)]
pub struct DependencyBoundary {
    pub kind: BoundaryKind,
    #[serde(rename = "crate")]
    pub crate_identity: Option<RustCrateIdentity>,
    pub crate_scope: &'static str,
    pub bridge: String,
    pub function: String,
    pub identity: Option<SymbolIdentity>,
    pub coverage: &'static str,
}
#[derive(Debug, Clone, Copy, Serialize)]
pub enum BridgeKind {
    #[serde(rename = "rust-bridge")]
    RustBridge,
}
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BridgeStatus {
    Ok,
    Unknown,
}
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BridgeEvidenceKind {
    Assumption,
    Unknown,
}
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeTrust {
    DeveloperDeclared,
    Undeclared,
}
#[derive(Serialize)]
pub struct BridgeQuery<'a> {
    pub schema: u32,
    pub kind: BridgeKind,
    pub status: BridgeStatus,
    pub bridges: Vec<BridgeRecord<'a>>,
    pub crate_candidates: &'a [RustCrateIdentity],
}
#[derive(Serialize)]
pub struct BridgeSignature<'a> {
    pub inputs: Cow<'a, [Scalar]>,
    pub output: Cow<'a, Scalar>,
}
#[derive(Serialize)]
pub struct BridgeAssumptions {
    pub no_block: bool,
    pub no_io: bool,
}
#[derive(Serialize)]
pub struct AssumptionLocation {
    pub path: Option<String>,
    pub start: usize,
    pub end: usize,
}
#[derive(Serialize)]
pub struct BridgeRecord<'a> {
    pub schema: u32,
    pub kind: BridgeKind,
    pub symbol: &'a str,
    pub function: &'a str,
    pub identity: &'a Option<SymbolIdentity>,
    #[serde(rename = "crate")]
    pub crate_identity: Option<&'a RustCrateIdentity>,
    pub abi_symbol: &'a str,
    pub effects: u8,
    pub execution: &'static str,
    #[serde(rename = "async")]
    pub is_async: bool,
    pub assume_no_block: bool,
    pub no_io: bool,
    pub assumptions: BridgeAssumptions,
    pub review_current: bool,
    pub evidence_kind: BridgeEvidenceKind,
    pub assumption_location: Option<AssumptionLocation>,
    pub trust: BridgeTrust,
    pub official_verified: bool,
    pub signature: BridgeSignature<'a>,
    pub declaration: &'a [Location],
    pub declaration_text: &'a str,
    pub coverage: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub records: Option<Cow<'a, [Record]>>,
}

impl InteropGraph {
    pub fn query(&self, selector: Option<&str>) -> Value {
        serde_json::to_value(self.query_typed(selector)).expect("bridge query serializes")
    }
    pub fn query_typed(&self, selector: Option<&str>) -> BridgeQuery<'_> {
        let records: Vec<_> = self
            .bridges
            .iter()
            .filter(|b| selector.is_none_or(|s| s == b.function || s == b.symbol.willow_function))
            .map(|b| {
                let mut signature = BridgeSignature {
                    inputs: Cow::Borrowed(b.symbol.input_types.as_slice()),
                    output: Cow::Borrowed(&b.symbol.output_type),
                };
                let mut records = (!b.symbol.records.is_empty())
                    .then(|| Cow::Borrowed(b.symbol.records.as_slice()));
                if b.symbol.native.is_some() {
                    native_signature_names(
                        &b.symbol,
                        signature.inputs.to_mut(),
                        signature.output.to_mut(),
                        records
                            .as_mut()
                            .map_or(&mut [], |records| records.to_mut().as_mut_slice()),
                    );
                }
                BridgeRecord {
                    schema: 2,
                    kind: BridgeKind::RustBridge,
                    symbol: &b.symbol.willow_function,
                    function: &b.function,
                    identity: &b.identity,
                    crate_identity: (self.crates.len() == 1).then(|| &self.crates[0]),
                    abi_symbol: &b.symbol.abi_symbol,
                    effects: b.symbol.effects,
                    execution: if b.symbol.is_async {
                        "async_executor"
                    } else if b.symbol.uses_blocking_pool() {
                        "blocking_pool_in_task"
                    } else {
                        "direct"
                    },
                    is_async: b.symbol.is_async,
                    assume_no_block: b.symbol.no_block,
                    no_io: b.symbol.no_io,
                    assumptions: BridgeAssumptions {
                        no_block: b.symbol.no_block,
                        no_io: b.symbol.no_io,
                    },
                    review_current: b.symbol.review_current,
                    evidence_kind: if b.symbol.review_current && b.symbol.has_assumptions() {
                        BridgeEvidenceKind::Assumption
                    } else {
                        BridgeEvidenceKind::Unknown
                    },
                    assumption_location: b.symbol.assumption_span.map(|span| AssumptionLocation {
                        path: b
                            .symbol
                            .native
                            .as_ref()
                            .map(|n| n.source_path.to_string_lossy().into_owned())
                            .or_else(|| b.declaration.first().map(|l| l.path.clone())),
                        start: span.start,
                        end: span.end,
                    }),
                    trust: if b.symbol.no_block || b.symbol.no_io {
                        BridgeTrust::DeveloperDeclared
                    } else {
                        BridgeTrust::Undeclared
                    },
                    official_verified: false,
                    signature,
                    declaration: &b.declaration,
                    declaration_text: &b.declaration_text,
                    coverage: "conservative adapter dependency set; Rust internals are opaque",
                    records,
                }
            })
            .collect();
        BridgeQuery {
            schema: 2,
            kind: BridgeKind::RustBridge,
            status: if selector.is_some() && records.is_empty() {
                BridgeStatus::Unknown
            } else {
                BridgeStatus::Ok
            },
            bridges: records,
            crate_candidates: &self.crates,
        }
    }

    pub(crate) fn load_project(&mut self, root: &Path) -> Result<()> {
        if self.bridges.is_empty() {
            return Ok(());
        }
        let manifest = crate::project::ProjectManifest::load(&root.join("project.toml"))?;
        let input_hash = crate::package::lock::rust_input_hash(
            &manifest,
            crate::rust_bridge_ir::WRAPPER_SCHEMA,
            &willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION.to_string(),
        )?;
        let lock = crate::package::lock::read_rust_lock(root)?
            .filter(|lock| lock.bridge_input_hash == input_hash);
        let lock = if let Some(lock) = lock {
            match std::fs::read(root.join(".willow/rust/Cargo.lock")) {
                Ok(bytes)
                    if format!("{:x}", crate::hex::Hex(&Sha256::digest(&bytes)))
                        == lock.cargo_lock_hash =>
                {
                    Some(lock)
                }
                Ok(_) => None,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            }
        } else {
            None
        };
        self.crates = manifest
            .rust_dependencies
            .iter()
            .map(|(name, spec)| RustCrateIdentity {
                name: name.clone(),
                version: lock
                    .as_ref()
                    .and_then(|l| l.dependencies.get(name))
                    .cloned(),
                source: RustCrateSource::Project(ProjectSource {
                    path: spec
                        .path
                        .as_ref()
                        .map(|p| root.join(p).to_string_lossy().into_owned()),
                    git: spec.git.clone(),
                    rev: spec.rev.clone(),
                    tag: spec.tag.clone(),
                    registry: spec.version.as_ref().map(|_| "crates.io".into()),
                }),
            })
            .collect();
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
                    source: serde_json::from_value(d["source"].clone())?,
                })
            })
            .collect::<Result<_>>()?;
        Ok(())
    }

    pub fn boundaries(
        &self,
        functions: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Vec<DependencyBoundary> {
        let reached: std::collections::HashSet<String> =
            functions.into_iter().map(|f| f.as_ref().into()).collect();
        self.bridges
            .iter()
            .filter(|b| reached.contains(&b.function))
            .map(|b| DependencyBoundary {
                kind: BoundaryKind::Rust,
                crate_identity: (self.crates.len() == 1).then(|| self.crates[0].clone()),
                crate_scope: "rust_dependencies",
                bridge: b.symbol.willow_function.clone(),
                function: b.function.clone(),
                identity: b.identity.clone(),
                coverage: "conservative adapter dependency set",
            })
            .collect()
    }
}

/// Wire names disambiguate independent Rust modules inside the generated crate.
/// Query consumers need the qualified Willow names they can write in source.
fn native_signature_names(
    symbol: &RustBridgeSymbol,
    inputs: &mut [Scalar],
    output: &mut Scalar,
    records: &mut [Record],
) {
    let namespace = crate::qualified_name::QualifiedName::new(&symbol.willow_function).qualifier();
    let rename = |name: &mut String| {
        let local = symbol.willow_record_name(name);
        *name = namespace.map_or_else(
            || local.to_owned(),
            |namespace| crate::qualified_name::QualifiedName::join(namespace, local),
        );
    };
    let mut pending: Vec<_> = inputs.iter_mut().chain(std::iter::once(output)).collect();
    for record in records {
        rename(&mut record.name);
        pending.extend(record.fields.iter_mut().map(|(_, ty)| ty));
    }
    while let Some(ty) = pending.pop() {
        match ty {
            Scalar::Record(name) | Scalar::Opaque(name) => rename(name),
            Scalar::Array(inner) | Scalar::Option(inner) => pending.push(inner),
            Scalar::Result(ok, error) => {
                pending.push(ok);
                pending.push(error);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bridge {
    pub function: String,
    pub identity: Option<SymbolIdentity>,
    pub declaration: Vec<Location>,
    pub declaration_text: String,
    pub symbol: crate::rust_bridge_ir::RustBridgeSymbol,
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
    fn dependency_source_shapes_preserve_wire_bytes_and_unknown_fields() {
        for wire in [
            json!({"git":null,"path":"/work/native","registry":null,"rev":null,"tag":null}),
            json!({"kind":"local","path":"/work/native"}),
            json!("registry+https://example.invalid/index"),
            json!(null),
            json!({"path":"/work/native"}),
            json!({"kind":"local","path":"/work/native","future":{"path":"other"}}),
        ] {
            let typed: RustCrateSource = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(
                serde_json::to_string(&typed).unwrap(),
                serde_json::to_string(&wire).unwrap()
            );
        }
        let project: RustCrateSource = serde_json::from_value(
            json!({"git":null,"path":"/work/native","registry":null,"rev":null,"tag":null}),
        )
        .unwrap();
        assert!(matches!(project, RustCrateSource::Project(_)));
        let local: RustCrateSource =
            serde_json::from_value(json!({"kind":"local","path":"/work/native"})).unwrap();
        assert!(matches!(local, RustCrateSource::Local { .. }));
        let partial: RustCrateSource =
            serde_json::from_value(json!({"path":"/work/native"})).unwrap();
        assert!(matches!(partial, RustCrateSource::Other(_)));
    }

    #[test]
    fn native_query_uses_willow_names_in_nested_record_and_handle_types() {
        let (program, diagnostics) = crate::rust_module::parse(
            Path::new("src/native.rs"),
            "pub struct Leaf { pub count: i64 }\n\
             pub struct Packet { pub leaf: Option<Leaf> }\n\
             pub struct Handle { value: i64 }\n\
             pub fn transfer(packet: Option<Packet>, handle: &Handle) -> Result<Packet, Box<Handle>> { todo!() }",
            crate::diagnostics::FileId(1),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let mut symbol = program
            .items
            .into_iter()
            .find_map(|item| match item {
                crate::parser::ast::Item::Function(function) if function.name == "transfer" => {
                    function.rust_bridge.map(|symbol| *symbol)
                }
                _ => None,
            })
            .unwrap();
        symbol.willow_function = "nested::native::transfer".into();
        let original = serde_json::to_value(&symbol).unwrap();
        let graph = InteropGraph {
            bridges: vec![Bridge {
                function: "transfer-id".into(),
                identity: None,
                declaration: vec![],
                declaration_text: String::new(),
                symbol,
            }],
            ..Default::default()
        };
        let query = graph.query(None);
        let bridge = &query["bridges"][0];
        assert_eq!(
            bridge["signature"],
            json!({
                "inputs": [{"Option": {"Record": "nested::native::Packet"}}, {"Opaque": "nested::native::Handle"}],
                "output": {"Result": [{"Record": "nested::native::Packet"}, {"Opaque": "nested::native::Handle"}]}
            })
        );
        let packet = bridge["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["name"] == "nested::native::Packet")
            .unwrap();
        assert_eq!(
            packet["fields"],
            json!([["leaf", {"Option": {"Record": "nested::native::Leaf"}}]])
        );
        assert_eq!(
            serde_json::to_value(&graph.bridges[0].symbol).unwrap(),
            original
        );
    }

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
                            symbol: crate::rust_bridge_ir::RustBridgeSymbol::new(
                                format!("f{i}"),
                                vec![],
                                crate::rust_bridge_ir::Scalar::Void,
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
