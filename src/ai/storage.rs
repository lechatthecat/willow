use super::*;
use std::io::{Read, Write};

const MAX_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn compiler_stamp() -> String {
    static STAMP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STAMP.get_or_init(compute_compiler_stamp).clone()
}

fn compute_compiler_stamp() -> String {
    // Artifact identity, rather than the unchanged package version, is the
    // compatibility authority. Read in bounded chunks; never retain a binary.
    let digest = (|| -> Result<String> {
        let mut file = std::fs::File::open(std::env::current_exe()?)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        Ok(format!(
            "{}:{:x}",
            env!("CARGO_PKG_VERSION"),
            hasher.finalize()
        ))
    })();
    // Failure cannot yield a portable compatibility promise.
    digest.unwrap_or_else(|_| format!("unavailable:{}", std::process::id()))
}

impl Snapshot {
    pub(super) fn digest(&self) -> Result<String> {
        // Hash by streaming serialization, without cloning a second snapshot.
        struct HashWriter(Sha256);
        impl Write for HashWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = HashWriter(Sha256::new());
        serde_json::to_writer(
            &mut writer,
            &(
                self.version,
                &self.compiler,
                &self.compatibility,
                &self.workspace,
                &self.sources,
                &self.functions,
                &self.semantic,
            ),
        )?;
        Ok(format!("{:x}", writer.0.finalize()))
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported snapshot version");
        ensure!(
            self.compiler == compiler_stamp(),
            "incompatible compiler snapshot"
        );
        ensure!(self.revision == self.digest()?, "corrupt snapshot digest");
        let mut module_paths = std::collections::HashSet::new();
        for module in &self.semantic.modules {
            ensure!(
                self.sources.contains_key(&module.path) && module_paths.insert(&module.path),
                "invalid package module source"
            );
            ensure!(
                module
                    .dependencies
                    .iter()
                    .all(|&i| i < self.semantic.modules.len()),
                "invalid package module edge"
            );
        }
        let mut ids = std::collections::HashSet::new();
        for f in &self.functions {
            ensure!(ids.insert(&f.id), "duplicate FunctionId");
            ensure!(
                f.runtime_effects & !RuntimeEffects::ALL.bits() == 0,
                "invalid effects"
            );
            for span in &f.locations {
                ensure!(
                    span.start <= span.end && self.sources.contains_key(&span.path),
                    "invalid location"
                );
            }
        }
        for f in &self.functions {
            for target in &f.callees {
                ensure!(ids.contains(target), "dangling call edge");
            }
        }
        let mut symbol_ids = std::collections::HashSet::new();
        for symbol in &self.semantic.symbols {
            ensure!(symbol_ids.insert(&symbol.id), "duplicate symbol identity");
            if let Some(l) = &symbol.location {
                ensure!(
                    l.start < l.end && self.sources.contains_key(&l.path),
                    "invalid symbol location"
                );
            }
        }
        for reference in &self.semantic.references {
            let l = &reference.location;
            ensure!(
                symbol_ids.contains(&reference.target),
                "dangling symbol reference"
            );
            ensure!(
                l.start < l.end && self.sources.contains_key(&l.path),
                "invalid reference location"
            );
        }
        for flow in &self.semantic.flows {
            ensure!(
                ids.contains(&flow.function) && flow.entry < flow.nodes.len(),
                "invalid semantic flow"
            );
            for node in &flow.nodes {
                ensure!(
                    node.location.start <= node.location.end
                        && self.sources.contains_key(&node.location.path),
                    "invalid flow location"
                );
                ensure!(
                    node.successors.iter().all(|&i| i < flow.nodes.len()),
                    "invalid flow edge"
                );
            }
        }
        for e in &self.semantic.expressions {
            ensure!(
                ids.contains(&e.function) && e.target.as_ref().is_none_or(|t| ids.contains(t)),
                "invalid semantic identity"
            );
            ensure!(
                e.location.start <= e.location.end && self.sources.contains_key(&e.location.path),
                "invalid expression location"
            );
            ensure!(
                self.semantic
                    .flows
                    .get(e.flow)
                    .is_some_and(|f| f.function == e.function
                        && e.node < f.nodes.len()
                        && !e.occurrences.is_empty()
                        && e.occurrences.iter().all(|&n| n < f.nodes.len())),
                "invalid expression flow"
            );
        }
        ensure!(
            self.semantic.witnesses.keys().all(|id| ids.contains(id)),
            "invalid witness identity"
        );
        Ok(())
    }
    /// Atomic create-only publication. Existing baselines are never overwritten.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = path.with_extension(format!("snapshot-{}-{sequence}.tmp", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let result = (|| -> Result<()> {
            {
                let mut writer = std::io::BufWriter::new(&mut file);
                serde_json::to_writer(&mut writer, self)?;
                writer.flush()?;
            }
            ensure!(
                file.metadata()?.len() <= MAX_BYTES,
                "snapshot exceeds 64 MiB limit"
            );
            file.sync_all()?;
            std::fs::hard_link(&temp, path)
                .context("publish baseline (destination must not exist)")?;
            Ok(())
        })();
        drop(file);
        let _ = std::fs::remove_file(temp);
        result
    }
    pub fn load(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        ensure!(
            file.metadata()?.len() <= MAX_BYTES,
            "snapshot exceeds 64 MiB limit"
        );
        let mut reader = file.take(MAX_BYTES + 1);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_BYTES,
            "snapshot exceeds 64 MiB limit"
        );
        let snapshot: Self = serde_json::from_slice(&bytes)?;
        drop(bytes);
        snapshot.validate()?;
        Ok(snapshot)
    }
}

#[derive(Debug, Serialize)]
pub struct FunctionChange {
    pub before: Option<String>,
    pub after: Option<String>,
    pub kind: String,
    pub added_effects: u8,
    pub removed_effects: u8,
    pub added_callees: Vec<String>,
    pub removed_callees: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct Difference {
    pub before_revision: String,
    pub after_revision: String,
    pub changes: Vec<FunctionChange>,
    pub before_impact: Option<Impact>,
    pub after_impact: Option<Impact>,
}

impl Snapshot {
    pub fn compare(&self, after: &Self, limits: Limits) -> Result<Difference> {
        self.validate()?;
        after.validate()?;
        ensure!(
            self.workspace == after.workspace && self.compatibility == after.compatibility,
            "incompatible workspace/configuration/dependencies"
        );
        let before: HashMap<_, _> = self.functions.iter().map(|f| (&f.id, f)).collect();
        let next: HashMap<_, _> = after.functions.iter().map(|f| (&f.id, f)).collect();
        let mut changes = Vec::new();
        let mut old_seeds = Vec::new();
        let mut new_seeds = Vec::new();
        // Unique body matches report renames; ambiguous equal bodies remain
        // explicit additions/deletions instead of guessing correspondence.
        let mut removed = HashMap::<(&str, &str), Vec<&Function>>::new();
        let mut added = HashMap::<(&str, &str), Vec<&Function>>::new();
        for f in &self.functions {
            if !next.contains_key(&f.id) && !f.synthetic {
                removed
                    .entry((&f.module, &f.body_fingerprint))
                    .or_default()
                    .push(f);
            }
        }
        for f in &after.functions {
            if !before.contains_key(&f.id) && !f.synthetic {
                added
                    .entry((&f.module, &f.body_fingerprint))
                    .or_default()
                    .push(f);
            }
        }
        let mut renames = HashMap::new();
        let mut matched = std::collections::HashSet::new();
        for (key, old) in removed {
            if old.len() == 1
                && let Some(new) = added.get(&key)
                && new.len() == 1
            {
                renames.insert(&old[0].id, new[0]);
                matched.insert(&new[0].id);
            }
        }
        for old in &self.functions {
            let new = next
                .get(&old.id)
                .copied()
                .or_else(|| renames.get(&old.id).copied());
            if let Some(new) = new {
                if old.id == new.id
                    && old.fingerprint == new.fingerprint
                    && old.callees == new.callees
                    && old.runtime_effects == new.runtime_effects
                    && old.unknown == new.unknown
                    && old.unresolved == new.unresolved
                {
                    continue;
                }
                changes.push(change(
                    Some(old),
                    Some(new),
                    if old.id == new.id {
                        "modified"
                    } else {
                        "renamed"
                    },
                ));
                new_seeds.push(new.id.clone());
            } else {
                changes.push(change(Some(old), None, "deleted"));
            }
            old_seeds.push(old.id.clone());
        }
        for new in &after.functions {
            if !before.contains_key(&new.id) && !matched.contains(&new.id) {
                changes.push(change(None, Some(new), "added"));
                new_seeds.push(new.id.clone());
            }
        }
        changes.sort_by(|a, b| (&a.before, &a.after).cmp(&(&b.before, &b.after)));
        Ok(Difference {
            before_revision: self.revision.clone(),
            after_revision: after.revision.clone(),
            changes,
            before_impact: if old_seeds.is_empty() {
                None
            } else {
                Some(self.impact(&old_seeds, Direction::Callers, limits, None)?)
            },
            after_impact: if new_seeds.is_empty() {
                None
            } else {
                Some(after.impact(&new_seeds, Direction::Callers, limits, None)?)
            },
        })
    }
}
fn change(old: Option<&Function>, new: Option<&Function>, kind: &str) -> FunctionChange {
    let old_bits = old.map_or(0, |f| f.runtime_effects);
    let new_bits = new.map_or(0, |f| f.runtime_effects);
    let old_edges: BTreeSet<_> = old.into_iter().flat_map(|f| f.callees.iter()).collect();
    let new_edges: BTreeSet<_> = new.into_iter().flat_map(|f| f.callees.iter()).collect();
    FunctionChange {
        before: old.map(|f| f.id.clone()),
        after: new.map(|f| f.id.clone()),
        kind: kind.into(),
        added_effects: new_bits & !old_bits,
        removed_effects: old_bits & !new_bits,
        added_callees: new_edges
            .difference(&old_edges)
            .map(|s| (*s).clone())
            .collect(),
        removed_callees: old_edges
            .difference(&new_edges)
            .map(|s| (*s).clone())
            .collect(),
    }
}
