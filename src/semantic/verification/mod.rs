//! Fail-closed static verification over checked scalars and immutable data.
//! Mathematical integers model values; every finite-width operation has a
//! separate safety obligation. Summaries share helper bodies across call sites.
mod cache;
pub mod comparison;
mod data;
mod encode;
pub use cache::ProofCache;
use data::*;
mod solver;
#[cfg(test)]
mod tests;
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use encode::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

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
pub struct Report {
    #[serde(skip)]
    diagnostic_code: ErrorCode,
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
}
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
pub struct Verification {
    pub reports: Vec<Report>,
    pub smt: String,
    pub artifact_error: Option<String>,
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
        diagnostic_code: ErrorCode::E2631,
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
        analyzer_version: "3",
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
    }
}

pub(crate) fn check(
    program: &Program,
    previous: &[Diagnostic],
    writes: &crate::semantic::field_writes::FieldWrites,
) -> Vec<Diagnostic> {
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
                ErrorCode::E2631,
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
        return diagnostics;
    }
    let prior_error = previous.iter().any(|d| d.severity == Severity::Error);
    let verification = verify_with_data(
        program,
        &Options::default(),
        prior_error,
        &mut ProofCache::default(),
        &DataTypes::new(program, writes),
    );
    for r in &verification.reports {
        if r.status != Status::Proven && r.status != Status::RuntimeChecked {
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
                r.diagnostic_code,
                r.location,
                &format!("@verified `{}`: {:?}: {detail}", r.function, r.status)
                    .replace("Counterexample", "COUNTEREXAMPLE")
                    .replace("VacuousPrecondition", "VACUOUS_PRECONDITION")
                    .replace("Unknown", "UNKNOWN"),
            ));
        }
    }
    if let Some(error) = verification.artifact_error {
        diagnostics.push(diagnostic(
            ErrorCode::E2632,
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
            &format!("UNKNOWN: cannot save verification artifacts: {error}"),
        ));
    }
    diagnostics
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
}
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
    let mut valid = vec![false; functions.len()];
    let mut failures: Vec<Option<String>> = vec![None; functions.len()];
    let mut engine = if !valid_budget(options) {
        Err("invalid verification timeout/resource budget".into())
    } else if roots.is_empty() || prior_error {
        Err("type or purity checking failed".into())
    } else {
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
        let result: Result<(), String> = (|| {
            if prior_error {
                return Err("type or purity checking failed".into());
            }
            if verified && !has(f, EffectContractKind::Pure) {
                return Err("@verified requires @pure".into());
            }
            if cyclic.contains(&id) {
                return Err("recursive call graph outside V1".into());
            }
            for &dep in &edges[id] {
                if !valid[dep] {
                    return Err(format!(
                        "dependency `{}` is unverified: {}",
                        functions[dep].name,
                        failures[dep].as_deref().unwrap_or("recursive dependency")
                    ));
                }
            }
            let summary = encode(id, &functions, &names, data)?;
            r.vc_nodes = summary.nodes;
            r.dependency_digests = edges[id]
                .iter()
                .map(|i| semantic_digests[*i].clone())
                .collect();
            semantic_digests[id] =
                digest(&(summary.definitions.clone() + &r.dependency_digests.join("\n")));
            r.diagnostic_code = ErrorCode::E2632;
            if summary.dependencies != edges[id] {
                return Err("unresolved call dependency".into());
            }
            let s = engine.as_mut().map_err(|e| e.clone())?;
            s.exchange(&summary.definitions)?;
            if !verified {
                return Ok(());
            }
            let cache_key = cache::identity(&summary, &r.dependency_digests, f, options);
            let cached = cache.get(&cache_key);
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
                if s.query("false", &[])?.0 != "unsat" {
                    cache.remove(&cache_key);
                    return Err("solver unavailable for cached proof".into());
                }
                r.obligations = obligations;
            } else {
                for (kind, formula, want) in [
                    (
                        "requires safety",
                        format!("(and {domain} (not {}))", call("pre_safe")),
                        "unsat",
                    ),
                    (
                        "requires satisfiability",
                        format!("(and {domain} {})", call("pre")),
                        "sat",
                    ),
                    (
                        "body safety and termination",
                        format!(
                            "(and {domain} {} {} (not {}))",
                            call("pre"),
                            call("assume"),
                            call("safe")
                        ),
                        "unsat",
                    ),
                    (
                        "ensures safety",
                        format!(
                            "(and {domain} {} {} (not {}))",
                            call("pre"),
                            call("assume"),
                            call("post_safe")
                        ),
                        "unsat",
                    ),
                    (
                        "ensures truth",
                        format!(
                            "(and {domain} {} {} (not {}))",
                            call("pre"),
                            call("assume"),
                            call("post")
                        ),
                        "unsat",
                    ),
                ] {
                    let (answer, model) =
                        s.query(&formula, if want == "unsat" { &model_symbols } else { &[] })?;
                    r.obligations.push(Obligation {
                        kind: kind.into(),
                        solver_result: answer.clone(),
                        expected_solver_result: want.into(),
                        counterexample_candidate: if answer != want { model } else { None },
                    });
                    if answer != want {
                        r.status = if answer == "unknown" {
                            Status::Unknown
                        } else if want == "sat" {
                            Status::VacuousPrecondition
                        } else {
                            Status::Counterexample
                        };
                        if answer != "unknown" {
                            r.diagnostic_code = ErrorCode::E2630;
                        }
                        return Err(if answer == "unknown" {
                            "solver returned UNKNOWN (timeout or incomplete theory)".into()
                        } else {
                            format!("failed {kind}")
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
            cache.insert(cache_key, r.obligations.clone());
            Ok(())
        })();
        match result {
            Ok(()) => valid[id] = true,
            Err(error) => {
                cache.invalidate();
                failures[id] = Some(error.clone());
                if r.status == Status::Unknown {
                    r.unknown_reasons.push(error);
                }
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
    let mut result = Verification {
        reports,
        smt,
        artifact_error: None,
    };
    if let Some(dir) = &options.artifacts {
        let save = (|| -> Result<(), Box<dyn std::error::Error>> {
            let json = serde_json::to_string_pretty(&result.reports)?;
            let key = digest(&(result.smt.clone() + &json));
            let dir = dir.join(&key[7..]);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("verification.smt2"), &result.smt)?;
            std::fs::write(dir.join("verification.json"), json)?;
            Ok(())
        })();
        if let Err(error) = save {
            result.artifact_error = Some(error.to_string());
        }
    }
    result
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
                        if let Some(guard) = &arm.guard {
                            stack.push(Part::Expr(guard));
                            stack.push(Part::Text(" if ".into()));
                        }
                        let pattern = match &arm.pattern {
                            Pattern::Wildcard(..) => "_".into(),
                            Pattern::Binding { name, .. } => name.clone(),
                            Pattern::LiteralBool(b, ..) => b.to_string(),
                            Pattern::LiteralInt(n, ..) => n.to_string(),
                            Pattern::EnumVariant {
                                enum_name, variant, ..
                            } => format!("{enum_name}::{variant}"),
                            Pattern::EnumVariantTuple {
                                enum_name,
                                variant,
                                bindings,
                                ..
                            } => format!("{enum_name}::{variant}({})", bindings.join(", ")),
                            _ => "<unsupported pattern>".into(),
                        };
                        stack.push(Part::Text(pattern));
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
