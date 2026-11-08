//! Retained semantic-report projections. Updates visit the changed owners and
//! their reverse closure; immutable maps preserve the baseline for comparison.
use super::*;
use crate::qualified_name::QualifiedName;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Coordinate maps are captured with source input, independently of semantic
/// rebuilding. A spatial index selects only moved witness coordinates.
#[derive(Default)]
pub(super) struct RelocationIndex(BTreeMap<String, Vec<(usize, usize, usize)>>);
impl RelocationIndex {
    fn new(changes: &[crate::ai::VerificationRelocation], workspace: &str) -> Self {
        let mut result = Self::default();
        for change in changes {
            result
                .0
                .entry(relative(&change.path, workspace))
                .or_default()
                .push((change.old_start, change.old_end, change.new_start));
        }
        for ranges in result.0.values_mut() {
            ranges.sort_unstable();
        }
        result
    }
    pub(super) fn apply(&self, location: &mut Value) {
        let Some(path) = location["path"].as_str() else {
            return;
        };
        let Some(ranges) = self.0.get(path) else {
            return;
        };
        let (Some(start), Some(end)) = (location["start"].as_u64(), location["end"].as_u64())
        else {
            return;
        };
        let at = ranges.partition_point(|(old, _, _)| *old <= start as usize);
        if let Some(&(old_start, old_end, new_start)) =
            at.checked_sub(1).and_then(|i| ranges.get(i))
            && end as usize <= old_end
        {
            location["start"] = json!(new_start + (start as usize - old_start));
            location["end"] = json!(new_start + (end as usize - old_start));
        }
    }
}

impl CausePaths {
    fn remove_leaf(&mut self, address: &(String, u8)) {
        if let Some(value) = self.leaves.remove(address)
            && let (Some(path), Some(start)) = (
                value["location"]["path"].as_str(),
                value["location"]["start"].as_u64(),
            )
        {
            self.locations.remove(&(
                path.to_owned(),
                start as usize,
                address.0.clone(),
                address.1,
            ));
        }
    }
    fn insert_leaf(&mut self, address: (String, u8), value: Value) {
        self.remove_leaf(&address);
        if let (Some(path), Some(start), Some(end)) = (
            value["location"]["path"].as_str(),
            value["location"]["start"].as_u64(),
            value["location"]["end"].as_u64(),
        ) {
            self.locations.insert(
                (
                    path.to_owned(),
                    start as usize,
                    address.0.clone(),
                    address.1,
                ),
                end as usize,
            );
        }
        self.leaves.insert(address, value);
    }
    // This is coordinate input repair, not semantic witness reconstruction.
    // A persistent spatial index touches each moved leaf at most once and keeps
    // only current coordinates; old captures retain their own map roots.
    fn relocate(
        &mut self,
        changes: &[crate::ai::VerificationRelocation],
        workspace: &str,
    ) -> usize {
        let maps = RelocationIndex::new(changes, workspace);
        let mut moved = Vec::new();
        for (path, ranges) in &maps.0 {
            let mut coverage: Vec<(usize, usize)> = Vec::new();
            for &(start, end, _) in ranges {
                if let Some(last) = coverage.last_mut()
                    && start <= last.1
                {
                    last.1 = last.1.max(end);
                } else {
                    coverage.push((start, end));
                }
            }
            for (start, end) in coverage {
                for ((p, at, owner, bit), _) in self
                    .locations
                    .range((path.clone(), start, String::new(), 0)..)
                {
                    if p != path || *at > end {
                        break;
                    }
                    moved.push((owner.clone(), *bit));
                }
            }
        }
        let visits = moved.len();
        for address in moved {
            if let Some(old) = self.leaves.get(&address) {
                let mut value = old.clone();
                maps.apply(&mut value["location"]);
                if &value != old {
                    self.insert_leaf(address, value);
                }
            }
        }
        visits
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Route {
    distance: usize,
    root: String,
    next: String,
}

#[derive(Clone)]
pub(super) struct ReportState {
    pub(super) functions: im::OrdMap<String, std::sync::Arc<Function>>,
    pub(super) ids: im::OrdMap<String, String>,
    types: Arc<display::TypeNames>,
    reverse: im::OrdMap<String, im::OrdSet<String>>,
    asynchronous: im::OrdSet<String>,
    routes: im::OrdMap<(String, u8), Route>,
}
impl ReportState {
    pub(super) fn new(
        snapshot: &Snapshot,
        ids: im::OrdMap<String, String>,
        types: Arc<display::TypeNames>,
    ) -> Self {
        let mut reverse: im::OrdMap<String, im::OrdSet<String>> = im::OrdMap::new();
        for function in &snapshot.functions {
            for callee in &function.callees {
                reverse
                    .entry(callee.clone())
                    .or_default()
                    .insert(function.id.clone());
            }
        }
        Self {
            functions: snapshot
                .functions
                .iter()
                .map(|f| (f.id.clone(), std::sync::Arc::new(f.clone())))
                .collect(),
            ids,
            types,
            reverse,
            asynchronous: snapshot.semantic.async_functions.iter().cloned().collect(),
            routes: im::OrdMap::new(),
        }
    }
    fn closure(&self, seeds: impl IntoIterator<Item = String>) -> (BTreeSet<String>, usize) {
        let mut affected: BTreeSet<_> = seeds.into_iter().collect();
        let mut edges = 0;
        let mut pending: VecDeque<_> = affected.iter().cloned().collect();
        while let Some(id) = pending.pop_front() {
            for caller in self.reverse.get(&id).into_iter().flatten() {
                edges += 1;
                if affected.insert(caller.clone()) {
                    pending.push_back(caller.clone());
                }
            }
        }
        (affected, edges)
    }
    pub(super) fn replace_leaves(&mut self, patch: &Snapshot, causes: &mut CausePaths) {
        for f in &patch.functions {
            let key = &self.ids[&f.id];
            if f.unknown && super::super::super::bundled_source(&f.module).is_none() {
                causes.unknown.insert(key.clone());
            } else {
                causes.unknown.remove(key);
            }
            for i in 0..RuntimeEffects::BIT_COUNT {
                let bit = RuntimeEffects::from_bit(i).bits();
                let address = (key.clone(), bit);
                causes.remove_leaf(&address);
                let witness = patch
                    .semantic
                    .witnesses
                    .get(&f.id)
                    .into_iter()
                    .flatten()
                    .filter(|w| w["effect"] == bit && !w["witness"]["cause"]["location"].is_null())
                    .min_by_key(|w| w.to_string());
                if let Some(w) = witness {
                    let mut location = w["witness"]["cause"]["location"].clone();
                    if let Some(path) = location["path"].as_str() {
                        location["path"] = json!(relative(path, &patch.workspace));
                    }
                    let certainty = if w["witness"]["cause"]["is_conservative"] == true {
                        "conservative-bound"
                    } else {
                        w["witness"]["certainty"]
                            .as_str()
                            .unwrap_or("compiler-proven")
                    };
                    causes.insert_leaf(address, json!({"location":location,"certainty":certainty}));
                }
            }
        }
    }
    fn transmits(&self, caller: &str, callee: &str, bit: u8) -> bool {
        self.functions
            .get(caller)
            .is_some_and(|f| f.runtime_effects & bit != 0)
            && !(bit
                & RuntimeEffects::MAY_BLOCK
                    .union(RuntimeEffects::MAY_SUSPEND)
                    .bits()
                != 0
                && self.asynchronous.contains(callee))
    }
    /// Nearest source witness, then stable source identity and next-hop identity.
    /// Distance strictly decreases along next hops, including within cycles.
    pub(super) fn repair_causes(
        &mut self,
        affected: &BTreeSet<String>,
        causes: &mut CausePaths,
    ) -> (usize, usize, usize) {
        let mut visits = 0;
        let mut edge_visits = 0;
        let mut queue_operations = 0;
        for i in 0..RuntimeEffects::BIT_COUNT {
            let bit = RuntimeEffects::from_bit(i).bits();
            for id in affected {
                self.routes.remove(&(id.clone(), bit));
                if let Some(key) = self.ids.get(id) {
                    causes.next.remove(&(key.clone(), bit));
                }
            }
            let mut queue = BinaryHeap::new();
            for id in affected {
                let Some(f) = self.functions.get(id) else {
                    continue;
                };
                visits += 1;
                if causes.leaves.contains_key(&(self.ids[id].clone(), bit)) {
                    queue_operations += 1;
                    queue.push(Reverse((
                        Route {
                            distance: 0,
                            root: self.ids[id].clone(),
                            next: self.ids[id].clone(),
                        },
                        id.clone(),
                        id.clone(),
                    )));
                }
                for callee in &f.callees {
                    edge_visits += 1;
                    if !affected.contains(callee)
                        && self.transmits(id, callee, bit)
                        && let Some(route) = self.routes.get(&(callee.clone(), bit))
                    {
                        queue_operations += 1;
                        queue.push(Reverse((
                            Route {
                                distance: route.distance + 1,
                                root: route.root.clone(),
                                next: self.ids[callee].clone(),
                            },
                            id.clone(),
                            callee.clone(),
                        )));
                    }
                }
            }
            while let Some(Reverse((route, id, next))) = queue.pop() {
                queue_operations += 1;
                let address = (id.clone(), bit);
                if self.routes.get(&address).is_some_and(|old| old <= &route) {
                    continue;
                }
                self.routes.insert(address, route.clone());
                if id != next {
                    causes
                        .next
                        .insert((self.ids[&id].clone(), bit), self.ids[&next].clone());
                }
                for caller in self.reverse.get(&id).into_iter().flatten() {
                    edge_visits += 1;
                    if affected.contains(caller) && self.transmits(caller, &id, bit) {
                        queue_operations += 1;
                        queue.push(Reverse((
                            Route {
                                distance: route.distance + 1,
                                root: route.root.clone(),
                                next: self.ids[&id].clone(),
                            },
                            caller.clone(),
                            id.clone(),
                        )));
                    }
                }
            }
        }
        (visits, edge_visits, queue_operations)
    }
}

impl Capture {
    pub(super) fn update(
        &self,
        patch: &Snapshot,
        removed_functions: &[String],
        removed_symbols: &[String],
        relocations: &[crate::ai::VerificationRelocation],
        diagnostics: Vec<DiagnosticFact>,
    ) -> Result<Self> {
        // The producer supplies ordinary declaration and body deltas. Ambiguous
        // identities and structural namespace changes use its full fallback.
        let mut next = self.clone();
        next.coordinate_updates = next.causes.relocate(relocations, &patch.workspace);
        let seeds = patch
            .functions
            .iter()
            .map(|f| f.id.clone())
            .chain(removed_functions.iter().cloned());
        let (mut affected, mut graph_edges) = next.retained.closure(seeds);
        for id in patch
            .functions
            .iter()
            .map(|f| &f.id)
            .chain(removed_functions)
        {
            if let Some(old) = next.retained.functions.get(id) {
                graph_edges += old.callees.len();
                for callee in &old.callees {
                    if let Some(callers) = next.retained.reverse.get_mut(callee) {
                        callers.remove(id);
                    }
                }
            }
        }
        for id in removed_functions {
            next.retained.functions.remove(id);
            next.retained.asynchronous.remove(id);
            next.retained.reverse.remove(id);
        }
        for id in removed_symbols.iter().chain(removed_functions) {
            if let Some(key) = next.retained.ids.get(id) {
                next.shapes.remove(key);
                if matching::is_lambda(key)
                    && let Some(owner) = matching::scope(key)
                    && let Some(lambdas) = next.lambda_owners.get_mut(&owner)
                {
                    lambdas.remove(key);
                    if lambdas.is_empty() {
                        next.lambda_owners.remove(&owner);
                    }
                }
                if let Some(old) = next.baseline.facts.remove(key) {
                    next.baseline.public_count -=
                        usize::from(public_signature(old.signature.as_ref()));
                    graph_edges += old.callees.len();
                    for callee in &old.callees {
                        if let Some(callers) = next.baseline.callers.get_mut(callee) {
                            callers.remove(key);
                        }
                    }
                }
                next.baseline.callers.remove(key);
                next.baseline.refresh_effect_fact_index(key)?;
                next.contracts.remove(key);
                next.violations.remove(key);
                Node::remove(&mut next.contract_index, &hash(key), key);
                for bit in 0..RuntimeEffects::BIT_COUNT {
                    let address = (key.clone(), RuntimeEffects::from_bit(bit).bits());
                    next.causes.remove_leaf(&address);
                }
                next.causes.unknown.remove(key);
            }
        }
        for symbol in &patch.semantic.symbols {
            if !matches!(
                symbol.kind.as_str(),
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
            ) {
                continue;
            }
            let Some(location) = &symbol.location else {
                continue;
            };
            if super::super::super::bundled_source(&location.path).is_some() {
                continue;
            }
            let key = next
                .retained
                .ids
                .get(&symbol.id)
                .cloned()
                .unwrap_or_else(|| {
                    QualifiedName::join(
                        QualifiedName::joined(
                            relative(&location.path, &patch.workspace),
                            &symbol.kind,
                        ),
                        format_args!(
                            "{}#0",
                            symbol.source_name.as_deref().unwrap_or(&symbol.name)
                        ),
                    )
                });
            next.retained.ids.insert(symbol.id.clone(), key.clone());
            let mut fact = next.baseline.facts.get(&key).map_or(
                Fact {
                    signature: None,
                    effects: None,
                    body: String::new(),
                    callees: vec![],
                },
                |f| (**f).clone(),
            );
            next.baseline.public_count -= usize::from(public_signature(fact.signature.as_ref()));
            fact.signature = Some(declaration_signature(symbol, &next.retained.types));
            next.baseline.public_count += usize::from(public_signature(fact.signature.as_ref()));
            next.baseline.facts.insert(key.clone(), Arc::new(fact));
            next.baseline.refresh_effect_fact_index(&key)?;
            if symbol.details.effect_contracts.is_empty() {
                next.contracts.remove(&key);
                next.violations.remove(&key);
                Node::remove(&mut next.contract_index, &hash(&key), &key);
            } else {
                let contract: BTreeSet<_> =
                    symbol.details.effect_contracts.iter().cloned().collect();
                Node::insert(
                    &mut next.contract_index,
                    &hash(&key),
                    &key,
                    hash_serialized(&contract)?,
                );
                next.contracts.insert(key, contract);
            }
        }
        // Establish all new function identities before resolving their edges.
        for f in &patch.functions {
            if !next.retained.ids.contains_key(&f.id) {
                let key = QualifiedName::from_segments([
                    relative(&f.module, &patch.workspace).as_str(),
                    "function",
                    &f.name,
                ]);
                next.retained.ids.insert(f.id.clone(), key.clone());
                next.baseline.facts.insert(
                    key,
                    Arc::new(Fact {
                        signature: None,
                        effects: None,
                        body: String::new(),
                        callees: vec![],
                    }),
                );
            }
        }
        for f in &patch.functions {
            next.retained
                .functions
                .insert(f.id.clone(), std::sync::Arc::new(f.clone()));
            if patch.semantic.async_functions.contains(&f.id) {
                next.retained.asynchronous.insert(f.id.clone());
            } else {
                next.retained.asynchronous.remove(&f.id);
            }
            graph_edges += f.callees.len();
            for callee in &f.callees {
                ensure!(
                    next.retained.ids.contains_key(callee),
                    "missing retained callee"
                );
                next.retained
                    .reverse
                    .entry(callee.clone())
                    .or_default()
                    .insert(f.id.clone());
            }
        }
        let (new_affected, new_edges) = next.retained.closure(affected.iter().cloned());
        affected.extend(new_affected);
        graph_edges += new_edges;
        // Assign changed references with the same ordered interval sweep as cold
        // capture, including nested callable ownership. Unchanged declarations
        // are resolved through the retained identity table.
        let mut owners: BTreeMap<&str, Vec<(usize, usize, &str)>> = BTreeMap::new();
        for f in &patch.functions {
            for location in &f.locations {
                owners.entry(&location.path).or_default().push((
                    location.start,
                    location.end,
                    &f.id,
                ));
            }
        }
        let mut refs: BTreeMap<&str, Vec<_>> = BTreeMap::new();
        for r in &patch.semantic.references {
            if let Some(key) = next.retained.ids.get(&r.target) {
                refs.entry(&r.location.path)
                    .or_default()
                    .push((&r.location, key));
            }
        }
        let mut extra: HashMap<&str, Vec<String>> = HashMap::new();
        for (path, refs) in &mut refs {
            let Some(ranges) = owners.get_mut(path) else {
                continue;
            };
            ranges.sort_by_key(|(start, end, _)| (*start, Reverse(*end)));
            refs.sort_by_key(|(location, _)| location.start);
            let mut cursor = 0;
            let mut stack: Vec<(usize, usize, &str)> = Vec::new();
            for (location, target) in refs {
                while cursor < ranges.len() && ranges[cursor].0 <= location.start {
                    let range = ranges[cursor];
                    cursor += 1;
                    while stack.last().is_some_and(|r| r.1 <= range.0) {
                        stack.pop();
                    }
                    stack.push(range);
                }
                while stack.last().is_some_and(|r| r.1 < location.end) {
                    stack.pop();
                }
                if let Some((_, _, id)) = stack.last()
                    && next.retained.ids[*id] != **target
                {
                    extra.entry(id).or_default().push((*target).clone());
                }
            }
        }
        next.fact_updates = removed_functions.len();
        for f in &patch.functions {
            let key = &next.retained.ids[&f.id];
            let old = next
                .baseline
                .facts
                .get(key)
                .context("retained function fact")?
                .clone();
            let mut fact = (*old).clone();
            fact.body = f.body_fingerprint.clone();
            fact.callees = f
                .callees
                .iter()
                .map(|id| next.retained.ids[id].clone())
                .chain(extra.remove(f.id.as_str()).unwrap_or_default())
                .collect();
            fact.callees.sort();
            fact.callees.dedup();
            let waiting = RuntimeEffects::MAY_BLOCK
                .union(RuntimeEffects::MAY_SUSPEND)
                .bits();
            let bits = patch
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
            graph_edges += old.callees.len() + fact.callees.len();
            for callee in &old.callees {
                if let Some(callers) = next.baseline.callers.get_mut(callee) {
                    callers.remove(key);
                }
            }
            for callee in &fact.callees {
                next.baseline
                    .callers
                    .entry(callee.clone())
                    .or_default()
                    .insert(key.clone());
            }
            next.baseline.facts.insert(key.clone(), Arc::new(fact));
            next.baseline.refresh_effect_fact_index(key)?;
            next.fact_updates += 1;
            if let Some(contract) = next.contracts.get(key) {
                if bits & contract_bits(contract) != 0
                    || (f.unknown && contract_bits(contract) != 0)
                {
                    next.violations.insert(key.clone());
                } else {
                    next.violations.remove(key);
                }
            }
        }
        next.graph_edges = graph_edges;
        next.retained.replace_leaves(patch, &mut next.causes);
        (next.witness_updates, next.witness_edges, next.witness_queue) =
            next.retained.repair_causes(&affected, &mut next.causes);
        for id in removed_symbols.iter().chain(removed_functions) {
            if !next.retained.functions.contains_key(id) {
                next.retained.ids.remove(id);
            }
        }
        let relocation = RelocationIndex::new(relocations, &patch.workspace);
        let mut old_sites = HashMap::new();
        for diagnostic in &self.baseline.diagnostics {
            for site in &diagnostic.locations {
                let mut location = json!({"path":site.path,"start":site.start,"end":site.end});
                relocation.apply(&mut location);
                old_sites.insert(
                    (
                        diagnostic.code.clone(),
                        diagnostic.message.clone(),
                        site.path.clone(),
                        location["start"].as_u64().unwrap() as usize,
                        location["end"].as_u64().unwrap() as usize,
                        site.anchor.clone(),
                    ),
                    site.owner.clone(),
                );
            }
        }
        next.baseline.diagnostics = diagnostics;
        let mut diagnostic_sites: BTreeMap<&str, Vec<_>> = BTreeMap::new();
        for diagnostic in &mut next.baseline.diagnostics {
            for site in &mut diagnostic.locations {
                site.owner = old_sites
                    .get(&(
                        diagnostic.code.clone(),
                        diagnostic.message.clone(),
                        relative(&site.path, &patch.workspace),
                        site.start,
                        site.end,
                        site.anchor.clone(),
                    ))
                    .cloned()
                    .flatten();
                diagnostic_sites.entry(&site.path).or_default().push((
                    &mut site.owner,
                    site.start,
                    site.end,
                ));
            }
        }
        for (path, sites) in &mut diagnostic_sites {
            let Some(ranges) = owners.get_mut(path) else {
                continue;
            };
            ranges.sort_by_key(|(start, end, _)| (*start, Reverse(*end)));
            sites.sort_by_key(|(_, start, _)| *start);
            let mut cursor = 0;
            let mut stack: Vec<(usize, usize, &str)> = Vec::new();
            for (owner, start, end) in sites {
                while cursor < ranges.len() && ranges[cursor].0 <= *start {
                    let range = ranges[cursor];
                    cursor += 1;
                    while stack.last().is_some_and(|r| r.1 <= range.0) {
                        stack.pop();
                    }
                    stack.push(range);
                }
                while stack.last().is_some_and(|r| r.1 < *end) {
                    stack.pop();
                }
                if let Some((_, _, id)) = stack.last() {
                    **owner = Some(next.retained.ids[*id].clone());
                }
            }
        }
        next.baseline.normalize_diagnostics(&patch.workspace);
        next.baseline.reindex_diagnostics()?;
        // This private capture is compared in memory; the public report receives
        // the independently captured source-tree digest, not a serialized baseline.
        matching::update_shapes(
            &mut next.shapes,
            &mut next.lambda_owners,
            patch,
            &next.retained.ids,
        )?;
        next.baseline.revision = patch.revision.clone();
        Ok(next)
    }
}
