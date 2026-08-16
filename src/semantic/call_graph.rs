//! One call graph and one virtual-dispatch resolution, shared by every analysis
//! that needs to know what a body can reach (willow-uqzx.1.2, catalog item 4).
//!
//! Before this module the compiler built the same graph twice, with two
//! different dispatch rules that could disagree:
//!
//! * `backend::cranelift::panic_effect` built a backend-symbol keyed graph from
//!   the raw AST at codegen time, with a private `class_bases` map and its own
//!   `dispatch_targets` / `is_same_or_subclass`;
//! * `semantic::type_checker::check` builds `lock_effect_edges` keyed by
//!   [`FunctionId`] as a side effect of the checker's own type-directed walk.
//!
//! The willow-s9ej.11 bug — a `self.method()` fast path that bypassed dispatch
//! resolution and so missed a panicking override — is exactly the defect that a
//! single shared resolution makes impossible to write twice.
//!
//! # Key space
//!
//! [`FunctionId`] is the only key. A free function is `FunctionId::free(name)`;
//! a method (including a constructor, whose name is `init`) is
//! `FunctionId::method(TypeId, name)`. The backend maps that to its linker
//! symbol at the boundary and nowhere else — see
//! `backend::cranelift::panic_effect::backend_symbol`.
//!
//! # Fail-closed
//!
//! A call site with no static target does not silently vanish: it sets
//! [`CallSites::has_unknown`] on the enclosing body. Function values, calls to
//! names this unit cannot resolve, interface dispatch and enum constructors all
//! land there, so an analysis that reads the graph cannot mistake "no edge" for
//! "no effect".

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::parser::ast::*;
use crate::parser::visit::{AstVisitor, walk_expr, walk_stmt};
use crate::semantic::ids::{FunctionId, TypeId};

/// The class inheritance relation and the set of methods each class declares.
///
/// Built either from the raw AST (the backend, which runs after checking and
/// has no symbol tables) or from the checker's symbol tables. Both feed the one
/// [`ClassHierarchy::dispatch_targets`] below, so the two consumers cannot drift
/// apart in how they resolve a virtual call.
#[derive(Debug, Default, Clone)]
pub struct ClassHierarchy {
    /// class -> its base class, if any. Every known class has an entry, so the
    /// key set is the set of concrete dispatch candidates.
    bases: BTreeMap<String, Option<String>>,
    /// (class, method) pairs the class declares a body for. Static methods are
    /// excluded: they are not dispatch targets.
    declared: BTreeSet<(String, String)>,
}

impl ClassHierarchy {
    /// Collect the hierarchy from a parsed program. Constructors are recorded
    /// under the name `init`, matching [`FunctionId`] usage elsewhere.
    pub fn from_program(program: &Program) -> Self {
        let mut hierarchy = Self::default();
        for item in &program.items {
            let Item::Class(class) = item else {
                continue;
            };
            hierarchy.add_class(
                &class.name,
                class.base_class.as_ref().map(|base| base.name()),
            );
            for method in &class.methods {
                if method.is_static {
                    continue;
                }
                hierarchy.add_method(&class.name, &method.name);
            }
            if !class.constructors.is_empty() {
                hierarchy.add_method(&class.name, "init");
            }
        }
        hierarchy
    }

    /// Register a class and its base. Calling this twice for one class keeps the
    /// first base, so a caller merging several sources cannot silently sever a
    /// hierarchy by re-registering a class it only saw as a base name.
    pub fn add_class(&mut self, class: &str, base: Option<&str>) {
        let entry = self.bases.entry(class.to_string()).or_default();
        if entry.is_none() {
            *entry = base.map(str::to_owned);
        }
    }

    /// Register a non-static method body declared directly by `class`.
    pub fn add_method(&mut self, class: &str, method: &str) {
        self.declared
            .insert((class.to_string(), method.to_string()));
    }

    pub fn is_known_class(&self, class: &str) -> bool {
        self.bases.contains_key(class)
    }

    pub fn base_of(&self, class: &str) -> Option<&str> {
        self.bases.get(class).and_then(|base| base.as_deref())
    }

    /// Whether `class` is `base` or inherits from it. A malformed cyclic
    /// hierarchy answers `false` rather than looping; the checker owns the
    /// diagnostic for the cycle itself.
    pub fn is_same_or_subclass(&self, class: &str, base: &str) -> bool {
        let mut current = Some(class);
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if name == base {
                return true;
            }
            if !seen.insert(name) {
                return false;
            }
            current = self.base_of(name);
        }
        false
    }

    /// The nearest class at or above `class` that declares `method`, i.e. the
    /// body an instance of exactly `class` would run. `None` when no class in
    /// the chain declares it (an interface default, a builtin, or an imported
    /// implementation this unit cannot see).
    pub fn declaring_class(&self, class: &str, method: &str) -> Option<&str> {
        let mut current = Some(class);
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name) {
                return None;
            }
            if self
                .declared
                .contains(&(name.to_string(), method.to_string()))
            {
                // Return the borrowed key rather than the loop variable so the
                // lifetime is tied to the hierarchy, not to `class`.
                return self
                    .bases
                    .get_key_value(name)
                    .map(|(stored, _)| stored.as_str());
            }
            current = self.base_of(name);
        }
        None
    }

    /// Every body a virtual call on a `declared_class`-typed receiver can reach.
    ///
    /// The receiver's static type only bounds the dynamic type from above, so
    /// the answer is the union over every concrete class at or below it — an
    /// override in a subclass is reachable even though the call site names the
    /// base. Resolving only `declared_class` upward is the willow-s9ej.11 bug.
    ///
    /// An empty result means "no body in this unit", which callers must treat as
    /// unknown rather than as safe.
    pub fn dispatch_targets(&self, declared_class: &str, method: &str) -> Vec<FunctionId> {
        let mut targets = BTreeSet::new();
        for concrete in self.bases.keys() {
            if !self.is_same_or_subclass(concrete, declared_class) {
                continue;
            }
            if let Some(declaring) = self.declaring_class(concrete, method) {
                // `from_source_name` rather than `local`: a hierarchy built from
                // the checker's symbol tables carries module-qualified class
                // names, and the namespace must land in the ID, not the owner.
                // An AST-built hierarchy has no qualified names, so the two
                // agree there.
                targets.insert(FunctionId::method(
                    TypeId::from_source_name(declaring),
                    method,
                ));
            }
        }
        targets.into_iter().collect()
    }

    /// Class names in a stable order. Used by consumers that need to enumerate
    /// dispatch candidates themselves.
    pub fn classes(&self) -> impl Iterator<Item = &str> {
        self.bases.keys().map(String::as_str)
    }
}

/// What one body can reach.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CallSites {
    /// Statically resolved targets, including the full virtual-dispatch union.
    pub targets: BTreeSet<FunctionId>,
    /// At least one call site in this body has no static target. Fail-closed:
    /// consumers must treat the body as reaching something they cannot see.
    pub has_unknown: bool,
}

/// The call graph of one compilation unit, keyed by [`FunctionId`].
#[derive(Debug, Default, Clone)]
pub struct CallGraph {
    nodes: BTreeMap<FunctionId, CallSites>,
}

impl CallGraph {
    /// Build the graph for `program`. `lambdas` are the lifted lambda bodies,
    /// each already given the id it will be known by; their call sites are still
    /// indirect at every caller, so a lambda contributes a node but never an
    /// inbound static edge.
    pub fn build(
        program: &Program,
        hierarchy: &ClassHierarchy,
        lambdas: &[(FunctionId, &LambdaExpr)],
    ) -> Self {
        let mut graph = Self::default();
        let free_functions: HashSet<&str> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Function(function) => Some(function.name.as_str()),
                _ => None,
            })
            .collect();

        for item in &program.items {
            match item {
                Item::Function(function) => {
                    let id = FunctionId::free(function.name.as_str());
                    let sites = collect_call_sites(
                        &function.params,
                        &function.body,
                        None,
                        hierarchy,
                        &free_functions,
                    );
                    graph.merge(id, sites);
                }
                Item::Class(class) => {
                    let owner = TypeId::local(class.name.as_str());
                    for method in &class.methods {
                        let id = FunctionId::method(owner.clone(), method.name.as_str());
                        let sites = collect_call_sites(
                            &method.params,
                            &method.body,
                            Some(class.name.as_str()),
                            hierarchy,
                            &free_functions,
                        );
                        graph.merge(id, sites);
                    }
                    for constructor in &class.constructors {
                        let id = FunctionId::method(owner.clone(), "init");
                        let sites = collect_call_sites(
                            &constructor.params,
                            &constructor.body,
                            Some(class.name.as_str()),
                            hierarchy,
                            &free_functions,
                        );
                        graph.merge(id, sites);
                    }
                }
                Item::Enum(_) | Item::Interface(_) => {}
            }
        }

        for (id, lambda) in lambdas {
            if let LambdaBody::Block(body) = &lambda.body {
                // The lifted body is walked standalone, so its own parameters
                // have to be seeded here: a call spelled with a parameter name
                // is a call through a function value, not a static edge to a
                // same-named top-level helper.
                let mut sites = collect_call_sites(&[], body, None, hierarchy, &free_functions);
                let param_names: HashSet<&str> =
                    lambda.params.iter().map(|p| p.name.as_str()).collect();
                let shadowed: Vec<FunctionId> = sites
                    .targets
                    .iter()
                    .filter(|target| {
                        target.owner().is_none()
                            && target.namespace().is_none()
                            && param_names.contains(target.name())
                    })
                    .cloned()
                    .collect();
                for target in shadowed {
                    sites.targets.remove(&target);
                    sites.has_unknown = true;
                }
                graph.merge(id.clone(), sites);
            }
        }
        graph
    }

    /// Union `sites` into the node for `id`. Several declarations can share one
    /// id — multiple constructors currently do — and the union keeps an earlier
    /// hazard from being erased by a later declaration.
    pub fn merge(&mut self, id: FunctionId, sites: CallSites) {
        let node = self.nodes.entry(id).or_default();
        node.targets.extend(sites.targets);
        node.has_unknown |= sites.has_unknown;
    }

    pub fn get(&self, id: &FunctionId) -> Option<&CallSites> {
        self.nodes.get(id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &FunctionId> {
        self.nodes.keys()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&FunctionId, &CallSites)> {
        self.nodes.iter()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Walk one body and record what it calls.
fn collect_call_sites(
    params: &[Param],
    body: &Block,
    current_class: Option<&str>,
    hierarchy: &ClassHierarchy,
    free_functions: &HashSet<&str>,
) -> CallSites {
    let mut collector = CallSiteCollector {
        hierarchy,
        free_functions,
        current_class,
        // The parameter scope. `walk_block` opens the body's own scope on top of
        // it, and `exit_scope` refuses to pop this one, so parameters stay
        // visible for the whole body without being confusable with a local.
        scopes: vec![
            params
                .iter()
                .map(|param| (param.name.clone(), Some(param.ty.clone())))
                .collect(),
        ],
        sites: CallSites::default(),
    };
    collector.visit_block(body);
    collector.sites
}

struct CallSiteCollector<'a> {
    hierarchy: &'a ClassHierarchy,
    free_functions: &'a HashSet<&'a str>,
    current_class: Option<&'a str>,
    /// Names visible at the current point, innermost scope last. A binding maps
    /// to the class-bearing type it is known to hold, or `None` when the type is
    /// not one this unit can name.
    ///
    /// This is the walker's real lexical scope stack (willow-uqzx.1.3). It used
    /// to be one flat set collected in a pre-pass, so a name bound *anywhere* in
    /// a body — a `match` arm binding, a lambda parameter — made every
    /// same-named call site in that body look like a call through a function
    /// value. That direction was safe but blunt; the scope stack answers the
    /// question the call site actually asks. The same stack carries the receiver
    /// types, which the flat map got outright wrong: an inner binding used to
    /// leak its class to a same-named outer receiver after the inner scope had
    /// already closed.
    scopes: Vec<HashMap<String, Option<Type>>>,
    sites: CallSites,
}

impl CallSiteCollector<'_> {
    fn record(&mut self, id: FunctionId) {
        self.sites.targets.insert(id);
    }

    fn record_unknown(&mut self) {
        self.sites.has_unknown = true;
    }

    /// The innermost binding of `name`, or `None` when the name is not a local.
    fn lookup(&self, name: &str) -> Option<&Option<Type>> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    /// The class a receiver expression is known to hold. Only the forms the
    /// backend can see without type information are recognised; everything else
    /// is unknown, which is the fail-closed answer.
    fn receiver_class(&self, expression: &Expr) -> Option<String> {
        let ty = match expression {
            Expr::New(new) => Some(Type::Named(new.class_name.clone())),
            Expr::ObjectLiteral(object) => Some(Type::Named(object.class.clone())),
            Expr::Var(name, _) => self.lookup(name).cloned().flatten(),
            _ => None,
        }?;
        match ty {
            // Deliberately `Named` only. A generic instantiation is not treated
            // as a class receiver, which keeps the answer conservative rather
            // than resolving a dispatch the backend cannot name.
            Type::Named(name) if self.hierarchy.is_known_class(&name) => Some(name),
            _ => None,
        }
    }
}

/// Call sites are read off the shared structural walk (willow-uqzx.1.1). Every
/// arm is POST-order: the children are already recorded by `walk_*` before this
/// node is classified.
impl AstVisitor for CallSiteCollector<'_> {
    /// A lambda body is its own node and every call through a function value is
    /// unknown at the call site, so descending here would only double-count.
    /// Its parameters are therefore never bound here either, which is correct:
    /// they are not in scope at any call site this collector still sees.
    fn visit_lambda(&mut self, _lambda: &LambdaExpr) {}

    fn enter_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    /// The parameter scope is not the walker's to close.
    fn exit_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    fn bind(&mut self, name: &str) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), None);
        }
    }

    fn visit_stmt(&mut self, statement: &Stmt) {
        walk_stmt(self, statement);
        match statement {
            Stmt::Let(stmt) => {
                // `walk_stmt` already bound the name in the innermost scope,
                // after the initializer was walked. Fill in its type there.
                if let Some(ty) = stmt.ty.clone().or_else(|| match &stmt.init {
                    Expr::New(new) => Some(Type::Named(new.class_name.clone())),
                    Expr::ObjectLiteral(object) => Some(Type::Named(object.class.clone())),
                    _ => None,
                }) && let Some(slot) = self
                    .scopes
                    .last_mut()
                    .and_then(|scope| scope.get_mut(&stmt.name))
                {
                    *slot = Some(ty);
                }
            }
            // Resolving the base constructor needs the hierarchy plus the
            // enclosing class; `super.init` is rare enough that leaving the edge
            // unknown costs little and cannot be wrong.
            Stmt::SuperInit(_) => self.record_unknown(),
            _ => {}
        }
    }

    fn visit_expr(&mut self, expression: &Expr) {
        walk_expr(self, expression);
        match expression {
            Expr::Call(call) => {
                let callee = call.callee.as_str();
                if self.lookup(callee).is_some() {
                    // Call through a function value: no static target even when
                    // the value is currently a source lambda.
                    self.record_unknown();
                } else if self.free_functions.contains(callee) {
                    self.record(FunctionId::free(callee));
                } else {
                    // Builtins, enum constructors and imported names. The
                    // consumer decides what a name it does not own means.
                    self.record(FunctionId::free_from_source_name(callee));
                }
            }
            Expr::MethodCall(call) => {
                // `self.method()` is virtual just like a call through any other
                // class-typed receiver: an inherited body running on a subclass
                // instance selects that subclass's override (willow-s9ej.11).
                let declared_class = if matches!(&call.object, Expr::Var(name, _) if name == "self")
                {
                    self.current_class.map(str::to_owned)
                } else {
                    self.receiver_class(&call.object)
                };
                let targets = declared_class
                    .map(|class| self.hierarchy.dispatch_targets(&class, &call.method))
                    .unwrap_or_default();
                if targets.is_empty() {
                    // Interface dispatch, builtin collection helpers, and any
                    // receiver whose class this unit cannot name.
                    self.record_unknown();
                } else {
                    for target in targets {
                        self.record(target);
                    }
                }
            }
            Expr::StaticCall(call) => {
                let class = if call.class == "Self" {
                    self.current_class.map(str::to_owned)
                } else {
                    Some(call.class.clone())
                };
                match class {
                    Some(class) => self.record(FunctionId::method(
                        TypeId::from_source_name(&class),
                        call.method.as_str(),
                    )),
                    None => self.record_unknown(),
                }
            }
            Expr::New(new) => self.record(FunctionId::method(
                TypeId::from_source_name(&new.class_name),
                "init",
            )),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    //! Resolution perspectives for the shared call graph (willow-uqzx.1.2).
    //!
    //! 01 direct free call, 02 unknown free name is still an edge the consumer
    //! can classify, 03 a call through a parameter is indirect, 04 a call
    //! through a `let`-bound function value is indirect, 05 a shadowing binding
    //! makes a same-named helper indirect for the whole body, 06 `self.method()`
    //! resolves virtually, 07 an override is included in the union, 08 a deep
    //! hierarchy reaches the deepest override, 09 sibling subclasses union, 10 a
    //! subclass with no override inherits the base body, 11 a call on a base-typed
    //! local reaches the subclass override, 12 a static call resolves to its
    //! class, 13 `Self::` resolves to the enclosing class, 14 `new` resolves to
    //! `init`, 15 a module-qualified free call keeps its namespace, 16 a
    //! module-qualified static call keeps its namespace, 17 a lambda body is not
    //! merged into its definer, 18 an interface-typed receiver is unknown, 19 a
    //! receiver with no inferable class is unknown, 20 `super.init` is unknown,
    //! 21 multiple constructors union into one node, 22 nested expression slots
    //! are all reached, 23 a static method is not a virtual dispatch target, 24 a
    //! cyclic hierarchy terminates, 25 an unrelated class is not a dispatch
    //! candidate.
    //!
    //! Lexical-scope perspectives (willow-uqzx.1.3), which the flat pre-pass
    //! could not express: 26 a sibling scope's binding stops shadowing when that
    //! scope closes, 27 the same for a `match` arm binding, 28 for a lambda
    //! parameter, 29 for a loop variable, 30 for a `lock` binding, 31 a `let`
    //! initializer still sees the outer helper it shadows, 32 an inner
    //! receiver's class does not leak to a same-named outer receiver, 33 a
    //! parameter stays local inside a nested block.
    use super::*;

    fn parse(source: &str) -> Program {
        let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        program
    }

    fn graph_of(source: &str) -> (CallGraph, ClassHierarchy) {
        let program = parse(source);
        let hierarchy = ClassHierarchy::from_program(&program);
        let graph = CallGraph::build(&program, &hierarchy, &[]);
        (graph, hierarchy)
    }

    /// Targets of a free function, rendered as sorted `Display` strings.
    fn targets_of(source: &str, id: FunctionId) -> Vec<String> {
        let (graph, _) = graph_of(source);
        graph
            .get(&id)
            .unwrap_or_else(|| panic!("no node for {id}"))
            .targets
            .iter()
            .map(|target| target.to_string())
            .collect()
    }

    fn free_targets(source: &str, name: &str) -> Vec<String> {
        targets_of(source, FunctionId::free(name))
    }

    fn method_targets(source: &str, class: &str, method: &str) -> Vec<String> {
        targets_of(source, FunctionId::method(TypeId::local(class), method))
    }

    fn has_unknown(source: &str, name: &str) -> bool {
        let (graph, _) = graph_of(source);
        graph
            .get(&FunctionId::free(name))
            .expect("node")
            .has_unknown
    }

    #[test]
    fn p01_direct_free_call_is_a_static_edge() {
        let source = "fn helper() -> i64 { return 1; }\n\
                      fn caller() -> i64 { return helper(); }";
        assert_eq!(free_targets(source, "caller"), vec!["helper"]);
        assert!(!has_unknown(source, "caller"));
    }

    #[test]
    fn p02_an_unowned_name_is_still_recorded_as_an_edge() {
        // `imported_helper` is not declared here. It stays an edge so the
        // consumer can classify it against its own inventory; it is not
        // silently dropped, and it is not folded into `has_unknown` either.
        let source = "fn caller() -> i64 { return imported_helper(1); }";
        assert_eq!(free_targets(source, "caller"), vec!["imported_helper"]);
        assert!(!has_unknown(source, "caller"));
    }

    #[test]
    fn p03_a_call_through_a_parameter_is_indirect() {
        let source = "fn caller(f: fn(i64) -> i64) -> i64 { return f(1); }";
        assert!(free_targets(source, "caller").is_empty());
        assert!(has_unknown(source, "caller"));
    }

    #[test]
    fn p04_a_call_through_a_local_function_value_is_indirect() {
        let source = "fn caller() -> i64 { let f = |x: i64| -> i64 { return x; }; return f(1); }";
        assert!(free_targets(source, "caller").is_empty());
        assert!(has_unknown(source, "caller"));
    }

    #[test]
    fn p05_a_shadowing_binding_makes_a_same_named_helper_indirect() {
        let source = "fn helper(n: i64) -> i64 { return n; }\n\
                      fn caller() -> i64 {\n\
                        let helper = |x: i64| -> i64 { return x + 1; };\n\
                        return helper(1);\n\
                      }";
        assert!(free_targets(source, "caller").is_empty());
        assert!(has_unknown(source, "caller"));
    }

    #[test]
    fn p06_self_method_call_resolves_virtually() {
        let source = "class Work {\n\
                        pub fn leaf(self) -> i64 { return 1; }\n\
                        pub fn run(self) -> i64 { return self.leaf(); }\n\
                      }\nfn main() {}";
        assert_eq!(method_targets(source, "Work", "run"), vec!["Work::leaf"]);
    }

    #[test]
    fn p07_an_override_joins_the_dispatch_union() {
        let source = "open class Base {\n\
                        pub open fn hook(self) -> i64 { return 1; }\n\
                        pub fn run(self) -> i64 { return self.hook(); }\n\
                      }\n\
                      class Derived extends Base {\n\
                        pub override fn hook(self) -> i64 { return 2; }\n\
                      }\nfn main() {}";
        assert_eq!(
            method_targets(source, "Base", "run"),
            vec!["Base::hook", "Derived::hook"]
        );
    }

    #[test]
    fn p08_a_deep_hierarchy_reaches_the_deepest_override() {
        let source = "open class A {\n\
                        pub open fn hook(self) -> i64 { return 1; }\n\
                        pub fn run(self) -> i64 { return self.hook(); }\n\
                      }\n\
                      open class B extends A {}\n\
                      class C extends B { pub override fn hook(self) -> i64 { return 3; } }\n\
                      fn main() {}";
        assert_eq!(
            method_targets(source, "A", "run"),
            vec!["A::hook", "C::hook"]
        );
    }

    #[test]
    fn p09_sibling_subclasses_union_their_overrides() {
        let source = "open class Base {\n\
                        pub open fn hook(self) -> i64 { return 0; }\n\
                        pub fn run(self) -> i64 { return self.hook(); }\n\
                      }\n\
                      class Left extends Base { pub override fn hook(self) -> i64 { return 1; } }\n\
                      class Right extends Base { pub override fn hook(self) -> i64 { return 2; } }\n\
                      fn main() {}";
        assert_eq!(
            method_targets(source, "Base", "run"),
            vec!["Base::hook", "Left::hook", "Right::hook"]
        );
    }

    #[test]
    fn p10_a_subclass_without_an_override_inherits_the_base_body() {
        let source = "open class Base {\n\
                        pub open fn hook(self) -> i64 { return 0; }\n\
                        pub fn run(self) -> i64 { return self.hook(); }\n\
                      }\n\
                      class Plain extends Base {}\nfn main() {}";
        // `Plain` contributes no separate target: it runs `Base::hook`.
        assert_eq!(method_targets(source, "Base", "run"), vec!["Base::hook"]);
    }

    #[test]
    fn p11_a_base_typed_local_still_reaches_the_subclass_override() {
        let source = "open class Base {\n\
                        pub open fn hook(self) -> i64 { return 0; }\n\
                      }\n\
                      class Derived extends Base { pub override fn hook(self) -> i64 { return 1; } }\n\
                      fn caller() -> i64 {\n\
                        let value: Base = new Derived();\n\
                        return value.hook();\n\
                      }\nfn main() {}";
        assert_eq!(
            free_targets(source, "caller"),
            vec!["Base::hook", "Derived::hook", "Derived::init"]
        );
    }

    #[test]
    fn p12_a_static_call_resolves_to_its_class() {
        let source = "class Tool { pub static fn make() -> i64 { return 1; } }\n\
                      fn caller() -> i64 { return Tool::make(); }";
        assert_eq!(free_targets(source, "caller"), vec!["Tool::make"]);
    }

    #[test]
    fn p13_self_qualified_static_call_resolves_to_the_enclosing_class() {
        let source = "class Tool {\n\
                        pub static fn make() -> i64 { return 1; }\n\
                        pub static fn wrap() -> i64 { return Self::make(); }\n\
                      }\nfn main() {}";
        assert_eq!(method_targets(source, "Tool", "wrap"), vec!["Tool::make"]);
    }

    #[test]
    fn p14_new_resolves_to_the_classs_init() {
        let source = "class Cell { pub value: i64; init(self, value: i64) { self.value = value; } }\n\
                      fn caller() -> i64 { let cell = new Cell(1); return cell.value; }";
        assert_eq!(free_targets(source, "caller"), vec!["Cell::init"]);
    }

    #[test]
    fn p15_a_module_qualified_free_call_keeps_its_namespace() {
        let source = "fn caller() -> i64 { return math::add(1, 2); }";
        assert_eq!(free_targets(source, "caller"), vec!["math::add"]);
    }

    #[test]
    fn p16_a_module_qualified_static_call_keeps_its_namespace() {
        let source = "fn caller() -> i64 { return util::Tool::make(); }";
        assert_eq!(free_targets(source, "caller"), vec!["util::Tool::make"]);
    }

    #[test]
    fn p17_a_lambda_body_is_not_merged_into_its_definer() {
        let source = "fn hazard() -> i64 { return 1; }\n\
                      fn caller() -> i64 {\n\
                        let f = |x: i64| -> i64 { return hazard(); };\n\
                        return 0;\n\
                      }";
        // `hazard` is called only from the lambda body, which is its own node.
        assert!(free_targets(source, "caller").is_empty());
    }

    #[test]
    fn p18_an_interface_typed_receiver_is_unknown() {
        let source = "interface Speaker { fn speak(self) -> i64; }\n\
                      fn caller(s: Speaker) -> i64 { return s.speak(); }";
        assert!(free_targets(source, "caller").is_empty());
        assert!(has_unknown(source, "caller"));
    }

    #[test]
    fn p19_a_receiver_with_no_inferable_class_is_unknown() {
        let source = "class Cell {\n\
                        pub value: i64;\n\
                        init(self, value: i64) { self.value = value; }\n\
                        pub fn take(self) -> i64 { return 1; }\n\
                      }\n\
                      fn make() -> Cell { return new Cell(1); }\n\
                      fn caller() -> i64 { return make().take(); }";
        // The receiver is a call result; without types the class is unknown.
        assert!(has_unknown(source, "caller"));
        assert_eq!(free_targets(source, "caller"), vec!["make"]);
    }

    #[test]
    fn p20_super_init_is_unknown() {
        let source = "open class Base { init(self) {} }\n\
                      class Derived extends Base { init(self) { super.init(); } }\n\
                      fn main() {}";
        let (graph, _) = graph_of(source);
        let node = graph
            .get(&FunctionId::method(TypeId::local("Derived"), "init"))
            .expect("node");
        assert!(node.has_unknown);
    }

    #[test]
    fn p21_declarations_sharing_one_id_union_their_edges() {
        // Two bodies merged under one id must not lose the first one's edges.
        let mut graph = CallGraph::default();
        let id = FunctionId::method(TypeId::local("Cell"), "init");
        graph.merge(
            id.clone(),
            CallSites {
                targets: [FunctionId::free("first")].into_iter().collect(),
                has_unknown: false,
            },
        );
        graph.merge(
            id.clone(),
            CallSites {
                targets: [FunctionId::free("second")].into_iter().collect(),
                has_unknown: true,
            },
        );
        let node = graph.get(&id).expect("node");
        assert_eq!(node.targets.len(), 2);
        assert!(node.has_unknown);
    }

    #[test]
    fn p22_every_nested_expression_slot_is_reached() {
        let source = "fn a() -> i64 { return 1; }\n\
                      fn b() -> i64 { return 2; }\n\
                      fn c() -> i64 { return 3; }\n\
                      fn d() -> i64 { return 4; }\n\
                      fn e() -> i64 { return 5; }\n\
                      fn caller(flag: bool) -> i64 {\n\
                        let picked = flag ? a() : b();\n\
                        let mut total = picked + c();\n\
                        while total < 0 { total = total + d(); }\n\
                        return -e();\n\
                      }";
        assert_eq!(
            free_targets(source, "caller"),
            vec!["a", "b", "c", "d", "e"]
        );
    }

    #[test]
    fn p23_a_static_method_is_not_a_virtual_dispatch_target() {
        let program =
            parse("class Tool { pub static fn make() -> i64 { return 1; } }\nfn main() {}");
        let hierarchy = ClassHierarchy::from_program(&program);
        assert!(hierarchy.dispatch_targets("Tool", "make").is_empty());
    }

    #[test]
    fn p24_a_cyclic_hierarchy_terminates() {
        // The checker owns the cycle diagnostic; resolution must not hang.
        let mut hierarchy = ClassHierarchy::default();
        hierarchy.add_class("A", Some("B"));
        hierarchy.add_class("B", Some("A"));
        hierarchy.add_method("A", "hook");
        assert!(!hierarchy.is_same_or_subclass("A", "Missing"));
        // A name no class in the cycle declares must terminate, not loop.
        assert!(hierarchy.declaring_class("B", "missing").is_none());
        // The cycle still answers the reachable case correctly.
        assert_eq!(hierarchy.declaring_class("B", "hook"), Some("A"));
        assert!(hierarchy.dispatch_targets("A", "missing").is_empty());
    }

    #[test]
    fn p25_an_unrelated_class_is_not_a_dispatch_candidate() {
        let source = "open class Base { pub open fn hook(self) -> i64 { return 0; } }\n\
                      class Other { pub fn hook(self) -> i64 { return 1; } }\n\
                      fn main() {}";
        let program = parse(source);
        let hierarchy = ClassHierarchy::from_program(&program);
        assert_eq!(
            hierarchy
                .dispatch_targets("Base", "hook")
                .iter()
                .map(FunctionId::to_string)
                .collect::<Vec<_>>(),
            vec!["Base::hook"]
        );
    }

    #[test]
    fn p26_a_sibling_scope_binding_does_not_shadow_after_it_closes() {
        let source = "fn helper() -> i64 { return 1; }\n\
                      fn caller() -> i64 {\n\
                        if true {\n\
                          let helper = |x: i64| -> i64 { return x; };\n\
                          let inner = helper(1);\n\
                        }\n\
                        return helper();\n\
                      }";
        // The inner call is still indirect, so `has_unknown` stays set — but the
        // call after the block reaches the real helper. The flat pre-pass lost
        // that edge for the whole body.
        assert_eq!(free_targets(source, "caller"), vec!["helper"]);
        assert!(has_unknown(source, "caller"));
    }

    #[test]
    fn p27_a_match_arm_binding_does_not_shadow_outside_its_arm() {
        let source = "fn helper() -> i64 { return 1; }\n\
                      fn caller(v: Option<i64>) -> i64 {\n\
                        let picked = match v { Some(helper) => helper, None => 0 };\n\
                        return picked + helper();\n\
                      }";
        assert_eq!(free_targets(source, "caller"), vec!["helper"]);
        assert!(!has_unknown(source, "caller"));
    }

    #[test]
    fn p28_a_lambda_parameter_does_not_shadow_outside_the_lambda() {
        let source = "fn helper() -> i64 { return 1; }\n\
                      fn caller() -> i64 {\n\
                        let f = |helper: i64| -> i64 { return helper; };\n\
                        return helper();\n\
                      }";
        assert_eq!(free_targets(source, "caller"), vec!["helper"]);
        assert!(!has_unknown(source, "caller"));
    }

    #[test]
    fn p29_a_loop_variable_does_not_shadow_after_the_loop() {
        let source = "fn helper() -> i64 { return 1; }\n\
                      fn caller() -> i64 {\n\
                        let mut total = 0;\n\
                        for helper in 0..3 {\n\
                          total = total + helper;\n\
                        }\n\
                        return total + helper();\n\
                      }";
        assert_eq!(free_targets(source, "caller"), vec!["helper"]);
        assert!(!has_unknown(source, "caller"));
    }

    #[test]
    fn p30_a_lock_binding_does_not_shadow_after_the_section() {
        let source = "fn helper() -> i64 { return 1; }\n\
                      fn caller(m: Mutex<i64>) -> i64 {\n\
                        let mut seen = 0;\n\
                        lock m as helper {\n\
                          seen = helper;\n\
                        }\n\
                        return seen + helper();\n\
                      }";
        assert_eq!(free_targets(source, "caller"), vec!["helper"]);
        assert!(!has_unknown(source, "caller"));
    }

    #[test]
    fn p31_a_shadowing_initializer_still_sees_the_outer_helper() {
        // `let helper = helper();` binds *after* its initializer is walked, so
        // the initializer is a static edge and only later spellings are locals.
        let source = "fn helper() -> i64 { return 1; }\n\
                      fn caller() -> i64 {\n\
                        let helper = helper();\n\
                        return helper;\n\
                      }";
        assert_eq!(free_targets(source, "caller"), vec!["helper"]);
        assert!(!has_unknown(source, "caller"));
    }

    #[test]
    fn p32_an_inner_receiver_class_does_not_leak_to_the_outer_binding() {
        // The receiver types ride the same scope stack. With one flat map the
        // inner `obj` kept its class after its scope closed, so the outer call
        // dispatched on `Derived` and `Other::run` was never recorded.
        let source = "open class Base { pub open fn run(self) -> i64 { return 0; } }\n\
                      class Derived extends Base { pub override fn run(self) -> i64 { return 1; } }\n\
                      class Other { pub fn run(self) -> i64 { return 2; } }\n\
                      fn caller() -> i64 {\n\
                        let obj = new Other();\n\
                        if true {\n\
                          let obj = new Derived();\n\
                          let inner = obj.run();\n\
                        }\n\
                        return obj.run();\n\
                      }\nfn main() {}";
        let targets = free_targets(source, "caller");
        assert!(
            targets.contains(&"Other::run".to_string()),
            "outer receiver lost its own class: {targets:?}"
        );
        assert!(
            targets.contains(&"Derived::run".to_string()),
            "inner receiver lost its class: {targets:?}"
        );
    }

    #[test]
    fn p33_a_parameter_is_still_local_inside_a_nested_block() {
        // The parameter scope sits under every block scope and `exit_scope`
        // refuses to pop it, so a parameter shadows for the whole body.
        let source = "fn helper() -> i64 { return 1; }\n\
                      fn caller(helper: fn() -> i64) -> i64 {\n\
                        if true {\n\
                          let inner = helper();\n\
                        }\n\
                        return 0;\n\
                      }";
        assert!(free_targets(source, "caller").is_empty());
        assert!(has_unknown(source, "caller"));
    }
}
