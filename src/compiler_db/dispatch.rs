//! Declaration-only dispatch inputs and their revision-tracked consumers.
use super::{
    incremental::SyntaxQueries,
    tracked::{
        InputNode, QueryNode, QueryProvider, QueryValue, ResultFingerprint, TrackedQueryTable,
    },
};
use crate::{
    module::UnitId,
    semantic::{call_graph::ClassHierarchy, ids::TypeId},
};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct DispatchDeclaration {
    pub(crate) exists: bool,
    pub(crate) base: Option<String>,
    pub(crate) methods: BTreeSet<String>,
}

pub(crate) fn capture(
    queries: &mut SyntaxQueries,
    unit: UnitId,
    hierarchy: &ClassHierarchy,
) -> Result<()> {
    let mut declarations = hierarchy.dispatch_declarations();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, declaration) in &declarations {
        if let Some(base) = &declaration.base {
            children.entry(base.clone()).or_default().push(name.clone());
        }
    }
    for base in children.keys() {
        declarations.entry(base.clone()).or_default();
    }
    let current: BTreeSet<_> = declarations.keys().cloned().collect();
    for removed in queries
        .dispatch_classes
        .insert(unit, current.clone())
        .unwrap_or_default()
        .difference(&current)
    {
        declarations.insert(removed.clone(), DispatchDeclaration::default());
    }
    for (name, declaration) in declarations {
        let id = TypeId::from_source_name(&name);
        queries.capture_input(
            InputNode::DispatchDeclaration(unit, id),
            QueryValue::new(
                declaration,
                ResultFingerprint::bytes(b"dispatch-declaration"),
            ),
        )?;
        queries.capture_input(
            InputNode::DispatchChildren(unit, id),
            QueryValue::new(
                children.remove(&name).unwrap_or_default(),
                ResultFingerprint::bytes(b"dispatch-children"),
            ),
        )?;
    }
    Ok(())
}

pub(crate) struct DispatchProvider;
impl QueryProvider for DispatchProvider {
    fn compute(&self, table: &TrackedQueryTable, node: &QueryNode) -> Result<QueryValue> {
        let QueryNode::DispatchTargets(unit, class, method) = node else {
            anyhow::bail!("unexpected dispatch query {node:?}");
        };
        let targets = crate::semantic::call_graph::resolve_dispatch_targets(
            &class.to_string(),
            method,
            |name| {
                let input = table.input(&InputNode::DispatchDeclaration(
                    *unit,
                    TypeId::from_source_name(name),
                ))?;
                let header = input.get::<DispatchDeclaration>();
                Ok((
                    header.exists,
                    header.base.clone(),
                    header.methods.contains(method),
                ))
            },
            |name| {
                Ok(table
                    .input(&InputNode::DispatchChildren(
                        *unit,
                        TypeId::from_source_name(name),
                    ))?
                    .get::<Vec<String>>()
                    .clone())
            },
        )?;
        let value = serde_json::to_value(targets)?;
        let fingerprint = ResultFingerprint::bytes(&serde_json::to_vec(&value)?);
        Ok(QueryValue::new(value, fingerprint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::ids::FunctionId;

    #[test]
    fn tracked_dispatch_reparent_delete_missing_base_and_cycles_match_cold() {
        let mut accepted = SyntaxQueries::default();
        for step in 0..6 {
            let mut hierarchy = ClassHierarchy::default();
            hierarchy.add_class("A", if step == 5 { Some("B") } else { None });
            hierarchy.add_method("A", "m");
            hierarchy.add_class("B", None);
            hierarchy.add_method("B", "m");
            if step != 3 {
                hierarchy.add_class(
                    "Child",
                    Some(if step == 0 || step == 4 { "A" } else { "B" }),
                );
                hierarchy.add_method("Child", "m");
            }
            hierarchy.add_class("Orphan", Some("Missing"));
            if step == 5 {
                hierarchy.add_class("B", Some("A"));
            }
            let mut candidate = accepted.candidate().unwrap();
            capture(&mut candidate, UnitId::ENTRY, &hierarchy).unwrap();
            for class in ["A", "B", "Child", "Orphan", "Missing"] {
                for method in ["m", "absent"] {
                    let expected = hierarchy.dispatch_targets(class, method);
                    let before = candidate.stats();
                    for _ in 0..8 {
                        assert_eq!(
                            candidate
                                .dispatch_targets(UnitId::ENTRY, TypeId::local(class), method)
                                .unwrap(),
                            expected,
                            "{step} {class}.{method}"
                        );
                    }
                    assert!(candidate.stats().recomputed - before.recomputed <= 1);
                }
            }
            // The same spellings in another unit must never share an inventory.
            let other = crate::module::ModuleId(91);
            let mut foreign = ClassHierarchy::default();
            foreign.add_class("A", None);
            foreign.add_method("A", "other");
            capture(&mut candidate, other, &foreign).unwrap();
            assert!(
                candidate
                    .dispatch_targets(other, TypeId::local("A"), "m")
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                candidate
                    .dispatch_targets(other, TypeId::local("A"), "other")
                    .unwrap(),
                vec![FunctionId::method(TypeId::local("A"), "other")]
            );
            accepted = candidate;
        }
    }

    #[test]
    fn tracked_dispatch_chain_and_fanout_reuse_is_bounded_by_reachable_inputs() {
        for size in [8, 32, 128] {
            for chain in [true, false] {
                let mut hierarchy = ClassHierarchy::default();
                hierarchy.add_class("Root", None);
                hierarchy.add_method("Root", "m");
                for i in 0..size {
                    let base = if chain && i > 0 {
                        format!("C{}", i - 1)
                    } else {
                        "Root".into()
                    };
                    hierarchy.add_class(&format!("C{i}"), Some(&base));
                    if i % 2 == 0 {
                        hierarchy.add_method(&format!("C{i}"), "m");
                    }
                    hierarchy.add_class(&format!("Unrelated{i}"), None);
                }
                let mut first = SyntaxQueries::default().candidate().unwrap();
                capture(&mut first, UnitId::ENTRY, &hierarchy).unwrap();
                let expected = hierarchy.dispatch_targets("Root", "m");
                assert_eq!(
                    first
                        .dispatch_targets(UnitId::ENTRY, TypeId::local("Root"), "m")
                        .unwrap(),
                    expected
                );
                let mut next = first.candidate().unwrap();
                // An unrelated header edit advances the revision without changing this union.
                hierarchy.add_method("Unrelated0", "other");
                capture(&mut next, UnitId::ENTRY, &hierarchy).unwrap();
                assert_eq!(
                    next.dispatch_targets(UnitId::ENTRY, TypeId::local("Root"), "m")
                        .unwrap(),
                    expected
                );
                assert_eq!(next.stats().recomputed, 0);
                assert_eq!(next.stats().dependency_edges_visited, 2 * (size + 1));
                let before = next.stats();
                for _ in 0..128 {
                    next.dispatch_targets(UnitId::ENTRY, TypeId::local("Root"), "m")
                        .unwrap();
                }
                assert_eq!(next.stats(), before);
                println!(
                    "dispatch shape={} size={size} validated_edges={} recomputed=0",
                    if chain { "chain" } else { "fanout" },
                    before.dependency_edges_visited
                );
            }
        }
    }
}
