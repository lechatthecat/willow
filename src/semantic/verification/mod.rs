//! Fail-closed static verification over checked scalars and immutable data.
//! Mathematical integers model values; every finite-width operation has a
//! separate safety obligation. Summaries share helper bodies across call sites.
mod cache;
#[cfg(test)]
mod panic_effect_tests;
#[cfg(test)]
thread_local! {
    static PROOF_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SOLVER_STARTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
pub mod comparison;
mod data;
mod encode;
mod error;
pub use cache::ProofCache;
use data::*;
mod solver;
#[cfg(test)]
mod tests;
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use encode::*;
use error::{FailureKind, FunctionFailure, VerificationError, VerificationResult};
use serde::Serialize;
use sha2::{Digest, Sha256};
use solver::Answer;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

const PRELUDE: &str = "(define-fun wabs ((x Int)) Int (ite (< x 0) (- x) x))\n(define-fun wdiv ((x Int) (y Int)) Int (ite (= (< x 0) (< y 0)) (div (wabs x) (wabs y)) (- (div (wabs x) (wabs y)))))\n(define-fun wrem ((x Int) (y Int)) Int (- x (* y (wdiv x y))))\n";
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Proven,
    Counterexample,
    VacuousPrecondition,
    Unknown,
    RuntimeChecked,
}
#[derive(Debug, Clone, Serialize)]
pub struct Obligation {
    pub kind: String,
    pub solver_result: String,
    pub expected_solver_result: String,
    pub counterexample_candidate: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct InferredLoopAnnotations {
    pub location: Span,
    pub kind: &'static str,
    pub counter: String,
    pub captured_start: String,
    pub captured_end: String,
    /// `captured_start/end` are the once-evaluated range bounds, or 0 and
    /// the once-evaluated FrozenArray length. `counter` names the field above.
    pub invariant: &'static str,
    /// None means the user's variant is used instead. Arithmetic is checked i64.
    pub variant: Option<&'static str>,
}
#[derive(Debug, Serialize)]
pub struct Report {
    #[serde(skip)]
    failure_kind: Option<FailureKind>,
    pub purity: serde_json::Value,
    pub contract: serde_json::Value,
    pub specification_provenance: serde_json::Value,
    pub compiler_version: &'static str,
    pub solver_resource_limit: u64,
    pub dependency_digests: Vec<String>,
    pub schema: &'static str,
    pub function: String,
    /// Signature identity within the analyzed source; pair with its source path.
    pub declaration_id: String,
    pub location: Span,
    pub status: Status,
    pub model: &'static str,
    pub verified_scope: &'static str,
    pub analyzer_version: &'static str,
    pub solver_version: &'static str,
    pub solver_timeout_ms: u64,
    pub approval_status: &'static str,
    pub contract_digest: String,
    pub dependencies: Vec<String>,
    pub input_names: Vec<String>,
    pub input_symbols: std::collections::BTreeMap<String, String>,
    pub unknown_reasons: Vec<String>,
    pub obligations: Vec<Obligation>,
    pub vc_nodes: usize,
    pub inferred_loop_annotations: Vec<InferredLoopAnnotations>,
}
#[derive(Clone)]
pub struct Options {
    pub solver: PathBuf,
    pub timeout_ms: u64,
    pub resource_limit: u64,
    pub artifacts: Option<PathBuf>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            solver: std::env::var_os("WILLOW_Z3")
                .map(PathBuf::from)
                .unwrap_or_else(|| "z3".into()),
            timeout_ms: budget("WILLOW_VERIFICATION_TIMEOUT_MS", 5_000),
            resource_limit: budget("WILLOW_VERIFICATION_RLIMIT", 1_000_000),
            artifacts: Some(
                std::env::var_os("WILLOW_VERIFICATION_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| "target/willow-verification".into()),
            ),
        }
    }
}
fn valid_budget(options: &Options) -> bool {
    (1..=60_000).contains(&options.timeout_ms)
        && (1..=1_000_000_000).contains(&options.resource_limit)
}
fn budget(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .map(|v| v.parse().unwrap_or(0))
        .unwrap_or(default)
}
#[derive(Default)]
pub struct Verification {
    pub reports: Vec<Report>,
    pub smt: String,
    pub artifact_error: Option<String>,
}
impl Verification {
    fn with_artifacts(
        mut self,
        options: &Options,
        on_error: impl FnOnce(&VerificationError),
    ) -> Self {
        if let Some(dir) = &options.artifacts
            && let Err(cause) = self.write_artifacts(dir)
        {
            let error = FailureKind::Infrastructure.error(cause.to_string());
            on_error(&error);
            self.artifact_error = Some(error.message.into_owned());
        }
        self
    }

    fn write_artifacts(&self, dir: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(&self.reports)?;
        let mut hash = Sha256::new();
        hash.update(self.smt.as_bytes());
        hash.update(json.as_bytes());
        let dir = dir.join(format!("{:x}", crate::hex::Hex(&hash.finalize())));
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("verification.smt2"), &self.smt)?;
        std::fs::write(dir.join("verification.json"), json)
    }
}

fn digest(text: &str) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}
fn report(f: &FunctionDecl, options: &Options) -> Report {
    Report {
        failure_kind: None,
        purity: serde_json::Value::Null,
        contract: serde_json::Value::Null,
        specification_provenance: serde_json::Value::Null,
        compiler_version: env!("CARGO_PKG_VERSION"),
        solver_resource_limit: options.resource_limit,
        dependency_digests: Vec::new(),
        schema: "willow.verification.v1",
        function: f.name.clone(),
        declaration_id: digest(&comparison::signature(f).to_string()),
        location: f.span,
        status: Status::Unknown,
        model: "checked-i64",
        verified_scope: "checked scalars, immutable data, bounded quantifiers and annotated loops",
        analyzer_version: "4",
        solver_version: solver::VERSION,
        solver_timeout_ms: options.timeout_ms,
        approval_status: "UNVERIFIED",
        contract_digest: comparison::contract_digest(f),
        dependencies: Vec::new(),
        input_symbols: std::collections::BTreeMap::new(),
        input_names: f.params.iter().map(|p| p.name.clone()).collect(),
        unknown_reasons: Vec::new(),
        obligations: Vec::new(),
        vc_nodes: 0,
        inferred_loop_annotations: Vec::new(),
    }
}

pub(crate) fn check(
    program: &Program,
    previous: &[Diagnostic],
    calls: &crate::semantic::intrinsics::BuiltinCalls,
    writes: &crate::semantic::field_writes::FieldWrites,
    options: &Options,
    cache: &mut ProofCache,
) -> (Vec<Diagnostic>, Verification) {
    let mut diagnostics = Vec::new();
    // Methods are outside V1, including interface declarations without bodies.
    for item in &program.items {
        let attrs: Vec<_> = match item {
            Item::Class(c) => c.methods.iter().flat_map(|m| &m.effect_contracts).collect(),
            Item::Interface(i) => i.methods.iter().flat_map(|m| &m.effect_contracts).collect(),
            _ => Vec::new(),
        };
        for a in attrs
            .into_iter()
            .filter(|a| a.kind == EffectContractKind::Verified)
        {
            diagnostics.push(diagnostic(
                FailureKind::Unsupported.code(),
                a.span,
                "UNKNOWN: @verified V1 requires a top-level scalar function",
            ));
        }
    }
    if !program
        .items
        .iter()
        .any(|i| matches!(i, Item::Function(f) if has(f,EffectContractKind::Verified)))
    {
        return (diagnostics, Verification::default());
    }
    let prior_error = previous.iter().any(|d| d.severity == Severity::Error);
    let verification = verify_with_data(
        program,
        options,
        prior_error,
        cache,
        &DataTypes {
            calls: Some(calls),
            ..DataTypes::new(program, writes)
        },
    );
    for r in &verification.reports {
        if let Some(failure) = r.failure_kind {
            let detail = r.unknown_reasons.first().cloned().unwrap_or_else(|| {
                r.obligations
                    .last()
                    .map(|o| {
                        format!(
                            "{}: {}{}",
                            o.kind,
                            o.solver_result,
                            o.counterexample_candidate
                                .as_ref()
                                .map(|m| format!("; candidate inputs {m}"))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_default()
            });
            diagnostics.push(diagnostic(
                failure.code(),
                r.location,
                &format!("@verified `{}`: {:?}: {detail}", r.function, r.status)
                    .replace("Counterexample", "COUNTEREXAMPLE")
                    .replace("VacuousPrecondition", "VACUOUS_PRECONDITION")
                    .replace("Unknown", "UNKNOWN"),
            ));
        }
    }
    let verification = verification.with_artifacts(options, |error| {
        diagnostics.push(diagnostic(
            error.kind.code(),
            program
                .items
                .iter()
                .find_map(|i| {
                    if let Item::Function(f) = i {
                        Some(f.span)
                    } else {
                        None
                    }
                })
                .unwrap(),
            &format!(
                "UNKNOWN: cannot save verification artifacts: {}",
                error.message
            ),
        ));
    });
    (diagnostics, verification)
}
fn diagnostic(code: ErrorCode, span: Span, message: &str) -> Diagnostic {
    Diagnostic::new(Severity::Error, code, message).with_label(Label::primary(span,"static verification failed")).with_help("check the failed obligation and candidate inputs; UNKNOWN is not a proof; replay saved SMT with the pinned Z3 version")
}

/// Caller must complete type and purity checking first. `prior_error` prevents
/// ill-typed or impure declarations from acquiring a successful proof report.
pub fn verify(program: &Program, options: &Options, prior_error: bool) -> Verification {
    verify_cached(program, options, prior_error, &mut ProofCache::default())
}

pub fn verify_cached(
    program: &Program,
    options: &Options,
    prior_error: bool,
    cache: &mut ProofCache,
) -> Verification {
    let writes = crate::semantic::field_writes::FieldWrites::from_program(program);
    verify_with_data(
        program,
        options,
        prior_error,
        cache,
        &DataTypes::new(program, &writes),
    )
    .with_artifacts(options, |_| {})
}
#[allow(clippy::too_many_lines, reason = "split pending")]
fn verify_with_data(
    program: &Program,
    options: &Options,
    prior_error: bool,
    cache: &mut ProofCache,
    data: &DataTypes,
) -> Verification {
    let functions: Vec<_> = program
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
    let names: HashMap<_, _> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), i))
        .collect();
    let edges: Vec<_> = functions.iter().map(|f| encode::edges(f, &names)).collect();
    let roots: Vec<_> = functions
        .iter()
        .enumerate()
        .filter_map(|(i, f)| has(f, EffectContractKind::Verified).then_some(i))
        .collect();
    // Iterative DFS gives one shared topological ordering and rejects cycles.
    let mut colors = vec![0; functions.len()];
    let mut order = Vec::new();
    let mut cyclic = BTreeSet::new();
    for &root in &roots {
        if colors[root] != 0 {
            continue;
        }
        let mut stack = vec![(root, false)];
        while let Some((id, exit)) = stack.pop() {
            if exit {
                colors[id] = 2;
                order.push(id);
                continue;
            }
            if colors[id] == 2 {
                continue;
            }
            if colors[id] == 1 {
                cyclic.insert(id);
                continue;
            }
            colors[id] = 1;
            stack.push((id, true));
            for &dep in edges[id].iter().rev() {
                if colors[dep] == 1 {
                    cyclic.insert(dep);
                    cyclic.insert(id);
                } else if colors[dep] == 0 {
                    stack.push((dep, false));
                }
            }
        }
    }
    let mut reports = Vec::new();
    let mut semantic_digests = vec![String::new(); functions.len()];
    #[derive(Clone, Copy)]
    enum FunctionState {
        Pending,
        Valid,
        Failed(usize),
    }
    let mut states = vec![FunctionState::Pending; functions.len()];
    let mut failures = Vec::new();
    let mut engine = if !valid_budget(options) {
        Err(FailureKind::Infrastructure.error("invalid verification timeout/resource budget"))
    } else if roots.is_empty() || prior_error {
        Err(FailureKind::Unsupported.error("type or purity checking failed"))
    } else {
        #[cfg(test)]
        SOLVER_STARTS.with(|n| n.set(n.get() + 1));
        solver::Solver::new(&options.solver, options.timeout_ms)
    };
    if let Ok(s) = &mut engine
        && let Err(e) = s.exchange(&format!(
            "(set-option :rlimit {})\n{PRELUDE}",
            options.resource_limit
        ))
    {
        engine = Err(e);
    }
    if let Ok(s) = &mut engine
        && let Err(error) = s.exchange(&data.definitions)
    {
        engine = Err(error);
    }
    for id in order {
        let f = functions[id];
        let verified = has(f, EffectContractKind::Verified);
        let mut r = report(f, options);
        if !data.definitions.is_empty() {
            r.contract_digest = digest(&(r.contract_digest + &data.digest));
        }
        r.dependencies = edges[id]
            .iter()
            .map(|i| functions[*i].name.clone())
            .collect();
        let mut verify_function = || -> VerificationResult<()> {
            let summary = encode(id, &functions, &names, data)?;
            r.vc_nodes = summary.nodes;
            r.dependency_digests = edges[id]
                .iter()
                .map(|i| semantic_digests[*i].clone())
                .collect();
            semantic_digests[id] =
                digest(&(summary.definitions.clone() + &r.dependency_digests.join("\n")));
            if summary.dependencies != edges[id] {
                return Err(FailureKind::Infrastructure.error("unresolved call dependency"));
            }
            let s = engine.as_mut().map_err(|e| e.clone())?;
            s.exchange(&summary.definitions)?;
            if !verified {
                return Ok(());
            }
            #[cfg(test)]
            PROOF_RUNS.with(|n| n.set(n.get() + 1));
            let cache_key = cache::identity(&summary, &r.dependency_digests, f, options);
            r.inferred_loop_annotations = summary.inferred_loop_annotations;
            let cached = cache.get(&cache_key);
            let cache_hit = cached.is_some();
            let mut args = Vec::new();
            let mut domain = vec!["true".to_string()];
            for (i, p) in f.params.iter().enumerate() {
                let name = format!("a{id}_{i}");
                let ty = sort(&p.ty)?;
                s.exchange(&format!("(declare-const {name} {ty})\n"))?;
                domain.push(data::domain(&p.ty, &name));
                r.input_symbols.insert(name.clone(), p.name.clone());
                args.push(name);
            }
            let mut model_symbols = args.clone();
            model_symbols.extend(summary.loop_symbols.keys().cloned());
            r.input_symbols.extend(summary.loop_symbols.clone());
            let domain = format!("(and {})", domain.join(" "));
            let call = |suffix: &str| application(&format!("f{id}_{suffix}"), &args);
            if let Some(obligations) = cached {
                // Probe the live solver before reuse: missing, poisoned or exhausted
                // sessions cannot be turned into a successful old result.
                if s.query("false", &[])?.0 != Answer::Unsat {
                    cache.remove(&cache_key);
                    return Err(
                        FailureKind::Infrastructure.error("solver unavailable for cached proof")
                    );
                }
                r.obligations = obligations;
            } else {
                for (kind, formula, want) in [
                    (
                        "requires safety",
                        format!("(and {domain} (not {}))", call("pre_safe")),
                        Answer::Unsat,
                    ),
                    (
                        "requires satisfiability",
                        format!("(and {domain} {})", call("pre")),
                        Answer::Sat,
                    ),
                    (
                        "body safety and termination",
                        format!(
                            "(and {domain} {} {} (not {}))",
                            call("pre"),
                            call("assume"),
                            call("safe")
                        ),
                        Answer::Unsat,
                    ),
                    (
                        "ensures safety",
                        format!(
                            "(and {domain} {} {} (not {}))",
                            call("pre"),
                            call("assume"),
                            call("post_safe")
                        ),
                        Answer::Unsat,
                    ),
                    (
                        "ensures truth",
                        format!(
                            "(and {domain} {} {} (not {}))",
                            call("pre"),
                            call("assume"),
                            call("post")
                        ),
                        Answer::Unsat,
                    ),
                ] {
                    let (answer, model) = s.query(
                        &formula,
                        if want == Answer::Unsat {
                            &model_symbols
                        } else {
                            &[]
                        },
                    )?;
                    r.obligations.push(Obligation {
                        kind: kind.into(),
                        solver_result: answer.as_str().into(),
                        expected_solver_result: want.as_str().into(),
                        counterexample_candidate: if answer != want { model } else { None },
                    });
                    if answer != want {
                        return Err(match answer {
                            Answer::Unknown => FailureKind::Infrastructure
                                .error("solver returned UNKNOWN (timeout or incomplete theory)"),
                            Answer::Unsat => {
                                FailureKind::VacuousPrecondition.error(format!("failed {kind}"))
                            }
                            Answer::Sat => {
                                FailureKind::Counterexample.error(format!("failed {kind}"))
                            }
                        });
                    }
                }
            }
            // Only proved dependencies get an abstract return function. The
            // encoder instantiates their contracts at individual call sites.
            let sorts = f
                .params
                .iter()
                .map(|p| sort(&p.ty))
                .collect::<Result<Vec<_>, _>>()?
                .join(" ");
            s.exchange(&format!(
                "(declare-fun f{id}_contract ({sorts}) {})\n",
                sort(&f.return_type)?
            ))?;
            r.status = Status::Proven;
            if !cache_hit {
                cache.insert(cache_key, r.obligations.clone());
            }
            Ok(())
        };
        let result = if prior_error {
            Err(FailureKind::Unsupported
                .error("type or purity checking failed")
                .into())
        } else if verified && !has(f, EffectContractKind::Pure) {
            Err(FailureKind::Unsupported
                .error("@verified requires @pure")
                .into())
        } else if cyclic.contains(&id) {
            Err(FailureKind::Unsupported
                .error("recursive call graph outside V1")
                .into())
        } else if let Some(failure) = edges[id].iter().find_map(|&dep| {
            let name = functions[dep].name.as_str();
            match states[dep] {
                FunctionState::Valid => None,
                FunctionState::Failed(cause) => Some(FunctionFailure::Dependency { name, cause }),
                FunctionState::Pending => Some(
                    FailureKind::Unsupported
                        .error(format!(
                            "dependency `{name}` is unverified: recursive dependency"
                        ))
                        .into(),
                ),
            }
        }) {
            Err(failure)
        } else {
            verify_function().map_err(FunctionFailure::Direct)
        };
        match result {
            Ok(()) => states[id] = FunctionState::Valid,
            Err(error) => {
                cache.invalidate();
                if verified {
                    let kind = error.kind();
                    r.failure_kind = Some(kind);
                    r.status = kind.status();
                    if r.status == Status::Unknown {
                        r.unknown_reasons.push(error.message(&failures));
                    }
                }
                states[id] = FunctionState::Failed(failures.len());
                failures.push(error);
            }
        }
        if verified {
            reports.push(r);
        }
    }
    for f in &functions {
        if !has(f, EffectContractKind::Verified)
            && (!f.requires.is_empty() || !f.ensures.is_empty())
        {
            let mut r = report(f, options);
            r.status = Status::RuntimeChecked;
            reports.push(r);
        }
    }
    for r in &mut reports {
        let f = functions[names[r.function.as_str()]];
        let requires: Vec<_> = f.requires.iter().map(contract_text).collect();
        let ensures: Vec<_> = f.ensures.iter().map(contract_text).collect();
        r.purity = serde_json::json!({"status": if has(f,EffectContractKind::Pure) && !prior_error {"PROVEN"} else {"UNKNOWN"}, "assumptions": [], "witnesses": []});
        r.contract = serde_json::json!({"status":r.status,"requires":requires,"ensures":ensures,"verified_scope":r.verified_scope,"model":r.model,"unknown_reasons":r.unknown_reasons,"assumptions":r.dependencies});
        r.specification_provenance = serde_json::json!({"declared_origin":"developer","approval_status":"UNVERIFIED","approval_evidence":null,"contract_digest":r.contract_digest});
    }
    reports.sort_by_key(|r| (r.location.file_id.0, r.location.start));
    let smt = engine
        .as_ref()
        .map(|s| s.transcript.clone())
        .unwrap_or_default();
    Verification {
        reports,
        smt,
        artifact_error: None,
    }
}

fn contract_text(expr: &Expr) -> String {
    enum Part<'a> {
        Expr(&'a Expr),
        Text(String),
    }
    let mut stack = vec![Part::Expr(expr)];
    let mut output = String::new();
    while let Some(part) = stack.pop() {
        match part {
            Part::Text(text) => output.push_str(&text),
            Part::Expr(expr) => match expr {
                Expr::Integer(n, ..) => output.push_str(&n.to_string()),
                Expr::Bool(b, ..) => output.push_str(&b.to_string()),
                Expr::Var(n, ..) => output.push_str(n),
                Expr::Binary(b) => {
                    stack.push(Part::Text(")".into()));
                    stack.push(Part::Expr(&b.rhs));
                    stack.push(Part::Text(format!(" {} ", b.op.symbol())));
                    stack.push(Part::Expr(&b.lhs));
                    stack.push(Part::Text("(".into()));
                }
                Expr::Unary(u) => {
                    stack.push(Part::Text(")".into()));
                    stack.push(Part::Expr(&u.expr));
                    stack.push(Part::Text(
                        if u.op == UnaryOp::Neg { "(-" } else { "(!" }.into(),
                    ));
                }
                Expr::Ternary(t) => {
                    stack.push(Part::Text(")".into()));
                    stack.push(Part::Expr(&t.else_expr));
                    stack.push(Part::Text(" : ".into()));
                    stack.push(Part::Expr(&t.then_expr));
                    stack.push(Part::Text(" ? ".into()));
                    stack.push(Part::Expr(&t.condition));
                    stack.push(Part::Text("(".into()));
                }
                Expr::FieldAccess(object, field, ..) => {
                    stack.push(Part::Text(format!(").{field}")));
                    stack.push(Part::Expr(object));
                    stack.push(Part::Text("(".into()));
                }
                Expr::Index(array, index, ..) => {
                    stack.push(Part::Text("]".into()));
                    stack.push(Part::Expr(index));
                    stack.push(Part::Text("[".into()));
                    stack.push(Part::Expr(array));
                }
                Expr::MethodCall(call) => {
                    stack.push(Part::Text(")".into()));
                    for (i, arg) in call.args.iter().enumerate().rev() {
                        stack.push(Part::Expr(&arg.expr));
                        if i > 0 {
                            stack.push(Part::Text(", ".into()));
                        }
                    }
                    stack.push(Part::Text(format!(").{}(", call.method)));
                    stack.push(Part::Expr(&call.object));
                    stack.push(Part::Text("(".into()));
                }
                Expr::Call(call) => {
                    stack.push(Part::Text(")".into()));
                    for (i, arg) in call.args.iter().enumerate().rev() {
                        stack.push(Part::Expr(&arg.expr));
                        if i > 0 {
                            stack.push(Part::Text(", ".into()));
                        }
                    }
                    stack.push(Part::Text(format!("{}(", call.callee)));
                }
                Expr::Lambda(lambda) if matches!(lambda.body, LambdaBody::Expr(_)) => {
                    let LambdaBody::Expr(body) = &lambda.body else {
                        unreachable!()
                    };
                    stack.push(Part::Expr(body));
                    stack.push(Part::Text(format!(
                        "|{}| ",
                        lambda
                            .params
                            .iter()
                            .map(|p| p.name.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
                Expr::Match(m) if m.arms.iter().all(|a| matches!(a.body, MatchBody::Expr(_))) => {
                    stack.push(Part::Text(" }".into()));
                    for arm in m.arms.iter().rev() {
                        stack.push(Part::Text(", ".into()));
                        let MatchBody::Expr(body) = &arm.body else {
                            unreachable!()
                        };
                        stack.push(Part::Expr(body));
                        stack.push(Part::Text(" => ".into()));
                        for (i, guard) in arm.guard.iter().enumerate().rev() {
                            stack.push(Part::Expr(guard.expr()));
                            if let Guard::Let(step) = guard {
                                stack.push(Part::Text(format!("let {} = ", step.pattern)));
                            }
                            stack.push(Part::Text(if i == 0 { " if " } else { " && " }.into()));
                        }
                        stack.push(Part::Text(arm.pattern.to_string()));
                    }
                    stack.push(Part::Text(" { ".into()));
                    stack.push(Part::Expr(&m.scrutinee));
                    stack.push(Part::Text("match ".into()));
                }
                _ => output.push_str(&comparison::canonical(expr).to_string()),
            },
        }
    }
    output
}

#[cfg(test)]
mod loop_tests;

#[cfg(test)]
mod data_tests;

#[cfg(test)]
mod inferred_loop_tests;
