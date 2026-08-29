//! Interface vtable LAYOUT: which method occupies which slot, and where a
//! super-interface's table sits inside a sub-interface's.
//!
//! A Willow interface value is a two-word box, `[object | vtable]`, and a call
//! through it indexes the vtable by a slot number computed from the receiver's
//! STATIC interface. Widening such a value to a super-interface therefore has
//! to produce a box whose vtable answers the SUPER's slot numbering — and the
//! concrete class is not known at the widening site, so the target table can
//! only come from the source table.
//!
//! The layout below makes that possible. An interface's slots are the slots of
//! each direct super, **verbatim and in order**, followed by the methods the
//! interface itself adds:
//!
//! ```text
//! interface A { fn a(); }          A: [a]
//! interface B { fn b(); }          B: [b]
//! interface C extends A, B { fn c(); }
//!                                  C: [a | b | c]
//!                                      ^   ^
//!                                      |   B's table, embedded at slot 1
//!                                      A's table, embedded at slot 0
//! ```
//!
//! So every super-interface's table is a contiguous run inside the sub's, and
//! widening is `vtable + offset * 8` — the C++ multiple-inheritance trick.
//! [`super_offset`] computes that offset; it is `0` for the single-`extends`
//! chain, where the super's table is a plain prefix and the box can be reused
//! unchanged.
//!
//! Two properties make the "verbatim copy" honest:
//!
//! * A slot is filled by NAME (`resolve_class_method_func_id`), so a method
//!   that appears in two regions — a diamond's shared grandparent, or an own
//!   declaration that re-states an inherited one — gets the same address in
//!   every slot that names it. Duplicate slots can never disagree.
//! * [`slot_of`] resolves a method to its FIRST slot, and a super's region is
//!   built by this same function, so the index a call site computes from the
//!   super equals the index inside the embedded region.
//!
//! This is deliberately not the interface's `method_order`, which desugaring
//! composes with cross-super deduplication: that list is the SEMANTIC view (what
//! a class must implement), and deduplicating across supers is exactly what
//! destroys the embedded-region property (willow-1fc6).
//!
//! # Cost
//!
//! Repeating a shared super is the point, so a table grows with the number of
//! PATHS to each ancestor, not the number of ancestors: nesting diamonds
//! doubles the slot count per level. Neither [`slots`] nor [`super_offset`]
//! memoises either, so a query re-walks the whole super graph. Both are
//! compile-time and static-data costs only — dispatch stays one indexed load —
//! and inheritance graphs deep enough to notice do not occur in practice. If
//! one ever does, cache `slots` per interface; the results are pure functions
//! of the interface table.

use std::collections::HashSet;

/// The interface table the layout rules read: direct supers and composed method
/// names, by interface name.
///
/// A trait rather than the backend's `interface_infos` map directly, so the LIR
/// walker's eligibility tables answer from the same rules the emitter lays out
/// vtables with.
pub(super) trait IfaceShapes {
    /// The name an interface is registered under, so an import alias and its
    /// target are recognised as one interface.
    fn canonical(&self, iface: &str) -> String;
    /// Direct super-interfaces, in declaration order.
    fn supers(&self, iface: &str) -> Vec<String>;
    /// The interface's composed method names (desugaring's list: supers first,
    /// deduplicated, own methods last).
    fn methods(&self, iface: &str) -> Vec<String>;
}

/// The vtable slots of `iface`, in emission order.
pub(super) fn slots<S: IfaceShapes + ?Sized>(shapes: &S, iface: &str) -> Vec<String> {
    slots_guarded(shapes, iface, &mut HashSet::new())
}

/// The slot a call to `iface::method` indexes, or `None` when the interface
/// does not have that method at all.
pub(super) fn slot_of<S: IfaceShapes + ?Sized>(
    shapes: &S,
    iface: &str,
    method: &str,
) -> Option<usize> {
    slots(shapes, iface).iter().position(|n| n == method)
}

/// How many slots into `source`'s vtable the embedded `target` table starts, or
/// `None` when `target` is not a super-interface of `source`.
///
/// `Some(0)` — an interface widened to itself, or to the first link of its
/// `extends` chain — means the source box already IS a target box.
pub(super) fn super_offset<S: IfaceShapes + ?Sized>(
    shapes: &S,
    source: &str,
    target: &str,
) -> Option<usize> {
    let target = shapes.canonical(target);
    super_offset_guarded(shapes, source, &target, &mut HashSet::new())
}

fn slots_guarded<S: IfaceShapes + ?Sized>(
    shapes: &S,
    iface: &str,
    visiting: &mut HashSet<String>,
) -> Vec<String> {
    let canonical = shapes.canonical(iface);
    if !visiting.insert(canonical.clone()) {
        return Vec::new(); // `extends` cycle: already diagnosed, stop recursing
    }
    let mut out: Vec<String> = Vec::new();
    for sup in shapes.supers(iface) {
        out.extend(slots_guarded(shapes, &sup, visiting));
    }
    for method in shapes.methods(iface) {
        if !out.contains(&method) {
            out.push(method);
        }
    }
    visiting.remove(&canonical);
    out
}

fn super_offset_guarded<S: IfaceShapes + ?Sized>(
    shapes: &S,
    source: &str,
    target: &str,
    visiting: &mut HashSet<String>,
) -> Option<usize> {
    let canonical = shapes.canonical(source);
    if canonical == target {
        return Some(0);
    }
    if !visiting.insert(canonical.clone()) {
        return None; // `extends` cycle: already diagnosed, stop recursing
    }
    let mut offset = 0;
    let mut found = None;
    for sup in shapes.supers(source) {
        if let Some(inner) = super_offset_guarded(shapes, &sup, target, visiting) {
            found = Some(offset + inner);
            break;
        }
        offset += slots(shapes, &sup).len();
    }
    visiting.remove(&canonical);
    found
}

impl IfaceShapes for std::collections::HashMap<String, crate::semantic::symbols::InterfaceInfo> {
    fn canonical(&self, iface: &str) -> String {
        self.get(iface)
            .map(|info| info.name.clone())
            .unwrap_or_else(|| iface.to_string())
    }

    fn supers(&self, iface: &str) -> Vec<String> {
        self.get(iface)
            .map(|info| info.extends.clone())
            .unwrap_or_default()
    }

    fn methods(&self, iface: &str) -> Vec<String> {
        self.get(iface)
            .map(|info| info.method_order.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// `name -> (supers, composed methods)`, i.e. what desugaring leaves behind.
    struct Table(HashMap<&'static str, (Vec<&'static str>, Vec<&'static str>)>);

    impl Table {
        fn new(rows: &[(&'static str, &[&'static str], &[&'static str])]) -> Self {
            Table(
                rows.iter()
                    .map(|(n, s, m)| (*n, (s.to_vec(), m.to_vec())))
                    .collect(),
            )
        }
    }

    impl IfaceShapes for Table {
        fn canonical(&self, iface: &str) -> String {
            iface.to_string()
        }
        fn supers(&self, iface: &str) -> Vec<String> {
            self.0
                .get(iface)
                .map(|(s, _)| s.iter().map(|n| n.to_string()).collect())
                .unwrap_or_default()
        }
        fn methods(&self, iface: &str) -> Vec<String> {
            self.0
                .get(iface)
                .map(|(_, m)| m.iter().map(|n| n.to_string()).collect())
                .unwrap_or_default()
        }
    }

    // A plain interface lays its own methods out in declaration order.
    #[test]
    fn unit_vtable_01_own_methods_keep_declaration_order() {
        let t = Table::new(&[("A", &[], &["a", "z"])]);
        assert_eq!(slots(&t, "A"), vec!["a".to_string(), "z".to_string()]);
        assert_eq!(slot_of(&t, "A", "z"), Some(1));
        assert_eq!(slot_of(&t, "A", "nope"), None);
    }

    // A single-`extends` chain leaves the super's table as a prefix: the
    // widened box needs no adjustment at all.
    #[test]
    fn unit_vtable_02_single_chain_is_a_prefix() {
        let t = Table::new(&[("A", &[], &["a"]), ("B", &["A"], &["a", "b"])]);
        assert_eq!(slots(&t, "B"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(super_offset(&t, "B", "A"), Some(0));
    }

    // THE bug: a SECOND super's table is embedded after the first's, so the
    // widened box must advance the vtable pointer past it (willow-1fc6).
    #[test]
    fn unit_vtable_03_second_super_is_embedded_after_the_first() {
        let t = Table::new(&[
            ("A", &[], &["a"]),
            ("B", &[], &["b"]),
            ("C", &["A", "B"], &["a", "b", "c"]),
        ]);
        assert_eq!(
            slots(&t, "C"),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(super_offset(&t, "C", "A"), Some(0));
        assert_eq!(super_offset(&t, "C", "B"), Some(1));
        // The embedded region really is `B`'s own table.
        assert_eq!(slots(&t, "C")[1..2], slots(&t, "B")[..]);
    }

    // A diamond repeats the shared grandparent's slots in BOTH regions rather
    // than deduplicating them, which is what keeps each region contiguous.
    #[test]
    fn unit_vtable_04_diamond_repeats_the_shared_super() {
        let t = Table::new(&[
            ("X", &[], &["x"]),
            ("A", &["X"], &["x", "a"]),
            ("B", &["X"], &["x", "b"]),
            ("C", &["A", "B"], &["x", "a", "b", "c"]),
        ]);
        assert_eq!(slots(&t, "C"), ["x", "a", "x", "b", "c"]);
        assert_eq!(super_offset(&t, "C", "A"), Some(0));
        assert_eq!(super_offset(&t, "C", "B"), Some(2));
        assert_eq!(super_offset(&t, "C", "X"), Some(0));
        assert_eq!(slots(&t, "C")[2..4], slots(&t, "B")[..]);
        // A method in two regions resolves to the first, which is the slot the
        // un-widened receiver indexes.
        assert_eq!(slot_of(&t, "C", "x"), Some(0));
    }

    // Widening runs one way: a super has no slot for the sub's own methods.
    #[test]
    fn unit_vtable_05_narrowing_has_no_offset() {
        let t = Table::new(&[("A", &[], &["a"]), ("B", &["A"], &["a", "b"])]);
        assert_eq!(super_offset(&t, "A", "B"), None);
        assert_eq!(super_offset(&t, "A", "A"), Some(0));
    }

    // An `extends` cycle is a diagnosed program, but layout still has to
    // terminate: codegen runs on whatever the checker handed it.
    #[test]
    fn unit_vtable_06_extends_cycle_terminates() {
        let t = Table::new(&[("A", &["B"], &["a"]), ("B", &["A"], &["b"])]);
        assert_eq!(slots(&t, "A"), ["b", "a"]);
        assert_eq!(super_offset(&t, "A", "B"), Some(0));
    }

    // An unknown super (an interface from a module the backend never saw)
    // contributes no slots, so the sub's own methods still get stable indices.
    #[test]
    fn unit_vtable_07_unknown_super_contributes_nothing() {
        let t = Table::new(&[("C", &["Gone"], &["c"])]);
        assert_eq!(slots(&t, "C"), ["c"]);
        assert_eq!(super_offset(&t, "C", "Gone"), Some(0));
        assert_eq!(super_offset(&t, "C", "Other"), None);
    }

    // An own method that re-states an inherited one does not get a second
    // slot: it is the same name, and slots are filled by name.
    #[test]
    fn unit_vtable_08_own_redeclaration_reuses_the_inherited_slot() {
        let t = Table::new(&[("A", &[], &["m"]), ("B", &["A"], &["m", "b"])]);
        assert_eq!(slots(&t, "B"), ["m", "b"]);
        assert_eq!(slot_of(&t, "B", "m"), Some(0));
        assert_eq!(super_offset(&t, "B", "A"), Some(0));
    }

    // Three supers: each region starts where the previous one ended.
    #[test]
    fn unit_vtable_09_three_supers_are_laid_out_end_to_end() {
        let t = Table::new(&[
            ("A", &[], &["a1", "a2"]),
            ("B", &[], &["b"]),
            ("C", &[], &["c1", "c2", "c3"]),
            (
                "D",
                &["A", "B", "C"],
                &["a1", "a2", "b", "c1", "c2", "c3", "d"],
            ),
        ]);
        assert_eq!(slots(&t, "D").len(), 7);
        assert_eq!(super_offset(&t, "D", "A"), Some(0));
        assert_eq!(super_offset(&t, "D", "B"), Some(2));
        assert_eq!(super_offset(&t, "D", "C"), Some(3));
        assert_eq!(slot_of(&t, "D", "d"), Some(6));
    }

    // A transitive super is found through the sub-super's own offset, so the
    // two hops add up.
    #[test]
    fn unit_vtable_10_transitive_super_offsets_compose() {
        let t = Table::new(&[
            ("P", &[], &["p"]),
            ("Q", &[], &["q"]),
            ("R", &["P", "Q"], &["p", "q", "r"]),
            ("S", &[], &["s"]),
            ("T", &["S", "R"], &["s", "p", "q", "r", "t"]),
        ]);
        assert_eq!(slots(&t, "T"), ["s", "p", "q", "r", "t"]);
        assert_eq!(super_offset(&t, "T", "R"), Some(1));
        assert_eq!(super_offset(&t, "T", "Q"), Some(2));
        assert_eq!(slots(&t, "T")[2..3], slots(&t, "Q")[..]);
    }
}
