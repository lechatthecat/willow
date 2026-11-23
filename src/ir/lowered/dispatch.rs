//! Virtual task effects. A class subtree contributes its declared async overrides.
//! An implementation edge additionally exposes bodies inherited from a class
//! outside the interface's descendant graph.
use super::{LoweringDeclarations, Type, TypeId};
use crate::ir::typed_ast::HirSignature;
use std::collections::HashMap;

type Methods = HashMap<String, HirSignature>;

// Empty branches share No. Methodless paths share the same predicate ID.
// Compound predicates form a DAG of integer links, including on deep hierarchies.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Effects {
    #[default]
    No,
    Open,
    Test(usize),
}

// A materialized index shares one declaration table. Union with a small
// table need only inspect its new async names, even if the base is very wide.
#[derive(Clone)]
struct MethodIndex<'a> {
    base: &'a Methods,
    changes: Option<im::HashMap<&'a str, bool>>,
}

impl<'a> MethodIndex<'a> {
    fn declaration(&self, method: &str) -> Option<bool> {
        self.changes
            .as_ref()
            .and_then(|c| c.get(method))
            .copied()
            .or_else(|| self.base.get(method).map(|m| m.is_async))
    }

    fn contains(&self, method: &str) -> bool {
        self.declaration(method) == Some(true)
    }

    fn set(&mut self, name: &'a str, value: bool) {
        if self.contains(name) == value {
            return;
        }
        if self.base.get(name).is_some_and(|m| m.is_async) == value {
            if let Some(changes) = &mut self.changes {
                changes.remove(name);
                if changes.is_empty() {
                    self.changes = None;
                }
            }
        } else {
            self.changes
                .get_or_insert_with(Default::default)
                .insert(name, value);
        }
    }

    fn changes_len(&self) -> usize {
        self.changes.as_ref().map_or(0, im::HashMap::len)
    }

    fn width(&self) -> usize {
        self.base.len() + self.changes_len()
    }

    // Nearer declarations win, including synchronous declarations that mask
    // an async method beyond the part of the ancestry indexed so far.
    fn inherit(&self, parent: &Self, unresolved: bool, budget: usize) -> Option<(Self, usize)> {
        let near_is_smaller = self.width() <= parent.width();
        let (small, large) = if near_is_smaller {
            (self, parent)
        } else {
            (parent, self)
        };
        let cost = small.width();
        if cost > budget {
            return None;
        }
        let mut result = large.clone();
        let changes = small
            .changes
            .iter()
            .flatten()
            .map(|(&name, &value)| (name, value));
        let declarations = small
            .base
            .iter()
            .filter(|(name, _)| {
                !small
                    .changes
                    .as_ref()
                    .is_some_and(|c| c.contains_key(name.as_str()))
            })
            .map(|(name, sig)| (name.as_str(), sig.is_async));
        for (name, enabled) in changes.chain(declarations) {
            if !near_is_smaller && self.declaration(name).is_some() {
                continue;
            }
            if unresolved && !enabled && !result.base.contains_key(name) {
                result
                    .changes
                    .get_or_insert_with(Default::default)
                    .insert(name, false);
            } else {
                result.set(name, enabled);
            }
        }
        Some((result, cost))
    }

    fn union(&self, other: &Self, budget: usize) -> Option<(Self, usize)> {
        // When both views borrow the same table, only changes to that table
        // can distinguish them. A negative override survives only in both.
        if std::ptr::eq(self.base, other.base) {
            let cost = self.changes_len() + other.changes_len();
            if cost > budget {
                return None;
            }
            let mut result = self.clone();
            for (name, enabled) in self.changes.iter().flatten() {
                if !enabled && other.contains(name) {
                    result.set(name, true);
                }
            }
            for (name, enabled) in other.changes.iter().flatten() {
                if *enabled {
                    result.set(name, true);
                }
            }
            return Some((result, cost));
        }

        let (large, small) = if self.width() >= other.width() {
            (self, other)
        } else {
            (other, self)
        };
        let cost = small.width();
        if cost > budget {
            return None;
        }
        let mut result = large.clone();
        for name in small.base.keys() {
            if small.contains(name) {
                result.set(name, true);
            }
        }
        for (name, enabled) in small.changes.iter().flatten() {
            if *enabled {
                result.set(name, true);
            }
        }
        Some((result, cost))
    }
}

enum Predicate<'a> {
    Indexed(MethodIndex<'a>),
    Override(MethodIndex<'a>, usize),
    Either(usize, usize),
    // A shared union with an indexed prefix and an unevaluated remainder.
    Prefix(MethodIndex<'a>, usize),
}

struct Node<'a> {
    test: Predicate<'a>,
    last: Option<(usize, bool)>,
    probes: usize,
    first_root: Option<usize>,
}

#[derive(Default)]
struct Predicates<'a> {
    nodes: Vec<Node<'a>>,
    tables: HashMap<*const Methods, usize>,
    // A compound reached only through one root already benefits from its root cache.
    answers: HashMap<usize, HashMap<usize, bool>>,
    spellings: HashMap<Box<str>, usize>,
    #[cfg(test)]
    indexed_entries_read: usize,
    #[cfg(test)]
    visits: usize,
    #[cfg(test)]
    fast_queries: usize,
}

impl<'a> Predicates<'a> {
    fn push(&mut self, test: Predicate<'a>) -> Effects {
        let id = self.nodes.len();
        self.nodes.push(Node {
            test,
            last: None,
            probes: 0,
            first_root: None,
        });
        Effects::Test(id)
    }

    fn declared(&mut self, methods: &'a Methods) -> Effects {
        if methods.is_empty() {
            return Effects::No;
        }
        let id = *self
            .tables
            .entry(std::ptr::from_ref(methods))
            .or_insert_with(|| {
                let id = self.nodes.len();
                self.nodes.push(Node {
                    test: Predicate::Indexed(MethodIndex {
                        base: methods,
                        changes: None,
                    }),
                    last: None,
                    probes: 0,
                    first_root: None,
                });
                id
            });
        Effects::Test(id)
    }

    fn extend(&mut self, base: Effects, methods: &'a Methods) -> Effects {
        if methods.is_empty() {
            return base;
        }
        match base {
            Effects::Test(id) => self.push(Predicate::Override(
                MethodIndex {
                    base: methods,
                    changes: None,
                },
                id,
            )),
            Effects::No | Effects::Open => self.declared(methods),
        }
    }

    fn either(&mut self, left: Effects, right: Effects) -> Effects {
        match (left, right) {
            (Effects::Open, _) | (_, Effects::Open) => Effects::Open,
            (Effects::No, other) | (other, Effects::No) => other,
            (Effects::Test(a), Effects::Test(b)) if a != b => self.push(Predicate::Either(a, b)),
            _ => left,
        }
    }

    fn contains(&mut self, effects: Effects, method: &str) -> bool {
        let root = match effects {
            Effects::No => return false,
            Effects::Open => return true,
            Effects::Test(id) => id,
        };
        if let Predicate::Indexed(index) = &self.nodes[root].test {
            #[cfg(test)]
            {
                self.fast_queries += 1;
            }
            return index.contains(method);
        }
        let next = self.spellings.len();
        let spelling = self.spellings.get(method).copied().unwrap_or_else(|| {
            self.spellings.insert(method.into(), next);
            next
        });
        if let Some(answer) = self.answers.get(&root).and_then(|a| a.get(&spelling)) {
            #[cfg(test)]
            {
                self.fast_queries += 1;
            }
            return *answer;
        }
        enum Work {
            Probe(usize),
            Either(usize, usize, usize),
            Copy(usize, usize),
        }
        let mut work = vec![Work::Probe(root)];
        while let Some(action) = work.pop() {
            let visited_tail = matches!(action, Work::Copy(..));
            let (id, answer) = match action {
                Work::Probe(id) => {
                    #[cfg(test)]
                    {
                        self.visits += 1;
                    }
                    if self.nodes[id]
                        .last
                        .is_some_and(|(name, _)| name == spelling)
                    {
                        continue;
                    }
                    if let Some(&answer) =
                        self.answers.get(&id).and_then(|cache| cache.get(&spelling))
                    {
                        self.nodes[id].last = Some((spelling, answer));
                        continue;
                    }
                    match &self.nodes[id].test {
                        Predicate::Indexed(index) => {
                            self.nodes[id].last = Some((spelling, index.contains(method)));
                            continue;
                        }
                        Predicate::Override(index, parent) => {
                            if let Some(is_async) = index.declaration(method) {
                                (id, is_async)
                            } else {
                                work.push(Work::Copy(id, *parent));
                                work.push(Work::Probe(*parent));
                                continue;
                            }
                        }
                        Predicate::Prefix(index, tail) => {
                            if index.contains(method) {
                                (id, true)
                            } else {
                                work.push(Work::Copy(id, *tail));
                                work.push(Work::Probe(*tail));
                                continue;
                            }
                        }
                        Predicate::Either(a, b) => {
                            work.push(Work::Either(id, *a, *b));
                            work.push(Work::Probe(*a));
                            continue;
                        }
                    }
                }
                Work::Either(id, a, b) => {
                    if self.nodes[a].last.expect("left effect operand evaluated").1 {
                        (id, true)
                    } else {
                        work.push(Work::Copy(id, b));
                        work.push(Work::Probe(b));
                        continue;
                    }
                }
                Work::Copy(id, child) => (
                    id,
                    self.nodes[child].last.expect("effect operand evaluated").1,
                ),
            };
            self.nodes[id].last = Some((spelling, answer));
            self.index_after_repeated_probes(id, visited_tail);
            if !matches!(self.nodes[id].test, Predicate::Indexed(_)) {
                // A different root can enter this suffix without visiting the
                // first root. Remember its answer here for every later entrant.
                let first_root = *self.nodes[id].first_root.get_or_insert(root);
                if id == root || first_root != root {
                    self.answers.entry(id).or_default().insert(spelling, answer);
                }
            }
        }
        self.nodes[root].last.expect("root effect evaluated").1
    }

    fn indexed_prefix(&self, id: usize) -> Option<(&MethodIndex<'a>, usize)> {
        match &self.nodes[id].test {
            Predicate::Prefix(index, tail) => Some((index, *tail)),
            Predicate::Either(head, tail) => match &self.nodes[*head].test {
                Predicate::Indexed(index) => Some((index, *tail)),
                _ => None,
            },
            _ => None,
        }
    }

    // A miss in the prefix paid for visiting the tail. Fold its ready prefix
    // into this node even when the rest of that tail has never been evaluated.
    fn index_after_repeated_probes(&mut self, id: usize, visited_tail: bool) {
        self.nodes[id].probes += 1;
        let probes = self.nodes[id].probes;
        let (replacement, cost) = match self.nodes[id].test {
            Predicate::Override(ref nearer, parent) => {
                let (farther, rest) = match &self.nodes[parent].test {
                    Predicate::Indexed(index) => (index, None),
                    Predicate::Override(index, rest) if visited_tail => (index, Some(*rest)),
                    _ => return,
                };
                let Some((merged, cost)) = nearer.inherit(farther, rest.is_some(), probes) else {
                    return;
                };
                let replacement = match rest {
                    Some(rest) => Predicate::Override(merged, rest),
                    None => Predicate::Indexed(merged),
                };
                (replacement, cost)
            }
            Predicate::Either(..) | Predicate::Prefix(..) => {
                let Some((prefix, tail)) = self.indexed_prefix(id) else {
                    return;
                };
                let (next, rest) = match &self.nodes[tail].test {
                    Predicate::Indexed(index) => (index, None),
                    _ if visited_tail => {
                        let Some((index, rest)) = self.indexed_prefix(tail) else {
                            return;
                        };
                        (index, Some(rest))
                    }
                    _ => return,
                };
                let Some((merged, cost)) = prefix.union(next, probes) else {
                    return;
                };
                let replacement = match rest {
                    Some(rest) => Predicate::Prefix(merged, rest),
                    None => Predicate::Indexed(merged),
                };
                (replacement, cost)
            }
            _ => return,
        };
        #[cfg(test)]
        {
            self.indexed_entries_read += cost;
        }
        self.nodes[id].probes -= cost;
        if matches!(replacement, Predicate::Indexed(_)) {
            self.answers.remove(&id);
        }
        self.nodes[id].test = replacement;
    }
}

#[derive(Clone, Copy)]
struct Child {
    id: TypeId,
    implementation: bool,
}

#[derive(Default)]
pub(super) struct DispatchEffects<'a> {
    children: Option<HashMap<TypeId, Vec<Child>>>,
    descendants: HashMap<TypeId, Effects>,
    inherited: HashMap<TypeId, Effects>,
    predicates: Predicates<'a>,
    #[cfg(test)]
    pub(super) visits: usize,
    #[cfg(test)]
    edge_reads: usize,
}

impl<'a> DispatchEffects<'a> {
    /// The caller has checked the exact receiver. A methodless base edge
    /// introduces no new effect for any spelling.
    pub(super) fn may_start_task(
        &mut self,
        resolution: &'a LoweringDeclarations<'_>,
        root: TypeId,
        method: &str,
    ) -> bool {
        if resolution.open_dispatch_types.contains(&root) {
            return true;
        }
        if let Some(effects) = self.descendants.get(&root) {
            return self.predicates.contains(*effects, method);
        }
        let children = self.children.get_or_insert_with(|| {
            let mut children = HashMap::<_, Vec<_>>::new();
            for (&id, info) in &resolution.classes {
                if let Some(base) = info.base {
                    children.entry(base).or_default().push(Child {
                        id,
                        implementation: false,
                    });
                }
                for ty in &info.implements {
                    if let Type::Named(interface) | Type::Generic(interface, _) = ty {
                        children.entry(*interface).or_default().push(Child {
                            id,
                            implementation: true,
                        });
                    }
                }
            }
            for (&id, info) in &resolution.interfaces {
                for base in &info.extends {
                    children.entry(*base).or_default().push(Child {
                        id,
                        implementation: false,
                    });
                }
            }
            children
        });
        let mut work = vec![(root, false)];
        while let Some((id, exiting)) = work.pop() {
            if !exiting {
                let std::collections::hash_map::Entry::Vacant(entry) = self.descendants.entry(id)
                else {
                    continue;
                };
                #[cfg(test)]
                {
                    self.visits += 1;
                }
                if resolution.open_dispatch_types.contains(&id) {
                    entry.insert(Effects::Open);
                } else {
                    // Mark before scheduling to bound invalid cycles.
                    entry.insert(Effects::No);
                    if let Some(edges) = children.get(&id) {
                        work.push((id, true));
                        work.extend(edges.iter().map(|c| (c.id, false)));
                        #[cfg(test)]
                        {
                            self.edge_reads += edges.len();
                        }
                    }
                }
                continue;
            }
            let mut effects = Effects::No;
            for child in &children[&id] {
                #[cfg(test)]
                {
                    self.edge_reads += 1;
                }
                // Prepend the branch in depth-first order: test the child itself,
                // then its descendants, then the remaining siblings.
                effects = self.predicates.either(self.descendants[&child.id], effects);
                if matches!(effects, Effects::Open) {
                    break;
                }
                let local = if child.implementation {
                    implementation_effects(
                        child.id,
                        resolution,
                        &mut self.inherited,
                        &mut self.predicates,
                    )
                } else if let Some(interface) = resolution.interfaces.get(&child.id) {
                    self.predicates.declared(&interface.methods)
                } else {
                    resolution
                        .classes
                        .get(&child.id)
                        .map_or(Effects::No, |c| self.predicates.declared(&c.methods))
                };
                effects = self.predicates.either(local, effects);
            }
            self.descendants.insert(id, effects);
        }
        self.predicates.contains(self.descendants[&root], method)
    }
}

// Only an interface implementation needs the exact inherited async set.
// A methodless implementer shares its base's table. A sync declaration masks
// the inherited async body for that spelling in this branch alone.
fn implementation_effects<'a>(
    mut id: TypeId,
    resolution: &'a LoweringDeclarations<'_>,
    cache: &mut HashMap<TypeId, Effects>,
    predicates: &mut Predicates<'a>,
) -> Effects {
    let mut path = Vec::new();
    let mut effects = loop {
        if let Some(effects) = cache.get(&id) {
            break *effects;
        }
        if let Some(interface) = resolution.interfaces.get(&id) {
            break predicates.declared(&interface.methods);
        }
        let Some(info) = resolution.classes.get(&id) else {
            break Effects::No;
        };
        cache.insert(id, Effects::No);
        path.push((id, &info.methods));
        let Some(base) = info.base else {
            break Effects::No;
        };
        id = base;
    };
    for (id, methods) in path.into_iter().rev() {
        effects = predicates.extend(effects, methods);
        cache.insert(id, effects);
    }
    effects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::typed_ast::{HirClassInfo, HirResolution};

    fn task_method(is_async: bool) -> HirSignature {
        HirSignature {
            params: vec![],
            param_modes: vec![],
            is_static: false,
            is_async,
            return_type: if is_async {
                Type::I64
            } else {
                Type::Generic(TypeId::local("Task"), vec![Type::I64])
            },
        }
    }

    #[test]
    fn virtual_task_effects_share_methodless_descendants_across_names() {
        let root = TypeId::local("Root");
        for count in [1, 8, 32] {
            let names: Vec<_> = (0..count).map(|i| format!("m{i}")).collect();
            for descendants in [1, 8, 32] {
                for override_last in [false, true] {
                    let mut hir = HirResolution::default();
                    hir.classes.insert(
                        root,
                        HirClassInfo {
                            methods: names
                                .iter()
                                .map(|name| (name.clone(), task_method(false)))
                                .collect(),
                            ..Default::default()
                        },
                    );
                    for n in 0..descendants {
                        let mut child = HirClassInfo {
                            base: Some(root),
                            ..Default::default()
                        };
                        if override_last && n + 1 == descendants {
                            child
                                .methods
                                .insert(names[count - 1].clone(), task_method(true));
                        }
                        hir.classes.insert(TypeId::local(format!("Leaf{n}")), child);
                    }
                    let view = LoweringDeclarations::new(&hir);
                    let mut dispatch = DispatchEffects::default();
                    let mut peak = 0;
                    for _ in 0..2 {
                        for (i, name) in names.iter().enumerate() {
                            assert_eq!(
                                dispatch.may_start_task(&view, root, name),
                                override_last && i + 1 == count
                            );
                            let graph = dispatch.children.as_ref().unwrap();
                            let predicates = &dispatch.predicates;
                            assert!(
                                dispatch.inherited.is_empty()
                                    && predicates.answers.is_empty()
                                    && predicates.spellings.is_empty()
                            );
                            assert!(predicates.nodes.iter().all(|n| matches!(
                                n.test,
                                Predicate::Indexed(MethodIndex { changes: None, .. })
                            )));
                            peak = peak.max(
                                graph.len()
                                    + graph.values().map(Vec::len).sum::<usize>()
                                    + dispatch.descendants.len()
                                    + predicates.nodes.len()
                                    + predicates.tables.len(),
                            );
                        }
                    }
                    assert_eq!(peak, 2 * descendants + 2 + 2 * usize::from(override_last));
                    assert_eq!(dispatch.visits, descendants + 1);
                    assert_eq!(dispatch.edge_reads, 2 * descendants);
                    println!(
                        "methods={count} descendants={descendants} async_override={override_last} visited={} edge_reads={} peak_entries={peak}",
                        dispatch.visits, dispatch.edge_reads
                    );
                }
            }
        }
    }

    // Count retained records separately from the input declarations. Counting
    // each overlay's logical changes is an upper bound if trie paths are shared.
    fn retained_entries(dispatch: &DispatchEffects<'_>) -> (usize, usize) {
        let p = &dispatch.predicates;
        let pairs = p.answers.values().map(HashMap::len).sum::<usize>();
        let graph = dispatch.children.as_ref().unwrap();
        let changes = p
            .nodes
            .iter()
            .map(|node| match &node.test {
                Predicate::Indexed(index)
                | Predicate::Prefix(index, _)
                | Predicate::Override(index, _) => index.changes_len(),
                _ => 0,
            })
            .sum::<usize>();
        let total = graph.len()
            + graph.values().map(Vec::len).sum::<usize>()
            + dispatch.descendants.len()
            + dispatch.inherited.len()
            + p.nodes.len()
            + p.tables.len()
            + p.spellings.len()
            + p.answers.len()
            + pairs
            + changes;
        (pairs, total)
    }

    #[test]
    fn virtual_task_effects_index_successively_deeper_overrides() {
        let root = TypeId::local("Base");
        for depth in [4, 16, 64, 256] {
            let names: Vec<_> = (0..depth).map(|i| format!("m{i}")).collect();
            let mut hir = HirResolution::default();
            hir.classes.insert(
                root,
                HirClassInfo {
                    methods: names
                        .iter()
                        .map(|name| (name.clone(), task_method(false)))
                        .collect(),
                    ..Default::default()
                },
            );
            let mut parent = root;
            for (i, name) in names.iter().enumerate() {
                let id = TypeId::local(format!("Child{i}"));
                hir.classes.insert(
                    id,
                    HirClassInfo {
                        base: Some(parent),
                        methods: [(name.clone(), task_method(true))].into(),
                        ..Default::default()
                    },
                );
                parent = id;
            }
            let view = LoweringDeclarations::new(&hir);
            let mut dispatch = DispatchEffects::default();
            let mut peak_entries = 0;
            let mut peak_pairs = 0;
            for name in &names {
                assert!(dispatch.may_start_task(&view, root, name));
                let (pairs, entries) = retained_entries(&dispatch);
                peak_pairs = peak_pairs.max(pairs);
                peak_entries = peak_entries.max(entries);
            }
            let p = &dispatch.predicates;
            let visits = p.visits + p.fast_queries;
            assert_eq!(dispatch.visits, depth + 1);
            assert!(visits <= 3 * depth);
            assert!(p.indexed_entries_read <= 2 * depth);
            assert!(peak_pairs <= depth);
            assert!(peak_entries <= 9 * depth);
            println!(
                "successive depth={depth} visits={visits} entries_read={} peak_pairs={peak_pairs} peak_entries={peak_entries}",
                p.indexed_entries_read
            );
            assert!(!dispatch.may_start_task(&view, root, "absent"));
        }
    }

    #[test]
    fn virtual_task_effects_index_progressively_inherited_interface_methods() {
        let interface = TypeId::local("Runnable");
        for depth in [8, 32, 128, 256] {
            let mut hir = HirResolution::default();
            let mut parent = None;
            for level in 0..depth {
                let id = TypeId::local(format!("C{level}"));
                let name = format!("m{level}");
                hir.interfaces
                    .entry(interface)
                    .or_default()
                    .methods
                    .insert(name.clone(), task_method(false));
                let mut class = HirClassInfo {
                    base: parent,
                    methods: [(name, task_method(true))].into(),
                    ..Default::default()
                };
                if level + 1 == depth {
                    class.implements.push(Type::Named(interface));
                }
                hir.classes.insert(id, class);
                parent = Some(id);
            }
            let view = LoweringDeclarations::new(&hir);
            let mut dispatch = DispatchEffects::default();
            let mut peak = 0;
            for level in (0..depth).rev() {
                assert!(dispatch.may_start_task(&view, interface, &format!("m{level}")));
                peak = peak.max(retained_entries(&dispatch).1);
            }
            let p = &dispatch.predicates;
            let visits = p.visits + p.fast_queries;
            println!(
                "inherited depth={depth} visits={visits} reads={} peak={peak}",
                p.indexed_entries_read
            );
            assert!(visits <= 2 * depth);
            assert!(p.indexed_entries_read <= depth);
            assert!(peak <= 7 * depth);
            assert!(!dispatch.may_start_task(&view, interface, "absent"));
        }
    }

    #[test]
    fn virtual_task_effects_keep_nearest_interface_declaration_in_partial_indexes() {
        let interface = TypeId::local("Runnable");
        for width in [1, 4, 16] {
            let depth = 32;
            let mut hir = HirResolution::default();
            let mut parent = None;
            for level in 0..depth {
                let id = TypeId::local(format!("C{level}"));
                let mut methods: Methods = (0..width)
                    .map(|i| (format!("own{level}_{i}"), task_method(true)))
                    .collect();
                if level == 0 {
                    for i in 1..depth {
                        methods.insert(format!("masked{i}"), task_method(true));
                        methods.insert(format!("enabled{i}"), task_method(false));
                    }
                } else {
                    methods.insert(format!("masked{level}"), task_method(false));
                    methods.insert(format!("enabled{level}"), task_method(true));
                }
                let mut class = HirClassInfo {
                    base: parent,
                    methods,
                    ..Default::default()
                };
                if level + 1 == depth {
                    class.implements.push(Type::Named(interface));
                }
                hir.classes.insert(id, class);
                parent = Some(id);
            }
            hir.interfaces.entry(interface).or_default();
            let view = LoweringDeclarations::new(&hir);
            let mut dispatch = DispatchEffects::default();
            for level in (depth / 2..depth).rev() {
                assert!(dispatch.may_start_task(&view, interface, &format!("own{level}_0")));
            }
            // These spellings have no cached answers. A synchronous entry in a
            // partial index must still hide the deeper asynchronous declaration.
            let Effects::Test(root) = dispatch.descendants[&interface] else {
                panic!("the implementer has inherited method declarations");
            };
            let Predicate::Override(prefix, _) = &dispatch.predicates.nodes[root].test else {
                panic!("queries into the nearer half leave an unresolved base");
            };
            if width == 1 {
                assert_eq!(prefix.declaration("masked30"), Some(false));
                assert_eq!(prefix.declaration("enabled30"), Some(true));
            }
            for level in (1..depth).rev() {
                assert!(!dispatch.may_start_task(&view, interface, &format!("masked{level}")));
                assert!(dispatch.may_start_task(&view, interface, &format!("enabled{level}")));
            }
            assert!(!dispatch.may_start_task(&view, interface, "absent"));
            assert!(dispatch.predicates.indexed_entries_read <= dispatch.predicates.visits);
        }
    }

    #[test]
    fn virtual_task_effects_budget_partial_unions_by_table_width() {
        for depth in [16, 64, 256] {
            for width in [2, 8, 32] {
                let tables: Vec<Methods> = (0..depth)
                    .map(|i| {
                        (0..width)
                            .map(|j| (format!("m{i}_{j}"), task_method(j == 0)))
                            .collect()
                    })
                    .collect();
                let mut p = Predicates::default();
                let mut root = Effects::No;
                for table in tables.iter().rev() {
                    let head = p.declared(table);
                    root = p.either(head, root);
                }
                for i in 0..depth {
                    assert!(p.contains(root, &format!("m{i}_0")));
                }
                println!(
                    "padded depth={depth} width={width} visits={} reads={}",
                    p.visits + p.fast_queries,
                    p.indexed_entries_read
                );
                assert!(p.indexed_entries_read <= p.visits);
            }
        }
    }

    #[test]
    fn virtual_task_effects_share_paid_prefix_with_other_receivers() {
        let depth = 24;
        for width in [2, 8, 32] {
            let tables: Vec<Methods> = (0..depth)
                .map(|i| {
                    (0..width)
                        .map(|j| (format!("m{i}_{j}"), task_method(true)))
                        .collect()
                })
                .collect();
            let sibling: Methods = [("other".into(), task_method(false))].into();
            let mut p = Predicates::default();
            let mut shared = Effects::No;
            for table in tables.iter().rev() {
                let head = p.declared(table);
                shared = p.either(head, shared);
            }
            let sibling = p.declared(&sibling);
            let parent = p.either(sibling, shared);
            // Leave one fresh spelling in each table. Probe enough of the
            // shared subtree to index an interior override while the tail is cold.
            for i in 0..depth / 2 {
                for j in 1..width {
                    assert!(p.contains(shared, &format!("m{i}_{j}")));
                }
            }
            let fresh = "m3_0";
            let Effects::Test(id) = shared else {
                panic!("nonempty shared predicate");
            };
            let Predicate::Prefix(index, _) = &p.nodes[id].test else {
                panic!("partially indexed subtree");
            };
            assert!(index.contains(fresh), "width={width}");
            assert!(!p.spellings.contains_key(fresh));
            let visits = p.visits;
            assert!(p.contains(parent, fresh));
            assert_eq!(p.visits - visits, 3);
            assert!(p.indexed_entries_read <= p.visits);
            assert!(p.contains(parent, &format!("m{}_0", depth - 1)));
            assert!(!p.contains(shared, "absent"));
        }
    }

    #[test]
    fn virtual_task_effects_stop_at_immediate_async_overrides() {
        let root = TypeId::local("Base");
        let child = TypeId::local("Override");
        for depth in [2, 8, 32] {
            for method_count in [2, 8, 32] {
                let names: Vec<_> = (0..method_count).map(|i| format!("m{i}")).collect();
                let mut hir = HirResolution::default();
                for (id, base) in [(root, None), (child, Some(root))] {
                    hir.classes.insert(
                        id,
                        HirClassInfo {
                            base,
                            methods: names
                                .iter()
                                .map(|name| (name.clone(), task_method(id == child)))
                                .collect(),
                            ..Default::default()
                        },
                    );
                }
                let mut parent = child;
                for level in 0..depth {
                    let id = TypeId::local(format!("Deeper{level}"));
                    hir.classes.insert(
                        id,
                        HirClassInfo {
                            base: Some(parent),
                            methods: (0..=method_count)
                                .map(|i| (format!("other{level}_{i}"), task_method(false)))
                                .collect(),
                            ..Default::default()
                        },
                    );
                    parent = id;
                }
                let view = LoweringDeclarations::new(&hir);
                let mut dispatch = DispatchEffects::default();
                let mut peak_pairs = 0;
                let mut peak_entries = 0;
                for name in &names {
                    assert!(dispatch.may_start_task(&view, root, name));
                    let (pairs, entries) = retained_entries(&dispatch);
                    peak_pairs = peak_pairs.max(pairs);
                    peak_entries = peak_entries.max(entries);
                }
                let p = &dispatch.predicates;
                let probes = p.visits + p.fast_queries;
                assert_eq!(dispatch.visits, depth + 2);
                assert_eq!(dispatch.edge_reads, 2 * (depth + 1));
                assert_eq!(probes, 2 * method_count);
                assert_eq!(p.indexed_entries_read, 0);
                assert_eq!(peak_pairs, method_count);
                assert_eq!(peak_entries, 6 * depth + 7 + 2 * method_count);
                println!(
                    "immediate depth={depth} methods={method_count} hierarchy_visits={} predicate_visits={probes} peak_pairs={peak_pairs} peak_entries={peak_entries}",
                    dispatch.visits
                );
                assert!(!dispatch.may_start_task(&view, root, "absent"));
            }
        }
    }

    #[test]
    fn virtual_task_effects_reuse_nested_suffix_answers() {
        for depth in [2, 8, 32] {
            for method_count in [2, 8, 32] {
                for wide in [false, true] {
                    let classes: Vec<_> = (0..=depth)
                        .map(|i| TypeId::local(format!("C{i}")))
                        .collect();
                    let methods: Vec<_> = (0..method_count).map(|i| format!("m{i}")).collect();
                    let mut hir = HirResolution::default();
                    for (i, &id) in classes.iter().enumerate() {
                        let mut declarations: Methods = if i == 0 || i == depth {
                            methods
                                .iter()
                                .enumerate()
                                .map(|(j, name)| {
                                    (
                                        name.clone(),
                                        task_method(i == depth && (!wide || j % 2 == 0)),
                                    )
                                })
                                .collect()
                        } else {
                            [(format!("other{i}"), task_method(false))].into()
                        };
                        if wide {
                            // Keep every union cold and mix positive and negative
                            // answers, so this case specifically needs sparse reuse.
                            for j in 0..(4 * method_count) {
                                declarations.insert(format!("padding{i}_{j}"), task_method(false));
                            }
                        }
                        hir.classes.insert(
                            id,
                            HirClassInfo {
                                base: i.checked_sub(1).map(|parent| classes[parent]),
                                methods: declarations,
                                ..Default::default()
                            },
                        );
                    }
                    let view = LoweringDeclarations::new(&hir);
                    for reverse in [false, true] {
                        let mut dispatch = DispatchEffects::default();
                        let mut peak_pairs = 0;
                        let mut peak_entries = 0;
                        for i in 0..depth {
                            let id = classes[if reverse { depth - 1 - i } else { i }];
                            for (j, name) in methods.iter().enumerate() {
                                assert_eq!(
                                    dispatch.may_start_task(&view, id, name),
                                    !wide || j % 2 == 0
                                );
                                let (pairs, entries) = retained_entries(&dispatch);
                                peak_pairs = peak_pairs.max(pairs);
                                peak_entries = peak_entries.max(entries);
                            }
                            if i == 0 {
                                assert!(peak_pairs <= method_count);
                            }
                        }
                        let p = &dispatch.predicates;
                        let probes = p.visits + p.fast_queries;
                        assert_eq!(dispatch.visits, depth + 1);
                        assert!(probes <= 6 * depth * method_count);
                        if wide {
                            assert_eq!(p.indexed_entries_read, 0);
                            assert_eq!(peak_pairs, (depth - 1) * method_count);
                            assert!(peak_entries <= peak_pairs + 7 * depth + method_count);
                        } else {
                            assert_eq!(peak_pairs, 0);
                            assert_eq!(p.indexed_entries_read, depth - 1);
                            assert_eq!(peak_entries, 6 * depth + 1);
                            assert!(probes <= depth * method_count + 2 * depth);
                        }
                        println!(
                            "depth={depth} methods={method_count} reverse={reverse} wide={wide} worklist_probes={} all_probes={probes} peak_pairs={peak_pairs} peak_entries={peak_entries}",
                            p.visits
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn virtual_task_effects_union_preserves_positive_and_negative_overrides() {
        let base: Methods = [
            ("sync".into(), task_method(false)),
            ("async".into(), task_method(true)),
        ]
        .into();
        let foreign = base.clone();
        let names = ["sync", "async", "added"];
        for second_base in [&base, &foreign] {
            for a in 0..8 {
                for b in 0..8 {
                    let mut left = MethodIndex {
                        base: &base,
                        changes: None,
                    };
                    let mut right = MethodIndex {
                        base: second_base,
                        changes: None,
                    };
                    for (i, name) in names.iter().enumerate() {
                        left.set(name, a & (1 << i) != 0);
                        right.set(name, b & (1 << i) != 0);
                    }
                    let (union, _) = left.union(&right, usize::MAX).unwrap();
                    for (i, name) in names.iter().enumerate() {
                        assert_eq!(union.contains(name), (a | b) & (1 << i) != 0);
                    }
                    assert!(!union.contains("missing"));
                }
            }
        }
    }

    #[test]
    fn virtual_task_effects_probe_wide_tables_before_indexing() {
        let [base, left, right, interface] =
            ["Base", "Left", "Right", "Selectable"].map(TypeId::local);
        for width in [8, 64] {
            let mut hir = HirResolution::default();
            let mut base_methods = Methods::new();
            let mut left_methods = Methods::new();
            let mut right_methods = Methods::new();
            for n in 0..width {
                base_methods.insert(format!("inherited{n}"), task_method(true));
                left_methods.insert(format!("left{n}"), task_method(n % 2 == 0));
                right_methods.insert(format!("right{n}"), task_method(n % 2 == 0));
            }
            hir.classes.insert(
                base,
                HirClassInfo {
                    methods: base_methods,
                    ..Default::default()
                },
            );
            hir.interfaces.entry(interface).or_default();
            for (id, methods) in [(left, left_methods), (right, right_methods)] {
                hir.classes.insert(
                    id,
                    HirClassInfo {
                        base: Some(base),
                        implements: vec![Type::Named(interface)],
                        methods,
                        ..Default::default()
                    },
                );
            }
            let view = LoweringDeclarations::new(&hir);
            let mut dispatch = DispatchEffects::default();
            assert!(!dispatch.may_start_task(&view, base, "missing"));
            assert!(dispatch.may_start_task(&view, interface, "inherited0"));
            assert_eq!(dispatch.predicates.indexed_entries_read, 0);
            for n in 0..(4 * width) {
                let method = format!("absent{n}");
                assert!(!dispatch.may_start_task(&view, base, &method));
                assert!(!dispatch.may_start_task(&view, interface, &method));
            }
            for n in 0..width {
                for id in [base, interface] {
                    for prefix in ["left", "right"] {
                        assert_eq!(
                            dispatch.may_start_task(&view, id, &format!("{prefix}{n}")),
                            n % 2 == 0
                        );
                    }
                }
                assert!(dispatch.may_start_task(&view, interface, &format!("inherited{n}")));
            }
            assert!(dispatch.predicates.answers.is_empty());
        }
    }

    #[test]
    fn virtual_task_effects_preserve_interface_inheritance_shadowing_and_open_dispatch() {
        let [base, child, hidden, open, top, sub, masked] =
            ["Base", "Child", "Hidden", "Open", "Top", "Sub", "Masked"].map(TypeId::local);
        let mut hir = HirResolution::default();
        hir.interfaces.entry(top).or_default();
        hir.interfaces.entry(sub).or_default().extends.push(top);
        hir.interfaces.entry(masked).or_default();
        hir.classes.insert(
            base,
            HirClassInfo {
                methods: [
                    ("run".into(), task_method(true)),
                    ("read".into(), task_method(false)),
                ]
                .into(),
                ..Default::default()
            },
        );
        for (id, interface) in [(child, sub), (hidden, masked)] {
            hir.classes.insert(
                id,
                HirClassInfo {
                    base: Some(base),
                    implements: vec![Type::Named(interface)],
                    ..Default::default()
                },
            );
        }
        hir.classes
            .get_mut(&hidden)
            .unwrap()
            .methods
            .insert("run".into(), task_method(false));
        hir.classes
            .get_mut(&hidden)
            .unwrap()
            .methods
            .insert("other".into(), task_method(true));
        hir.classes.insert(
            open,
            HirClassInfo {
                base: Some(child),
                ..Default::default()
            },
        );
        for open_dispatch in [false, true] {
            if open_dispatch {
                hir.open_dispatch_types.insert(open);
            }
            let view = LoweringDeclarations::new(&hir);
            let mut dispatch = DispatchEffects::default();
            for id in [sub, top] {
                assert!(dispatch.may_start_task(&view, id, "run"));
                assert_eq!(dispatch.may_start_task(&view, id, "read"), open_dispatch);
                assert_eq!(dispatch.may_start_task(&view, id, "absent"), open_dispatch);
            }
            assert!(!dispatch.may_start_task(&view, masked, "run"));
            assert!(dispatch.may_start_task(&view, masked, "other"));
        }
    }
}
