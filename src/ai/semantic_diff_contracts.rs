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
/// A trusted CI wrapper supplies the verifier executable, never project config.
/// The verifier independently authenticates the evidence and protected revision.
/// Its exact JSON response must bind the complete request and approve it.
fn approval(policy: &Policy, root: &Path, binding: &Value) -> Result<bool> {
    approval_in_context(
        policy,
        root,
        binding,
        std::env::var("CI").is_ok_and(|v| v == "true"),
    )
}
fn approval_in_context(
    policy: &Policy,
    root: &Path,
    binding: &Value,
    trusted_ci: bool,
) -> Result<bool> {
    let (Some(verifier), Some(evidence)) = (&policy.approval_verifier, &policy.approval_evidence)
    else {
        return Ok(false);
    };
    ensure!(
        trusted_ci,
        "approval verification requires trusted CI invocation"
    );
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
    let mut child = std::process::Command::new(verifier)
        .arg(&evidence)
        .arg(serde_json::to_string(binding)?)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let output = child.stdout.take().context("approval verifier output")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut bytes = Vec::new();
        let result = output.take(65537).read_to_end(&mut bytes).map(|_| bytes);
        let _ = tx.send(result);
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("approval verifier timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
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
pub fn verify(options: &Options, policy: &Policy) -> Result<SemanticDiff> {
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
    let compared = comparison::compare(
        &a,
        &b,
        &verification::Options {
            artifacts: None,
            ..Default::default()
        },
    );
    // Preserve effect policy coverage in v2 as well as the unchanged v1 entry point.
    let mut report = super::verify(options)?;
    let without_lock = |tree: &tree::Tree| {
        let mut t = tree.clone();
        t.remove("contracts.lock");
        t
    };
    ensure!(
        report.base["digest"] == hash_serialized(&without_lock(&before))?
            && report.target["digest"] == hash_serialized(&without_lock(&target))?,
        "effect and contract snapshots differ"
    );
    report.schema = "willow.semantic-diff.v2";
    report.summary["effects"] = json!({"changes":report.changes,"summary":report.summary});
    report.changes = compared["changes"]
        .as_array()
        .context("contract changes")?
        .clone();
    let mut uncertainty = report.summary["uncertainty"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    uncertainty.retain(|v| {
        v.as_str().is_none_or(|s| {
            !(s.starts_with("base: E2630:")
                || s.starts_with("target: E2630:")
                || ((s.starts_with("base: E2620:") || s.starts_with("target: E2620:"))
                    && !s.contains("UNKNOWN")))
        })
    });
    uncertainty.extend(
        compared["uncertainty"]
            .as_array()
            .context("contract uncertainty")?
            .clone(),
    );
    let mut approved = false;
    let mut changed = true;
    let mut contract_changed = true;
    let mut semantic_dependencies_changed = true;
    let mut protected_lock_present = None;
    let target_lock_present = target.contains_key("contracts.lock");
    let mut binding = Value::Null;
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
            uncertainty.push(json!(
                "protected contracts.lock does not match analyzed contracts"
            ));
        }
        uncertainty.extend(protected.errors.iter().map(|e| json!(e)));
        let manifest_present = protected_inputs.contains_key("contracts.lock");
        protected_lock_present = Some(manifest_present);
        contract_changed = protected.lock != b.lock;
        semantic_dependencies_changed = protected.lock.functions.len() != b.lock.functions.len()
            || protected.lock.functions.iter().zip(&b.lock.functions).any(
                |((name, digest), (other_name, other_digest))| {
                    name != other_name || digest.semantics != other_digest.semantics
                },
            );
        // Optional bootstrapping does not authorize removal of a protected manifest.
        changed = contract_changed || (manifest_present && !target_lock_present);
        binding = json!({"protected_commit":commit,"base_tree":before_digest,"target_tree":target_digest,"protected_contracts":protected.lock.digest(),"target_contracts":b.lock.digest(),"normalization":b.lock.normalization,"protected_lock_present":manifest_present,"target_lock_present":target_lock_present});
        match approval(policy, &root, &binding) {
            Ok(v) => approved = v && protected.errors.is_empty() && b.errors.is_empty(),
            Err(e) => uncertainty.push(json!(format!("approval evidence: {e:#}"))),
        }
    } else {
        uncertainty.push(json!("protected baseline unavailable"));
    }
    if !consistent(&target, &b.lock)? {
        uncertainty.push(json!(
            "local contracts.lock does not match analyzed contracts"
        ));
    }
    if !consistent(&before, &a.lock)? {
        uncertainty.push(json!(
            "base contracts.lock does not match analyzed contracts"
        ));
    }
    let violations = report.summary["contract_violations"].as_u64().unwrap_or(0)
        + compared["contract_violations"].as_u64().unwrap_or(0);
    let policy_violations =
        report.summary["policy_violations"].as_u64().unwrap_or(0) + u64::from(changed && !approved);
    report.summary["contract_violations"] = json!(violations);
    report.summary["policy_violations"] = json!(policy_violations);
    report.summary["uncertainty"] = json!(uncertainty);
    report.summary["contracts_lock"] = json!(b.lock);
    report.summary["approval"] = json!({"contract_status":if contract_changed {"contract_changed"} else {"contract_digest_unchanged"},"approval_status":if approved {"approval_verified"} else if changed {"approval_required"} else {"not_required"},"specification_provenance":if approved {"APPROVED_BY_POLICY"} else {"UNVERIFIED"},"binding":binding,"semantic_dependencies_changed":semantic_dependencies_changed});
    report.summary["lock_manifest"] = json!({
        "protected_present":protected_lock_present,"target_present":target_lock_present,
        "change": match (protected_lock_present,target_lock_present) {
            (Some(true),false)=>"removed",(Some(false),true)=>"added",
            (Some(_),_)=>"unchanged",(None,_)=>"unknown"
        }
    });
    // Preserve complete proof evidence, including failures, without rerunning the solver.
    // Locations and declaration IDs are scoped to this workspace-relative source.
    report.summary["verification"] = json!({
        "source_path":entry,
        "base":a.verification.reports,
        "target":b.verification.reports
    });
    report.summary["comparison_work"] = compared["work"].clone();
    report.summary["proof_cache"] = json!({"hits":cache.hits,"misses":cache.misses,"scope":"process-owned; disk artifacts are untrusted"});
    report.base = json!({"revision":options.base,"digest":before_digest});
    report.target = json!({"revision":"working-tree","digest":target_digest});
    if hash_serialized(&tree::target_inputs(&root, true)?)? != target_digest {
        report.summary["uncertainty"]
            .as_array_mut()
            .unwrap()
            .push(json!("snapshot changed during verification"));
    }
    report.status = if report.summary["uncertainty"]
        .as_array()
        .is_some_and(|a| a.is_empty())
    {
        "complete"
    } else {
        "incomplete"
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
    Ok(report)
}

#[cfg(test)]
#[path = "semantic_diff_contracts_tests.rs"]
mod tests;
