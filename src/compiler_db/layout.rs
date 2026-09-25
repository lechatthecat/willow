//! Immutable canonical class layout queries with parent-first evaluation.
//! Composition is iterative; backend symbol allocation is deliberately separate.
use super::query::QueryTable;
use crate::semantic::{ids::TypeId, method_slots::MethodSlots};
use anyhow::Result;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    sync::Arc,
};

type Type = crate::parser::ast::Type<TypeId>;

struct ClassLayoutDeclaration {
    base: Option<TypeId>,
    fields: Vec<(String, Type)>,
    methods: Vec<String>,
}

/// One canonical class on one target: the key of every byte-level layout
/// derived from a semantic field list. Two targets with the same pointer width
/// and ABI revision place fields identically; anything else must not share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TargetLayoutKey {
    pub(crate) ty: TypeId,
    /// Pointer width in bits.
    pub(crate) pointer_width: u8,
    pub(crate) abi_revision: u32,
}

impl TargetLayoutKey {
    pub(crate) fn new(ty: TypeId, pointer_bytes: u32) -> Self {
        Self {
            ty,
            pointer_width: (pointer_bytes * 8) as u8,
            abi_revision: willow_abi::OBJECT_LAYOUT_REVISION,
        }
    }

    pub(crate) fn pointer_bytes(&self) -> u32 {
        u32::from(self.pointer_width) / 8
    }
}

/// The byte placement of one class's fields on one target: word 0 holds the
/// class descriptor, field `i` the storage word `i + 1`. Field lookup by name
/// is an index, not a scan, because emission asks once per access.
pub(crate) struct ObjectLayout {
    fields: Arc<Vec<(String, Type)>>,
    index: HashMap<String, usize>,
    word_bytes: i64,
}

impl ObjectLayout {
    pub(crate) fn new(fields: Arc<Vec<(String, Type)>>, pointer_bytes: u32) -> Self {
        let index = fields
            .iter()
            .enumerate()
            .map(|(index, (name, _))| (name.clone(), index))
            .collect();
        Self {
            fields,
            index,
            word_bytes: i64::from(willow_abi::storage_word_bytes(pointer_bytes)),
        }
    }

    pub(crate) fn fields(&self) -> &Arc<Vec<(String, Type)>> {
        &self.fields
    }

    /// Byte offset of field `index` from the object base.
    pub(crate) fn field_offset(&self, index: usize) -> i64 {
        (index as i64 + 1) * self.word_bytes
    }

    /// `(offset, declared type)` of the field named `name`.
    pub(crate) fn field(&self, name: &str) -> Option<(i64, &Type)> {
        let index = *self.index.get(name)?;
        Some((self.field_offset(index), &self.fields[index].1))
    }

    /// Bytes of the object payload: the descriptor word plus one storage word
    /// per field.
    pub(crate) fn size_bytes(&self) -> i64 {
        (self.fields.len() as i64 + 1) * self.word_bytes
    }
}

pub(crate) struct LayoutQueries {
    declarations: RefCell<HashMap<TypeId, ClassLayoutDeclaration>>,
    pending: RefCell<HashSet<TypeId>>,
    completed: RefCell<HashSet<TypeId>>,
    bases: QueryTable<TypeId, Option<TypeId>>,
    runtime_ids: QueryTable<TypeId, i64>,
    registered_runtime_ids: RefCell<HashMap<TypeId, i64>>,
    fields: QueryTable<TypeId, Vec<(String, Type)>>,
    slots: QueryTable<TypeId, MethodSlots>,
    interfaces: QueryTable<TypeId, crate::semantic::symbols::InterfaceInfo<TypeId>>,
    object_layouts: QueryTable<TargetLayoutKey, ObjectLayout>,
    #[cfg(test)]
    work: std::cell::Cell<[usize; 4]>,
    #[cfg(test)]
    declaration_visits: std::cell::Cell<usize>,
}

impl Default for LayoutQueries {
    fn default() -> Self {
        let queries = Self {
            declarations: Default::default(),
            pending: Default::default(),
            completed: Default::default(),
            bases: QueryTable::named("class_base"),
            runtime_ids: QueryTable::named("runtime_type_id"),
            registered_runtime_ids: Default::default(),
            fields: QueryTable::named("class_layout"),
            slots: QueryTable::named("class_vslots"),
            interfaces: QueryTable::named("interface_composition"),
            object_layouts: QueryTable::named("object_layout"),
            #[cfg(test)]
            work: Default::default(),
            #[cfg(test)]
            declaration_visits: Default::default(),
        };
        // The one builtin class with a field layout but no declaration: a
        // panic handler reads `PanicInfo` fields, while a user class that
        // `extends PanicInfo` inherits nothing from it and no runtime type id
        // is ever allocated for it. Installed outside the declaration tables
        // so it counts as neither a visit nor layout work.
        let panic_info = TypeId::from_source_name("PanicInfo");
        let fields = crate::semantic::builtin_types::panic_info_fields()
            .into_iter()
            .map(|(name, ty)| (name.to_string(), ty.into()))
            .collect::<Vec<_>>();
        queries
            .fields
            .query(panic_info, || Ok(fields))
            .expect("builtin layout");
        queries
            .slots
            .query(panic_info, || Ok(MethodSlots::default()))
            .expect("builtin slots");
        queries.completed.borrow_mut().insert(panic_info);
        queries
    }
}

impl LayoutQueries {
    pub(crate) fn has_class(&self, id: TypeId) -> bool {
        self.declarations.borrow().contains_key(&id)
    }

    // ── Frozen reads ──────────────────────────────────────────────────────
    //
    // Every accessor below answers from results that already exist: none
    // evaluates, allocates a runtime id, or records a query call, so backend
    // emission reading a layout in any order observes the same answers. A
    // successful read is counted as a `frozen_reads` reuse.

    /// Whether `id` has a completed (inheritance-resolved) field layout.
    pub(crate) fn has_layout(&self, id: TypeId) -> bool {
        self.fields.is_ready(&id)
    }

    /// The completed field layout of `id`, parent fields first.
    pub(crate) fn fields(&self, id: TypeId) -> Option<Arc<Vec<(String, Type)>>> {
        self.fields.ready(&id)
    }

    /// The completed virtual slot order of `id`.
    pub(crate) fn slots(&self, id: TypeId) -> Option<Arc<MethodSlots>> {
        self.slots.ready(&id)
    }

    /// The frozen direct base of `id`, if it was registered with one.
    pub(crate) fn base(&self, id: TypeId) -> Option<TypeId> {
        self.bases.ready(&id).and_then(|base| *base)
    }

    /// The runtime type id allocated for `id` at its declaration.
    pub(crate) fn runtime_id(&self, id: TypeId) -> Option<i64> {
        self.registered_runtime_ids.borrow().get(&id).copied()
    }

    /// Every class with a runtime type id, in no particular order.
    pub(crate) fn runtime_classes(&self) -> Vec<(TypeId, i64)> {
        self.registered_runtime_ids
            .borrow()
            .iter()
            .map(|(id, runtime_id)| (*id, *runtime_id))
            .collect()
    }

    /// The frozen composition of interface `id`.
    pub(crate) fn interface(
        &self,
        id: TypeId,
    ) -> Option<Arc<crate::semantic::symbols::InterfaceInfo<TypeId>>> {
        self.interfaces.ready(&id)
    }

    pub(crate) fn has_interface(&self, id: TypeId) -> bool {
        self.interfaces.is_ready(&id)
    }

    /// The byte layout of `key.ty` on `key`'s target, derived once from its
    /// completed field layout. `None` until that layout is complete.
    pub(crate) fn object_layout(&self, key: TargetLayoutKey) -> Option<Arc<ObjectLayout>> {
        if let Some(layout) = self.object_layouts.ready(&key) {
            return Some(layout);
        }
        let fields = self.fields(key.ty)?;
        self.object_layouts
            .query(key, || Ok(ObjectLayout::new(fields, key.pointer_bytes())))
            .ok()
    }

    /// Registration freezes own declarations, never provisional inherited data.
    /// Re-registration requests another consumer view of the same frozen result.
    pub(crate) fn register_class(
        &self,
        id: TypeId,
        base: Option<TypeId>,
        fields: Vec<(String, Type)>,
        methods: Vec<String>,
    ) {
        self.declarations
            .borrow_mut()
            .entry(id)
            .or_insert(ClassLayoutDeclaration {
                base,
                fields,
                methods,
            });
        self.pending.borrow_mut().insert(id);
    }

    /// Evaluate only requested declarations and their unfinished ancestors.
    /// Both field and virtual-slot queries use the same topological traversal;
    /// all methods are known before any backend method symbols are allocated.
    /// Returns every requested class, in completion order; their layouts are
    /// read back through [`Self::fields`] and [`Self::slots`].
    pub(crate) fn complete_pending_classes(&self) -> Result<Vec<TypeId>> {
        // Keep requests pending on errors so a repeated query cannot turn a
        // diagnosed cycle into a successful empty completion.
        let mut pending = self.pending.borrow().clone();
        let declarations = self.declarations.borrow();
        let mut completed = self.completed.borrow_mut();
        let mut path = Vec::new();
        let mut visiting = HashSet::new();
        let mut results = Vec::with_capacity(pending.len());
        let starts: Vec<_> = pending.iter().copied().collect();
        for start in starts {
            if !pending.contains(&start) {
                continue;
            }
            let mut current = start;
            while !completed.contains(&current) {
                anyhow::ensure!(
                    visiting.insert(current),
                    "cyclic class layout inheritance at {current}"
                );
                let declaration = &declarations[&current];
                #[cfg(test)]
                self.declaration_visits
                    .set(self.declaration_visits.get() + 1);
                path.push(current);
                let Some(base) = declaration
                    .base
                    .filter(|base| declarations.contains_key(base))
                else {
                    break;
                };
                current = base;
            }
            while let Some(id) = path.pop() {
                let declaration = &declarations[&id];
                // Missing/non-class parents never contribute a synthetic builtin
                // layout, matching the backend's previous declared-parent filter.
                let parent = declaration
                    .base
                    .filter(|base| declarations.contains_key(base));
                let parent_fields = parent
                    .map(|base| {
                        self.fields
                            .query(base, || anyhow::bail!("unfinished parent layout {base}"))
                    })
                    .transpose()?;
                let parent_slots = parent
                    .map(|base| {
                        self.slots
                            .query(base, || anyhow::bail!("unfinished parent slots {base}"))
                    })
                    .transpose()?;
                self.class_layout(
                    id,
                    parent_fields.as_deref().map(Vec::as_slice),
                    &declaration.fields,
                )?;
                self.class_vslots(id, parent_slots.as_deref(), &declaration.methods)?;
                completed.insert(id);
                visiting.remove(&id);
                pending.remove(&id);
                results.push(id);
            }
            // A previously evaluated declaration re-requested by a later unit
            // is complete already, without traversing its ancestors again.
            if pending.remove(&start) {
                anyhow::ensure!(
                    self.fields.is_ready(&start) && self.slots.is_ready(&start),
                    "missing completed layout {start}"
                );
                results.push(start);
            }
        }
        self.pending.borrow_mut().clear();
        Ok(results)
    }

    /// Freeze the resolved direct base with the canonical declaration. A base
    /// identity does not require its layout to have been evaluated yet.
    pub(crate) fn class_base(
        &self,
        id: TypeId,
        resolve: impl FnOnce() -> Option<TypeId>,
    ) -> Result<Option<TypeId>> {
        self.bases.query(id, || Ok(resolve())).map(|base| *base)
    }

    /// Allocate only at canonical class declaration time, in declaration order.
    pub(crate) fn register_runtime_type(&self, id: TypeId) -> Result<i64> {
        let mut registered = self.registered_runtime_ids.borrow_mut();
        let count = registered.len();
        if let std::collections::hash_map::Entry::Vacant(entry) = registered.entry(id) {
            let runtime_id = willow_abi::runtime_type_ids::generated_type_id(count)
                .ok_or_else(|| anyhow::anyhow!("generated runtime type ID range exhausted"))?;
            entry.insert(i64::from(runtime_id));
        }
        drop(registered);
        self.runtime_type_id(id)
    }

    /// Reading a runtime identity cannot allocate or depend on query order.
    /// Check registration before entering the table so an early invalid request
    /// cannot poison the eventual declaration with a cached query error.
    pub(crate) fn runtime_type_id(&self, id: TypeId) -> Result<i64> {
        let runtime_id = self
            .registered_runtime_ids
            .borrow()
            .get(&id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("runtime type requested before declaration: {id}"))?;
        self.runtime_ids.query(id, || Ok(runtime_id)).map(|id| *id)
    }

    /// Desugaring has already composed inherited methods in declaration order.
    /// Freeze the canonical semantic record once; aliases and backend consumers
    /// share it instead of converting and retaining separate method tables.
    pub(crate) fn interface_composition(
        &self,
        id: TypeId,
        compute: impl FnOnce() -> crate::semantic::symbols::InterfaceInfo<TypeId>,
    ) -> Result<Arc<crate::semantic::symbols::InterfaceInfo<TypeId>>> {
        self.interfaces.query(id, || {
            let info = compute();
            anyhow::ensure!(info.name == id, "interface query identity mismatch");
            Ok(info)
        })
    }

    /// Canonical identities and their parent/own declarations are immutable for
    /// this query table's lifetime. Repeated requests return the same allocation.
    pub(crate) fn class_layout(
        &self,
        id: TypeId,
        parent: Option<&[(String, Type)]>,
        own: &[(String, Type)],
    ) -> Result<Arc<Vec<(String, Type)>>> {
        self.fields.query(id, || {
            let parent = parent.unwrap_or_default();
            let mut fields = parent.to_vec();
            let mut names: HashSet<&str> = HashSet::with_capacity(parent.len());
            for (name, _) in parent {
                names.insert(name);
                #[cfg(test)]
                self.count_work(0);
            }
            for (name, ty) in own {
                #[cfg(test)]
                self.count_work(1);
                if names.insert(name) {
                    fields.push((name.clone(), ty.clone()));
                }
            }
            Ok(fields)
        })
    }

    /// Inherited virtual slots keep their indices; new methods append in source
    /// order. The canonical class identity has the same immutable-input contract.
    pub(crate) fn class_vslots(
        &self,
        id: TypeId,
        parent: Option<&MethodSlots>,
        own: &[String],
    ) -> Result<Arc<MethodSlots>> {
        self.slots.query(id, || {
            let mut slots = parent.cloned().unwrap_or_default();
            #[cfg(test)]
            {
                let mut work = self.work.get();
                work[2] += slots.len();
                self.work.set(work);
            }
            for name in own {
                #[cfg(test)]
                self.count_work(3);
                slots.insert(name);
            }
            Ok(slots)
        })
    }

    /// Test-only work counters: copied parent fields, own fields, copied
    /// parent slots, own methods.
    #[cfg(test)]
    pub(crate) fn work(&self) -> [usize; 4] {
        self.work.get()
    }

    /// Test-only count of declarations visited by `complete_pending_classes`.
    #[cfg(test)]
    pub(crate) fn declaration_visits(&self) -> usize {
        self.declaration_visits.get()
    }

    #[cfg(test)]
    fn count_work(&self, dimension: usize) {
        let mut work = self.work.get();
        work[dimension] += 1;
        self.work.set(work);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The builtin layouts every `LayoutQueries` starts with: `PanicInfo`'s
    /// fields and its (empty) slot table, computed once in `Default`.
    const BUILTIN: usize = 1;

    #[test]
    fn frozen_reads_report_every_reuse_of_a_completed_layout() {
        for reads in [1, 8, 64] {
            let queries = LayoutQueries::default();
            let id = TypeId::local("C");
            queries.register_class(id, None, vec![("f".into(), Type::I64)], vec![]);
            queries.complete_pending_classes().unwrap();
            let key = TargetLayoutKey::new(id, 8);
            let first = queries.object_layout(key).unwrap();
            let before = (queries.fields.stats(), queries.object_layouts.stats());
            for _ in 0..reads {
                assert!(Arc::ptr_eq(&first, &queries.object_layout(key).unwrap()));
                assert!(queries.fields(id).is_some());
            }
            let (fields, objects) = (queries.fields.stats(), queries.object_layouts.stats());
            assert_eq!(objects.computations, 1);
            assert_eq!(objects.frozen_reads - before.1.frozen_reads, reads);
            assert_eq!(fields.frozen_reads - before.0.frozen_reads, reads);
            assert_eq!(
                (objects.calls, objects.hits),
                (before.1.calls, before.1.hits)
            );
            assert_eq!((fields.calls, fields.hits), (before.0.calls, before.0.hits));
        }
    }

    #[test]
    fn frozen_class_layout_queries_visit_only_new_declarations_and_materialized_entries() {
        for size in [8, 32, 128] {
            let queries = LayoutQueries::default();
            for depth in (0..size).rev() {
                let id = TypeId::local(format!("C{depth}"));
                let base = (depth > 0).then(|| TypeId::local(format!("C{}", depth - 1)));
                queries.register_class(
                    id,
                    base,
                    vec![(format!("f{depth}"), Type::I64)],
                    vec![format!("m{depth}")],
                );
            }
            let layouts = queries.complete_pending_classes().unwrap();
            assert_eq!(layouts.len(), size);
            assert_eq!(queries.declaration_visits.get(), size);
            for id in &layouts {
                let depth: usize = id.name().strip_prefix('C').unwrap().parse().unwrap();
                let fields = queries.fields(*id).unwrap();
                let slots = queries.slots(*id).unwrap();
                assert_eq!(fields.len(), depth + 1);
                assert_eq!(slots.len(), depth + 1);
                for index in 0..=depth {
                    assert_eq!(fields[index].0, format!("f{index}"));
                    assert_eq!(slots.slot_of(&format!("m{index}")), Some(index));
                }
            }
            let copied = size * (size - 1) / 2;
            assert_eq!(queries.work.get(), [copied, size, copied, size]);
            for _ in 0..16 {
                assert!(queries.complete_pending_classes().unwrap().is_empty());
            }
            let root = TypeId::local("C0");
            let root_fields = queries.fields(root).unwrap();
            let root_slots = queries.slots(root).unwrap();
            queries.register_class(root, None, vec![], vec![]);
            let repeat = queries.complete_pending_classes().unwrap();
            assert_eq!(repeat, [root]);
            assert!(Arc::ptr_eq(&queries.fields(root).unwrap(), &root_fields));
            assert!(Arc::ptr_eq(&queries.slots(root).unwrap(), &root_slots));
            assert_eq!(queries.declaration_visits.get(), size);
            // Later units add one class; no pass revisits the prior chain.
            for index in 0..size {
                queries.register_class(
                    TypeId::local(format!("Leaf{index}")),
                    Some(root),
                    vec![],
                    vec![],
                );
                assert_eq!(queries.complete_pending_classes().unwrap().len(), 1);
                assert_eq!(queries.declaration_visits.get(), size + index + 1);
            }
            assert_eq!(
                queries.work.get(),
                [copied + size, size, copied + size, size]
            );
        }
    }

    #[test]
    fn frozen_class_layout_queries_preserve_missing_parent_and_reject_cycles() {
        let queries = LayoutQueries::default();
        let a = TypeId::local("A");
        let b = TypeId::local("B");
        queries.register_class(
            a,
            Some(b),
            vec![("own".into(), Type::I64)],
            vec!["own".into()],
        );
        assert_eq!(queries.complete_pending_classes().unwrap(), [a]);
        assert_eq!(queries.fields(a).unwrap().len(), 1);
        assert_eq!(queries.slots(a).unwrap().as_slice(), ["own"]);
        let cyclic = LayoutQueries::default();
        cyclic.register_class(a, Some(b), vec![], vec![]);
        cyclic.register_class(b, Some(a), vec![], vec![]);
        for _ in 0..2 {
            assert!(
                cyclic
                    .complete_pending_classes()
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("cyclic class layout")
            );
        }
    }

    #[test]
    fn class_base_queries_freeze_roots_and_forward_edges_per_session() {
        let child = TypeId::local("Child");
        let root = TypeId::local("Root");
        let other = TypeId::local("Other");
        let queries = LayoutQueries::default();
        assert_eq!(
            queries.class_base(child, || Some(root)).unwrap(),
            Some(root)
        );
        assert_eq!(queries.class_base(root, || None).unwrap(), None);
        for _ in 0..16 {
            assert_eq!(
                queries
                    .class_base(root, || panic!("root recomputed"))
                    .unwrap(),
                None
            );
            assert_eq!(
                queries
                    .class_base(child, || panic!("edge recomputed"))
                    .unwrap(),
                Some(root)
            );
        }
        assert_eq!(queries.bases.stats().computations, 2);
        assert_eq!(queries.bases.stats().hits, 32);
        let next_session = LayoutQueries::default();
        assert_eq!(
            next_session.class_base(child, || Some(other)).unwrap(),
            Some(other)
        );
    }

    #[test]
    fn generated_class_type_ids_reuse_declarations_under_reversed_reads() {
        for count in [1, 16, 64, 256] {
            let queries = LayoutQueries::default();
            let classes: Vec<_> = (0..count).map(|i| TypeId::local(format!("C{i}"))).collect();
            for (i, &class) in classes.iter().enumerate() {
                assert!(queries.runtime_type_id(class).is_err());
                assert_eq!(queries.register_runtime_type(class).unwrap(), i as i64 + 1);
            }
            for _ in 0..8 {
                for (i, &class) in classes.iter().enumerate().rev() {
                    assert_eq!(queries.runtime_type_id(class).unwrap(), i as i64 + 1);
                }
            }
            assert_eq!(queries.registered_runtime_ids.borrow().len(), count);
            assert_eq!(queries.runtime_ids.stats().computations, count);
            assert_eq!(queries.runtime_ids.stats().hits, 8 * count);
        }
    }

    #[test]
    fn interface_composition_is_shared_per_canonical_identity() {
        let source = "interface Base { fn a(self) -> i64; } interface Derived extends Base { fn b(self) -> i64; }";
        let (mut program, errors) =
            crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
                .parse();
        assert!(errors.is_empty());
        let desugared = crate::desugar::DesugarPass::run(&mut program, &mut []);
        assert!(desugared.diagnostics.is_empty());
        let mut checker = crate::semantic::TypeChecker::new();
        checker.check_program(&program);
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        let declaration = checker.symbols.lookup_interface("Derived").unwrap();
        for count in [16, 64, 256] {
            let queries = LayoutQueries::default();
            for index in 0..count {
                let id = TypeId::local(format!("Derived{index}"));
                let first = queries
                    .interface_composition(id, || {
                        let mut info = declaration.to_semantic();
                        info.name = id;
                        info
                    })
                    .unwrap();
                assert_eq!(first.method_order.as_slice(), ["a", "b"]);
                assert_eq!(first.method_order.slot_of("a"), Some(0));
                assert_eq!(first.extends, [TypeId::local("Base")]);
                for _ in 0..8 {
                    let again = queries
                        .interface_composition(id, || panic!("recomputed interface"))
                        .unwrap();
                    assert!(Arc::ptr_eq(&first, &again));
                }
            }
            assert_eq!(queries.interfaces.stats().computations, count);
            assert_eq!(queries.interfaces.stats().hits, 8 * count);
        }
    }

    #[test]
    fn inherited_fields_and_slots_preserve_order_and_identity() {
        let queries = LayoutQueries::default();
        let parent_fields = vec![("old".into(), Type::I64), ("base".into(), Type::Bool)];
        let own = vec![
            ("old".into(), Type::F64),
            ("new".into(), Type::F64),
            ("new".into(), Type::Bool),
        ];
        let parent_slots = MethodSlots::from(vec!["old".into(), "base".into()]);
        let methods = vec!["old".into(), "new".into(), "new".into()];
        let id = TypeId::local("Derived");
        let fields = queries
            .class_layout(id, Some(&parent_fields), &own)
            .unwrap();
        let slots = queries
            .class_vslots(id, Some(&parent_slots), &methods)
            .unwrap();
        assert_eq!(
            *fields,
            vec![
                ("old".into(), Type::I64),
                ("base".into(), Type::Bool),
                ("new".into(), Type::F64)
            ]
        );
        assert_eq!(slots.as_slice(), ["old", "base", "new"]);
        assert_eq!(slots.slot_of("old"), Some(0));
        assert_eq!(slots.slot_of("new"), Some(2));
        let work = queries.work.get();
        for _ in 0..16 {
            assert!(Arc::ptr_eq(
                &fields,
                &queries
                    .class_layout(id, Some(&parent_fields), &own)
                    .unwrap()
            ));
            assert!(Arc::ptr_eq(
                &slots,
                &queries
                    .class_vslots(id, Some(&parent_slots), &methods)
                    .unwrap()
            ));
        }
        assert_eq!(queries.work.get(), work);
        assert_eq!(queries.fields.stats().computations, BUILTIN + 1);
        assert_eq!(queries.slots.stats().computations, BUILTIN + 1);
        assert_eq!(queries.fields.stats().hits, 16);
        assert_eq!(queries.slots.stats().hits, 16);
        let other = TypeId::local("Other");
        assert!(queries.class_layout(other, None, &[]).unwrap().is_empty());
        assert!(queries.class_vslots(other, None, &[]).unwrap().is_empty());
        assert_eq!(queries.fields.stats().computations, BUILTIN + 2);
        assert_eq!(queries.slots.stats().computations, BUILTIN + 2);
    }

    #[test]
    fn deep_chains_scale_with_materialized_layout_entries() {
        for size in [16, 64, 256] {
            let queries = LayoutQueries::default();
            let mut parent_fields: Option<Arc<Vec<(String, Type)>>> = None;
            let mut parent_slots: Option<Arc<MethodSlots>> = None;
            let mut materialized = 0;
            for depth in 0..size {
                let id = TypeId::local(format!("Chain{depth}"));
                let own = vec![(format!("f{depth}"), Type::I64)];
                let methods = vec![format!("m{depth}")];
                let fields = queries
                    .class_layout(id, parent_fields.as_deref().map(Vec::as_slice), &own)
                    .unwrap();
                let slots = queries
                    .class_vslots(id, parent_slots.as_deref(), &methods)
                    .unwrap();
                assert_eq!(fields.len(), depth + 1);
                assert_eq!(slots.len(), depth + 1);
                materialized += fields.len();
                for _ in 0..4 {
                    queries
                        .class_layout(id, parent_fields.as_deref().map(Vec::as_slice), &own)
                        .unwrap();
                    queries
                        .class_vslots(id, parent_slots.as_deref(), &methods)
                        .unwrap();
                }
                parent_fields = Some(fields);
                parent_slots = Some(slots);
            }
            let copied = size * (size - 1) / 2;
            assert_eq!(materialized, size * (size + 1) / 2);
            assert_eq!(queries.work.get(), [copied, size, copied, size]);
            assert_eq!(queries.fields.stats().computations, BUILTIN + size);
            assert_eq!(queries.slots.stats().computations, BUILTIN + size);
            eprintln!(
                "layout-chain size={size} materialized={materialized} work={:?}",
                queries.work.get()
            );
        }
    }

    #[test]
    fn fanout_with_overrides_scales_with_output_and_own_declarations() {
        for width in [16, 64, 256, 1024] {
            let queries = LayoutQueries::default();
            let root = TypeId::local("Root");
            let fields = queries
                .class_layout(root, None, &[("base".into(), Type::I64)])
                .unwrap();
            let slots = queries.class_vslots(root, None, &["base".into()]).unwrap();
            let mut materialized = fields.len();
            for child in 0..width {
                let id = TypeId::local(format!("Child{child}"));
                let own = [("base".into(), Type::Bool), ("child".into(), Type::Bool)];
                let methods = ["base".into(), "child".into()];
                let child_fields = queries.class_layout(id, Some(&fields), &own).unwrap();
                let child_slots = queries.class_vslots(id, Some(&slots), &methods).unwrap();
                assert_eq!(child_fields.len(), 2);
                assert_eq!(child_fields[0].1, Type::I64);
                assert_eq!(child_slots.slot_of("child"), Some(1));
                materialized += child_fields.len();
                for _ in 0..4 {
                    queries.class_layout(id, Some(&fields), &own).unwrap();
                    queries.class_vslots(id, Some(&slots), &methods).unwrap();
                }
            }
            assert_eq!(
                queries.work.get(),
                [width, 1 + 2 * width, width, 1 + 2 * width]
            );
            assert_eq!(materialized, 1 + 2 * width);
            assert_eq!(queries.fields.stats().computations, BUILTIN + width + 1);
            assert_eq!(queries.slots.stats().computations, BUILTIN + width + 1);
            eprintln!(
                "layout-fanout width={width} materialized={materialized} work={:?}",
                queries.work.get()
            );
        }
    }
}
