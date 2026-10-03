//! Build-wide initialization planning over the shared typed call graph.
//!
//! Keep helper nodes in the graph: materializing a transitive set of statics
//! for every helper/initializer would turn a shared chain into quadratic work.
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
};

use super::{effects::EffectQueries, ids::StaticId};
use crate::{
    diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span},
    module::UnitId,
    parser::ast::{Item, Program, Type, TypePath},
    semantic::{
        call_graph::CallGraph,
        ids::{FunctionId, TypeId},
    },
};

pub(crate) fn initializer_id(class: &str, field: &str) -> FunctionId {
    FunctionId::method(
        TypeId::from_source_name(class),
        format!("$static_init.{field}"),
    )
}

struct Static {
    id: StaticId,
    callable: FunctionId,
    name: String,
    span: Span,
}
struct DispatchType {
    is_interface: bool,
    id: TypeId,
    parents: Vec<TypeId>,
    methods: Vec<FunctionId>,
}
struct Unit {
    dispatch_types: Vec<DispatchType>,
    graph: CallGraph,
    statics: Vec<Static>,
    imports: HashMap<String, String>,
}
#[derive(Default)]
pub(crate) struct StaticInitQueries {
    units: RefCell<HashMap<UnitId, Unit>>,
    plan: RefCell<Option<std::sync::Arc<[StaticId]>>>,
}

impl StaticInitQueries {
    pub(crate) fn record(
        &self,
        unit: UnitId,
        program: &Program,
        calls: &CallGraph,
        reads: &HashMap<FunctionId, HashSet<FunctionId>>,
        imports: &HashMap<String, String>,
        index: Option<&super::ids::BodyIndex>,
    ) {
        let mut graph = calls.clone();
        for (&caller, targets) in reads {
            graph.merge(
                caller,
                crate::semantic::call_graph::CallSites {
                    unsupported_initialization: Default::default(),
                    virtual_calls: Default::default(),
                    targets: targets.iter().copied().collect(),
                    has_unknown: false,
                },
            );
        }
        let mut statics = Vec::new();
        let mut dispatch_types = Vec::new();
        for item in &program.items {
            if let Item::Interface(interface) = item {
                let owner = TypeId::local(&interface.name);
                dispatch_types.push(DispatchType {
                    is_interface: true,
                    id: owner,
                    parents: interface
                        .extends
                        .iter()
                        .map(|name| TypeId::from_source_name(name))
                        .collect(),
                    methods: Vec::new(),
                });
            }
            let Item::Class(class) = item else { continue };
            let owner = TypeId::local(&class.name);
            let base = class.base_class.as_ref().map(|base| match base {
                TypePath::Local(name) => TypeId::from_source_name(name),
                TypePath::Qualified(parts) => TypeId::from_source_name(&parts.join("::")),
            });
            dispatch_types.push(DispatchType {
                is_interface: false,
                id: owner,
                parents: base
                    .into_iter()
                    .chain(
                        class
                            .implements
                            .iter()
                            .take(class.source_implements_len)
                            .filter_map(|ty| match ty {
                                Type::Named(name) | Type::Generic(name, _) => {
                                    Some(TypeId::from_source_name(name))
                                }
                                _ => None,
                            }),
                    )
                    .collect(),
                methods: class
                    .methods
                    .iter()
                    .filter(|method| !method.is_static)
                    .map(|method| FunctionId::method(owner, &method.name))
                    .collect(),
            });
            for (field_index, field) in class.fields.iter().enumerate() {
                if !field.is_static || field.initializer.is_none() {
                    continue;
                }
                let callable = initializer_id(&class.name, &field.name);
                graph.merge(callable, Default::default());
                statics.push(Static {
                    id: index
                        .and_then(|bodies| {
                            bodies.static_id(field.initializer.as_ref().unwrap().id())
                        })
                        .unwrap_or(StaticId {
                            unit,
                            owner: TypeId::local(&class.name),
                            index: field_index as u32,
                        }),
                    callable,
                    name: format!("{}::{}", class.name, field.name),
                    span: field.span,
                });
            }
        }
        self.units.borrow_mut().insert(
            unit,
            Unit {
                dispatch_types,
                graph,
                statics,
                imports: imports.clone(),
            },
        );
        *self.plan.borrow_mut() = None;
    }

    pub(crate) fn order(&self) -> Option<std::sync::Arc<[StaticId]>> {
        self.plan.borrow().clone()
    }

    /// `order` is the resolver's stable dependency-first unit order. Nodes and
    /// edges are indexed once. SCC detection, witness construction and the
    /// dependency-first walk are iterative and linear in the combined graph.
    pub(crate) fn solve(&self, order: &[UnitId], effects: &EffectQueries) -> Vec<Diagnostic> {
        if self.plan.borrow().is_some() {
            return Vec::new();
        }
        let mut units = self.units.borrow_mut();
        if units.values().all(|unit| unit.statics.is_empty()) {
            *self.plan.borrow_mut() = Some(Vec::new().into());
            units.clear();
            return Vec::new();
        }
        let mut keys = Vec::new();
        let mut positions = HashMap::new();
        for &unit in order {
            if let Some(facts) = units.get(&unit) {
                for &id in facts.graph.ids() {
                    positions.insert((unit, id), keys.len());
                    keys.push((unit, id));
                }
            }
        }
        let mut edges = vec![Vec::new(); keys.len()];
        let mut unknown = vec![false; keys.len()];
        let mut unsupported = vec![None; keys.len()];
        for &unit in order {
            let Some(facts) = units.get(&unit) else {
                continue;
            };
            for (&id, calls) in facts.graph.iter() {
                let i = positions[&(unit, id)];
                unknown[i] = calls.has_unknown || !calls.unsupported_initialization.is_empty();
                unsupported[i] = calls.unsupported_initialization.first().copied();
                for target in &calls.targets {
                    let local = positions.get(&(unit, *target)).copied();
                    let external = || {
                        effects
                            .external_target(unit, target, &facts.imports)
                            .and_then(|key| positions.get(&key).copied())
                    };
                    if let Some(target) = local.or_else(external) {
                        edges[i].push(target);
                    }
                    // Bodyless builtins add no direct static reads. Callback
                    // APIs carry an explicit unsupported reason from checking;
                    // never infer callback safety just from the missing body.
                }
            }
        }
        connect_build_dispatch(order, &units, effects, &positions, &mut keys, &mut edges);
        unknown.resize(edges.len(), false);
        let mut statics = HashMap::new();
        let mut roots = Vec::new();
        for &unit in order {
            if let Some(facts) = units.get(&unit) {
                for item in &facts.statics {
                    let node = positions[&(unit, item.callable)];
                    statics.insert(node, item);
                    roots.push(node);
                }
            }
        }
        let (ordered, failure, _) = dependency_order(&edges, &roots, &unknown);
        if let Some(path) = failure {
            let root = path[0];
            let item = statics[&root];
            let cycle = path.len() > 1 && path.last() == Some(&root);
            let names: Vec<_> = path
                .iter()
                // Dispatch unions are analysis nodes, not executed bodies.
                .filter(|node| **node < positions.len())
                .map(|node| {
                    statics
                        .get(node)
                        .map(|s| s.name.clone())
                        .unwrap_or_else(|| keys[*node].1.to_string())
                })
                .collect();
            let message = if cycle {
                format!("static initialization cycle: {}", names.join(" -> "))
            } else {
                format!(
                    "unsupported static initialization through {}: {}",
                    unsupported[*path.last().expect("dependency path")]
                        .unwrap_or(
                            crate::semantic::call_graph::UnsupportedInitialization::UnresolvedCall
                        )
                        .description(),
                    names.join(" -> ")
                )
            };
            let mut diagnostic =
                Diagnostic::new(Severity::Error, ErrorCode::E0838, message).with_label(
                    Label::primary(item.span, "static initialization dependency"),
                );
            if !cycle {
                diagnostic = diagnostic.with_help("use a directly resolved helper or an explicit class method implementation; alternatively, perform this initialization after startup in main");
            }
            return vec![diagnostic];
        }
        *self.plan.borrow_mut() = Some(
            ordered
                .into_iter()
                .filter_map(|node| statics.get(&node).map(|s| s.id))
                .collect(),
        );
        // Only the compact plan survives into code generation. Typed-body
        // artifacts remain authoritative for a later revision's graph.
        units.clear();
        Vec::new()
    }
}

/// Shared union nodes represent (receiver type, method), rather than copying
/// every override into every call site. Inherited bodies have a separate memo:
/// dispatch through an interface must include a class's inherited implementation.
fn connect_build_dispatch(
    order: &[UnitId],
    units: &HashMap<UnitId, Unit>,
    effects: &EffectQueries,
    bodies: &HashMap<(UnitId, FunctionId), usize>,
    keys: &mut Vec<(UnitId, FunctionId)>,
    edges: &mut Vec<Vec<usize>>,
) {
    let mut types = Vec::new();
    let mut positions = HashMap::new();
    for &unit in order {
        if let Some(facts) = units.get(&unit) {
            for declaration in &facts.dispatch_types {
                positions.insert((unit, declaration.id), types.len());
                types.push((unit, declaration));
            }
        }
    }
    let resolve = |unit, id: FunctionId| {
        id.owner_type()
            .and_then(|owner| positions.get(&(unit, owner)).copied())
            .or_else(|| {
                effects
                    .external_target(unit, &id, &units[&unit].imports)
                    .and_then(|(unit, id)| positions.get(&(unit, id.owner_type()?)).copied())
            })
    };
    let mut children = vec![Vec::new(); types.len()];
    let mut bases = vec![None; types.len()];
    let mut methods = HashMap::new();
    for (index, &(unit, declaration)) in types.iter().enumerate() {
        for &parent in &declaration.parents {
            if let Some(parent) = resolve(unit, FunctionId::method(parent, "$type")) {
                children[parent].push(index);
                if !types[parent].1.is_interface && !declaration.is_interface {
                    bases[index] = Some(parent);
                }
            }
        }
        for &method in &declaration.methods {
            if let Some(&body) = bodies.get(&(unit, method)) {
                methods.insert((index, method.name().to_string()), body);
            }
        }
    }
    let mut unions = HashMap::new();
    let mut pending = Vec::new();
    let mut exact = HashMap::new();
    // Allocate each pair once; pending traversal also handles shared diamonds.
    let ensure = |ty: usize,
                  method: &str,
                  unions: &mut HashMap<(usize, String), usize>,
                  pending: &mut Vec<(usize, String, usize)>,
                  keys: &mut Vec<(UnitId, FunctionId)>,
                  edges: &mut Vec<Vec<usize>>| {
        *unions.entry((ty, method.to_owned())).or_insert_with(|| {
            let node = edges.len();
            edges.push(Vec::new());
            keys.push((types[ty].0, FunctionId::method(types[ty].1.id, method)));
            pending.push((ty, method.to_owned(), node));
            node
        })
    };
    for &unit in order {
        let Some(facts) = units.get(&unit) else {
            continue;
        };
        for (&caller, calls) in facts.graph.iter() {
            for &call in &calls.virtual_calls {
                if let Some(ty) = resolve(unit, call) {
                    let node = ensure(
                        ty,
                        call.name().as_ref(),
                        &mut unions,
                        &mut pending,
                        keys,
                        edges,
                    );
                    edges[bodies[&(unit, caller)]].push(node);
                }
            }
        }
    }
    while let Some((ty, method, node)) = pending.pop() {
        if !types[ty].1.is_interface
            && let Some(body) = inherited_body(ty, &method, &bases, &methods, &mut exact)
        {
            edges[node].push(body);
        }
        for &child in &children[ty] {
            let target = ensure(child, &method, &mut unions, &mut pending, keys, edges);
            edges[node].push(target);
        }
    }
}

fn inherited_body(
    ty: usize,
    method: &str,
    bases: &[Option<usize>],
    methods: &HashMap<(usize, String), usize>,
    cache: &mut HashMap<(usize, String), Option<usize>>,
) -> Option<usize> {
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(ty);
    let mut result = None;
    while let Some(ty) = current {
        let key = (ty, method.to_owned());
        if let Some(&body) = cache.get(&key) {
            result = body;
            break;
        }
        if !seen.insert(ty) {
            break;
        }
        path.push(key.clone());
        if let Some(&body) = methods.get(&key) {
            result = Some(body);
            break;
        }
        current = bases[ty];
    }
    for key in path {
        cache.insert(key, result);
    }
    result
}

/// Return a postorder, or a concrete cycle/indirect-call path starting at a
/// static. Helper-only recursion is legal; SCCs containing a static are not.
fn dependency_order(
    edges: &[Vec<usize>],
    roots: &[usize],
    unknown: &[bool],
) -> (Vec<usize>, Option<Vec<usize>>, usize) {
    let components = crate::semantic::effects::strongly_connected_components(edges);
    let mut sizes = vec![0; edges.len()];
    for &c in &components {
        sizes[c] += 1;
    }
    for &root in roots {
        let c = components[root];
        if sizes[c] == 1 && !edges[root].contains(&root) {
            continue;
        }
        // Find a path back to root inside its SCC, once for the reported cycle.
        let mut parent = vec![usize::MAX; edges.len()];
        let mut pending = vec![root];
        parent[root] = root;
        while let Some(node) = pending.pop() {
            for &next in &edges[node] {
                if components[next] != c {
                    continue;
                }
                if next == root {
                    let mut path = vec![node];
                    let mut cursor = node;
                    while cursor != root {
                        cursor = parent[cursor];
                        path.push(cursor);
                    }
                    path.reverse();
                    path.push(root);
                    return (Vec::new(), Some(path), 0);
                }
                if parent[next] == usize::MAX {
                    parent[next] = node;
                    pending.push(next);
                }
            }
        }
        unreachable!("cyclic SCC has a return path");
    }
    let mut walked_edges = 0;
    let mut seen = vec![false; edges.len()];
    let mut result = Vec::new();
    for &root in roots {
        if seen[root] {
            continue;
        }
        let mut stack = vec![(root, 0)];
        seen[root] = true;
        while let Some((node, offset)) = stack.last_mut() {
            if unknown[*node] {
                return (
                    Vec::new(),
                    Some(stack.iter().map(|(node, _)| *node).collect()),
                    walked_edges,
                );
            }
            if let Some(&child) = edges[*node].get(*offset) {
                *offset += 1;
                walked_edges += 1;
                if !seen[child] {
                    seen[child] = true;
                    stack.push((child, 0));
                }
            } else {
                result.push(*node);
                stack.pop();
            }
        }
    }
    (result, None, walked_edges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_dispatch_shares_queries_and_hierarchy_edges() {
        for n in [32usize, 128, 512, 2048] {
            for shape in ["chain", "wide", "diamond"] {
                let unit = UnitId::ENTRY;
                let owner = |i| TypeId::local(format!("T{i}"));
                let mut graph = CallGraph::default();
                let body_owner = if shape == "diamond" { n - 1 } else { 0 };
                let body = FunctionId::method(owner(body_owner), "seed");
                graph.merge(body, Default::default());
                let dispatch_types = (0..n)
                    .map(|i| {
                        let parents = if shape == "diamond" {
                            (i.saturating_sub(2)..i).map(owner).collect()
                        } else if i == 0 {
                            Vec::new()
                        } else {
                            vec![owner(if shape == "wide" { 0 } else { i - 1 })]
                        };
                        DispatchType {
                            is_interface: shape == "diamond" && i != n - 1,
                            id: owner(i),
                            parents,
                            methods: if i == body_owner {
                                vec![body]
                            } else {
                                Vec::new()
                            },
                        }
                    })
                    .collect();
                for i in 0..n {
                    graph.merge(
                        FunctionId::free(format!("caller{i}")),
                        crate::semantic::call_graph::CallSites {
                            unsupported_initialization: Default::default(),
                            virtual_calls: [FunctionId::method(owner(0), "seed")].into(),
                            ..Default::default()
                        },
                    );
                }
                let mut keys: Vec<_> = graph.ids().map(|&id| (unit, id)).collect();
                let positions = keys.iter().enumerate().map(|(i, &key)| (key, i)).collect();
                let mut edges = vec![Vec::new(); keys.len()];
                let units = [(
                    unit,
                    Unit {
                        dispatch_types,
                        graph,
                        statics: Vec::new(),
                        imports: HashMap::new(),
                    },
                )]
                .into();
                connect_build_dispatch(
                    &[unit],
                    &units,
                    &EffectQueries::default(),
                    &positions,
                    &mut keys,
                    &mut edges,
                );
                let count: usize = edges.iter().map(Vec::len).sum();
                assert_eq!(keys.len(), 2 * n + 1);
                assert_eq!(
                    count,
                    if shape == "diamond" {
                        3 * n - 2
                    } else {
                        3 * n - 1
                    }
                );
                let (_, failure, visits) = dependency_order(
                    &edges,
                    &(0..n + 1).collect::<Vec<_>>(),
                    &vec![false; edges.len()],
                );
                assert!(failure.is_none());
                assert_eq!(visits, count);
                eprintln!(
                    "build-dispatch shape={shape} types={n} calls={n} unions={n} edges={count} visits={visits}"
                );
            }
        }
    }

    #[test]
    fn inherited_body_memoizes_deep_chains() {
        for n in [32usize, 128, 512, 2048] {
            let bases: Vec<_> = (0..n).map(|i| i.checked_sub(1)).collect();
            let methods = [((0, "seed".to_owned()), 42)].into();
            let mut cache = HashMap::new();
            for i in (0..n).rev() {
                assert_eq!(
                    inherited_body(i, "seed", &bases, &methods, &mut cache),
                    Some(42)
                );
            }
            assert_eq!(cache.len(), n);
        }
    }

    #[test]
    fn shared_chains_and_fanout_visit_each_edge_once() {
        for n in [32, 128, 512, 2048] {
            for recursive in [false, true] {
                // n initializers share n helpers and one leaf static.
                let mut edges = vec![Vec::new(); 2 * n + 1];
                for successors in &mut edges[..n] {
                    successors.push(n);
                }
                for (i, successors) in edges.iter_mut().enumerate().take(2 * n).skip(n) {
                    successors.push(i + 1);
                }
                if recursive {
                    edges[2 * n - 1].push(n);
                }
                let roots: Vec<_> = (0..n).chain([2 * n]).collect();
                let (order, error, visited) =
                    dependency_order(&edges, &roots, &vec![false; edges.len()]);
                assert!(error.is_none());
                assert_eq!(order.len(), 2 * n + 1);
                assert_eq!(order[0], 2 * n);
                assert_eq!(visited, 2 * n + usize::from(recursive));
                eprintln!(
                    "static-plan n={n} recursive={recursive} nodes={} edges={visited} visits={visited}",
                    edges.len()
                );
            }
        }
    }

    #[test]
    fn deep_static_chain_and_cycle_have_linear_witnesses() {
        for n in [32, 128, 512, 2048] {
            let mut edges: Vec<_> = (0..n)
                .map(|i| if i + 1 < n { vec![i + 1] } else { vec![] })
                .collect();
            let roots: Vec<_> = (0..n).collect();
            let unknown = vec![false; n];
            let (order, error, visits) = dependency_order(&edges, &roots, &unknown);
            assert!(error.is_none());
            assert_eq!(order, roots.iter().rev().copied().collect::<Vec<_>>());
            assert_eq!(visits, n - 1);
            edges[n - 1].push(0);
            let (_, error, _) = dependency_order(&edges, &roots, &unknown);
            assert_eq!(error.unwrap(), (0..n).chain([0]).collect::<Vec<_>>());
        }
    }

    #[test]
    fn helper_recursion_does_not_hide_indirect_calls_or_static_cycles() {
        let edges = vec![vec![1], vec![2], vec![1, 3], vec![]];
        let (order, error, _) = dependency_order(&edges, &[0, 3], &[false; 4]);
        assert!(error.is_none());
        assert!(order.iter().position(|&n| n == 3) < order.iter().position(|&n| n == 0));
        let (_, error, _) = dependency_order(&edges, &[0, 3], &[false, false, true, false]);
        assert_eq!(error.unwrap(), vec![0, 1, 2]);
        let (_, error, _) = dependency_order(&[vec![1], vec![2], vec![1, 0]], &[0], &[false; 3]);
        assert_eq!(error.unwrap(), vec![0, 1, 2, 0]);
    }

    #[test]
    fn standalone_checker_reports_cycle() {
        let source = "class A { pub static x: i64 = B::y; } class B { pub static y: i64 = A::x; } fn main() {}";
        let (program, errors) =
            crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
                .parse();
        assert!(errors.is_empty());
        let mut checker = crate::semantic::TypeChecker::new();
        checker.check_program(&program);
        assert!(
            checker
                .errors
                .iter()
                .any(|d| d.code == ErrorCode::E0838 && d.message.contains("cycle"))
        );
    }
}
