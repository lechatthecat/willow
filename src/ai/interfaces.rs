//! Generic interface dispatch per instantiation. The checker's interface node
//! `I::m` unions the implementations of every instantiation of `I`, so a call
//! on `I<A>` would reach `C<B>::m`. A caller whose every call of `I::m` has a
//! keyed receiver (see
//! [`crate::semantic::symbols::SymbolTable::interface_instance_key`]) instead
//! reaches one union per key: the implementations declaring that
//! instantiation plus those without a key (shared through one wildcard
//! union).
use super::*;
use crate::semantic::symbols::SymbolTable;
use std::collections::HashSet;

/// `sites`: (caller, interface method, receiver key, source span); `targets`: (interface
/// method, implementation, every instantiation key its class implements, or
/// `None` when any of them is unknown); `monos`: (method of a `$mono$`
/// instance, the same method of its template).
#[derive(Default)]
pub(super) struct Captured {
    sites: Vec<(FunctionId, FunctionId, Option<String>, Span)>,
    targets: Vec<(FunctionId, FunctionId, Option<Vec<String>>)>,
    monos: Vec<(FunctionId, FunctionId)>,
}

impl Captured {
    /// A call in `caller` resolved to `target`. A call of a generic interface
    /// method is keyed only by a receiver spelled as an instantiation of that
    /// interface; any other call of it (a `Self` receiver in a default body, a
    /// non-method call) records no key, which keeps the caller on the union.
    pub(super) fn site(
        &mut self,
        symbols: &SymbolTable,
        caller: FunctionId,
        target: FunctionId,
        receiver: Option<&Type>,
        span: Span,
    ) {
        let Some(interface) = generic_interface(symbols, target) else {
            return;
        };
        let key = receiver
            .filter(|receiver| {
                matches!(receiver, Type::Generic(name, _)
                if symbols.lookup_interface(name).is_some_and(|i| {
                    i.declaration_span == interface.declaration_span
                }))
            })
            .and_then(|receiver| symbols.interface_instance_key(receiver));
        self.sites.push((caller, target, key, span));
    }

    /// The implementations of the unit's generic interface methods, keyed by
    /// every instantiation their class or its ancestors declare
    /// (`implements I<A>, I<B>`).
    pub(super) fn targets(&mut self, symbols: &SymbolTable, graph: &CallGraph) {
        // One ancestor walk per (class, interface), shared by its methods.
        let mut walked = HashMap::new();
        for (&id, sites) in graph.iter() {
            let Some(owner) = id.owner() else {
                continue;
            };
            // A monomorphized interface's methods (`Self` calls in its default
            // bodies) carry no dispatch targets of their own: `route` links
            // them to the template's union. Classes are filtered there.
            if let Some((template, _)) = owner.split_once("$mono$") {
                let method = FunctionId::method(TypeId::local(template), id.name());
                let method = match (id.module(), id.namespace()) {
                    (Some(module), _) => method.in_module(module),
                    (None, Some(namespace)) => method.in_namespace(namespace),
                    (None, None) => method,
                };
                self.monos.push((id, method));
                continue;
            }
            let Some(interface) = generic_interface(symbols, id) else {
                continue;
            };
            for &target in &sites.targets {
                let keys = target.owner().and_then(|class| {
                    walked
                        .entry((class.clone(), owner.clone()))
                        .or_insert_with(|| implemented_keys(symbols, class.as_ref(), interface))
                        .clone()
                });
                self.targets.push((id, target, keys));
            }
        }
    }
}

/// The generic interface declaring `method`, unless it is a default body.
fn generic_interface(
    symbols: &SymbolTable,
    method: FunctionId,
) -> Option<&crate::semantic::symbols::InterfaceInfo> {
    let interface = symbols.lookup_interface(method.owner()?.as_ref())?;
    (!interface.type_params.is_empty() && !method.name().starts_with("$default$"))
        .then_some(interface)
}

/// Keys of every instantiation of `interface` that `class` or an ancestor
/// implements (a class may implement several, e.g. `I<A>, I<B>`). `None`
/// (implements every key) when the chain or any instantiation is unresolved.
fn implemented_keys(
    symbols: &SymbolTable,
    class: &str,
    interface: &crate::semantic::symbols::InterfaceInfo,
) -> Option<Vec<String>> {
    let mut keys = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(class.to_owned());
    while let Some(name) = current {
        if !seen.insert(name.clone()) {
            break;
        }
        let class = symbols.lookup_class(&name)?;
        for ty in &class.implements {
            let (Type::Named(name) | Type::Generic(name, _)) = ty else {
                continue;
            };
            if symbols
                .lookup_interface(name)
                .is_some_and(|i| i.declaration_span == interface.declaration_span)
            {
                keys.push(symbols.interface_instance_key(ty)?);
            }
        }
        current = class.base_class.clone();
    }
    (!keys.is_empty()).then_some(keys)
}

type Key = (UnitId, FunctionId);

#[derive(Default)]
struct Method {
    keyed: HashMap<String, BTreeSet<String>>,
    wild: BTreeSet<String>,
}

pub(super) struct Routing {
    pub functions: Vec<Function>,
    /// One source-bearing edge per routed call site, not sites × candidates.
    pub calls: Vec<(String, String, Span)>,
}

/// Reroute `edges` of callers whose every call of a known, complete generic
/// interface method is keyed; returns the new unions. Linear in sites +
/// targets + distinct (method, key) pairs: each union holds its own targets
/// and one edge to the method's shared wildcard union.
pub(super) fn route<'a>(
    captured: impl IntoIterator<Item = (UnitId, &'a Captured)>,
    nodes: &HashMap<Key, Function>,
    resolve: &impl Fn(UnitId, FunctionId) -> Key,
    edges: &mut HashMap<Key, BTreeSet<String>>,
) -> Routing {
    // Units see one implementation through different aliases; a unit that
    // cannot resolve them yields no key, and any unit's key is the class's.
    let mut implemented = HashMap::<(Key, &str), Option<BTreeSet<&String>>>::new();
    let mut calls = HashMap::<(Key, Key), Option<BTreeSet<&str>>>::new();
    let mut monos = Vec::new();
    let mut source_sites = HashMap::<(Key, Key), Vec<(Option<&str>, Span)>>::new();
    for (unit, capture) in captured {
        monos.extend(
            capture
                .monos
                .iter()
                .map(|&(mono, template)| (resolve(unit, mono), resolve(unit, template))),
        );
        for (method, target, key) in &capture.targets {
            if let Some(target) = nodes.get(&resolve(unit, *target)) {
                let known = implemented
                    .entry((resolve(unit, *method), target.id.as_str()))
                    .or_default();
                if let Some(keys) = key {
                    known.get_or_insert_default().extend(keys);
                }
            }
        }
        for (caller, method, key, span) in &capture.sites {
            source_sites
                .entry((resolve(unit, *caller), resolve(unit, *method)))
                .or_default()
                .push((key.as_deref(), *span));
            let keys = calls
                .entry((resolve(unit, *caller), resolve(unit, *method)))
                .or_insert_with(|| Some(BTreeSet::new()));
            match (keys.as_mut(), key) {
                (Some(keys), Some(key)) => {
                    keys.insert(key);
                }
                _ => *keys = None,
            }
        }
    }
    let mut methods = HashMap::<Key, Method>::new();
    for ((method, target), keys) in implemented {
        let method = methods.entry(method).or_default();
        match keys {
            Some(keys) => {
                for key in keys {
                    method
                        .keyed
                        .entry(key.clone())
                        .or_default()
                        .insert(target.to_owned());
                }
            }
            None => {
                method.wild.insert(target.to_owned());
            }
        }
    }
    // The checker records interface dispatch on the template only: a
    // monomorphized interface method calls the template's union, and the
    // template's other members (default bodies, which dispatch reaches)
    // stand for their instances. Class instances keep their own edges.
    let interfaces: HashSet<_> = methods
        .keys()
        .map(|(unit, method)| (*unit, method.owner_type()))
        .collect();
    for (mono, template) in monos {
        let (Some(node), Some(template_node)) = (nodes.get(&mono), nodes.get(&template)) else {
            continue;
        };
        if methods.contains_key(&template) {
            edges
                .entry(mono)
                .or_default()
                .insert(template_node.id.clone());
        } else if interfaces.contains(&(template.0, template.1.owner_type())) {
            edges.entry(template).or_default().insert(node.id.clone());
        }
    }
    let mut functions = Vec::new();
    let mut routed_calls = Vec::new();
    let mut made = HashSet::new();
    for ((caller, method_key), keys) in calls {
        let (Some(keys), Some(method), Some(node)) =
            (keys, methods.get(&method_key), nodes.get(&method_key))
        else {
            continue;
        };
        // An incomplete interface node keeps every caller on it.
        if node.unknown {
            continue;
        }
        let Some(caller_edges) = edges.get_mut(&caller) else {
            continue;
        };
        if !caller_edges.remove(&node.id) {
            continue;
        }
        let mut routed = HashMap::new();
        for key in keys {
            // No implementation of this instantiation is known: keep the
            // whole union rather than claim the call reaches nothing.
            if !method.keyed.contains_key(key) && method.wild.is_empty() {
                caller_edges.insert(node.id.clone());
                continue;
            }
            let id = hash(serde_json::to_vec(&(&node.id, key)).unwrap());
            caller_edges.insert(id.clone());
            routed.insert(key, id.clone());
            if !made.insert(id.clone()) {
                continue;
            }
            let mut callees: Vec<_> = method
                .keyed
                .get(key)
                .into_iter()
                .flatten()
                .cloned()
                .collect();
            if !method.wild.is_empty() {
                let wild = hash(serde_json::to_vec(&(&node.id, "*")).unwrap());
                if made.insert(wild.clone()) {
                    functions.push(union(
                        wild.clone(),
                        node,
                        "*",
                        method.wild.iter().cloned().collect(),
                    ));
                }
                callees.push(wild);
            }
            functions.push(union(id, node, key, callees));
        }
        for (key, span) in source_sites
            .remove(&(caller, method_key))
            .unwrap_or_default()
        {
            if let Some(target) = key.and_then(|key| routed.get(key)) {
                routed_calls.push((nodes[&caller].id.clone(), target.clone(), span));
            }
        }
    }
    Routing {
        functions,
        calls: routed_calls,
    }
}

fn union(id: String, method: &Function, key: &str, mut callees: Vec<String>) -> Function {
    callees.sort_unstable();
    Function {
        identity: None,
        body_id: None,
        body_location: None,
        rename_calls: vec![],
        rename_imports: vec![],
        id,
        module: method.module.clone(),
        name: format!("<{} implementations: {key}>", method.name),
        locations: vec![],
        synthetic: true,
        fingerprint: String::new(),
        body_fingerprint: String::new(),
        callees,
        // Bodyless/builtin implementations reached by the interface node are
        // folded into its own bits, not edges: every union keeps them.
        runtime_effects: method.runtime_effects,
        unknown: false,
        unresolved: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::ids::SymbolInterner;

    fn node(id: &str, runtime_effects: u8) -> Function {
        let mut node = union(
            id.into(),
            &union(String::new(), &root(), "", vec![]),
            "",
            vec![],
        );
        node.synthetic = false;
        node.runtime_effects = runtime_effects;
        node
    }

    fn root() -> Function {
        let mut root = union(String::new(), &placeholder(), "", vec![]);
        root.module = "m".into();
        root
    }

    fn placeholder() -> Function {
        Function {
            identity: None,
            body_id: None,
            body_location: None,
            rename_calls: vec![],
            rename_imports: vec![],
            id: String::new(),
            module: String::new(),
            name: String::new(),
            locations: vec![],
            synthetic: true,
            fingerprint: String::new(),
            body_fingerprint: String::new(),
            callees: vec![],
            runtime_effects: 0,
            unknown: false,
            unresolved: vec![],
        }
    }

    /// Bodyless/builtin implementations are folded into the interface node's
    /// own bits; a keyed union must keep them (review round 3).
    #[test]
    fn rerouted_unions_keep_the_interface_nodes_own_effects() {
        let interner = SymbolInterner::new();
        let _scope = interner.enter();
        let unit = UnitId::ENTRY;
        let method = FunctionId::method(TypeId::local("I"), "m");
        let a = FunctionId::method(TypeId::local("A"), "m");
        let b = FunctionId::method(TypeId::local("B"), "m");
        let caller = FunctionId::free("caller");
        let bits = RuntimeEffects::ALL.bits();
        let mut interfaces = Captured::default();
        interfaces.targets.push((method, a, Some(vec!["a".into()])));
        interfaces.targets.push((method, b, Some(vec!["b".into()])));
        interfaces
            .sites
            .push((caller, method, Some("a".into()), Span::dummy()));
        let nodes = HashMap::from([
            ((unit, method), node("I::m", bits)),
            ((unit, a), node("A::m", 0)),
            ((unit, b), node("B::m", 0)),
            ((unit, caller), node("caller", 0)),
        ]);
        let mut edges = HashMap::from([((unit, caller), BTreeSet::from(["I::m".to_owned()]))]);
        let unions = route(
            [(unit, &interfaces)],
            &nodes,
            &|unit, id| (unit, id),
            &mut edges,
        );
        assert_eq!(unions.calls.len(), 1);
        assert_eq!(
            unions.calls[0],
            (
                "caller".into(),
                unions.functions[0].id.clone(),
                Span::dummy()
            )
        );
        let unions = unions.functions;
        assert_eq!(unions.len(), 1);
        assert_eq!(unions[0].callees, ["A::m"]);
        assert_eq!(unions[0].runtime_effects, bits);
        assert_eq!(
            edges[&(unit, caller)],
            BTreeSet::from([unions[0].id.clone()])
        );
    }
}
