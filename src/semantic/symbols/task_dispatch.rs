//! Snapshot-local async dispatch effects, shared by all body checkers.
use super::{SymbolRead, SymbolTable, record_read};
use crate::parser::ast::Type;
use crate::semantic::ids::{FunctionId, TypeId};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub(super) struct TaskDispatch {
    children: Option<HashMap<TypeId, Vec<TypeId>>>,
    results: HashMap<FunctionId, bool>,
    #[cfg(test)]
    visits: usize,
}

impl SymbolTable {
    /// True when an instance call can select an async implementation, including
    /// unknown implementations of exported interfaces/open classes. Returning
    /// Task alone is insufficient: a synchronous body can return an independent
    /// task without retaining its reference arguments.
    ///
    /// Record this aggregate read even on cache hits, so adding an async override
    /// or changing an implementation invalidates callers during body reuse.
    /// Declaration mutations detach the cache; body forks share immutable results.
    pub(crate) fn dispatch_may_start_task(&self, class: &str, method: &str) -> bool {
        record_read(|| SymbolRead::TaskDispatch(class.to_owned(), method.to_owned()));
        let canonical = |name: &str| {
            let id = TypeId::from_source_name(name);
            self.classes
                .get(&id)
                .map(|info| info.name.as_str())
                .or_else(|| self.interfaces.get(&id).map(|info| info.name.as_str()))
                .map_or(id, TypeId::from_source_name)
        };
        let class = canonical(class);
        let key = FunctionId::method(class, method);
        let mut cache = self.task_dispatch.borrow_mut();
        if let Some(&result) = cache.results.get(&key) {
            return result;
        }
        let open = |class: TypeId| {
            self.interfaces.get(&class).is_some_and(|info| info.public)
                || self
                    .classes
                    .get(&class)
                    .is_some_and(|info| info.public && info.is_open)
        };
        // Importer implementations need no local descendant enumeration.
        if open(class) {
            cache.results.insert(key, true);
            return true;
        }
        if cache.children.is_none() {
            let mut children: HashMap<TypeId, Vec<TypeId>> = HashMap::new();
            let mut seen = HashSet::new();
            for info in self.classes.values() {
                let child = TypeId::from_source_name(&info.name);
                if !seen.insert(child) {
                    continue;
                }
                if let Some(base) = &info.base_class {
                    children.entry(canonical(base)).or_default().push(child);
                }
                for interface in &info.implements {
                    if let Type::Named(name) | Type::Generic(name, _) = interface {
                        children.entry(canonical(name)).or_default().push(child);
                    }
                }
            }
            seen.clear();
            for info in self.interfaces.values() {
                let child = TypeId::from_source_name(&info.name);
                if seen.insert(child) {
                    for base in &info.extends {
                        children.entry(canonical(base)).or_default().push(child);
                    }
                }
            }
            cache.children = Some(children);
        }
        let mut work = vec![(class, false)];
        let mut active = HashSet::new();
        while let Some((class, exiting)) = work.pop() {
            let key = FunctionId::method(class, method);
            if cache.results.contains_key(&key) {
                continue;
            }
            if exiting {
                let result = cache
                    .children
                    .as_ref()
                    .unwrap()
                    .get(&class)
                    .is_some_and(|children| {
                        children.iter().any(|child| {
                            cache
                                .results
                                .get(&FunctionId::method(*child, method))
                                .copied()
                                .unwrap_or(false)
                        })
                    });
                cache.results.insert(key, result);
                active.remove(&class);
                continue;
            }
            // Invalid declaration cycles are diagnosed elsewhere; stay bounded.
            if !active.insert(class) {
                continue;
            }
            #[cfg(test)]
            {
                cache.visits += 1;
            }
            // A dependency closure cannot see implementations in its importers.
            // Exported interfaces and extensible classes therefore have an open
            // implementation set, even when every currently visible body is sync.
            if open(class) {
                cache.results.insert(key, true);
                active.remove(&class);
                continue;
            }
            // The defining body can be inherited from outside this descendant
            // graph (an interface implemented by a subclass, for example).
            let is_async = self
                .resolved_method_class(&class.to_string(), method)
                .and_then(|owner| self.classes.get(&TypeId::from_source_name(&owner)))
                .and_then(|info| info.methods.get(method))
                .is_some_and(|info| info.is_async);
            if is_async {
                cache.results.insert(key, true);
                active.remove(&class);
                continue;
            }
            work.push((class, true));
            if let Some(children) = cache.children.as_ref().unwrap().get(&class) {
                work.extend(children.iter().map(|child| (*child, false)));
            }
        }
        cache
            .results
            .get(&FunctionId::method(class, method))
            .copied()
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::symbols::SymbolReadCapture;

    fn symbols(source: &str) -> SymbolTable {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut checker = crate::semantic::type_checker::TypeChecker::new();
        checker.check_module_program(&program);
        checker.symbols
    }

    #[test]
    fn task_dispatch_open_world_and_visibility_invalidation() {
        let mut symbols = symbols(
            "interface Reading { fn read(self) -> Task<String>; } open class Base { pub open fn read(self) -> Task<String> { return value(); } } class Closed { pub fn read(self) -> Task<String> { return value(); } } async fn value() -> String { return \"v\"; }",
        );
        assert!(!symbols.dispatch_may_start_task("Reading", "read"));
        assert!(!symbols.dispatch_may_start_task("Base", "read"));
        let before = symbols.fork_body_scope();
        symbols
            .interfaces
            .get_mut(&TypeId::local("Reading"))
            .unwrap()
            .public = true;
        symbols
            .classes
            .get_mut(&TypeId::local("Base"))
            .unwrap()
            .public = true;
        symbols
            .classes
            .get_mut(&TypeId::local("Closed"))
            .unwrap()
            .public = true;
        for name in ["Reading", "Base"] {
            let read = SymbolRead::TaskDispatch(name.into(), "read".into());
            assert!(!before.dispatch_may_start_task(name, "read"));
            assert!(symbols.dispatch_may_start_task(name, "read"));
            assert_ne!(before.symbol_value(&read), symbols.symbol_value(&read));
        }
        // Open-world queries have no graph walk or whole-snapshot index cost.
        assert!(symbols.task_dispatch.borrow().children.is_none());
        assert_eq!(symbols.task_dispatch.borrow().visits, 0);
        assert!(!symbols.dispatch_may_start_task("Closed", "read"));
        let alias = symbols.lookup_interface("Reading").unwrap().clone();
        symbols.define_interface("Alias".into(), alias);
        assert!(symbols.dispatch_may_start_task("Alias", "read"));
    }

    #[test]
    fn task_dispatch_cache_scaling_and_snapshot_invalidation() {
        for n in [1, 8, 64, 128] {
            for fanout in [false, true] {
                let mut source = "open class C0 { pub open fn read(self) -> Task<String> { return value(); } } async fn value() -> String { return \"v\"; }".to_string();
                for i in 1..n {
                    source.push_str(&format!(
                        "open class C{i} extends C{} {{}}",
                        if fanout { 0 } else { i - 1 }
                    ));
                }
                let mut symbols = symbols(&source);
                assert!(!symbols.dispatch_may_start_task("C0", "read"));
                assert_eq!(symbols.task_dispatch.borrow().visits, n);
                let old = symbols.fork_body_scope();
                for _ in 0..8 {
                    for i in 0..n {
                        let fork = symbols.fork_body_scope();
                        assert!(!fork.dispatch_may_start_task(&format!("C{i}"), "read"));
                    }
                }
                assert_eq!(symbols.task_dispatch.borrow().visits, n);
                let mut method = symbols.classes[&TypeId::local("C0")].methods["read"].clone();
                method.is_async = true;
                symbols
                    .classes
                    .get_mut(&TypeId::local(format!("C{}", n - 1)))
                    .unwrap()
                    .methods
                    .insert("read".into(), method);
                assert!(symbols.dispatch_may_start_task("C0", "read"));
                assert!(!old.dispatch_may_start_task("C0", "read"));
                assert_eq!(symbols.task_dispatch.borrow().visits, n);
                let capture = SymbolReadCapture::begin();
                assert!(symbols.dispatch_may_start_task("C0", "read"));
                let read = SymbolRead::TaskDispatch("C0".into(), "read".into());
                assert_eq!(capture.finish(), vec![read.clone()]);
                assert_ne!(symbols.symbol_value(&read), old.symbol_value(&read));
                println!(
                    "task dispatch N={n} fanout={fanout}: cold visits={n}, 8N forked warm queries add 0, changed snapshot visits={n}"
                );
            }
        }
    }

    #[test]
    fn task_dispatch_interfaces_aliases_and_inherited_bodies() {
        let mut symbols = symbols(
            "interface Reading { fn read(self) -> Task<String>; } interface Derived extends Reading {} open class Parent { pub async fn read(self) -> String { return \"v\"; } } class Child extends Parent implements Derived {} ",
        );
        let alias = symbols.lookup_interface("Reading").unwrap().clone();
        symbols.define_interface("Alias".into(), alias);
        for name in ["Reading", "Derived", "Alias", "Parent", "Child"] {
            assert!(symbols.dispatch_may_start_task(name, "read"), "{name}");
            assert!(!symbols.dispatch_may_start_task(name, "absent"), "{name}");
        }
        let encoded = serde_json::to_string(&symbols).unwrap();
        let decoded: SymbolTable = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.dispatch_may_start_task("Alias", "read"));
    }
}
