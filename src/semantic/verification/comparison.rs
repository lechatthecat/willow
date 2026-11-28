//! Versioned scalar and immutable-data contract comparison. Unknown evidence is never preservation.
use super::*;
#[path = "comparison_calls.rs"]
mod calls;
use crate::parser::iter::{AstEvent, AstWalk};
use crate::semantic::field_writes::FieldWrites;
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
pub(super) fn canonical(v: impl Serialize) -> Json {
    let mut v = serde_json::to_value(v).expect("AST serialization");
    normalize(&mut v);
    v
}
pub(super) fn signature(f: &FunctionDecl) -> Json {
    canonical(
        json!({"name":f.name,"params":f.params,"return":f.return_type,"type_params":f.type_params,"async":f.is_async(),"public":f.public(),"bridge":f.rust_bridge}),
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::ai::evidence::present_nullable"
    )]
    pub no_panic: Option<Option<crate::semantic::type_checker::PanicEvidence>>,
}
impl Lock {
    pub fn digest(&self) -> String {
        digest(&serde_json::to_string(self).expect("lock serialization"))
    }
}
pub(super) fn contract_digest(f: &FunctionDecl) -> String {
    digest(&canonical(json!({"schema":"willow.typed-contract.v1","signature":signature(f),"attributes":f.effect_contracts,"requires":f.requires,"ensures":f.ensures})).to_string())
}
// Serialize declarations once per snapshot, not once for every function using them.
// Their complete canonical definitions include constructor/method bodies and defaults.
fn data_identity(program: &Program) -> (Option<String>, usize) {
    let mut declarations: Vec<_> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Class(c) => Some((c.name.as_str(), item)),
            Item::Enum(e) => Some((e.name.as_str(), item)),
            _ => None,
        })
        .collect();
    if declarations.is_empty() {
        return (None, 0);
    }
    declarations.sort_by(|a, b| a.0.cmp(b.0));
    let serialized = canonical(declarations).to_string();
    (Some(digest(&serialized)), serialized.len())
}

fn with_data_identity(identity: String, data: Option<&str>) -> String {
    match data {
        Some(data) => digest(&json!({"identity":identity,"data_declarations":data}).to_string()),
        // Keep scalar-only lock identities compatible with the original normalization.
        None => identity,
    }
}

fn relevant(f: &FunctionDecl) -> bool {
    has(f, EffectContractKind::NoPanic)
        || !f.requires.is_empty()
        || !f.ensures.is_empty()
        || has(f, EffectContractKind::Pure)
        || has(f, EffectContractKind::Verified)
}
pub struct Snapshot {
    pub(crate) targets: HashMap<ExprId, String>,
    pub program: Program,
    pub lock: Lock,
    pub verification: Verification,
    pub errors: Vec<String>,
    pub purity_losses: BTreeSet<String>,
    pub semantic_identities: BTreeMap<String, String>,
    pub dependency_edges: usize,
    pub canonical_bytes: usize,
    data_digest: Option<String>,
}
pub fn capture(program: Program, options: &Options, cache: &mut ProofCache) -> Snapshot {
    let symbols = crate::semantic::ids::SymbolInterner::new();
    let _symbols = symbols.enter();
    let mut checker = crate::semantic::TypeChecker::new();
    checker.verification_options = Some(options.clone());
    checker.verification_cache = std::mem::take(cache);
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
        .any(|i| !matches!(i, Item::Function(_) | Item::Class(_) | Item::Enum(_)))
    {
        errors.push(
            "comparison requires top-level functions and data declarations without imports".into(),
        );
    }
    let verification = if program
        .items
        .iter()
        .any(|i| matches!(i, Item::Function(f) if has(f, EffectContractKind::Verified)))
    {
        std::mem::take(&mut checker.verification)
    } else {
        verify_cached(
            &program,
            options,
            prior_error || !errors.is_empty(),
            &mut checker.verification_cache,
        )
    };
    *cache = std::mem::take(&mut checker.verification_cache);
    from_checked(
        program,
        HashMap::new(),
        verification,
        errors,
        purity_losses,
        &checker.panic_evidence,
    )
}
pub(crate) fn from_checked(
    program: Program,
    targets: HashMap<ExprId, String>,
    verification: Verification,
    mut errors: Vec<String>,
    purity_losses: BTreeSet<String>,
    panic_evidence: &HashMap<
        crate::semantic::ids::FunctionId,
        crate::semantic::type_checker::PanicEvidence,
    >,
) -> Snapshot {
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
    let (data_digest, mut bytes) = data_identity(&program);
    for f in &fs {
        let body = canonical(f).to_string();
        bytes += body.len();
        local.push(digest(&body));
        contracts.push(with_data_identity(
            contract_digest(f),
            data_digest.as_deref(),
        ));
        let mut deps = encode::resolved_edges(f, &names, Some(&targets));
        for event in AstWalk::new(AstEvent::Block(&f.body)).chain(
            f.requires
                .iter()
                .chain(&f.ensures)
                .flat_map(|e| AstWalk::new(AstEvent::Expr(e))),
        ) {
            if let AstEvent::Expr(expr @ Expr::Var(n, ..)) = event
                && let Some(&id) = names.get(targets.get(&expr.id()).unwrap_or(n).as_str())
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
                let mut deps: Vec<_> = edges[id]
                    .iter()
                    .map(|&d| (&fs[d].name, &semantic[d]))
                    .collect();
                // Source declaration positions are not semantic identities.
                deps.sort_unstable_by(|a, b| a.0.cmp(b.0));
                dependency_digests[id] =
                    with_data_identity(digest(&json!(deps).to_string()), data_digest.as_deref());
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
                    no_panic: has(f, EffectContractKind::NoPanic).then(|| {
                        panic_evidence
                            .get(&crate::semantic::ids::FunctionId::free(&f.name))
                            .cloned()
                    }),
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
        targets,
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
        data_digest,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Preservation {
    Unknown,
    NotPreserved,
    Proven,
}
impl Preservation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "UNKNOWN",
            Self::NotPreserved => "NOT_PRESERVED",
            Self::Proven => "PROVEN",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BehaviorEquivalence {
    Unsupported,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonObligationKind {
    RequiresDomain,
    PreconditionNotStrengthened,
    EnsuresDomain,
    PostconditionNotWeakened,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SolverResult {
    Sat,
    Unsat,
    Unknown,
}
impl From<Answer> for SolverResult {
    fn from(answer: Answer) -> Self {
        match answer {
            Answer::Sat => Self::Sat,
            Answer::Unsat => Self::Unsat,
            Answer::Unknown => Self::Unknown,
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct ComparisonObligation {
    pub counterexample_candidate: Option<String>,
    pub expected_solver_result: SolverResult,
    pub kind: ComparisonObligationKind,
    pub result: SolverResult,
}
#[derive(Debug, Clone, Serialize)]
pub struct ComparisonPurity {
    pub base_declared: bool,
    pub base_proven: bool,
    pub target_declared: bool,
    pub target_proven: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct ComparisonVerification {
    pub base: Option<Status>,
    pub declaration_changed: bool,
    pub target: Option<Status>,
}
#[derive(Debug, Clone, Serialize)]
pub struct ProofDependencies {
    pub digests: Vec<String>,
    pub functions: Vec<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct ComparisonDependencies {
    pub base: Option<ProofDependencies>,
    pub target: Option<ProofDependencies>,
}
#[derive(Debug, Clone, Serialize)]
pub struct ComparisonChange {
    pub base: Option<Digests>,
    pub behavior_equivalent: BehaviorEquivalence,
    pub contract_preserved: Preservation,
    pub implementation_verified: bool,
    pub input_symbols: BTreeMap<String, String>,
    pub obligations: Vec<ComparisonObligation>,
    pub proof_dependencies: ComparisonDependencies,
    pub proof_dependencies_changed: bool,
    pub purity: ComparisonPurity,
    pub semantic_identity_changed: bool,
    pub symbol: String,
    pub target: Option<Digests>,
    pub unknown_reasons: Vec<String>,
    pub verification: ComparisonVerification,
}
#[derive(Debug, Clone, Serialize)]
pub struct ComparisonWork {
    pub base_bytes: usize,
    pub base_edges: usize,
    pub comparison_queries: usize,
    pub predicate_bytes: usize,
    pub target_bytes: usize,
    pub target_edges: usize,
}
#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub changes: Vec<ComparisonChange>,
    pub contract_violations: usize,
    pub uncertainty: Vec<String>,
    pub work: ComparisonWork,
}

pub fn compare(a: &Snapshot, b: &Snapshot, options: &Options) -> Json {
    serde_json::to_value(compare_typed(a, b, options)).expect("contract comparison serializes")
}

#[allow(
    clippy::too_many_lines,
    reason = "one comparison session shares solver declarations and revision-specific name environments; keeping obligation queries beside status aggregation makes uncertainty and violation handling auditable"
)]
pub fn compare_typed(a: &Snapshot, b: &Snapshot, options: &Options) -> Comparison {
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
    let data_a = DataTypes {
        symbol_namespace: "old",
        targets: Some(&a.targets),
        ..DataTypes::new(&a.program, &FieldWrites::from_program(&a.program))
    };
    let data_b = DataTypes {
        symbol_namespace: "new",
        targets: Some(&b.targets),
        ..DataTypes::new(&b.program, &FieldWrites::from_program(&b.program))
    };
    // An identical spelling with a different representation cannot share solver sorts.
    // Conservatively require unchanged data declarations and immutable type domains.
    let data_changed = a.data_digest != b.data_digest || data_a.definitions != data_b.definitions;
    let mut engine = if !valid_budget(options) {
        Err(FailureKind::Infrastructure.error("invalid verification timeout/resource budget"))
    } else {
        solver::Solver::new(&options.solver, options.timeout_ms)
    }
    .and_then(|mut s| {
        s.exchange(&format!(
            "{PRELUDE}(set-option :rlimit {})\n",
            options.resource_limit
        ))?;
        // Both revisions use the same sort names. Declare the shared model only once.
        s.exchange(&data_a.definitions)?;
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
    let mut declarations = calls::Declarations::new(a, b);
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
        let mut status = Preservation::Unknown;
        let mut obligations = Vec::new();
        let mut reasons = Vec::new();
        let mut input_symbols = BTreeMap::new();
        let fa = old.get(key);
        let fb = new.get(key);
        let pure_a = fa.is_some_and(|f| has(f, EffectContractKind::Pure));
        let pure_b = fb.is_some_and(|f| has(f, EffectContractKind::Pure));
        if let (Some(fa), Some(fb)) = (fa, fb) {
            if signature(fa) != signature(fb) {
                status = Preservation::NotPreserved;
                reasons.push("signature changed".to_owned());
            } else if data_changed {
                status = Preservation::NotPreserved;
                reasons.push("data declarations or immutable domains changed".into());
            } else if pure_a && (!pure_b || b.purity_losses.contains(key)) {
                status = Preservation::NotPreserved;
                reasons.push("purity guarantee lost".into());
            } else if a.errors.is_empty() && b.errors.is_empty() && a.purity_losses.is_empty() {
                let result = (|| -> VerificationResult<bool> {
                    let i = names_a[key];
                    let j = names_b[key];
                    let s = engine.as_mut().map_err(|e| e.clone())?;
                    calls::declare(
                        fa,
                        &functions,
                        &names_a,
                        &data_a,
                        &reports_a,
                        &mut declarations,
                        s,
                    )?;
                    calls::declare(
                        fb,
                        &functions,
                        &names_b,
                        &data_b,
                        &reports_b,
                        &mut declarations,
                        s,
                    )?;
                    let ca = contract_predicates(i, &functions, &names_a, &data_a)?;
                    let cb = contract_predicates(j, &functions, &names_b, &data_b)?;
                    predicate_bytes += ca.len() + cb.len();
                    s.exchange(&ca)?;
                    s.exchange(&cb)?;
                    let mut args = Vec::new();
                    let mut domain = vec!["true".into()];
                    for (n, p) in fa.params.iter().enumerate() {
                        let v = format!("cmp{i}_{n}");
                        data_a.check(&p.ty)?;
                        data_b.check(&p.ty)?;
                        let ty = sort(&p.ty)?;
                        s.exchange(&format!("(declare-const {v} {ty})\n"))?;
                        domain.push(super::data::domain(&p.ty, &v));
                        input_symbols.insert(v.clone(), p.name.clone());
                        args.push(v);
                    }
                    let ret = format!("cmp{i}_return");
                    data_a.check(&fa.return_type)?;
                    data_b.check(&fa.return_type)?;
                    let ty = sort(&fa.return_type)?;
                    s.exchange(&format!("(declare-const {ret} {ty})\n"))?;
                    domain.push(super::data::domain(&fa.return_type, &ret));
                    let pre_a = application(&format!("c{i}_pre"), &args);
                    let pre_b = application(&format!("c{j}_pre"), &args);
                    let pa_a = application(&format!("c{i}_pre_assume"), &args);
                    let pa_b = application(&format!("c{j}_pre_assume"), &args);
                    let ps_a = application(&format!("c{i}_pre_safe"), &args);
                    let ps_b = application(&format!("c{j}_pre_safe"), &args);
                    input_symbols.insert(ret.clone(), "result".to_owned());
                    args.push(ret);
                    let post_a = application(&format!("c{i}_post"), &args);
                    let post_b = application(&format!("c{j}_post"), &args);
                    let ss_a = application(&format!("c{i}_post_safe"), &args);
                    let ss_b = application(&format!("c{j}_post_safe"), &args);
                    let post_assume_a = application(&format!("c{i}_post_assume"), &args);
                    let post_assume_b = application(&format!("c{j}_post_assume"), &args);
                    let domain = format!(
                        "(and {} {pa_a} {pa_b} {post_assume_a} {post_assume_b})",
                        domain.join(" ")
                    );
                    let mut preserved = true;
                    for (kind, formula) in [
                        (
                            ComparisonObligationKind::RequiresDomain,
                            format!("(and {domain} (not (and {ps_a} {ps_b})))"),
                        ),
                        (
                            ComparisonObligationKind::PreconditionNotStrengthened,
                            format!("(and {domain} {pre_a} (not {pre_b}))"),
                        ),
                        (
                            ComparisonObligationKind::EnsuresDomain,
                            format!("(and {domain} {pre_a} (not (and {ss_a} {ss_b})))"),
                        ),
                        (
                            ComparisonObligationKind::PostconditionNotWeakened,
                            format!("(and {domain} {pre_a} {post_b} (not {post_a}))"),
                        ),
                    ] {
                        comparison_queries += 1;
                        let (answer, candidate) = s.query(&formula, &args)?;
                        obligations.push(ComparisonObligation {
                            kind,
                            result: answer.into(),
                            expected_solver_result: SolverResult::Unsat,
                            counterexample_candidate: candidate,
                        });
                        match answer {
                            Answer::Unknown => {
                                return Err(
                                    FailureKind::Infrastructure.error("solver returned UNKNOWN")
                                );
                            }
                            Answer::Sat => preserved = false,
                            Answer::Unsat => {}
                        }
                    }
                    Ok(preserved)
                })();
                match result {
                    Ok(true) => status = Preservation::Proven,
                    Ok(false) => status = Preservation::NotPreserved,
                    Err(e) => reasons.push(e.message.into_owned()),
                }
            }
        } else {
            status = Preservation::NotPreserved;
            reasons.push("declaration added or removed; no preservation claim".into());
        }
        if status == Preservation::Unknown {
            unknown.push(format!("{key}: {}", reasons.join("; ")));
        }
        if status == Preservation::NotPreserved {
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
        if b.purity_losses.contains(key) && status != Preservation::NotPreserved {
            violations += 1;
        }
        let da = a.lock.functions.get(key);
        let db = b.lock.functions.get(key);
        let dependencies = |report: &&Report| ProofDependencies {
            functions: report.dependencies.clone(),
            digests: report.dependency_digests.clone(),
        };
        changes.push(ComparisonChange {
            symbol: key.to_owned(),
            contract_preserved: status,
            implementation_verified: proof_b == Some(&Status::Proven),
            behavior_equivalent: BehaviorEquivalence::Unsupported,
            purity: ComparisonPurity {
                base_declared: pure_a,
                target_declared: pure_b,
                base_proven: pure_a && !a.purity_losses.contains(key) && a.errors.is_empty(),
                target_proven: pure_b && !b.purity_losses.contains(key) && b.errors.is_empty(),
            },
            verification: ComparisonVerification {
                base: proof_a.cloned(),
                target: proof_b.cloned(),
                declaration_changed: fa.is_some_and(|f| has(f, EffectContractKind::Verified))
                    != fb.is_some_and(|f| has(f, EffectContractKind::Verified)),
            },
            semantic_identity_changed: a.semantic_identities.get(key)
                != b.semantic_identities.get(key),
            proof_dependencies_changed: da.map(|d| &d.semantics) != db.map(|d| &d.semantics),
            proof_dependencies: ComparisonDependencies {
                base: reports_a.get(key).map(dependencies),
                target: reports_b.get(key).map(dependencies),
            },
            base: da.cloned(),
            target: db.cloned(),
            obligations,
            input_symbols,
            unknown_reasons: reasons,
        });
    }
    functions.clear();
    Comparison {
        changes,
        contract_violations: violations,
        uncertainty: unknown,
        work: ComparisonWork {
            comparison_queries,
            predicate_bytes,
            base_edges: a.dependency_edges,
            target_edges: b.dependency_edges,
            base_bytes: a.canonical_bytes,
            target_bytes: b.canonical_bytes,
        },
    }
}

#[cfg(test)]
#[path = "comparison_tests.rs"]
mod tests;
