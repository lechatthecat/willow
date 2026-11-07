//! Lock-order proof over shared callable nodes and allocation classes.
//!
//! Callable nodes retain sharing: a long helper chain does not materialize a
//! transitive acquisition set at every caller. Only cycles containing a lock
//! class are errors; ordinary recursive helper components are harmless here.
//!
//! A lock with no known allocation gets a class named by its value type: lock
//! types are invariant, so it can only alias locks of that exact type. The class
//! stands for each known lock of the type without merging them: anything held
//! before it is ordered before each of them, and each of them is ordered before
//! anything taken under it. Nesting two such locks of one type is then a cycle,
//! while a parent reached before its differently typed child is ordered.
use std::collections::{HashMap, HashSet};

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::parser::iter::{AstEvent, AstWalk};
use crate::semantic::call_graph::{CallGraph, ClassHierarchy};
use crate::semantic::ids::{FunctionId, TypeId};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Class {
    Allocation(ExprId),
    Global(Span),
    Type(Type),
}

#[derive(Clone, Default)]
struct Identity {
    class: Option<Class>,
    // A stable lexical name proves repeated use even when its origin is opaque.
    same: Option<Span>,
}

#[derive(Default, Clone)]
struct Graph {
    edges: Vec<Vec<(usize, Span)>>,
    locks: Vec<bool>,
    #[cfg(test)]
    operations: usize,
}

/// Which nodes act for a type class. Built on the final merged graph, so the
/// node numbers stay valid.
#[derive(Default)]
struct TypeClasses {
    /// A class node and its `after` node, each mapped to the class.
    sides: HashMap<usize, usize>,
    /// A known lock mapped to the class that may alias it.
    known: HashMap<usize, usize>,
}

impl Graph {
    /// Let `class` stand for each of `locks` in O(locks), with no edge product:
    /// `class` keeps its incoming edges, a new node takes its outgoing ones, and
    /// every member sits between the two. Members stay unordered between
    /// themselves, so code that never meets the class is unaffected.
    fn stand_for(
        &mut self,
        class: usize,
        locks: impl Iterator<Item = usize>,
        classes: &mut TypeClasses,
    ) {
        let after = self.node(false);
        classes.sides.insert(class, class);
        classes.sides.insert(after, class);
        self.edges[after] = std::mem::take(&mut self.edges[class]);
        self.edge(class, after, Span::dummy());
        for lock in locks.filter(|&lock| lock != class) {
            self.edge(class, lock, Span::dummy());
            self.edge(lock, after, Span::dummy());
            classes.known.insert(lock, class);
        }
    }

    fn node(&mut self, lock: bool) -> usize {
        let result = self.edges.len();
        self.edges.push(Vec::new());
        self.locks.push(lock);
        result
    }

    fn edge(&mut self, from: usize, to: usize, span: Span) {
        self.edges[from].push((to, span));
    }

    /// Iterative Kosaraju, O(V + E) time and storage, without native recursion.
    fn diagnostics(&mut self, classes: &TypeClasses) -> Vec<Diagnostic> {
        let n = self.edges.len();
        let mut reverse = vec![Vec::new(); n];
        for (from, targets) in self.edges.iter().enumerate() {
            for &(to, _) in targets {
                reverse[to].push(from);
                #[cfg(test)]
                {
                    self.operations += 1;
                }
            }
        }
        let mut seen = vec![false; n];
        let mut order = Vec::with_capacity(n);
        for start in 0..n {
            if seen[start] {
                continue;
            }
            seen[start] = true;
            let mut stack = vec![(start, 0)];
            while let Some((node, next)) = stack.last_mut() {
                #[cfg(test)]
                {
                    self.operations += 1;
                }
                if let Some(&(child, _)) = self.edges[*node].get(*next) {
                    *next += 1;
                    if !seen[child] {
                        seen[child] = true;
                        stack.push((child, 0));
                    }
                } else {
                    order.push(*node);
                    stack.pop();
                }
            }
        }
        let mut components = vec![usize::MAX; n];
        let mut groups = Vec::new();
        for &start in order.iter().rev() {
            if components[start] != usize::MAX {
                continue;
            }
            let id = groups.len();
            let mut group = Vec::new();
            let mut stack = vec![start];
            components[start] = id;
            while let Some(node) = stack.pop() {
                group.push(node);
                #[cfg(test)]
                {
                    self.operations += 1;
                }
                for &parent in &reverse[node] {
                    #[cfg(test)]
                    {
                        self.operations += 1;
                    }
                    if components[parent] == usize::MAX {
                        components[parent] = id;
                        stack.push(parent);
                    }
                }
            }
            groups.push(group);
        }
        let mut diagnostics = Vec::new();
        for (id, group) in groups.iter().enumerate() {
            if !group.iter().any(|&node| self.locks[node]) {
                continue;
            }
            let cyclic =
                group.len() > 1 || self.edges[group[0]].iter().any(|&(to, _)| to == group[0]);
            if !cyclic {
                continue;
            }
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E2605,
                "lock acquisition order contains a cycle",
            );
            let mut spans = HashSet::new();
            for &node in group {
                for &(to, span) in &self.edges[node] {
                    #[cfg(test)]
                    {
                        self.operations += 1;
                    }
                    if components[to] == id && span != Span::dummy() && spans.insert(span) {
                        let label = if diagnostic.labels.is_empty() {
                            Label::primary(
                                span,
                                "acquisition participates in this lock-order cycle",
                            )
                        } else {
                            Label::secondary(span, "another acquisition or call in the cycle")
                        };
                        diagnostic = diagnostic.with_label(label);
                    }
                }
            }
            let present = group
                .iter()
                .filter_map(|node| classes.sides.get(node))
                .collect::<HashSet<_>>();
            let (aliased, separate): (Vec<&usize>, Vec<&usize>) = group
                .iter()
                .filter(|&&node| self.locks[node] && !classes.sides.contains_key(&node))
                .partition(|node| {
                    classes
                        .known
                        .get(node)
                        .is_some_and(|class| present.contains(class))
                });
            // A class in the cycle covers the known locks it may alias, so only
            // the classes and the remaining locks are certainly different.
            diagnostics.push(if present.len() + separate.len() == 1 {
                let diagnostic = diagnostic
                    .with_note("these locks cannot be told apart, so they may be the same lock: they were created at the same place, such as one factory function, or one has an unknown origin and shares the other's type")
                    .with_note("a lock reached only through another held lock, or the same lock taken twice, cannot be acquired together")
                    .with_help("if both locks are available before either is held, acquire them together: `lock first as a, second as b { ... }`");
                // Known locks can also be reversed against each other here.
                if aliased.len() > 1 {
                    diagnostic.with_help("if these locks also nest in opposite orders, use one consistent nesting order")
                } else {
                    diagnostic
                }
            } else {
                let diagnostic = diagnostic.with_help("use one consistent nesting order, or acquire the locks together in one `lock` statement");
                if present.is_empty() {
                    diagnostic
                } else {
                    diagnostic.with_note("a lock of unknown origin may be any lock of its type")
                }
            });
        }
        diagnostics
    }
}

struct Analysis<'a> {
    types: &'a HashMap<ExprId, Type>,
    /// Declaration identity of a type, independent of import spelling.
    identity: &'a dyn Fn(&str) -> String,
    /// Lock class node to value-type key, for every class with a concrete type.
    lock_types: HashMap<usize, Type>,
    graph: Graph,
    functions: HashMap<FunctionId, (usize, bool)>,
    dispatch: HashMap<FunctionId, usize>,
    classes: HashMap<Class, usize>,
    globals: HashMap<(String, String), Identity>,
    opaque: usize,
    diagnostics: Vec<Diagnostic>,
    external: HashMap<FunctionId, usize>,
}

impl Analysis<'_> {
    fn identity(&self, expr: &Expr, locals: &HashMap<String, Identity>) -> Identity {
        match expr {
            Expr::Var(name, ..) => locals.get(name).cloned().unwrap_or_default(),
            Expr::StaticField(field) => self
                .globals
                .get(&(field.class.clone(), field.field.clone()))
                .cloned()
                .unwrap_or_default(),
            Expr::StaticCall(call)
                if matches!(call.class.as_str(), "Mutex" | "RwLock") && call.method == "new" =>
            {
                Identity {
                    class: Some(Class::Allocation(expr.id())),
                    same: Some(expr.span()),
                }
            }
            // A field path or collection element alone is not a disjointness
            // proof: two different fields may store the same lock handle.
            _ => Identity::default(),
        }
    }

    /// The target's identity and type key. A body's own type parameters can
    /// instantiate to any lock type, so such a target has no provable class.
    fn target_identity(
        &self,
        target: &Expr,
        locals: &HashMap<String, Identity>,
        generics: &HashSet<&str>,
    ) -> (Identity, Option<Type>) {
        let mut identity = self.identity(target, locals);
        let Some(ty) = self.types.get(&target.id()) else {
            // Generic templates are not typed here, so a known lock in one
            // could not join its type's order.
            if !generics.is_empty() {
                identity.class = None;
            }
            return (identity, None);
        };
        let key = type_key(ty, generics, self.identity);
        match &key {
            None => identity.class = None,
            Some(key) if identity.class.is_none() => {
                identity.class = Some(Class::Type(key.clone()));
            }
            Some(_) => {}
        }
        (identity, key)
    }

    fn class_node(&mut self, class: Class) -> usize {
        *self
            .classes
            .entry(class)
            .or_insert_with(|| self.graph.node(true))
    }

    fn target_node(&mut self, id: FunctionId) -> Option<usize> {
        if let Some(&(node, asynchronous)) = self.functions.get(&id) {
            return (!asynchronous).then_some(node);
        }
        if crate::compiler_db::effects::intrinsic_effects(&id).is_some() {
            None
        } else {
            Some(
                *self
                    .external
                    .entry(id)
                    .or_insert_with(|| self.graph.node(false)),
            )
        }
    }

    fn call_node(&mut self, expr: &Expr, locals: &HashMap<String, Identity>) -> Option<usize> {
        match expr {
            Expr::Call(call) if locals.contains_key(&call.callee) => Some(self.opaque),
            Expr::Call(call) => self.target_node(FunctionId::free_from_source_name(&call.callee)),
            Expr::StaticCall(call)
                if matches!(call.class.as_str(), "Mutex" | "RwLock") && call.method == "new" =>
            {
                None
            }
            Expr::StaticCall(call) => self.target_node(FunctionId::method(
                TypeId::from_source_name(&call.class),
                &call.method,
            )),
            Expr::MethodCall(call) => {
                let Some(Type::Named(owner) | Type::Generic(owner, _)) =
                    self.types.get(&call.object.id())
                else {
                    return Some(self.opaque);
                };
                let id = FunctionId::method(TypeId::from_source_name(owner), &call.method);
                self.dispatch
                    .get(&id)
                    .copied()
                    .or_else(|| self.target_node(id))
            }
            Expr::New(call) => self.target_node(FunctionId::method(
                TypeId::from_source_name(&call.class_name),
                "init",
            )),
            _ => None,
        }
    }

    fn reject(&mut self, span: Span, outer: Span, message: &str) {
        self.diagnostics.push(Diagnostic::new(Severity::Error, ErrorCode::E2605, message)
            .with_label(Label::primary(span, "cannot prove a distinct, consistently ordered acquisition"))
            .with_label(Label::secondary(outer, "the enclosing acquisition is held here"))
            .with_help("acquire possibly aliasing locks together; acquiring the same lock twice is never allowed"));
    }

    fn body(&mut self, owner: usize, body: &Block, params: &[Param], generics: &HashSet<&str>) {
        let mut locals = HashMap::new();
        for parameter in params {
            locals.insert(
                parameter.name.clone(),
                Identity {
                    class: None,
                    same: Some(parameter.span),
                },
            );
        }
        let mut undo: Vec<(String, Option<Identity>)> = Vec::new();
        let mut scopes = Vec::new();
        let mut held: Vec<(Vec<usize>, bool, Span)> = Vec::new();
        let mut walk = AstWalk::new(AstEvent::Block(body));
        while let Some(event) = walk.next() {
            match event {
                AstEvent::EnterScope => scopes.push(undo.len()),
                AstEvent::ExitScope => {
                    let mark = scopes.pop().unwrap_or(0);
                    while undo.len() > mark {
                        let (name, old) = undo.pop().unwrap();
                        if let Some(old) = old {
                            locals.insert(name, old);
                        } else {
                            locals.remove(&name);
                        }
                    }
                }
                AstEvent::Bind(name) => {
                    let old = locals.insert(name.to_owned(), Identity::default());
                    undo.push((name.to_owned(), old));
                }
                AstEvent::ExitStmt(Stmt::Let(binding)) => {
                    // Mutable handles can change in branches/loops: no single
                    // source-order scan is a sound identity proof for them.
                    let value = if binding.mutable {
                        Identity::default()
                    } else {
                        self.identity(&binding.init, &locals)
                    };
                    locals.insert(binding.name.clone(), value);
                }
                AstEvent::Expr(Expr::Lambda(_)) => walk.skip_children(),
                AstEvent::Expr(expr) if !held.is_empty() => {
                    if let Some(callee) = self.call_node(expr, &locals) {
                        // A call alone is never rejected. `lock` is async-only (E2603) and
                        // a critical section cannot await (E2604), so a same-stack callee
                        // cannot acquire a lock in a program that passes those checks.
                        let (nodes, ..) = held.last().cloned().unwrap();
                        for node in nodes {
                            self.graph.edge(node, callee, expr.span());
                        }
                    }
                }
                AstEvent::Stmt(stmt @ Stmt::Lock(lock)) => {
                    walk.skip_children();
                    let targets: Vec<_> = std::iter::once((&lock.target, lock.target_span))
                        .chain(
                            lock.additional
                                .iter()
                                .map(|target| (&target.target, target.target_span)),
                        )
                        .collect();
                    // Target expressions execute before this acquisition; only
                    // outer locks are held while evaluating them.
                    for &(target, _) in &targets {
                        let mut expressions = AstWalk::new(AstEvent::Expr(target));
                        while let Some(event) = expressions.next() {
                            match event {
                                AstEvent::Expr(Expr::Lambda(_)) => expressions.skip_children(),
                                AstEvent::Expr(expr) => {
                                    if let Some(callee) = self.call_node(expr, &locals)
                                        && let Some((nodes, ..)) = held.last().cloned()
                                    {
                                        for node in nodes {
                                            self.graph.edge(node, callee, expr.span());
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    let identities: Vec<_> = targets
                        .iter()
                        .map(|(target, _)| self.target_identity(target, &locals, generics))
                        .collect();
                    let mut duplicates = HashSet::new();
                    for ((identity, _), &(_, span)) in identities.iter().zip(&targets) {
                        if let Some(same) = identity.same
                            && !duplicates.insert(same)
                        {
                            self.reject(
                                span,
                                lock.target_span,
                                "the same lock is acquired more than once",
                            );
                        }
                    }
                    let unknown = identities
                        .iter()
                        .any(|(identity, _)| identity.class.is_none());
                    let mut nodes = Vec::new();
                    for ((identity, key), &(_, span)) in identities.into_iter().zip(&targets) {
                        let Some(class) = identity.class else {
                            self.graph.edge(owner, self.opaque, span);
                            continue;
                        };
                        let node = self.class_node(class);
                        self.graph.edge(owner, node, span);
                        nodes.push(node);
                        if let Some(key) = key {
                            self.lock_types.insert(node, key);
                        }
                    }
                    if let Some((outer, outer_unknown, span)) = held.last().cloned() {
                        if unknown || outer_unknown {
                            self.reject(
                                lock.header_span(),
                                span,
                                "lock identity or ordering is unknown for nested acquisition",
                            );
                        }
                        // Adjacent scopes imply all outer-to-inner constraints
                        // transitively. Never repeat the entire held prefix.
                        for &from in &outer {
                            for &to in &nodes {
                                self.graph.edge(from, to, lock.header_span());
                            }
                        }
                    }
                    held.push((nodes, unknown, lock.header_span()));
                    // Children were skipped to order target evaluation, so the
                    // binding scope must be replayed: bindings hide outer names.
                    walk.push(AstEvent::ExitStmt(stmt));
                    walk.push(AstEvent::ExitScope);
                    walk.push(AstEvent::Block(&lock.body));
                    for member in lock.additional.iter().rev() {
                        walk.push(AstEvent::Bind(&member.binding));
                    }
                    walk.push(AstEvent::Bind(&lock.binding));
                    walk.push(AstEvent::EnterScope);
                }
                AstEvent::ExitStmt(Stmt::Lock(_)) => {
                    held.pop();
                }
                _ => {}
            }
        }
    }
}

/// Keys come from declaration identity, so one type has one key in every unit
/// and under every spelling. Soundness depends on this: split keys would hide a
/// real cycle. Type parameters have no key because they stand for any type.
fn type_key(
    ty: &Type,
    generics: &HashSet<&str>,
    identity: &dyn Fn(&str) -> String,
) -> Option<Type> {
    let mut generic = false;
    let key = ty.map_names(|name| {
        generic |= generics.contains(name.as_str());
        identity(name)
    });
    (!generic).then_some(key)
}

pub(crate) fn collect(
    program: &Program,
    types: &HashMap<ExprId, Type>,
    identity: &dyn Fn(&str) -> String,
    calls: &CallGraph,
) -> Inputs {
    let mut graph = Graph::default();
    let opaque = graph.node(false);
    let mut analysis = Analysis {
        types,
        identity,
        lock_types: HashMap::new(),
        graph,
        functions: HashMap::new(),
        dispatch: HashMap::new(),
        classes: HashMap::new(),
        globals: HashMap::new(),
        opaque,
        diagnostics: Vec::new(),
        external: HashMap::new(),
    };
    let mut bodies = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(function) => bodies.push((
                FunctionId::free(&function.name),
                function.is_async,
                &function.body,
                function.params.as_slice(),
                function.type_params.iter().map(String::as_str).collect(),
            )),
            Item::Class(class) => {
                for field in &class.fields {
                    if field.is_static && !field.is_mut && field.initializer.as_ref().is_some_and(|expr| matches!(expr, Expr::StaticCall(call) if matches!(call.class.as_str(), "Mutex" | "RwLock") && call.method == "new")) {
                        analysis.globals.insert((class.name.clone(), field.name.clone()), Identity {
                            class: Some(Class::Global(field.span)), same: Some(field.span),
                        });
                    }
                }
                for method in &class.methods {
                    bodies.push((
                        FunctionId::method(TypeId::local(&class.name), &method.name),
                        method.is_async,
                        &method.body,
                        method.params.as_slice(),
                        class.type_params.iter().map(String::as_str).collect(),
                    ));
                }
                for constructor in &class.constructors {
                    bodies.push((
                        FunctionId::method(TypeId::local(&class.name), "init"),
                        false,
                        &constructor.body,
                        constructor.params.as_slice(),
                        class.type_params.iter().map(String::as_str).collect(),
                    ));
                }
            }
            Item::Interface(_) | Item::Enum(_) => {}
        }
    }
    for &(id, asynchronous, ..) in &bodies {
        analysis
            .functions
            .entry(id)
            .or_insert_with(|| (analysis.graph.node(false), asynchronous));
    }
    let hierarchy = ClassHierarchy::from_program(program);
    for (_, sites) in calls.iter() {
        for &id in &sites.virtual_calls {
            if analysis.dispatch.contains_key(&id) {
                continue;
            }
            let node = analysis.graph.node(false);
            analysis.dispatch.insert(id, node);
            let targets = id.owner().map_or_else(Vec::new, |owner| {
                hierarchy.dispatch_targets(&owner, &id.name())
            });
            if targets.is_empty()
                && let Some(target) = analysis.target_node(id)
            {
                analysis.graph.edge(node, target, Span::dummy());
            }
            for target in targets {
                if let Some(target) = analysis.target_node(target) {
                    analysis.graph.edge(node, target, Span::dummy());
                }
            }
        }
    }
    for (&id, sites) in calls.iter() {
        let Some(&(caller, _)) = analysis.functions.get(&id) else {
            continue;
        };
        for &target in &sites.targets {
            if let Some(target_node) = analysis.target_node(target) {
                analysis.graph.edge(
                    caller,
                    target_node,
                    sites
                        .call_spans
                        .get(&target)
                        .copied()
                        .unwrap_or_else(Span::dummy),
                );
            }
        }
        if sites.has_unknown {
            analysis.graph.edge(
                caller,
                opaque,
                sites.unknown_span.unwrap_or_else(Span::dummy),
            );
        }
    }
    for (id, _, body, params, generics) in bodies {
        let owner = analysis.functions[&id].0;
        analysis.body(owner, body, params, &generics);
    }
    Inputs {
        graph: analysis.graph,
        functions: analysis.functions,
        classes: analysis.classes,
        external: analysis.external,
        lock_types: analysis.lock_types,
        diagnostics: analysis.diagnostics,
    }
}

/// Owned session-local facts; no borrowed syntax and no persistent stale proof.
#[derive(Clone)]
pub(crate) struct Inputs {
    graph: Graph,
    functions: HashMap<FunctionId, (usize, bool)>,
    classes: HashMap<Class, usize>,
    external: HashMap<FunctionId, usize>,
    lock_types: HashMap<usize, Type>,
    diagnostics: Vec<Diagnostic>,
}

/// Resolve cross-unit source callables using the compiler's canonical import
/// map. Async callees intentionally contribute no same-stack acquisition edge.
pub(crate) fn analyze_all(
    units: Vec<(crate::module::UnitId, Inputs)>,
    resolve: impl Fn(crate::module::UnitId, FunctionId) -> Option<(crate::module::UnitId, FunctionId)>,
) -> Vec<Diagnostic> {
    let mut graph = Graph::default();
    let mut functions = HashMap::new();
    let mut parts = Vec::new();
    let mut diagnostics = Vec::new();
    let mut equal_classes: HashMap<Class, usize> = HashMap::new();
    let mut class_aliases = Vec::new();
    let mut typed_nodes: HashMap<Type, Vec<usize>> = HashMap::new();
    for (unit, input) in units {
        let offset = graph.edges.len();
        for (&id, &(node, asynchronous)) in &input.functions {
            functions.insert((unit, id), (offset + node, asynchronous));
        }
        for (class, node) in input.classes {
            let node = offset + node;
            if let Some(&prior) = equal_classes.get(&class) {
                class_aliases.push(vec![prior, node]);
            } else {
                equal_classes.insert(class, node);
            }
        }
        for (node, key) in input.lock_types {
            typed_nodes.entry(key).or_default().push(offset + node);
        }
        diagnostics.extend(input.diagnostics);
        graph.locks.extend(input.graph.locks);
        graph
            .edges
            .extend(input.graph.edges.into_iter().map(|edges| {
                edges
                    .into_iter()
                    .map(|(to, span)| (offset + to, span))
                    .collect()
            }));
        parts.push((unit, offset, input.external));
    }
    for (unit, offset, external) in parts {
        for (target, node) in external {
            let resolved = resolve(unit, target).and_then(|key| functions.get(&key).copied());
            // Unresolved targets are builtins or foreign code, neither of which
            // can run a Willow `lock` statement on this stack.
            if let Some((target, false)) = resolved {
                graph.edge(offset + node, target, Span::dummy());
            }
        }
    }
    let roots = collapse_groups(&mut graph, &class_aliases);
    let mut stand_ins = typed_nodes
        .into_iter()
        .filter_map(|(key, nodes)| {
            let class = *equal_classes.get(&Class::Type(key))?;
            Some((
                roots[class],
                nodes.into_iter().map(|node| roots[node]).collect(),
            ))
        })
        .collect();
    order_stand_ins(&mut stand_ins);
    let mut type_classes = TypeClasses::default();
    for (class, nodes) in stand_ins {
        graph.stand_for(class, nodes.into_iter(), &mut type_classes);
    }
    diagnostics.extend(graph.diagnostics(&type_classes));
    diagnostics
}

// Map iteration order must not reach the graph: edge order decides which
// acquisition a diagnostic reports first. O(L log L) for L typed locks.
fn order_stand_ins(stand_ins: &mut Vec<(usize, Vec<usize>)>) {
    stand_ins.sort_unstable_by_key(|&(class, _)| class);
    for (_, nodes) in stand_ins {
        nodes.sort_unstable();
        nodes.dedup();
    }
}

// Merge source-identical classes shared by more than one unit snapshot.
// A star per class plus component traversal is O(V + memberships).
fn collapse_groups(graph: &mut Graph, groups: &[Vec<usize>]) -> Vec<usize> {
    let mut adjacent = vec![Vec::new(); graph.edges.len()];
    for group in groups {
        if let Some((&first, rest)) = group.split_first() {
            for &node in rest {
                adjacent[first].push(node);
                adjacent[node].push(first);
            }
        }
    }
    let mut roots = vec![usize::MAX; graph.edges.len()];
    for start in 0..roots.len() {
        if roots[start] != usize::MAX {
            continue;
        }
        roots[start] = start;
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            for &next in &adjacent[node] {
                if roots[next] == usize::MAX {
                    roots[next] = start;
                    stack.push(next);
                }
            }
        }
    }
    let old = std::mem::replace(&mut graph.edges, vec![Vec::new(); roots.len()]);
    for (node, edges) in old.into_iter().enumerate() {
        graph.edges[roots[node]].extend(edges.into_iter().map(|(to, span)| (roots[to], span)));
    }
    roots
}

pub(super) fn analyze(
    program: &Program,
    types: &HashMap<ExprId, Type>,
    identity: &dyn Fn(&str) -> String,
    calls: &CallGraph,
) -> Vec<Diagnostic> {
    analyze_all(
        vec![(
            crate::module::UnitId::ENTRY,
            collect(program, types, identity, calls),
        )],
        |_, _| None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(source: &str) -> Vec<Diagnostic> {
        let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let calls = super::super::TypeChecker::resolved_effect_graph(&program);
        analyze(&program, &HashMap::new(), &str::to_owned, &calls)
    }

    fn local(body: &str) -> Vec<Diagnostic> {
        check(&format!(
            "async fn main() {{ let a = Mutex::new(1); let b = Mutex::new(2); let c = Mutex::new(3); {body} }}"
        ))
    }

    #[test]
    fn lock_order_01_distinct_allocations() {
        assert!(local("lock a as x { lock b as y {} }").is_empty());
    }
    #[test]
    fn lock_order_02_repeated_same_order() {
        assert!(local("lock a as x { lock b as y {} } lock a as x { lock b as y {} }").is_empty());
    }
    #[test]
    fn lock_order_03_reverse_order_cycle() {
        let errors = local("lock a as x { lock b as y {} } lock b as x { lock a as y {} }");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].labels.len() >= 2);
    }
    #[test]
    fn lock_order_04_three_class_cycle() {
        assert!(!local("lock a as x { lock b as y {} } lock b as x { lock c as y {} } lock c as x { lock a as y {} }").is_empty());
    }
    #[test]
    fn lock_order_05_same_handle_reacquisition() {
        assert!(!local("lock a as x { lock a as y {} }").is_empty());
    }
    #[test]
    fn lock_order_06_alias_reacquisition() {
        assert!(!local("let alias = a; lock a as x { lock alias as y {} }").is_empty());
    }
    #[test]
    fn lock_order_07_read_read_reacquisition() {
        assert!(!check("async fn main() { let a = RwLock::new(1); lock read a as x { lock read a as y {} } }").is_empty());
    }
    #[test]
    fn lock_order_08_distinct_read_locks() {
        assert!(check("async fn main() { let a = RwLock::new(1); let b = RwLock::new(2); lock read a as x { lock read b as y {} } }").is_empty());
    }
    #[test]
    fn lock_order_09_opaque_parameters() {
        assert!(
            !check("async fn f(a: Mutex<i64>, b: Mutex<i64>) { lock a as x { lock b as y {} } }")
                .is_empty()
        );
    }
    #[test]
    fn lock_order_10_single_opaque_lock() {
        assert!(check("async fn f(a: Mutex<i64>) { lock a as x {} }").is_empty());
    }
    #[test]
    fn lock_order_11_grouped_opaque_parameters() {
        assert!(
            check("async fn f(a: Mutex<i64>, b: Mutex<i64>) { lock a as x, b as y {} }").is_empty()
        );
    }
    #[test]
    fn lock_order_12_grouped_duplicate_parameter() {
        assert!(!check("async fn f(a: Mutex<i64>) { lock a as x, a as y {} }").is_empty());
    }
    #[test]
    fn lock_order_13_grouped_allocations() {
        assert!(local("lock a as x, b as y {}").is_empty());
    }
    #[test]
    fn lock_order_14_grouped_alias_duplicate() {
        assert!(!local("let alias = a; lock a as x, alias as y {}").is_empty());
    }
    #[test]
    fn lock_order_15_group_then_inner() {
        assert!(local("lock a as x, b as y { lock c as z {} }").is_empty());
    }
    #[test]
    fn lock_order_16_group_reacquired_inside() {
        assert!(!local("lock a as x, b as y { lock a as z {} }").is_empty());
    }
    #[test]
    fn lock_order_17_group_order_constraint_cycle() {
        assert!(
            !local("lock a as x, b as y { lock c as z {} } lock c as z { lock b as y {} }")
                .is_empty()
        );
    }
    #[test]
    fn lock_order_18_mutable_alias_is_conservative() {
        assert!(
            !local("let mut alias = b; if true { alias = a; } lock a as x { lock alias as y {} }")
                .is_empty()
        );
    }
    #[test]
    fn lock_order_19_sequential_same_handle() {
        assert!(local("lock a as x {} lock a as y {}").is_empty());
    }
    #[test]
    fn lock_order_20_shadowed_distinct_allocation() {
        assert!(local("lock a as x { let a = Mutex::new(4); lock a as y {} }").is_empty());
    }
    #[test]
    fn lock_order_21_deep_acyclic_nesting() {
        assert!(local("lock a as x { lock b as y { lock c as z {} } }").is_empty());
    }
    #[test]
    fn lock_order_22_async_task_creation_is_not_acquisition() {
        assert!(check("async fn worker(a: Mutex<i64>) { lock a as x {} } async fn main() { let a = Mutex::new(1); lock a as x { let task = worker(a); } }").is_empty());
    }
    #[test]
    fn lock_order_23_recursive_helpers_without_locks() {
        let mut graph = Graph::default();
        let a = graph.node(false);
        let b = graph.node(false);
        graph.edge(a, b, Span::dummy());
        graph.edge(b, a, Span::dummy());
        assert!(graph.diagnostics(&TypeClasses::default()).is_empty());
    }
    #[test]
    fn lock_order_stand_ins_ignore_map_iteration_order() {
        let mut first = vec![(7, vec![9, 3, 9]), (2, vec![5, 4])];
        let mut second = vec![(2, vec![4, 5, 4]), (7, vec![3, 9])];
        order_stand_ins(&mut first);
        order_stand_ins(&mut second);
        assert_eq!(first, vec![(2, vec![4, 5]), (7, vec![3, 9])]);
        assert_eq!(first, second);
    }
    #[test]
    fn lock_order_27_call_under_opaque_lock_is_accepted() {
        assert!(
            check(
                "fn helper(n: i64) -> i64 { return n + 1; } \
                 async fn f(m: Mutex<i64>) { lock m as mut x { x = helper(x); } }"
            )
            .is_empty()
        );
    }
    #[test]
    fn lock_order_28_function_value_under_opaque_lock_is_accepted() {
        assert!(
            check("async fn f(m: Mutex<i64>, g: fn(i64) -> i64) { lock m as mut x { x = g(x); } }")
                .is_empty()
        );
    }
    #[test]
    fn lock_order_29_virtual_call_under_lock_is_accepted() {
        assert!(
            check(
                "interface Step { fn step(n: i64) -> i64; } \
                 async fn f(m: Mutex<i64>, s: Step) { lock m as mut x { x = s.step(x); } }"
            )
            .is_empty()
        );
    }
    #[test]
    fn lock_order_30_cycle_across_functions_uses_shared_classes() {
        let errors = check(
            "class L { static a: Mutex<i64> = Mutex::new(1); static b: Mutex<i64> = Mutex::new(2); } \
             async fn p() { lock L::a as x { lock L::b as y {} } } \
             async fn q() { lock L::b as x { lock L::a as y {} } }",
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].labels.len() >= 2);
    }
    #[test]
    fn lock_order_31_lock_binding_shadows_outer_allocation() {
        assert!(
            !local(
                "let y = Mutex::new(9); let h = Mutex::new(a); \
                 lock a as v { lock h as y { lock y as w {} } }"
            )
            .is_empty()
        );
    }
    #[test]
    fn lock_order_32_group_binding_shadows_outer_allocation() {
        assert!(
            !local(
                "let y = Mutex::new(9); let h = Mutex::new(a); \
                 lock b as z, h as y { lock y as w {} }"
            )
            .is_empty()
        );
    }
    #[test]
    fn lock_order_33_outer_identity_returns_after_lock_scope() {
        assert!(
            local(
                "let y = Mutex::new(9); let h = Mutex::new(a); \
                 lock h as y {} lock b as x { lock y as w {} }"
            )
            .is_empty()
        );
    }
    #[test]
    fn lock_order_34_type_class_expansion_stays_linear() {
        for n in [32, 128, 512, 2048] {
            let mut graph = Graph::default();
            let class = graph.node(true);
            let locks: Vec<_> = (0..n).map(|_| graph.node(true)).collect();
            for pair in locks.windows(2) {
                graph.edge(pair[0], pair[1], Span::new(1, 2, 1, 1));
            }
            graph.edge(class, class, Span::new(3, 4, 1, 3));
            let mut classes = TypeClasses::default();
            graph.stand_for(class, locks.into_iter(), &mut classes);
            let edges = (n - 1) + 2 + 2 * n;
            let errors = graph.diagnostics(&classes);
            assert_eq!(errors.len(), 1);
            assert!(graph.operations <= 3 * (n + 2) + 4 * edges);
            println!("type class locks={n} operations={}", graph.operations);
        }
    }

    #[test]
    fn lock_order_24_helper_chain_cycle_witness() {
        let mut graph = Graph::default();
        let a = graph.node(true);
        let helper = graph.node(false);
        let b = graph.node(true);
        graph.edge(a, helper, Span::new(1, 2, 1, 1));
        graph.edge(helper, b, Span::new(3, 4, 1, 3));
        graph.edge(b, a, Span::new(5, 6, 1, 5));
        let errors = graph.diagnostics(&TypeClasses::default());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].labels.len(), 3);
    }
    #[test]
    fn lock_order_25_linear_graph_operation_counts() {
        for n in [32, 128, 512, 2048] {
            for cycle in [false, true] {
                let mut graph = Graph::default();
                for index in 0..n {
                    graph.node(index % 3 == 0);
                }
                for index in 1..n {
                    graph.edge(index - 1, index, Span::dummy());
                }
                if cycle {
                    graph.edge(n - 1, 0, Span::dummy());
                }
                let edges = n - 1 + usize::from(cycle);
                let errors = graph.diagnostics(&TypeClasses::default());
                assert_eq!(errors.len(), usize::from(cycle));
                assert!(graph.operations <= 3 * n + 4 * edges);
                println!(
                    "lock graph nodes={n} edges={edges} cycle={cycle} operations={}",
                    graph.operations
                );
            }
        }
    }
    #[test]
    fn lock_order_26_fanout_and_repeated_edges_are_linear() {
        for n in [32, 128, 512] {
            let mut graph = Graph::default();
            for index in 0..n {
                graph.node(index % 2 == 0);
            }
            for child in 1..n {
                graph.edge(0, child, Span::dummy());
                graph.edge(0, child, Span::dummy());
            }
            assert!(graph.diagnostics(&TypeClasses::default()).is_empty());
            assert!(graph.operations <= 3 * n + 4 * (2 * (n - 1)));
        }
    }
}
