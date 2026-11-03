//! Process-owned proof entries. Disk artifacts are never trusted as proofs.
use super::*;
use std::collections::BTreeMap;
#[derive(Default)]
pub struct ProofCache {
    entries: BTreeMap<String, Vec<Obligation>>,
    pub hits: usize,
    pub misses: usize,
}
impl ProofCache {
    pub(super) fn invalidate(&mut self) {
        self.entries.clear();
    }
    pub(super) fn get(&mut self, key: &str) -> Option<Vec<Obligation>> {
        if let Some(value) = self.entries.get(key) {
            self.hits += 1;
            Some(value.clone())
        } else {
            self.misses += 1;
            None
        }
    }
    pub(super) fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }
    pub(super) fn insert(&mut self, key: String, value: Vec<Obligation>) {
        if self.entries.len() >= 256 && !self.entries.contains_key(&key) {
            self.entries.pop_first();
        }
        self.entries.insert(key, value);
    }
}
pub(super) fn identity(
    summary: &Summary,
    dependencies: &[String],
    f: &FunctionDecl,
    options: &Options,
) -> String {
    digest(
        &serde_json::json!({
            "schema":"willow.proof-cache.v1", "typed_semantics":summary.definitions,
            "contracts":summary.contract_definitions, "dependencies":dependencies,
            "signature":comparison::signature(f), "model":"checked-i64", "vc_generator":"2",
            "compiler":crate::ai::compiler_identity(), "solver_version":solver::VERSION,
            "solver_executable":options.solver, "timeout_ms":options.timeout_ms,
            "resource_limit":options.resource_limit
        })
        .to_string(),
    )
}
