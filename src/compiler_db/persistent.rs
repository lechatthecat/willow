//! Best-effort persistent compiler analysis. Final artifacts and linking are
//! deliberately outside this cache; every accepted revision runs the backend.
use super::{
    body::{BodyQueries, BodySnapshot},
    ids::BodyIndex,
    incremental::{SyntaxQueries, SyntaxSnapshot},
    inputs::CompilerInputs,
};
use crate::{
    Frontend,
    diagnostics::{DiagnosticEmitter, FileId, SourceMap},
    module::{
        UnitId,
        artifacts::{ArtifactStore, ParsedRecords, UnitArtifacts},
    },
    parser::ast::Program,
    semantic::ids::SymbolModule,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

const FORMAT: u32 = 1;
const MAX_BYTES: u64 = 512 * 1024 * 1024;
type ModuleIdentity = (UnitId, PathBuf, Option<SymbolModule>);

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct CacheKey {
    format: u32,
    compiler: String,
    target: String,
    runtime_abi: u64,
    std_schema: String,
    configuration: Vec<Vec<u8>>,
    entry: PathBuf,
}
impl CacheKey {
    fn new(inputs: &CompilerInputs, entry: &Path) -> Result<Self> {
        let configuration =
            super::captured::configuration(inputs, entry, Some(&inputs.project_root))?
                .into_iter()
                .map(|(_, value)| value.get::<Vec<u8>>().clone())
                .collect();
        Ok(Self {
            format: FORMAT,
            compiler: format!(
                "{}:{}",
                env!("CARGO_PKG_VERSION"),
                env!("WILLOW_COMPILER_SCHEMA_HASH")
            ),
            target: target_lexicon::HOST.to_string(),
            runtime_abi: willow_abi::fingerprint::ABI_HASH_64,
            std_schema: env!("WILLOW_STDLIB_SCHEMA_HASH").into(),
            configuration,
            entry: entry.to_owned(),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    key: CacheKey,
    id_ends: [u64; 2],
    modules: Vec<ModuleIdentity>,
    parsed: Vec<(FileId, String, Program)>,
    expanded: Vec<(FileId, Program)>,
    index: Rc<BodyIndex>,
    bodies: BodySnapshot,
    syntax: SyntaxSnapshot,
    definitions: Vec<(
        super::stable_path::StableDefPath,
        crate::parser::ast::BodyId,
    )>,
}

pub(crate) struct DiskRevision {
    pub store: Rc<ArtifactStore>,
    pub parsed: ParsedRecords,
    pub expanded: std::collections::HashMap<FileId, usize>,
    pub index: Rc<BodyIndex>,
    pub bodies: BodyQueries,
    pub modules: Vec<ModuleIdentity>,
    pub definitions:
        std::collections::HashMap<super::stable_path::StableDefPath, crate::parser::ast::BodyId>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", crate::hex::Hex(&Sha256::digest(bytes)))
}
fn cache_path(root: &Path, entry: &Path) -> PathBuf {
    std::env::var_os("WILLOW_QUERY_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target").join("willow-query-cache"))
        .join(format!(
            "{}.cache",
            digest(entry.as_os_str().as_encoded_bytes())
        ))
}

fn load(path: &Path, key: &CacheKey) -> Result<(DiskRevision, SyntaxQueries)> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_BYTES && bytes.len() >= 65,
        "invalid cache size"
    );
    let (checksum, payload) = bytes.split_at(65);
    anyhow::ensure!(
        checksum[64] == b'\n' && &checksum[..64] == digest(payload).as_bytes(),
        "cache checksum mismatch"
    );
    let snapshot: Snapshot = serde_json::from_slice(payload)?;
    anyhow::ensure!(snapshot.key == *key, "incompatible compiler cache");
    // Reserving ranges also covers IDs belonging only to synthesized bodies.
    crate::parser::ast::reserve_cached_ids(snapshot.id_ends)?;
    let syntax = SyntaxQueries::from_disk(snapshot.syntax)?;
    let artifacts = UnitArtifacts::new()?;
    let store = Rc::clone(&artifacts.store);
    let mut parsed = ParsedRecords::new();
    for (file, hash, program) in snapshot.parsed {
        anyhow::ensure!(
            parsed
                .insert(file, (hash, store.write(&program)?))
                .is_none(),
            "duplicate cached module"
        );
    }
    let expanded = snapshot
        .expanded
        .into_iter()
        .map(|(file, program)| Ok((file, store.write(&program)?)))
        .collect::<Result<_>>()?;
    let bodies = BodyQueries::from_disk(
        snapshot.bodies,
        Rc::clone(&store),
        Rc::clone(&snapshot.index),
    )?;
    Ok((
        DiskRevision {
            store,
            parsed,
            expanded,
            index: snapshot.index,
            bodies,
            modules: snapshot.modules,
            definitions: snapshot.definitions.into_iter().collect(),
        },
        syntax,
    ))
}

fn save(path: &Path, key: CacheKey, frontend: &Frontend, syntax: &SyntaxQueries) -> Result<()> {
    let artifacts = frontend
        .module_graph
        .artifacts
        .as_ref()
        .context("missing analysis artifacts")?;
    let mut parsed = artifacts
        .parsed
        .iter()
        .map(|(&file, (hash, record))| Ok((file, hash.clone(), artifacts.store.read(*record)?)))
        .collect::<Result<Vec<_>>>()?;
    parsed.sort_by_key(|(file, _, _)| file.0);
    let definitions =
        super::stable_path::inventory(&key.entry, &frontend.module_graph.files, artifacts, syntax)?
            .into_iter()
            .map(|(id, key)| (key, id))
            .collect();
    let snapshot = Snapshot {
        key,
        id_ends: crate::parser::ast::cached_id_ends(),
        modules: frontend
            .module_graph
            .files
            .iter()
            .map(|m| (m.id, m.path.clone(), m.symbol_module))
            .collect(),
        parsed,
        expanded: artifacts
            .expanded
            .iter()
            .map(|(&file, &record)| Ok((file, artifacts.store.read(record)?)))
            .collect::<Result<_>>()?,
        index: Rc::clone(&artifacts.bodies),
        bodies: frontend.db.typed_bodies.disk_snapshot()?,
        syntax: syntax.disk_snapshot()?,
        definitions,
    };
    let payload = serde_json::to_vec(&snapshot)?;
    anyhow::ensure!(
        payload.len() as u64 + 65 <= MAX_BYTES,
        "compiler cache size limit"
    );
    let mut bytes = digest(&payload).into_bytes();
    bytes.push(b'\n');
    bytes.extend(payload);
    std::fs::create_dir_all(path.parent().context("cache parent")?)?;
    crate::project::init::atomic_replace(path, &bytes)
}

pub(crate) fn run(
    source: &str,
    root: &Path,
    map: &SourceMap,
    inputs: CompilerInputs,
    emitter: &mut dyn DiagnosticEmitter,
) -> Result<Frontend> {
    // Read-only AI/edit sessions must not add cache files to isolated inputs.
    if (inputs.capture_analysis && !inputs.verification)
        || std::env::var_os("WILLOW_QUERY_CACHE").is_some_and(|v| v == "0")
    {
        return crate::run_frontend_revision(source, root, map, inputs, emitter, None, None);
    }
    let read_only = inputs.verification;
    let entry = Path::new(&map.path);
    let Ok(key) = CacheKey::new(&inputs, entry) else {
        return crate::run_frontend_revision(source, root, map, inputs, emitter, None, None);
    };
    let path = cache_path(root, entry);
    let loaded = load(&path, &key).ok();
    let hit = loaded.is_some();
    let (previous, syntax) = match loaded {
        Some((previous, syntax)) => (Some(previous), syntax),
        None => (None, SyntaxQueries::default()),
    };
    let syntax = Rc::new(RefCell::new(syntax.candidate()?));
    syntax
        .borrow_mut()
        .configure_sources(inputs.package_graph.as_deref());
    let frontend = crate::run_frontend_revision_cached(
        source,
        root,
        map,
        inputs,
        emitter,
        None,
        Some(Rc::clone(&syntax)),
        previous.as_ref(),
    )?;
    syntax.borrow_mut().finish()?;
    let stats = syntax.borrow().stats();
    crate::query_stats::query_cache(|| {
        serde_json::json!({"loaded": hit, "typechecks": frontend.db.typed_bodies.typechecks(), "reused_bodies": frontend.db.typed_bodies.reused(), "queries_recomputed": stats.recomputed, "queries_validated": stats.validated, "dependency_edges_visited": stats.dependency_edges_visited}).to_string()
    });
    // Cache failures never change successful compilation into a failure.
    if !read_only && let Err(error) = save(&path, key, &frontend, &syntax.borrow()) {
        crate::query_stats::query_cache(|| format!("write skipped: {error}"));
    }
    Ok(frontend)
}

#[cfg(test)]
#[path = "persistent_tests.rs"]
mod tests;
