use super::*;

#[willow_continuations::checker]
impl TypeChecker {
    /// Standalone backend callers use the same typed edge collector. This
    /// mode collects only; the caller supplies external facts before solving.
    pub(crate) fn resolved_effect_graph(
        program: &Program,
    ) -> crate::semantic::call_graph::CallGraph {
        Self::resolved_effect_graph_and_builtins(program).0
    }

    pub(crate) fn resolved_effect_graph_and_builtins(
        program: &Program,
    ) -> (
        crate::semantic::call_graph::CallGraph,
        crate::semantic::intrinsics::BuiltinCalls,
    ) {
        let mut checker = Self::new();
        checker.effect_graph_only = Some(Default::default());
        checker.check_program(program);
        (
            checker
                .effect_graph_only
                .take()
                .expect("graph collection mode"),
            checker.builtin_calls,
        )
    }

    /// Reset and index the named callables consumed by the semantic
    /// `MAY_BLOCK | MAY_SUSPEND` analysis for E2604.
    pub(super) fn prepare_lock_effect_analysis(&mut self, program: &Program) {
        // Value-flow argument/return edges consume checked call identities.
        self.capture_call_sites = true;
        if program.items.iter().any(|item| matches!(item, Item::Function(f) if f.effect_contracts.iter().any(|a| matches!(a.kind, EffectContractKind::Pure | EffectContractKind::Verified)))) {
            self.capture_call_sites = true;
        }
        self.local.current_effect_callable = None;
        self.effect_index.callables.clear();
        self.effect_index.local_methods.clear();
        self.effect_index.imports = program
            .imports
            .iter()
            .map(|import| {
                (
                    import.alias.clone().unwrap_or_else(|| {
                        QualifiedName::new(&import.path).last_segment().to_string()
                    }),
                    import.path.clone(),
                )
            })
            .collect::<HashMap<_, _>>()
            .into();
        self.effect_index.hierarchy = std::sync::Arc::new(self.build_lock_effect_hierarchy());
        self.effect_inputs.edges.clear();
        self.resolved_calls.clear();
        self.static_reads.clear();
        self.unsafe_static_accesses.clear();
        self.static_sync.clear();
        self.static_markers = Default::default();
        self.analysis_calls.clear();
        self.pending_constructor_flow.clear();
        self.effect_inputs.direct.clear();
        self.effect_inputs.direct_sites.clear();
        self.effect_inputs.sites.clear();
        self.indirect_lock.clear();

        let root_queries = self.effect_queries.clone();
        let register_root = |body, location| {
            if let Some((queries, unit)) = &root_queries {
                queries.register_root(*unit, body, location);
            }
        };
        use crate::compiler_db::effects::EffectRootLocation;
        for (item_index, item) in program.items.iter().enumerate() {
            if let Some((_, unit)) = &root_queries {
                crate::ai::incremental_snapshot::record_declaration(*unit, item);
            }
            match item {
                Item::Function(function) => {
                    if let Some((_, unit)) = &root_queries {
                        crate::ai::incremental_snapshot::record_root(*unit, item_index, function);
                    }
                    register_root(function.body.id, EffectRootLocation::Function(item_index));
                    self.effect_index.callables.insert(
                        FunctionId::free_from_source_name(function.name.as_str()),
                        function.is_async(),
                    );
                }
                Item::Class(class) => {
                    let owner = TypeId::local(class.name.as_str());
                    let canonical = self
                        .symbols
                        .lookup_class(&class.name)
                        .map_or(owner, |info| TypeId::from_source_name(&info.name));
                    for (member, method) in class.methods.iter().enumerate() {
                        if let Some((_, unit)) = &root_queries {
                            crate::ai::incremental_snapshot::record_method(
                                *unit, item_index, member, class, method,
                            );
                        }
                        register_root(
                            method.body.id,
                            EffectRootLocation::Method {
                                item: item_index,
                                member,
                            },
                        );
                        self.effect_index.local_methods.insert(
                            FunctionId::method(canonical, &method.name),
                            FunctionId::method(owner, &method.name),
                        );
                        self.effect_index.callables.insert(
                            FunctionId::method(owner, method.name.as_str()),
                            method.is_async(),
                        );
                    }
                    for (member, constructor) in class.constructors.iter().enumerate() {
                        if let Some((_, unit)) = &root_queries {
                            crate::ai::incremental_snapshot::record_constructor(
                                *unit,
                                item_index,
                                member,
                                class,
                                constructor,
                            );
                        }
                        register_root(
                            constructor.body.id,
                            EffectRootLocation::Constructor {
                                item: item_index,
                                member,
                            },
                        );
                    }
                    if !class.constructors.is_empty() {
                        self.effect_index.local_methods.insert(
                            FunctionId::method(canonical, "init"),
                            FunctionId::method(owner, "init"),
                        );
                        self.effect_index
                            .callables
                            .insert(FunctionId::method(owner, "init"), false);
                    }
                }
                Item::Interface(interface) => {
                    for (member, method) in interface.methods.iter().enumerate() {
                        if let Some(body) = &method.default_body {
                            if let Some((_, unit)) = &root_queries {
                                crate::ai::incremental_snapshot::record_default(
                                    *unit, item_index, member, interface, method,
                                );
                            }
                            register_root(
                                body.id,
                                EffectRootLocation::InterfaceDefault {
                                    item: item_index,
                                    member,
                                },
                            );
                        }
                    }
                }
                Item::Enum(_) => {}
            }
        }

        // Include imported interfaces too. Interface methods are synchronous
        // in the current language; each ID is a synthetic union node for every
        // implementation reachable at dispatch.
        let interface_callables = self
            .symbols
            .interfaces
            .values()
            .flat_map(|interface| {
                let owner = TypeId::from_source_name(&interface.name);
                interface
                    .method_order
                    .iter()
                    .map(move |method| FunctionId::method(owner, method.as_str()))
            })
            .collect::<Vec<_>>();
        for callable in interface_callables {
            self.effect_index.callables.insert(callable, false);
        }
        for interface in program.items.iter().filter_map(|item| match item {
            Item::Interface(interface) => Some(interface),
            _ => None,
        }) {
            for method in &interface.methods {
                if method.default_body.is_some() && interface.type_params.is_empty() {
                    self.effect_index.callables.insert(
                        interface_default_effect_id(&interface.name, &method.name),
                        false,
                    );
                }
            }
        }

        self.connect_interface_lock_effects(program);
        // Freeze the declaration-derived interface unions before checking any
        // body; the same query result feeds the real effect graph.
        if let (Some(queries), Some((_, unit))) = (&self.body_queries, &self.effect_queries) {
            let inputs = self
                .symbols
                .interfaces
                .values()
                .flat_map(|interface| {
                    let owner = TypeId::from_source_name(&interface.name);
                    interface
                        .method_order
                        .iter()
                        .map(move |method| FunctionId::method(owner, method.as_str()))
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|id| {
                    let mut targets: Vec<_> = self
                        .effect_inputs
                        .edges
                        .get(&id)
                        .into_iter()
                        .flatten()
                        .copied()
                        .collect();
                    targets.sort();
                    (id, targets)
                })
                .collect();
            match queries.interface_dispatch_targets(*unit, inputs) {
                Ok(results) => {
                    for (id, targets) in results {
                        self.effect_inputs
                            .edges
                            .insert(id, targets.into_iter().collect());
                    }
                }
                Err(error) => self.body_query_error = Some(error),
            }
        }
    }

    /// Build the shared [`ClassHierarchy`] the effect analysis dispatches
    /// through. The symbol tables are the source rather than the AST because
    /// they also carry imported classes, so a receiver typed as an imported
    /// base still unions over the local subclasses that override its methods.
    ///
    /// Aliased item imports can insert one `ClassInfo` under several symbol
    /// keys; the canonical `ClassInfo::name` keeps the hierarchy deterministic.
    pub(super) fn build_lock_effect_hierarchy(&self) -> ClassHierarchy {
        let mut hierarchy = ClassHierarchy::default();
        let mut classes = self.symbols.classes.values().collect::<Vec<_>>();
        classes.sort_by(|left, right| left.name.cmp(&right.name));
        classes.dedup_by(|left, right| left.name == right.name);
        for class in classes {
            let base = class.base_class.as_deref().map(|base| {
                self.symbols
                    .lookup_class(base)
                    .map_or(base, |info| info.name.as_str())
            });
            hierarchy.add_class(&class.name, base);
            for (name, method) in &class.methods {
                // A static method is not a dispatch target: an instance call
                // can never select it, so it must not appear in the union.
                if !method.is_static {
                    hierarchy.add_method(&class.name, name);
                }
            }
            if class.constructor.is_some() {
                hierarchy.add_method(&class.name, "init");
            }
        }
        hierarchy
    }

    /// Connect each interface-method union node to every concrete body that
    /// may be selected by dynamic dispatch. Local implementations reuse the
    /// ordinary method call graph. Imported implementations reuse completed
    /// effect capability queries, falling back conservatively if unavailable.
    pub(super) fn connect_interface_lock_effects(&mut self, program: &Program) {
        let symbols = &self.symbols;
        let local_methods = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(class) => Some(class),
                _ => None,
            })
            .flat_map(|class| {
                // Imported-module declarations already carry canonical symbol
                // owners. Compare in that same identity domain; source names
                // would misclassify this unit's own methods as external.
                let owner = symbols
                    .lookup_class(&class.name)
                    .map_or(&class.name, |info| &info.name);
                class.methods.iter().map(move |method| {
                    #[cfg(test)]
                    LOCAL_INTERFACE_METHOD_RECORDS.with(|records| records.set(records.get() + 1));
                    (
                        (owner.clone(), method.name.clone()),
                        (
                            method.is_default_injected,
                            method.body.id,
                            FunctionId::method(TypeId::local(&class.name), &method.name),
                        ),
                    )
                })
            })
            .collect::<HashMap<_, _>>();

        let defaults: HashMap<_, _> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Interface(interface) => Some(interface),
                _ => None,
            })
            .flat_map(|interface| {
                interface.methods.iter().filter_map(move |method| {
                    method.default_body.as_ref().map(|body| {
                        (
                            body.id,
                            interface_default_effect_id(&interface.name, &method.name),
                        )
                    })
                })
            })
            .collect();

        // Aliased item imports can insert the same ClassInfo under multiple
        // symbol keys. Canonical ClassInfo::name keeps the union deterministic
        // and avoids duplicate external witnesses.
        let mut classes = self.symbols.classes.values().cloned().collect::<Vec<_>>();
        classes.sort_by(|left, right| left.name.cmp(&right.name));
        classes.dedup_by(|left, right| left.name == right.name);

        for class in classes {
            let mut interfaces = self.effect_interfaces_implemented_by(&class.name);
            interfaces.sort();
            interfaces.dedup();
            for interface_name in interfaces {
                let Some(interface) = self.symbols.lookup_interface(&interface_name).cloned()
                else {
                    continue;
                };
                for method_name in &interface.method_order {
                    let interface_id = FunctionId::method(
                        TypeId::from_source_name(&interface.name),
                        method_name.as_str(),
                    );
                    let Some(declaring) = self.resolved_method_class(&class.name, method_name)
                    else {
                        // Conformance checking owns the missing-method error.
                        continue;
                    };
                    // An inherited base spelling can be source-local even when
                    // the receiver and interface are canonical declarations.
                    let declaring = self
                        .symbols
                        .lookup_class(&declaring)
                        .map(|info| info.name.clone())
                        .unwrap_or(declaring);
                    let local = local_methods.get(&(declaring.clone(), method_name.clone()));
                    // Match declarations by canonical identity, while this
                    // unit's body graph keeps its source-local callable keys.
                    let implementation_id = local.map_or_else(
                        || {
                            FunctionId::method(
                                TypeId::from_source_name(&declaring),
                                method_name.as_str(),
                            )
                        },
                        |(_, _, id)| *id,
                    );
                    match local {
                        Some((true, body, _)) => {
                            // A non-generic injected default is checked once as
                            // its own synthetic callable. Connect it only when
                            // this implementation actually selects the default;
                            // overrides must not inherit an unused body effect.
                            let source = self
                                .body_queries
                                .as_ref()
                                .map_or(*body, |q| q.index().source_body(*body));
                            let default = defaults
                                .get(&source)
                                .copied()
                                .or_else(|| {
                                    let (
                                        _,
                                        crate::compiler_db::ids::BodyOwner::InterfaceDefault(id),
                                    ) = self.body_queries.as_ref()?.index().owner(source)?
                                    else {
                                        return None;
                                    };
                                    let mut owner = id.owner_type()?;
                                    // Template prechecks precede module attachment in
                                    // BodyIndex. Preserve the provider namespace while
                                    // selecting its concrete interface instance.
                                    if owner.namespace().is_none()
                                        && let Some(namespace) =
                                            TypeId::from_source_name(&interface.name).namespace()
                                    {
                                        owner = owner.in_namespace(namespace);
                                    }
                                    Some(FunctionId::synthetic_method(
                                        owner,
                                        format!("$default${}", id.name()),
                                        crate::qualified_name::SyntheticKind::InterfaceDefault,
                                    ))
                                })
                                .unwrap_or_else(|| {
                                    interface_default_effect_id(&interface.name, method_name)
                                });
                            // Concrete receivers select this same default too.
                            // Its waiting effects must reach C::method as well
                            // as the interface dispatch union (E2604).
                            self.effect_inputs
                                .edges
                                .entry(implementation_id)
                                .or_default()
                                .insert(default);
                            self.effect_inputs
                                .edges
                                .entry(interface_id)
                                .or_default()
                                .insert(default);
                        }
                        Some((false, _, _)) => {
                            self.effect_inputs
                                .edges
                                .entry(interface_id)
                                .or_default()
                                .insert(implementation_id);
                        }
                        None => {
                            use crate::semantic::effects::RuntimeEffects;
                            let waiting = self.effect_queries.as_ref().map_or(
                                crate::compiler_db::effects::LOCK_EFFECT_WAIT,
                                |(queries, unit)| {
                                    queries.external_waiting(
                                        *unit,
                                        &implementation_id,
                                        &self.effect_index.imports,
                                    )
                                },
                            );
                            let suspend = waiting.intersects(RuntimeEffects::MAY_SUSPEND);
                            let block = waiting.intersects(RuntimeEffects::MAY_BLOCK);
                            self.effect_inputs
                                .edges
                                .entry(interface_id)
                                .or_default()
                                .insert(implementation_id);
                            let kind = match (suspend, block) {
                                (true, true) => LockEffectKind::SuspendOrBlock,
                                (true, false) => LockEffectKind::Suspend,
                                (false, true) => LockEffectKind::Block,
                                (false, false) => continue,
                            };
                            let span = self
                                .symbols
                                .lookup_class(&declaring)
                                .and_then(|info| info.methods.get(method_name))
                                .map_or(class.declaration_span, |method| method.declaration_span);
                            let cause = self
                                .effect_queries
                                .as_ref()
                                .and_then(|(queries, unit)| {
                                    let (declaring, id) = queries.external_target(
                                        *unit,
                                        &implementation_id,
                                        &self.effect_index.imports,
                                    )?;
                                    let evidence = queries.effect_evidence(declaring, id)?;
                                    evidence.witness(LOCK_EFFECT_WAIT)?.lock().cloned()
                                })
                                .unwrap_or_else(|| LockEffectCause {
                                    is_conservative: true,
                                    span,
                                    operation: "interface dispatch to an imported implementation"
                                        .into(),
                                    kind,
                                    lock: None,
                                });
                            self.effect_inputs
                                .direct
                                .entry(implementation_id)
                                .or_default()
                                .insert(cause);
                        }
                    }
                }
            }
        }
    }

    /// Return the canonical interfaces a class can be dispatched through,
    /// including interfaces inherited from a base class and super-interfaces.
    pub(super) fn effect_interfaces_implemented_by(&self, class_name: &str) -> Vec<String> {
        let mut result = Vec::new();
        let mut class_stack = vec![class_name.to_string()];
        let mut seen_classes = std::collections::HashSet::new();
        let mut seen_interfaces = std::collections::HashSet::new();

        while let Some(name) = class_stack.pop() {
            if !seen_classes.insert(name.clone()) {
                continue;
            }
            let Some(class) = self.symbols.lookup_class(&name) else {
                continue;
            };
            if let Some(base) = &class.base_class {
                class_stack.push(base.clone());
            }
            let mut interface_stack = class
                .implements
                .iter()
                .filter_map(|ty| match ty {
                    Type::Named(name) | Type::Generic(name, _) => Some(name.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            while let Some(interface_name) = interface_stack.pop() {
                let Some(interface) = self.symbols.lookup_interface(&interface_name) else {
                    continue;
                };
                if !seen_interfaces.insert(interface.name.clone()) {
                    continue;
                }
                result.push(interface.name.clone());
                interface_stack.extend(interface.extends.iter().cloned());
            }
        }
        result
    }

    /// Record a typed call to a local or imported callable.
    ///
    /// An `async` callee is still an edge, but a masked one: calling an
    /// `async fn` eagerly creates a Task, which allocates in the caller, while
    /// the callee's waiting happens on that Task rather than on the caller's
    /// stack. `report_transitive_lock_effects` installs the mask that drops
    /// `MAY_SUSPEND | MAY_BLOCK` across such an edge, so the
    /// graph stays complete without the wait leaking into the caller. For the
    /// same reason a bare async call is never itself a lock-held wait site.
    pub(in crate::semantic::type_checker) fn record_lock_effect_call(
        &mut self,
        callee: FunctionId,
        span: Span,
    ) {
        let callee = self.effect_index.local_target(callee);
        let Some(callee_is_async) = self.lock_callee_is_async(&callee) else {
            return;
        };
        // A lock-held call site does not depend on the caller having a graph
        // node: non-callable contexts have none, and their direct waits are
        // diagnosed the same way (record_direct_lock_effect).
        if let Some(caller) = self.local.current_effect_callable {
            self.effect_inputs
                .edges
                .entry(caller)
                .or_default()
                .insert(callee);
        }
        if callee_is_async {
            return;
        }
        if self.local.lock_depth > 0 {
            self.effect_inputs
                .sites
                .push(LockEffectCallsite { callee, span });
        }
    }

    /// Whether a named callable of this unit or an imported one is `async`;
    /// `None` when it is neither (a builtin, or an unresolved name).
    pub(in crate::semantic::type_checker) fn lock_callee_is_async(
        &self,
        callee: &FunctionId,
    ) -> Option<bool> {
        let callee = &self.effect_index.local_target(*callee);
        if let Some(is_async) = self.effect_index.callables.get(callee) {
            return Some(*is_async);
        }
        let (queries, unit) = self.effect_queries.as_ref()?;
        queries
            .external_lock_callable(*unit, callee, &self.effect_index.imports)
            .map(|(_, _, is_async)| is_async)
    }

    /// Record one direct wait operation in the callable currently being
    /// checked. Block and suspend must both survive for bit-specific contracts.
    pub(in crate::semantic::type_checker) fn record_direct_lock_effect(
        &mut self,
        span: Span,
        operation: &'static str,
        kind: LockEffectKind,
    ) {
        // Callable bodies contribute transitive effects. Direct lock-held
        // sites must still be diagnosed when no callable identity is available.
        if let Some(caller) = self.local.current_effect_callable {
            self.effect_inputs
                .direct
                .entry(caller)
                .or_default()
                .insert(LockEffectCause {
                    is_conservative: false,
                    span,
                    operation: operation.into(),
                    kind,
                    lock: None,
                });
        }
        if self.local.lock_depth > 0 {
            self.effect_inputs.direct_sites.push(LockEffectCause {
                is_conservative: false,
                span,
                operation: operation.into(),
                kind,
                lock: None,
            });
        }
    }

    /// Resolve a receiver call to every callable node whose body/effect can run.
    ///
    /// The receiver's static type only bounds the dynamic type from above, so a
    /// class receiver resolves to the shared dispatch union: every body an
    /// instance of that class or any subclass would select. Resolving only the
    /// declaring class upward misses an override that waits, which is the
    /// virtual-dispatch effect analysis defect in the backend's copy of this rule.
    ///
    ///
    /// Interface receivers resolve to the single synthetic union node connected
    /// to every known implementation by `connect_interface_lock_effects`.
    pub(in crate::semantic::type_checker) fn method_effect_ids(
        &self,
        obj_ty: &Type,
        method: &str,
    ) -> Vec<FunctionId> {
        if matches!(obj_ty, Type::Array(_) | Type::FixedArray(..))
            && !matches!(
                crate::semantic::intrinsics::MethodName::classify(method),
                crate::semantic::intrinsics::MethodName::AbiConcat
                    | crate::semantic::intrinsics::MethodName::AbiCopy
                    | crate::semantic::intrinsics::MethodName::AbiDebug
                    | crate::semantic::intrinsics::MethodName::AbiLen
                    | crate::semantic::intrinsics::MethodName::AbiResize
            )
            && let Some(owner) = self.array_storage_method_class(obj_ty)
        {
            return vec![FunctionId::method(TypeId::from_source_name(owner), method)];
        }
        let class = match obj_ty {
            Type::Named(class) | Type::Generic(class, _) => class,
            _ => return Vec::new(),
        };
        // Default bodies check `self` as opaque Self. Dispatch still belongs
        // to the current (possibly instantiated) interface, not an unknown type.
        let class = if class == "Self" && self.local.self_interface.is_some() {
            self.local.current_class.as_ref().unwrap_or(class)
        } else {
            class
        };
        if let Some(interface) = self.symbols.lookup_interface(class) {
            if interface.methods.contains_key(method) {
                return vec![FunctionId::method(
                    TypeId::from_source_name(&interface.name),
                    method,
                )];
            }
            return Vec::new();
        }
        // Resolve through `ClassInfo::name`: an aliased item import reaches the
        // class under a local name the canonical hierarchy does not know.
        let Some(info) = self.symbols.lookup_class(class) else {
            return Vec::new();
        };
        if let (Some(queries), Some(body)) = (&self.body_queries, self.local.current_body) {
            return queries.dispatch_targets(body, TypeId::from_source_name(&info.name), method);
        }
        self.effect_index
            .hierarchy
            .dispatch_targets(&info.name, method)
    }

    /// Resolve a module function or static method to its declaring body without
    /// emitting diagnostics a second time.
    pub(in crate::semantic::type_checker) fn static_method_effect_id(
        &self,
        class: &str,
        method: &str,
    ) -> Option<FunctionId> {
        let class = if class == "Self" {
            self.local.current_class.as_deref()?
        } else {
            class
        };
        if self.symbols.lookup_module_func(class, method).is_some() {
            return Some(FunctionId::method(TypeId::from_source_name(class), method));
        }
        self.symbols.lookup_class(class)?;
        let declaring = self.resolved_method_class(class, method)?;
        let method_info = self.symbols.lookup_class(&declaring)?.methods.get(method)?;
        if !method_info.is_static {
            return None;
        }
        Some(FunctionId::method(
            TypeId::from_source_name(&declaring),
            method,
        ))
    }

    /// Close the same-program call graph and report lock-held helper calls
    /// whose synchronous execution can block or suspend before returning.
    pub(super) fn finish_effect_analysis(&mut self, program: &Program) {
        use crate::semantic::call_graph::{CallGraph, CallSites};
        let mut graph = CallGraph::default();
        for &id in self.effect_index.callables.keys() {
            graph.merge(id, CallSites::default());
        }
        for (id, sites) in &self.resolved_calls {
            graph.merge(*id, sites.clone());
        }
        for item in &program.items {
            if let Item::Class(class) = item {
                for method in &class.methods {
                    if method.is_default_injected {
                        // These bodies are checked under the canonical interface
                        // identity. A known edge supplies that body's effects;
                        // only a missing edge needs an unknown-call seed.
                        let id = FunctionId::method(TypeId::local(&class.name), &method.name);
                        graph.merge(
                            id,
                            CallSites {
                                unsupported_initialization: [crate::semantic::call_graph::UnsupportedInitialization::InjectedDefault].into(),
                                call_spans: Default::default(),
                                indirect_spans: Default::default(),
                                unknown_span: None,
                                virtual_spans: Default::default(),
                                virtual_calls: Default::default(),
                                targets: Default::default(),
                                has_unknown: self.effect_inputs.edges.get(&id).is_none_or(|targets| targets.is_empty()),
                            },
                        );
                    }
                }
            }
        }
        // Interface union/default edges are declaration facts, not call sites.
        for (id, targets) in &self.effect_inputs.edges {
            graph.merge(
                *id,
                CallSites {
                    unsupported_initialization: Default::default(),
                    call_spans: Default::default(),
                    indirect_spans: Default::default(),
                    unknown_span: None,
                    virtual_spans: Default::default(),
                    virtual_calls: Default::default(),
                    targets: targets.iter().copied().collect(),
                    has_unknown: false,
                },
            );
        }
        let mut capture_graph = graph.clone();
        self.refine_function_flow(program, &mut graph);
        capture_graph.flow_sensitive = graph.flow_sensitive;
        if self.effect_graph_only.is_some() {
            self.effect_graph_only = Some(graph);
            return;
        }
        let imports = std::sync::Arc::clone(&self.effect_index.imports);
        if let Some((queries, unit)) = &self.effect_queries {
            queries.lock_orders.borrow_mut().insert(
                *unit,
                (
                    super::lock_order::collect(
                        program,
                        &self.expr_types,
                        &|name| self.declaration_name_key(name),
                        &graph,
                    ),
                    std::sync::Arc::clone(&imports),
                ),
            );
        } else {
            let diagnostics = super::lock_order::analyze(
                program,
                &self.expr_types,
                &|name| self.declaration_name_key(name),
                &graph,
            );
            for diagnostic in diagnostics {
                self.push(diagnostic);
            }
        }

        if let Some((queries, unit)) = &self.effect_queries {
            queries.static_initializers.record(
                *unit,
                program,
                &graph,
                &self.static_reads,
                &imports,
                self.body_queries.as_ref().map(|queries| queries.index()),
            );
        }
        if self.effect_queries.is_none() && self.errors.is_empty() {
            let queries = crate::compiler_db::effects::EffectQueries::default();
            let unit = crate::module::UnitId::ENTRY;
            queries.static_initializers.record(
                unit,
                program,
                &graph,
                &self.static_reads,
                &imports,
                None,
            );
            for diagnostic in queries.static_initializers.solve(&[unit], &queries) {
                self.push(diagnostic);
            }
        }
        let compute = || {
            crate::compiler_db::effects::solve_unit(
                program,
                &graph,
                &self.expr_types,
                self.body_queries
                    .as_ref()
                    .zip(self.effect_queries.as_ref())
                    .map(|(b, (q, _))| (b.index(), q.as_ref())),
                &self.effect_index.callables,
                &self.effect_inputs.direct,
                |target| match &self.effect_queries {
                    Some((queries, unit)) => queries.external(*unit, target, &imports),
                    None => crate::compiler_db::effects::intrinsic_effects(target)
                        .unwrap_or(crate::semantic::effects::RuntimeEffects::MAY_PANIC),
                },
            )
        };
        if self.template_precheck
            && !self
                .errors
                .iter()
                .any(|diagnostic| diagnostic.severity == Severity::Error)
        {
            // The final checking phase solves and reports this unit's effects.
            // A unit with errors aborts the build first, so it reports them here.
            self.record_annotation_uses(program);
            return;
        }
        let effects = match &self.effect_queries {
            Some((queries, unit)) => match queries.complete(*unit, compute) {
                Ok(effects) => effects,
                Err(error) => {
                    self.body_query_error = Some(error);
                    return;
                }
            },
            None => std::sync::Arc::new(compute()),
        };
        self.record_annotation_uses(program);
        if let Some((queries, unit)) = &self.effect_queries
            && let Some(captured) = queries.analysis.borrow_mut().as_mut()
        {
            captured.insert(
                *unit,
                crate::ai::capture_incremental(
                    crate::ai::incremental_snapshot::CaptureOrigin {
                        unit: *unit,
                        bodies: self.body_queries.as_deref(),
                        queries,
                    },
                    program,
                    &capture_graph,
                    crate::ai::CaptureInputs {
                        indirect: &self.indirect_lock,
                        types: &self.expr_types,
                        calls: &self.analysis_calls,
                        patterns: &self.pattern_resolutions,
                        symbols: &self.analysis_symbols,
                    },
                    &effects.facts,
                    &self.symbols,
                    self.body_queries.as_ref().map(|q| q.index()),
                ),
            );
        }
        self.nonpreemptible_methods = std::sync::Arc::clone(&effects.helpers);
        if !self.pending_constructor_flow.is_empty() {
            let pending: HashSet<_> = std::mem::take(&mut self.pending_constructor_flow)
                .into_iter()
                .collect();
            let external = |target: &FunctionId| match &self.effect_queries {
                Some((queries, unit)) => queries.external(*unit, target, &imports),
                None => crate::compiler_db::effects::intrinsic_effects(target)
                    .unwrap_or(crate::semantic::effects::RuntimeEffects::MAY_PANIC),
            };
            let diagnostics = self.constructor_flow_diagnostics(program, &pending, &|call| {
                self.constructor_call_may_panic(call, &effects.facts, &external)
            });
            for diagnostic in diagnostics {
                self.push(diagnostic);
            }
        }
        self.report_task_method_calls();
        self.record_task_context(program, &graph, &effects, &imports);
        self.report_effect_contracts(program, &graph, &effects.facts, false);
        if let Some((queries, unit)) = &self.effect_queries
            && let Some(captured) = queries.analysis.borrow_mut().as_mut()
            && let Some(capture) = captured.get_mut(unit)
        {
            crate::ai::capture_panic_evidence(capture, self.panic_evidence.clone());
        }
        self.report_transitive_lock_effects(&effects.facts);
        self.report_indirect_lock_effects(&graph, &effects.facts);
    }

    /// Whether one constructor call site may panic, from the unit's solved
    /// facts: O(1) expected per site through the recorded unique target.
    /// Unknown, indirect, interface and overridable targets stay conservative.
    pub(super) fn constructor_call_may_panic(
        &self,
        call: &Expr,
        facts: &crate::semantic::effects::EffectFacts<crate::compiler_db::effects::EffectWitness>,
        external: &dyn Fn(&FunctionId) -> crate::semantic::effects::RuntimeEffects,
    ) -> bool {
        let Some(Some(target)) = self.analysis_calls.get(&call.id()) else {
            return true;
        };
        if let Expr::MethodCall(method) = call
            && !self.is_super_receiver(&method.object)
            && !self.final_method_receiver(&method.object, target)
        {
            // A subclass in an importing unit could override this target.
            return true;
        }
        // Read the profile-independent summary: checked arithmetic that this
        // build wraps still counts, so E0842 is the same in debug and release.
        let panics = crate::compiler_db::effects::ANY_PANIC;
        match facts.get(target) {
            Some(summary) => summary.intersects(panics),
            None => external(target).intersects(panics),
        }
    }

    /// True when dynamic dispatch through `receiver` cannot reach code other
    /// than `target`: the receiver class has no subclasses, or the resolved
    /// declaring method cannot be overridden. Two indexed lookups, without
    /// walking the inheritance chain per call site.
    pub(super) fn final_method_receiver(&self, receiver: &Expr, target: &FunctionId) -> bool {
        let Some(Type::Named(class) | Type::Generic(class, _)) =
            self.expr_types.get(&receiver.id())
        else {
            return false;
        };
        let Some(info) = self.symbols.lookup_class(class) else {
            return false;
        };
        if !info.is_open {
            return true;
        }
        target
            .owner_type()
            .and_then(|owner| self.symbols.classes.get(&owner))
            .and_then(|declaring| declaring.methods.get(target.name().as_ref()))
            .is_some_and(|method| method.is_static || !method.is_open && !method.is_override)
    }

    /// Why calling `callee` at `span` may suspend or block before returning,
    /// or `None` when it cannot. A callable of this unit (lambdas included)
    /// is read from the unit's own facts; an imported one from its declaring
    /// unit's frozen evidence, and missing evidence is assumed to wait.
    pub(in crate::semantic::type_checker) fn lock_wait_cause(
        &self,
        callee: &FunctionId,
        span: Span,
        facts: &crate::semantic::effects::EffectFacts<crate::compiler_db::effects::EffectWitness>,
    ) -> Option<LockEffectCause> {
        if let Some(cause) = self
            .effect_inputs
            .direct
            .get(callee)
            .and_then(DirectLockEffects::first)
        {
            return Some(cause.clone());
        }
        // Lambdas are never imported; any other callable outside this unit's
        // index is imported, even when the unit's graph has a node for it.
        let local = self.effect_index.callables.contains_key(callee)
            || callee.is_lambda()
            || matches!(
                callee.synthetic_kind(),
                crate::qualified_name::SyntheticKind::Indirect { .. }
            );
        if !local
            && crate::compiler_db::effects::intrinsic_effects(callee)
                .is_some_and(|effects| !effects.intersects(LOCK_EFFECT_WAIT))
        {
            return None;
        }
        let evidence = self.effect_queries.as_ref().and_then(|(queries, unit)| {
            if local {
                queries.effect_evidence(*unit, *callee)
            } else {
                let (declaring, id) =
                    queries.external_target(*unit, callee, &self.effect_index.imports)?;
                queries.effect_evidence(declaring, id)
            }
        });
        evidence
            .as_deref()
            .or_else(|| facts.get(callee))
            .filter(|summary| summary.intersects(LOCK_EFFECT_WAIT))
            .map(|summary| {
                summary
                    .witness(LOCK_EFFECT_WAIT)
                    .and_then(|witness| witness.lock().cloned())
                    .unwrap_or_else(|| LockEffectCause {
                        is_conservative: true,
                        span,
                        operation: "unavailable waiting evidence".into(),
                        kind: LockEffectKind::SuspendOrBlock,
                        lock: None,
                    })
            })
            .or_else(|| {
                (!local && evidence.is_none()).then(|| LockEffectCause {
                    is_conservative: true,
                    span,
                    operation: "unavailable imported effect evidence".into(),
                    kind: LockEffectKind::SuspendOrBlock,
                    lock: None,
                })
            })
    }

    pub(super) fn report_transitive_lock_effects(
        &mut self,
        facts: &crate::semantic::effects::EffectFacts<crate::compiler_db::effects::EffectWitness>,
    ) {
        // Direct and transitive waits use the same typed effect inventory.
        // Deferred-body validation owns E0905; do not report its sites twice.
        let already_reported = self
            .errors
            .iter()
            .filter(|diagnostic| diagnostic.code == ErrorCode::E0905)
            .flat_map(|diagnostic| diagnostic.labels.iter().map(|label| label.span))
            .collect::<Vec<_>>();
        let mut direct_callsites = self.effect_inputs.direct_sites.clone();
        direct_callsites.sort_by_key(|cause| (cause.span.start, cause.span.end));
        direct_callsites.dedup_by_key(|cause| cause.span);
        for cause in direct_callsites {
            if already_reported
                .iter()
                .any(|reported| reported.contains(cause.span))
            {
                continue;
            }
            let action = match cause.kind {
                LockEffectKind::Io => "accesses external resources",
                LockEffectKind::Suspend => "suspends the task",
                LockEffectKind::Block => "blocks the scheduler worker",
                LockEffectKind::SuspendOrBlock => {
                    "may suspend the task or block the scheduler worker"
                }
            };
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2604,
                    "cannot suspend or block while holding a Willow lock",
                )
                .with_label(Label::primary(
                    cause.span,
                    format!("`{}` {action} inside the critical section", cause.operation),
                ))
                .with_help(format!(
                    "move the `{}` outside the critical section; Rust calls here require a current reviewed nonblocking declaration",
                    cause.operation
                )),
            );
        }

        let mut callsites = self.effect_inputs.sites.clone();
        callsites.sort_by(|left, right| {
            (left.span.start, left.span.end)
                .cmp(&(right.span.start, right.span.end))
                .then_with(|| left.callee.cmp(&right.callee))
        });
        callsites.dedup_by(|a, b| a.span == b.span && a.callee == b.callee);
        // One call site is one diagnostic. A virtual call now records an edge
        // per member of its dispatch union, so report the
        // first waiting target in the sorted order and suppress the rest of
        // that span instead of emitting the same error once per override.
        let mut reported_spans = HashSet::new();
        for site in callsites {
            // A callable that waits on its own is explained by its own
            // operation; only a purely transitive effect needs the propagated
            // witness.
            let Some(cause) = self.lock_wait_cause(&site.callee, site.span, facts) else {
                continue;
            };
            if !reported_spans.insert(site.span) {
                continue;
            }
            let effect = match cause.kind {
                LockEffectKind::Io => "access external resources",
                LockEffectKind::Suspend => "suspend the task",
                LockEffectKind::Block => "block the scheduler worker",
                LockEffectKind::SuspendOrBlock => "suspend the task or block the scheduler worker",
            };
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2604,
                    "cannot call a waiting helper while holding a Willow lock",
                )
                .with_label(Label::primary(
                    site.span,
                    format!("`{}` may {effect} before returning", site.callee),
                ))
                .with_label(Label::secondary(
                    cause.span,
                    format!("`{}` causes this transitive effect", cause.operation),
                ))
                .with_help("move the helper call outside the critical section; Rust calls here require a current reviewed nonblocking declaration"),
            );
        }
    }
}
