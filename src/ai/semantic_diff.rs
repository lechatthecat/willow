//! Git revision semantic verification, independent of legacy edit JSON.
use super::*;
#[path = "semantic_diff_tree.rs"]
mod tree;
pub use tree::resolve_base;
#[path = "semantic_diff_impact.rs"]
mod impact;

#[derive(Debug)]
pub struct Options {
    pub root: std::path::PathBuf,
    /// None selects the default independently from each captured manifest.
    pub entry: Option<std::path::PathBuf>,
    pub project: bool,
    pub base: String,
    pub fail_on_growth: bool,
}
#[derive(Debug, Serialize)]
pub struct SemanticDiff {
    pub schema: &'static str,
    pub status: &'static str,
    pub base: Value,
    pub target: Value,
    pub changes: Vec<Value>,
    pub summary: Value,
}
impl SemanticDiff {
    pub fn exit_code(&self) -> i32 {
        if self.status == "incomplete" {
            3
        } else if self.summary["contract_violations"].as_u64().unwrap_or(0) != 0
            || self.summary["policy_violations"].as_u64().unwrap_or(0) != 0
        {
            1
        } else {
            0
        }
    }
    pub fn incomplete(reason: String) -> Self {
        Self {
            schema: "willow.semantic-diff.v1",
            status: "incomplete",
            base: Value::Null,
            target: Value::Null,
            changes: vec![],
            summary: json!({"uncertainty":[reason]}),
        }
    }
    pub fn human(&self, explain: bool) -> String {
        fn list(value: &Value) -> String {
            value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "none".into())
        }
        let mut text = format!("Semantic verification: {}\n", self.status);
        if let Some(digest) = self.base["digest"].as_str() {
            text.push_str(&format!("Base digest: {digest}\n"));
        }
        if let Some(digest) = self.target["digest"].as_str() {
            text.push_str(&format!("Target digest: {digest}\n"));
        }
        for change in &self.changes {
            text.push_str(&format!(
                "{}: {}\n",
                change["symbol"].as_str().unwrap_or("?"),
                change["match"].as_str().unwrap_or("?")
            ));
            text.push_str(&format!(
                "  Effects added: {}; removed: {}\n",
                list(&change["effects"]["added"]),
                list(&change["effects"]["removed"])
            ));
            text.push_str(&format!(
                "  Contract {}: {} -> {}\n",
                change["contract"]["change"].as_str().unwrap_or("unknown"),
                list(&change["contract"]["base"]),
                list(&change["contract"]["target"])
            ));
            text.push_str(&format!(
                "  Contract violations: base {}; target {}\n",
                list(&change["contract"]["base_violations"]),
                list(&change["contract"]["target_violations"])
            ));
            if change["public_api"] != "unchanged" {
                text.push_str(&format!(
                    "  Public API: {}\n",
                    change["public_api"].as_str().unwrap_or("unknown")
                ));
            }
            if change["targets"]["base"] != change["targets"]["target"] {
                text.push_str(&format!(
                    "  Call targets: {} -> {}\n",
                    list(&change["targets"]["base"]),
                    list(&change["targets"]["target"])
                ));
            }
            if explain {
                for cause in change["cause"].as_array().into_iter().flatten() {
                    let chain = cause["chain"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" -> ");
                    text.push_str(&format!(
                        "  cause: {} {} via {} ({})\n",
                        cause["side"].as_str().unwrap_or("?"),
                        list(&cause["effect"]),
                        chain,
                        cause["certainty"].as_str().unwrap_or("missing-source")
                    ));
                    if let Some(path) = cause["location"]["path"].as_str() {
                        text.push_str(&format!(
                            "    {path}:{}-{}\n",
                            cause["location"]["start"], cause["location"]["end"]
                        ));
                    }
                }
                text.push_str(&format!(
                    "  Impacted callers: {}\n",
                    list(&change["impacted_callers"])
                ));
            }
        }
        text.push_str(&format!(
            "Contract violations: {}; policy violations: {}\n",
            self.summary["contract_violations"].as_u64().unwrap_or(0),
            self.summary["policy_violations"].as_u64().unwrap_or(0)
        ));
        for reason in self.summary["uncertainty"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            text.push_str(&format!("Incomplete: {reason}\n"));
        }
        text
    }
}
struct Capture {
    baseline: Baseline,
    contracts: BTreeMap<String, BTreeSet<String>>,
    causes: CausePaths,
    ambiguous: BTreeSet<String>,
}
#[derive(Default)]
struct CausePaths {
    next: BTreeMap<(String, u8), String>,
    leaves: BTreeMap<(String, u8), Value>,
    unknown: BTreeSet<String>,
}
impl CausePaths {
    fn render(&self, key: &str, bit: u8) -> Value {
        let mut chain = vec![key.to_owned()];
        let mut cursor = key;
        while let Some(target) = self.next.get(&(cursor.to_owned(), bit)) {
            chain.push(target.clone());
            cursor = target;
        }
        let leaf = self.leaves.get(&(cursor.to_owned(), bit));
        json!({"effect":effect_names(bit), "chain":chain,
            "location":leaf.map(|v| &v["location"]),
            "certainty":leaf.map_or(if self.unknown.contains(key) {"missing-source"} else {"conservative-bound"}, |v| v["certainty"].as_str().unwrap())})
    }
}
fn symbol_stem(s: &symbols::Symbol, workspace: &str) -> Option<String> {
    let location = s.location.as_ref()?;
    if super::super::bundled_source(&location.path).is_some() {
        return None;
    }
    Some(format!(
        "{}::{}::{}",
        relative(&location.path, workspace),
        s.kind,
        s.source_name.as_deref().unwrap_or(&s.name)
    ))
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
            _ => 0,
        }
    })
}
impl Capture {
    fn from_snapshot(snapshot: &Snapshot, diagnostics: Vec<DiagnosticFact>) -> Result<Self> {
        let mut baseline = Baseline::from_snapshot(snapshot, diagnostics)?;
        let mut declarations: Vec<_> = snapshot
            .semantic
            .symbols
            .iter()
            .filter(|s| {
                matches!(
                    s.kind.as_str(),
                    "function"
                        | "method"
                        | "constructor"
                        | "class"
                        | "interface"
                        | "enum"
                        | "variant"
                        | "field"
                        | "static-field"
                        | "const"
                )
            })
            .collect();
        declarations.sort_by_key(|s| s.location.as_ref().map(|l| (&l.path, l.start, l.end)));
        let mut ordinals = BTreeMap::new();
        let mut ids = BTreeMap::new();
        let mut contracts = BTreeMap::new();
        for s in declarations {
            let Some(stem) = symbol_stem(s, &snapshot.workspace) else {
                continue;
            };
            let ordinal = ordinals.entry(stem.clone()).or_insert(0usize);
            let key = format!("{stem}#{ordinal}");
            *ordinal += 1;
            ids.insert(s.id.clone(), key.clone());
            contracts.insert(key, s.details.effect_contracts.iter().cloned().collect());
        }
        for f in &snapshot.functions {
            ids.entry(f.id.clone()).or_insert_with(|| {
                format!(
                    "{}::function::{}",
                    relative(&f.module, &snapshot.workspace),
                    f.name
                )
            });
        }
        // Compiler waiting summaries apply async transmission masks. Legacy
        // graph aggregation is intentionally unchanged; use those summaries
        // here so merely creating a Task does not introduce a wait.
        for f in &snapshot.functions {
            if let Some(fact) = baseline.facts.get_mut(&ids[&f.id]) {
                let waiting = RuntimeEffects::MAY_BLOCK
                    .union(RuntimeEffects::MAY_SUSPEND)
                    .bits();
                let bits = snapshot
                    .semantic
                    .compiler_effects
                    .get(&f.id)
                    .map_or(f.runtime_effects, |checked| {
                        (f.runtime_effects & !waiting) | (checked & waiting)
                    });
                fact.effects = Some(EffectState {
                    may: bits,
                    unknown: if f.unknown {
                        RuntimeEffects::ALL.bits() & !bits
                    } else {
                        0
                    },
                });
            }
        }
        baseline.reindex()?;
        let ambiguous = ordinals
            .into_iter()
            .filter(|(_, n)| *n > 1)
            .map(|(s, _)| s)
            .collect();
        let mut causes = CausePaths::default();
        let functions: BTreeMap<_, _> = snapshot
            .functions
            .iter()
            .map(|f| (f.id.as_str(), f))
            .collect();
        let mut reverse: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for f in &snapshot.functions {
            for callee in &f.callees {
                reverse.entry(callee).or_default().push(&f.id);
            }
        }
        for callers in reverse.values_mut() {
            callers.sort_by_key(|id| &ids[*id]);
            callers.dedup();
        }
        let mut ordered: Vec<_> = snapshot.functions.iter().collect();
        ordered.sort_by_key(|f| &ids[&f.id]);
        // A deterministic next-hop forest per capability, shared by all reports.
        for i in 0..RuntimeEffects::BIT_COUNT {
            let bit = RuntimeEffects::from_bit(i).bits();
            let mut next: BTreeMap<&str, &str> = BTreeMap::new();
            let mut leaves = BTreeMap::new();
            let mut queue = VecDeque::new();
            for f in &ordered {
                let mut witnesses: Vec<_> = snapshot
                    .semantic
                    .witnesses
                    .get(&f.id)
                    .into_iter()
                    .flatten()
                    .filter(|w| w["effect"] == bit && !w["witness"]["cause"]["location"].is_null())
                    .collect();
                witnesses.sort_by_cached_key(|w| w.to_string());
                if let Some(w) = witnesses.first() {
                    let mut location = w["witness"]["cause"]["location"].clone();
                    if let Some(path) = location["path"].as_str() {
                        location["path"] = json!(relative(path, &snapshot.workspace));
                    }
                    let certainty = if w["witness"]["cause"]["is_conservative"] == true {
                        "conservative-bound"
                    } else {
                        w["witness"]["certainty"]
                            .as_str()
                            .unwrap_or("compiler-proven")
                    };
                    leaves.insert(
                        f.id.as_str(),
                        json!({"location":location,"certainty":certainty}),
                    );
                    next.insert(&f.id, &f.id);
                    queue.push_back(f.id.as_str());
                }
            }
            while let Some(callee) = queue.pop_front() {
                for &caller in reverse.get(callee).into_iter().flatten() {
                    if functions[caller].runtime_effects & bit != 0
                        && !next.contains_key(caller)
                        && !(bit
                            & RuntimeEffects::MAY_BLOCK
                                .union(RuntimeEffects::MAY_SUSPEND)
                                .bits()
                            != 0
                            && snapshot.semantic.async_functions.contains(callee))
                    {
                        next.insert(caller, callee);
                        queue.push_back(caller);
                    }
                }
            }
            for (id, target) in next {
                if id != target {
                    causes
                        .next
                        .insert((ids[id].clone(), bit), ids[target].clone());
                }
            }
            for (id, leaf) in leaves {
                causes.leaves.insert((ids[id].clone(), bit), leaf);
            }
        }
        causes.unknown = ordered
            .iter()
            .filter(|f| f.unknown && super::super::bundled_source(&f.module).is_none())
            .map(|f| ids[&f.id].clone())
            .collect();
        Ok(Self {
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
    let session = crate::CompilerSession::new(
        entry.to_str().context("non UTF-8 entry")?,
        "",
        &compiler_options,
        project.then(|| scratch.0.clone()),
    );
    let mut diagnostics = Diagnostics::default();
    let mut snapshot = revision
        .analyze_verification(session, &mut diagnostics)
        .map_err(|error| {
            let messages = diagnostics
                .facts
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::anyhow!("{error:#}: {messages}")
        })?;
    for path in snapshot.sources.keys() {
        ensure!(
            Path::new(path).starts_with(&scratch.0) || super::super::bundled_source(path).is_some(),
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
    Capture::from_snapshot(&snapshot, diagnostics.facts)
}
fn compare(before: &Capture, after: &Capture, fail_on_growth: bool) -> Result<SemanticDiff> {
    let old = &before.baseline;
    let new = &after.baseline;
    let legacy = old.compare(new)?;
    let keys: BTreeSet<_> = old.facts.keys().chain(new.facts.keys()).collect();
    let empty = BTreeSet::new();
    let mut changes = Vec::new();
    let mut growth = 0;
    let mut emitted_hops = 0usize;
    let mut uncertainty = Vec::new();
    for stem in before.ambiguous.union(&after.ambiguous) {
        uncertainty.push(format!("ambiguous symbol: {stem}"));
        changes.push(json!({"symbol":stem,"match":"ambiguous","effects":null,"cause":[],"impacted_callers":[],"contract":null,"public_api":"unknown","targets":null}));
    }
    for key in before.causes.unknown.union(&after.causes.unknown) {
        uncertainty.push(format!("missing-source: {key}"));
    }
    for key in keys {
        let a = old.facts.get(key);
        let b = new.facts.get(key);
        let ca = before.contracts.get(key).unwrap_or(&empty);
        let cb = after.contracts.get(key).unwrap_or(&empty);
        let ae = a
            .and_then(|f| f.effects.as_ref())
            .map_or(0, |e| e.may | e.unknown);
        let be = b
            .and_then(|f| f.effects.as_ref())
            .map_or(0, |e| e.may | e.unknown);
        let stem = key.rsplit_once('#').map_or(key.as_str(), |(stem, _)| stem);
        if before.ambiguous.contains(stem) || after.ambiguous.contains(stem) {
            continue;
        }
        let added = be & !ae;
        let removed = ae & !be;
        let av = ae & contract_bits(ca);
        let bv = be & contract_bits(cb);
        if added != 0 {
            growth += 1;
        }
        let signature_a = a.and_then(|f| f.signature.as_ref());
        let signature_b = b.and_then(|f| f.signature.as_ref());
        let targets_a = a.map(|f| &f.callees);
        let targets_b = b.map(|f| &f.callees);
        let effects_changed =
            a.and_then(|f| f.effects.as_ref()) != b.and_then(|f| f.effects.as_ref());
        if signature_a == signature_b
            && !effects_changed
            && ca == cb
            && targets_a == targets_b
            && av == 0
            && bv == 0
        {
            continue;
        }
        let matching = if a.is_none() {
            "added"
        } else if b.is_none() {
            "removed"
        } else if signature_a != signature_b {
            "signature-changed"
        } else {
            "exact"
        };
        let contract_change = if ca == cb {
            "unchanged"
        } else if ca.is_empty() {
            "added"
        } else if cb.is_empty() {
            "removed"
        } else if contract_bits(ca) == contract_bits(cb) {
            "equivalent"
        } else if contract_bits(ca) & !contract_bits(cb) == 0 {
            "strengthened"
        } else if contract_bits(cb) & !contract_bits(ca) == 0 {
            "weakened"
        } else {
            "changed"
        };
        let mut causes = Vec::new();
        for i in 0..RuntimeEffects::BIT_COUNT {
            let bit = RuntimeEffects::from_bit(i).bits();
            for (capture, bits, side) in [
                (before, removed | av, "base"),
                (after, added | bv, "target"),
            ] {
                if bits & bit != 0 {
                    let mut cause = capture.causes.render(key, bit);
                    emitted_hops += cause["chain"].as_array().map_or(0, Vec::len);
                    ensure!(
                        emitted_hops <= 1_000_000,
                        "witness evidence exceeds output limit"
                    );
                    cause["side"] = json!(side);
                    causes.push(cause);
                }
            }
        }
        let public_api = match (public_signature(signature_a), public_signature(signature_b)) {
            (true, false) => "removed",
            (false, true) => "added",
            (true, true) if signature_a != signature_b => "signature-changed",
            _ => "unchanged",
        };
        changes.push(json!({"symbol":key,"match":matching,
            "signature":{"base":signature_a,"target":signature_b},
            "effects":{"added":effect_names(added),"removed":effect_names(removed),"base":effect_names(ae),"target":effect_names(be)},
            "cause":causes,"impacted_callers":[],
            "contract":{"change":contract_change,"base":ca,"target":cb,"base_violations":effect_names(av),"target_violations":effect_names(bv)},
            "public_api":public_api,
            "targets":{"base":targets_a,"target":targets_b}}));
    }
    impact::assign(&mut changes, old, new)?;
    let violations = old
        .diagnostics
        .iter()
        .chain(&new.diagnostics)
        .filter(|d| d.code == "E2610")
        .count();
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
    let status = if legacy.complete && uncertainty.is_empty() {
        "complete"
    } else {
        "incomplete"
    };
    Ok(SemanticDiff {
        schema: "willow.semantic-diff.v1",
        status,
        base: Value::Null,
        target: Value::Null,
        changes,
        summary: json!({"contract_violations":violations,"policy_violations":if fail_on_growth {growth} else {0},
            "effect_growth":growth,"impacted_callers":legacy.affected_callers,"uncertainty":uncertainty,
            "diagnostics":{"base":old.diagnostics,"target":new.diagnostics}}),
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
        report.summary["diagnostics"] = json!({"base":old,"target":new});
        report.summary["contract_violations"] =
            json!(old.iter().chain(new).filter(|d| d.code == "E2610").count());
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
    report.base = json!({"revision":options.base,"digest":before_digest});
    report.target = json!({"revision":"working-tree","digest":target_digest});
    check_target(&mut report, &root, &target_digest)?;
    Ok(report)
}
fn check_target(report: &mut SemanticDiff, root: &Path, digest: &str) -> Result<()> {
    if hash_serialized(&tree::target(root)?)? != digest {
        report.status = "incomplete";
        report.summary["uncertainty"]
            .as_array_mut()
            .context("uncertainty array")?
            .push(json!("snapshot changed during verification"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "semantic_diff_tests.rs"]
mod tests;
