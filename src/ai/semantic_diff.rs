//! Git revision semantic verification, independent of legacy edit JSON.
use super::*;
#[path = "semantic_diff_contracts.rs"]
pub mod contracts;
#[path = "semantic_diff_tree.rs"]
mod tree;
pub use tree::resolve_base;
#[path = "semantic_diff_impact.rs"]
mod impact;
#[path = "semantic_diff_matching.rs"]
mod matching;
#[path = "semantic_diff_state.rs"]
mod state;

#[derive(Debug)]
pub struct Options {
    pub root: std::path::PathBuf,
    /// None selects the default independently from each captured manifest.
    pub entry: Option<std::path::PathBuf>,
    pub project: bool,
    pub base: String,
    pub fail_on_growth: bool,
}
macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant),+
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(match self { $(Self::$variant => $wire),+ })
            }
        }
    };
}
wire_enum!(ReportStatus { Complete => "complete", Incomplete => "incomplete" });
wire_enum!(MatchKind {
    Ambiguous => "ambiguous", Renamed => "renamed", Added => "added",
    Removed => "removed", SignatureChanged => "signature-changed", Exact => "exact",
});
wire_enum!(ContractChange {
    Unchanged => "unchanged", Added => "added", Removed => "removed",
    Equivalent => "equivalent", Strengthened => "strengthened", Weakened => "weakened",
    Changed => "changed",
});
wire_enum!(PublicApiChange {
    Unknown => "unknown", Unchanged => "unchanged", Removed => "removed",
    Added => "added", Renamed => "renamed", SignatureChanged => "signature-changed",
});
wire_enum!(TargetChange { Changed => "changed", Unchanged => "unchanged" });
wire_enum!(Side { Base => "base", Target => "target" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CauseCertainty {
    MissingSource,
    ConservativeBound,
    CompilerProven,
    Other(String),
}
impl CauseCertainty {
    fn as_str(&self) -> &str {
        match self {
            Self::MissingSource => "missing-source",
            Self::ConservativeBound => "conservative-bound",
            Self::CompilerProven => "compiler-proven",
            Self::Other(value) => value,
        }
    }
}
impl From<&str> for CauseCertainty {
    fn from(value: &str) -> Self {
        match value {
            "missing-source" => Self::MissingSource,
            "conservative-bound" => Self::ConservativeBound,
            "compiler-proven" => Self::CompilerProven,
            _ => Self::Other(value.to_owned()),
        }
    }
}
impl Serialize for CauseCertainty {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}
impl std::fmt::Display for CauseCertainty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RevisionDescription {
    pub digest: String,
    pub revision: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct Sides<T> {
    pub base: T,
    pub target: T,
}
#[derive(Debug, Clone, Serialize)]
pub struct EffectDifference {
    pub added: Vec<String>,
    pub base: Vec<String>,
    pub removed: Vec<String>,
    pub target: Vec<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct ContractDifference {
    pub base: BTreeSet<String>,
    pub base_violations: Vec<String>,
    pub change: ContractChange,
    pub target: BTreeSet<String>,
    pub target_violations: Vec<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct TargetDifference {
    pub base: Option<Vec<String>>,
    pub change: TargetChange,
    pub target: Option<Vec<String>>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Cause {
    pub certainty: CauseCertainty,
    pub chain: Vec<String>,
    pub effect: Vec<String>,
    pub location: Option<super::super::Location>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<Side>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct CauseLeaf {
    certainty: CauseCertainty,
    location: Option<super::super::Location>,
}
#[derive(Debug, Clone, Serialize)]
pub struct SymbolChange {
    pub cause: Vec<Cause>,
    pub contract: Option<ContractDifference>,
    pub effects: Option<EffectDifference>,
    pub impacted_callers: Vec<String>,
    #[serde(rename = "match")]
    pub matching: MatchKind,
    pub public_api: PublicApiChange,
    /// Ambiguous records omit this field; matched records retain explicit null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_to: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<Sides<Option<DeclarationSignature>>>,
    pub symbol: String,
    pub targets: Option<TargetDifference>,
}
type AssumptionEvidence = BTreeMap<String, BTreeMap<String, super::super::assumptions::Assumption>>;

#[derive(Debug, Default, Clone, Serialize)]
pub struct ReportSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assumptions: Option<Sides<AssumptionEvidence>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_violations: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Sides<Vec<DiagnosticFact>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect_growth: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impacted_callers: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_panic: Option<Sides<BTreeMap<String, crate::semantic::type_checker::PanicEvidence>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_violations: Option<usize>,
    pub uncertainty: Vec<String>,
}
pub trait SummaryCounts {
    fn contract_violations(&self) -> usize;
    fn policy_violations(&self) -> usize;
}
impl SummaryCounts for ReportSummary {
    fn contract_violations(&self) -> usize {
        self.contract_violations.unwrap_or(0)
    }
    fn policy_violations(&self) -> usize {
        self.policy_violations.unwrap_or(0)
    }
}

// Legacy JSON objects used lexical key order. Sort only while serializing the
// wire result; the analysis and human renderer retain concrete records.
fn serialize_wire<S: serde::Serializer, T: Serialize>(
    value: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serde_json::to_value(value)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}
#[derive(Debug, Serialize)]
#[serde(bound(serialize = "C: Serialize, S: Serialize"))]
pub struct SemanticDiff<C = SymbolChange, S = ReportSummary> {
    pub schema: &'static str,
    pub status: ReportStatus,
    pub base: Option<RevisionDescription>,
    pub target: Option<RevisionDescription>,
    #[serde(serialize_with = "serialize_wire")]
    pub changes: Vec<C>,
    #[serde(serialize_with = "serialize_wire")]
    pub summary: S,
}
impl<C, S: SummaryCounts> SemanticDiff<C, S> {
    pub fn exit_code(&self) -> i32 {
        if self.status == ReportStatus::Incomplete {
            3
        } else if self.summary.contract_violations() != 0 || self.summary.policy_violations() != 0 {
            1
        } else {
            0
        }
    }
}
impl SemanticDiff {
    pub fn incomplete(reason: String) -> Self {
        Self {
            schema: "willow.semantic-diff.v3",
            status: ReportStatus::Incomplete,
            base: None,
            target: None,
            changes: vec![],
            summary: ReportSummary {
                uncertainty: vec![reason],
                ..Default::default()
            },
        }
    }
    pub fn human(&self, explain: bool) -> String {
        fn list<'a>(items: impl IntoIterator<Item = &'a String>) -> String {
            let text = items
                .into_iter()
                .map(|item| matching::human_name(item))
                .collect::<Vec<_>>()
                .join(", ");
            if text.is_empty() { "none".into() } else { text }
        }
        let mut text = format!("Semantic verification: {}\n", self.status);
        if let Some(base) = &self.base {
            text.push_str(&format!("Base digest: {}\n", base.digest));
        }
        if let Some(target) = &self.target {
            text.push_str(&format!("Target digest: {}\n", target.digest));
        }
        if let Some(evidence) = &self.summary.no_panic {
            for (side, evidence) in [
                (Side::Base, &evidence.base),
                (Side::Target, &evidence.target),
            ] {
                for (name, proof) in evidence {
                    if proof.status == crate::semantic::type_checker::PanicStatus::Proven {
                        text.push_str(&format!(
                            "no_panic {side} {}: {}\n",
                            matching::human_name(name),
                            proof.basis.as_str()
                        ));
                    }
                }
            }
        }
        if let Some(functions) = &self.summary.assumptions {
            for (side, functions) in [
                (Side::Base, &functions.base),
                (Side::Target, &functions.target),
            ] {
                for (function, contracts) in functions {
                    for (contract, evidence) in contracts {
                        text.push_str(&format!(
                            "{contract} {side} {}: assumption at {}:{}\n",
                            matching::human_name(function),
                            evidence
                                .location
                                .as_ref()
                                .and_then(|location| location.path.as_deref())
                                .unwrap_or("?"),
                            evidence.location.as_ref().map_or_else(
                                || "null".into(),
                                |location| location.start.to_string()
                            ),
                        ));
                    }
                }
            }
        }
        for change in &self.changes {
            text.push_str(&format!(
                "{}: {}\n",
                matching::human_name(&change.symbol),
                change.matching
            ));
            if let Some(Some(target)) = &change.renamed_to {
                text.push_str(&format!("  Renamed to: {}\n", matching::human_name(target)));
            }
            text.push_str(&format!(
                "  Effects added: {}; removed: {}\n",
                list(change.effects.iter().flat_map(|effects| &effects.added)),
                list(change.effects.iter().flat_map(|effects| &effects.removed)),
            ));
            let contract_change = change
                .contract
                .as_ref()
                .map_or_else(|| "unknown".into(), |contract| contract.change.to_string());
            text.push_str(&format!(
                "  Contract {contract_change}: {} -> {}\n",
                list(change.contract.iter().flat_map(|contract| &contract.base)),
                list(change.contract.iter().flat_map(|contract| &contract.target)),
            ));
            text.push_str(&format!(
                "  Contract violations: base {}; target {}\n",
                list(
                    change
                        .contract
                        .iter()
                        .flat_map(|contract| &contract.base_violations)
                ),
                list(
                    change
                        .contract
                        .iter()
                        .flat_map(|contract| &contract.target_violations)
                ),
            ));
            if change.public_api != PublicApiChange::Unchanged {
                text.push_str(&format!("  Public API: {}\n", change.public_api));
            }
            if let Some(targets) = &change.targets
                && targets.change == TargetChange::Changed
            {
                text.push_str(&format!(
                    "  Call targets: {} -> {}\n",
                    list(targets.base.iter().flatten()),
                    list(targets.target.iter().flatten()),
                ));
            }
            if explain {
                for cause in &change.cause {
                    let chain = cause
                        .chain
                        .iter()
                        .map(|name| matching::human_name(name))
                        .collect::<Vec<_>>()
                        .join(" -> ");
                    let side = cause
                        .side
                        .map_or_else(|| "?".into(), |side| side.to_string());
                    text.push_str(&format!(
                        "  cause: {side} {} via {chain} ({})\n",
                        list(&cause.effect),
                        cause.certainty
                    ));
                    if let Some(location) = &cause.location {
                        text.push_str(&format!(
                            "    {}:{}-{}\n",
                            location.path, location.start, location.end
                        ));
                    }
                }
                text.push_str(&format!(
                    "  Impacted callers: {}\n",
                    list(&change.impacted_callers)
                ));
            }
        }
        text.push_str(&format!(
            "Contract violations: {}; policy violations: {}\n",
            self.summary.contract_violations(),
            self.summary.policy_violations(),
        ));
        for reason in &self.summary.uncertainty {
            text.push_str(&format!("Incomplete: {}\n", matching::human_name(reason)));
        }
        text
    }
}
#[derive(Clone)]
pub(crate) struct Capture {
    baseline: Baseline,
    shapes: im::OrdMap<String, String>,
    kinds: im::OrdMap<String, SymbolKind>,
    lambda_owners: im::OrdMap<String, im::OrdSet<String>>,
    retained: state::ReportState,
    fact_updates: usize,
    witness_updates: usize,
    witness_edges: usize,
    witness_queue: usize,
    graph_edges: usize,
    coordinate_updates: usize,
    contracts: im::OrdMap<String, BTreeSet<String>>,
    causes: CausePaths,
    ambiguous: im::OrdSet<String>,
    contract_index: Option<Node>,
    panic_evidence: im::OrdMap<String, crate::semantic::type_checker::PanicEvidence>,
    assumption_evidence: AssumptionEvidence,
    violations: im::OrdSet<String>,
}
#[derive(Clone, Default)]
struct CausePaths {
    next: im::OrdMap<(String, u8), String>,
    leaves: im::OrdMap<(String, u8), CauseLeaf>,
    unknown: im::OrdSet<String>,
    locations: im::OrdMap<(String, usize, String, u8), usize>,
}
impl CausePaths {
    fn render(&self, key: &str, bit: u8) -> Cause {
        let mut chain = vec![key.to_owned()];
        let mut cursor = key;
        while let Some(target) = self.next.get(&(cursor.to_owned(), bit)) {
            chain.push(target.clone());
            cursor = target;
        }
        let address = (cursor.to_owned(), bit);
        let leaf = self.leaves.get(&address);
        Cause {
            effect: effect_names(bit),
            chain,
            location: leaf.and_then(|leaf| leaf.location.clone()),
            certainty: leaf.map_or_else(
                || {
                    if self.unknown.contains(key) {
                        CauseCertainty::MissingSource
                    } else {
                        CauseCertainty::ConservativeBound
                    }
                },
                |leaf| leaf.certainty.clone(),
            ),
            side: None,
        }
    }
}
fn effect_names(bits: u8) -> Vec<String> {
    (0..RuntimeEffects::BIT_COUNT)
        .filter_map(|i| {
            let bit = RuntimeEffects::from_bit(i);
            (bits & bit.bits() != 0).then(|| {
                [
                    "may-allocate",
                    "may-block",
                    "may-suspend",
                    "may-preempt",
                    "no-preempt-region",
                    "may-panic",
                    "may-io",
                ][i as usize]
                    .to_owned()
            })
        })
        .collect()
}
fn contract_bits(contracts: &BTreeSet<String>) -> u8 {
    contracts.iter().fold(0, |bits, c| {
        bits | match c.as_str() {
            "no_wait" => RuntimeEffects::MAY_BLOCK
                .union(RuntimeEffects::MAY_SUSPEND)
                .bits(),
            "no_block" => RuntimeEffects::MAY_BLOCK.bits(),
            "no_suspend" => RuntimeEffects::MAY_SUSPEND.bits(),
            "no_io" => RuntimeEffects::MAY_IO.bits(),
            "no_panic" => RuntimeEffects::MAY_PANIC
                .union(RuntimeEffects::MAY_CHECKED_PANIC)
                .bits(),
            "no_alloc" => RuntimeEffects::MAY_ALLOCATE.bits(),
            _ => 0,
        }
    })
}
impl Capture {
    fn from_snapshot(snapshot: &Snapshot, diagnostics: Vec<DiagnosticFact>) -> Result<Self> {
        let (mut baseline, names) = Baseline::from_snapshot_with_names(snapshot, diagnostics)?;
        let super::SnapshotNames {
            type_names,
            ids,
            ambiguous,
            contracts,
        } = names;
        // Keep the kind chosen by the stable-key producer alongside each key.
        // Function-only records use the producer's function namespace even when
        // their internal identity describes a lambda or dispatch summary.
        let mut kinds = im::OrdMap::new();
        for symbol in &snapshot.semantic.symbols {
            if matches!(
                symbol.kind,
                SymbolKind::Function
                    | SymbolKind::Method
                    | SymbolKind::Constructor
                    | SymbolKind::Class
                    | SymbolKind::Interface
                    | SymbolKind::Enum
                    | SymbolKind::Variant
                    | SymbolKind::Field
                    | SymbolKind::StaticField
                    | SymbolKind::Const
            ) && let Some(key) = ids.get(&symbol.id)
            {
                kinds.insert(key.clone(), symbol.kind.clone());
            }
        }
        for function in &snapshot.functions {
            kinds
                .entry(ids[&function.id].clone())
                .or_insert(SymbolKind::Function);
        }
        // Compiler waiting summaries apply async transmission masks. Legacy
        // graph aggregation is intentionally unchanged; use those summaries
        // here so merely creating a Task does not introduce a wait.
        for f in &snapshot.functions {
            if let Some(fact) = baseline.facts.get_mut(&ids[&f.id]).map(Arc::make_mut) {
                let waiting = RuntimeEffects::MAY_BLOCK
                    .union(RuntimeEffects::MAY_SUSPEND)
                    .bits();
                let mut bits = snapshot
                    .semantic
                    .compiler_effects
                    .get(&f.id)
                    .map_or(f.runtime_effects, |checked| {
                        (f.runtime_effects & !waiting) | (checked & waiting)
                    });
                if snapshot
                    .semantic
                    .panic_evidence
                    .get(&f.id)
                    .is_some_and(|e| e.status == crate::semantic::type_checker::PanicStatus::Proven)
                {
                    bits &= !RuntimeEffects::MAY_PANIC
                        .union(RuntimeEffects::MAY_CHECKED_PANIC)
                        .bits();
                }
                let effects = Some(EffectState {
                    may: bits,
                    unknown: if f.unknown {
                        RuntimeEffects::ALL.bits() & !bits
                    } else {
                        0
                    },
                });
                if fact.effects != effects {
                    fact.effects = effects;
                    baseline.refresh_effect_fact_index(&ids[&f.id])?;
                }
            }
        }
        let panic_evidence = snapshot
            .semantic
            .panic_evidence
            .iter()
            .filter_map(|(id, e)| ids.get(id).map(|key| (key.clone(), e.clone())))
            .collect();
        let assumptions = super::super::assumptions::Analysis::new(snapshot);
        let assumption_evidence = snapshot
            .functions
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                let output = assumptions.typed_output(i);
                let contracts = contracts.get(&ids[&f.id])?;
                let mut evidence = BTreeMap::new();
                for (contract, assumption) in [
                    ("no_block", output.no_block),
                    ("no_io", output.no_io),
                    ("no_wait", output.no_wait),
                ] {
                    if contracts.contains(contract)
                        && let Some(assumption) = assumption
                    {
                        let mut assumption = assumption.clone();
                        if let Some(path) = assumption
                            .location
                            .as_mut()
                            .and_then(|location| location.path.as_mut())
                        {
                            *path = relative(path, &snapshot.workspace);
                        }
                        if let Some(function) = ids.get(&assumption.function) {
                            assumption.function = function.clone();
                        }
                        evidence.insert(contract.to_owned(), assumption);
                    }
                }
                (!evidence.is_empty()).then(|| (ids[&f.id].clone(), evidence))
            })
            .collect();
        let mut causes = CausePaths::default();
        let mut retained = state::ReportState::new(snapshot, ids, type_names);
        retained.replace_leaves(snapshot, &mut causes);
        let affected = retained.functions.keys().cloned().collect();
        let (witness_updates, witness_edges, witness_queue) =
            retained.repair_causes(&affected, &mut causes);
        // Reuse the baseline fact index; only contracts need an extra index.
        // Empty contracts are the default, so unrelated declarations cost no
        // additional index storage or hashing here.
        let mut entries = Vec::new();
        let mut violations = im::OrdSet::new();
        for (key, contract) in &contracts {
            let bits = baseline
                .facts
                .get(key)
                .and_then(|f| f.effects.as_ref())
                .map_or(0, |e| e.may | e.unknown);
            if bits & contract_bits(contract) != 0 {
                violations.insert(key.clone());
            }
            entries.push((hash(key), key.clone(), hash_serialized(contract)?));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let contract_index = Node::build(&entries);
        let mut shapes = im::OrdMap::new();
        let mut lambda_owners = im::OrdMap::new();
        matching::update_shapes(&mut shapes, &mut lambda_owners, snapshot, &retained.ids)?;
        Ok(Self {
            shapes,
            kinds,
            lambda_owners,
            fact_updates: snapshot.functions.len(),
            witness_updates,
            witness_edges,
            witness_queue,
            graph_edges: 0,
            coordinate_updates: 0,
            retained,
            contract_index,
            panic_evidence,
            assumption_evidence,
            violations,
            baseline,
            contracts,
            causes,
            ambiguous,
        })
    }
}
fn resolve_entry(options: &Options, inputs: &tree::Tree) -> Result<std::path::PathBuf> {
    let entry = if let Some(entry) = &options.entry {
        entry.clone()
    } else if let Some(bytes) = inputs.get("project.toml") {
        let manifest: crate::project::ProjectManifest =
            toml::from_str(std::str::from_utf8(bytes)?)?;
        manifest.validate()?;
        manifest
            .project
            .entry
            .unwrap_or_else(|| "src/main.wi".into())
            .into()
    } else {
        "main.wi".into()
    };
    ensure!(
        !entry.as_os_str().is_empty()
            && entry
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
        "entry must be workspace-relative"
    );
    Ok(entry)
}
fn analyze(
    options: &Options,
    scratch: &tree::Scratch,
    revision: &mut crate::compiler_db::revision::AnalysisRevision,
    entry: &Path,
) -> Result<Capture> {
    let entry = scratch.0.join(entry);
    // Both the resolved entry and discovery belong to this captured tree.
    // --project can explicitly require project discovery.
    let project = options.project || scratch.0.join("project.toml").is_file();
    let mut compiler_options = crate::CompilerOptions::debug();
    compiler_options.offline = project;
    compiler_options.locked = project && scratch.0.join("project.lock").is_file();
    let run = |revision: &mut crate::compiler_db::revision::AnalysisRevision,
               diagnostics: &mut Diagnostics| {
        let session = crate::CompilerSession::new(
            entry.to_str().context("non UTF-8 entry")?,
            "",
            &compiler_options,
            project.then(|| scratch.0.clone()),
        );
        revision.analyze_verification_delta(session, diagnostics)
    };
    let mut diagnostics = Diagnostics::default();
    let result = match run(revision, &mut diagnostics) {
        Ok(crate::ai::SnapshotOutcome::Retry(retry)) => {
            match retry {
                // Duplicate identities cannot safely key reusable compiler facts.
                crate::ai::SnapshotRetry::ColdCompilation => *revision = Default::default(),
                crate::ai::SnapshotRetry::FullCapture => {
                    revision.snapshot_state = Default::default()
                }
            }
            diagnostics = Diagnostics::default();
            run(revision, &mut diagnostics)
        }
        result => result,
    };
    let mut update = result
        .and_then(|outcome| match outcome {
            crate::ai::SnapshotOutcome::Done(update) => Ok(update),
            crate::ai::SnapshotOutcome::Retry(retry) => anyhow::bail!("{retry}"),
        })
        .map_err(|error| {
            let messages = diagnostics
                .facts
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::anyhow!("{error:#}: {messages}")
        })?;
    let capture_result = (|| -> Result<Capture> {
        let snapshot = match &mut update {
            crate::ai::VerificationSnapshotUpdate::Full(snapshot) => snapshot,
            crate::ai::VerificationSnapshotUpdate::Delta { patch, .. } => patch,
        };
        for path in snapshot.sources.keys() {
            ensure!(
                Path::new(path).starts_with(&scratch.0)
                    || super::super::bundled_source(path).is_some(),
                "dependency outside captured inputs"
            );
        }
        // Standalone analysis uses the entry's parent for compiler resolution.
        // Evidence, including baseline identities and diagnostics, uses --root.
        snapshot.workspace = scratch
            .0
            .to_str()
            .context("non UTF-8 captured root")?
            .to_owned();
        let capture = match &update {
            crate::ai::VerificationSnapshotUpdate::Full(snapshot) => {
                Capture::from_snapshot(snapshot, diagnostics.facts)?
            }
            crate::ai::VerificationSnapshotUpdate::Delta {
                patch,
                removed_functions,
                removed_symbols,
                relocations,
            } => revision
                .verification_capture
                .as_ref()
                .context("retained report baseline")?
                .update(
                    patch,
                    removed_functions,
                    removed_symbols,
                    relocations,
                    diagnostics.facts,
                )?,
        };
        Ok(capture)
    })();
    let capture = match capture_result {
        Ok(capture) => capture,
        Err(error) => {
            revision.snapshot_state = crate::ai::VerificationSnapshotState::default();
            revision.verification_capture = None;
            return Err(error);
        }
    };
    revision.incremental_stats["report_facts_rebuilt"] = json!(capture.fact_updates);
    revision.incremental_stats["report_witness_owners_visited"] = json!(capture.witness_updates);
    revision.incremental_stats["report_witness_edges_visited"] = json!(capture.witness_edges);
    revision.incremental_stats["report_witness_queue_operations"] = json!(capture.witness_queue);
    revision.incremental_stats["report_dependency_edges_visited"] = json!(capture.graph_edges);
    revision.incremental_stats["report_coordinate_leaves_visited"] =
        json!(capture.coordinate_updates);
    revision.verification_capture = Some(capture.clone());
    Ok(capture)
}
fn comparison_keys(before: &Capture, after: &Capture, work: &mut Work) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    Node::difference(
        before.baseline.index.as_ref(),
        after.baseline.index.as_ref(),
        &mut keys,
        work,
    );
    Node::difference(
        before.contract_index.as_ref(),
        after.contract_index.as_ref(),
        &mut keys,
        work,
    );
    // Existing violations remain evidence even when their summaries are equal.
    keys.extend(
        before
            .violations
            .iter()
            .chain(after.violations.iter())
            .cloned(),
    );
    work.compared_symbols = keys.len();
    keys
}
fn compare(before: &Capture, after: &Capture, fail_on_growth: bool) -> Result<SemanticDiff> {
    let keys = comparison_keys(before, after, &mut Work::default());
    compare_keys(before, after, fail_on_growth, keys)
}
fn symbol_pairs<'a>(
    old: &Baseline,
    new: &Baseline,
    keys: &'a BTreeSet<String>,
    renames: &'a BTreeMap<String, String>,
) -> Vec<(Option<&'a String>, Option<&'a String>)> {
    let renamed_targets: BTreeSet<_> = renames.values().collect();
    // Source and target identities are separate namespaces: a displaced lambda
    // may consume a key which is also the source of another correspondence.
    let mut comparisons = Vec::new();
    for key in keys {
        if old.facts.contains_key(key) {
            let target = renames.get(key).or_else(|| {
                (!matching::is_lambda(key) && new.facts.contains_key(key)).then_some(key)
            });
            comparisons.push((Some(key), target));
        }
        if new.facts.contains_key(key)
            && !renamed_targets.contains(key)
            && (matching::is_lambda(key) || !old.facts.contains_key(key))
        {
            comparisons.push((None, Some(key)));
        }
    }
    comparisons
}

fn contract_difference(
    base: &BTreeSet<String>,
    target: &BTreeSet<String>,
    base_violations: u8,
    target_violations: u8,
) -> ContractDifference {
    let change = if base == target {
        ContractChange::Unchanged
    } else if base.is_empty() {
        ContractChange::Added
    } else if target.is_empty() {
        ContractChange::Removed
    } else {
        let base_bits = contract_bits(base);
        let target_bits = contract_bits(target);
        if base_bits == target_bits {
            ContractChange::Equivalent
        } else if base_bits & !target_bits == 0 {
            ContractChange::Strengthened
        } else if target_bits & !base_bits == 0 {
            ContractChange::Weakened
        } else {
            ContractChange::Changed
        }
    };
    ContractDifference {
        change,
        base: base.clone(),
        target: target.clone(),
        base_violations: effect_names(base_violations),
        target_violations: effect_names(target_violations),
    }
}

fn comparison_causes(
    sides: [(&Capture, &str, u8, Side); 2],
    emitted_hops: &mut usize,
) -> Result<Vec<Cause>> {
    let mut causes = Vec::new();
    for i in 0..RuntimeEffects::BIT_COUNT {
        let bit = RuntimeEffects::from_bit(i).bits();
        for (capture, key, bits, side) in sides {
            if bits & bit != 0 {
                let mut cause = capture.causes.render(key, bit);
                *emitted_hops += cause.chain.len();
                ensure!(
                    *emitted_hops <= 1_000_000,
                    "witness evidence exceeds output limit"
                );
                cause.side = Some(side);
                causes.push(cause);
            }
        }
    }
    Ok(causes)
}

fn append_diagnostic_uncertainty(uncertainty: &mut Vec<String>, old: &Baseline, new: &Baseline) {
    for (side, baseline) in [("base", old), ("target", new)] {
        for diagnostic in &baseline.diagnostics {
            if diagnostic.severity == "error" && diagnostic.code != "E2610" {
                uncertainty.push(format!(
                    "{side}: {}: {}",
                    diagnostic.code, diagnostic.message
                ));
            }
        }
    }
}

fn compare_keys(
    before: &Capture,
    after: &Capture,
    fail_on_growth: bool,
    keys: BTreeSet<String>,
) -> Result<SemanticDiff> {
    let old = &before.baseline;
    let new = &after.baseline;
    let legacy = old.compare(new)?;
    let (renames, keys) = matching::pairs(before, after, &keys)?;
    let empty = BTreeSet::new();
    let mut changes = Vec::new();
    let mut old_panic = BTreeMap::new();
    let mut new_panic = BTreeMap::new();
    let mut growth = 0;
    let mut emitted_hops = 0usize;
    let mut uncertainty = Vec::new();
    for stem in before.ambiguous.clone().union(after.ambiguous.clone()) {
        uncertainty.push(format!("ambiguous symbol: {stem}"));
        changes.push(SymbolChange {
            symbol: stem,
            matching: MatchKind::Ambiguous,
            effects: None,
            cause: vec![],
            impacted_callers: vec![],
            contract: None,
            public_api: PublicApiChange::Unknown,
            targets: None,
            renamed_to: None,
            signature: None,
        });
    }
    for key in before
        .causes
        .unknown
        .clone()
        .union(after.causes.unknown.clone())
    {
        uncertainty.push(format!("missing-source: {key}"));
    }
    let comparisons = symbol_pairs(old, new, &keys, &renames);
    for (base_key, new_key) in comparisons {
        let key = base_key.or(new_key).expect("comparison identity");
        let target_key = new_key.unwrap_or(key);
        let a = base_key.and_then(|key| old.facts.get(key));
        let renamed = target_key != key && a.is_some_and(|f| f.signature.is_some());
        let b = new_key.and_then(|key| new.facts.get(key));
        let ca = base_key
            .and_then(|key| before.contracts.get(key))
            .unwrap_or(&empty);
        let cb = new_key
            .and_then(|key| after.contracts.get(key))
            .unwrap_or(&empty);
        let ae = a
            .and_then(|f| f.effects.as_ref())
            .map_or(0, |e| e.may | e.unknown);
        let be = b
            .and_then(|f| f.effects.as_ref())
            .map_or(0, |e| e.may | e.unknown);
        let added = be & !ae;
        let removed = ae & !be;
        let av = ae & contract_bits(ca);
        let bv = be & contract_bits(cb);
        let signature_a = a.and_then(|f| f.signature.as_ref());
        let signature_b = b.and_then(|f| f.signature.as_ref());
        let effects_changed =
            a.and_then(|f| f.effects.as_ref()) != b.and_then(|f| f.effects.as_ref());
        let summary_changed =
            signature_a != signature_b || effects_changed || ca != cb || av != 0 || bv != 0;
        let raw_targets_changed = a.map(|f| &f.callees) != b.map(|f| &f.callees);
        // Body-only edits still carry proof evidence. Visiting an identical
        // owner during matching or a full comparison must not add evidence.
        if base_key != new_key
            || summary_changed
            || raw_targets_changed
            || a.map(|f| &f.body) != b.map(|f| &f.body)
        {
            old_panic.extend(base_key.and_then(|key| before.panic_evidence.get_key_value(key)));
            new_panic.extend(new_key.and_then(|key| after.panic_evidence.get_key_value(key)));
        }
        let stem = key.rsplit_once('#').map_or(key.as_str(), |(stem, _)| stem);
        if before.ambiguous.contains(stem) || after.ambiguous.contains(stem) {
            continue;
        }
        if added != 0 {
            growth += 1;
        }
        let targets_b = b.map(|f| &f.callees);
        let targets_changed = if renames.is_empty() {
            raw_targets_changed
        } else {
            a.map(|f| matching::targets(&f.callees, &renames)).as_ref() != targets_b
        };
        if !summary_changed && !targets_changed && !renamed {
            continue;
        }
        let matching = if renamed {
            MatchKind::Renamed
        } else if a.is_none() {
            MatchKind::Added
        } else if b.is_none() {
            MatchKind::Removed
        } else if signature_a != signature_b {
            MatchKind::SignatureChanged
        } else {
            MatchKind::Exact
        };
        let contract = contract_difference(ca, cb, av, bv);
        let causes = comparison_causes(
            [
                (before, key, removed | av, Side::Base),
                (after, target_key, added | bv, Side::Target),
            ],
            &mut emitted_hops,
        )?;
        let public_api = match (public_signature(signature_a), public_signature(signature_b)) {
            (true, false) => PublicApiChange::Removed,
            (false, true) => PublicApiChange::Added,
            (true, true) if renamed => PublicApiChange::Renamed,
            (true, true) if signature_a != signature_b => PublicApiChange::SignatureChanged,
            _ => PublicApiChange::Unchanged,
        };
        changes.push(SymbolChange {
            symbol: key.clone(),
            matching,
            renamed_to: Some(
                (base_key.is_some() && new_key.is_some() && target_key != key)
                    .then(|| target_key.clone()),
            ),
            signature: Some(Sides {
                base: signature_a.cloned(),
                target: signature_b.cloned(),
            }),
            effects: Some(EffectDifference {
                added: effect_names(added),
                removed: effect_names(removed),
                base: effect_names(ae),
                target: effect_names(be),
            }),
            cause: causes,
            impacted_callers: vec![],
            contract: Some(contract),
            public_api,
            targets: Some(TargetDifference {
                base: a.map(|f| f.callees.clone()),
                target: targets_b.cloned(),
                change: if targets_changed {
                    TargetChange::Changed
                } else {
                    TargetChange::Unchanged
                },
            }),
        });
    }
    impact::assign(&mut changes, old, new)?;
    let impacted: BTreeSet<_> = changes
        .iter()
        .flat_map(|change| &change.impacted_callers)
        .collect();
    let impacted_count = impacted.len();
    let violations = old
        .diagnostics
        .iter()
        .chain(&new.diagnostics)
        .filter(|d| d.code == "E2610")
        .count();
    append_diagnostic_uncertainty(&mut uncertainty, old, new);
    let status = if legacy.complete && uncertainty.is_empty() {
        ReportStatus::Complete
    } else {
        ReportStatus::Incomplete
    };
    Ok(SemanticDiff {
        schema: "willow.semantic-diff.v3",
        status,
        base: None,
        target: None,
        changes,
        summary: ReportSummary {
            contract_violations: Some(violations),
            policy_violations: Some(if fail_on_growth { growth } else { 0 }),
            effect_growth: Some(growth),
            impacted_callers: Some(impacted_count),
            uncertainty,
            no_panic: Some(Sides {
                base: old_panic
                    .into_iter()
                    .map(|(key, proof)| (key.clone(), proof.clone()))
                    .collect(),
                target: new_panic
                    .into_iter()
                    .map(|(key, proof)| (key.clone(), proof.clone()))
                    .collect(),
            }),
            assumptions: Some(Sides {
                base: before.assumption_evidence.clone(),
                target: after.assumption_evidence.clone(),
            }),
            diagnostics: Some(Sides {
                base: old.diagnostics.clone(),
                target: new.diagnostics.clone(),
            }),
        },
    })
}
pub fn verify(options: &Options) -> Result<SemanticDiff> {
    let root = std::fs::canonicalize(&options.root)?;
    let before = tree::base(&root, &options.base)?;
    let target = tree::target(&root)?;
    let before_digest = hash_serialized(&before)?;
    let target_digest = hash_serialized(&target)?;
    let scratch = tree::Scratch::new()?;
    scratch.write(&BTreeMap::new(), &before)?;
    let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
    let incomplete = |error: anyhow::Error, before: Option<&Capture>, after: Option<&Capture>| {
        let mut report = SemanticDiff::incomplete(
            format!("{error:#}").replace(&scratch.0.to_string_lossy().to_string(), "."),
        );
        let old = before.map_or(&[][..], |c| c.baseline.diagnostics.as_slice());
        let new = after.map_or(&[][..], |c| c.baseline.diagnostics.as_slice());
        report.summary.diagnostics = Some(Sides {
            base: old.to_vec(),
            target: new.to_vec(),
        });
        report.summary.contract_violations =
            Some(old.iter().chain(new).filter(|d| d.code == "E2610").count());
        report
    };
    let base_analysis = resolve_entry(options, &before).and_then(|entry| {
        analyze(options, &scratch, &mut revision, &entry).map(|capture| (entry, capture))
    });
    let mut report = match base_analysis {
        Err(error) => incomplete(error, None, None),
        Ok((base_entry, a)) => match resolve_entry(options, &target) {
            Err(error) => incomplete(error, Some(&a), None),
            Ok(target_entry)
                if before.contains_key("project.toml") != target.contains_key("project.toml")
                    || base_entry != target_entry =>
            {
                incomplete(
                    anyhow::anyhow!("project analysis scope changed between captured trees"),
                    Some(&a),
                    None,
                )
            }
            Ok(target_entry) => match scratch
                .write(&before, &target)
                .and_then(|()| analyze(options, &scratch, &mut revision, &target_entry))
            {
                Err(error) => incomplete(error, Some(&a), None),
                Ok(b) => compare(&a, &b, options.fail_on_growth)
                    .unwrap_or_else(|error| incomplete(error, Some(&a), Some(&b))),
            },
        },
    };
    report.base = Some(RevisionDescription {
        revision: options.base.clone(),
        digest: before_digest,
    });
    report.target = Some(RevisionDescription {
        revision: "working-tree".into(),
        digest: target_digest.clone(),
    });
    check_target(&mut report, &root, &target_digest)?;
    Ok(report)
}
fn check_target(report: &mut SemanticDiff, root: &Path, digest: &str) -> Result<()> {
    if hash_serialized(&tree::target(root)?)? != digest {
        report.status = ReportStatus::Incomplete;
        report
            .summary
            .uncertainty
            .push("snapshot changed during verification".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "semantic_diff_tests.rs"]
mod tests;
