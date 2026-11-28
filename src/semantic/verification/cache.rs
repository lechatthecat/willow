//! Process-owned proof entries. Disk artifacts are never trusted as proofs.
use super::*;
#[derive(Debug, Default, Clone)]
pub struct ProofCache {
    entries: HashMap<String, (String, Vec<Obligation>)>,
    pub hits: usize,
    pub misses: usize,
}
impl ProofCache {
    pub(super) fn get(&mut self, owner: &str, key: &str) -> Option<Vec<Obligation>> {
        if let Some((identity, value)) = self.entries.get(owner)
            && identity == key
        {
            self.hits += 1;
            Some(value.clone())
        } else {
            self.misses += 1;
            None
        }
    }
    pub(super) fn remove(&mut self, owner: &str) {
        self.entries.remove(owner);
    }
    pub(super) fn insert(&mut self, owner: String, key: String, value: Vec<Obligation>) {
        self.entries.insert(owner, (key, value));
    }
}
pub(super) fn identity(
    summary: &Summary,
    dependency_names: &[String],
    dependencies: &[String],
    f: &FunctionDecl,
    options: &Options,
) -> String {
    digest(
        &serde_json::json!({
            "schema":"willow.proof-cache.v1", "typed_semantics":summary.definitions,
            "contracts":summary.contract_definitions, "dependencies":dependencies, "dependency_names":dependency_names,
            "signature":comparison::signature(f), "model":"checked-i64", "vc_generator":"8", "data_model":summary.data_digest,
            "compiler":crate::ai::compiler_identity(), "solver_version":solver::VERSION,
            "solver_executable":options.solver, "timeout_ms":options.timeout_ms,
            "resource_limit":options.resource_limit
        })
        .to_string(),
    )
}
