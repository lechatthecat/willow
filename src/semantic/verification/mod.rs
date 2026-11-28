//! Fail-closed static verification over checked scalars and immutable data.
//! Mathematical integers model values; every finite-width operation has a
//! separate safety obligation. Summaries share helper bodies across call sites.
mod cache;
pub(crate) mod modular;
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
mod presentation;
mod solver;
pub use presentation::{DisplayInput, Presentation};
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
impl Status {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Proven => "PROVEN",
            Self::Counterexample => "COUNTEREXAMPLE",
            Self::VacuousPrecondition => "VACUOUS_PRECONDITION",
            Self::Unknown => "UNKNOWN",
            Self::RuntimeChecked => "RUNTIME_CHECKED",
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct Obligation {
    pub kind: String,
    pub solver_result: String,
    pub expected_solver_result: String,
    pub counterexample_candidate: Option<String>,
    pub presentation: Option<Presentation>,
}
#[derive(Debug, Clone, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
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
            timeout_ms: willow_abi::settings::verification_timeout_ms(),
            resource_limit: willow_abi::settings::verification_resource_limit(),
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
        verified_scope: "checked scalars, byte strings, immutable data, bounded quantifiers and annotated loops",
        analyzer_version: "5",
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
    check_with_data(
        program,
        previous,
        options,
        cache,
        &DataTypes {
            calls: Some(calls),
            ..DataTypes::new(program, writes)
        },
        None,
    )
}
fn check_with_data(
    program: &Program,
    previous: &[Diagnostic],
    options: &Options,
    cache: &mut ProofCache,
    data: &DataTypes,
    world: Option<&mut modular::World>,
) -> (Vec<Diagnostic>, Verification) {
    let mut diagnostics = Vec::new();
    let prior_error = previous.iter().any(|d| d.severity == Severity::Error);
    // Methods are outside V1, including interface declarations without bodies.
    for item in &program.items {
        let attrs: Vec<_> = match item {
            Item::Class(c) => c.methods.iter().flat_map(|m| &m.effect_contracts).collect(),
            Item::Interface(i) => i.methods.iter().flat_map(|m| &m.effect_contracts).collect(),
            _ => Vec::new(),
        };
        for a in attrs
            .into_iter()
            .filter(|a| !prior_error && a.kind == EffectContractKind::Verified)
        {
            diagnostics.push(diagnostic(
                FailureKind::Unsupported.code(),
                a.span,
                "UNKNOWN: @verified V1 requires a top-level scalar function",
            ));
        }
    }
    if world.is_none()
        && !program
            .items
            .iter()
            .any(|i| matches!(i, Item::Function(f) if has(f,EffectContractKind::Verified)))
    {
        return (diagnostics, Verification::default());
    }
    let verification = verify_with_data(program, options, prior_error, cache, data, world);
    for r in &verification.reports {
        if let Some(failure) = r.failure_kind
            && !prior_error
        {
            let detail = r.unknown_reasons.first().cloned().unwrap_or_else(|| {
                r.obligations
                    .last()
                    .map(|o| {
                        format!(
                            "{}: {}{}",
                            o.kind,
                            o.solver_result,
                            o.presentation
                                .as_ref()
                                .filter(|m| m.evidence == "CANDIDATE")
                                .map(|m| format!("; candidate inputs {}", m.text()))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_default()
            });
            let mut error = diagnostic(
                failure.code(),
                r.obligations
                    .last()
                    .and_then(|o| o.presentation.as_ref())
                    .and_then(|p| p.location)
                    .unwrap_or(r.location),
                &format!(
                    "@verified `{}`: {}: {detail}",
                    r.function,
                    r.status.as_str()
                ),
            );
            if let Some(kind) = r
                .obligations
                .last()
                .and_then(|o| o.presentation.as_ref())
                .and_then(|p| p.obligation.as_ref())
            {
                error.labels[0].message = format!("failed {kind}");
            }
            diagnostics.push(error);
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
                .unwrap_or_else(Span::dummy),
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
        None,
    )
    .with_artifacts(options, |_| {})
}
#[expect(
    clippy::too_many_lines,
    reason = "topological proof scheduling keeps failure provenance, imported declarations and cache admission in one state transition"
)]
fn verify_with_data(
    program: &Program,
    options: &Options,
    prior_error: bool,
    cache: &mut ProofCache,
    data: &DataTypes,
    mut world: Option<&mut modular::World>,
) -> Verification {
    let imported = world
        .as_ref()
        .map(|w| w.imports(program, data.targets))
        .unwrap_or_default();
    let imported_len = imported.len();
    let functions: Vec<_> = imported
        .iter()
        .map(|e| &e.function)
        .chain(program.items.iter().filter_map(|i| {
            if let Item::Function(f) = i {
                Some(f)
            } else {
                None
            }
        }))
        .collect();
    let mut names: HashMap<_, _> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), i))
        .collect();
    // Source-local constants and direct calls retain their lexical spelling;
    // imported stubs never contribute unqualified aliases.
    if world.is_some() {
        for (i, f) in functions.iter().enumerate().skip(imported_len) {
            if let Some((_, local)) = f.name.rsplit_once("::") {
                names.insert(local, i);
            }
        }
    }
    let edges: Vec<_> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| {
            if i < imported_len {
                BTreeSet::new()
            } else {
                encode::resolved_edges(f, &names, data.targets)
            }
        })
        .collect();
    let roots: Vec<_> = functions
        .iter()
        .enumerate()
        .filter_map(|(i, f)| {
            (i >= imported_len
                && (has(f, EffectContractKind::Verified)
                    || (world.is_some() && has(f, EffectContractKind::Pure))))
            .then_some(i)
        })
        .collect();
    // Iterative DFS gives one shared topological ordering and rejects cycles.
    let mut colors = vec![0; functions.len()];
    colors[..imported_len].fill(2);
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
    } else if !roots
        .iter()
        .any(|i| has(functions[*i], EffectContractKind::Verified))
        || prior_error
    {
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
    for (id, export) in imported.iter().enumerate() {
        semantic_digests[id] = export.digest.clone();
        match &export.error {
            Some(error) => {
                states[id] = FunctionState::Failed(failures.len());
                failures.push(FunctionFailure::Direct(error.clone()));
            }
            None => {
                if let Ok(s) = &mut engine
                    && let Err(e) = s.exchange(&export.definitions)
                {
                    engine = Err(e);
                }
                states[id] = FunctionState::Valid;
            }
        }
    }
    let proof = ProofContext {
        functions: &functions,
        names: &names,
        data,
        options,
        edges: &edges,
    };
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
        // Reuse this canonical ordering for reports, cache keys and exports.
        r.dependencies.sort_unstable();
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
            proof
                .verify_function(
                    id,
                    &mut r,
                    &mut engine,
                    cache,
                    &mut semantic_digests,
                    world.as_deref_mut(),
                )
                .map_err(FunctionFailure::Direct)
        };
        match result {
            Ok(()) => states[id] = FunctionState::Valid,
            Err(error) => {
                if let Some(world) = world.as_deref_mut() {
                    world.fail(f, error.message(&failures));
                }
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
    for f in &functions[imported_len..] {
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

/// Shared immutable inputs for each proof in the topological schedule.
struct ProofContext<'a> {
    functions: &'a [&'a FunctionDecl],
    names: &'a HashMap<&'a str, usize>,
    data: &'a DataTypes<'a>,
    options: &'a Options,
    edges: &'a [BTreeSet<usize>],
}

impl ProofContext<'_> {
    fn verify_function(
        &self,
        id: usize,
        r: &mut Report,
        engine: &mut VerificationResult<solver::Solver>,
        cache: &mut ProofCache,
        semantic_digests: &mut [String],
        world: Option<&mut modular::World>,
    ) -> VerificationResult<()> {
        let Self {
            functions,
            names,
            data,
            options,
            edges,
        } = *self;
        let f = functions[id];
        let verified = has(f, EffectContractKind::Verified);
        let summary = encode(id, functions, names, data)?;
        r.vc_nodes = summary.nodes;
        r.dependency_digests = r
            .dependencies
            .iter()
            .map(|name| semantic_digests[names[name.as_str()]].clone())
            .collect();
        let exported_edges = if verified {
            &summary.contract_dependencies
        } else {
            &summary.dependencies
        };
        let exported_dependencies: Vec<_> = r
            .dependencies
            .iter()
            .filter(|name| exported_edges.contains(&names[name.as_str()]))
            .map(|name| {
                (
                    name.as_str(),
                    semantic_digests[names[name.as_str()]].as_str(),
                )
            })
            .collect();
        let exported_digest = digest(&serde_json::json!({
            "definitions": if verified { &summary.contract_definitions } else { &summary.definitions },
            "dependencies": exported_dependencies,
        }).to_string());
        let export = modular::Export::new(
            f,
            &summary,
            exported_digest.clone(),
            exported_dependencies
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect(),
        );
        semantic_digests[id] = exported_digest;
        if summary.dependencies != edges[id] {
            return Err(FailureKind::Infrastructure.error("unresolved call dependency"));
        }
        if !verified {
            if let Ok(s) = engine {
                s.exchange(&summary.definitions)?;
            }
            if let Some(world) = world {
                world.insert(export);
            }
            return Ok(());
        }
        let s = engine.as_mut().map_err(|e| e.clone())?;
        s.exchange(&summary.definitions)?;
        let symbol = symbol(f);
        let cache_key =
            cache::identity(&summary, &r.dependencies, &r.dependency_digests, f, options);
        r.inferred_loop_annotations = summary.inferred_loop_annotations;
        let cached = cache.get(&f.name, &cache_key);
        let cache_hit = cached.is_some();
        let mut args = Vec::new();
        let mut domain = vec!["true".to_string()];
        for (i, p) in f.params.iter().enumerate() {
            let name = format!("a{symbol}_{i}");
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
        let call = |suffix: &str| application(&format!("f{symbol}_{suffix}"), &args);
        if let Some(obligations) = cached {
            // Probe the live solver before reuse: missing, poisoned or exhausted
            // sessions cannot be turned into a successful old result.
            if s.query("false", &[])?.0 != Answer::Unsat {
                cache.remove(&f.name);
                return Err(
                    FailureKind::Infrastructure.error("solver unavailable for cached proof")
                );
            }
            r.obligations = obligations;
        } else {
            #[cfg(test)]
            PROOF_RUNS.with(|n| n.set(n.get() + 1));
            for (kind, formula, want) in [
                (
                    "requires safety",
                    format!(
                        "(and {domain} {} (not {}))",
                        call("pre_assume"),
                        call("pre_safe")
                    ),
                    Answer::Unsat,
                ),
                (
                    "requires satisfiability",
                    format!("(and {domain} {} {})", call("pre_assume"), call("pre")),
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
                let phase = match kind {
                    "requires safety" => "pre_safe",
                    "body safety and termination" => "safe",
                    "ensures safety" => "post_safe",
                    _ => "post",
                };
                let (answer, model, presentation) = s.query_detailed(
                    &formula,
                    if want == Answer::Unsat {
                        &model_symbols
                    } else {
                        &[]
                    },
                    |solver| {
                        Ok((want == Answer::Unsat).then(|| {
                            presentation::capture(solver, data, f, &args, &summary.sites, phase)
                        }))
                    },
                )?;
                r.obligations.push(Obligation {
                    kind: kind.into(),
                    solver_result: answer.as_str().into(),
                    expected_solver_result: want.as_str().into(),
                    counterexample_candidate: if answer != want { model } else { None },
                    presentation: if answer == Answer::Unsat && want == Answer::Sat {
                        Some(Presentation::vacuous(summary.precondition_location))
                    } else {
                        presentation
                    },
                });
                if answer != want {
                    return Err(match answer {
                        Answer::Unknown => FailureKind::Infrastructure
                            .error("solver returned UNKNOWN (timeout or incomplete theory)"),
                        Answer::Unsat => {
                            FailureKind::VacuousPrecondition.error(format!("failed {kind}"))
                        }
                        Answer::Sat => FailureKind::Counterexample.error(format!("failed {kind}")),
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
            "(declare-fun f{symbol}_contract ({sorts}) {})\n",
            sort(&f.return_type)?
        ))?;
        if let Some(world) = world {
            world.insert(export);
        }
        r.status = Status::Proven;
        if !cache_hit {
            cache.insert(f.name.clone(), cache_key, r.obligations.clone());
        }
        Ok(())
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

#[cfg(test)]
mod string_tests;

#[cfg(test)]
mod diagnostic_tests;
