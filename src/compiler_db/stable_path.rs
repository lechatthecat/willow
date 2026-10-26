//! Persistent declaration identities independent of allocated syntax IDs.
use super::{ids::BodyOwner, incremental::SyntaxQueries};
use crate::{
    module::{ResolvedModule, UnitId, artifacts::UnitArtifacts},
    parser::ast::BodyId,
};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct StableDefPath {
    module: Arc<PathBuf>,
    declaration_kind: Arc<str>,
    owner_path: Arc<str>,
    name: Arc<str>,
    signature_discriminator: [u8; 32],
    // Preorder within this declaration, rather than a recursively copied path.
    lambda_ordinal: Option<u64>,
}

fn module_identity(path: &Path) -> Result<PathBuf> {
    if path.starts_with("<std>") {
        // Bundled sources have a compiler-versioned virtual identity, no file.
        return Ok(path.to_owned());
    }
    path.canonicalize()
        .with_context(|| format!("canonicalize declaration module {}", path.display()))
}

pub(crate) fn inventory(
    entry: &Path,
    modules: &[ResolvedModule],
    artifacts: &UnitArtifacts,
    syntax: &SyntaxQueries,
) -> Result<HashMap<BodyId, StableDefPath>> {
    let owners = super::revision::tracked_body_owners(entry, modules, artifacts);
    let paths: HashMap<_, _> = modules
        .iter()
        .map(|module| (module.id, module.path.as_path()))
        .chain(std::iter::once((UnitId::ENTRY, entry)))
        .map(|(unit, path)| Ok((unit, Arc::new(module_identity(path)?))))
        .collect::<Result<_>>()?;
    let mut signatures = HashMap::new();
    let mut result = HashMap::new();
    let mut traversed = HashSet::new();
    for (body, unit) in artifacts.bodies.entries() {
        let Some((_, concrete)) = artifacts.bodies.owner(body) else {
            continue;
        };
        // Expansion can leave source and instantiated roots in the inventory.
        // Only the current binding owns the checked semantic body.
        if artifacts.bodies.body(unit, concrete) != Some(body)
            || matches!(concrete, BodyOwner::Lambda { .. })
        {
            continue;
        }
        let Some((source, owner)) = owners.get(&body) else {
            continue;
        };
        let signature = match signatures.entry((source.clone(), owner.clone())) {
            std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
            std::collections::hash_map::Entry::Vacant(entry) => *entry.insert(<[u8; 32]>::from(
                Sha256::digest(syntax.signature_bytes(source, owner)?),
            )),
        };
        let (kind, name, context) = match concrete {
            BodyOwner::Function(function) => (
                if function.owner_type().is_some() {
                    "method"
                } else {
                    "function"
                },
                function.name(),
                serde_json::to_string(&function)?,
            ),
            BodyOwner::InterfaceDefault(function) => (
                "interface_default",
                function.name(),
                serde_json::to_string(&function)?,
            ),
            BodyOwner::Constructor { owner, .. } => (
                "constructor",
                Arc::from("init"),
                serde_json::to_string(&owner)?,
            ),
            BodyOwner::StaticInitializer(field) => (
                "static_initializer",
                Arc::from(owner.rsplit(':').next().unwrap_or(owner)),
                serde_json::to_string(&field.owner)?,
            ),
            BodyOwner::Lambda { .. } => unreachable!(),
        };
        let Some(module) = paths.get(&unit) else {
            continue;
        };
        let key = StableDefPath {
            module: Arc::clone(module),
            declaration_kind: Arc::from(kind),
            owner_path: Arc::from(format!("{owner}|{context}")),
            name,
            signature_discriminator: signature,
            lambda_ordinal: None,
        };
        traversed.insert(body);
        result.insert(body, key.clone());
        let mut pending: Vec<_> = artifacts
            .bodies
            .child_lambdas(body)
            .map(|(_, child)| child)
            .collect();
        pending.reverse();
        let mut ordinal = 0_u64;
        while let Some(child) = pending.pop() {
            if !traversed.insert(child) {
                continue;
            }
            ordinal += 1;
            let mut lambda = key.clone();
            lambda.lambda_ordinal = Some(ordinal);
            lambda.declaration_kind = Arc::from("lambda");
            result.insert(child, lambda);
            let start = pending.len();
            pending.extend(
                artifacts
                    .bodies
                    .child_lambdas(child)
                    .map(|(_, child)| child),
            );
            pending[start..].reverse();
        }
    }
    // Ambiguous overload/source projections are conservatively uncacheable.
    let mut seen = HashSet::new();
    let mut ambiguous = HashSet::new();
    for key in result.values() {
        if !seen.insert(key.clone()) {
            ambiguous.insert(key.clone());
        }
    }
    result.retain(|_, key| !ambiguous.contains(key));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn keys(source: &str) -> HashSet<StableDefPath> {
        let path = std::env::current_dir().unwrap().join("Cargo.toml");
        let mut syntax = SyntaxQueries::default().candidate().unwrap();
        let (mut program, diagnostics, failed) = syntax
            .parse(&path, crate::diagnostics::FileId::ENTRY, source)
            .unwrap();
        assert!(!failed && diagnostics.is_empty());
        let mut artifacts = UnitArtifacts::new().unwrap();
        artifacts
            .retain_parse(
                crate::diagnostics::FileId::ENTRY,
                &path,
                source,
                &mut program,
            )
            .unwrap();
        artifacts.offload(&mut program).unwrap();
        inventory(&path, &[], &artifacts, &syntax)
            .unwrap()
            .into_values()
            .collect()
    }
    #[test]
    fn stable_paths_bundled_module_identity_needs_no_file() {
        let path = Path::new("<std>/collections.wi");
        assert_eq!(module_identity(path).unwrap(), path);
    }
    #[test]
    fn stable_paths_ignore_unrelated_declaration_and_syntax_ids() {
        let source = "fn f(x: i64) -> i64 { let a = |y: i64| y; let b = |z: i64| z; return a(x); }";
        let first = keys(source);
        let second: HashSet<_> = keys(&format!("fn unrelated() {{}}\n{source}"))
            .into_iter()
            .filter(|key| key.name.as_ref() == "f")
            .collect();
        assert_eq!(first.len(), 3);
        assert_eq!(first, second);
        let changed = keys("fn f(x: bool) -> bool { return x; }");
        assert!(first.is_disjoint(&changed));
        let serialized = serde_json::to_vec(&first).unwrap();
        assert_eq!(first, serde_json::from_slice(&serialized).unwrap());
    }
    #[test]
    fn stable_paths_lambda_inventory_scales_with_body_count() {
        for count in [8, 32, 128] {
            let mut source = String::from("fn f() {");
            for index in 0..count {
                source.push_str(&format!("let a{index} = |x: i64| x;"));
            }
            source.push('}');
            let keys = keys(&source);
            assert_eq!(keys.len(), count + 1);
            let mut ordinals: Vec<_> = keys.iter().filter_map(|key| key.lambda_ordinal).collect();
            ordinals.sort_unstable();
            assert_eq!(ordinals, (1..=count as u64).collect::<Vec<_>>());
        }
    }
}
