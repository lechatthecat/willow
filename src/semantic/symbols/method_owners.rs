//! Declaration-snapshot-local, path-compressed defining-class queries.
use super::{SymbolRead, SymbolTable, record_read};
use crate::semantic::ids::TypeId;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub(super) struct MethodOwners {
    // Nested maps permit borrowed lookups without allocating a pair of Strings.
    by_method: HashMap<String, HashMap<String, Option<String>>>,
    #[cfg(test)]
    steps: usize,
}

impl SymbolTable {
    /// Nearest declaring class, preserving the written alias on a direct hit.
    /// Cache every traversed (class spelling, method) pair, including misses.
    /// Body forks share this cache; declaration mutation detaches it in DerefMut.
    pub(crate) fn resolved_method_class(&self, class: &str, method: &str) -> Option<String> {
        // Record the query even on a cache hit. Replaying all ancestor Class
        // reads would reintroduce depth-proportional work in every body.
        record_read(|| SymbolRead::MethodOwner(class.to_owned(), method.to_owned()));
        let mut cache = self.method_owners.borrow_mut();
        let MethodOwners {
            by_method,
            #[cfg(test)]
            steps,
        } = &mut *cache;
        if let Some(owner) = by_method.get(method).and_then(|entries| entries.get(class)) {
            return owner.clone();
        }
        let entries = by_method.entry(method.to_owned()).or_default();
        let mut path = HashSet::new();
        let mut current = class;
        let owner = loop {
            if let Some(owner) = entries.get(current) {
                break owner.clone();
            }
            if !path.insert(current) {
                // No declaration was encountered anywhere on this cycle.
                break None;
            }
            #[cfg(test)]
            {
                *steps += 1;
            }
            // The MethodOwner read above captures the entire resolution result;
            // do not accumulate redundant depth-sized per-body Class reads.
            let Some(info) = self.classes.get(&TypeId::from_source_name(current)) else {
                break None;
            };
            if info.methods.contains_key(method) {
                break Some(current.to_owned());
            }
            let Some(base) = info.base_class.as_deref() else {
                break None;
            };
            current = base;
        };
        for name in path {
            entries.insert(name.to_owned(), owner.clone());
        }
        owner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Span;
    use crate::parser::ast::Type;
    use crate::semantic::symbols::{ClassInfo, MethodInfo, SymbolReadCapture};

    fn class(name: &str, base: Option<&str>, methods: &[&str]) -> ClassInfo {
        ClassInfo {
            name: name.into(),
            public: true,
            is_open: true,
            base_class: base.map(str::to_owned),
            implements: vec![],
            declaration_span: Span::dummy(),
            fields: HashMap::new(),
            methods: methods
                .iter()
                .map(|name| {
                    (
                        name.to_string(),
                        MethodInfo {
                            params: vec![],
                            param_infos: vec![],
                            is_static: false,
                            is_async: false,
                            return_type: Type::I64,
                            public: true,
                            protected: false,
                            is_open: true,
                            is_override: false,
                            declaration_span: Span::dummy(),
                        },
                    )
                })
                .collect(),
            static_props: HashMap::new(),
            instance_field_order: vec![],
            constructor: None,
        }
    }

    // Exact pre-cache algorithm, including written aliases and malformed chains.
    fn original(symbols: &SymbolTable, class: &str, method: &str) -> Option<String> {
        let mut current = Some(class.to_owned());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                break;
            }
            let info = symbols.lookup_class(&name)?;
            if info.methods.contains_key(method) {
                return Some(name);
            }
            current = info.base_class.clone();
        }
        None
    }

    #[test]
    fn method_owner_exhaustive_partial_hierarchies_match_original() {
        // 5^3 base assignments * 2^3 declaration placements * both query orders.
        // Includes roots, missing bases, self/multi-node cycles, reachers,
        // overrides, fan-out, positive/negative results and warmed lookups.
        let names = ["A", "B", "C"];
        let bases = [None, Some("A"), Some("B"), Some("C"), Some("Missing")];
        for graph in 0..125 {
            for mask in 0..8 {
                for reverse in [false, true] {
                    let mut symbols = SymbolTable::default();
                    for (i, name) in names.iter().enumerate() {
                        let methods = if mask & (1 << i) != 0 {
                            vec!["m"]
                        } else {
                            vec![]
                        };
                        symbols.define_class(
                            (*name).into(),
                            class(name, bases[graph / 5_usize.pow(i as u32) % 5], &methods),
                        );
                    }
                    let mut queries = vec!["A", "B", "C", "Missing"];
                    if reverse {
                        queries.reverse();
                    }
                    for _ in 0..2 {
                        for name in &queries {
                            for method in ["m", "absent"] {
                                assert_eq!(
                                    symbols.resolved_method_class(name, method),
                                    original(&symbols, name, method),
                                    "graph={graph} mask={mask} reverse={reverse} {name}.{method}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn method_owner_linear_chain_and_fanout_counts() {
        for n in [16, 64, 256] {
            for fanout in [false, true] {
                for reverse in [false, true] {
                    let mut symbols = SymbolTable::default();
                    symbols.define_class("C0".into(), class("C0", None, &["m"]));
                    for i in 1..n {
                        symbols.define_class(
                            format!("C{i}"),
                            class(
                                &format!("C{i}"),
                                Some(&format!("C{}", if fanout { 0 } else { i - 1 })),
                                &[],
                            ),
                        );
                    }
                    let mut indices: Vec<_> = (0..n).collect();
                    if reverse {
                        indices.reverse();
                    }
                    for i in &indices {
                        assert_eq!(
                            symbols.resolved_method_class(&format!("C{i}"), "m"),
                            Some("C0".into())
                        );
                        assert_eq!(
                            symbols.resolved_method_class(&format!("C{i}"), "absent"),
                            None
                        );
                    }
                    assert_eq!(symbols.method_owners.borrow().steps, 2 * n);
                    // Independent body forks must not repeat hierarchy walks.
                    for i in indices {
                        let body = symbols.fork_body_scope();
                        assert_eq!(
                            body.resolved_method_class(&format!("C{i}"), "m"),
                            Some("C0".into())
                        );
                        assert_eq!(body.resolved_method_class(&format!("C{i}"), "absent"), None);
                    }
                    assert_eq!(symbols.method_owners.borrow().steps, 2 * n);
                    println!(
                        "N={n} fanout={fanout} reverse={reverse}: positive+negative steps={}, warm additional=0",
                        2 * n
                    );
                }
            }
        }
    }

    #[test]
    fn method_owner_alias_mutation_fork_and_serialization() {
        let mut symbols = SymbolTable::default();
        symbols.define_class("pkg::Base".into(), class("pkg::Base", None, &["m"]));
        symbols.define_class("Alias".into(), class("pkg::Base", None, &["m"]));
        symbols.define_class("Child".into(), class("Child", Some("Alias"), &[]));
        assert_eq!(
            symbols.resolved_method_class("Alias", "m"),
            Some("Alias".into())
        );
        assert_eq!(
            symbols.resolved_method_class("Child", "m"),
            Some("Alias".into())
        );
        let old = symbols.fork_body_scope();
        // Public map mutation must invalidate as well as define_class.
        symbols
            .classes
            .get_mut(&TypeId::from_source_name("Child"))
            .unwrap()
            .methods = class("Child", None, &["m"]).methods;
        assert_eq!(
            symbols.resolved_method_class("Child", "m"),
            Some("Child".into())
        );
        assert_eq!(
            old.resolved_method_class("Child", "m"),
            Some("Alias".into())
        );
        assert_eq!(symbols.resolved_method_class("Unknown", "m"), None);
        symbols.define_class("Unknown".into(), class("Unknown", None, &["m"]));
        assert_eq!(
            symbols.resolved_method_class("Unknown", "m"),
            Some("Unknown".into())
        );
        let restored: SymbolTable =
            serde_json::from_value(serde_json::to_value(&symbols).unwrap()).unwrap();
        assert_eq!(restored.method_owners.borrow().steps, 0);
        assert_eq!(
            restored.resolved_method_class("Child", "m"),
            Some("Child".into())
        );
    }

    #[test]
    fn method_owner_dependency_is_recorded_on_cold_and_warm_queries() {
        let mut symbols = SymbolTable::default();
        symbols.define_class("Base".into(), class("Base", None, &["m"]));
        symbols.define_class("Child".into(), class("Child", Some("Base"), &[]));
        let read = SymbolRead::MethodOwner("Child".into(), "m".into());
        for _ in 0..2 {
            let capture = SymbolReadCapture::begin();
            assert_eq!(
                symbols.resolved_method_class("Child", "m"),
                Some("Base".into())
            );
            assert_eq!(capture.finish(), vec![read.clone()]);
        }
        let before = symbols.symbol_value(&read);
        symbols.define_class("Child".into(), class("Child", Some("Base"), &["m"]));
        assert_ne!(symbols.symbol_value(&read), before);
        let miss = SymbolRead::MethodOwner("Child".into(), "new".into());
        for _ in 0..2 {
            let capture = SymbolReadCapture::begin();
            assert_eq!(symbols.resolved_method_class("Child", "new"), None);
            assert_eq!(capture.finish(), vec![miss.clone()]);
        }
        let before = symbols.symbol_value(&miss);
        symbols.define_class("Base".into(), class("Base", None, &["m", "new"]));
        assert_ne!(symbols.symbol_value(&miss), before);
    }
}
