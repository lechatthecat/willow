//! Opt-in v2 policy layer; all v1 callers retain their original report and codes.
use super::*;
use crate::semantic::verification::{self, ProofCache, comparison};
use std::path::PathBuf;
#[derive(Default)]
pub struct Policy {
    pub protected_base: Option<String>,
    pub approval_evidence: Option<PathBuf>,
    pub approval_verifier: Option<PathBuf>,
    pub write_lock: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalBinding {
    pub base_tree: String,
    pub normalization: String,
    pub protected_commit: String,
    pub protected_contracts: String,
    pub protected_lock_present: bool,
    pub target_contracts: String,
    pub target_lock_present: bool,
    pub target_tree: String,
}
#[derive(Debug, Clone, Copy, Serialize)]
pub enum ContractStatus {
    #[serde(rename = "contract_changed")]
    Changed,
    #[serde(rename = "contract_digest_unchanged")]
    DigestUnchanged,
}
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    ApprovalVerified,
    ApprovalRequired,
    NotRequired,
}
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SpecificationProvenance {
    ApprovedByPolicy,
    Unverified,
}
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalEvidence {
    pub approval_status: ApprovalStatus,
    pub binding: Option<ApprovalBinding>,
    pub contract_status: ContractStatus,
    pub semantic_dependencies_changed: bool,
    pub specification_provenance: SpecificationProvenance,
}
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestChange {
    Removed,
    Added,
    Unchanged,
    Unknown,
}
#[derive(Debug, Clone, Serialize)]
pub struct LockManifest {
    pub change: ManifestChange,
    pub protected_present: Option<bool>,
    pub target_present: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct CacheWork {
    pub hits: usize,
    pub misses: usize,
    pub scope: &'static str,
}
#[derive(Debug, Serialize)]
pub struct VerificationEvidence {
    pub base: Vec<verification::Report>,
    pub source_path: PathBuf,
    pub target: Vec<verification::Report>,
}
#[derive(Debug, Clone, Serialize)]
pub struct EffectEvidence {
    pub changes: Vec<SymbolChange>,
    pub summary: ReportSummary,
}
#[derive(Debug, Default, Serialize)]
pub struct ContractSummary {
    #[serde(flatten)]
    pub semantic: ReportSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison_work: Option<comparison::ComparisonWork>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contracts_lock: Option<comparison::Lock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effects: Option<EffectEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_manifest: Option<LockManifest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof_cache: Option<CacheWork>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationEvidence>,
}
impl SummaryCounts for ContractSummary {
    fn contract_violations(&self) -> usize {
        self.semantic.contract_violations()
    }
    fn policy_violations(&self) -> usize {
        self.semantic.policy_violations()
    }
}
pub type Report = SemanticDiff<comparison::ComparisonChange, ContractSummary>;

pub fn incomplete(reason: String) -> Report {
    let original = SemanticDiff::incomplete(reason);
    SemanticDiff {
        schema: "willow.semantic-diff.v4",
        status: original.status,
        base: original.base,
        target: original.target,
        changes: vec![],
        summary: ContractSummary {
            semantic: original.summary,
            ..Default::default()
        },
    }
}

fn capture(
    inputs: &tree::Tree,
    entry: &Path,
    cache: &mut ProofCache,
) -> Result<comparison::Snapshot> {
    let source = std::str::from_utf8(
        inputs
            .get(entry.to_str().context("UTF-8 entry")?)
            .context("entry missing")?,
    )?;
    let tokens = crate::lexer::Lexer::new(source)
        .tokenize()
        .map_err(|e| anyhow::anyhow!("lex failed: {e:?}"))?;
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    ensure!(errors.is_empty(), "contract input parsing failed");
    Ok(comparison::capture(
        program,
        &verification::Options {
            artifacts: None,
            ..Default::default()
        },
        cache,
    ))
}
fn consistent(inputs: &tree::Tree, actual: &comparison::Lock) -> Result<bool> {
    inputs
        .get("contracts.lock")
        .map(|bytes| serde_json::from_slice::<comparison::Lock>(bytes).map(|l| l == *actual))
        .transpose()
        .map(|v| v.unwrap_or(true))
        .map_err(Into::into)
}
/// The caller supplies a trusted verifier executable, never project config.
/// Trust comes from that verifier authenticating the evidence and protected
/// revision, not from environment variables or the executable's location alone.
/// Its exact JSON response must bind the complete request and approve it.
fn approval(policy: &Policy, root: &Path, binding: &impl Serialize) -> Result<bool> {
    let (Some(verifier), Some(evidence)) = (&policy.approval_verifier, &policy.approval_evidence)
    else {
        return Ok(false);
    };
    let verifier = std::fs::canonicalize(verifier)?;
    ensure!(
        !verifier.starts_with(root),
        "approval verifier must be outside the editable workspace"
    );
    let evidence = std::fs::canonicalize(evidence)?;
    ensure!(
        std::fs::metadata(&evidence)?.len() <= 65536,
        "approval evidence exceeds limit"
    );
    let child = shared_child::SharedChild::spawn(
        std::process::Command::new(verifier)
            .arg(&evidence)
            .arg(serde_json::to_string(binding)?)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
    )?;
    let output = child.take_stdout().context("approval verifier output")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut bytes = Vec::new();
        let result = output.take(65537).read_to_end(&mut bytes).map(|_| bytes);
        let _ = tx.send(result);
    });
    let status = wait_for_approval(&child)?;
    ensure!(status.success(), "approval verifier rejected evidence");
    let bytes = rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .context("approval verifier output unavailable")??;
    ensure!(
        bytes.len() <= 65536,
        "approval verifier output exceeds limit"
    );
    let response: Value = serde_json::from_slice(&bytes)?;
    Ok(response
        == json!({"schema":"willow.contract-approval.v1","approved":true,"binding":binding}))
}
// SharedChild permits a blocking wait and timeout kill concurrently without PID
// reuse races. Disable its optional SIGCHLD timeout machinery: a scoped waiter
// needs no process-wide signal handler and is joined on every return path.
fn wait_for_approval(child: &shared_child::SharedChild) -> Result<std::process::ExitStatus> {
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let waiter = std::thread::Builder::new().spawn_scoped(scope, move || {
            let _ = tx.send(child.wait());
        });
        if let Err(error) = waiter {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(status) => Ok(status?),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("approval verifier timed out");
            }
        }
    })
}

fn dependencies_changed(base: &comparison::Lock, target: &comparison::Lock) -> bool {
    base.functions.len() != target.functions.len()
        || base.functions.iter().zip(&target.functions).any(
            |((name, digest), (other_name, other_digest))| {
                name != other_name || digest.semantics != other_digest.semantics
            },
        )
}

pub fn verify(options: &Options, policy: &Policy) -> Result<Report> {
    let root = std::fs::canonicalize(&options.root)?;
    let before = tree::base_inputs(&root, &options.base, true)?;
    let target = tree::target_inputs(&root, true)?;
    let before_digest = hash_serialized(&before)?;
    let target_digest = hash_serialized(&target)?;
    ensure!(
        !(options.project
            || before.contains_key("project.toml")
            || target.contains_key("project.toml"))
            || (before.keys().filter(|p| p.ends_with(".wi")).count() <= 1
                && target.keys().filter(|p| p.ends_with(".wi")).count() <= 1),
        "contract comparison does not support multi-source project discovery"
    );
    let entry = resolve_entry(options, &before)?;
    ensure!(
        entry == resolve_entry(options, &target)?,
        "contract analysis scope changed"
    );
    let mut cache = ProofCache::default();
    let a = capture(&before, &entry, &mut cache)?;
    let b = capture(&target, &entry, &mut cache)?;
    let compared = comparison::compare_typed(
        &a,
        &b,
        &verification::Options {
            artifacts: None,
            ..Default::default()
        },
    );
    // Preserve effect policy coverage in v2 as well as the unchanged v1 entry point.
    let report = super::verify(options)?;
    let without_lock = |tree: &tree::Tree| {
        let mut t = tree.clone();
        t.remove("contracts.lock");
        t
    };
    ensure!(
        report
            .base
            .as_ref()
            .map(|revision| revision.digest.as_str())
            == Some(hash_serialized(&without_lock(&before))?.as_str())
            && report
                .target
                .as_ref()
                .map(|revision| revision.digest.as_str())
                == Some(hash_serialized(&without_lock(&target))?.as_str()),
        "effect and contract snapshots differ"
    );
    let mut uncertainty = report.summary.uncertainty.clone();
    uncertainty.retain(|reason| {
        !(reason.starts_with("base: E2630:")
            || reason.starts_with("target: E2630:")
            || ((reason.starts_with("base: E2620:") || reason.starts_with("target: E2620:"))
                && !reason.contains("UNKNOWN")))
    });
    uncertainty.extend(compared.uncertainty);
    let mut approved = false;
    let mut changed = true;
    let mut contract_changed = true;
    let mut semantic_dependencies_changed = true;
    let mut protected_lock_present = None;
    let target_lock_present = target.contains_key("contracts.lock");
    let mut binding = None;
    if let Some(commit) = &policy.protected_base {
        let commit = tree::protected_commit(&root, commit)?;
        let protected = tree::base_inputs(&root, &commit, true)?;
        ensure!(
            entry == resolve_entry(options, &protected)?,
            "protected analysis scope changed"
        );
        let protected_inputs = protected;
        let protected_capture = if protected_inputs == before {
            None
        } else {
            Some(capture(&protected_inputs, &entry, &mut cache)?)
        };
        let protected = protected_capture.as_ref().unwrap_or(&a);
        if !consistent(&protected_inputs, &protected.lock)? {
            uncertainty.push("protected contracts.lock does not match analyzed contracts".into());
        }
        uncertainty.extend(protected.errors.iter().cloned());
        let manifest_present = protected_inputs.contains_key("contracts.lock");
        protected_lock_present = Some(manifest_present);
        contract_changed = protected.lock != b.lock;
        semantic_dependencies_changed = dependencies_changed(&protected.lock, &b.lock);
        // Optional bootstrapping does not authorize removal of a protected manifest.
        changed = contract_changed || (manifest_present && !target_lock_present);
        let request = ApprovalBinding {
            protected_commit: commit,
            base_tree: before_digest.clone(),
            target_tree: target_digest.clone(),
            protected_contracts: protected.lock.digest(),
            target_contracts: b.lock.digest(),
            normalization: b.lock.normalization.clone(),
            protected_lock_present: manifest_present,
            target_lock_present,
        };
        match approval(policy, &root, &request) {
            Ok(v) => approved = v && protected.errors.is_empty() && b.errors.is_empty(),
            Err(e) => uncertainty.push(format!("approval evidence: {e:#}")),
        }
        binding = Some(request);
    } else {
        uncertainty.push("protected baseline unavailable".into());
    }
    if !consistent(&target, &b.lock)? {
        uncertainty.push("local contracts.lock does not match analyzed contracts".into());
    }
    if !consistent(&before, &a.lock)? {
        uncertainty.push("base contracts.lock does not match analyzed contracts".into());
    }
    let violations = report.summary.contract_violations() + compared.contract_violations;
    let policy_violations = report.summary.policy_violations() + usize::from(changed && !approved);
    if hash_serialized(&tree::target_inputs(&root, true)?)? != target_digest {
        uncertainty.push("snapshot changed during verification".into());
    }
    let status = if uncertainty.is_empty() {
        ReportStatus::Complete
    } else {
        ReportStatus::Incomplete
    };
    if policy.write_lock {
        ensure!(
            b.errors.is_empty(),
            "cannot write a lock for invalid inputs"
        );
        crate::project::init::atomic_replace(
            &root.join("contracts.lock"),
            format!("{}\n", serde_json::to_string_pretty(&b.lock)?).as_bytes(),
        )?;
    }
    let mut summary = report.summary.clone();
    summary.contract_violations = Some(violations);
    summary.policy_violations = Some(policy_violations);
    summary.uncertainty = uncertainty;
    Ok(SemanticDiff {
        schema: "willow.semantic-diff.v4",
        status,
        base: Some(RevisionDescription {
            revision: options.base.clone(),
            digest: before_digest,
        }),
        target: Some(RevisionDescription {
            revision: "working-tree".into(),
            digest: target_digest,
        }),
        changes: compared.changes,
        summary: ContractSummary {
            semantic: summary,
            effects: Some(EffectEvidence {
                changes: report.changes,
                summary: report.summary,
            }),
            contracts_lock: Some(b.lock),
            approval: Some(ApprovalEvidence {
                contract_status: if contract_changed {
                    ContractStatus::Changed
                } else {
                    ContractStatus::DigestUnchanged
                },
                approval_status: if approved {
                    ApprovalStatus::ApprovalVerified
                } else if changed {
                    ApprovalStatus::ApprovalRequired
                } else {
                    ApprovalStatus::NotRequired
                },
                specification_provenance: if approved {
                    SpecificationProvenance::ApprovedByPolicy
                } else {
                    SpecificationProvenance::Unverified
                },
                binding,
                semantic_dependencies_changed,
            }),
            lock_manifest: Some(LockManifest {
                protected_present: protected_lock_present,
                target_present: target_lock_present,
                change: match (protected_lock_present, target_lock_present) {
                    (Some(true), false) => ManifestChange::Removed,
                    (Some(false), true) => ManifestChange::Added,
                    (Some(_), _) => ManifestChange::Unchanged,
                    (None, _) => ManifestChange::Unknown,
                },
            }),
            verification: Some(VerificationEvidence {
                source_path: entry,
                base: a.verification.reports,
                target: b.verification.reports,
            }),
            comparison_work: Some(compared.work),
            proof_cache: Some(CacheWork {
                hits: cache.hits,
                misses: cache.misses,
                scope: "process-owned; disk artifacts are untrusted",
            }),
        },
    })
}

#[cfg(test)]
#[path = "semantic_diff_contracts_tests.rs"]
mod tests;
