//! One call graph and one virtual-dispatch resolution, shared by every analysis
//! that needs to know what a body can reach (willow-uqzx.1.2, catalog item 4).
//!
//! Before this module the compiler built the same graph twice, with two
//! different dispatch rules that could disagree:
//!
//! * `backend::cranelift::panic_effect` built a backend-symbol keyed graph from
//!   the raw AST at codegen time, with a private `class_bases` map and its own
//!   `dispatch_targets` / `is_same_or_subclass`;
//! * `semantic::type_checker::check` builds typed call edges keyed by
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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use std::sync::Mutex;

use crate::parser::ast::*;
use crate::semantic::ids::{FunctionId, TypeId};

/// The class inheritance relation and the set of methods each class declares.
///
/// Built either from the raw AST (the backend, which runs after checking and
/// has no symbol tables) or from the checker's symbol tables. Both feed the one
/// [`ClassHierarchy::dispatch_targets`] below, so the two consumers cannot drift
/// apart in how they resolve a virtual call.
#[derive(Debug, Default)]
pub struct ClassHierarchy {
    /// class -> its base class, if any. Every known class has an entry, so the
    /// key set is the set of concrete dispatch candidates.
    bases: BTreeMap<String, Option<String>>,
    /// (class, method) pairs the class declares a body for. Static methods are
    /// excluded: they are not dispatch targets.
    declared: BTreeSet<(String, String)>,
    dispatch: Mutex<Option<DispatchAnalysis>>,
}

// Bound memoized queries independently of call-site count. Each result has at
// most one entry per declaration; FIFO eviction only affects recomputation.
const DISPATCH_CACHE_ENTRIES: usize = 256;

#[derive(Debug, Default)]
struct DispatchAnalysis {
    children: HashMap<String, Vec<String>>,
    owners: HashMap<String, HashSet<String>>,
    results: HashMap<(String, String), Vec<FunctionId>>,
    order: VecDeque<(String, String)>,
    #[cfg(test)]
    visits: usize,
}

impl Clone for ClassHierarchy {
    fn clone(&self) -> Self {
        Self {
            bases: self.bases.clone(),
            declared: self.declared.clone(),
            dispatch: Mutex::default(),
        }
    }
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
        *self.dispatch.get_mut().expect("dispatch cache lock") = None;
        let entry = self.bases.entry(class.to_string()).or_default();
        if entry.is_none() {
            *entry = base.map(str::to_owned);
        }
    }

    /// Register a non-static method body declared directly by `class`.
    pub fn add_method(&mut self, class: &str, method: &str) {
        *self.dispatch.get_mut().expect("dispatch cache lock") = None;
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
        let mut cached = self.dispatch.lock().expect("dispatch cache lock");
        let analysis = cached.get_or_insert_with(|| {
            let mut analysis = DispatchAnalysis::default();
            for (class, base) in &self.bases {
                if let Some(base) = base {
                    analysis
                        .children
                        .entry(base.clone())
                        .or_default()
                        .push(class.clone());
                }
            }
            for (class, method) in &self.declared {
                analysis
                    .owners
                    .entry(method.clone())
                    .or_default()
                    .insert(class.clone());
            }
            analysis
        });
        let key = (declared_class.to_owned(), method.to_owned());
        if let Some(targets) = analysis.results.get(&key) {
            return targets.clone();
        }
        let Some(owners) = analysis.owners.get(method) else {
            return Vec::new();
        };
        #[cfg(test)]
        let visits = std::cell::Cell::new(0usize);
        let targets = resolve_dispatch_targets(
            declared_class,
            method,
            |class| {
                #[cfg(test)]
                visits.set(visits.get() + 1);
                Ok((
                    self.is_known_class(class),
                    self.base_of(class).map(str::to_owned),
                    owners.contains(class),
                ))
            },
            |class| Ok(analysis.children.get(class).cloned().unwrap_or_default()),
        )
        .expect("in-memory dispatch inventory");
        #[cfg(test)]
        {
            analysis.visits += visits.get();
        }
        if analysis.results.len() == DISPATCH_CACHE_ENTRIES {
            let oldest = analysis
                .order
                .pop_front()
                .expect("cached query has an order entry");
            analysis.results.remove(&oldest);
        }
        analysis.order.push_back(key.clone());
        analysis.results.insert(key, targets.clone());
        targets
    }

    pub(crate) fn dispatch_declarations(
        &self,
    ) -> BTreeMap<String, crate::compiler_db::dispatch::DispatchDeclaration> {
        let mut declarations: BTreeMap<_, _> = self
            .bases
            .iter()
            .map(|(class, base)| {
                (
                    class.clone(),
                    crate::compiler_db::dispatch::DispatchDeclaration {
                        exists: true,
                        base: base.clone(),
                        methods: Default::default(),
                    },
                )
            })
            .collect();
        for (class, method) in &self.declared {
            if let Some(declaration) = declarations.get_mut(class) {
                declaration.methods.insert(method.clone());
            }
        }
        declarations
    }

    /// Class names in a stable order. Used by consumers that need to enumerate
    /// dispatch candidates themselves.
    pub fn classes(&self) -> impl Iterator<Item = &str> {
        self.bases.keys().map(String::as_str)
    }
}

/// Shared dispatch algorithm for frozen and revision-tracked inventories.
/// Only the root's ancestors and descendants can contribute a target.
pub(crate) fn resolve_dispatch_targets(
    root: &str,
    method: &str,
    mut declaration: impl FnMut(&str) -> anyhow::Result<(bool, Option<String>, bool)>,
    mut children: impl FnMut(&str) -> anyhow::Result<Vec<String>>,
) -> anyhow::Result<Vec<FunctionId>> {
    let mut targets = BTreeSet::new();
    let mut current = Some(root.to_owned());
    let mut ancestors = HashSet::new();
    while let Some(class) = current {
        if !ancestors.insert(class.clone()) {
            break;
        }
        let (exists, base, declares) = declaration(&class)?;
        if declares {
            if exists {
                targets.insert(FunctionId::method(TypeId::from_source_name(&class), method));
            }
            break;
        }
        current = base;
    }
    let mut pending = vec![root.to_owned()];
    let mut seen = HashSet::new();
    while let Some(class) = pending.pop() {
        if !seen.insert(class.clone()) {
            continue;
        }
        let (exists, _, declares) = declaration(&class)?;
        if exists && declares {
            targets.insert(FunctionId::method(TypeId::from_source_name(&class), method));
        }
        pending.extend(children(&class)?);
    }
    Ok(targets.into_iter().collect())
}

/// What one body can reach.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
        let graph = crate::semantic::TypeChecker::resolved_effect_graph(&program);
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
    fn dispatch_reuses_chain_wide_and_unrelated_queries() {
        for n in [32, 64, 128, 256] {
            for wide in [false, true] {
                let mut hierarchy = ClassHierarchy::default();
                hierarchy.add_class("C0", None);
                hierarchy.add_method("C0", "m");
                for i in 1..n {
                    let parent = if wide {
                        "C0".to_owned()
                    } else {
                        format!("C{}", i - 1)
                    };
                    hierarchy.add_class(&format!("C{i}"), Some(&parent));
                    hierarchy.add_class(&format!("Unrelated{i}"), None);
                    hierarchy.add_method(&format!("Unrelated{i}"), "m");
                }
                let expected = vec![FunctionId::method(TypeId::local("C0"), "m")];
                for _ in 0..n {
                    assert_eq!(hierarchy.dispatch_targets("C0", "m"), expected);
                }
                let cache = hierarchy.dispatch.lock().unwrap();
                let analysis = cache.as_ref().unwrap();
                assert_eq!(analysis.visits, n + 1);
                eprintln!(
                    "dispatch n={n} wide={wide} queries={n} visits={}",
                    analysis.visits
                );
            }
        }
    }

    #[test]
    fn dispatch_matches_concrete_union_on_malformed_hierarchies() {
        let mut seed = 7u64;
        for _ in 0..100 {
            let mut hierarchy = ClassHierarchy::default();
            for i in 0..12 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let parent = format!("C{}", (seed >> 32) % 15);
                hierarchy.add_class(&format!("C{i}"), Some(&parent));
                if seed & 3 == 0 {
                    hierarchy.add_method(&format!("C{i}"), "m");
                }
            }
            hierarchy.add_method("C14", "m"); // absent declaration owner
            for i in 0..16 {
                let class = format!("C{i}");
                let expected: BTreeSet<_> = hierarchy
                    .classes()
                    .filter(|concrete| hierarchy.is_same_or_subclass(concrete, &class))
                    .filter_map(|concrete| hierarchy.declaring_class(concrete, "m"))
                    .map(|owner| FunctionId::method(TypeId::from_source_name(owner), "m"))
                    .collect();
                assert_eq!(
                    hierarchy.dispatch_targets(&class, "m"),
                    expected.into_iter().collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn dispatch_cache_invalidates_and_bounds_queries() {
        let mut hierarchy = ClassHierarchy::default();
        hierarchy.add_class("pkg::Base", None);
        hierarchy.add_method("pkg::Base", "m");
        let base = FunctionId::method(TypeId::from_source_name("pkg::Base"), "m");
        assert_eq!(hierarchy.dispatch_targets("pkg::Base", "m"), vec![base]);
        hierarchy.add_class("pkg::Child", Some("pkg::Base"));
        hierarchy.add_method("pkg::Child", "m");
        let child = FunctionId::method(TypeId::from_source_name("pkg::Child"), "m");
        let expected: Vec<_> = BTreeSet::from([base, child]).into_iter().collect();
        assert_eq!(hierarchy.dispatch_targets("pkg::Base", "m"), expected);
        let mut clone = hierarchy.clone();
        clone.add_class("Other", Some("pkg::Base"));
        clone.add_method("Other", "m");
        assert_eq!(clone.dispatch_targets("pkg::Base", "m").len(), 3);
        assert_eq!(hierarchy.dispatch_targets("pkg::Base", "m"), expected);
        for i in 0..2 * DISPATCH_CACHE_ENTRIES {
            assert!(
                hierarchy
                    .dispatch_targets(&format!("Missing{i}"), "m")
                    .is_empty()
            );
        }
        assert_eq!(
            hierarchy
                .dispatch
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .results
                .len(),
            DISPATCH_CACHE_ENTRIES
        );
        assert_eq!(hierarchy.dispatch_targets("pkg::Base", "m"), expected);
        hierarchy.add_class("A", Some("B"));
        hierarchy.add_class("B", Some("A"));
        hierarchy.add_class("Leaf", Some("A"));
        hierarchy.add_method("B", "m");
        hierarchy.add_method("Leaf", "m");
        for class in ["A", "B"] {
            let targets = hierarchy.dispatch_targets(class, "m");
            assert_eq!(targets.len(), 2);
            assert!(targets.contains(&FunctionId::method(TypeId::local("B"), "m")));
            assert!(targets.contains(&FunctionId::method(TypeId::local("Leaf"), "m")));
        }
        assert!(hierarchy.dispatch_targets("A", "missing").is_empty());
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
    fn p18_an_interface_typed_receiver_reaches_its_dispatch_union() {
        let source = "interface Speaker { fn speak(self) -> i64; }\n\
                      fn caller(s: Speaker) -> i64 { return s.speak(); }";
        assert_eq!(free_targets(source, "caller"), vec!["Speaker::speak"]);
        assert!(!has_unknown(source, "caller"));
    }

    #[test]
    fn p19_a_call_result_receiver_uses_its_checked_class() {
        let source = "class Cell {\n\
                        pub value: i64;\n\
                        init(self, value: i64) { self.value = value; }\n\
                        pub fn take(self) -> i64 { return 1; }\n\
                      }\n\
                      fn make() -> Cell { return new Cell(1); }\n\
                      fn caller() -> i64 { return make().take(); }";
        assert!(!has_unknown(source, "caller"));
        assert_eq!(free_targets(source, "caller"), vec!["make", "Cell::take"]);
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
            id,
            CallSites {
                targets: [FunctionId::free("first")].into_iter().collect(),
                has_unknown: false,
            },
        );
        graph.merge(
            id,
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
