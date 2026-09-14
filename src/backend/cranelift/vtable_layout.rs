//! Interface boxes remain `[object | vtable]`. Each vtable stores composed
//! method pointers once, followed by pointers to its direct-super tables.
//! Widening follows a statically resolved sequence of supertable pointer loads.
//! Shared ancestors are shared data symbols, so diamonds use O(methods + edges)
//! table words per class rather than one copy per inheritance path. This trades
//! O(path length) loads at widening sites for bounded static data; method
//! dispatch remains one indexed function-pointer load.

use std::collections::HashSet;

/// The interface table the layout rules read: direct supers and composed method
/// names, by interface name.
///
/// A trait rather than the backend's `interface_infos` map directly, so the LIR
/// walker's eligibility tables answer from the same rules the emitter lays out
/// vtables with.
pub(super) trait IfaceShapes {
    fn canonical(&self, iface: &super::TypeId) -> super::TypeId;
    fn supers(&self, iface: &super::TypeId) -> Vec<super::TypeId>;
    fn methods(&self, iface: &super::TypeId) -> Vec<String>;
}

pub(super) fn slots<S: IfaceShapes + ?Sized, Q: super::type_index::TypeLookup + ?Sized>(
    shapes: &S,
    iface: &Q,
) -> Vec<String> {
    shapes.methods(&iface.type_id())
}

pub(super) fn slot_of<S: IfaceShapes + ?Sized, Q: super::type_index::TypeLookup + ?Sized>(
    shapes: &S,
    iface: &Q,
    method: &str,
) -> Option<usize> {
    slots(shapes, iface).iter().position(|name| name == method)
}

/// Slot indices of direct-super pointers to load. Empty means identity.
pub(super) fn super_path<
    S: IfaceShapes + ?Sized,
    Q: super::type_index::TypeLookup + ?Sized,
    R: super::type_index::TypeLookup + ?Sized,
>(
    shapes: &S,
    source: &Q,
    target: &R,
) -> Option<Vec<usize>> {
    let target = shapes.canonical(&target.type_id());
    let source = shapes.canonical(&source.type_id());
    if source == target {
        return Some(Vec::new());
    }
    // Parent links avoid cloning an ever-growing path at each graph edge.
    let mut parents = std::collections::HashMap::new();
    let mut seen = HashSet::from([source]);
    let mut work = vec![source];
    while let Some(current) = work.pop() {
        let base = shapes.methods(&current).len().max(1);
        for (index, sup) in shapes.supers(&current).into_iter().enumerate() {
            let sup = shapes.canonical(&sup);
            if !seen.insert(sup) {
                continue;
            }
            parents.insert(sup, (current, base + index));
            if sup == target {
                let mut path = Vec::new();
                let mut node = target;
                while node != source {
                    let (parent, slot) = parents[&node];
                    path.push(slot);
                    node = parent;
                }
                path.reverse();
                return Some(path);
            }
            work.push(sup);
        }
    }
    None
}

impl IfaceShapes for super::type_index::TypeMap<super::InterfaceInfo> {
    fn canonical(&self, iface: &super::TypeId) -> super::TypeId {
        self.get_id(iface)
            .map(|info| info.name)
            .unwrap_or_else(|| *iface)
    }
    fn supers(&self, iface: &super::TypeId) -> Vec<super::TypeId> {
        self.get_id(iface)
            .map(|info| info.extends.clone())
            .unwrap_or_default()
    }
    fn methods(&self, iface: &super::TypeId) -> Vec<String> {
        self.get_id(iface)
            .map(|info| info.method_order.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Layout perspectives (willow-ssl7.5). 1 leaf interface exposes only its
    // own method slots, 2 `slot_of` finds a declared method and rejects an
    // undeclared one, 3 an unknown interface has no slots and no supers,
    // 4 a chain hop lands at `methods.max(1) + super index`, 5 a widening path
    // is the sequence of those hops, 6 two direct supers get distinct adjacent
    // pointer slots, 7 three supers extend that run, 8 a diamond reaches its
    // shared ancestor through one recorded path, 9 that path is bounded by the
    // graph depth rather than the number of paths, 10 narrowing is not a
    // widening, 11 identity widening is the empty path, 12 a methodless
    // interface still reserves one word before its super pointers, 13 an
    // `extends` cycle terminates, 14 a self-extending interface terminates,
    // 15 an unknown super name is skipped rather than fatal, 16 a disconnected
    // interface is unreachable, 17 aliases are canonicalized on the source,
    // 18 on the target and 19 on each super edge, 20 a ten-deep chain widens
    // with one slot per hop, 21 a deep diamond keeps total table words linear
    // in interfaces and edges, 22 a method redeclared by a super keeps one
    // slot, and 23 declaration order of methods is the slot order.

    /// `name -> (supers, composed methods)`, i.e. what desugaring leaves behind.
    struct Table {
        rows: HashMap<&'static str, (Vec<&'static str>, Vec<&'static str>)>,
        aliases: HashMap<&'static str, &'static str>,
    }

    impl Table {
        fn new(rows: &[(&'static str, &[&'static str], &[&'static str])]) -> Self {
            Table {
                rows: rows
                    .iter()
                    .map(|(n, s, m)| (*n, (s.to_vec(), m.to_vec())))
                    .collect(),
                aliases: HashMap::new(),
            }
        }

        fn alias(mut self, alias: &'static str, target: &'static str) -> Self {
            self.aliases.insert(alias, target);
            self
        }

        /// Table words one vtable occupies: composed methods (at least one
        /// reserved word) plus one pointer per direct super.
        fn words(&self, iface: &'static str) -> usize {
            let (supers, methods) = &self.rows[iface];
            methods.len().max(1) + supers.len()
        }
    }

    impl IfaceShapes for Table {
        fn canonical(&self, iface: &super::super::TypeId) -> super::super::TypeId {
            self.aliases
                .get(iface.name())
                .map(super::super::TypeId::local)
                .unwrap_or(*iface)
        }
        fn supers(&self, iface: &super::super::TypeId) -> Vec<super::super::TypeId> {
            self.rows
                .get(self.canonical(iface).name())
                .map(|(s, _)| s.iter().map(super::super::TypeId::local).collect())
                .unwrap_or_default()
        }
        fn methods(&self, iface: &super::super::TypeId) -> Vec<String> {
            self.rows
                .get(self.canonical(iface).name())
                .map(|(_, m)| m.iter().map(|n| n.to_string()).collect())
                .unwrap_or_default()
        }
    }

    fn diamond() -> Table {
        Table::new(&[
            ("X", &[], &["x"]),
            ("A", &["X"], &["x", "a"]),
            ("B", &["X"], &["x", "b"]),
            ("C", &["A", "B"], &["x", "a", "b", "c"]),
        ])
    }

    #[test]
    fn methods_and_shared_super_paths_have_one_layout() {
        let t = diamond();
        assert_eq!(slots(&t, "C"), ["x", "a", "b", "c"]);
        assert_eq!(slot_of(&t, "C", "b"), Some(2));
        assert_eq!(slot_of(&t, "C", "missing"), None);
        assert_eq!(super_path(&t, "C", "A"), Some(vec![4]));
        assert_eq!(super_path(&t, "C", "B"), Some(vec![5]));
        assert_eq!(super_path(&t, "C", "X"), Some(vec![5, 2]));
        assert_eq!(super_path(&t, "A", "X"), Some(vec![2]));
        assert_eq!(super_path(&t, "C", "C"), Some(vec![]));
        assert_eq!(super_path(&t, "A", "C"), None);
    }

    #[test]
    fn leaf_and_unknown_interfaces_expose_their_own_slots_only() {
        let t = Table::new(&[("L", &[], &["one", "two"])]);
        assert_eq!(slots(&t, "L"), ["one", "two"]);
        assert_eq!(slot_of(&t, "L", "one"), Some(0));
        assert_eq!(slot_of(&t, "L", "two"), Some(1));
        assert_eq!(slot_of(&t, "L", "three"), None);
        assert!(slots(&t, "Absent").is_empty());
        assert_eq!(super_path(&t, "Absent", "L"), None);
        assert_eq!(super_path(&t, "L", "Absent"), None);
    }

    #[test]
    fn chain_hops_follow_the_method_count_of_each_interface() {
        let t = Table::new(&[
            ("Base", &[], &["b"]),
            ("Mid", &["Base"], &["b", "m1", "m2"]),
            ("Top", &["Mid"], &["b", "m1", "m2", "t"]),
        ]);
        // Base pointer slot of `Top` is 4 (four methods), of `Mid` is 3.
        assert_eq!(super_path(&t, "Top", "Mid"), Some(vec![4]));
        assert_eq!(super_path(&t, "Mid", "Base"), Some(vec![3]));
        assert_eq!(super_path(&t, "Top", "Base"), Some(vec![4, 3]));
        // Widening is directional.
        assert_eq!(super_path(&t, "Base", "Top"), None);
        assert_eq!(super_path(&t, "Base", "Mid"), None);
    }

    #[test]
    fn multiple_direct_supers_get_adjacent_pointer_slots() {
        let t = Table::new(&[
            ("P", &[], &["p"]),
            ("Q", &[], &["q"]),
            ("R", &[], &["r"]),
            ("S", &["P", "Q", "R"], &["p", "q", "r"]),
        ]);
        assert_eq!(super_path(&t, "S", "P"), Some(vec![3]));
        assert_eq!(super_path(&t, "S", "Q"), Some(vec![4]));
        assert_eq!(super_path(&t, "S", "R"), Some(vec![5]));
        assert_eq!(t.words("S"), 6);
    }

    #[test]
    fn methodless_interfaces_reserve_one_word_before_super_pointers() {
        let t = Table::new(&[("E", &[], &[]), ("F", &["E"], &[])]);
        assert!(slots(&t, "F").is_empty());
        assert_eq!(super_path(&t, "F", "E"), Some(vec![1]));
        assert_eq!(t.words("E"), 1);
        assert_eq!(t.words("F"), 2);
    }

    #[test]
    fn cyclic_and_empty_layouts_are_bounded() {
        let t = Table::new(&[("A", &["B"], &[]), ("B", &["A"], &["b"])]);
        assert!(slots(&t, "A").is_empty());
        assert_eq!(super_path(&t, "A", "B"), Some(vec![1]));
        assert_eq!(super_path(&t, "A", "Missing"), None);
    }

    #[test]
    fn self_extending_and_unknown_supers_terminate() {
        let t = Table::new(&[
            ("Loop", &["Loop"], &["l"]),
            ("Ghosted", &["NotDeclared"], &["g"]),
        ]);
        assert_eq!(super_path(&t, "Loop", "Loop"), Some(vec![]));
        assert_eq!(super_path(&t, "Ghosted", "Loop"), None);
        // The unknown super is still a slot, so a later declaration of it
        // cannot move the ones already laid out.
        assert_eq!(super_path(&t, "Ghosted", "NotDeclared"), Some(vec![1]));
    }

    #[test]
    fn disconnected_interfaces_are_not_reachable() {
        let t = Table::new(&[
            ("Left", &[], &["l"]),
            ("Right", &[], &["r"]),
            ("LeftChild", &["Left"], &["l", "c"]),
        ]);
        assert_eq!(super_path(&t, "LeftChild", "Right"), None);
        assert_eq!(super_path(&t, "Left", "Right"), None);
        assert_eq!(super_path(&t, "LeftChild", "Left"), Some(vec![2]));
    }

    #[test]
    fn aliases_are_canonicalized_on_source_target_and_edges() {
        let t = diamond().alias("CAlias", "C").alias("XAlias", "X");
        assert_eq!(super_path(&t, "CAlias", "X"), super_path(&t, "C", "X"));
        assert_eq!(super_path(&t, "C", "XAlias"), super_path(&t, "C", "X"));
        assert_eq!(super_path(&t, "CAlias", "XAlias"), Some(vec![5, 2]));
        assert_eq!(super_path(&t, "CAlias", "CAlias"), Some(vec![]));
        assert_eq!(slots(&t, "CAlias"), ["x", "a", "b", "c"]);
        let aliased_edge = Table::new(&[("Sub", &["SuperAlias"], &["s"]), ("Super", &[], &["s"])])
            .alias("SuperAlias", "Super");
        assert_eq!(super_path(&aliased_edge, "Sub", "Super"), Some(vec![1]));
    }

    #[test]
    fn deep_chain_widens_with_one_slot_per_hop() {
        let names: Vec<String> = (0..10).map(|i| format!("I{i}")).collect();
        let leaked: Vec<&'static str> = names
            .iter()
            .map(|n| &*Box::leak(n.clone().into_boxed_str()))
            .collect();
        let mut rows: Vec<(&'static str, Vec<&'static str>, Vec<&'static str>)> = Vec::new();
        for (i, name) in leaked.iter().enumerate() {
            let supers = if i == 0 {
                Vec::new()
            } else {
                vec![leaked[i - 1]]
            };
            rows.push((*name, supers, vec!["m"]));
        }
        let table = Table::new(
            &rows
                .iter()
                .map(|(n, s, m)| (*n, s.as_slice(), m.as_slice()))
                .collect::<Vec<_>>(),
        );
        let path = super_path(&table, leaked[9], leaked[0]).expect("chain widening");
        assert_eq!(path, vec![1; 9]);
        assert_eq!(
            super_path(&table, leaked[9], leaked[5]),
            Some(vec![1, 1, 1, 1])
        );
        // Each vtable stores its method plus one super pointer regardless of
        // how deep the chain is.
        assert!(leaked.iter().skip(1).all(|name| table.words(name) == 2));
    }

    #[test]
    fn deep_diamonds_keep_table_words_linear() {
        // Level i has `Ai`/`Bi`, both extending both of level i-1: the number
        // of distinct inheritance PATHS to level 0 doubles per level, so a
        // verbatim-embedding layout would too.
        const LEVELS: usize = 8;
        let mut names: Vec<&'static str> = Vec::new();
        let mut rows: Vec<(&'static str, Vec<&'static str>, Vec<&'static str>)> = Vec::new();
        for level in 0..LEVELS {
            let a: &'static str = Box::leak(format!("A{level}").into_boxed_str());
            let b: &'static str = Box::leak(format!("B{level}").into_boxed_str());
            let supers = if level == 0 {
                Vec::new()
            } else {
                vec![names[names.len() - 2], names[names.len() - 1]]
            };
            rows.push((a, supers.clone(), vec!["m"]));
            rows.push((b, supers, vec!["m"]));
            names.push(a);
            names.push(b);
        }
        let table = Table::new(
            &rows
                .iter()
                .map(|(n, s, m)| (*n, s.as_slice(), m.as_slice()))
                .collect::<Vec<_>>(),
        );
        // Two root tables of one method word each, then two tables per level
        // holding that method word plus one pointer per direct super.
        let total: usize = names.iter().map(|name| table.words(name)).sum();
        assert_eq!(total, 2 + (LEVELS - 1) * 2 * (1 + 2));
        assert!(total <= 4 * names.len());
        // Widening from the deepest table to either root is one pointer load
        // per level, not one per inheritance path.
        let deepest = names[names.len() - 1];
        let path = super_path(&table, deepest, "A0").expect("diamond widening");
        assert_eq!(path.len(), LEVELS - 1, "{path:?}");
        assert_eq!(
            super_path(&table, deepest, "B0").map(|p| p.len()),
            Some(LEVELS - 1)
        );
    }

    #[test]
    fn composed_method_order_is_the_slot_order() {
        // Composition already de-duplicated the redeclared `x`; the layout
        // keeps one slot for it and dispatch resolves to the first position.
        let t = Table::new(&[
            ("Sup", &[], &["x", "y"]),
            ("Sub", &["Sup"], &["x", "y", "z"]),
        ]);
        assert_eq!(slots(&t, "Sub"), ["x", "y", "z"]);
        assert_eq!(slot_of(&t, "Sub", "x"), Some(0));
        assert_eq!(slot_of(&t, "Sup", "x"), Some(0));
        assert_eq!(slot_of(&t, "Sub", "z"), Some(2));
        assert_eq!(slot_of(&t, "Sup", "z"), None);
        assert_eq!(super_path(&t, "Sub", "Sup"), Some(vec![3]));
    }
}
