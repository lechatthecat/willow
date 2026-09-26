//! Opt-in, compiler-owned impact facts. No backend or runtime dependency.
mod dispatch;
pub mod edit;
mod graph;
mod semantic;
mod storage;
mod symbols;
mod warm;
pub use graph::{Direction, Impact, ImpactNode, Limits};
pub use semantic::{QueryRequest, QuerySession, SemanticFacts};
pub use storage::{Difference, FunctionChange};
pub use warm::WarmSession;
pub(crate) use warm::check_size;

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
    },
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
};

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
    #[serde(skip)]
    pub(crate) body_id: Option<BodyId>,
    #[serde(skip)]
    pub(crate) body_location: Option<Location>,
    #[serde(skip)]
    pub(crate) rename_calls: Vec<Location>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub version: u32,
    pub compiler: String,
    pub compatibility: String,
    pub workspace: String,
    pub revision: String,
    pub sources: BTreeMap<String, String>,
    pub functions: Vec<Function>,
    pub semantic: SemanticFacts,
}

pub(crate) fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
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
    Ok(format!("{:x}", writer.0.finalize()))
}

pub(crate) struct CapturedUnit {
    graph: CallGraph,
    semantic: semantic::Captured,
    symbols: crate::semantic::analysis_symbols::Facts,
    declarations: HashMap<FunctionId, Vec<(Span, Span)>>,
    identities: HashMap<FunctionId, BodyId>,
    lambda_names: HashMap<FunctionId, String>,
    rename_calls: Vec<(Span, FunctionId)>,
    direct: HashMap<FunctionId, u8>,
    dispatches: HashMap<FunctionId, BTreeSet<FunctionId>>,
    dispatch_sites: HashMap<Span, FunctionId>,
}

pub(crate) struct CaptureInputs<'a> {
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
        types,
        calls,
        patterns,
        symbols: symbol_facts,
    } = inputs;
    let mut result = CapturedUnit {
        graph: graph.clone(),
        semantic: semantic::Captured::default(),
        symbols: symbol_facts.clone(),
        declarations: HashMap::new(),
        identities: HashMap::new(),
        lambda_names: HashMap::new(),
        rename_calls: Vec::new(),
        direct: HashMap::new(),
        dispatches: HashMap::new(),
        dispatch_sites: HashMap::new(),
    };
    symbols::declarations(program, &mut result.symbols, symbols);
    let mut pending = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) => pending.push((
                FunctionId::free(&f.name),
                f.span,
                f.body.id,
                AstEvent::Block(&f.body),
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
                    ));
                }
                for init in &c.constructors {
                    pending.push((
                        FunctionId::method(TypeId::local(&c.name), "init"),
                        init.span,
                        init.body.id,
                        AstEvent::Block(&init.body),
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
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    let mut lambda_roots = HashMap::new();
    while let Some((id, span, body_id, body)) = pending.pop() {
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
        if let Some(summary) = facts.get(&id) {
            result
                .semantic
                .compiler_effects
                .insert(id, summary.effects().bits());
            let witnesses = (0..RuntimeEffects::BIT_COUNT).filter_map(|bit| {
                let effect = RuntimeEffects::from_bit(bit);
                if !summary.contains(effect) { return None; }
                let witness = match summary.witness(effect) {
                    Some(crate::compiler_db::effects::EffectWitness::Lock(w)) => serde_json::json!({"kind":"wait", "owner_key":w.owner, "cause":w.cause}),
                    Some(crate::compiler_db::effects::EffectWitness::Panic {owner, source}) => serde_json::json!({"kind":"panic", "owner_key":owner, "cause":{"span":Span::in_file(crate::diagnostics::FileId(source.0),source.1,source.2,0,0)}}),
                    Some(crate::compiler_db::effects::EffectWitness::External(target)) => serde_json::json!({"kind":"external-boundary","target":target.to_string()}),
                    Some(crate::compiler_db::effects::EffectWitness::Helper(reason)) => serde_json::json!({"kind":"nonpreemptible", "reason":format!("{reason:?}")}),
                    None => serde_json::json!({"kind":"unavailable"}),
                };
                Some(serde_json::json!({"effect":effect.bits(), "witness":witness}))
            }).collect();
            result.semantic.witnesses.insert(id, witnesses);
        }
        let mut bits = facts.get(&id).map_or_else(
            || {
                if matches!(body, AstEvent::Expr(_)) {
                    RuntimeEffects::ALL.bits()
                } else {
                    0
                }
            },
            |f| f.effects().bits(),
        );
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
                    let effects = match e {
                        Expr::New(_)
                        | Expr::ArrayLiteral(..)
                        | Expr::ObjectLiteral(_)
                        | Expr::String(..) => RuntimeEffects::MAY_ALLOCATE.bits(),
                        Expr::Await(_) | Expr::Select(_) | Expr::Print(..) => {
                            RuntimeEffects::ALL.bits()
                        }
                        Expr::Binary(b)
                            if b.op == BinOp::Add
                                && !matches!(types.get(&b.id), Some(Type::I64 | Type::F64)) =>
                        {
                            RuntimeEffects::ALL.bits()
                        }
                        Expr::Call(c)
                            if symbols
                                .lookup_func(&c.callee)
                                .is_none_or(|f| f.declaration_span.end == 0) =>
                        {
                            crate::semantic::intrinsics::builtin_call_runtime_name(&c.callee)
                                .and_then(willow_abi::runtime_symbol)
                                .map_or(0, |s| s.effects().bits())
                        }
                        _ => 0,
                    };
                    Some((effects, e.span(), "typed-expression-lowering"))
                }
                _ => None,
            };
            if let Some((effects, span, reason)) = capability {
                bits |= effects;
                if effects != 0 {
                    result.semantic.capability(id, effects, span, reason);
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
                        pending.push((child_id, l.span, child, event));
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
        *result.direct.entry(id).or_default() |= bits;
    }
    result
}

pub(crate) fn snapshot(
    frontend: &crate::Frontend,
    entry: &Path,
    source: &str,
    project: Option<&Path>,
) -> Result<Snapshot> {
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
    let mut paths = HashMap::from([(UnitId::ENTRY, entry.to_string_lossy().into_owned())]);
    let mut namespaces = HashMap::new();
    let mut programs = HashMap::from([(UnitId::ENTRY, &frontend.program)]);
    let artifacts = frontend
        .module_graph
        .artifacts
        .as_ref()
        .context("source artifacts")?;
    let mut sources = BTreeMap::new();
    // Index requested ranges first, then read/tokenize one unit at a time. No
    // workspace-wide token/AST cache is retained by the snapshot adapter.
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
    let mut fingerprints = HashMap::new();
    let mut symbol_names = HashMap::new();
    let mut index_source = |unit: UnitId, path: String, source: &str| -> Result<()> {
        sources.insert(path, hash(source));
        let tokens = TokenHashes::new(source)?;
        if let Some(spans) = ranges.remove(&unit) {
            for span in spans {
                fingerprints.insert(span, tokens.range(span));
            }
        }
        symbol_names.insert(unit, tokens.names);
        Ok(())
    };
    index_source(UnitId::ENTRY, paths[&UnitId::ENTRY].clone(), source)?;
    for m in &frontend.module_graph.files {
        let path = std::fs::canonicalize(&m.path)?
            .to_string_lossy()
            .into_owned();
        paths.insert(m.id, path.clone());
        namespaces.insert(m.identity_path().to_string(), m.id);
        programs.insert(m.id, &m.program);
        let source = artifacts.source(m.id.file_id())?;
        index_source(m.id, path, &source)?;
    }
    // Resolve aliases per consumer once, using the compiler's package index.
    let mut aliases = HashMap::new();
    for (&unit, program) in &programs {
        let mut map = HashMap::new();
        for import in &program.imports {
            let local = import
                .alias
                .as_deref()
                .unwrap_or_else(|| import.path.rsplit("::").next().unwrap());
            if let Some(target) = frontend.db.dependencies().unit_for_path(unit, &import.path) {
                map.insert(local.to_string(), (target, None));
                map.insert(import.path.clone(), (target, None));
                if let Some(module) = programs.get(&target).and_then(|p| p.module.as_ref())
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
            } else if let Some((module, item)) = import.path.rsplit_once("::")
                && let Some(target) = frontend.db.dependencies().unit_for_path(unit, module)
            {
                map.insert(local.to_string(), (target, Some(item.to_string())));
            }
        }
        aliases.insert(unit, map);
    }
    let local_id = |id: FunctionId| match id.owner() {
        Some(owner) => FunctionId::method(TypeId::local(owner), id.name()),
        None => FunctionId::free(id.name()),
    };
    let resolve = |unit: UnitId, id: FunctionId| -> (UnitId, FunctionId) {
        if let Some(ns) = id.namespace() {
            if let Some(target) = namespaces.get(ns) {
                return (*target, local_id(id));
            }
            if let Some((target, _)) = aliases[&unit].get(ns) {
                return (*target, local_id(id));
            }
        } else if let Some(owner) = id.owner() {
            if let Some((target, item)) = aliases[&unit].get(owner) {
                return (
                    *target,
                    item.as_ref().map_or_else(
                        || FunctionId::free(id.name()),
                        |item| FunctionId::method(TypeId::local(item), id.name()),
                    ),
                );
            }
        } else if !captured[&unit].declarations.contains_key(&id)
            && let Some((target, Some(item))) = aliases[&unit].get(id.name())
        {
            return (*target, FunctionId::free(item));
        }
        (unit, id)
    };
    let mut nodes: HashMap<(UnitId, FunctionId), Function> = HashMap::new();
    for (&unit, capture) in &captured {
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
                    body_id: None,
                    body_location: None,
                    rename_calls: vec![],
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
    for (&unit, capture) in &captured {
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
    let mut edges = HashMap::<(UnitId, FunctionId), BTreeSet<String>>::new();
    for (&unit, capture) in &captured {
        for (&id, locations) in &capture.declarations {
            let node = nodes.get_mut(&resolve(unit, id)).unwrap();
            node.synthetic = false;
            node.body_id = capture.identities.get(&id).copied();
            let mut function_fingerprints = Vec::new();
            let mut bodies = Vec::new();
            for &(span, body) in locations {
                let file = crate::module::ModuleId(span.file_id.0);
                if let Some(path) = paths.get(&file) {
                    node.body_location =
                        paths
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
        }
        for (&id, sites) in capture.graph.iter() {
            let key = resolve(unit, id);
            let mut unknown = sites.has_unknown;
            let mut unresolved = BTreeSet::new();
            if sites.has_unknown {
                unresolved.insert("indirect-or-unresolved".to_string());
            }
            let mut bits = 0;
            for target in &sites.targets {
                let key_target = resolve(unit, *target);
                if let Some(node) = nodes.get(&key_target) {
                    edges.entry(key).or_default().insert(node.id.clone());
                } else if target.owner().is_none()
                    && target.namespace().is_none()
                    && let Some(runtime) =
                        crate::semantic::intrinsics::builtin_call_runtime_name(target.name())
                    && let Some(symbol) = willow_abi::runtime_symbol(runtime)
                {
                    bits |= symbol.effects().bits();
                } else if target.owner().is_none()
                    && matches!(target.name(), "panic" | "recover" | "pow" | "powf")
                {
                    if target.name() == "panic" {
                        bits |= RuntimeEffects::MAY_PANIC.bits();
                    }
                } else {
                    unknown = true;
                    unresolved.insert(format!("{}#{}", paths[&key_target.0], key_target.1));
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
    // A dependency checker cannot see subclasses declared by its consumers.
    // Complete virtual dispatch from all checked declarations. Group identical
    // receiver/method queries and share each union, avoiding Q * D new edges.
    let class_key = |unit: UnitId, owner: &str| format!("$unit{}::{owner}", unit.0);
    let mut classes = Vec::new();
    for (&unit, program) in &programs {
        for item in &program.items {
            if let Item::Class(class) = item {
                let key = class_key(unit, &class.name);
                let base = class.base_class.as_ref().map(|base| {
                    let spelling = match base {
                        TypePath::Local(name) => name.clone(),
                        TypePath::Qualified(parts) => parts.join("::"),
                    };
                    let (unit, id) = resolve(
                        unit,
                        FunctionId::method(TypeId::from_source_name(&spelling), "$dispatch"),
                    );
                    class_key(unit, id.owner().unwrap())
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
    let mut dispatches: HashMap<(String, String), BTreeSet<(UnitId, FunctionId)>> = HashMap::new();
    for (&unit, capture) in &captured {
        for (&caller, targets) in &capture.dispatches {
            for &target in targets {
                let (owner, target) = resolve(unit, target);
                dispatches
                    .entry((
                        class_key(owner, target.owner().unwrap()),
                        target.name().to_string(),
                    ))
                    .or_default()
                    .insert(resolve(unit, caller));
            }
        }
    }
    let dispatch = dispatch::complete(&classes, &dispatches.keys().cloned().collect());
    for (key, callers) in dispatches {
        if let Some(id) = dispatch.queries.get(&key) {
            for caller in callers {
                edges.entry(caller).or_default().insert(id.clone());
            }
        } else {
            for caller in callers {
                let node = nodes.get_mut(&caller).unwrap();
                node.unknown = true;
                node.runtime_effects |= RuntimeEffects::ALL.bits();
            }
        }
    }
    let mut semantic = semantic::finish(&captured, &paths, &nodes, &resolve, &fingerprints)?;
    let mut dispatch_sites = HashMap::new();
    for (&unit, capture) in &captured {
        for (&span, &target) in &capture.dispatch_sites {
            let (owner, target) = resolve(unit, target);
            if let Some(id) = dispatch.queries.get(&(
                class_key(owner, target.owner().unwrap()),
                target.name().to_string(),
            )) && let Some(path) = paths.get(&crate::module::ModuleId(span.file_id.0))
            {
                dispatch_sites.insert((path.as_str(), span.start, span.end), id.clone());
            }
        }
    }
    for expression in &mut semantic.expressions {
        let l = &expression.location;
        if let Some(target) = dispatch_sites.get(&(l.path.as_str(), l.start, l.end)) {
            expression.target = Some(target.clone());
        }
    }
    for (key, targets) in edges {
        nodes.get_mut(&key).unwrap().callees = targets.into_iter().collect();
    }
    let mut functions: Vec<_> = nodes.into_values().chain(dispatch.functions).collect();
    for node in &mut functions {
        if node.synthetic && node.callees.is_empty() {
            node.unknown = true;
            node.runtime_effects = RuntimeEffects::ALL.bits();
        }
    }
    functions.sort_by(|a, b| a.id.cmp(&b.id));
    for node in &mut functions {
        node.locations
            .sort_by(|a, b| (&a.path, a.start, a.end).cmp(&(&b.path, b.start, b.end)));
        node.locations.dedup();
        node.unresolved.sort();
        node.unresolved.dedup();
    }
    graph::propagate(&mut functions);
    // Compiler format revision plus build source stamp: snapshots never silently
    // cross a compiler change, even when Cargo's package version stays fixed.
    let compiler = storage::compiler_stamp();
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
    let (symbol_definitions, symbol_references) =
        symbols::finish(&captured, &paths, &symbol_names, &functions);
    semantic.symbols = symbol_definitions;
    semantic.references = symbol_references;
    captured.clear();
    let mut snapshot = Snapshot {
        version: 1,
        compiler,
        compatibility: hash(config),
        workspace,
        revision: String::new(),
        sources,
        functions,
        semantic,
    };
    snapshot.revision = snapshot.digest()?;
    Ok(snapshot)
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
            result
                .names
                .entry(source[token.span.start..token.span.end].to_owned())
                .or_default()
                .push((token.span.start, token.span.end));
            let hash = Sha256::digest(format!("{:?}", token.kind).as_bytes());
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
