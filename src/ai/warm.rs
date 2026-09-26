//! Warm revision adapter. Cold frontend validation remains the source of truth
//! on refresh; body query values reuse the compiler's memoization infrastructure.
use super::*;
use crate::compiler_db::{
    analysis::{AnalysisQueries, Key, Kind},
    ids::BodyId,
};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
struct Input {
    body: BodyId,
    fingerprints: [String; 3],
}
pub struct WarmSession {
    session: QuerySession,
    inputs: HashMap<String, Input>,
    cache: AnalysisQueries,
    pub updates: usize,
    pub input_visits: usize,
    pub invalidation_visits: usize,
}
impl WarmSession {
    pub fn new(snapshot: Snapshot, max_entries: usize, max_bytes: usize) -> Result<Self> {
        check_size(&snapshot)?;
        let session = QuerySession::new(snapshot)?;
        let (inputs, visits) = inputs(&session, &HashMap::new())?;
        Ok(Self {
            session,
            inputs,
            cache: AnalysisQueries::new(max_entries, max_bytes),
            updates: 0,
            input_visits: visits,
            invalidation_visits: 0,
        })
    }
    pub fn revision(&self) -> &str {
        self.session.revision()
    }
    pub fn stats(&self) -> Value {
        let stats = self.cache.stats();
        let (entries, bytes) = self.cache.retained();
        json!({"calls":stats.calls,"hits":stats.hits,"computations":stats.computations,"retained_entries":entries,"retained_json_bytes":bytes,
            "updates":self.updates,"input_visits":self.input_visits,"invalidation_visits":self.invalidation_visits})
    }
    /// Install only a fully validated revision. Invalid revisions leave the old
    /// session intact. Deleted owners and obsolete cache entries are discarded.
    pub fn update(&mut self, snapshot: Snapshot) -> Result<()> {
        ensure!(
            snapshot.workspace == self.session.snapshot.workspace
                && snapshot.compatibility == self.session.snapshot.compatibility,
            "warm session input configuration changed; restart required"
        );
        check_size(&snapshot)?;
        let next = QuerySession::new(snapshot)?;
        let (inputs, visits) = inputs(&next, &self.inputs)?;
        let mut invalid = HashSet::new();
        let mut effects = HashSet::new();
        for (id, old) in &self.inputs {
            let new = inputs.get(id);
            for (i, kind) in [Kind::Symbol, Kind::References, Kind::Effects]
                .into_iter()
                .enumerate()
            {
                if new.is_none_or(|new| new.fingerprints[i] != old.fingerprints[i]) {
                    invalid.insert(Key(old.body, kind));
                    if kind == Kind::Effects {
                        effects.insert(id.clone());
                    }
                }
            }
        }
        // Union old/new reverse dependencies so removal and addition of an
        // edge both invalidate every cached transitive witness that used it.
        let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
        for snapshot in [&self.session.snapshot, &next.snapshot] {
            for f in &snapshot.functions {
                for callee in &f.callees {
                    reverse.entry(callee).or_default().push(&f.id);
                }
            }
        }
        let mut queue: VecDeque<String> = effects.iter().cloned().collect();
        while let Some(id) = queue.pop_front() {
            if let Some(input) = self.inputs.get(&id) {
                invalid.insert(Key(input.body, Kind::Effects));
            }
            for &caller in reverse.get(id.as_str()).into_iter().flatten() {
                self.invalidation_visits += 1;
                if effects.insert(caller.into()) {
                    queue.push_back(caller.into());
                }
            }
        }
        self.cache.invalidate(&invalid);
        self.session = next;
        self.inputs = inputs;
        self.updates += 1;
        self.input_visits += visits;
        Ok(())
    }
    pub fn query(&mut self, request: QueryRequest) -> Result<Value> {
        let (revision, function, kind) = match &request {
            QueryRequest::SymbolInfo { revision, function } => (revision, function, Kind::Symbol),
            QueryRequest::References { revision, function } => {
                (revision, function, Kind::References)
            }
            QueryRequest::Effects { revision, function } => (revision, function, Kind::Effects),
            // Position queries already use the V2 logarithmic interval index;
            // retaining arbitrary byte offsets would only consume cache space.
            QueryRequest::Affected { .. }
            | QueryRequest::TypeAt { .. }
            | QueryRequest::Symbols { .. }
            | QueryRequest::SymbolAt { .. } => return Ok(self.session.query(request)),
        };
        if revision != self.revision() {
            return Ok(self.session.query(request));
        }
        let Some(input) = self.inputs.get(function) else {
            return Ok(self.session.query(request));
        };
        let key = Key(input.body, kind);
        let value = self.cache.query(key, || {
            let mut response = self.session.query(request);
            response
                .as_object_mut()
                .context("query response")?
                .remove("revision");
            Ok(response)
        })?;
        let mut response = (*value).clone();
        response
            .as_object_mut()
            .unwrap()
            .insert("revision".into(), json!(self.revision()));
        Ok(response)
    }
}
fn inputs(
    session: &QuerySession,
    previous: &HashMap<String, Input>,
) -> Result<(HashMap<String, Input>, usize)> {
    let snapshot = &session.snapshot;
    let mut references: HashMap<&str, Vec<&semantic::Expression>> = HashMap::new();
    for expression in &snapshot.semantic.expressions {
        if let Some(target) = &expression.target {
            references.entry(target).or_default().push(expression);
        }
    }
    let mut result = HashMap::new();
    for f in &snapshot.functions {
        let body = previous
            .get(&f.id)
            .map(|i| i.body)
            .or(f.body_id)
            .unwrap_or_else(BodyId::fresh);
        let symbol = hash_serialized(f)?;
        let refs = hash_serialized(&(
            session.incomplete_references,
            &f.identity,
            references.get(f.id.as_str()),
        ))?;
        let effects = hash_serialized(&(
            f.runtime_effects,
            f.unknown,
            &f.callees,
            snapshot.semantic.witnesses.get(&f.id),
        ))?;
        result.insert(
            f.id.clone(),
            Input {
                body,
                fingerprints: [symbol, refs, effects],
            },
        );
    }
    Ok((
        result,
        snapshot.functions.len() + snapshot.semantic.expressions.len(),
    ))
}

pub(crate) fn check_size(snapshot: &Snapshot) -> Result<()> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            if self.0 > 64 * 1024 * 1024 {
                return Err(std::io::Error::other("warm snapshot exceeds 64 MiB"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(&mut Counter(0), snapshot)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(n: usize, fanout: bool) -> Snapshot {
        let mut snapshot = Snapshot {
            version: 1,
            compiler: storage::compiler_stamp(),
            compatibility: "test".into(),
            workspace: "/workspace".into(),
            revision: String::new(),
            sources: BTreeMap::new(),
            semantic: SemanticFacts::default(),
            functions: (0..n)
                .map(|i| Function {
                    identity: None,
                    body_id: Some(BodyId::fresh()),
                    body_location: None,
                    rename_calls: vec![],
                    id: format!("f{i}"),
                    module: "m".into(),
                    name: format!("f{i}"),
                    locations: vec![],
                    synthetic: false,
                    fingerprint: "initial".into(),
                    body_fingerprint: "initial".into(),
                    callees: if fanout && i == 0 {
                        (1..n).map(|j| format!("f{j}")).collect()
                    } else {
                        vec![]
                    },
                    runtime_effects: 0,
                    unknown: false,
                    unresolved: vec![],
                })
                .collect(),
        };
        snapshot.revision = snapshot.digest().unwrap();
        snapshot
    }
    fn request(snapshot: &Snapshot, i: usize, kind: Kind) -> QueryRequest {
        let revision = snapshot.revision.clone();
        let function = format!("f{i}");
        match kind {
            Kind::Symbol => QueryRequest::SymbolInfo { revision, function },
            Kind::References => QueryRequest::References { revision, function },
            Kind::Effects => QueryRequest::Effects { revision, function },
        }
    }
    #[test]
    fn changed_body_queries_reuse_unaffected_values_and_match_cold() {
        for n in [16, 64, 256] {
            let first = snapshot(n, true);
            let mut warm = WarmSession::new(first.clone(), 3 * n, 8 * 1024 * 1024).unwrap();
            for kind in [Kind::Symbol, Kind::References, Kind::Effects] {
                for i in 0..n {
                    warm.query(request(&first, i, kind)).unwrap();
                }
            }
            let identity = warm.inputs["f1"].body;
            let mut next = first.clone();
            next.functions[1].runtime_effects = RuntimeEffects::MAY_PANIC.bits();
            graph::propagate(&mut next.functions);
            next.revision = next.digest().unwrap();
            warm.update(next.clone()).unwrap();
            assert_eq!(warm.inputs["f1"].body, identity);
            let mut cold = QuerySession::new(next.clone()).unwrap();
            for kind in [Kind::Symbol, Kind::References, Kind::Effects] {
                for i in 0..n {
                    let req = request(&next, i, kind);
                    assert_eq!(warm.query(req.clone()).unwrap(), cold.query(req));
                }
            }
            let stats = warm.cache.stats();
            assert_eq!(stats.computations, 3 * n + 4); // two symbols + two effect witnesses
            assert_eq!(stats.hits, 3 * n - 4);
            assert_eq!(warm.invalidation_visits, 2); // one affected edge in old/new graphs
            assert_eq!(warm.input_visits, 2 * n);
            println!(
                "warm functions={n} initial={} recomputed=4 reused={} invalidation_edges=2",
                3 * n,
                3 * n - 4
            );
        }
    }
    #[test]
    fn retention_is_bounded_and_deletion_discards_old_identities() {
        let first = snapshot(64, false);
        let mut warm = WarmSession::new(first.clone(), 4, 4096).unwrap();
        for _ in 0..3 {
            for i in 0..64 {
                warm.query(request(&first, i, Kind::Symbol)).unwrap();
                let (entries, bytes) = warm.cache.retained();
                assert!(entries <= 4 && bytes <= 4096);
            }
        }
        let mut next = first.clone();
        next.functions.clear();
        next.revision = next.digest().unwrap();
        warm.update(next).unwrap();
        assert!(warm.inputs.is_empty());
        assert_eq!(warm.cache.retained(), (0, 0));
        let old = warm.revision().to_owned();
        let mut invalid = first;
        invalid.revision = "bad".into();
        assert!(warm.update(invalid).is_err());
        assert_eq!(warm.revision(), old);
    }
    #[test]
    fn removing_edges_invalidates_transitive_witnesses() {
        let first = snapshot(16, true);
        let mut warm = WarmSession::new(first.clone(), 64, 65536).unwrap();
        warm.query(request(&first, 0, Kind::Effects)).unwrap();
        let mut next = first.clone();
        next.functions[0].callees.pop();
        next.revision = next.digest().unwrap();
        warm.update(next.clone()).unwrap();
        let req = request(&next, 0, Kind::Effects);
        assert_eq!(
            warm.query(req.clone()).unwrap(),
            QuerySession::new(next).unwrap().query(req)
        );
        assert_eq!(warm.cache.stats().computations, 2);
    }
    #[test]
    fn dependency_invalidation_visits_deep_chains_and_cycles_once() {
        for cyclic in [false, true] {
            for n in [16, 64, 256] {
                let mut first = snapshot(n, false);
                for i in 0..n {
                    if i + 1 < n {
                        first.functions[i].callees.push(format!("f{}", i + 1));
                    } else if cyclic {
                        first.functions[i].callees.push("f0".into());
                    }
                }
                first.revision = first.digest().unwrap();
                let mut warm = WarmSession::new(first.clone(), n, 16 * 1024 * 1024).unwrap();
                for i in 0..n {
                    warm.query(request(&first, i, Kind::Effects)).unwrap();
                }
                let mut next = first;
                next.functions[n - 1].runtime_effects = RuntimeEffects::MAY_PANIC.bits();
                graph::propagate(&mut next.functions);
                next.revision = next.digest().unwrap();
                warm.update(next.clone()).unwrap();
                let mut cold = QuerySession::new(next.clone()).unwrap();
                for i in 0..n {
                    let request = request(&next, i, Kind::Effects);
                    assert_eq!(warm.query(request.clone()).unwrap(), cold.query(request));
                }
                assert_eq!(warm.cache.stats().computations, 2 * n);
                let edges = if cyclic { n } else { n - 1 };
                assert_eq!(warm.invalidation_visits, 2 * edges);
                println!(
                    "warm shape={} functions={n} invalidation_edges={} recomputed={n}",
                    if cyclic { "cycle" } else { "chain" },
                    warm.invalidation_visits
                );
            }
        }
    }
}
