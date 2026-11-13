//! Metadata-only admission for the best-effort persistent snapshot.
//! Low-reuse revisions miss before old semantic payloads are restored.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
pub(crate) struct SourceAdmission {
    pub path: PathBuf,
    pub file: FileId,
    source: [u8; 32],
    declarations: [u8; 32],
    imports: [u8; 32],
    /// Generic owners. Editing one (or a member nested under it) may
    /// invalidate many instantiations despite identical signatures.
    template_owners: Vec<String>,
    owners: Vec<(String, [u8; 32], usize)>,
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[derive(Default)]
pub(crate) struct ReuseDensity {
    pub owners: usize,
    pub matched: usize,
    pub bytes: u128,
    pub matched_bytes: u128,
}

impl SyntaxQueries {
    pub(crate) fn disk_admission(&self) -> Result<Vec<SourceAdmission>> {
        self.captured
            .iter()
            .map(|(path, source)| {
                let value = self
                    .table
                    .peek(&QueryNode::Parse(path.clone()))
                    .ok_or_else(|| anyhow::anyhow!("missing admission parse"))?;
                let parsed = value.get::<ParsedValue>();
                fn templates(value: &Value) -> bool {
                    match value {
                        Value::Object(fields) => fields.iter().any(|(key, value)| {
                            (key == "type_params"
                                && value.as_array().is_some_and(|v| !v.is_empty()))
                                || templates(value)
                        }),
                        Value::Array(values) => values.iter().any(templates),
                        _ => false,
                    }
                }
                Ok(SourceAdmission {
                    path: path.clone(),
                    file: source.file,
                    source: hash(source.text.as_bytes()),
                    declarations: hash(&parsed.declarations.bytes),
                    imports: hash(&parsed.imports.bytes),
                    template_owners: parsed
                        .owners
                        .iter()
                        .map(|(owner, (signature, _))| {
                            Ok(templates(&signature.decode::<Value>()?).then(|| owner.clone()))
                        })
                        .filter_map(Result::transpose)
                        .collect::<Result<_>>()?,
                    owners: parsed
                        .owners
                        .iter()
                        .map(|(owner, (signature, body))| {
                            let mut digest = Sha256::new();
                            digest.update(&signature.bytes);
                            digest.update(&body.bytes);
                            (
                                owner.clone(),
                                digest.finalize().into(),
                                signature.bytes.len() + body.bytes.len(),
                            )
                        })
                        .collect(),
                })
            })
            .collect()
    }
}

impl SourceAdmission {
    pub(crate) fn owners(&self) -> usize {
        self.owners.len()
    }

    /// Keep ordinary value edits incremental, but reject structurally different
    /// or mostly replaced inputs before restoring the monolithic payload. Both
    /// count and byte weight need a majority of unchanged candidates: a large
    /// replaced body must not hide behind many tiny stable declarations.
    pub(crate) fn density(&self, source: &str) -> Result<Option<ReuseDensity>> {
        if self.source == hash(source.as_bytes()) {
            let bytes = self
                .owners
                .iter()
                .map(|(_, _, weight)| *weight as u128)
                .sum();
            return Ok(Some(ReuseDensity {
                owners: self.owners.len(),
                matched: self.owners.len(),
                bytes,
                matched_bytes: bytes,
            }));
        }
        let mut syntax = SyntaxQueries::default().candidate()?;
        let (_, diagnostics, failed) = syntax.parse(&self.path, self.file, source)?;
        if failed || !diagnostics.is_empty() {
            return Ok(None);
        }
        let current = syntax
            .disk_admission()?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing current admission"))?;
        // Import edits are revalidated through the restored dependency graph;
        // like body edits they are admitted when owner reuse stays dense.
        if self.declarations != current.declarations {
            return Ok(None);
        }
        let current: HashMap<_, _> = current
            .owners
            .into_iter()
            .map(|(name, fingerprint, _)| (name, fingerprint))
            .collect();
        let mut matched = 0;
        // Disk weights are untrusted usize values. Accumulate in u128 so even
        // malformed metadata cannot overflow the density comparison.
        let mut matched_bytes = 0_u128;
        let mut bytes = 0_u128;
        let templates: std::collections::HashSet<&str> =
            self.template_owners.iter().map(String::as_str).collect();
        // A member owner is named `<parent>/<field><kind>:<name>`, so its
        // generic ancestors are the `/`-delimited prefixes of its name.
        let generic = |name: &str| {
            templates.contains(name)
                || name
                    .match_indices('/')
                    .any(|(end, _)| templates.contains(&name[..end]))
        };
        for (name, fingerprint, weight) in &self.owners {
            bytes += *weight as u128;
            if current.get(name) == Some(fingerprint) {
                matched += 1;
                matched_bytes += *weight as u128;
            } else if generic(name) {
                // Template edits may invalidate many instantiations despite
                // identical signatures, so a source-owner density test must
                // not admit them. Edits outside templates are still measured.
                return Ok(None);
            }
        }
        Ok(Some(ReuseDensity {
            owners: self.owners.len(),
            matched,
            bytes,
            matched_bytes,
        }))
    }
}
