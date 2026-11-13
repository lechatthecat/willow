//! Opt-in, compiler-owned impact facts. No backend or runtime dependency.
mod diff;
pub mod direct;
mod dispatch;
mod display;
pub mod edit;
mod effect_states;
pub(crate) mod generics;
mod graph;
mod indirect;
mod interfaces;
pub mod interop;
pub mod overview;
mod packages;
mod rename;
pub mod verify;
pub use packages::{ModuleEvidence, ModuleIdentity, SymbolIdentity, UpdateDelta};
mod incremental_dispatch;
mod incremental_indirect;
pub(crate) mod incremental_snapshot;
mod semantic;
mod storage;
pub(crate) use incremental_snapshot::{
    VerificationRelocation, VerificationSnapshotColdFallback, VerificationSnapshotFallback,
    VerificationSnapshotState, VerificationSnapshotUpdate, capture_incremental,
    incremental_snapshot,
};
pub(crate) mod symbols;
pub use graph::{Direction, Impact, ImpactNode, Limits};
pub use semantic::{QueryRequest, QuerySession, SemanticFacts};
pub(crate) use storage::check_size;
pub use storage::{Difference, FunctionChange, compact_output_paths, expand_output_paths};

use crate::qualified_name::QualifiedName;
use crate::{
    diagnostics::Span,
    module::UnitId,
    parser::{
        ast::*,
        iter::{AstEvent, AstWalk},
    },
    semantic::{
        call_graph::CallGraph,
        effects::RuntimeEffects,
        ids::{FunctionId, TypeId},
        intrinsics::Intrinsic,
    },
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
};

/// Compiler-owned source is queryable, but never a filesystem edit target.
pub(crate) fn bundled_source(path: &str) -> Option<&'static str> {
    if Path::new(path) == Path::new("<std>/collections.wi") {
        Some(include_str!("../../std/collections.wi"))
    } else if Path::new(path) == Path::new("<std>/json.wi") {
        Some(include_str!("../../std/json.wi"))
    } else {
        None
    }
}
pub(crate) fn source_path(path: &Path) -> Result<std::path::PathBuf> {
    if path
        .to_str()
        .is_some_and(|path| bundled_source(path).is_some())
    {
        Ok(path.to_owned())
    } else {
        Ok(std::fs::canonicalize(path)?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub path: String,
    pub start: usize,
    pub end: usize,
}

/// The key is meaningful only with this snapshot's revision. Matching keys across
/// compatible revisions is a comparison operation, never reference resolution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Function {
    #[serde(default)]
    pub identity: Option<SymbolIdentity>,
    #[serde(skip)]
    pub(crate) body_id: Option<BodyId>,
    #[serde(skip)]
    pub(crate) body_location: Option<Location>,
    #[serde(skip)]
    pub(crate) rename_calls: Vec<Location>,
    /// Import declarations naming this function as an item (`import m::{f}`).
    #[serde(skip)]
    pub(crate) rename_imports: Vec<Location>,
    pub id: String,
    pub module: String,
    pub name: String,
    pub locations: Vec<Location>,
    pub synthetic: bool,
    pub fingerprint: String,
    pub body_fingerprint: String,
    pub callees: Vec<String>,
    pub runtime_effects: u8,
    pub unknown: bool,
    pub unresolved: Vec<String>,
}

impl Function {
    /// Explanation uses existing graph facts, without copying transitive cause
    /// sets into every caller (which would grow quadratically on long chains).
    pub(crate) fn unknown_reason(&self) -> Option<&'static str> {
        self.unknown.then_some(if !self.unresolved.is_empty() {
            "Indirect or unresolved call targets have no complete effect summary; all runtime capabilities are conservatively retained."
        } else if self.synthetic {
            "Dynamic dispatch has no complete implementation summary; all runtime capabilities are conservatively retained."
        } else {
            "A reachable callee or dynamic dispatch candidate has an incomplete effect summary; its conservative runtime capabilities are retained."
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    #[serde(skip)]
    pub(crate) edit_context: Option<edit::AnalysisInputs>,
    pub version: u32,
    pub compiler: String,
    pub compatibility: String,
    pub workspace: String,
    pub revision: String,
    pub sources: BTreeMap<String, String>,
    pub functions: Vec<Function>,
    pub semantic: SemanticFacts,
}

pub(crate) fn compiler_identity() -> String {
    storage::compiler_stamp()
}

pub(crate) fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", crate::hex::Hex(&Sha256::digest(bytes.as_ref())))
}

pub(crate) fn hash_serialized(value: &impl Serialize) -> Result<String> {
    struct HashWriter(Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(format!("{:x}", crate::hex::Hex(&writer.0.finalize())))
}

pub(crate) struct CapturedUnit {
    graph: CallGraph,
    indirect: indirect::Inputs,
    semantic: semantic::Captured,
    symbols: crate::semantic::analysis_symbols::Facts,
    symbol_owners: Vec<(Span, String)>,
    symbol_details: HashMap<Span, symbols::DeclarationDetails>,
    declarations: HashMap<FunctionId, Vec<(Span, Span)>>,
    identities: HashMap<FunctionId, BodyId>,
    lambda_names: HashMap<FunctionId, String>,
    rename_calls: Vec<(Span, FunctionId)>,
    direct: HashMap<FunctionId, u8>,
    own_effects: HashMap<FunctionId, u8>,
    dispatches: HashMap<FunctionId, BTreeSet<FunctionId>>,
    dispatch_sites: HashMap<Span, FunctionId>,
    interfaces: interfaces::Captured,
    /// Never-instantiated generic templates, which no execution can enter.
    templates: std::collections::HashSet<FunctionId>,
}

pub(crate) struct CaptureInputs<'a> {
    pub indirect: &'a crate::semantic::indirect_lock::IndirectInputs,
    pub types: &'a HashMap<ExprId, Type>,
    pub calls: &'a HashMap<ExprId, Option<FunctionId>>,
    pub patterns: &'a HashMap<PatternId, Pattern>,
    pub symbols: &'a crate::semantic::analysis_symbols::Facts,
}

pub(crate) fn capture(
    program: &Program,
    graph: &CallGraph,
    inputs: CaptureInputs<'_>,
    facts: &crate::semantic::effects::EffectFacts<crate::compiler_db::effects::EffectWitness>,
    symbols: &crate::semantic::symbols::SymbolTable,
    index: Option<&crate::compiler_db::ids::BodyIndex>,
) -> CapturedUnit {
    let CaptureInputs {
        indirect,
        types,
        calls,
        patterns,
        symbols: symbol_facts,
    } = inputs;
    // Bodiless interface contracts (e.g. prelude `Into::into`) are callable
    // signatures, not functions. Calls through them stay unresolved/unknown.
    let mut unit_graph = CallGraph::default();
    for (id, sites) in graph.iter() {
        let contract = id.namespace().is_none()
            && !id.name().starts_with("$default$")
            && id
                .owner()
                .is_some_and(|owner| symbols.lookup_interface(owner.as_ref()).is_some())
            && sites.targets.is_empty()
            && !sites.has_unknown;
        if !contract {
            unit_graph.merge(*id, sites.clone());
        }
    }
    let mut result = CapturedUnit {
        graph: unit_graph,
        indirect: indirect::Inputs {
            values: indirect
                .values
                .iter()
                .map(|value| (value.target, value.bit, value.key.clone()))
                .collect(),
            calls: indirect.calls.clone(),
        },
        semantic: semantic::Captured::default(),
        symbols: symbol_facts.clone(),
        symbol_owners: symbols::owners(program),
        symbol_details: symbols::details(program, symbols),
        declarations: HashMap::new(),
        identities: HashMap::new(),
        lambda_names: HashMap::new(),
        rename_calls: Vec::new(),
        direct: HashMap::new(),
        own_effects: HashMap::new(),
        dispatches: HashMap::new(),
        dispatch_sites: HashMap::new(),
        interfaces: Default::default(),
        templates: Default::default(),
    };
    result.interfaces.targets(symbols, &result.graph);
    symbols::declarations(program, &mut result.symbols, symbols);
    result.semantic.rename_relations = rename::relations(program, symbols);
    let mut pending = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) => pending.push((
                FunctionId::free(&f.name),
                f.span,
                f.body.id,
                AstEvent::Block(&f.body),
                f.is_async,
            )),
            Item::Class(c) => {
                for field in &c.fields {
                    if let Some(expr) = &field.initializer
                        && let Some(body) = index.and_then(|i| i.initializer_body(expr.id()))
                    {
                        pending.push((
                            FunctionId::method(
                                TypeId::local(&c.name),
                                format!("$static${}", field.name),
                            ),
                            field.span,
                            body,
                            AstEvent::Expr(expr),
                            false,
                        ));
                    }
                }
                for m in &c.methods {
                    // Injected copies retain the original file's source span.
                    pending.push((
                        FunctionId::method(TypeId::local(&c.name), &m.name),
                        m.span,
                        m.body.id,
                        AstEvent::Block(&m.body),
                        m.is_async,
                    ));
                }
                for init in &c.constructors {
                    pending.push((
                        FunctionId::method(TypeId::local(&c.name), "init"),
                        init.span,
                        init.body.id,
                        AstEvent::Block(&init.body),
                        false,
                    ));
                }
            }
            Item::Interface(i) => {
                for m in &i.methods {
                    if let Some(body) = &m.default_body {
                        pending.push((
                            FunctionId::method(
                                TypeId::local(&i.name),
                                format!("$default${}", m.name),
                            ),
                            m.span,
                            body.id,
                            AstEvent::Block(body),
                            false,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    let builtin_aliases = crate::compiler_db::normalize::builtin_module_aliases(program);
    let mut lambda_roots = HashMap::new();
    while let Some((id, span, body_id, body, is_async)) = pending.pop() {
        result.identities.insert(id, body_id);
        let body_span = match body {
            AstEvent::Block(b) => b.span,
            AstEvent::Expr(_) => span,
            _ => unreachable!(),
        };
        result
            .declarations
            .entry(id)
            .or_default()
            .push((span, body_span));
        // Existing compiler facts own panic/lock semantics. Extra potential
        // allocation/preemption facts are conservative and never prove absence
        // for operations whose runtime lowering is not represented here.
        result.semantic.body(
            id,
            body,
            &semantic::CaptureContext {
                types,
                calls,
                patterns,
            },
        );
        if is_async {
            result.semantic.async_functions.insert(id);
        }
        if let Some(summary) = facts.get(&id) {
            result.semantic.compiler_effects.insert(
                id,
                summary.effects().intersection(RuntimeEffects::ALL).bits(),
            );
            let witnesses = (0..RuntimeEffects::BIT_COUNT).filter_map(|bit| {
                let effect = RuntimeEffects::from_bit(bit);
                if !summary.contains(effect) { return None; }
                let witness = match summary.witness(effect) {
                    Some(crate::compiler_db::effects::EffectWitness::Lock(w)) => serde_json::json!({"kind":"wait", "owner_key":w.owner, "cause":w.cause}),
                    Some(crate::compiler_db::effects::EffectWitness::Panic {owner, source}) => serde_json::json!({"kind":"panic", "owner_key":owner, "cause":{"span":Span::in_file(crate::diagnostics::FileId(source.0),source.1,source.2,0,0), "operation":"panic-guard"}}),
                    Some(crate::compiler_db::effects::EffectWitness::Allocation {owner, source}) => serde_json::json!({"kind":"allocation", "owner_key":owner, "cause":{"span":Span::in_file(crate::diagnostics::FileId(source.0),source.1,source.2,0,0), "operation":"allocation"}}),
                    Some(crate::compiler_db::effects::EffectWitness::External(target)) => serde_json::json!({"kind":"external-boundary","target":target.to_string(),"target_key":target}),
                    Some(crate::compiler_db::effects::EffectWitness::Helper { reason, owner, source }) => serde_json::json!({"kind":"nonpreemptible", "owner_key":owner, "reason":format!("{reason:?}: synchronous helper may run in a nonpreemptible region; loop safepoints are separate potential preemption points, subject to runtime/platform gating"), "cause":{"span":Span::in_file(crate::diagnostics::FileId(source.0),source.1,source.2,0,0),"operation":match reason { crate::semantic::concurrency::NonpreemptibleReason::Loop => "synchronous-loop", crate::semantic::concurrency::NonpreemptibleReason::Recursion => "synchronous-recursion" }}}),
                    None => serde_json::json!({"kind":"unavailable"}),
                };
                // Fixed-point witnesses may belong to a callee. Capture only
                // this body's facts; query traversal explains transitive ones.
                if witness.get("owner_key").is_some_and(|owner| *owner != serde_json::json!(id)) {
                    return None;
                }
                Some(serde_json::json!({"effect":effect.bits(), "witness":witness}))
            }).collect();
            result.semantic.witnesses.insert(id, witnesses);
        }
        let compiler_bits = facts.get(&id).map_or_else(
            || {
                if matches!(body, AstEvent::Expr(_)) {
                    RuntimeEffects::ALL.bits()
                } else {
                    0
                }
            },
            |f| f.effects().intersection(RuntimeEffects::ALL).bits(),
        );
        let mut bits = 0;
        if let Some(sites) = graph.get(&id) {
            for &span in sites.indirect_spans.values() {
                result.semantic.capability(
                    id,
                    RuntimeEffects::MAY_PANIC.bits(),
                    span,
                    "indirect-call",
                );
            }
        }
        let mut initializer_calls = crate::semantic::call_graph::CallSites::default();
        let initializer = id.name().starts_with("$static$");
        let mut walk = AstWalk::new(body);
        while let Some(event) = walk.next() {
            let capability = match event {
                AstEvent::Lambda(l) => Some((
                    RuntimeEffects::MAY_ALLOCATE.bits(),
                    l.span,
                    "lambda-environment",
                )),
                AstEvent::Stmt(s @ (Stmt::While(_) | Stmt::For(_))) => Some((
                    RuntimeEffects::MAY_PREEMPT.bits(),
                    s.span(),
                    "loop-safepoint",
                )),
                AstEvent::Expr(e) => {
                    if let Some(target) = calls.get(&e.id()) {
                        let operation = match e {
                            Expr::Call(c) => Some(c.callee.clone()),
                            Expr::MethodCall(c) => Some(c.method.clone()),
                            Expr::StaticCall(c) => Some(QualifiedName::join(&c.class, &c.method)),
                            _ => None,
                        };
                        let builtin = target.as_ref().is_some_and(|target| {
                            crate::semantic::intrinsics::builtin_target_effects(target)
                                .or_else(|| {
                                    crate::semantic::intrinsics::builtin_static_effects(
                                        target,
                                        &builtin_aliases,
                                    )
                                })
                                .is_some()
                        });
                        if builtin && let Some(target) = target {
                            result.semantic.builtin_calls.push((id, e.span(), *target));
                        }
                        if !builtin && let Some(operation) = operation {
                            result
                                .semantic
                                .call_sites
                                .push((id, e.span(), *target, operation));
                        }
                    }
                    if let Expr::MethodCall(call) = e
                        && let Some(intrinsic) = calls
                            .get(&e.id())
                            .and_then(|t| t.as_ref())
                            .and_then(Intrinsic::from_function_id)
                        && matches!(
                            intrinsic,
                            Intrinsic::StringLen
                                | Intrinsic::StringByteAt
                                | Intrinsic::StringContains
                                | Intrinsic::StringFind
                                | Intrinsic::StringStartsWith
                                | Intrinsic::StringEndsWith
                        )
                    {
                        // Presentation only: never narrow the ABI/call-graph mask.
                        result.semantic.builtin_effect_paths(
                            id,
                            e.span(),
                            &call.method,
                            intrinsic == Intrinsic::StringByteAt,
                        );
                    }
                    if matches!(e, Expr::String(..)) {
                        // Literal slots can allocate on first use, but a static
                        // literal is not evidence of a per-evaluation allocation.
                        // Keep the bound without displacing actual allocation sites.
                        bits |= RuntimeEffects::MAY_ALLOCATE.bits();
                    }
                    // A resolved enum variant has no callable body. Keep its
                    // allocation bound tied to the construction site rather
                    // than exposing a propagated bit without any witness.
                    if matches!(e, Expr::StaticCall(_))
                        && calls
                            .get(&e.id())
                            .and_then(|target| *target)
                            .and_then(|target| target.owner_type())
                            .is_some_and(|owner| symbols.lookup_enum(&owner.to_string()).is_some())
                    {
                        bits |= RuntimeEffects::MAY_ALLOCATE.bits();
                        result.semantic.capability(
                            id,
                            RuntimeEffects::MAY_ALLOCATE.bits(),
                            e.span(),
                            "enum-allocation",
                        );
                    }
                    let effects = match e {
                        Expr::New(_) | Expr::ArrayLiteral(..) | Expr::ObjectLiteral(_) => {
                            RuntimeEffects::MAY_ALLOCATE.bits()
                        }
                        // Async select parks via SuspendOp::SelectWait. Its case
                        // expressions/bodies are visited independently below.
                        Expr::Select(_) if is_async => RuntimeEffects::MAY_ALLOCATE
                            .union(RuntimeEffects::MAY_SUSPEND)
                            .union(RuntimeEffects::MAY_PREEMPT)
                            .union(RuntimeEffects::MAY_PANIC)
                            .bits(),
                        Expr::Binary(b)
                            if b.op == BinOp::Add
                                && matches!(types.get(&b.id), Some(Type::String)) =>
                        {
                            willow_abi::runtime_symbol("willow_string_concat")
                                .expect("string concat ABI")
                                .effects()
                                .bits()
                        }
                        // Only direct runtime scheduler builtins prove this lowering.
                        // Stored/custom futures retain their conservative await bound.
                        Expr::Await(a)
                            if is_async
                                && matches!(&a.expr, Expr::Call(c)
                                    if matches!(c.callee.as_str(), "sleep" | "yield")
                                        && calls.get(&c.id) == Some(&Some(FunctionId::free(&c.callee)))
                                        && symbols.lookup_func(&c.callee)
                                            .is_some_and(|f| f.declaration_span.end == 0)) =>
                        {
                            RuntimeEffects::MAY_SUSPEND
                                .union(RuntimeEffects::MAY_PREEMPT)
                                .bits()
                        }
                        // An async function lowers `await` of a Task/JoinHandle/
                        // TaskResult value to `SuspendOp::AwaitTask`: the task
                        // parks in `willow_frame_await` and reads the terminal
                        // value through `willow_frame_await_check` (plain await:
                        // a cancelled task panics) or a `Result` allocation
                        // (`TaskResult`). No OS thread blocks. `Future<T>`
                        // values still lower to a blocking runtime call.
                        Expr::Await(a)
                            if is_async
                                && types.get(&a.expr.id()).is_some_and(|ty| {
                                    crate::semantic::type_checker::awaitable_task_type(ty).is_some()
                                }) =>
                        {
                            ["willow_frame_await", "willow_frame_await_check"]
                                .into_iter()
                                .map(|name| {
                                    willow_abi::runtime_symbol(name)
                                        .expect("task await ABI")
                                        .effects()
                                })
                                .fold(RuntimeEffects::MAY_PREEMPT, RuntimeEffects::union)
                                .intersection(RuntimeEffects::ALL)
                                .bits()
                        }
                        Expr::Await(_) | Expr::Select(_) | Expr::Print(..) => {
                            RuntimeEffects::ALL.bits()
                        }
                        Expr::Binary(b)
                            if b.op == BinOp::Add
                                && !matches!(types.get(&b.id), Some(Type::I64 | Type::F64)) =>
                        {
                            RuntimeEffects::ALL.bits()
                        }
                        Expr::MethodCall(_) => calls
                            .get(&e.id())
                            .and_then(|target| target.as_ref())
                            .and_then(crate::semantic::intrinsics::builtin_target_effects)
                            .map_or(0, |effects| effects.bits()),
                        Expr::StaticCall(_) => calls
                            .get(&e.id())
                            .and_then(|target| target.as_ref())
                            .filter(|target| {
                                target
                                    .owner()
                                    .is_some_and(|owner| symbols.lookup_class(&owner).is_none())
                            })
                            .and_then(|target| {
                                crate::semantic::intrinsics::builtin_static_effects(
                                    target,
                                    &builtin_aliases,
                                )
                            })
                            .map_or(0, |effects| effects.bits()),
                        Expr::Call(c)
                            if symbols
                                .lookup_func(&c.callee)
                                .is_none_or(|f| f.declaration_span.end == 0) =>
                        {
                            crate::semantic::intrinsics::builtin_call_runtime_name(&c.callee)
                                .and_then(willow_abi::runtime_symbol)
                                .map_or(0, |s| s.effects().intersection(RuntimeEffects::ALL).bits())
                        }
                        _ => 0,
                    };
                    if let Some(intrinsic) = calls
                        .get(&e.id())
                        .and_then(|target| target.as_ref())
                        .and_then(Intrinsic::from_function_id)
                    {
                        let operation = match intrinsic {
                            Intrinsic::ChannelSend => Some("channel-send"),
                            Intrinsic::ChannelRecv => Some("channel-recv"),
                            Intrinsic::ChannelRecvOpt => Some("channel-recv-opt"),
                            _ => None,
                        };
                        if let Some(operation) = operation {
                            result.semantic.channel_site(id, e.span(), operation);
                        }
                    }
                    let operation = match e {
                        Expr::New(_) => "object-allocation",
                        Expr::ArrayLiteral(..) => "array-allocation",
                        Expr::ObjectLiteral(_) => "object-literal-allocation",
                        Expr::Await(_) => "await",
                        Expr::Select(_) => "select",
                        Expr::Print(..) => "print",
                        Expr::Binary(b) if matches!(types.get(&b.id), Some(Type::String)) => {
                            "string-concatenation"
                        }
                        Expr::Binary(_) => "non-scalar-addition",
                        Expr::MethodCall(_)
                            if calls
                                .get(&e.id())
                                .and_then(|target| target.as_ref())
                                .and_then(Intrinsic::from_function_id)
                                .is_some_and(|intrinsic| {
                                    matches!(
                                        intrinsic,
                                        Intrinsic::ArrayLen | Intrinsic::FrozenArrayLen
                                    )
                                }) =>
                        {
                            "array-length-null-panic"
                        }
                        Expr::MethodCall(_) => "builtin-method-call",
                        Expr::Call(_) | Expr::StaticCall(_) => "builtin-call",
                        _ => "expression",
                    };
                    Some((effects, e.span(), operation))
                }
                _ => None,
            };
            if let Some((effects, span, reason)) = capability {
                bits |= effects;
                if effects != 0 {
                    result.semantic.capability(id, effects, span, reason);
                    if let AstEvent::Expr(Expr::Await(a)) = event {
                        let operand = types.get(&a.expr.id()).map(awaited_type_name);
                        result.semantic.await_type(
                            id,
                            span,
                            operand.as_deref(),
                            effects & RuntimeEffects::MAY_BLOCK.bits() == 0,
                        );
                    }
                }
            }
            match event {
                AstEvent::Lambda(l) => {
                    let child = index
                        .and_then(|index| index.lambda_in(body_id, l.id))
                        .or(match &l.body {
                            LambdaBody::Block(b) => Some(b.id),
                            LambdaBody::Expr(_) => None,
                        });
                    if let Some(child) = child {
                        let event = match &l.body {
                            LambdaBody::Block(b) => AstEvent::Block(b),
                            LambdaBody::Expr(e) => AstEvent::Expr(e),
                        };
                        let child_id = FunctionId::lambda(child);
                        let root = lambda_roots.get(&id).copied().unwrap_or(id);
                        lambda_roots.insert(child_id, root);
                        result.lambda_names.insert(
                            child_id,
                            format!("<lambda {root}@{}:{}>", l.span.start, l.span.end),
                        );
                        pending.push((child_id, l.span, child, event, false));
                    } else {
                        bits |= RuntimeEffects::ALL.bits();
                    }
                    walk.skip_children();
                }
                AstEvent::Expr(e) => {
                    if initializer && let Some(target) = calls.get(&e.id()) {
                        if let Some(target) = target {
                            initializer_calls.targets.insert(*target);
                        } else {
                            initializer_calls.has_unknown = true;
                        }
                    }
                    if let Some(Some(target)) = calls.get(&e.id()) {
                        let receiver = match e {
                            Expr::MethodCall(call) => types.get(&call.object.id()),
                            _ => None,
                        };
                        result
                            .interfaces
                            .site(symbols, id, *target, receiver, e.span());
                    }
                    if let Expr::MethodCall(call) = e
                        && let Some(Type::Named(name) | Type::Generic(name, _)) =
                            types.get(&call.object.id())
                        && let Some(class) = symbols.lookup_class(name)
                    {
                        result.rename_calls.push((
                            call.span,
                            FunctionId::method(TypeId::from_source_name(&class.name), &call.method),
                        ));
                        result.dispatch_sites.insert(
                            call.span,
                            FunctionId::method(TypeId::from_source_name(&class.name), &call.method),
                        );
                        result
                            .dispatches
                            .entry(id)
                            .or_default()
                            .insert(FunctionId::method(
                                TypeId::from_source_name(&class.name),
                                &call.method,
                            ));
                    }
                }
                _ => {}
            }
        }
        if initializer {
            result.graph.merge(id, initializer_calls);
        }
        *result.own_effects.entry(id).or_default() |= bits;
        *result.direct.entry(id).or_default() |= bits | compiler_bits;
    }
    result
}

/// Spell an awaited operand type for AI output. A task with no value is
/// written `Task<()>` (likewise `JoinHandle<()>`/`TaskResult<()>`), the unit
/// spelling users annotate, instead of the checker's internal `void` payload.
fn awaited_type_name(ty: &Type) -> String {
    match ty {
        Type::Generic(name, args)
            if matches!(args.as_slice(), [Type::Void])
                && crate::semantic::type_checker::awaitable_task_type(ty).is_some() =>
        {
            format!("{name}<()>")
        }
        _ => crate::semantic::type_checker::type_name(ty),
    }
}

pub(crate) fn snapshot(
    frontend: &crate::Frontend,
    entry: &Path,
    source: &str,
    project: Option<&Path>,
) -> Result<Snapshot> {
    snapshot_with_boundary(
        frontend,
        entry,
        source,
        project,
        SnapshotBoundary::default(),
    )
}

type DispatchDependents<'a> = &'a dyn Fn(&[Function], &[String], usize) -> Vec<Function>;
type InstanceCapture = Vec<(String, String, Vec<String>)>;
type RetainedFunction<'a> = &'a dyn Fn(&str) -> Option<Function>;
#[derive(Default)]
struct SnapshotBoundary<'a> {
    instances: Option<&'a std::cell::RefCell<InstanceCapture>>,
    functions: HashMap<(UnitId, FunctionId), Function>,
    synthetic: Vec<Function>,
    dispatch: Option<&'a std::cell::RefCell<Option<incremental_dispatch::State>>>,
    dispatch_retired: Option<&'a std::collections::HashSet<String>>,
    dispatch_dependents: Option<DispatchDependents<'a>>,
    retained_id: Option<RetainedFunction<'a>>,
    fixed_ids: Option<&'a std::cell::RefCell<std::collections::HashSet<String>>>,
    retained: Option<&'a dyn Fn(UnitId, FunctionId) -> Option<Function>>,
    bodyless: Option<&'a dyn Fn(UnitId, FunctionId) -> Option<RuntimeEffects>>,
    indirect: Option<&'a HashMap<UnitId, indirect::Inputs>>,
    generics: Option<&'a HashMap<UnitId, generics::Source>>,
}
fn snapshot_with_boundary(
    frontend: &crate::Frontend,
    entry: &Path,
    source: &str,
    project: Option<&Path>,
    context: SnapshotBoundary<'_>,
) -> Result<Snapshot> {
    let SnapshotBoundary {
        instances: retained_instances,
        functions: boundary,
        synthetic: retained_synthetic,
        dispatch: retained_dispatch,
        dispatch_retired,
        dispatch_dependents,
        retained_id,
        fixed_ids,
        retained,
        bodyless: retained_bodyless,
        indirect: retained_indirect,
        generics: selected_generic_sources,
    } = context;
    // A full snapshot, or a patch whose generic sources were reselected,
    // rebuilds generic restoration, aggregation, source edges and spellings.
    let regenerates_generics = retained.is_none() || selected_generic_sources.is_some();
    let generic_sources = selected_generic_sources.unwrap_or(&frontend.db.generic_sources);
    let workspace =
        std::fs::canonicalize(project.unwrap_or(entry.parent().context("entry parent")?))?;
    let workspace = workspace
        .to_str()
        .context("non UTF-8 workspace")?
        .to_string();
    let mut captured = frontend
        .db
        .effects
        .analysis
        .borrow_mut()
        .take()
        .context("analysis capture disabled")?;
    if regenerates_generics {
        generics::restore(generic_sources, &mut captured);
    }
    let SourceIndex {
        paths,
        namespaces,
        programs,
        sources,
        fingerprints,
        symbol_names,
        bridge_declarations,
    } = SourceIndex::new(frontend, entry, source, &captured, retained.is_none())?;
    let (aliases, item_imports) =
        import_aliases(frontend, &programs, &mut captured, retained.is_none());
    if let Some(inputs) = retained_indirect {
        for (&unit, capture) in &mut captured {
            capture.indirect = inputs.get(&unit).cloned().unwrap_or_default();
        }
    }
    let resolver = CallResolver {
        frontend,
        captured: &captured,
        namespaces: &namespaces,
        aliases: &aliases,
        resolutions: Default::default(),
    };
    let resolve = |unit: UnitId, id: FunctionId| resolver.resolve(unit, id);
    let mut nodes = function_nodes(&captured, &paths, &resolve, boundary);
    if let Some(retained) = retained {
        retain_direct_targets(&captured, &resolve, &mut nodes, retained);
    }
    attach_rename_calls(&captured, &paths, &resolve, &mut nodes);
    // Declared types whose calls have no Willow body: an implicit memberwise
    // `init` only stores fields (the `new` site already carries the
    // allocation), and a variant constructor boxes its payload. Indexed once
    // so each call target is classified in O(1).
    let (bodyless, module_aliases) = bodyless_types(&programs, retained.is_none());
    let bodyless_effects = |(unit, target): &(UnitId, FunctionId)| {
        if let Some(retained) = retained_bodyless {
            return retained(*unit, *target);
        }
        let is_enum = *bodyless.get(&(*unit, target.owner()?.as_ref()))?;
        match (is_enum, target.name().as_ref()) {
            (false, "init") => Some(RuntimeEffects::NONE),
            (true, _) => Some(RuntimeEffects::MAY_ALLOCATE),
            _ => None,
        }
    };
    // Effects of call targets with no Willow body: intrinsics, implicit
    // initializers, variant constructors and builtin statics.
    let bodyless_target_effects = |unit: UnitId, target: &FunctionId| {
        crate::semantic::intrinsics::builtin_target_effects(target)
            .or_else(|| bodyless_effects(&resolve(unit, *target)))
            .or_else(|| {
                crate::semantic::intrinsics::builtin_static_effects(target, &module_aliases[&unit])
            })
    };
    if let Some(retained) = retained {
        retain_indirect_targets(&captured, &resolve, &mut nodes, retained);
    }
    let (indirect_functions, indirect_calls) = indirect::infer(&captured, &nodes, &resolve);
    let incomplete_indirect: std::collections::HashSet<_> = indirect_functions
        .iter()
        .filter(|f| f.unknown)
        .map(|f| f.id.as_str())
        .collect();
    let mut edges = HashMap::<(UnitId, FunctionId), BTreeSet<String>>::new();
    for (&unit, capture) in &captured {
        for (&id, locations) in &capture.declarations {
            let node = nodes.get_mut(&resolve(unit, id)).unwrap();
            attach_declaration(node, capture, id, locations, &paths, &fingerprints)?;
        }
        let calls = CallSites {
            unit,
            capture,
            paths: &paths,
            resolve: &resolve,
            indirect_calls: &indirect_calls,
            incomplete_indirect: &incomplete_indirect,
            bodyless_target_effects: &bodyless_target_effects,
        };
        calls.attach(&mut nodes, &mut edges);
    }
    let interface_routing = interfaces::route(
        captured
            .iter()
            .map(|(&unit, capture)| (unit, &capture.interfaces)),
        &nodes,
        &resolve,
        &mut edges,
    );
    // A dependency checker cannot see subclasses declared by its consumers.
    // Complete virtual dispatch from all checked declarations. Group identical
    // receiver/method queries and share each union, avoiding Q * D new edges.
    let classes = if retained.is_some() {
        Vec::new()
    } else {
        dispatch_classes(&programs, &nodes, &resolve, &paths)
    };
    let dispatches = dispatch_queries(&captured, &resolve);
    let mut dispatch_changed_ids = std::collections::HashSet::new();
    let mut retained_synthetic = retained_synthetic;
    let dispatch = if let Some(retained_dispatch) = retained_dispatch {
        let owners = dispatch_owners(dispatch_retired, &captured, &nodes, &resolve, &dispatches);
        let retained = RetainedDispatch {
            state: retained_dispatch,
            dependents: dispatch_dependents,
            retained_id,
        };
        retained.complete(
            owners,
            &classes,
            &dispatches,
            &mut dispatch_changed_ids,
            &mut retained_synthetic,
        )
    } else {
        dispatch::complete(&classes, &dispatches.keys().cloned().collect())
    };
    apply_dispatch_queries(dispatches, &dispatch.queries, &mut nodes, &mut edges);
    let dispatch_sites = dispatch_sites(&captured, &resolve, &dispatch.queries, &paths);
    let mut semantic = semantic::finish(
        &captured,
        &paths,
        &nodes,
        &resolve,
        &|unit, target| bodyless_target_effects(unit, target).is_some(),
        &fingerprints,
        &dispatch_sites,
    )?;
    attach_dispatch_targets(&mut semantic, &dispatch_sites);
    attach_interface_calls(&mut semantic, interface_routing.calls, &paths);
    for (key, targets) in edges {
        nodes.get_mut(&key).unwrap().callees = targets.into_iter().collect();
    }
    let captured_ids = captured_function_ids(&captured, &nodes, &resolve);
    let indirect_ids: std::collections::HashSet<_> = indirect_functions
        .iter()
        .map(|function| function.id.clone())
        .collect();
    let functions: Vec<_> = nodes
        .into_values()
        .chain(dispatch.functions)
        .chain(indirect_functions)
        .chain(interface_routing.functions)
        .collect();
    let produced = ProducedIds {
        captured: &captured_ids,
        dispatch: &dispatch_changed_ids,
        indirect: &indirect_ids,
    };
    let mut functions = produced.merge_synthetic(functions, retained_synthetic, fixed_ids);
    // Instance graph node numbers follow this order; the node map has none.
    functions.sort_by(|a, b| a.id.cmp(&b.id));
    let instance_ids: Vec<String> = functions.iter().map(|f| f.id.clone()).collect();
    let instance_callees: Vec<Vec<String>> = functions.iter().map(|f| f.callees.clone()).collect();
    let merged = if regenerates_generics {
        generics::aggregate(generic_sources, &paths, &mut functions, &mut semantic)
    } else {
        HashMap::new()
    };
    if let Some(output) = retained_instances {
        output.borrow_mut().extend(
            instance_ids
                .iter()
                .zip(&instance_callees)
                .filter(|(id, _)| retained.is_none() || produced.contains(id))
                .map(|(id, callees)| {
                    (
                        id.clone(),
                        merged.get(id).unwrap_or(id).clone(),
                        callees.clone(),
                    )
                }),
        );
    }
    attach_item_imports(&mut functions, item_imports, &paths);
    normalize_functions(&mut functions);
    // Compiler format revision plus build source stamp: snapshots never silently
    // cross a compiler change, even when Cargo's package version stays fixed.
    let compiler = storage::compiler_stamp();
    let config = compatibility_config(frontend, entry, project);
    // The bundled standard library is not workspace source. Its bodies give
    // workspace calls their effects above, but it has no editable symbols or
    // references.
    for file in &frontend.module_graph.files {
        if file.is_bundled() {
            captured.remove(&file.id);
        }
    }
    let symbol_sources = SymbolSources {
        frontend,
        captured: &captured,
        paths: &paths,
        names: &symbol_names,
    };
    symbol_sources.finish(&functions, &mut semantic, retained.is_some())?;
    if regenerates_generics {
        generics::source_edges(
            generic_sources,
            &paths,
            &mut functions,
            &semantic.references,
        );
    }
    semantic.instances =
        graph::Instances::new(&instance_ids, &instance_callees, &merged, &functions);
    graph::propagate(&mut functions);
    packages::attach(frontend, &paths, &captured, &mut functions, &mut semantic)?;
    if retained.is_none() {
        attach_rust_bridges(
            frontend,
            &paths,
            &functions,
            bridge_declarations,
            &mut semantic,
            project,
        )?;
    }
    captured.clear();
    let mut snapshot = Snapshot {
        edit_context: None,
        version: 1,
        compiler,
        compatibility: hash(config),
        workspace,
        revision: String::new(),
        sources,
        functions,
        semantic,
    };
    if regenerates_generics && has_workspace_generics(frontend, generic_sources) {
        generics::present_types(&mut snapshot);
    }
    snapshot.revision = snapshot.digest()?;
    Ok(snapshot)
}

type CallKey = (UnitId, FunctionId);
/// Per consumer unit: local import spelling to target unit, plus the imported
/// item for an item import.
type ImportAliases = HashMap<UnitId, HashMap<String, (UnitId, Option<String>)>>;
/// Item imports to attach as rename sites: consumer, target, item and span.
type ItemImports<'a> = Vec<(UnitId, UnitId, &'a str, Span)>;

/// Source paths, hashes and token fingerprints of every checked unit.
struct SourceIndex<'a> {
    paths: HashMap<UnitId, String>,
    namespaces: HashMap<String, UnitId>,
    programs: HashMap<UnitId, &'a Program>,
    sources: BTreeMap<String, String>,
    fingerprints: HashMap<Span, String>,
    symbol_names: HashMap<UnitId, symbols::Names>,
    bridge_declarations: HashMap<(u32, String), String>,
}
impl<'a> SourceIndex<'a> {
    /// Indexes requested ranges first, then reads/tokenizes one unit at a time.
    /// No workspace-wide token/AST cache is retained by the snapshot adapter.
    fn new(
        frontend: &'a crate::Frontend,
        entry: &Path,
        source: &str,
        captured: &HashMap<UnitId, CapturedUnit>,
        index_bridges: bool,
    ) -> Result<Self> {
        let artifacts = frontend
            .module_graph
            .artifacts
            .as_ref()
            .context("source artifacts")?;
        let mut ranges = requested_ranges(captured);
        let mut bridge_spans: HashMap<UnitId, Vec<(&str, Span)>> = HashMap::new();
        for (unit, program) in std::iter::once((UnitId::ENTRY, &frontend.program)).chain(
            frontend
                .module_graph
                .files
                .iter()
                .map(|m| (m.id, &m.program)),
        ) {
            if !index_bridges {
                continue;
            }
            for item in &program.items {
                if let Item::Function(f) = item
                    && f.rust_bridge.is_some()
                {
                    bridge_spans
                        .entry(unit)
                        .or_default()
                        .push((&f.name, f.span));
                }
            }
        }
        let mut index = Self {
            paths: HashMap::from([(UnitId::ENTRY, entry.to_string_lossy().into_owned())]),
            namespaces: HashMap::new(),
            programs: HashMap::from([(UnitId::ENTRY, &frontend.program)]),
            sources: BTreeMap::new(),
            fingerprints: HashMap::new(),
            symbol_names: HashMap::new(),
            bridge_declarations: HashMap::new(),
        };
        let entry_path = index.paths[&UnitId::ENTRY].clone();
        index.index_unit(
            UnitId::ENTRY,
            entry_path,
            source,
            &mut ranges,
            &mut bridge_spans,
        )?;
        for m in &frontend.module_graph.files {
            let path = source_path(&m.path)?.to_string_lossy().into_owned();
            index.paths.insert(m.id, path.clone());
            index.namespaces.insert(m.identity_path().to_string(), m.id);
            index.programs.insert(m.id, &m.program);
            let source = artifacts.source(m.id.file_id())?;
            index.index_unit(m.id, path, &source, &mut ranges, &mut bridge_spans)?;
        }
        Ok(index)
    }

    fn index_unit(
        &mut self,
        unit: UnitId,
        path: String,
        source: &str,
        ranges: &mut HashMap<UnitId, std::collections::HashSet<Span>>,
        bridge_spans: &mut HashMap<UnitId, Vec<(&str, Span)>>,
    ) -> Result<()> {
        if let Some(spans) = bridge_spans.remove(&unit) {
            for (name, span) in spans {
                self.bridge_declarations.insert(
                    (unit.0, name.to_owned()),
                    source
                        .get(span.start..span.end)
                        .context("invalid bridge declaration span")?
                        .to_owned(),
                );
            }
        }
        self.sources.insert(path, hash(source));
        let tokens = TokenHashes::new(source)?;
        if let Some(spans) = ranges.remove(&unit) {
            for span in spans {
                self.fingerprints.insert(span, tokens.range(span));
            }
        }
        self.symbol_names.insert(unit, tokens.names);
        Ok(())
    }
}

/// Every semantic, declaration and body span the snapshot fingerprints.
fn requested_ranges(
    captured: &HashMap<UnitId, CapturedUnit>,
) -> HashMap<UnitId, std::collections::HashSet<Span>> {
    let mut ranges: HashMap<UnitId, std::collections::HashSet<Span>> = HashMap::new();
    for capture in captured.values() {
        for span in capture.semantic.spans() {
            ranges
                .entry(crate::module::ModuleId(span.file_id.0))
                .or_default()
                .insert(span);
        }
        for locations in capture.declarations.values() {
            for &(span, body) in locations {
                ranges
                    .entry(crate::module::ModuleId(span.file_id.0))
                    .or_default()
                    .insert(span);
                ranges
                    .entry(crate::module::ModuleId(body.file_id.0))
                    .or_default()
                    .insert(body);
            }
        }
    }
    ranges
}

/// Resolves aliases per consumer once, using the compiler's package index.
/// A full snapshot also records import-target references and item imports.
fn import_aliases<'a>(
    frontend: &'a crate::Frontend,
    programs: &HashMap<UnitId, &'a Program>,
    captured: &mut HashMap<UnitId, CapturedUnit>,
    full: bool,
) -> (ImportAliases, ItemImports<'a>) {
    let mut aliases = HashMap::new();
    let mut item_imports = Vec::new();
    for (&unit, program) in programs {
        let mut map = HashMap::new();
        // Generated imports still resolve checked call targets. Source imports
        // below supply public selectors and edit provenance, not replacement
        // compiler namespaces.
        for import in &program.imports {
            if let Some(target) = frontend.db.dependencies().unit_for_path(unit, &import.path) {
                let local = import
                    .alias
                    .as_deref()
                    .unwrap_or_else(|| QualifiedName::new(&import.path).last_segment());
                map.insert(local.to_owned(), (target, None));
                map.insert(import.path.clone(), (target, None));
            }
        }
        for import in frontend
            .db
            .generic_sources
            .get(&unit)
            .map_or(&program.imports, |s| &s.imports)
        {
            let local = import
                .alias
                .as_deref()
                .unwrap_or_else(|| QualifiedName::new(&import.path).last_segment());
            if let Some(target) = frontend.db.dependencies().unit_for_path(unit, &import.path) {
                map.insert(local.to_string(), (target, None));
                map.insert(import.path.clone(), (target, None));
                if full
                    && let Some(module) = programs.get(&target).and_then(|p| p.module.as_ref())
                    && let Some(capture) = captured.get_mut(&unit)
                {
                    capture.symbols.reference(
                        import.span,
                        &import.path,
                        crate::semantic::analysis_symbols::Declaration::new(
                            &module.path,
                            "module",
                            module.span,
                            None,
                        ),
                        "import-target",
                        false,
                    );
                }
            } else if let Some((module, item)) = QualifiedName::new(&import.path).split_last()
                && let Some(target) = frontend.db.dependencies().unit_for_path(unit, module)
            {
                map.insert(local.to_string(), (target, Some(item.to_string())));
                if full {
                    item_imports.push((unit, target, item, import.span));
                }
                if full
                    && let Some(declaration) = frontend
                        .db
                        .generic_sources
                        .get(&target)
                        .and_then(|s| s.items.get(item))
                    && let Some(capture) = captured.get_mut(&unit)
                {
                    capture.symbols.reference(
                        import.span,
                        &import.path,
                        declaration.clone(),
                        "import-target",
                        true,
                    );
                }
            }
        }
        aliases.insert(unit, map);
    }
    (aliases, item_imports)
}

fn local_id(id: FunctionId) -> FunctionId {
    match id.owner() {
        Some(owner) => FunctionId::method(TypeId::local(owner), id.name()),
        None => FunctionId::free(id.name()),
    }
}

/// Resolves a call target spelled in one unit to its declaring unit and local
/// identity. Each distinct (unit, target) pair is resolved once.
struct CallResolver<'a> {
    frontend: &'a crate::Frontend,
    captured: &'a HashMap<UnitId, CapturedUnit>,
    namespaces: &'a HashMap<String, UnitId>,
    aliases: &'a ImportAliases,
    resolutions: std::cell::RefCell<HashMap<CallKey, CallKey>>,
}
impl CallResolver<'_> {
    fn resolve(&self, unit: UnitId, id: FunctionId) -> CallKey {
        let key = (unit, id);
        if let Some(target) = self.resolutions.borrow().get(&key).copied() {
            return target;
        }
        let target = self.resolve_uncached(unit, id);
        self.resolutions.borrow_mut().insert(key, target);
        target
    }

    fn resolve_uncached(&self, unit: UnitId, id: FunctionId) -> CallKey {
        let aliases = &self.aliases[&unit];
        if let (Some(ns), Some(owner)) = (id.namespace(), id.owner()) {
            // A qualified module function is represented as a static call:
            // the module's final path segment occupies the owner slot.
            let module = QualifiedName::join(&ns, &owner);
            if let Some(target) = self.namespaces.get(&module).copied().or_else(|| {
                aliases
                    .get(&module)
                    .filter(|(_, item)| item.is_none())
                    .map(|(target, _)| *target)
            }) {
                return (target, FunctionId::free(id.name()));
            }
        }
        if let Some(ns) = id.namespace() {
            if let Some(target) = self.namespaces.get(ns.as_ref()) {
                return (*target, local_id(id));
            }
            if let Some((target, _)) = aliases.get(ns.as_ref()) {
                return (*target, local_id(id));
            }
        } else if let Some(owner) = id.owner() {
            if let Some((target, item)) = aliases.get(owner.as_ref()) {
                return (
                    *target,
                    item.as_ref().map_or_else(
                        || FunctionId::free(id.name()),
                        |item| FunctionId::method(TypeId::local(item), id.name()),
                    ),
                );
            }
            // `import m::Item` alone still lets the checker call `m::f`: the
            // module path resolves without a module import.
            if !self.captured[&unit].declarations.contains_key(&id)
                && let Some(target) = self
                    .frontend
                    .db
                    .dependencies()
                    .unit_for_path(unit, owner.as_ref())
            {
                return (target, FunctionId::free(id.name()));
            }
        } else if !self.captured[&unit].declarations.contains_key(&id)
            && let Some((target, Some(item))) = aliases.get(id.name().as_ref())
        {
            return (*target, FunctionId::free(item));
        }
        (unit, id)
    }
}

/// One node per resolved graph or declared callable; boundary nodes win.
fn function_nodes(
    captured: &HashMap<UnitId, CapturedUnit>,
    paths: &HashMap<UnitId, String>,
    resolve: &impl Fn(UnitId, FunctionId) -> CallKey,
    boundary: HashMap<CallKey, Function>,
) -> HashMap<CallKey, Function> {
    let mut nodes = boundary;
    for (&unit, capture) in captured {
        for id in capture.graph.ids().chain(capture.declarations.keys()) {
            let (owner, local) = resolve(unit, *id);
            let key = (owner, local);
            nodes.entry(key).or_insert_with(|| {
                let module = paths[&owner].clone();
                let name = captured[&owner]
                    .lambda_names
                    .get(&local)
                    .cloned()
                    .unwrap_or_else(|| local.to_string());
                Function {
                    identity: None,
                    body_id: None,
                    body_location: None,
                    rename_calls: vec![],
                    rename_imports: vec![],
                    id: hash(serde_json::to_vec(&(&module, &name)).unwrap()),
                    module,
                    name,
                    locations: vec![],
                    synthetic: true,
                    fingerprint: String::new(),
                    body_fingerprint: String::new(),
                    callees: vec![],
                    runtime_effects: 0,
                    unknown: false,
                    unresolved: vec![],
                }
            });
        }
    }
    nodes
}

/// A patch keeps retained nodes for direct call targets outside its closure.
fn retain_direct_targets(
    captured: &HashMap<UnitId, CapturedUnit>,
    resolve: &impl Fn(UnitId, FunctionId) -> CallKey,
    nodes: &mut HashMap<CallKey, Function>,
    retained: &dyn Fn(UnitId, FunctionId) -> Option<Function>,
) {
    for (&unit, capture) in captured {
        for (_, sites) in capture.graph.iter() {
            for target in &sites.targets {
                let key = resolve(unit, *target);
                if let std::collections::hash_map::Entry::Vacant(entry) = nodes.entry(key)
                    && let Some(node) = retained(key.0, key.1)
                {
                    entry.insert(node);
                }
            }
        }
    }
}

/// A patch keeps retained nodes for function values reaching indirect calls.
fn retain_indirect_targets(
    captured: &HashMap<UnitId, CapturedUnit>,
    resolve: &impl Fn(UnitId, FunctionId) -> CallKey,
    nodes: &mut HashMap<CallKey, Function>,
    retained: &dyn Fn(UnitId, FunctionId) -> Option<Function>,
) {
    for (&unit, capture) in captured {
        for (target, _, _) in &capture.indirect.values {
            if let Some(target) = target {
                let key = resolve(unit, *target);
                if !nodes.contains_key(&key)
                    && let Some(function) = retained(key.0, key.1)
                {
                    nodes.insert(key, function);
                }
            }
        }
    }
}

fn attach_rename_calls(
    captured: &HashMap<UnitId, CapturedUnit>,
    paths: &HashMap<UnitId, String>,
    resolve: &impl Fn(UnitId, FunctionId) -> CallKey,
    nodes: &mut HashMap<CallKey, Function>,
) {
    for (&unit, capture) in captured {
        for &(span, target) in &capture.rename_calls {
            if let Some(node) = nodes.get_mut(&resolve(unit, target))
                && let Some(path) = paths.get(&crate::module::ModuleId(span.file_id.0))
            {
                node.rename_calls.push(Location {
                    path: path.clone(),
                    start: span.start,
                    end: span.end,
                });
            }
        }
    }
}

/// Declared types without a constructor body, by unit and name: `true` for
/// enums.
type BodylessTypes<'a> = HashMap<(UnitId, &'a str), bool>;
/// Builtin module aliases of every unit and, for a full snapshot, its declared
/// types without a constructor body (`true` for enums).
fn bodyless_types<'a>(
    programs: &HashMap<UnitId, &'a Program>,
    full: bool,
) -> (BodylessTypes<'a>, HashMap<UnitId, HashMap<String, String>>) {
    let mut bodyless = HashMap::<(UnitId, &str), bool>::new();
    let mut module_aliases = HashMap::new();
    for (&unit, program) in programs {
        module_aliases.insert(
            unit,
            crate::compiler_db::normalize::builtin_module_aliases(program),
        );
        if !full {
            continue;
        }
        for item in &program.items {
            match item {
                Item::Class(class) if class.constructors.is_empty() => {
                    bodyless.insert((unit, class.name.as_str()), false);
                }
                Item::Enum(e) => {
                    bodyless.insert((unit, e.name.as_str()), true);
                }
                _ => {}
            }
        }
    }
    (bodyless, module_aliases)
}

/// Marks a declared callable as analyzed and attaches its source locations,
/// fingerprints and direct effects.
fn attach_declaration(
    node: &mut Function,
    capture: &CapturedUnit,
    id: FunctionId,
    locations: &[(Span, Span)],
    paths: &HashMap<UnitId, String>,
    fingerprints: &HashMap<Span, String>,
) -> Result<()> {
    node.synthetic = false;
    node.body_id = capture.identities.get(&id).copied();
    let mut function_fingerprints = Vec::new();
    let mut bodies = Vec::new();
    for &(span, body) in locations {
        let file = crate::module::ModuleId(span.file_id.0);
        if let Some(path) = paths.get(&file) {
            node.body_location = paths
                .get(&crate::module::ModuleId(body.file_id.0))
                .map(|path| Location {
                    path: path.clone(),
                    start: body.start,
                    end: body.end,
                });
            node.locations.push(Location {
                path: path.clone(),
                start: span.start,
                end: span.end,
            });
            function_fingerprints.push(
                fingerprints
                    .get(&span)
                    .context("missing declaration source")?
                    .clone(),
            );
            bodies.push(
                fingerprints
                    .get(&body)
                    .context("missing body source")?
                    .clone(),
            );
        }
    }
    function_fingerprints.sort();
    bodies.sort();
    node.fingerprint = hash(serde_json::to_vec(&function_fingerprints)?);
    node.body_fingerprint = hash(serde_json::to_vec(&bodies)?);
    node.runtime_effects |= capture.direct.get(&id).copied().unwrap_or(0);
    Ok(())
}

/// One unit's call sites with the inputs that classify their targets.
struct CallSites<'a, R, B> {
    unit: UnitId,
    capture: &'a CapturedUnit,
    paths: &'a HashMap<UnitId, String>,
    resolve: &'a R,
    indirect_calls: &'a HashMap<CallKey, BTreeSet<String>>,
    incomplete_indirect: &'a std::collections::HashSet<&'a str>,
    bodyless_target_effects: &'a B,
}
impl<R, B> CallSites<'_, R, B>
where
    R: Fn(UnitId, FunctionId) -> CallKey,
    B: Fn(UnitId, &FunctionId) -> Option<RuntimeEffects>,
{
    /// Adds call edges and folds unresolved or bodyless targets into effects.
    fn attach(
        &self,
        nodes: &mut HashMap<CallKey, Function>,
        edges: &mut HashMap<CallKey, BTreeSet<String>>,
    ) {
        let (unit, capture) = (self.unit, self.capture);
        for (&id, sites) in capture.graph.iter() {
            let key = (self.resolve)(unit, id);
            let indirect_targets = self.indirect_calls.get(&key);
            // Only replace genuinely indirect obligations, never unresolved
            // methods, virtual dispatch, or unchecked generic templates.
            let inferred = indirect_targets.is_some_and(|targets| {
                targets
                    .iter()
                    .all(|target| !self.incomplete_indirect.contains(target.as_str()))
            }) && sites.unknown_span.is_none()
                && sites.unsupported_initialization.iter().all(|reason| {
                    matches!(
                        reason,
                        crate::semantic::call_graph::UnsupportedInitialization::IndirectCall
                    )
                });
            if let Some(targets) = indirect_targets {
                edges
                    .entry(key)
                    .or_default()
                    .extend(targets.iter().cloned());
            }
            let mut unknown = sites.has_unknown && !inferred;
            let mut unresolved = BTreeSet::new();
            if unknown {
                unresolved.insert(if capture.templates.contains(&id) {
                    generics::UNINSTANTIATED.to_string()
                } else {
                    "indirect-or-unresolved".to_string()
                });
            }
            let mut bits = 0;
            for target in &sites.targets {
                let key_target = (self.resolve)(unit, *target);
                if let Some(node) = nodes.get(&key_target) {
                    edges.entry(key).or_default().insert(node.id.clone());
                } else if let Some(effects) = (self.bodyless_target_effects)(unit, target) {
                    bits |= effects.bits();
                } else {
                    unknown = true;
                    unresolved.insert(format!("{}#{}", self.paths[&key_target.0], key_target.1));
                }
            }
            let node = nodes.get_mut(&key).unwrap();
            node.unresolved.extend(unresolved);
            node.unknown |= unknown;
            node.runtime_effects |= bits;
            if unknown {
                node.runtime_effects |= RuntimeEffects::ALL.bits();
            }
        }
    }
}

fn class_key(unit: UnitId, owner: &str) -> String {
    QualifiedName::join(format_args!("$unit{}", unit.0), owner)
}

/// Every checked class with its resolved base and dispatchable methods.
fn dispatch_classes(
    programs: &HashMap<UnitId, &Program>,
    nodes: &HashMap<CallKey, Function>,
    resolve: &impl Fn(UnitId, FunctionId) -> CallKey,
    paths: &HashMap<UnitId, String>,
) -> Vec<dispatch::Class> {
    let mut classes = Vec::new();
    for (&unit, program) in programs {
        for item in &program.items {
            if let Item::Class(class) = item {
                let key = class_key(unit, &class.name);
                let base = class.base_class.as_ref().map(|base| {
                    let spelling = base.spelling();
                    let (unit, id) = resolve(
                        unit,
                        FunctionId::method(TypeId::from_source_name(&spelling), "$dispatch"),
                    );
                    class_key(unit, id.owner().unwrap().as_ref())
                });
                let methods = class
                    .methods
                    .iter()
                    .filter(|m| !m.is_static)
                    .filter_map(|m| {
                        nodes
                            .get(&(
                                unit,
                                FunctionId::method(TypeId::local(&class.name), &m.name),
                            ))
                            .map(|f| (m.name.clone(), f.id.clone()))
                    })
                    .collect();
                classes.push(dispatch::Class {
                    key,
                    base,
                    module: paths[&unit].clone(),
                    name: class.name.clone(),
                    methods,
                });
            }
        }
    }
    classes
}

/// Virtual dispatch queries, (receiver class, method), with their callers.
fn dispatch_queries(
    captured: &HashMap<UnitId, CapturedUnit>,
    resolve: &impl Fn(UnitId, FunctionId) -> CallKey,
) -> HashMap<(String, String), BTreeSet<CallKey>> {
    let mut dispatches: HashMap<(String, String), BTreeSet<CallKey>> = HashMap::new();
    for (&unit, capture) in captured {
        for (&caller, targets) in &capture.dispatches {
            for &target in targets {
                let (owner, target) = resolve(unit, target);
                dispatches
                    .entry((
                        class_key(owner, target.owner().unwrap().as_ref()),
                        target.name().to_string(),
                    ))
                    .or_default()
                    .insert(resolve(unit, caller));
            }
        }
    }
    dispatches
}

/// The retained dispatch state of an incremental snapshot.
struct RetainedDispatch<'a> {
    state: &'a std::cell::RefCell<Option<incremental_dispatch::State>>,
    dependents: Option<DispatchDependents<'a>>,
    retained_id: Option<RetainedFunction<'a>>,
}
impl RetainedDispatch<'_> {
    /// Updates (or seeds) the retained dispatch state from the current owners
    /// and answers every query. Changed dispatch functions are recorded, and
    /// their retained dependents join `synthetic`.
    fn complete(
        &self,
        owners: HashMap<String, std::collections::HashSet<(String, String)>>,
        classes: &[dispatch::Class],
        dispatches: &HashMap<(String, String), BTreeSet<CallKey>>,
        changed_ids: &mut std::collections::HashSet<String>,
        synthetic: &mut Vec<Function>,
    ) -> dispatch::Dispatch {
        let mut state = self.state.borrow_mut();
        let mut functions;
        if let Some(state) = state.as_mut() {
            let delta = state.update(owners.into_iter().collect());
            changed_ids.extend(delta.functions.iter().map(|function| function.id.clone()));
            if let Some(dependents) = self.dependents {
                synthetic.extend(dependents(&delta.functions, &delta.removed, delta.visits));
            }
            functions = delta.functions;
        } else {
            let fresh = incremental_dispatch::State::new(classes, owners);
            functions = if fresh.supported() {
                fresh.functions().cloned().collect()
            } else {
                dispatch::complete(classes, &dispatches.keys().cloned().collect()).functions
            };
            *state = Some(fresh);
        }
        let state = state.as_ref().unwrap();
        let queries: HashMap<_, _> = dispatches
            .keys()
            .filter_map(|query| Some((query.clone(), state.query(&query.0, &query.1)?.to_owned())))
            .collect();
        let produced: std::collections::HashSet<_> = functions
            .iter()
            .map(|function| function.id.clone())
            .collect();
        for id in queries.values() {
            if !produced.contains(id)
                && let Some(function) = self
                    .retained_id
                    .and_then(|lookup| lookup(id))
                    .or_else(|| state.function(id).cloned())
            {
                functions.push(function);
            }
        }
        dispatch::Dispatch {
            functions,
            queries,
            #[cfg(test)]
            visits: 0,
        }
    }
}

fn attach_dispatch_targets(
    semantic: &mut SemanticFacts,
    dispatch_sites: &HashMap<(&str, usize, usize), String>,
) {
    for expression in &mut semantic.expressions {
        let l = &expression.location;
        if let Some(target) = dispatch_sites.get(&(l.path.as_str(), l.start, l.end)) {
            expression.target = Some(target.clone());
        }
    }
}

fn attach_interface_calls(
    semantic: &mut SemanticFacts,
    calls: Vec<(String, String, Span)>,
    paths: &HashMap<UnitId, String>,
) {
    for (caller, target, span) in calls {
        if let Some(path) = paths.get(&crate::module::ModuleId(span.file_id.0)) {
            semantic
                .effect_calls
                .entry(caller)
                .or_default()
                .push(semantic::EffectCall {
                    target,
                    location: Location {
                        path: path.clone(),
                        start: span.start,
                        end: span.end,
                    },
                });
        }
    }
}

/// Functions this projection produced: captured declarations, changed
/// dispatch functions and inferred indirect functions.
struct ProducedIds<'a> {
    captured: &'a std::collections::HashSet<String>,
    dispatch: &'a std::collections::HashSet<String>,
    indirect: &'a std::collections::HashSet<String>,
}
impl ProducedIds<'_> {
    fn contains(&self, id: &str) -> bool {
        self.captured.contains(id) || self.dispatch.contains(id) || self.indirect.contains(id)
    }

    /// Retained synthetic functions override projected ones unless this
    /// projection produced or fixed them. A synthetic node with no callees and
    /// no fixed value is unknown.
    fn merge_synthetic(
        &self,
        mut functions: Vec<Function>,
        retained_synthetic: Vec<Function>,
        fixed_ids: Option<&std::cell::RefCell<std::collections::HashSet<String>>>,
    ) -> Vec<Function> {
        let overrides: std::collections::HashSet<_> = retained_synthetic
            .iter()
            .filter(|function| {
                !self.contains(&function.id)
                    && fixed_ids.is_none_or(|fixed| !fixed.borrow().contains(&function.id))
            })
            .map(|function| function.id.clone())
            .collect();
        functions.retain(|function| !overrides.contains(&function.id));
        let produced: std::collections::HashSet<_> = functions
            .iter()
            .map(|function| function.id.clone())
            .collect();
        functions.extend(
            retained_synthetic
                .into_iter()
                .filter(|function| !produced.contains(&function.id)),
        );
        for node in &mut functions {
            if node.synthetic
                && node.callees.is_empty()
                && fixed_ids.is_none_or(|fixed| !fixed.borrow().contains(&node.id))
            {
                node.unknown = true;
                node.runtime_effects = RuntimeEffects::ALL.bits();
            }
        }
        functions
    }
}

fn attach_item_imports(
    functions: &mut [Function],
    item_imports: ItemImports<'_>,
    paths: &HashMap<UnitId, String>,
) {
    let by_source: HashMap<_, _> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| ((f.module.clone(), f.name.clone()), i))
        .collect();
    for (unit, target, item, span) in item_imports {
        if let Some(&i) = by_source.get(&(paths[&target].clone(), item.to_owned())) {
            functions[i].rename_imports.push(Location {
                path: paths[&unit].clone(),
                start: span.start,
                end: span.end,
            });
        }
    }
}

/// Entry, compiler options, target and project manifests that a snapshot is
/// only compatible with.
fn compatibility_config(
    frontend: &crate::Frontend,
    entry: &Path,
    project: Option<&Path>,
) -> String {
    let inputs = frontend.db.inputs();
    let mut config = format!(
        "{}|{:?}|{:?}|{}",
        entry.display(),
        inputs.options,
        inputs.target,
        target_lexicon::HOST
    );
    if let Some(root) = project {
        for name in ["project.toml", "project.lock"] {
            if let Ok(bytes) = std::fs::read(root.join(name)) {
                config.push_str(&hash(bytes));
            }
        }
    }
    config
}

/// Every dispatch owner of a retained dispatch update: retired functions,
/// captured declarations and the callers of each query.
fn dispatch_owners(
    retired: Option<&std::collections::HashSet<String>>,
    captured: &HashMap<UnitId, CapturedUnit>,
    nodes: &HashMap<CallKey, Function>,
    resolve: &impl Fn(UnitId, FunctionId) -> CallKey,
    dispatches: &HashMap<(String, String), BTreeSet<CallKey>>,
) -> HashMap<String, std::collections::HashSet<(String, String)>> {
    let mut owners = HashMap::<String, std::collections::HashSet<(String, String)>>::new();
    if let Some(retired) = retired {
        for id in retired {
            owners.entry(id.clone()).or_default();
        }
    }
    for (&unit, capture) in captured {
        for id in capture.declarations.keys() {
            if let Some(function) = nodes.get(&resolve(unit, *id)) {
                owners.entry(function.id.clone()).or_default();
            }
        }
    }
    for (query, callers) in dispatches {
        for caller in callers {
            if let Some(function) = nodes.get(caller) {
                owners
                    .entry(function.id.clone())
                    .or_default()
                    .insert(query.clone());
            }
        }
    }
    owners
}

/// Routes each caller to its answered dispatch function; an unanswered query
/// makes its callers unknown.
fn apply_dispatch_queries(
    dispatches: HashMap<(String, String), BTreeSet<CallKey>>,
    queries: &HashMap<(String, String), String>,
    nodes: &mut HashMap<CallKey, Function>,
    edges: &mut HashMap<CallKey, BTreeSet<String>>,
) {
    for (key, callers) in dispatches {
        if let Some(id) = queries.get(&key) {
            for caller in callers {
                edges.entry(caller).or_default().insert(id.clone());
            }
        } else {
            for caller in callers {
                let node = nodes.get_mut(&caller).unwrap();
                node.unknown = true;
                node.unresolved.push(QualifiedName::join(
                    format_args!("dispatch {}", key.0),
                    &key.1,
                ));
                node.runtime_effects |= RuntimeEffects::ALL.bits();
            }
        }
    }
}

/// The dispatch function answering each source dispatch site.
fn dispatch_sites<'p>(
    captured: &HashMap<UnitId, CapturedUnit>,
    resolve: &impl Fn(UnitId, FunctionId) -> CallKey,
    queries: &HashMap<(String, String), String>,
    paths: &'p HashMap<UnitId, String>,
) -> HashMap<(&'p str, usize, usize), String> {
    let mut dispatch_sites = HashMap::new();
    for (&unit, capture) in captured {
        for (&span, &target) in &capture.dispatch_sites {
            let (owner, target) = resolve(unit, target);
            if let Some(id) = queries.get(&(
                class_key(owner, target.owner().unwrap().as_ref()),
                target.name().to_string(),
            )) && let Some(path) = paths.get(&crate::module::ModuleId(span.file_id.0))
            {
                dispatch_sites.insert((path.as_str(), span.start, span.end), id.clone());
            }
        }
    }
    dispatch_sites
}

fn captured_function_ids(
    captured: &HashMap<UnitId, CapturedUnit>,
    nodes: &HashMap<CallKey, Function>,
    resolve: &impl Fn(UnitId, FunctionId) -> CallKey,
) -> std::collections::HashSet<String> {
    let mut captured_ids = std::collections::HashSet::new();
    for (&unit, capture) in captured {
        for id in capture.declarations.keys() {
            if let Some(function) = nodes.get(&resolve(unit, *id)) {
                captured_ids.insert(function.id.clone());
            }
        }
    }
    captured_ids
}

/// Sorts functions by id and their locations and unresolved targets.
fn normalize_functions(functions: &mut [Function]) {
    functions.sort_by(|a, b| a.id.cmp(&b.id));
    for node in functions {
        node.locations
            .sort_by(|a, b| (&a.path, a.start, a.end).cmp(&(&b.path, b.start, b.end)));
        node.locations.dedup();
        node.unresolved.sort();
        node.unresolved.dedup();
    }
}

/// Workspace units with their token names: the inputs of symbol, rename and
/// reference facts.
struct SymbolSources<'a> {
    frontend: &'a crate::Frontend,
    captured: &'a HashMap<UnitId, CapturedUnit>,
    paths: &'a HashMap<UnitId, String>,
    names: &'a HashMap<UnitId, symbols::Names>,
}
impl SymbolSources<'_> {
    /// Sets symbols, rename scopes and links, and references: a patch
    /// updates the retained reference index, a full snapshot captures it.
    fn finish(
        &self,
        functions: &[Function],
        semantic: &mut SemanticFacts,
        patch: bool,
    ) -> Result<()> {
        let (captured, paths, names) = (self.captured, self.paths, self.names);
        let (symbol_definitions, symbol_references) =
            symbols::finish(captured, paths, names, functions);
        semantic.symbols = symbol_definitions;
        semantic.rename_scopes = rename::scopes(captured, paths, names, &semantic.symbols);
        semantic.rename_links = rename::links(captured, paths, names, functions, &semantic.symbols);
        semantic.references = if patch {
            self.frontend.db.references.patch(symbol_references)
        } else {
            let ranges: Vec<_> = captured
                .values()
                .flat_map(|unit| &unit.symbol_owners)
                .filter_map(|(span, owner)| {
                    Some((
                        Location {
                            path: paths.get(&crate::module::ModuleId(span.file_id.0))?.clone(),
                            start: span.start,
                            end: span.end,
                        },
                        owner.clone(),
                    ))
                })
                .collect();
            self.frontend
                .db
                .references
                .capture(symbol_references, &ranges)?
        };
        Ok(())
    }
}

fn attach_rust_bridges(
    frontend: &crate::Frontend,
    paths: &HashMap<UnitId, String>,
    functions: &[Function],
    bridge_declarations: HashMap<(u32, String), String>,
    semantic: &mut SemanticFacts,
    project: Option<&Path>,
) -> Result<()> {
    semantic.rust_bridges = frontend
        .db
        .rust_bridge_symbols
        .iter()
        .map(|(_, symbol)| symbol.clone())
        .collect();
    interop::attach(frontend, paths, functions, bridge_declarations, semantic)?;
    if let Some(root) = project {
        semantic.interop.load_project(root)?;
    }
    Ok(())
}

/// Every unit records its generic sources, but only workspace templates and
/// instances introduce spellings that need presenting; the bundled standard
/// templates alone do not.
fn has_workspace_generics(
    frontend: &crate::Frontend,
    generic_sources: &HashMap<UnitId, generics::Source>,
) -> bool {
    let bundled: std::collections::HashSet<_> = frontend
        .module_graph
        .files
        .iter()
        .filter(|file| file.is_bundled())
        .map(|file| file.id)
        .collect();
    generic_sources
        .iter()
        .any(|(unit, source)| !bundled.contains(unit) && source.has_workspace_generics())
}

/// Two independent wrapping polynomial prefixes of SHA-256 token digests.
/// Range queries ignore comments/whitespace/absolute token positions in O(log T)
/// search + O(1) arithmetic, including deeply nested callable source ranges.
struct TokenHashes {
    names: symbols::Names,
    spans: Vec<(usize, usize)>,
    prefix: Vec<[u64; 2]>,
    powers: Vec<[u64; 2]>,
}
impl TokenHashes {
    fn new(source: &str) -> Result<Self> {
        Self::with_identifier_shape(source, false)
    }
    fn with_identifier_shape(source: &str, normalize: bool) -> Result<Self> {
        let tokens = crate::lexer::Lexer::new(source)
            .tokenize()
            .map_err(|_| anyhow::anyhow!("snapshot source lexing failed"))?;
        let mut result = Self {
            names: HashMap::new(),
            spans: vec![],
            prefix: vec![[0, 0]],
            powers: vec![[1, 1]],
        };
        for token in tokens {
            if matches!(token.kind, crate::lexer::token::TokenKind::Eof) {
                continue;
            }
            if !normalize {
                result
                    .names
                    .entry(source[token.span.start..token.span.end].to_owned())
                    .or_default()
                    .push((token.span.start, token.span.end));
            }
            let spelling =
                if normalize && matches!(token.kind, crate::lexer::token::TokenKind::Ident(_)) {
                    "identifier".to_owned()
                } else {
                    format!("{:?}", token.kind)
                };
            let hash = Sha256::digest(spelling.as_bytes());
            let mut next = [0; 2];
            let mut power = [0; 2];
            for (i, base) in [0x9e3779b185ebca87u64, 0xc2b2ae3d27d4eb4fu64]
                .into_iter()
                .enumerate()
            {
                let value = u64::from_le_bytes(hash[i * 8..i * 8 + 8].try_into().unwrap());
                next[i] = result.prefix.last().unwrap()[i]
                    .wrapping_mul(base)
                    .wrapping_add(value);
                power[i] = result.powers.last().unwrap()[i].wrapping_mul(base);
            }
            result.spans.push((token.span.start, token.span.end));
            result.prefix.push(next);
            result.powers.push(power);
        }
        Ok(result)
    }
    fn range(&self, span: Span) -> String {
        let start = self.spans.partition_point(|s| s.0 < span.start);
        let end = self.spans.partition_point(|s| s.1 <= span.end).max(start);
        let value: Vec<_> = (0..2)
            .map(|i| {
                self.prefix[end][i]
                    .wrapping_sub(self.prefix[start][i].wrapping_mul(self.powers[end - start][i]))
            })
            .collect();
        format!("{}:{:016x}{:016x}", end - start, value[0], value[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_ranges_ignore_offsets_but_detect_changes_with_linear_storage() {
        for n in [16, 64, 256, 1024, 4096] {
            // All nested ranges share one prefix inventory: no repeated hashing
            // of the contents of each increasingly deep enclosing range.
            let source = format!("{}x{}", "(".repeat(n), ")".repeat(n));
            let hashes = TokenHashes::new(&source).unwrap();
            assert_eq!(hashes.spans.len(), 2 * n + 1);
            assert_eq!(hashes.prefix.len(), 2 * n + 2);
            let shifted = format!("// ignored\n{source}");
            let other = TokenHashes::new(&shifted).unwrap();
            for i in 0..n {
                assert_eq!(
                    hashes.range(Span::new(i, source.len() - i, 1, 1)),
                    other.range(Span::new(i + 11, source.len() - i + 11, 2, 1))
                );
            }
            println!(
                "token-ranges depth={n} tokens={} prefix_entries={} ranges={n}",
                hashes.spans.len(),
                hashes.prefix.len()
            );
        }
        let a = TokenHashes::new("return 1;").unwrap();
        let b = TokenHashes::new("return 2;").unwrap();
        assert_ne!(
            a.range(Span::new(0, 9, 1, 1)),
            b.range(Span::new(0, 9, 1, 1))
        );
    }
}
