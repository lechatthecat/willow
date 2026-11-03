//! Versioned scalar contract comparison. Unknown evidence is never preservation.
use super::*;
use crate::parser::iter::{AstEvent, AstWalk};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

/// Remove only source coordinates and process-local syntax identities.
fn normalize(v: &mut Json) {
    match v {
        Json::Object(m) => {
            if m.contains_key("file_id") && m.contains_key("start") && m.contains_key("end") {
                *v = Json::Null;
                return;
            }
            m.retain(|k, _| {
                !matches!(k.as_str(), "span" | "id" | "body_id" | "return_type_span")
                    && !k.ends_with("_span")
            });
            for v in m.values_mut() {
                normalize(v);
            }
        }
        Json::Array(a) => {
            let mut identity = false;
            for v in a {
                if identity {
                    *v = Json::Null;
                    identity = false;
                    continue;
                }
                identity = v.as_object().is_some_and(|m| {
                    m.contains_key("file_id") && m.contains_key("start") && m.contains_key("end")
                });
                normalize(v);
            }
        }
        _ => {}
    }
}
fn canonical(v: impl Serialize) -> Json {
    let mut v = serde_json::to_value(v).expect("AST serialization");
    normalize(&mut v);
    v
}
pub(super) fn signature(f: &FunctionDecl) -> Json {
    canonical(
        json!({"name":f.name,"params":f.params,"return":f.return_type,"type_params":f.type_params,"async":f.is_async,"public":f.public,"bridge":f.rust_bridge}),
    )
}
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    pub schema: String,
    pub normalization: String,
    pub functions: BTreeMap<String, Digests>,
}
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Digests {
    pub contract: String,
    pub semantics: String,
}
impl Lock {
    pub fn digest(&self) -> String {
        digest(&serde_json::to_string(self).expect("lock serialization"))
    }
}
fn relevant(f: &FunctionDecl) -> bool {
    !f.requires.is_empty()
        || !f.ensures.is_empty()
        || has(f, EffectContractKind::Pure)
        || has(f, EffectContractKind::Verified)
}
pub struct Snapshot {
    pub program: Program,
    pub lock: Lock,
    pub verification: Verification,
    pub errors: Vec<String>,
    pub purity_losses: BTreeSet<String>,
    pub semantic_identities: BTreeMap<String, String>,
    pub dependency_edges: usize,
    pub canonical_bytes: usize,
}
pub fn capture(program: Program, options: &Options, cache: &mut ProofCache) -> Snapshot {
    let symbols = crate::semantic::ids::SymbolInterner::new();
    let _symbols = symbols.enter();
    let mut checker = crate::semantic::TypeChecker::new();
    checker.defer_static_verification = true;
    checker.check_module_program(&program);
    let purity_spans: BTreeSet<_> = checker
        .errors
        .iter()
        .filter(|d| d.code == ErrorCode::E2620)
        .flat_map(|d| d.labels.first())
        .map(|l| l.span.start)
        .collect();
    let purity_losses = program
        .items
        .iter()
        .filter_map(|i| {
            if let Item::Function(f) = i {
                f.effect_contracts
                    .iter()
                    .any(|c| {
                        c.kind == EffectContractKind::Pure && purity_spans.contains(&c.span.start)
                    })
                    .then(|| f.name.clone())
            } else {
                None
            }
        })
        .collect();
    let prior_error = checker.errors.iter().any(|d| d.severity == Severity::Error);
    let mut errors: Vec<_> = checker
        .errors
        .iter()
        .filter(|d| {
            d.severity == Severity::Error
                && (d.code != ErrorCode::E2620 || d.message.contains("UNKNOWN"))
                && !matches!(
                    d.code,
                    ErrorCode::E2630 | ErrorCode::E2631 | ErrorCode::E2632
                )
        })
        .map(|d| d.message.clone())
        .collect();
    if program
        .items
        .iter()
        .any(|i| !matches!(i, Item::Function(_)))
    {
        errors.push(
            "comparison currently requires top-level scalar declarations without imports".into(),
        );
    }
    let verification = verify_cached(&program, options, prior_error || !errors.is_empty(), cache);
    let fs: Vec<_> = program
        .items
        .iter()
        .filter_map(|i| {
            if let Item::Function(f) = i {
                Some(f)
            } else {
                None
            }
        })
        .collect();
    let names: HashMap<_, _> = fs
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), i))
        .collect();
    if names.len() != fs.len() {
        errors.push("ambiguous function identity".into());
    }
    let mut local = Vec::new();
    let mut contracts = Vec::new();
    let mut edges = Vec::new();
    let mut bytes = 0;
    for f in &fs {
        let body = canonical(f).to_string();
        bytes += body.len();
        local.push(digest(&body));
        contracts.push(digest(&canonical(json!({"schema":"willow.typed-contract.v1","signature":signature(f),"attributes":f.effect_contracts,"requires":f.requires,"ensures":f.ensures})).to_string()));
        let mut deps = encode::edges(f, &names);
        for event in AstWalk::new(AstEvent::Block(&f.body)).chain(
            f.requires
                .iter()
                .chain(&f.ensures)
                .flat_map(|e| AstWalk::new(AstEvent::Expr(e))),
        ) {
            if let AstEvent::Expr(Expr::Var(n, ..)) = event
                && let Some(&id) = names.get(n.as_str())
                && fs[id].constant.is_some()
            {
                deps.insert(id);
            }
        }
        edges.push(deps);
    }
    let mut colors = vec![0; fs.len()];
    let mut semantic = vec![String::new(); fs.len()];
    let mut dependency_digests = vec![String::new(); fs.len()];
    for root in 0..fs.len() {
        if colors[root] != 0 {
            continue;
        }
        let mut stack = vec![(root, false)];
        while let Some((id, exit)) = stack.pop() {
            if exit {
                let deps: Vec<_> = edges[id]
                    .iter()
                    .map(|&d| (&fs[d].name, &semantic[d]))
                    .collect();
                dependency_digests[id] = digest(&json!(deps).to_string());
                semantic[id] = digest(
                    &json!({"local":local[id],"dependencies":dependency_digests[id]}).to_string(),
                );
                colors[id] = 2;
                continue;
            }
            if colors[id] == 2 {
                continue;
            }
            if colors[id] == 1 {
                errors.push("recursive semantic dependency".into());
                continue;
            }
            colors[id] = 1;
            stack.push((id, true));
            for &d in edges[id].iter().rev() {
                if colors[d] == 1 {
                    errors.push("recursive semantic dependency".into());
                } else if colors[d] == 0 {
                    stack.push((d, false));
                }
            }
        }
    }
    let functions = fs
        .iter()
        .enumerate()
        .filter(|(_, f)| f.constant.is_none() && relevant(f))
        .map(|(i, f)| {
            (
                f.name.clone(),
                Digests {
                    contract: contracts[i].clone(),
                    semantics: dependency_digests[i].clone(),
                },
            )
        })
        .collect();
    let semantic_identities = fs
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.clone(), semantic[i].clone()))
        .collect();
    Snapshot {
        semantic_identities,
        program,
        purity_losses,
        lock: Lock {
            schema: "willow.contracts-lock.v1".into(),
            normalization: "typed-ast.v1/sha256/checked-i64".into(),
            functions,
        },
        verification,
        errors,
        dependency_edges: edges.iter().map(BTreeSet::len).sum(),
        canonical_bytes: bytes,
    }
}

pub fn compare(a: &Snapshot, b: &Snapshot, options: &Options) -> Json {
    let old: BTreeMap<_, _> = a
        .program
        .items
        .iter()
        .filter_map(|i| {
            if let Item::Function(f) = i {
                f.constant.is_none().then_some((f.name.as_str(), f))
            } else {
                None
            }
        })
        .collect();
    let new: BTreeMap<_, _> = b
        .program
        .items
        .iter()
        .filter_map(|i| {
            if let Item::Function(f) = i {
                f.constant.is_none().then_some((f.name.as_str(), f))
            } else {
                None
            }
        })
        .collect();
    let mut functions: Vec<_> = a
        .program
        .items
        .iter()
        .chain(&b.program.items)
        .filter_map(|i| {
            if let Item::Function(f) = i {
                Some(f)
            } else {
                None
            }
        })
        .collect();
    // Each revision has an isolated constant-name environment.
    let split = a
        .program
        .items
        .iter()
        .filter(|i| matches!(i, Item::Function(_)))
        .count();
    let names_a: HashMap<_, _> = functions[..split]
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), i))
        .collect();
    let names_b: HashMap<_, _> = functions[split..]
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), i + split))
        .collect();
    let mut engine = if !valid_budget(options) {
        Err("invalid verification timeout/resource budget".into())
    } else {
        solver::Solver::new(&options.solver, options.timeout_ms)
    }
    .and_then(|mut s| {
        s.exchange(&format!(
            "{PRELUDE}(set-option :rlimit {})\n",
            options.resource_limit
        ))?;
        Ok(s)
    });
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).copied().collect();
    let reports_a: BTreeMap<_, _> = a
        .verification
        .reports
        .iter()
        .map(|r| (r.function.as_str(), r))
        .collect();
    let reports_b: BTreeMap<_, _> = b
        .verification
        .reports
        .iter()
        .map(|r| (r.function.as_str(), r))
        .collect();
    let mut changes = Vec::new();
    let mut unknown = Vec::new();
    let mut violations = 0;
    let mut comparison_queries = 0;
    let mut predicate_bytes = 0;
    unknown.extend(a.errors.clone());
    unknown.extend(b.errors.clone());
    for key in keys {
        if !old.get(key).is_some_and(|f| relevant(f)) && !new.get(key).is_some_and(|f| relevant(f))
        {
            continue;
        }
        let mut status = "UNKNOWN";
        let mut obligations = Vec::new();
        let mut reasons = Vec::new();
        let fa = old.get(key);
        let fb = new.get(key);
        let pure_a = fa.is_some_and(|f| has(f, EffectContractKind::Pure));
        let pure_b = fb.is_some_and(|f| has(f, EffectContractKind::Pure));
        if let (Some(fa), Some(fb)) = (fa, fb) {
            if signature(fa) != signature(fb) {
                status = "NOT_PRESERVED";
                reasons.push("signature changed".to_owned());
            } else if pure_a && (!pure_b || b.purity_losses.contains(key)) {
                status = "NOT_PRESERVED";
                reasons.push("purity guarantee lost".into());
            } else if a.errors.is_empty() && b.errors.is_empty() && a.purity_losses.is_empty() {
                let result = (|| -> Result<bool, String> {
                    let i = names_a[key];
                    let j = names_b[key];
                    let s = engine.as_mut().map_err(|e| e.clone())?;
                    let ca = contract_predicates(i, &functions, &names_a)?;
                    let cb = contract_predicates(j, &functions, &names_b)?;
                    predicate_bytes += ca.len() + cb.len();
                    s.exchange(&ca)?;
                    s.exchange(&cb)?;
                    let mut args = Vec::new();
                    let mut domain = vec!["true".into()];
                    for (n, p) in fa.params.iter().enumerate() {
                        let v = format!("cmp{i}_{n}");
                        let ty = sort(&p.ty)?;
                        s.exchange(&format!("(declare-const {v} {ty})\n"))?;
                        if ty == "Int" {
                            domain.push(bounds(&v));
                        }
                        args.push(v);
                    }
                    let ret = format!("cmp{i}_return");
                    let ty = sort(&fa.return_type)?;
                    s.exchange(&format!("(declare-const {ret} {ty})\n"))?;
                    if ty == "Int" {
                        domain.push(bounds(&ret));
                    }
                    let pre_a = application(&format!("c{i}_pre"), &args);
                    let pre_b = application(&format!("c{j}_pre"), &args);
                    let ps_a = application(&format!("c{i}_pre_safe"), &args);
                    let ps_b = application(&format!("c{j}_pre_safe"), &args);
                    args.push(ret);
                    let post_a = application(&format!("c{i}_post"), &args);
                    let post_b = application(&format!("c{j}_post"), &args);
                    let ss_a = application(&format!("c{i}_post_safe"), &args);
                    let ss_b = application(&format!("c{j}_post_safe"), &args);
                    let domain = format!("(and {})", domain.join(" "));
                    let mut preserved = true;
                    for (kind, formula) in [
                        (
                            "requires_domain",
                            format!("(and {domain} (not (and {ps_a} {ps_b})))"),
                        ),
                        (
                            "precondition_not_strengthened",
                            format!("(and {domain} {pre_a} (not {pre_b}))"),
                        ),
                        (
                            "ensures_domain",
                            format!("(and {domain} {pre_a} (not (and {ss_a} {ss_b})))"),
                        ),
                        (
                            "postcondition_not_weakened",
                            format!("(and {domain} {pre_a} {post_b} (not {post_a}))"),
                        ),
                    ] {
                        comparison_queries += 1;
                        let (answer, _) = s.query(&formula, &[])?;
                        obligations.push(json!({"kind":kind,"result":answer}));
                        if answer == "unknown" {
                            return Err("solver returned UNKNOWN".into());
                        }
                        if answer != "unsat" {
                            preserved = false;
                        }
                    }
                    Ok(preserved)
                })();
                match result {
                    Ok(true) => status = "PROVEN",
                    Ok(false) => status = "NOT_PRESERVED",
                    Err(e) => reasons.push(e),
                }
            }
        } else {
            status = "NOT_PRESERVED";
            reasons.push("declaration added or removed; no preservation claim".into());
        }
        if status == "UNKNOWN" {
            unknown.push(format!("{key}: {}", reasons.join("; ")));
        }
        if status == "NOT_PRESERVED" {
            violations += 1;
        }
        let proof_a = reports_a.get(key).map(|r| &r.status);
        let proof_b = reports_b.get(key).map(|r| &r.status);
        if proof_b == Some(&Status::Unknown)
            && fb.is_some_and(|f| has(f, EffectContractKind::Verified))
        {
            unknown.push(format!("{key}: implementation proof is UNKNOWN"));
        }
        if matches!(
            proof_b,
            Some(Status::Counterexample | Status::VacuousPrecondition)
        ) {
            violations += 1;
        }
        if b.purity_losses.contains(key) && status != "NOT_PRESERVED" {
            violations += 1;
        }
        let da = a.lock.functions.get(key);
        let db = b.lock.functions.get(key);
        changes.push(json!({"symbol":key,"contract_preserved":status,"implementation_verified":proof_b==Some(&Status::Proven),"behavior_equivalent":"UNSUPPORTED","purity":{"base_declared":pure_a,"target_declared":pure_b,"base_proven":pure_a && !a.purity_losses.contains(key) && a.errors.is_empty(),"target_proven":pure_b && !b.purity_losses.contains(key) && b.errors.is_empty()},"verification":{"base":proof_a,"target":proof_b,"declaration_changed":fa.is_some_and(|f|has(f,EffectContractKind::Verified))!=fb.is_some_and(|f|has(f,EffectContractKind::Verified))},"semantic_identity_changed":a.semantic_identities.get(key)!=b.semantic_identities.get(key),"proof_dependencies_changed":da.map(|d|&d.semantics)!=db.map(|d|&d.semantics),"proof_dependencies":{"base":reports_a.get(key).map(|r|json!({"functions":r.dependencies,"digests":r.dependency_digests})),"target":reports_b.get(key).map(|r|json!({"functions":r.dependencies,"digests":r.dependency_digests}))},"base":da,"target":db,"obligations":obligations,"unknown_reasons":reasons}));
    }
    functions.clear();
    json!({"changes":changes,"contract_violations":violations,"uncertainty":unknown,"work":{"comparison_queries":comparison_queries,"predicate_bytes":predicate_bytes,"base_edges":a.dependency_edges,"target_edges":b.dependency_edges,"base_bytes":a.canonical_bytes,"target_bytes":b.canonical_bytes}})
}

#[cfg(test)]
#[path = "comparison_tests.rs"]
mod tests;
