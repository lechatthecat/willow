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
        let mut implemented = ImplementationKeys::default();
        for (&id, sites) in graph.iter() {
            let Some(owner) = id.owner() else {
                continue;
            };
            // A monomorphized interface's methods (`Self` calls in its default
            // bodies) carry no dispatch targets of their own: `route` links
            // them to the template's union. Classes are filtered there.
            if crate::semantic::ids::synthetic_name_kind(&owner)
                == crate::qualified_name::SyntheticKind::Monomorph
            {
                let template = crate::semantic::ids::source_name(&owner);
                let method = FunctionId::synthetic_method(
                    TypeId::local(template),
                    id.name(),
                    id.synthetic_kind(),
                );
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
                let keys = target
                    .owner_type()
                    .and_then(|class| implemented.get(symbols, class, interface.declaration_span));
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
    let interface = symbols.lookup_interface(&method.owner_type()?.to_string())?;
    (!interface.type_params.is_empty()
        && method.synthetic_kind() != crate::qualified_name::SyntheticKind::InterfaceDefault)
        .then_some(interface)
}

/// Inherited interface instances, indexed once per class across all methods
/// and interfaces. Persistent maps/sets share unchanged ancestors; a chain of
/// new instantiations does not copy every preceding prefix. None is an unknown
/// instantiation (inside the map) or unavailable/cyclic ancestry (in the cache).
type ClassInstances = im::HashMap<Span, Option<im::OrdSet<String>>>;
#[derive(Default)]
struct ImplementationKeys {
    classes: HashMap<TypeId, Option<ClassInstances>>,
}
#[cfg(test)]
thread_local! {
    static KEY_ANCESTOR_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
impl ImplementationKeys {
    fn get(
        &mut self,
        symbols: &SymbolTable,
        class: TypeId,
        interface: Span,
    ) -> Option<Vec<String>> {
        if !self.classes.contains_key(&class) {
            self.index(symbols, class);
        }
        self.classes
            .get(&class)?
            .as_ref()?
            .get(&interface)?
            .as_ref()
            .map(|keys| keys.iter().cloned().collect())
    }

    fn index(&mut self, symbols: &SymbolTable, class: TypeId) {
        let mut pending = Vec::new();
        let mut seen = HashSet::new();
        let mut current = Some(class);
        let mut instances = Some(ClassInstances::new());
        while let Some(id) = current {
            if let Some(cached) = self.classes.get(&id) {
                instances = cached.clone();
                break;
            }
            if !seen.insert(id) {
                instances = None;
                break;
            }
            #[cfg(test)]
            KEY_ANCESTOR_VISITS.with(|n| n.set(n.get() + 1));
            let Some(info) = symbols.lookup_class(&id.to_string()) else {
                self.classes.insert(id, None);
                instances = None;
                break;
            };
            pending.push((id, info));
            current = info.base_class.as_deref().map(TypeId::from_source_name);
        }
        for (id, info) in pending.into_iter().rev() {
            if let Some(instances) = &mut instances {
                for ty in &info.implements {
                    let (Type::Named(name) | Type::Generic(name, _)) = ty else {
                        continue;
                    };
                    let Some(declaration) = symbols.lookup_interface(name) else {
                        continue;
                    };
                    if declaration.type_params.is_empty() {
                        continue;
                    }
                    let keys = instances
                        .entry(declaration.declaration_span)
                        .or_insert_with(|| Some(im::OrdSet::new()));
                    match symbols.interface_instance_key(ty) {
                        Some(key) => {
                            if let Some(keys) = keys {
                                keys.insert(key);
                            }
                        }
                        None => *keys = None,
                    }
                }
            }
            self.classes.insert(id, instances.clone());
        }
    }
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
        synthetic_kind: crate::qualified_name::SyntheticKind::Source,
        identity: None,
        source_name: None,
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
            synthetic_kind: crate::qualified_name::SyntheticKind::Source,
            identity: None,
            source_name: None,
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

    #[test]
    fn dispatch_effects_generic_ancestry_is_indexed_once_per_class() {
        let directory = std::env::temp_dir().join(format!(
            "willow-interface-key-scaling-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("main.wi");
        for shape in ["chain", "fanout", "interfaces"] {
            for n in [8, 32, 128] {
                let interfaces = if shape == "interfaces" { n } else { 1 };
                let mut source = String::new();
                for i in 0..interfaces {
                    source.push_str(&format!("interface I{i}<T> {{ fn work(self) -> i64; }}\n"));
                }
                let contracts = (0..interfaces)
                    .map(|i| format!("I{i}<i64>"))
                    .collect::<Vec<_>>()
                    .join(", ");
                source.push_str(&format!("open class Root implements {contracts} {{ pub open fn work(self) -> i64 {{ return 0; }} }}\n"));
                for i in 0..n {
                    let base = if shape == "chain" && i != 0 {
                        format!("C{}", i - 1)
                    } else {
                        "Root".into()
                    };
                    source.push_str(&format!("open class C{i} extends {base} {{ pub open override fn work(self) -> i64 {{ return 0; }} }}\n"));
                }
                source.push_str("fn invoke(w: I0<i64>) -> i64 { return w.work(); } fn main() {}\n");
                std::fs::write(&path, source).unwrap();
                KEY_ANCESTOR_VISITS.with(|n| n.set(0));
                let snapshot = crate::CompilerSession::new(
                    path.to_str().unwrap(),
                    "",
                    &crate::CompilerOptions::debug(),
                    None,
                )
                .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
                .unwrap();
                let visits = KEY_ANCESTOR_VISITS.with(std::cell::Cell::get);
                assert_eq!(visits, n + 1, "{shape} n={n}");
                let function = snapshot
                    .functions
                    .iter()
                    .position(|f| f.name == "invoke")
                    .unwrap();
                let states = super::super::effect_states::Analysis::new(&snapshot).output(function);
                assert!(
                    states
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|e| e["state"] == "none"),
                    "{states}"
                );
                println!(
                    "interface keys shape={shape} classes={} interfaces={interfaces} ancestry_visits={visits}",
                    n + 1
                );
            }
        }
        std::fs::remove_dir_all(directory).unwrap();
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
