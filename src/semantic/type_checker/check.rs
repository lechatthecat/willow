//! The `check_*` type-checking methods (extracted from `mod.rs`). `check_program`
//! stays `pub` (the entry point); the rest are `pub(super)`. As a child module
//! these reach `TypeChecker`'s private fields/methods.

use std::collections::HashMap;

use crate::diagnostics::{Diagnostic, ErrorCode, FixSuggestion, Label, Severity, Span};
#[cfg(test)]
use crate::lexer::Lexer;
#[cfg(test)]
use crate::parser::Parser;
use crate::parser::ast::*;
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::call_graph::{CallGraph, CallSites};
use crate::semantic::effects::EffectProblem;
use crate::semantic::symbols::*;

use super::*;

impl TypeChecker {
    /// Introduce a local binding (let, parameter, loop variable, pattern
    /// binding, ...), rejecting the reserved builtin names first.
    ///
    /// `recover()` is not an ordinary function: whether it lowers to a runtime
    /// call or to a constant `None` depends on the enclosing defer, and both
    /// the checker and the backend dispatch it by callee name. A local binding
    /// or parameter named `recover` would therefore be silently ignored at the
    /// call site and compile to the builtin instead of the bound value
    /// (willow-s9ej.7), so the name is reserved at every binding site the same
    /// way a top-level `fn recover` is.
    pub(super) fn define_var(&mut self, name: String, info: VarInfo) {
        if name == "recover" {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0351,
                    "`recover` is a reserved builtin function",
                )
                .with_label(Label::primary(
                    info.declaration_span,
                    "cannot bind `recover` as a variable or parameter",
                ))
                .with_help(
                    "rename this binding; `recover()` always refers to the panic-recovery builtin",
                ),
            );
        }
        self.symbols.define_var(name, info);
    }

    /// Report E2003 if `name` (a local declaration) collides with an imported
    /// name (a module access name or a directly imported item).
    pub(super) fn check_local_decl_collision(&mut self, name: &str, span: Span) {
        if let Some(import_span) = self.imported_names.get(name).copied() {
            let mut diag = Diagnostic::new(
                Severity::Error,
                ErrorCode::E2003,
                format!("name `{name}` is defined both by an import and a local declaration"),
            )
            .with_label(Label::primary(span, "local declaration here"));
            if let Some(s) = import_span {
                diag = diag.with_label(Label::secondary(s, "imported here"));
            }
            self.push(diag.with_help("rename the local declaration or the import"));
        }
    }

    pub fn check_program(&mut self, program: &Program) {
        self.register_std_imports(&program.imports);
        // Looping sync helpers indexed by `Class::method`, so a call through a
        // typed non-`self` receiver (`obj.heavy()`) in a task context is flagged
        // E0810 — the AST-only ConcurrencyAnalyzer cannot resolve the receiver
        // type (willow-0a6k.2).
        self.nonpreemptible_methods =
            crate::semantic::concurrency::compute_nonpreemptible_helpers(program);

        // Pass 1: register class shapes, enum declarations, and interfaces.
        // Interfaces share the top-level namespace with classes/enums/functions
        // and must be registered before class conformance is validated.
        for item in &program.items {
            match item {
                Item::Class(c) => {
                    self.check_local_decl_collision(&c.name, c.span);
                    if c.name == "PanicInfo" {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0351,
                                "`PanicInfo` is a reserved runtime type",
                            )
                            .with_label(Label::primary(c.span, "cannot redeclare `PanicInfo`")),
                        );
                        continue;
                    }
                    self.register_class(c);
                }
                Item::Enum(e) => {
                    self.check_local_decl_collision(&e.name, e.span);
                    if e.name == "PanicInfo" {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0351,
                                "`PanicInfo` is a reserved runtime type",
                            )
                            .with_label(Label::primary(e.span, "cannot redeclare `PanicInfo`")),
                        );
                        continue;
                    }
                    self.register_enum(e);
                }
                Item::Interface(i) => {
                    self.check_local_decl_collision(&i.name, i.span);
                    if i.name == "PanicInfo" {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0351,
                                "`PanicInfo` is a reserved runtime type",
                            )
                            .with_label(Label::primary(i.span, "cannot redeclare `PanicInfo`")),
                        );
                        continue;
                    }
                    self.register_interface(i, None);
                }
                _ => {}
            }
        }

        // Pass 2: register all top-level function signatures
        for item in &program.items {
            if let Item::Function(f) = item {
                self.check_local_decl_collision(&f.name, f.span);
                if f.name == "recover" {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0351,
                            "`recover` is a reserved builtin function",
                        )
                        .with_label(Label::primary(f.span, "cannot redeclare `recover`")),
                    );
                    continue;
                }
                let params = self.normalize_param_types(&f.params);
                let param_infos = self.normalize_param_infos(&f.params);
                let return_type = self.normalize_type(&f.return_type, f.span);
                self.symbols.define_func(
                    f.name.clone(),
                    FuncInfo {
                        param_infos,
                        params,
                        return_type,
                        public: f.public,
                        is_async: f.is_async,
                        declaration_span: f.span,
                        module_path: None,
                    },
                );
            }
        }

        // Build the complete same-file callable index before checking any
        // body. Calls from an earlier definition to a later one must take part
        // in the lock-effect fixpoint just like backward calls do.
        self.prepare_lock_effect_analysis(program);

        // Pass 3: check bodies
        for item in &program.items {
            match item {
                Item::Function(f) => self.check_function(f),
                Item::Class(c) => self.check_class(c),
                Item::Enum(_) => {} // already registered
                Item::Interface(i) => self.check_interface(i), // validate `extends`
            }
        }

        self.report_transitive_lock_effects();
    }

    /// Reset and index the named callables consumed by the semantic
    /// `MAY_BLOCK | MAY_SUSPEND` analysis for E2604 (willow-38w.1.4).
    fn prepare_lock_effect_analysis(&mut self, program: &Program) {
        self.current_effect_callable = None;
        self.lock_effect_callables.clear();
        self.lock_effect_hierarchy = self.build_lock_effect_hierarchy();
        self.lock_effect_edges.clear();
        self.lock_direct_effects.clear();
        self.lock_direct_effect_callsites.clear();
        self.lock_effect_callsites.clear();

        for item in &program.items {
            match item {
                Item::Function(function) => {
                    self.lock_effect_callables
                        .insert(FunctionId::free(function.name.as_str()), function.is_async);
                }
                Item::Class(class) => {
                    let owner = TypeId::local(class.name.as_str());
                    for method in &class.methods {
                        self.lock_effect_callables.insert(
                            FunctionId::method(owner.clone(), method.name.as_str()),
                            method.is_async,
                        );
                    }
                    if !class.constructors.is_empty() {
                        self.lock_effect_callables
                            .insert(FunctionId::method(owner, "init"), false);
                    }
                }
                Item::Enum(_) | Item::Interface(_) => {}
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
                    .map(move |method| FunctionId::method(owner.clone(), method.as_str()))
            })
            .collect::<Vec<_>>();
        for callable in interface_callables {
            self.lock_effect_callables.insert(callable, false);
        }
        for interface in program.items.iter().filter_map(|item| match item {
            Item::Interface(interface) => Some(interface),
            _ => None,
        }) {
            for method in &interface.methods {
                if method.default_body.is_some() && interface.type_params.is_empty() {
                    self.lock_effect_callables.insert(
                        interface_default_effect_id(&interface.name, &method.name),
                        false,
                    );
                }
            }
        }

        self.connect_interface_lock_effects(program);
    }

    /// Build the shared [`ClassHierarchy`] the effect analysis dispatches
    /// through. The symbol tables are the source rather than the AST because
    /// they also carry imported classes, so a receiver typed as an imported
    /// base still unions over the local subclasses that override its methods.
    ///
    /// Aliased item imports can insert one `ClassInfo` under several symbol
    /// keys; the canonical `ClassInfo::name` keeps the hierarchy deterministic.
    fn build_lock_effect_hierarchy(&self) -> ClassHierarchy {
        let mut hierarchy = ClassHierarchy::default();
        let mut classes = self.symbols.classes.values().collect::<Vec<_>>();
        classes.sort_by(|left, right| left.name.cmp(&right.name));
        classes.dedup_by(|left, right| left.name == right.name);
        for class in classes {
            hierarchy.add_class(&class.name, class.base_class.as_deref());
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
    /// ordinary method call graph. Imported implementations are deliberately
    /// fail-closed until their effect summaries are serialized into module
    /// metadata: treating an unavailable body as pure would re-open E2604.
    fn connect_interface_lock_effects(&mut self, program: &Program) {
        let local_methods = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(class) => Some(class),
                _ => None,
            })
            .flat_map(|class| {
                class.methods.iter().map(move |method| {
                    (
                        (class.name.clone(), method.name.clone()),
                        method.is_default_injected,
                    )
                })
            })
            .collect::<HashMap<_, _>>();

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
                    let implementation_id = FunctionId::method(
                        TypeId::from_source_name(&declaring),
                        method_name.as_str(),
                    );
                    match local_methods.get(&(declaring.clone(), method_name.clone())) {
                        Some(true) => {
                            // A non-generic injected default is checked once as
                            // its own synthetic callable. Connect it only when
                            // this implementation actually selects the default;
                            // overrides must not inherit an unused body effect.
                            self.lock_effect_edges
                                .entry(interface_id)
                                .or_default()
                                .insert(interface_default_effect_id(&interface.name, method_name));
                        }
                        Some(false) => {
                            self.lock_effect_edges
                                .entry(interface_id)
                                .or_default()
                                .insert(implementation_id);
                        }
                        None => {
                            let span = self
                                .symbols
                                .lookup_class(&declaring)
                                .and_then(|info| info.methods.get(method_name))
                                .map_or(class.declaration_span, |method| method.declaration_span);
                            self.lock_direct_effects.entry(interface_id).or_insert(
                                LockEffectCause {
                                    span,
                                    operation: "interface dispatch to an imported implementation",
                                    kind: LockEffectKind::SuspendOrBlock,
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    /// Return the canonical interfaces a class can be dispatched through,
    /// including interfaces inherited from a base class and super-interfaces.
    fn effect_interfaces_implemented_by(&self, class_name: &str) -> Vec<String> {
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

    /// Record a typed call to a same-program callable.
    ///
    /// An `async` callee is still an edge, but a masked one: calling an
    /// `async fn` eagerly creates a Task, which allocates in the caller, while
    /// the callee's waiting happens on that Task rather than on the caller's
    /// stack. `report_transitive_lock_effects` installs the mask that drops
    /// `MAY_SUSPEND | MAY_BLOCK` across such an edge (willow-38w.1.4), so the
    /// graph stays complete without the wait leaking into the caller. For the
    /// same reason a bare async call is never itself a lock-held wait site.
    pub(super) fn record_lock_effect_call(&mut self, callee: FunctionId, span: Span) {
        let Some(callee_is_async) = self.lock_effect_callables.get(&callee).copied() else {
            return;
        };
        let Some(caller) = self.current_effect_callable.clone() else {
            return;
        };
        self.lock_effect_edges
            .entry(caller)
            .or_default()
            .insert(callee.clone());
        if callee_is_async {
            return;
        }
        if self.lock_depth > 0 {
            self.lock_effect_callsites
                .push(LockEffectCallsite { callee, span });
        }
    }

    /// Record one direct wait operation in the callable currently being
    /// checked. The first witness is sufficient: callers only need to know
    /// that some path can wait before the helper returns.
    pub(super) fn record_direct_lock_effect(
        &mut self,
        span: Span,
        operation: &'static str,
        kind: LockEffectKind,
    ) {
        let Some(caller) = self.current_effect_callable.clone() else {
            return;
        };
        self.lock_direct_effects
            .entry(caller)
            .or_insert(LockEffectCause {
                span,
                operation,
                kind,
            });
        if self.lock_depth > 0 {
            self.lock_direct_effect_callsites.push(LockEffectCause {
                span,
                operation,
                kind,
            });
        }
    }

    /// Resolve a receiver call to every callable node whose body/effect can run.
    ///
    /// The receiver's static type only bounds the dynamic type from above, so a
    /// class receiver resolves to the shared dispatch union: every body an
    /// instance of that class or any subclass would select. Resolving only the
    /// declaring class upward misses an override that waits, which is the
    /// willow-s9ej.11 defect in the backend's copy of this rule
    /// (willow-uqzx.1.2).
    ///
    /// Interface receivers resolve to the single synthetic union node connected
    /// to every known implementation by `connect_interface_lock_effects`.
    pub(super) fn method_effect_ids(&self, obj_ty: &Type, method: &str) -> Vec<FunctionId> {
        let class = match obj_ty {
            Type::Named(class) | Type::Generic(class, _) => class,
            _ => return Vec::new(),
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
        self.lock_effect_hierarchy
            .dispatch_targets(&info.name, method)
    }

    /// Resolve a local static method call to its declaring body without
    /// emitting diagnostics a second time.
    pub(super) fn static_method_effect_id(&self, class: &str, method: &str) -> Option<FunctionId> {
        let class = if class == "Self" {
            self.current_class.as_deref()?
        } else {
            class
        };
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
    fn report_transitive_lock_effects(&mut self) {
        // The propagation is the shared fixpoint (willow-uqzx.1.3). Only the
        // edges are local: `record_lock_effect_call` already filtered them to
        // same-program *synchronous* callables, because calling an `async fn`
        // eagerly creates a Task without suspending the caller. Everything the
        // checker cannot see is worth nothing here rather than everything: an
        // opaque imported callable is seeded explicitly as `SuspendOrBlock` by
        // `prepare_lock_effect_analysis`, so a silent unknown really is a leaf
        // with no wait.
        let mut graph = CallGraph::default();
        for (caller, callees) in &self.lock_effect_edges {
            graph.merge(
                caller.clone(),
                CallSites {
                    targets: callees.iter().cloned().collect(),
                    has_unknown: false,
                },
            );
        }
        let mut problem: EffectProblem<LockEffectWitness> = EffectProblem::new()
            .external_callee(RuntimeEffects::NONE)
            .unknown_callee(RuntimeEffects::NONE)
            .missing_body(RuntimeEffects::NONE);
        // An eager async call allocates a Task in the caller but does not make
        // the caller wait, so the edge keeps everything except the two wait
        // bits. This is the `NO_PREEMPT_REGION` distinction the lattice exists
        // to express: the effect is real, it just belongs to the Task.
        for (callable, is_async) in &self.lock_effect_callables {
            if *is_async {
                problem = problem.transmit(
                    callable.clone(),
                    RuntimeEffects::ALL.difference(LOCK_EFFECT_WAIT),
                );
            }
        }
        for (owner, cause) in &self.lock_direct_effects {
            problem = problem.seed(
                owner.clone(),
                cause.kind.effects(),
                Some(LockEffectWitness {
                    owner: owner.clone(),
                    cause: *cause,
                }),
            );
        }
        let facts = problem.solve(&graph);

        // `report_lock_suspensions` already emits the direct typed
        // await/select/channel/native-lock diagnostics while each lock body is
        // checked. Keep those diagnostics (they participate in the staged gate)
        // and add only direct effects that walker does not own, notably sync
        // std::fs calls.
        let already_reported = self
            .errors
            .iter()
            .filter(|diagnostic| matches!(diagnostic.code, ErrorCode::E2604 | ErrorCode::E0905))
            .flat_map(|diagnostic| diagnostic.labels.iter().map(|label| label.span))
            .collect::<Vec<_>>();
        let mut direct_callsites = self.lock_direct_effect_callsites.clone();
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
                    "move the `{}` outside the critical section",
                    cause.operation
                )),
            );
        }

        let mut callsites = self.lock_effect_callsites.clone();
        callsites.sort_by(|left, right| {
            (left.span.start, left.span.end)
                .cmp(&(right.span.start, right.span.end))
                .then_with(|| left.callee.cmp(&right.callee))
        });
        callsites.dedup_by(|a, b| a.span == b.span && a.callee == b.callee);
        // One call site is one diagnostic. A virtual call now records an edge
        // per member of its dispatch union (willow-uqzx.1.2), so report the
        // first waiting target in the sorted order and suppress the rest of
        // that span instead of emitting the same error once per override.
        let mut reported_spans = HashSet::new();
        for site in callsites {
            // A callable that waits on its own is explained by its own
            // operation; only a purely transitive effect needs the propagated
            // witness.
            let Some(cause) = self
                .lock_direct_effects
                .get(&site.callee)
                .copied()
                .or_else(|| {
                    facts
                        .get(&site.callee)
                        .filter(|summary| summary.intersects(LOCK_EFFECT_WAIT))
                        .and_then(|summary| summary.witness(LOCK_EFFECT_WAIT))
                        .map(|witness| witness.cause)
                })
            else {
                continue;
            };
            if !reported_spans.insert(site.span) {
                continue;
            }
            let effect = match cause.kind {
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
                .with_help("move the helper call outside the critical section"),
            );
        }
    }

    pub(super) fn check_block(&mut self, block: &Block) {
        self.lexical_block_depth += 1;
        self.symbols.push_scope();
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
        self.symbols.pop_scope();
        self.lexical_block_depth -= 1;
    }

    /// Type-check `lock <target> as [mut] <binding> { .. }` (willow-38w.1.1).
    ///
    /// V1 rules, in the order they are reported:
    ///   * the target must be the lock type the mode requires (E2602),
    ///   * the statement is legal only inside an `async fn` (E2603),
    ///   * no user-visible suspension inside the critical section (E2604),
    ///   * no nested `lock` inside another critical section (E2605).
    ///
    /// A statement that breaks none of those lowers through
    /// `emit_coop_lock` (willow-38w.1.4). Only `lock read` / `lock write` on an
    /// `RwLock<T>` remain behind the staged gate (E2502) until Stage 5.
    pub(super) fn check_lock_stmt(&mut self, s: &LockStmt) {
        // A target that already reported its own error (an unknown name, a bad
        // call) has a placeholder type; a second "wrong lock type" diagnostic on
        // top of it would only be noise.
        let errors_before = self.errors.len();
        let target_ty = self.check_expr(&s.target);
        let target_is_broken = self.errors.len() > errors_before;
        let protected = if target_is_broken {
            None
        } else {
            self.lock_protected_type(s, &target_ty)
        };
        let mut well_formed = protected.is_some();

        // The lock statement parks the task on contention, so it needs an async
        // frame to resume into. The blocking primitive is a separate type
        // (`BlockingCell<T>`); `Mutex<T>` never changes meaning by context.
        if !self.current_async_context {
            well_formed = false;
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2603,
                    "scheduler-aware lock acquisition is only allowed in an async function",
                )
                .with_label(Label::primary(
                    s.header_span(),
                    format!("`{}` used in a synchronous function", s.mode.keyword()),
                ))
                .with_help(
                    "mark the enclosing function `async fn`, or move the critical section into one",
                ),
            );
        }

        let outermost = self.lock_depth == 0;
        if !outermost {
            well_formed = false;
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2605,
                    "nested lock acquisition is not supported in V1",
                )
                .with_label(Label::primary(
                    s.header_span(),
                    "this lock is acquired inside another critical section",
                ))
                .with_help(
                    "acquire the locks one after another instead of nesting their critical sections",
                ),
            );
        }

        let binding_ty = protected.unwrap_or(Type::Never);
        // An async fn resumes into the critical section after a contended
        // acquisition, so the binding lives in the async frame (willow-38w.1.3),
        // and so do the three pieces of compiler-owned state the resumed poll
        // and the cancel entry need: the evaluated lock handle, this
        // acquisition's registration token, and its phase (willow-38w.1.4).
        if self.current_async_context {
            self.async_local_types
                .insert(s.binding_span, binding_ty.clone());
            self.async_local_types
                .insert(s.handle_frame_key(), target_ty.clone());
            self.async_local_types
                .insert(s.token_frame_key(), Type::I64);
            self.async_local_types
                .insert(s.phase_frame_key(), Type::I64);
        }

        self.symbols.push_scope();
        self.define_var(
            s.binding.clone(),
            VarInfo {
                ty: binding_ty,
                mutable: s.mutable,
                is_param: false,
                declaration_span: s.binding_span,
            },
        );
        self.lock_depth += 1;
        self.lexical_block_depth += 1;
        let body_errors_start = self.errors.len();
        for stmt in &s.body.stmts {
            self.check_stmt(stmt);
        }
        self.lexical_block_depth -= 1;
        self.lock_depth -= 1;
        self.symbols.pop_scope();

        // The suspension scan needs `expr_types`, so it runs after the body is
        // checked. Only the OUTERMOST lock scans, so one `await` inside two
        // nested locks is still reported once.
        if outermost && !self.report_lock_suspensions(s, body_errors_start) {
            well_formed = false;
        }

        let _ = well_formed;
    }

    /// Report E2604 for every suspension inside `s`'s critical section, and
    /// return whether the section was clean.
    ///
    /// `body_errors_start` indexes `self.errors` just before the body was
    /// checked, so a suspension the `defer` rules already rejected (E0905
    /// covers `await`/`select`, channel operations, and nested `lock` inside a
    /// deferred body) is dropped instead of reported twice. Anything a future
    /// suspension form leaves uncovered there still surfaces here.
    fn report_lock_suspensions(&mut self, s: &LockStmt, body_errors_start: usize) -> bool {
        let suspends = lock_body_suspend_spans(&s.body, &self.expr_types);
        if suspends.is_empty() {
            return true;
        }
        let body_errors: Vec<Span> = self.errors[body_errors_start..]
            .iter()
            .flat_map(|d| d.labels.iter().map(|l| l.span))
            .collect();
        let mut clean = true;
        for suspend in suspends {
            if suspend.deferred_by.is_some()
                && body_errors
                    .iter()
                    .any(|reported| reported.contains(suspend.span))
            {
                // The defer rules (E0905) already reported this exact site; a
                // second diagnostic on the same mistake would only be noise.
                // The section is still not clean, so the lock stays gated.
                // Suspensions the defer rules do NOT cover — a `Channel.send`
                // in a deferred call, say — still get their own E2604 below.
                clean = false;
                continue;
            }
            clean = false;
            let LockSuspend {
                span,
                operation,
                kind,
                deferred_by,
            } = suspend;
            let action = match kind {
                LockEffectKind::Suspend => "suspends the task",
                LockEffectKind::Block => "blocks the scheduler worker",
                LockEffectKind::SuspendOrBlock => {
                    "may suspend the task or block the scheduler worker"
                }
            };
            let help = if deferred_by.is_some() {
                format!("`{operation}` cannot be deferred out of a critical section either")
            } else {
                format!("move the `{operation}` outside the critical section")
            };
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2604,
                    "cannot suspend or block while holding a Willow lock",
                )
                .with_label(Label::primary(
                    span,
                    format!("`{operation}` {action} inside the critical section"),
                ))
                .with_help(help),
            );
        }
        clean
    }

    /// The protected value type `T` when `target_ty` is the lock type `s.mode`
    /// requires, reporting E2602 and returning `None` when it is not.
    fn lock_protected_type(&mut self, s: &LockStmt, target_ty: &Type) -> Option<Type> {
        let required = s.mode.lock_type_name();
        if let Type::Generic(name, args) = target_ty
            && name == required
            && args.len() == 1
        {
            return Some(args[0].clone());
        }
        // A checker error upstream already reported itself; do not pile on.
        if *target_ty == Type::Never {
            return None;
        }

        let mut diagnostic = Diagnostic::new(
            Severity::Error,
            ErrorCode::E2602,
            format!(
                "`{}` requires `{required}<T>`, found `{}`",
                s.mode.keyword(),
                type_name(target_ty)
            ),
        )
        .with_label(Label::primary(
            s.target.span(),
            format!("expected `{required}<T>`"),
        ));
        // The most likely mistake is reaching for the wrong statement form.
        if let Type::Generic(name, args) = target_ty
            && args.len() == 1
        {
            if name == "Mutex" {
                diagnostic = diagnostic.with_help("`Mutex<T>` uses `lock <mutex> as [mut] value`");
            } else if name == "RwLock" {
                diagnostic = diagnostic.with_help(
                    "`RwLock<T>` uses `lock read <rwlock> as value` or `lock write <rwlock> as [mut] value`",
                );
            }
        }
        self.push(diagnostic);
        None
    }

    pub(super) fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                let annotation = s.ty.as_ref().map(|ty| self.normalize_type(ty, s.span));
                // A `let xs: Array<I> = [..]` literal is checked element-wise
                // against `I`, so classes implementing interface `I` are accepted.
                let inferred = match (&annotation, &s.init) {
                    (Some(Type::Array(elem)), Expr::ArrayLiteral(elements, lit_span)) => {
                        self.check_array_literal_expecting(elements, *lit_span, Some(elem.as_ref()))
                    }
                    (Some(ann), _) => self.check_expr_expecting(&s.init, ann),
                    _ => self.check_expr(&s.init),
                };
                let ty = if let Some(ann) = &annotation {
                    self.validate_type(ann, s.span);
                    let channel_ctor_infers_from_annotation = channel_element_type(ann).is_some()
                        && is_untyped_channel_ctor_call(&s.init);
                    if channel_ctor_infers_from_annotation {
                        // `Channel::new()` and `Channel::with_capacity(n)` with
                        // no type argument are typed `Channel<void>`: the
                        // element is not knowable from either call alone. The
                        // annotation is what supplies it, so
                        // record the ANNOTATION as the initializer's type
                        // rather than leaving the placeholder behind
                        // (willow-nk3g). Both backends read the element type
                        // from here to decide the channel's `is_ref` flag, and
                        // a `Channel<String>` left as `Channel<void>` builds an
                        // untraced buffer whose contents the collector is free
                        // to reclaim while they are still queued.
                        self.expr_types.insert(s.init.span(), ann.clone());
                    } else if !self.types_compatible(ann, &inferred) {
                        let code = self.type_mismatch_error_code(ann, &inferred);
                        let message = if code == ErrorCode::E0704 {
                            format!(
                                "cannot assign `{}` to variable `{}` of type `{}`",
                                type_name(&inferred),
                                s.name,
                                type_name(ann)
                            )
                        } else {
                            format!(
                                "mismatched types: expected `{}`, found `{}`",
                                type_name(ann),
                                type_name(&inferred)
                            )
                        };
                        let label = if code == ErrorCode::E0704 {
                            format!(
                                "expected `{}` because of this type annotation",
                                type_name(ann)
                            )
                        } else {
                            format!("expected `{}`", type_name(ann))
                        };
                        self.push(
                            Diagnostic::new(Severity::Error, code, message)
                                .with_label(Label::primary(s.span, label)),
                        );
                    }
                    ann.clone()
                } else {
                    if let Some(diag) = self.unresolved_generic_enum_diagnostic(
                        &s.init,
                        &inferred,
                        s.init.span(),
                        &s.name,
                    ) {
                        self.push(diag);
                    }
                    inferred
                };
                // Record the resolved type of locals inside async fns so the
                // backend can frame-back unannotated live-across-await locals
                // (willow-lpn.5c).
                if self.current_async_context {
                    self.async_local_types.insert(s.span, ty.clone());
                }
                // `_` is a wildcard: evaluate the initializer for side effects but do
                // not bind a variable (allows multiple `let _ = expr;` in the same scope).
                if s.name == "_" {
                    return;
                }
                // E0351: reject redeclaration in the same scope.
                if let Some(_prev) = self.symbols.lookup_var_current_scope(&s.name) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0351,
                            format!("variable `{}` is already defined in this scope", s.name),
                        )
                        .with_label(Label::primary(s.span, "previous definition here")),
                    );
                }
                self.define_var(
                    s.name.clone(),
                    VarInfo {
                        ty,
                        mutable: s.mutable,
                        is_param: false,
                        declaration_span: s.span,
                    },
                );
            }
            Stmt::FieldAssign(s) => {
                let obj_ty = self.check_expr(&s.object);
                let field_ty = self.resolve_field(&obj_ty, &s.field, s.span, true);
                let val_ty = if field_ty == Type::Void {
                    self.check_expr(&s.value)
                } else {
                    self.check_expr_expecting(&s.value, &field_ty)
                };
                if field_ty != Type::Void && !self.types_compatible(&field_ty, &val_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(&field_ty, &val_ty),
                            format!(
                                "mismatched types: expected `{}`, found `{}`",
                                type_name(&field_ty),
                                type_name(&val_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            s.span,
                            format!("expected `{}`", type_name(&field_ty)),
                        )),
                    );
                }
                if obj_ty == Type::Named("PanicInfo".to_string()) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "fields of `PanicInfo` are read-only",
                        )
                        .with_label(Label::primary(
                            s.span,
                            "runtime panic metadata cannot be modified",
                        )),
                    );
                }
            }
            Stmt::StaticFieldAssign(s) => self.check_static_field_assign(s),
            Stmt::SuperInit(s) => self.check_super_init(s),
            Stmt::IndexAssign(s) => {
                let arr_ty = self.check_expr(&s.array);
                let idx_ty = self.check_expr(&s.index);
                if !matches!(idx_ty, Type::I64) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("array index must be `i64`, found `{}`", type_name(&idx_ty)),
                        )
                        .with_label(Label::primary(s.index.span(), "index is not an `i64`")),
                    );
                }
                match &arr_ty {
                    Type::Array(elem) => {
                        let val_ty = self.check_expr_expecting(&s.value, elem);
                        if !self.types_compatible(elem, &val_ty) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    self.type_mismatch_error_code(elem, &val_ty),
                                    format!(
                                        "cannot assign `{}` to an element of `Array<{}>`",
                                        type_name(&val_ty),
                                        type_name(elem)
                                    ),
                                )
                                .with_label(Label::primary(
                                    s.span,
                                    format!("expected `{}`", type_name(elem)),
                                )),
                            );
                        }
                    }
                    Type::Void => {
                        self.check_expr(&s.value);
                    }
                    other => {
                        self.check_expr(&s.value);
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!("cannot index a value of type `{}`", type_name(other)),
                            )
                            .with_label(Label::primary(s.span, "not an array")),
                        );
                    }
                }
            }
            Stmt::Assign(s) => {
                if s.name == "this" {
                    self.push_legacy_this_error(s.span);
                    return;
                }
                // Reject direct assignment to `self`.
                if s.name == "self" {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0552,
                            format!("cannot assign to `{}`", s.name),
                        )
                        .with_label(Label::primary(s.span, "cannot assign to receiver"))
                        .with_help(format!("to mutate fields, use `{}.field = value`", s.name)),
                    );
                    return;
                }
                let info = self.symbols.lookup_var(&s.name).cloned();
                match info {
                    None => self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0350,
                            format!("cannot find variable `{}`", s.name),
                        )
                        .with_label(Label::primary(s.span, "not found in this scope")),
                    ),
                    Some(info) => {
                        if !info.mutable {
                            if info.is_param {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0302,
                                        format!(
                                            "cannot assign to immutable parameter `{}`",
                                            s.name
                                        ),
                                    )
                                    .with_label(Label::primary(
                                        s.span,
                                        "cannot assign to parameter",
                                    ))
                                    .with_help(format!(
                                        "introduce a mutable local variable: `let mut {} = {};`",
                                        s.name, s.name
                                    )),
                                );
                            } else {
                                // Build an insertion span just after "let " in the declaration.
                                let decl = info.declaration_span;
                                let insert_span = Span::new(
                                    decl.start + 4,
                                    decl.start + 4,
                                    decl.line,
                                    decl.col + 4,
                                );
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0301,
                                        format!("cannot assign to immutable variable `{}`", s.name),
                                    )
                                    .with_label(Label::primary(s.span, "cannot assign"))
                                    .with_label(Label::secondary(
                                        info.declaration_span,
                                        "declared immutable here",
                                    ))
                                    .with_help(format!(
                                        "declare it as mutable: `let mut {} = ...`",
                                        s.name
                                    ))
                                    .with_fix(
                                        FixSuggestion::insertion(
                                            insert_span,
                                            "mut ",
                                            "add `mut` here",
                                        ),
                                    ),
                                );
                            }
                        }
                        let got = self.check_expr_expecting(&s.value, &info.ty);
                        if !self.types_compatible(&info.ty, &got) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    self.type_mismatch_error_code(&info.ty, &got),
                                    format!(
                                        "mismatched types: expected `{}`, found `{}`",
                                        type_name(&info.ty),
                                        type_name(&got)
                                    ),
                                )
                                .with_label(Label::primary(
                                    s.span,
                                    format!("expected `{}`", type_name(&info.ty)),
                                )),
                            );
                        }
                    }
                }
            }
            Stmt::If(s) => {
                let cond_ty = self.check_expr(&s.cond);
                if cond_ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0203,
                            format!("condition must be `bool`, found `{}`", type_name(&cond_ty)),
                        )
                        .with_label(Label::primary(
                            s.cond.span(),
                            format!("expected `bool`, found `{}`", type_name(&cond_ty)),
                        ))
                        .with_help("use an explicit comparison, e.g. `!= 0`"),
                    );
                }
                self.check_block(&s.then_block);
                if let Some(else_b) = &s.else_block {
                    self.check_block(else_b);
                }
            }
            Stmt::While(s) => {
                let cond_ty = self.check_expr(&s.cond);
                if cond_ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0203,
                            format!("condition must be `bool`, found `{}`", type_name(&cond_ty)),
                        )
                        .with_label(Label::primary(
                            s.cond.span(),
                            format!("expected `bool`, found `{}`", type_name(&cond_ty)),
                        ))
                        .with_help("use an explicit comparison, e.g. `!= 0`"),
                    );
                }
                self.loop_depth += 1;
                self.check_block(&s.body);
                self.loop_depth -= 1;
            }
            Stmt::For(s) => {
                let iterable_ty = self.check_expr(&s.iterable);
                let elem_ty = match &iterable_ty {
                    Type::Array(elem) => (**elem).clone(),
                    Type::Generic(name, args)
                        if name == "Range" && args.as_slice() == [Type::I64] =>
                    {
                        Type::I64
                    }
                    Type::Void => Type::Void,
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!("cannot iterate over `{}`", type_name(other)),
                            )
                            .with_label(Label::primary(
                                s.iterable.span(),
                                "for-in requires an array or i64 range",
                            ))
                            .with_help(
                                "use `for item in array { ... }` with `Array<T>` or `for n in start..end { ... }`",
                            ),
                        );
                        Type::Void
                    }
                };

                if self.current_async_context {
                    let iter_slot_ty = if is_i64_range_type(&iterable_ty) {
                        Type::I64
                    } else {
                        iterable_ty.clone()
                    };
                    self.async_local_types
                        .insert(s.iter_frame_key(), iter_slot_ty);
                    self.async_local_types
                        .insert(s.index_frame_key(), Type::I64);
                    if s.name != "_" {
                        self.async_local_types.insert(s.name_span, elem_ty.clone());
                    }
                }

                self.symbols.push_scope();
                if s.name != "_" {
                    self.define_var(
                        s.name.clone(),
                        VarInfo {
                            ty: elem_ty,
                            mutable: false,
                            is_param: false,
                            declaration_span: s.name_span,
                        },
                    );
                }
                self.loop_depth += 1;
                self.lexical_block_depth += 1;
                for stmt in &s.body.stmts {
                    self.check_stmt(stmt);
                }
                self.lexical_block_depth -= 1;
                self.loop_depth -= 1;
                self.symbols.pop_scope();
            }
            Stmt::Break(span) | Stmt::Continue(span) => {
                if self.loop_depth == 0 {
                    let kw = if matches!(stmt, Stmt::Break(_)) {
                        "break"
                    } else {
                        "continue"
                    };
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0904,
                            format!("`{kw}` outside of a loop"),
                        )
                        .with_label(Label::primary(
                            *span,
                            format!("`{kw}` is only allowed inside `while` or `for`"),
                        )),
                    );
                }
            }
            Stmt::Lock(s) => self.check_lock_stmt(s),
            Stmt::Defer(d) => {
                // A direct call keeps the original defer contract: receiver
                // and arguments are evaluated at registration. A match/block
                // is evaluated at scope exit, allowing explicit Result
                // handling without changing the parent's result/control flow.
                let supported = matches!(
                    &d.body,
                    DeferBody::Expr(
                        Expr::Call(_) | Expr::MethodCall(_) | Expr::Print(..) | Expr::Match(_)
                    ) | DeferBody::Block(_)
                );
                if !supported {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0905,
                            "`defer` expects a call, match expression, or block",
                        )
                        .with_label(Label::primary(
                            d.body.span(),
                            "use `defer f(args);`, `defer match value { ... }`, or `defer { ... }`",
                        )),
                    );
                }
                if matches!(
                    &d.body,
                    DeferBody::Expr(Expr::Call(call)) if call.callee == "recover"
                ) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0905,
                            "`defer recover();` discards the panic metadata",
                        )
                        .with_label(Label::primary(
                            d.body.span(),
                            "handle `recover()` explicitly in a deferred block or match",
                        ))
                        .with_help(
                            "use `defer match recover() { Some(info) => { ... }, None => {} }`",
                        ),
                    );
                }
                let recovery_capable = defer_body_contains_direct_recover(&d.body);
                if recovery_capable
                    && self.lexical_block_depth == 1
                    && self.current_return_type != Type::Void
                {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0905,
                            "an outermost recovery scope cannot complete a non-void function",
                        )
                        .with_label(Label::primary(
                            d.body.span(),
                            "recovery here would leave the function without a return value",
                        ))
                        .with_help(
                            "put the recovery-capable defer in a nested block and return a value after that block",
                        ),
                    );
                }

                for (span, keyword) in defer_control_flow_violations(&d.body) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0905,
                            format!("`{keyword}` is not allowed inside a `defer`"),
                        )
                        .with_label(Label::primary(
                            span,
                            "a deferred body cannot change its enclosing function or loop",
                        )),
                    );
                }

                if defer_body_contains_suspend(&d.body) {
                    // The registration happens at the defer statement; an
                    // async operation inside the deferred body has no suspension point
                    // to resume from at flush/cancel time (willow-vynv.3).
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0905,
                            "`await` and `select` are not allowed inside a `defer`",
                        )
                        .with_label(Label::primary(
                            d.body.span(),
                            "deferred bodies run synchronously",
                        )),
                    );
                }
                // Reference arguments would act on a hidden COPY of the value
                // (operands are stashed at registration), silently missing the
                // caller's variable — reject them in v1 (review fix).
                let ref_arg_span = match &d.body {
                    DeferBody::Expr(Expr::Call(c)) => c
                        .args
                        .iter()
                        .find(|a| !matches!(a.mode, CallArgMode::Value))
                        .map(|a| a.span),
                    DeferBody::Expr(Expr::MethodCall(m)) => m
                        .args
                        .iter()
                        .find(|a| !matches!(a.mode, CallArgMode::Value))
                        .map(|a| a.span),
                    _ => None,
                };
                if let Some(span) = ref_arg_span {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0905,
                            "reference arguments are not supported in `defer`",
                        )
                        .with_label(Label::primary(
                            span,
                            "the deferred call would mutate a hidden copy, not this variable",
                        ))
                        .with_help("wrap the mutation in a class method and defer that instead"),
                    );
                }
                match &d.body {
                    DeferBody::Expr(expr) => {
                        self.check_expr(expr);
                    }
                    DeferBody::Block(block) => self.check_block(block),
                }
                if let Some((span, operation)) =
                    defer_scheduler_drive_span(&d.body, &self.expr_types)
                {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0905,
                            "scheduler-driving operations are not allowed inside a `defer`",
                        )
                        .with_label(Label::primary(
                            span,
                            format!(
                                "`{operation}` could run or suspend another task during cleanup"
                            ),
                        ))
                        .with_help("perform the operation explicitly before leaving the scope"),
                    );
                }
                // Async defer (willow-vynv.3): operands are stashed into the
                // task frame at registration — record their types (keyed by
                // operand span) so codegen can lay out + GC-mask the slots.
                if self.current_async_context {
                    let mut record = |expr: &Expr| {
                        let ty = self
                            .expr_types
                            .get(&expr.span())
                            .cloned()
                            .unwrap_or(Type::I64);
                        self.async_local_types.insert(expr.span(), ty);
                    };
                    match &d.body {
                        DeferBody::Expr(Expr::Call(c)) => {
                            c.args.iter().for_each(|a| record(&a.expr))
                        }
                        DeferBody::Expr(Expr::MethodCall(m)) => {
                            record(&m.object);
                            m.args.iter().for_each(|a| record(&a.expr));
                        }
                        DeferBody::Expr(Expr::Print(arg, ..)) => record(arg),
                        _ => {}
                    }
                }
                // An ASYNC callee would only SPAWN a task at scope exit — the
                // cleanup body would never be driven to completion. Reject
                // until async defer (Phase 3) defines this (review fix).
                if let Some(span) = defer_async_call_span(&d.body, &self.expr_types) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0905,
                            "cannot `defer` an async call",
                        )
                        .with_label(Label::primary(
                            span,
                            "this would only spawn a task at scope exit, not run it to completion",
                        ))
                        .with_help(
                            "perform the async cleanup explicitly, or defer a synchronous helper",
                        ),
                    );
                }
            }
            Stmt::Return(s) => {
                // In a constructor, a bare `return;` is fine but `return <value>`
                // is rejected (willow-scq2 §8 → E0841).
                if self.in_constructor {
                    if let Some(v) = &s.value {
                        self.check_expr(v);
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0841,
                                "constructor `init` cannot return a value",
                            )
                            .with_label(Label::primary(s.span, "remove the returned value"))
                            .with_help("a constructor implicitly returns the new object"),
                        );
                    }
                    return;
                }
                // `return Result::Ok();` (zero-arg) is the success value of a
                // `Result<void, E>` function: the Ok payload is void, so no
                // argument is required (willow-exg).
                if let Some(Expr::StaticCall(sc)) = &s.value {
                    let returns_result_void =
                        builtin_types::binary_args(&self.current_return_type, B::Result)
                            .is_some_and(|(ok, _)| *ok == Type::Void);
                    if returns_result_void
                        && sc.class == "Result"
                        && sc.method == "Ok"
                        && sc.args.is_empty()
                    {
                        return;
                    }
                }
                let ret_ty = match &s.value {
                    // Resolve an unqualified variant in `return Ok(42)` against
                    // the function's return type (willow-60o.1). Skipped inside a
                    // lambda, where `current_return_type` is not the lambda's.
                    Some(v) if self.lambda_return_stack.is_empty() => {
                        let expected = self.current_return_type.clone();
                        self.check_expr_expecting(v, &expected)
                    }
                    Some(v) => self.check_expr(v),
                    None => Type::Void,
                };
                // Inside a lambda with no annotation: record the return type for inference.
                if let Some(slot) = self.lambda_return_stack.last_mut() {
                    if slot.is_none() {
                        *slot = Some(ret_ty.clone());
                    }
                    return; // don't validate against outer current_return_type
                }
                if !self.types_compatible(&self.current_return_type, &ret_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(&self.current_return_type, &ret_ty),
                            format!(
                                "mismatched types: expected `{}`, found `{}`",
                                type_name(&self.current_return_type),
                                type_name(&ret_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            s.span,
                            format!("expected `{}`", type_name(&self.current_return_type)),
                        )),
                    );
                }
            }
            Stmt::Expr(s) => {
                self.check_expr(&s.expr);
            }
        }
    }

    pub(super) fn check_expr(&mut self, expr: &Expr) -> Type {
        let ty = self.check_expr_inner(expr);
        // Record the authoritative expression type for downstream consumers
        // (HIR lowering, willow-mb5): keyed by span, so the immutable AST
        // never needs to be re-derived.
        self.expr_types.insert(expr.span(), ty.clone());
        ty
    }

    fn check_expr_inner(&mut self, expr: &Expr) -> Type {
        match expr {
            Expr::Integer(_, _) => Type::I64,
            Expr::Float(_, _) => Type::F64,
            Expr::Bool(_, _) => Type::Bool,
            Expr::String(_, _) => Type::String,
            Expr::Var(name, span) => {
                if name == "this" {
                    self.push_legacy_this_error(*span);
                    return Type::Void;
                }
                // Local variable?
                if let Some(info) = self.symbols.lookup_var(name) {
                    return info.ty.clone();
                }
                // Named function used as a value: `apply(10, double)` where `double: fn(...)`
                if let Some(info) = self.symbols.lookup_func(name) {
                    let params = info.params.clone();
                    let ret = info.return_type.clone();
                    return Type::Fn(params, Box::new(ret));
                }
                // Give a specialized error for receiver keywords used outside instance methods.
                if name == "self" {
                    let diag = if self.in_static_method {
                        let where_ = self
                            .current_class
                            .as_deref()
                            .map(|c| format!(" `{}`", c))
                            .unwrap_or_default();
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0831,
                            format!("`self` is not available in static method{}", where_),
                        )
                        .with_label(Label::primary(*span, "`self` in a static method"))
                        .with_help(
                            "static methods have no receiver; use an instance method instead",
                        )
                    } else if self.in_static_initializer {
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0837,
                            "`self` is not available in a static property initializer",
                        )
                        .with_label(Label::primary(*span, "`self` in static initializer"))
                        .with_help("static initializers run before any instance exists")
                    } else {
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0550,
                            "`self` can only be used inside an instance method",
                        )
                        .with_label(Label::primary(*span, "`self` used outside instance method"))
                        .with_help(
                            "declare the method without `static` to make it an instance method",
                        )
                    };
                    self.push(diag);
                    return Type::Void;
                }
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("cannot find variable `{}`", name),
                    )
                    .with_label(Label::primary(*span, "not found in this scope")),
                );
                Type::I64
            }
            Expr::Binary(b) => self.check_binary(b),
            Expr::Unary(u) => self.check_unary(u),
            Expr::Call(c) => {
                // Lexical value bindings win over module/free-function names.
                // Looking up the free function first silently compiled
                // `let f = |...| ...; f(...)` as a direct call when a top-level
                // `fn f` also existed (willow-bv9.1).
                if let Some(var_info) = self.symbols.lookup_var(&c.callee).cloned() {
                    match var_info.ty {
                        Type::Fn(param_types, ret) => {
                            if param_types.len() != c.args.len() {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "function value `{}` takes {} argument(s) but {} were supplied",
                                            c.callee,
                                            param_types.len(),
                                            c.args.len()
                                        ),
                                    )
                                    .with_label(Label::primary(
                                        c.span,
                                        "wrong number of arguments",
                                    )),
                                );
                            }
                            self.check_value_call_args(&param_types, &c.args);
                            return *ret;
                        }
                        ty => {
                            for arg in &c.args {
                                self.check_expr(&arg.expr);
                            }
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "cannot call value `{}` of type `{}`",
                                        c.callee,
                                        type_name(&ty)
                                    ),
                                )
                                .with_label(Label::primary(
                                    c.span,
                                    "this binding is not a function",
                                )),
                            );
                            return Type::Void;
                        }
                    }
                }

                if c.callee == "format" {
                    return self.check_format_call(c);
                }
                // Variadic formatted panic (willow-csax): `panic(spec, args...)`.
                // The one-argument form stays a plain-String builtin call.
                if c.callee == "panic" && c.args.len() != 1 {
                    return self.check_panic_interpolation(c);
                }

                // Direct call to a named function.
                if let Some(info) = self.symbols.lookup_func(&c.callee).cloned() {
                    if info.params.len() != c.args.len() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "function `{}` takes {} argument(s) but {} were supplied",
                                    c.callee,
                                    info.params.len(),
                                    c.args.len()
                                ),
                            )
                            .with_label(Label::primary(c.span, "wrong number of arguments")),
                        );
                    }
                    self.check_call_args_against_param_infos(&info.param_infos, &c.args);
                    // Calling an async fn captures its arguments into a Task that
                    // may cross a worker boundary — enforce Send/Sync (dgwo.4).
                    if info.is_async {
                        self.check_async_capture(&info.param_infos, &c.args);
                    }
                    if let Some(operation) =
                        self.imported_blocking_std_functions.get(&c.callee).copied()
                    {
                        self.record_direct_lock_effect(c.span, operation, LockEffectKind::Block);
                    }
                    self.record_lock_effect_call(
                        FunctionId::free_from_source_name(&c.callee),
                        c.span,
                    );
                    return function_call_return_type(&info);
                }

                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("cannot find function `{}`", c.callee),
                    )
                    .with_label(Label::primary(c.span, "not found in this scope")),
                );
                Type::Void
            }
            Expr::FieldAccess(obj, field_name, span) => {
                let obj_ty = self.check_expr(obj);
                self.resolve_field(&obj_ty, field_name, *span, true)
            }
            Expr::MethodCall(m) => {
                // `.` is instance member access; module items use `::`. Using
                // `math.add(..)` on a module is an error that points at `::`.
                if let Expr::Var(name, _) = &m.object
                    && self.symbols.lookup_var(name).is_none()
                    && self.symbols.lookup_module(name).is_some()
                {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0350,
                            format!("`{name}` is a module; use `::` to access its items"),
                        )
                        .with_label(Label::primary(m.span, "module accessed with `.`"))
                        .with_help(format!(
                            "write `{name}::{method}(...)` instead of `{name}.{method}(...)`",
                            method = m.method
                        )),
                    );
                    return Type::Void;
                }
                let obj_ty = self.check_expr(&m.object);
                self.record_builtin_lock_effect(&obj_ty, m);
                if let Some(ret) = self.check_option_result_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_concurrency_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_array_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_frozen_array_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_map_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_frozen_map_method_call(&obj_ty, m) {
                    return ret;
                }
                self.check_task_method_call(&obj_ty, m);

                for callee in self.method_effect_ids(&obj_ty, &m.method) {
                    self.record_lock_effect_call(callee, m.span);
                }

                self.resolve_method(&obj_ty, &m.method, &m.args, m.span)
            }
            Expr::StaticCall(s) => {
                self.record_static_builtin_lock_effect(s);
                let callee = self.static_method_effect_id(&s.class, &s.method);
                let result =
                    self.resolve_static_call(&s.class, &s.type_args, &s.method, &s.args, s.span);
                if let Some(callee) = callee {
                    self.record_lock_effect_call(callee, s.span);
                }
                result
            }
            Expr::StaticField(s) => self.resolve_static_field_read(&s.class, &s.field, s.span),
            Expr::New(n) => {
                let result = self.check_new(n);
                self.record_lock_effect_call(
                    FunctionId::method(TypeId::from_source_name(&n.class_name), "init"),
                    n.span,
                );
                result
            }
            Expr::ObjectLiteral(o) => self.check_object_literal(o),
            Expr::Await(a) => {
                self.record_direct_lock_effect(a.span, "await", LockEffectKind::Suspend);
                let awaited_ty = self.check_expr(&a.expr);
                if !self.current_async_context {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0801,
                            "`await` can only be used inside an async function",
                        )
                        .with_label(Label::primary(
                            a.span,
                            "`await` used in a non-async function",
                        ))
                        .with_help("make the enclosing function `async`"),
                    );
                    return Type::Void;
                }
                // `Task<T>`/`JoinHandle<T>` yield `T`; `Future<T>` yields `T`;
                // `await task.result()` (willow-qrj9) is the same wait, same
                // frame and same waiter registration as `await task` — only the
                // cancelled mapping differs, so `TaskResult<T>` yields
                // `Result<T, Cancelled>`. `JoinHandle<T>` must be here because
                // it is what E0812 tells users to migrate `join()` to.
                match await_output_type(&awaited_ty) {
                    Some(ty) => ty,
                    None => {
                        let other = awaited_ty;
                        if let Some(replacement) = self.sync_fs_await_replacement(&a.expr) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0803,
                                    "synchronous filesystem operations cannot be awaited",
                                )
                                .with_label(Label::primary(
                                    a.expr.span(),
                                    format!(
                                        "this call returns `{}` immediately",
                                        type_name(&other)
                                    ),
                                ))
                                .with_help(format!(
                                    "use `await {replacement}(...)` to run on the blocking pool, or remove `await` to keep the synchronous call"
                                )),
                            );
                            return Type::Void;
                        }
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0803,
                                format!("cannot await value of type `{}`", type_name(&other)),
                            )
                            .with_label(Label::primary(a.expr.span(), "expected an awaitable"))
                            .with_help(
                                "await only `Task<T>` values returned by async functions or `Future<T>` runtime APIs",
                            ),
                        );
                        Type::Void
                    }
                }
            }
            Expr::Select(s) => {
                self.record_direct_lock_effect(s.span, "select", LockEffectKind::Suspend);
                self.check_select(s);
                Type::Void
            }
            Expr::Print(arg, newline, _) => {
                let arg_ty = self.check_expr(arg);
                // Printable: i64/f64/bool/String and Never (a panicking
                // argument never reaches the print). Option values require
                // explicit matching/unwrapping. Anything else used to compile
                // and silently print NOTHING (willow-0rq9).
                if !matches!(
                    &arg_ty,
                    Type::I64 | Type::F64 | Type::Bool | Type::String | Type::Never
                ) {
                    let fn_name = if *newline { "println" } else { "print" };
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E1402,
                            format!(
                                "cannot {fn_name} a value of type `{}`",
                                type_name(&arg_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            arg.span(),
                            "printable types are i64, f64, bool, and String",
                        ))
                        .with_help(
                            "convert the value first, e.g. with `match`, `.toString()`, or `format`",
                        ),
                    );
                }
                Type::Void
            }
            Expr::Ternary(t) => {
                let cond_ty = self.check_expr(&t.condition);
                if cond_ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0901,
                            format!(
                                "ternary condition must be `bool`, found `{}`",
                                type_name(&cond_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            t.condition.span(),
                            format!("expected `bool`, found `{}`", type_name(&cond_ty)),
                        )),
                    );
                }
                let then_ty = self.check_expr(&t.then_expr);
                let else_ty = self.check_expr(&t.else_expr);
                if let Some(unified_ty) = self.unify_ternary_types(&then_ty, &else_ty) {
                    self.validate_type(&unified_ty, t.span);
                    unified_ty
                } else {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0902,
                            format!(
                                "ternary branches have incompatible types: `{}` and `{}`",
                                type_name(&then_ty),
                                type_name(&else_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            t.else_expr.span(),
                            format!(
                                "expected `{}`, found `{}`",
                                type_name(&then_ty),
                                type_name(&else_ty)
                            ),
                        ))
                        .with_label(Label::secondary(
                            t.then_expr.span(),
                            format!("this branch has type `{}`", type_name(&then_ty)),
                        )),
                    );
                    Type::Void
                }
            }
            Expr::Range(r) => self.check_range(r),
            Expr::Lambda(l) => self.check_lambda(l),
            Expr::Match(m) => self.check_match_expr(m),
            Expr::TryPropagate(inner, span) => self.check_try_propagate(inner, *span),
            Expr::ArrayLiteral(elements, span) => self.check_array_literal(elements, *span),
            Expr::Index(arr, index, span) => self.check_index(arr, index, *span),
        }
    }

    /// Record direct built-in waits using the receiver's checked type. This is
    /// shared by the transitive helper summary and the direct lock-body scan:
    /// a user class that happens to name a method `send` must stay effect-free.
    fn record_builtin_lock_effect(&mut self, obj_ty: &Type, call: &MethodCallExpr) {
        let family = builtin_types::resolve(obj_ty).map(|resolved| resolved.id);
        let effect = match (family, call.method.as_str()) {
            (Some(B::Channel), "send" | "recv") => Some((
                if call.method == "send" {
                    "Channel.send"
                } else {
                    "Channel.recv"
                },
                LockEffectKind::Suspend,
            )),
            (Some(B::BlockingCell), "get" | "set") => Some((
                if call.method == "get" {
                    "BlockingCell.get"
                } else {
                    "BlockingCell.set"
                },
                LockEffectKind::Block,
            )),
            (Some(B::BlockingRwCell), "read" | "write") => Some((
                if call.method == "read" {
                    "BlockingRwCell.read"
                } else {
                    "BlockingRwCell.write"
                },
                LockEffectKind::Block,
            )),
            _ => None,
        };
        if let Some((operation, kind)) = effect {
            self.record_direct_lock_effect(call.span, operation, kind);
        }
    }

    /// Synchronous compatibility file APIs execute on the current native
    /// worker. Their `_async` counterparts only create a Task here and suspend
    /// later at `await`, so they are intentionally absent.
    fn record_static_builtin_lock_effect(&mut self, call: &StaticCallExpr) {
        let is_fs = call.class == "fs"
            || call.class == "std::fs"
            || self
                .imported_std_modules
                .get(&call.class)
                .is_some_and(|module| module.module == "fs");
        if !is_fs {
            return;
        }
        let operation = match call.method.as_str() {
            "read_to_string" => Some("fs::read_to_string"),
            "write_string" => Some("fs::write_string"),
            "exists" => Some("fs::exists"),
            "remove_file" => Some("fs::remove_file"),
            _ => None,
        };
        if let Some(operation) = operation {
            self.record_direct_lock_effect(call.span, operation, LockEffectKind::Block);
        }
    }

    /// Migration target when `await` is applied to an unsuffixed synchronous
    /// std::fs call. The explicit suffix keeps sync compatibility and makes the
    /// scheduling effect visible at the call site (willow-2s3.2).
    fn sync_fs_await_replacement(&self, expr: &Expr) -> Option<String> {
        let sync_method = |method: &str| {
            matches!(
                method,
                "read_to_string" | "write_string" | "exists" | "remove_file"
            )
        };
        match expr {
            Expr::StaticCall(call) => {
                let is_fs = call.class == "fs"
                    || call.class == "std::fs"
                    || self
                        .imported_std_modules
                        .get(&call.class)
                        .is_some_and(|module| module.module == "fs");
                (is_fs && sync_method(&call.method))
                    .then(|| format!("{}::{}_async", call.class, call.method))
            }
            Expr::Call(call) => self
                .imported_blocking_std_functions
                .get(&call.callee)
                .and_then(|operation| operation.rsplit_once("::").map(|(_, method)| method))
                .filter(|method| sync_method(method))
                .map(|method| format!("fs::{method}_async")),
            _ => None,
        }
    }

    pub(super) fn check_range(&mut self, range: &RangeExpr) -> Type {
        // A range is a first-class `Range<i64>` value. Its bounds are checked
        // normally; a nested range bound surfaces as a non-`i64` bound below.
        let start_ty = self.check_expr(&range.start);
        let end_ty = self.check_expr(&range.end);

        if start_ty != Type::I64 || end_ty != Type::I64 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "range bounds must be `i64`, found `{}` and `{}`",
                        type_name(&start_ty),
                        type_name(&end_ty)
                    ),
                )
                .with_label(Label::primary(range.span, "range bounds must be `i64`")),
            );
        }

        range_type()
    }
}

/// Test helper: lex+parse+type-check `source`, returning its diagnostics.
#[cfg(test)]
pub(super) fn check_source(source: &str) -> Vec<Diagnostic> {
    let tokens = Lexer::new(source).tokenize().expect("lexing failed");
    let (program, parse_errors) = Parser::new(tokens).parse();
    assert!(
        parse_errors.is_empty(),
        "unexpected parse errors: {parse_errors:?}"
    );

    let mut checker = TypeChecker::new();
    checker.check_program(&program);
    checker.errors
}

/// Walk executable syntax in a deferred body. Lambda bodies are deliberately
/// opaque: they are separate functions, so their control flow cannot escape
/// the defer body that merely constructs the function value. Nested `defer`
/// bodies validate themselves when `check_stmt` reaches them.
fn walk_defer_body(
    body: &DeferBody,
    on_stmt: &mut impl FnMut(&Stmt),
    on_expr: &mut impl FnMut(&Expr),
) {
    match body {
        DeferBody::Expr(expr) => walk_defer_expr(expr, on_stmt, on_expr),
        DeferBody::Block(block) => walk_defer_block(block, on_stmt, on_expr),
    }
}

fn walk_defer_block(
    block: &Block,
    on_stmt: &mut impl FnMut(&Stmt),
    on_expr: &mut impl FnMut(&Expr),
) {
    for stmt in &block.stmts {
        walk_defer_stmt(stmt, on_stmt, on_expr);
    }
}

fn walk_defer_stmt(stmt: &Stmt, on_stmt: &mut impl FnMut(&Stmt), on_expr: &mut impl FnMut(&Expr)) {
    on_stmt(stmt);
    match stmt {
        Stmt::Let(stmt) => walk_defer_expr(&stmt.init, on_stmt, on_expr),
        Stmt::Assign(stmt) => walk_defer_expr(&stmt.value, on_stmt, on_expr),
        Stmt::FieldAssign(stmt) => {
            walk_defer_expr(&stmt.object, on_stmt, on_expr);
            walk_defer_expr(&stmt.value, on_stmt, on_expr);
        }
        Stmt::SuperInit(stmt) => {
            for arg in &stmt.args {
                walk_defer_expr(&arg.expr, on_stmt, on_expr);
            }
        }
        Stmt::StaticFieldAssign(stmt) => walk_defer_expr(&stmt.value, on_stmt, on_expr),
        Stmt::IndexAssign(stmt) => {
            walk_defer_expr(&stmt.array, on_stmt, on_expr);
            walk_defer_expr(&stmt.index, on_stmt, on_expr);
            walk_defer_expr(&stmt.value, on_stmt, on_expr);
        }
        Stmt::If(stmt) => {
            walk_defer_expr(&stmt.cond, on_stmt, on_expr);
            walk_defer_block(&stmt.then_block, on_stmt, on_expr);
            if let Some(block) = &stmt.else_block {
                walk_defer_block(block, on_stmt, on_expr);
            }
        }
        Stmt::While(stmt) => {
            walk_defer_expr(&stmt.cond, on_stmt, on_expr);
            walk_defer_block(&stmt.body, on_stmt, on_expr);
        }
        Stmt::For(stmt) => {
            walk_defer_expr(&stmt.iterable, on_stmt, on_expr);
            walk_defer_block(&stmt.body, on_stmt, on_expr);
        }
        Stmt::Return(stmt) => {
            if let Some(value) = &stmt.value {
                walk_defer_expr(value, on_stmt, on_expr);
            }
        }
        Stmt::Expr(stmt) => walk_defer_expr(&stmt.expr, on_stmt, on_expr),
        Stmt::Lock(stmt) => {
            walk_defer_expr(&stmt.target, on_stmt, on_expr);
            walk_defer_block(&stmt.body, on_stmt, on_expr);
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Defer(_) => {}
    }
}

/// One user-visible suspension found inside a lock's critical section.
struct LockSuspend {
    /// The offending expression.
    span: Span,
    /// Operation name for the diagnostic, e.g. `await` or `Channel.recv`.
    operation: &'static str,
    /// Whether the operation parks only this task or blocks its native worker.
    kind: LockEffectKind,
    /// The enclosing `defer` statement, when the suspension is deferred rather
    /// than inline. `defer` runs its body at scope exit, still holding the
    /// lock, so it is in scope for E2604 — but the defer rules reject the same
    /// operations themselves, so `check_lock_stmt` suppresses the duplicate.
    deferred_by: Option<Span>,
}

/// Every user-visible suspension inside a lock's critical section, in source
/// order (willow-38w.1.4).
///
/// Stage 1 (willow-38w.1.1) scanned for `await`/`select` syntactically. That
/// missed typed wait operations: `Channel.send`/`recv` suspend the task, while
/// the compatibility `BlockingCell` and `RwLock` accessors block the native
/// worker. Recognising either category needs the receiver's type, so this scan
/// runs AFTER the body has been checked and `expr_types` is populated.
///
/// The task-suspending set below matches the non-preemption
/// `record_coop_suspend` call sites in `backend/cranelift/async_codegen.rs`:
/// `await`, `select`, channel send and channel recv. Native-blocking accessors
/// are included separately because waiting on them stalls a scheduler worker.
/// Compiler-inserted preemption stays legal — it re-polls the same task and
/// never hands the lock to another one. A nested `lock` is its own suspension
/// edge but is already reported as E2605.
fn lock_body_suspend_spans(body: &Block, expr_types: &HashMap<Span, Type>) -> Vec<LockSuspend> {
    let mut found = Vec::new();
    collect_lock_suspends_in_block(body, expr_types, None, &mut found);
    found.sort_by_key(|s| (s.span.start, s.span.end));
    found
}

fn collect_lock_suspends_in_block(
    block: &Block,
    expr_types: &HashMap<Span, Type>,
    deferred_by: Option<Span>,
    out: &mut Vec<LockSuspend>,
) {
    // Two accumulators, because the statement and expression callbacks are live
    // at the same time and cannot both borrow `out`.
    let mut direct = Vec::new();
    let mut from_defers = Vec::new();
    walk_defer_block(
        block,
        &mut |stmt| {
            if let Stmt::Defer(d) = stmt {
                // Attribute to the OUTERMOST defer: that is the statement whose
                // own diagnostics decide whether E2604 would be a duplicate.
                let owner = deferred_by.or(Some(d.span));
                match &d.body {
                    DeferBody::Expr(expr) => {
                        collect_lock_suspends_in_expr(expr, expr_types, owner, &mut from_defers)
                    }
                    DeferBody::Block(body) => {
                        collect_lock_suspends_in_block(body, expr_types, owner, &mut from_defers)
                    }
                }
            }
        },
        &mut |expr| {
            if let Some((operation, kind)) = lock_suspend_operation(expr, expr_types) {
                direct.push(LockSuspend {
                    span: expr.span(),
                    operation,
                    kind,
                    deferred_by,
                });
            }
        },
    );
    out.append(&mut direct);
    out.append(&mut from_defers);
}

fn collect_lock_suspends_in_expr(
    expr: &Expr,
    expr_types: &HashMap<Span, Type>,
    deferred_by: Option<Span>,
    out: &mut Vec<LockSuspend>,
) {
    let mut direct = Vec::new();
    let mut from_defers = Vec::new();
    walk_defer_expr(
        expr,
        &mut |stmt| {
            if let Stmt::Defer(d) = stmt {
                let owner = deferred_by.or(Some(d.span));
                match &d.body {
                    DeferBody::Expr(expr) => {
                        collect_lock_suspends_in_expr(expr, expr_types, owner, &mut from_defers)
                    }
                    DeferBody::Block(body) => {
                        collect_lock_suspends_in_block(body, expr_types, owner, &mut from_defers)
                    }
                }
            }
        },
        &mut |expr| {
            if let Some((operation, kind)) = lock_suspend_operation(expr, expr_types) {
                direct.push(LockSuspend {
                    span: expr.span(),
                    operation,
                    kind,
                    deferred_by,
                });
            }
        },
    );
    out.append(&mut direct);
    out.append(&mut from_defers);
}

/// The suspension this expression performs, or `None` when it never parks the
/// task. Keep in step with the suspension edges the cooperative backend emits.
fn lock_suspend_operation(
    expr: &Expr,
    expr_types: &HashMap<Span, Type>,
) -> Option<(&'static str, LockEffectKind)> {
    match expr {
        Expr::Await(_) => Some(("await", LockEffectKind::Suspend)),
        Expr::Select(_) => Some(("select", LockEffectKind::Suspend)),
        // A channel operation parks whenever the channel is full/empty, and the
        // scheduler may then run a task that wants the same lock.
        Expr::MethodCall(call) if matches!(call.method.as_str(), "send" | "recv") => {
            let on_channel = expr_types
                .get(&call.object.span())
                .is_some_and(|ty| builtin_types::is(ty, B::Channel));
            if !on_channel {
                return None;
            }
            Some((
                if call.method == "send" {
                    "Channel.send"
                } else {
                    "Channel.recv"
                },
                LockEffectKind::Suspend,
            ))
        }
        Expr::MethodCall(call) if matches!(call.method.as_str(), "get" | "set") => {
            let on_blocking_cell = matches!(
                expr_types.get(&call.object.span()),
                Some(Type::Generic(name, _)) if name == "BlockingCell"
            );
            on_blocking_cell.then_some((
                if call.method == "get" {
                    "BlockingCell.get"
                } else {
                    "BlockingCell.set"
                },
                LockEffectKind::Block,
            ))
        }
        Expr::MethodCall(call) if matches!(call.method.as_str(), "read" | "write") => {
            let on_rwlock = matches!(
                expr_types.get(&call.object.span()),
                Some(Type::Generic(name, _)) if name == "BlockingRwCell"
            );
            on_rwlock.then_some((
                if call.method == "read" {
                    "BlockingRwCell.read"
                } else {
                    "BlockingRwCell.write"
                },
                LockEffectKind::Block,
            ))
        }
        _ => None,
    }
}

fn walk_defer_expr(expr: &Expr, on_stmt: &mut impl FnMut(&Stmt), on_expr: &mut impl FnMut(&Expr)) {
    on_expr(expr);
    match expr {
        Expr::Call(call) => {
            for arg in &call.args {
                walk_defer_expr(&arg.expr, on_stmt, on_expr);
            }
        }
        Expr::MethodCall(call) => {
            walk_defer_expr(&call.object, on_stmt, on_expr);
            for arg in &call.args {
                walk_defer_expr(&arg.expr, on_stmt, on_expr);
            }
        }
        Expr::StaticCall(call) => {
            for arg in &call.args {
                walk_defer_expr(&arg.expr, on_stmt, on_expr);
            }
        }
        Expr::New(new) => {
            for arg in &new.args {
                walk_defer_expr(&arg.expr, on_stmt, on_expr);
            }
        }
        Expr::ObjectLiteral(object) => {
            for field in &object.fields {
                walk_defer_expr(&field.value, on_stmt, on_expr);
            }
        }
        Expr::Await(awaited) => walk_defer_expr(&awaited.expr, on_stmt, on_expr),
        Expr::Select(select) => {
            for case in &select.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        walk_defer_expr(channel, on_stmt, on_expr)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        walk_defer_expr(channel, on_stmt, on_expr);
                        walk_defer_expr(value, on_stmt, on_expr);
                    }
                    SelectCaseKind::Timeout { millis } => walk_defer_expr(millis, on_stmt, on_expr),
                    SelectCaseKind::Join { task, .. } => walk_defer_expr(task, on_stmt, on_expr),
                    SelectCaseKind::Default => {}
                }
                walk_defer_block(&case.body, on_stmt, on_expr);
            }
        }
        Expr::Binary(binary) => {
            walk_defer_expr(&binary.lhs, on_stmt, on_expr);
            walk_defer_expr(&binary.rhs, on_stmt, on_expr);
        }
        Expr::Unary(unary) => walk_defer_expr(&unary.expr, on_stmt, on_expr),
        Expr::FieldAccess(object, ..) => walk_defer_expr(object, on_stmt, on_expr),
        Expr::Print(arg, ..) => walk_defer_expr(arg, on_stmt, on_expr),
        Expr::Ternary(ternary) => {
            walk_defer_expr(&ternary.condition, on_stmt, on_expr);
            walk_defer_expr(&ternary.then_expr, on_stmt, on_expr);
            walk_defer_expr(&ternary.else_expr, on_stmt, on_expr);
        }
        Expr::Range(range) => {
            walk_defer_expr(&range.start, on_stmt, on_expr);
            walk_defer_expr(&range.end, on_stmt, on_expr);
        }
        Expr::Match(matched) => {
            walk_defer_expr(&matched.scrutinee, on_stmt, on_expr);
            for arm in &matched.arms {
                match &arm.body {
                    MatchBody::Expr(expr) => walk_defer_expr(expr, on_stmt, on_expr),
                    MatchBody::Block(block) => walk_defer_block(block, on_stmt, on_expr),
                }
            }
        }
        Expr::TryPropagate(inner, _) => walk_defer_expr(inner, on_stmt, on_expr),
        Expr::ArrayLiteral(elements, _) => {
            for element in elements {
                walk_defer_expr(element, on_stmt, on_expr);
            }
        }
        Expr::Index(array, index, _) => {
            walk_defer_expr(array, on_stmt, on_expr);
            walk_defer_expr(index, on_stmt, on_expr);
        }
        // A lambda is a separate function. Scalar leaves contain no children.
        Expr::Lambda(_)
        | Expr::Integer(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::String(..)
        | Expr::Var(..)
        | Expr::StaticField(_) => {}
    }
}

fn defer_control_flow_violations(body: &DeferBody) -> Vec<(Span, &'static str)> {
    let mut stmt_violations = Vec::new();
    let mut expr_violations = Vec::new();
    walk_defer_body(
        body,
        &mut |stmt| match stmt {
            Stmt::Return(stmt) => stmt_violations.push((stmt.span, "return")),
            Stmt::Break(span) => stmt_violations.push((*span, "break")),
            Stmt::Continue(span) => stmt_violations.push((*span, "continue")),
            _ => {}
        },
        &mut |expr| {
            if let Expr::TryPropagate(_, span) = expr {
                expr_violations.push((*span, "?"));
            }
        },
    );
    stmt_violations.extend(expr_violations);
    stmt_violations
}

fn defer_body_contains_suspend(body: &DeferBody) -> bool {
    let mut contains = false;
    walk_defer_body(body, &mut |_| {}, &mut |expr| {
        contains |= matches!(expr, Expr::Await(_) | Expr::Select(_));
    });
    contains
}

/// Whether this deferred AST directly contains the compiler-known
/// `recover()` builtin. The generic defer walker deliberately treats lambdas
/// as opaque, so a helper/lambda cannot inherit the caller's recovery
/// capability (willow-s9ej.3).
pub(crate) fn defer_body_contains_direct_recover(body: &DeferBody) -> bool {
    let mut contains = false;
    walk_defer_body(body, &mut |_| {}, &mut |expr| {
        contains |= matches!(expr, Expr::Call(call) if call.callee == "recover");
    });
    contains
}

fn defer_async_call_span(body: &DeferBody, expr_types: &HashMap<Span, Type>) -> Option<Span> {
    let mut found = None;
    walk_defer_body(body, &mut |_| {}, &mut |expr| {
        if found.is_some()
            || !matches!(
                expr,
                Expr::Call(_) | Expr::MethodCall(_) | Expr::StaticCall(_)
            )
        {
            return;
        }
        if expr_types.get(&expr.span()).is_some_and(|ty| {
            builtin_types::resolve(ty)
                .is_some_and(|resolved| matches!(resolved.id, B::Task | B::Future))
        }) {
            found = Some(expr.span());
        }
    });
    found
}

fn defer_scheduler_drive_span(
    body: &DeferBody,
    expr_types: &HashMap<Span, Type>,
) -> Option<(Span, &'static str)> {
    let mut statement = None;
    let mut expression = None;
    walk_defer_body(
        body,
        &mut |stmt| {
            if statement.is_none()
                && let Stmt::Lock(lock) = stmt
            {
                statement = Some((lock.span, "lock"));
            }
        },
        &mut |expr| {
            if expression.is_some() {
                return;
            }
            let Expr::MethodCall(call) = expr else {
                return;
            };
            let is_channel = expr_types
                .get(&call.object.span())
                .is_some_and(|ty| builtin_types::is(ty, B::Channel));
            if is_channel && matches!(call.method.as_str(), "send" | "recv") {
                expression = Some((
                    call.span,
                    if call.method == "send" {
                        "Channel.send"
                    } else {
                        "Channel.recv"
                    },
                ));
            }
        },
    );
    statement.or(expression)
}

/// Perspectives for the checker's virtual-dispatch resolution in the E2604
/// lock-effect graph (willow-uqzx.1.2).
///
/// Before this, `concrete_method_effect_id` resolved a receiver call by
/// walking *up* from the receiver's declared class to the nearest declaring
/// body. That answers "which body does an instance of exactly this class
/// run", which is not the question a call site asks: the declared type only
/// bounds the dynamic type from *above*, so a subclass override was invisible
/// and a waiting override under a lock compiled clean. Resolution now goes
/// through the shared [`ClassHierarchy::dispatch_targets`], the same union the
/// backend's panic-effect analysis uses — the willow-s9ej.11 defect class.
///
/// Perspectives:
///   d01 direct  — the receiver's own class declares the waiting body
///   d02 union   — a subclass override waits, the base body is pure
///   d03 pure    — base and override both pure, no diagnostic
///   d04 self    — `self.hook()` in a base body reaches a subclass override
///   d05 deep    — three-level chain, the leaf overrides and waits
///   d06 sibling — one of two siblings waits; base receiver sees it
///   d07 narrow  — the pure sibling's own type does not see it
///   d08 inherit — a subclass receiver that does not override reaches the base
///   d09 static  — a static method of the same name is not a dispatch target
///   d10 staticcall — `Self::helper()` still resolves as a direct edge
///   d11 ctor    — `new C()` reaches a waiting `init`
///   d12 iface   — an interface receiver still resolves to its union node
///   d13 once    — N waiting overrides produce exactly one diagnostic
///   d14 unrelated — a same-named method in a disjoint hierarchy is excluded
///   d15 generic — a `Type::Generic` receiver resolves like its named base
///   d16 recur   — a self-recursive waiting method terminates and reports
///   d17 async   — an async override does not propagate into a sync caller
///   d18 sealed  — a non-open class has itself as its only dispatch target
///   d19 outside — the same call outside the lock is legal
///   d20 nested  — the override's wait is reached through an extra helper hop
///   d21 twoargs — the union is keyed by method name, not by signature shape
///   d22 order   — the reported witness is stable regardless of declaration order
///   d23 severity — a caller reaching both kinds of wait reports the blocking one
#[cfg(test)]
mod lock_effect_dispatch_tests {
    use super::*;

    /// The primary-label text of every E2604 in `source`, in report order.
    fn lock_effect_labels(source: &str) -> Vec<String> {
        check_source(source)
            .into_iter()
            .filter(|diagnostic| diagnostic.code == ErrorCode::E2604)
            .map(|diagnostic| {
                diagnostic
                    .labels
                    .first()
                    .map(|label| label.message.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    fn assert_lock_effect_witness(source: &str, expected: &str) {
        let labels = lock_effect_labels(source);
        assert_eq!(
            labels.len(),
            1,
            "expected exactly one E2604 mentioning `{expected}`, got {labels:?}"
        );
        assert!(
            labels[0].contains(expected),
            "expected `{expected}` in {labels:?}"
        );
    }

    fn assert_no_lock_effect(source: &str) {
        let errors = check_source(source);
        assert!(
            errors.iter().all(|error| error.code != ErrorCode::E2604),
            "unexpected lock effect: {errors:?}"
        );
    }

    /// d01: the base line. The receiver's own class declares the waiting body,
    /// which the old upward walk already found.
    #[test]
    fn d01_direct_receiver_class_body_is_reported() {
        assert_lock_effect_witness(
            r#"
class Worker {
    pub fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let worker = new Worker();
    lock m as value {
        worker.run(ch);
    }
}
"#,
            "Worker::run",
        );
    }

    /// d02: the fix. The declared type is pure; the override that actually runs
    /// waits. An upward-only resolution reports nothing here.
    #[test]
    fn d02_subclass_override_is_reported_through_a_base_receiver() {
        assert_lock_effect_witness(
            r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    lock m as value {
        base.run(ch);
    }
}
"#,
            "Derived::run",
        );
    }

    /// d03: the union is not a blanket taint. Every reachable body being pure
    /// keeps the call legal, so the widened resolution cannot be trivially
    /// "report everything".
    #[test]
    fn d03_pure_hierarchy_stays_legal() {
        assert_no_lock_effect(
            r#"
open class Base {
    pub open fn run(self) -> i64 { return 1; }
}

class Derived extends Base {
    pub override fn run(self) -> i64 { return 2; }
}

async fn main() {
    let m = Mutex::new(0);
    let base: Base = new Derived();
    lock m as value {
        let got = base.run();
    }
}
"#,
        );
    }

    /// d04: `self.hook()` inside an inherited body is a virtual call too. This
    /// is the exact shape of the willow-s9ej.11 backend bug.
    #[test]
    fn d04_self_call_reaches_a_subclass_override() {
        assert_lock_effect_witness(
            r#"
open class Base {
    pub open fn hook(self, ch: Channel<i64>) {}

    pub fn run(self, ch: Channel<i64>) {
        self.hook(ch);
    }
}

class Child extends Base {
    pub override fn hook(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Child();
    lock m as value {
        base.run(ch);
    }
}
"#,
            "Base::run",
        );
    }

    /// d05: the union walks the whole subtree, not one level.
    #[test]
    fn d05_deep_hierarchy_leaf_override_is_reported() {
        assert_lock_effect_witness(
            r#"
open class A {
    pub open fn run(self, ch: Channel<i64>) {}
}

open class B extends A {
    pub override fn run(self, ch: Channel<i64>) {}
}

class C extends B {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let a: A = new C();
    lock m as value {
        a.run(ch);
    }
}
"#,
            "C::run",
        );
    }

    /// d06: siblings are unioned. The base-typed receiver can be either one, so
    /// the waiting sibling counts.
    #[test]
    fn d06_waiting_sibling_is_reported_through_the_base() {
        assert_lock_effect_witness(
            r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Quiet extends Base {
    pub override fn run(self, ch: Channel<i64>) {}
}

class Loud extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Quiet();
    lock m as value {
        base.run(ch);
    }
}
"#,
            "Loud::run",
        );
    }

    /// d07: the union is bounded below by the declared type. Naming the pure
    /// sibling excludes its cousin, so the widening stays type-directed.
    #[test]
    fn d07_sibling_type_does_not_see_its_cousin() {
        assert_no_lock_effect(
            r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Quiet extends Base {
    pub override fn run(self, ch: Channel<i64>) {}
}

class Loud extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let quiet = new Quiet();
    lock m as value {
        quiet.run(ch);
    }
}
"#,
        );
    }

    /// d08: a subclass that does not override still runs the inherited body,
    /// so the upward part of resolution must survive the widening.
    #[test]
    fn d08_non_overriding_subclass_reaches_the_inherited_body() {
        assert_lock_effect_witness(
            r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

class Derived extends Base {}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let derived = new Derived();
    lock m as value {
        derived.run(ch);
    }
}
"#,
            "Base::run",
        );
    }

    /// d09: an instance call can never select a static method, so a waiting
    /// static of the same name must stay out of the union.
    #[test]
    fn d09_static_namesake_is_not_a_dispatch_target() {
        assert_no_lock_effect(
            r#"
class Worker {
    pub fn run(self) -> i64 { return 1; }

    pub static fn helper(ch: Channel<i64>) -> i64 {
        return ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let worker = new Worker();
    lock m as value {
        let got = worker.run();
    }
}
"#,
        );
    }

    /// d10: static calls are not virtual and keep their exact single edge.
    #[test]
    fn d10_static_call_still_records_its_edge() {
        assert_lock_effect_witness(
            r#"
class Worker {
    pub static fn helper(ch: Channel<i64>) -> i64 {
        return ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    lock m as value {
        let got = Worker::helper(ch);
    }
}
"#,
            "Worker::helper",
        );
    }

    /// d11: `new C()` names an exact class, so the constructor edge is direct.
    #[test]
    fn d11_constructor_wait_is_reported() {
        assert_lock_effect_witness(
            r#"
class Worker {
    pub value: i64;

    pub init(self, ch: Channel<i64>) {
        self.value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    lock m as value {
        let worker = new Worker(ch);
    }
}
"#,
            "Worker::init",
        );
    }

    /// d12: an interface receiver keeps its synthetic union node, which
    /// `connect_interface_lock_effects` wires to the implementations. The class
    /// hierarchy must not shadow that path.
    #[test]
    fn d12_interface_receiver_uses_the_interface_union_node() {
        assert_lock_effect_witness(
            r#"
interface Receiver extends Send {
    fn run(self, ch: Channel<i64>);
}

class Impl implements Receiver {
    pub fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let receiver: Receiver = new Impl();
    lock m as value {
        receiver.run(ch);
    }
}
"#,
            "Receiver::run",
        );
    }

    /// d13: one call site is one diagnostic. Recording an edge per union member
    /// must not multiply the error the user sees.
    #[test]
    fn d13_many_waiting_overrides_report_once() {
        let labels = lock_effect_labels(
            r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class One extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

class Two extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

class Three extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new One();
    lock m as value {
        base.run(ch);
    }
}
"#,
        );
        assert_eq!(labels.len(), 1, "expected one diagnostic, got {labels:?}");
    }

    /// d14: a same-named method in a disjoint hierarchy is not reachable from
    /// this receiver. Without the subclass test the union would be "every class
    /// that declares the name".
    #[test]
    fn d14_unrelated_hierarchy_is_excluded() {
        assert_no_lock_effect(
            r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub override fn run(self, ch: Channel<i64>) {}
}

class Stranger {
    pub fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    lock m as value {
        base.run(ch);
    }
}
"#,
        );
    }

    /// d15: a `Type::Generic` receiver resolves through its named head, not
    /// through its type arguments. Classes are not generic in v1, so the shape
    /// arises from a generic interface receiver.
    #[test]
    fn d15_generic_receiver_resolves_through_its_named_head() {
        assert_lock_effect_witness(
            r#"
interface Receiver<T> extends Send {
    fn run(self, ch: Channel<i64>);
}

class Impl implements Receiver<i64> {
    pub fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let receiver: Receiver<i64> = new Impl();
    lock m as value {
        receiver.run(ch);
    }
}
"#,
            "Receiver::run",
        );
    }

    /// d16: the fixpoint must terminate on a self-recursive body and still
    /// report its own direct wait.
    #[test]
    fn d16_recursive_method_terminates_and_reports() {
        assert_lock_effect_witness(
            r#"
class Worker {
    pub fn run(self, ch: Channel<i64>, n: i64) -> i64 {
        if n <= 0 {
            return ch.recv();
        }
        return self.run(ch, n - 1);
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let worker = new Worker();
    lock m as value {
        let got = worker.run(ch, 3);
    }
}
"#,
            "Worker::run",
        );
    }

    /// d17: calling an `async` override creates a Task and returns; only a
    /// surrounding `await` suspends. The widened union must keep that rule.
    #[test]
    fn d17_async_override_does_not_propagate() {
        assert_no_lock_effect(
            r#"
open class Base {
    pub open async fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub override async fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    lock m as value {
        base.run(ch);
    }
}
"#,
        );
    }

    /// d18: an `open` class with no subclass in the unit has itself as its only
    /// dispatch target, so the widening neither loses the direct body nor
    /// invents a second witness.
    #[test]
    fn d18_open_class_without_subclasses_reports_itself_once() {
        assert_lock_effect_witness(
            r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base = new Base();
    lock m as value {
        base.run(ch);
    }
}
"#,
            "Base::run",
        );
    }

    /// d19: the union is only consulted for calls written under a lock. The
    /// same waiting override outside the critical section is legal.
    #[test]
    fn d19_call_outside_the_lock_is_legal() {
        assert_no_lock_effect(
            r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    base.run(ch);
    lock m as value {
        let ignored = 1;
    }
}
"#,
        );
    }

    /// d20: the override's wait can be two hops away. The union feeds the same
    /// transitive fixpoint as a direct edge.
    #[test]
    fn d20_override_reaches_its_wait_through_a_helper() {
        assert_lock_effect_witness(
            r#"
fn wait_for(ch: Channel<i64>) -> i64 {
    return ch.recv();
}

open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = wait_for(ch);
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    lock m as value {
        base.run(ch);
    }
}
"#,
            "Derived::run",
        );
    }

    /// d21: a class declaring an unrelated method of another name contributes
    /// nothing; the union is keyed by the called name only.
    #[test]
    fn d21_union_is_keyed_by_the_called_name() {
        assert_no_lock_effect(
            r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub fn other(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    lock m as value {
        base.run(ch);
    }
}
"#,
        );
    }

    /// d22: the reported witness does not depend on declaration order. Both
    /// spellings of the same hierarchy name the same waiting body.
    #[test]
    fn d22_reported_witness_is_declaration_order_independent() {
        let first = lock_effect_labels(
            r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Alpha extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

class Beta extends Base {
    pub override fn run(self, ch: Channel<i64>) {}
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Beta();
    lock m as value {
        base.run(ch);
    }
}
"#,
        );
        let second = lock_effect_labels(
            r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Beta extends Base {
    pub override fn run(self, ch: Channel<i64>) {}
}

class Alpha extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Beta();
    lock m as value {
        base.run(ch);
    }
}
"#,
        );
        assert_eq!(first, second, "witness depends on declaration order");
    }

    /// d23: when one helper reaches both a suspending and a blocking owner, the
    /// blocking one is reported.
    ///
    /// The shared fixpoint (willow-uqzx.1.3) seeds the kind-specific lattice bit
    /// instead of one boolean, and `EffectSummary::witness` answers from the
    /// lowest set bit in the query — `MAY_BLOCK` (bit 1) before `MAY_SUSPEND`
    /// (bit 2). That deliberately outranks the older "lexicographically smallest
    /// reachable owner" rule, which would name `a_wait_suspend` here: blocking a
    /// scheduler worker starves every other task on it, while a suspension only
    /// parks this one, so the worse of the two is the one worth naming. The
    /// min-owner rule still decides ties *within* a bit, which is what d22 pins.
    #[test]
    fn d23_blocking_witness_outranks_a_lexicographically_smaller_suspend() {
        let errors = check_source(
            r#"
class Hub {
    pub fn a_wait_suspend(self, ch: Channel<i64>) {
        let value = ch.recv();
    }

    pub fn z_wait_block(self, flag: BlockingCell<bool>) {
        let ready = flag.get();
    }

    pub fn fanout(self, ch: Channel<i64>, flag: BlockingCell<bool>) {
        self.a_wait_suspend(ch);
        self.z_wait_block(flag);
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let flag = BlockingCell::new(false);
    let hub = new Hub();
    lock m as value {
        hub.fanout(ch, flag);
    }
}
"#,
        );
        let lock_effects = errors
            .iter()
            .filter(|diagnostic| diagnostic.code == ErrorCode::E2604)
            .collect::<Vec<_>>();
        assert_eq!(
            lock_effects.len(),
            1,
            "expected exactly one E2604 for the one call site, got {lock_effects:?}"
        );
        let labels = lock_effects[0]
            .labels
            .iter()
            .map(|label| label.message.clone())
            .collect::<Vec<_>>();
        assert!(
            labels
                .iter()
                .any(|message| message.contains("may block the scheduler worker")),
            "expected the blocking witness, got {labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|message| message.contains("BlockingCell.get")),
            "expected the blocking operation to be named, got {labels:?}"
        );
    }
}
