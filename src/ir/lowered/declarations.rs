use super::{Type, TypeId};
use crate::ir::typed_ast::{
    HirEnumInfo, HirEnumVariant, HirResolution, HirSignature, HirTypeParameters,
};
use std::cell::{Ref, RefCell};
use std::collections::{HashMap, hash_map::Entry};

// Each class adds only its newly introduced fields. A fieldless subclass
// reuses the same head. Integer links let deep layouts drop iteratively.
#[derive(Clone, Copy, Default)]
struct FieldLayout {
    len: usize,
    head: Option<usize>,
}

type FieldLink<'a> = (&'a (String, Type), Option<usize>);

impl FieldLayout {
    fn pop<'a>(&mut self, fields: &[FieldLink<'a>]) -> Option<&'a (String, Type)> {
        let (field, previous) = fields[self.head?];
        self.head = previous;
        self.len -= 1;
        Some(field)
    }
}

#[cfg(not(test))]
type FieldNameHasher = std::hash::RandomState;

#[cfg(test)]
#[derive(Clone, Default)]
struct FieldNameHasher(std::hash::RandomState);

#[cfg(test)]
impl std::hash::BuildHasher for FieldNameHasher {
    type Hasher = <std::hash::RandomState as std::hash::BuildHasher>::Hasher;
    fn build_hasher(&self) -> Self::Hasher {
        FIELD_NAME_HASHES.set(FIELD_NAME_HASHES.get() + 1);
        self.0.build_hasher()
    }
}

#[derive(Clone, Default)]
struct ResolvedFields {
    order: FieldLayout,
    // Each branch shares its parent's hash trie up to the names it introduces.
    names: Option<im::HashSet<usize, FieldNameHasher>>,
}

#[derive(Default)]
struct FieldLayouts<'a> {
    classes: HashMap<TypeId, Option<ResolvedFields>>,
    fields: Vec<FieldLink<'a>>,
    // Hash the borrowed spelling once per declaration. Branches store its stable index.
    names: indexmap::IndexSet<&'a str, FieldNameHasher>,
}

/// Visit the most recently declared field first, in stack scheduling order.
pub(super) struct FieldsRev<'c, 'a> {
    layouts: Ref<'c, FieldLayouts<'a>>,
    remaining: FieldLayout,
}

impl<'a> Iterator for FieldsRev<'_, 'a> {
    type Item = &'a (String, Type);

    fn next(&mut self) -> Option<Self::Item> {
        self.remaining.pop(&self.layouts.fields)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining.len, Some(self.remaining.len))
    }
}
impl ExactSizeIterator for FieldsRev<'_, '_> {}

/// Both lowering passes borrow the same immutable program declarations. Build
/// indexes only for declarations that lowering actually inspects.
pub(super) struct LoweringDeclarations<'a> {
    hir: &'a HirResolution,
    layouts: RefCell<FieldLayouts<'a>>,
    methods: RefCell<MethodIndex<'a>>,
    variants: RefCell<HashMap<TypeId, VariantIndex<'a>>>,
}

// A receiver's generic arguments affect specialization, never the declaration
// found by a name lookup. Retain only unbound declarations in the shared index.
#[derive(Clone, Copy)]
pub(super) struct DeclaredMethod<'a> {
    pub(super) signature: &'a HirSignature,
    pub(super) interface_params: Option<&'a HirTypeParameters>,
}

// A methodless class shares the owner's native method table for every spelling.
// Only an inherited lookup through multiple method-bearing owners needs an overlay.
struct MethodIndex<'a> {
    ancestry: MethodAncestry<'a>,
    sparse: Option<SparseMethods<'a>>,
}

type MethodAnswers<'a> = indexmap::IndexMap<usize, Option<DeclaredMethod<'a>>>;

#[derive(Default)]
struct SparseMethods<'a> {
    answers: HashMap<Box<str>, MethodAnswers<'a>>,
    entries: usize,
    budget: usize,
    counted_declarations: bool,
}

#[derive(Default)]
struct MethodAncestry<'a> {
    routes: indexmap::IndexMap<TypeId, Option<usize>>,
    owners: Vec<MethodOwner<'a>>,
    names: indexmap::IndexSet<&'a str>,
    signatures: Vec<DeclaredMethod<'a>>,
}

#[derive(Clone, Copy)]
struct MethodTable<'a> {
    methods: &'a HashMap<String, HirSignature>,
    interface_params: Option<&'a HirTypeParameters>,
}

impl<'a> MethodTable<'a> {
    fn get(self, method: &str) -> Option<DeclaredMethod<'a>> {
        #[cfg(test)]
        METHOD_TABLE_PROBES.set(METHOD_TABLE_PROBES.get() + 1);
        Some(DeclaredMethod {
            signature: self.methods.get(method)?,
            interface_params: self.interface_params,
        })
    }
}

struct MethodOwner<'a> {
    table: MethodTable<'a>,
    base: Option<TypeId>,
    complete: Option<MethodLayout<'a>>,
}

#[derive(Clone)]
struct MethodLayout<'a> {
    root: MethodTable<'a>,
    overrides: Option<im::HashMap<usize, usize>>,
}

struct VariantIndex<'a> {
    declaration: &'a HirEnumInfo,
    indexed: usize,
    by_name: HashMap<&'a str, &'a HirEnumVariant>,
    // Scalar dispatch uses the first declaration. The CFG dispatcher also
    // selects by arity when a repeated spelling has another payload count.
    other_arities: HashMap<(&'a str, usize), &'a HirEnumVariant>,
    payload_free: Option<bool>,
}

impl<'a> LoweringDeclarations<'a> {
    pub(super) fn new(hir: &'a HirResolution) -> Self {
        Self {
            hir,
            layouts: Default::default(),
            methods: RefCell::new(MethodIndex {
                ancestry: Default::default(),
                sparse: Some(SparseMethods {
                    budget: hir.classes.len(),
                    ..Default::default()
                }),
            }),
            variants: Default::default(),
        }
    }

    pub(super) fn declared_method(
        &self,
        class: TypeId,
        method: &str,
    ) -> Option<DeclaredMethod<'a>> {
        if self.hir.classes.is_empty() && self.hir.interfaces.is_empty() {
            return None;
        }
        self.methods.borrow_mut().lookup(self.hir, class, method)
    }

    #[cfg(test)]
    pub(super) fn methodless_chain_entries(&self) -> usize {
        let index = self.methods.borrow();
        let ancestry = &index.ancestry;
        // These fixtures have a single method-bearing owner. Check every other
        // source of retained entries before counting routes and owner records.
        assert!(ancestry.names.is_empty() && ancestry.signatures.is_empty());
        assert!(ancestry.owners.iter().all(|owner| owner.complete.is_none()));
        assert!(index.sparse.as_ref().is_some_and(|s| s.answers.is_empty()));
        ancestry.routes.len() + ancestry.owners.len()
    }

    pub(super) fn fields_rev(&self, class: &TypeId) -> Option<FieldsRev<'_, 'a>> {
        if self.hir.classes.is_empty() {
            return None;
        }
        let layouts = self.layouts.borrow();
        if let Some(resolved) = layouts.classes.get(class) {
            let remaining = resolved.as_ref()?.order;
            return Some(FieldsRev { layouts, remaining });
        }
        drop(layouts);
        let remaining = self.layouts.borrow_mut().resolve(*class, self.hir)?;
        Some(FieldsRev {
            layouts: self.layouts.borrow(),
            remaining,
        })
    }

    fn with_variants<T>(
        &self,
        class: &TypeId,
        query: impl FnOnce(&mut VariantIndex<'a>) -> T,
    ) -> Option<T> {
        let mut variants = self.variants.borrow_mut();
        if let Some(index) = variants.get_mut(class) {
            return Some(query(index));
        }
        let declaration = self.hir.enums.get(class)?;
        let index = variants.entry(*class).or_insert_with(|| VariantIndex {
            declaration,
            indexed: 0,
            by_name: HashMap::new(),
            other_arities: HashMap::new(),
            payload_free: None,
        });
        Some(query(index))
    }

    pub(super) fn enum_variant(
        &self,
        class: &TypeId,
        name: &str,
        arity: Option<usize>,
    ) -> Option<(&'a HirEnumInfo, &'a HirEnumVariant)> {
        self.with_variants(class, |index| {
            index
                .lookup(name, arity)
                .map(|variant| (index.declaration, variant))
        })?
    }

    pub(super) fn enum_is_payload_free(&self, class: &TypeId) -> bool {
        self.with_variants(class, |index| {
            *index.payload_free.get_or_insert_with(|| {
                index.declaration.variants[index.indexed..]
                    .iter()
                    .all(|v| v.payloads.is_empty())
            })
        })
        .unwrap_or(false)
    }
}

impl<'a> MethodIndex<'a> {
    fn lookup(
        &mut self,
        hir: &'a HirResolution,
        class: TypeId,
        method: &str,
    ) -> Option<DeclaredMethod<'a>> {
        let ancestry = &mut self.ancestry;
        let owner = ancestry.owner(hir, class)?;
        let entry = &ancestry.owners[owner];
        if let Some(found) = entry.table.get(method) {
            return Some(found);
        }
        let base = ancestry.owner(hir, entry.base?)?;
        let parent = &ancestry.owners[base];
        if parent.base.is_none() {
            return parent.table.get(method);
        }

        // A cold lookup can borrow even a very wide table and inspect just this
        // spelling. Switch to shared layouts once the accumulated probes can pay
        // for indexing the declarations. Neither strategy copies a signature.
        if let Some(sparse) = &mut self.sparse {
            let answers = if let Some(answers) = sparse.answers.get_mut(method) {
                answers
            } else {
                sparse.answers.entry(method.into()).or_default()
            };
            let start = answers.len();
            let mut current = base;
            let found = loop {
                let slot = match answers.entry(current) {
                    indexmap::map::Entry::Occupied(entry) => break *entry.get(),
                    indexmap::map::Entry::Vacant(slot) => slot,
                };
                slot.insert(None);
                let entry = &ancestry.owners[current];
                if let Some(found) = entry.table.get(method) {
                    break Some(found);
                }
                let Some(next) = entry.base.and_then(|id| ancestry.owner(hir, id)) else {
                    break None;
                };
                current = next;
            };
            if found.is_some() {
                for answer in answers[start..].values_mut() {
                    *answer = found;
                }
            }
            sparse.entries += answers.len() - start;
            if sparse.entries > sparse.budget {
                if !sparse.counted_declarations {
                    // The class-count threshold pays for reading the table lengths.
                    sparse.budget += hir
                        .classes
                        .values()
                        .filter(|c| c.base.is_some())
                        .map(|c| c.methods.len())
                        .sum::<usize>();
                    sparse.counted_declarations = true;
                }
                if sparse.entries > sparse.budget {
                    self.sparse = None;
                }
            }
            return found;
        }
        if parent.complete.is_none() {
            ancestry.resolve_layout(hir, base);
        }
        let layout = ancestry.owners[base].complete.as_ref()?;
        if let Some(overrides) = &layout.overrides
            && let Some(name) = ancestry.names.get_index_of(method)
            && let Some(slot) = overrides.get(&name)
        {
            return Some(ancestry.signatures[*slot]);
        }
        layout.root.get(method)
    }
}

impl<'a> MethodAncestry<'a> {
    fn owner(&mut self, hir: &'a HirResolution, mut class: TypeId) -> Option<usize> {
        let start = self.routes.len();
        let owner = loop {
            #[cfg(test)]
            METHOD_CACHE_PROBES.set(METHOD_CACHE_PROBES.get() + 1);
            let slot = match self.routes.entry(class) {
                indexmap::map::Entry::Occupied(entry) => break *entry.get(),
                indexmap::map::Entry::Vacant(slot) => slot,
            };
            #[cfg(test)]
            METHOD_ANCESTRY_PROBES.set(METHOD_ANCESTRY_PROBES.get() + 1);
            let (methods, interface_params, base) = if let Some(info) = hir.interfaces.get(&class) {
                (&info.methods, Some(&info.type_params), None)
            } else {
                let Some(info) = hir.classes.get(&class) else {
                    break None;
                };
                (&info.methods, None, info.base)
            };
            // A provisional miss terminates a cycle consisting of empty tables.
            slot.insert(None);
            if !methods.is_empty() {
                let id = self.owners.len();
                self.owners.push(MethodOwner {
                    table: MethodTable {
                        methods,
                        interface_params,
                    },
                    base,
                    complete: None,
                });
                break Some(id);
            }
            let Some(base) = base else { break None };
            class = base;
        };
        if owner.is_some() {
            for slot in self.routes[start..].values_mut() {
                *slot = owner;
            }
        }
        owner
    }

    fn resolve_layout(&mut self, hir: &'a HirResolution, mut owner: usize) {
        let mut path = indexmap::IndexSet::new();
        let mut layout = loop {
            let entry = &self.owners[owner];
            if let Some(layout) = &entry.complete {
                break layout.clone();
            }
            let table = entry.table;
            let root = MethodLayout {
                root: table,
                overrides: None,
            };
            let Some(base) = entry.base.and_then(|id| self.owner(hir, id)) else {
                self.owners[owner].complete = Some(root.clone());
                break root;
            };
            let (position, fresh) = path.insert_full(owner);
            if !fresh {
                // Malformed cyclic declarations still resolve the nearest override
                // from each starting owner. Seed one full turn before unwinding.
                let mut cycle = root;
                for &id in path[position..].iter().rev() {
                    self.add_declarations(&mut cycle, id);
                }
                break cycle;
            }
            owner = base;
        };
        for owner in path.into_iter().rev() {
            self.add_declarations(&mut layout, owner);
            self.owners[owner].complete = Some(layout.clone());
        }
    }

    fn add_declarations(&mut self, layout: &mut MethodLayout<'a>, owner: usize) {
        let overrides = layout.overrides.get_or_insert_with(Default::default);
        let table = self.owners[owner].table;
        for (name, signature) in table.methods {
            let (key, _) = self.names.insert_full(name);
            overrides.insert(key, self.signatures.len());
            self.signatures.push(DeclaredMethod {
                signature,
                interface_params: table.interface_params,
            });
        }
    }
}

impl<'a> VariantIndex<'a> {
    fn lookup(&mut self, name: &str, arity: Option<usize>) -> Option<&'a HirEnumVariant> {
        #[cfg(test)]
        VARIANT_PROBES.set(VARIANT_PROBES.get() + 1);
        if let Some(&first) = self.by_name.get(name) {
            match arity {
                Some(n) if n != first.payloads.len() => {
                    if let Some(&variant) = self.other_arities.get(&(name, n)) {
                        return Some(variant);
                    }
                }
                _ => return Some(first),
            }
        }
        // Advance only to the first eligible declaration. All subsequent
        // lookups reuse the scanned prefix, including after an unsuccessful one.
        for variant in &self.declaration.variants[self.indexed..] {
            self.indexed += 1;
            #[cfg(test)]
            VARIANTS_INDEXED.set(VARIANTS_INDEXED.get() + 1);
            let spelling = variant.name.as_str();
            let n = variant.payloads.len();
            let first = self.by_name.entry(spelling).or_insert(variant);
            if first.payloads.len() != n {
                self.other_arities.entry((spelling, n)).or_insert(variant);
            }
            if n != 0 {
                self.payload_free = Some(false);
            }
            if spelling == name && arity.is_none_or(|expected| expected == n) {
                return Some(variant);
            }
        }
        None
    }
}

impl<'a> FieldLayouts<'a> {
    fn resolve(&mut self, class: TypeId, hir: &'a HirResolution) -> Option<FieldLayout> {
        #[cfg(test)]
        super::CLASS_FIELD_ENUMERATIONS.set(super::CLASS_FIELD_ENUMERATIONS.get() + 1);
        let mut chain = Vec::new();
        let mut current = class;
        // A temporary None marks a declaration on this walk. Encountering it
        // again detects a cycle. Invalid chains retain their negative result.
        let mut resolved = loop {
            let info = match self.classes.entry(current) {
                Entry::Occupied(entry) => break entry.get().clone()?,
                Entry::Vacant(entry) => {
                    let info = hir.classes.get(&current)?;
                    entry.insert(None);
                    info
                }
            };
            #[cfg(test)]
            FIELD_CLASS_VISITS.set(FIELD_CLASS_VISITS.get() + 1);
            chain.push((current, info));
            let Some(base) = info.base else {
                break ResolvedFields::default();
            };
            current = base;
        };
        for (id, info) in chain.into_iter().rev() {
            if !info.fields.is_empty() {
                let names = resolved.names.get_or_insert_with(Default::default);
                for field in &info.fields {
                    let (name, first_declaration) = self.names.insert_full(field.0.as_str());
                    if first_declaration || !names.contains(&name) {
                        names.insert(name);
                        let next = self.fields.len();
                        self.fields.push((field, resolved.order.head));
                        resolved.order.head = Some(next);
                        resolved.order.len += 1;
                    }
                }
            }
            self.classes.insert(id, Some(resolved.clone()));
        }
        Some(resolved.order)
    }
}

impl std::ops::Deref for LoweringDeclarations<'_> {
    type Target = HirResolution;
    fn deref(&self) -> &Self::Target {
        self.hir
    }
}

#[cfg(test)]
std::thread_local! {
    pub(super) static METHOD_TABLE_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static METHOD_CACHE_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static METHOD_ANCESTRY_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FIELD_NAME_HASHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static FIELD_CLASS_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static VARIANTS_INDEXED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static VARIANT_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::typed_ast::HirClassInfo;

    #[test]
    fn inherited_method_cache_limits_sparse_walks_and_shares_overrides() {
        let ids = ["Root", "Wide", "Left", "Right", "A", "B"].map(TypeId::local);
        let [root, wide, left, right, a, b] = ids;
        let mut hir = HirResolution::default();
        for (owner, base, names) in [
            (root, None, vec!["root", "shared"]),
            (wide, Some(root), vec!["wide", "shared"]),
            (left, Some(wide), vec!["left", "shared"]),
            (right, Some(wide), vec!["right"]),
            (a, Some(left), vec!["a"]),
            (b, Some(right), vec!["b"]),
        ] {
            let mut info = HirClassInfo {
                base,
                ..Default::default()
            };
            for name in names {
                info.methods.insert(
                    name.into(),
                    HirSignature {
                        params: vec![],
                        param_modes: vec![],
                        is_static: false,
                        is_async: false,
                        return_type: Type::I64,
                    },
                );
            }
            hir.classes.insert(owner, info);
        }
        let sig = hir.classes[&wide].methods["wide"].clone();
        for n in 0..128 {
            hir.classes
                .get_mut(&wide)
                .unwrap()
                .methods
                .insert(format!("unused{n}"), sig.clone());
        }
        let view = LoweringDeclarations::new(&hir);
        for pass in 0..2 {
            for (receiver, name, owner) in [
                (a, "root", root),
                (a, "shared", left),
                (b, "shared", wide),
                (a, "wide", wide),
                (b, "root", root),
                (b, "right", right),
            ] {
                assert!(std::ptr::eq(
                    view.declared_method(receiver, name).unwrap().signature,
                    &hir.classes[&owner].methods[name]
                ));
            }
            assert!(view.declared_method(a, "right").is_none());
            assert!(view.declared_method(b, "left").is_none());
            if pass == 0 {
                assert!(view.methods.borrow().ancestry.signatures.is_empty());
                for n in 0..160 {
                    assert!(view.declared_method(a, &format!("absent{n}")).is_none());
                }
                assert!(view.methods.borrow().sparse.is_none());
            }
        }
        let indexed = view.methods.borrow().ancestry.signatures.len();
        assert_eq!(
            indexed,
            [wide, left, right]
                .iter()
                .map(|id| hir.classes[id].methods.len())
                .sum::<usize>()
        );
        for _ in 0..32 {
            assert!(view.declared_method(a, "absent").is_none());
            assert!(view.declared_method(b, "unused127").is_some());
        }
        assert_eq!(view.methods.borrow().ancestry.signatures.len(), indexed);
    }

    #[test]
    fn construction_lookups_share_prefixes_and_deduplicate_each_branch() {
        let base = TypeId::local("Root");
        for width in [1, 8, 32] {
            let mut hir = HirResolution::default();
            hir.classes.insert(
                base,
                HirClassInfo {
                    fields: (0..width).map(|i| (format!("f{i}"), Type::I64)).collect(),
                    ..Default::default()
                },
            );
            let mut leaves = Vec::new();
            for i in 0..width {
                let child = TypeId::local(format!("C{i}"));
                let leaf = TypeId::local(format!("L{i}"));
                hir.classes.insert(
                    child,
                    HirClassInfo {
                        base: Some(base),
                        fields: vec![("f0".into(), Type::Bool), ("own".into(), Type::Char)],
                        ..Default::default()
                    },
                );
                hir.classes.insert(
                    leaf,
                    HirClassInfo {
                        base: Some(child),
                        fields: vec![("own".into(), Type::Bool), ("extra".into(), Type::I64)],
                        ..Default::default()
                    },
                );
                leaves.push((child, leaf));
            }
            let declarations = LoweringDeclarations::new(&hir);
            for (child, _) in &leaves {
                assert_eq!(declarations.fields_rev(child).unwrap().len(), width + 1);
            }
            for (child, leaf) in leaves.iter().rev() {
                let mut fields = declarations.fields_rev(leaf).unwrap();
                assert_eq!(fields.len(), width + 2);
                assert_eq!(fields.next().unwrap().0, "extra");
                assert!(std::ptr::eq(
                    fields.next().unwrap(),
                    &hir.classes[child].fields[1]
                ));
                assert!(std::ptr::eq(
                    fields.last().unwrap(),
                    &hir.classes[&base].fields[0]
                ));
            }
            // One slot for each original base field and two per branch.
            assert_eq!(declarations.layouts.borrow().fields.len(), 3 * width);
        }
    }

    #[test]
    fn construction_lookups_alternate_unrelated_wide_bases_without_rehashing_prefixes() {
        let bases = [TypeId::local("Left"), TypeId::local("Right")];
        for width in [1, 8, 128] {
            for children in [1, 8, 64] {
                let mut hir = HirResolution::default();
                let mut order = Vec::new();
                for (side, base) in bases.iter().enumerate() {
                    hir.classes.insert(
                        *base,
                        HirClassInfo {
                            fields: (0..width)
                                .map(|i| (format!("f{side}_{i}"), Type::I64))
                                .collect(),
                            ..Default::default()
                        },
                    );
                }
                for i in 0..children {
                    for (side, base) in bases.iter().enumerate() {
                        let child = TypeId::local(format!("C{side}_{i}"));
                        hir.classes.insert(
                            child,
                            HirClassInfo {
                                base: Some(*base),
                                // This name belongs to the other base, never to this one.
                                fields: vec![(format!("f{}_0", 1 - side), Type::Bool)],
                                ..Default::default()
                            },
                        );
                        order.push(child);
                    }
                }
                let declarations = LoweringDeclarations::new(&hir);
                FIELD_NAME_HASHES.set(0);
                for base in bases {
                    assert_eq!(declarations.fields_rev(&base).unwrap().len(), width);
                }
                let base_hashes = FIELD_NAME_HASHES.get();
                assert_eq!(base_hashes, 4 * width);
                for child in &order {
                    assert_eq!(declarations.fields_rev(child).unwrap().len(), width + 1);
                }
                let child_hashes = FIELD_NAME_HASHES.get() - base_hashes;
                assert_eq!(child_hashes, 6 * children);
                for child in order.iter().rev() {
                    let mut fields = declarations.fields_rev(child).unwrap();
                    assert!(std::ptr::eq(
                        fields.next().unwrap(),
                        &hir.classes[child].fields[0]
                    ));
                    assert_eq!(fields.len(), width);
                }
                assert_eq!(FIELD_NAME_HASHES.get(), base_hashes + child_hashes);
            }
        }
    }

    #[test]
    fn construction_lookups_reject_incomplete_and_cyclic_field_layouts() {
        let mut hir = HirResolution::default();
        let a = TypeId::local("A");
        let b = TypeId::local("B");
        let c = TypeId::local("C");
        let missing = TypeId::local("Missing");
        for (id, parent) in [(a, b), (b, a), (c, missing)] {
            hir.classes.insert(
                id,
                HirClassInfo {
                    base: Some(parent),
                    ..Default::default()
                },
            );
        }
        let declarations = LoweringDeclarations::new(&hir);
        FIELD_CLASS_VISITS.set(0);
        for _ in 0..8 {
            for id in [a, b, c, missing] {
                assert!(declarations.fields_rev(&id).is_none());
            }
        }
        assert_eq!(FIELD_CLASS_VISITS.get(), 3);
        assert!(declarations.layouts.borrow().fields.is_empty());
    }

    #[test]
    fn construction_lookups_index_only_the_requested_variant_prefix() {
        let class = TypeId::local("Wide");
        for width in [2, 8, 32] {
            let mut hir = HirResolution::default();
            hir.enums.insert(
                class,
                HirEnumInfo {
                    type_params: Default::default(),
                    variants: (0..width)
                        .map(|i| HirEnumVariant {
                            name: format!("V{i}"),
                            tag: i as i64,
                            payloads: vec![],
                        })
                        .collect(),
                },
            );
            let declarations = LoweringDeclarations::new(&hir);
            VARIANTS_INDEXED.set(0);
            for _ in 0..32 {
                assert_eq!(
                    declarations.enum_variant(&class, "V0", None).unwrap().1.tag,
                    0
                );
            }
            assert_eq!(VARIANTS_INDEXED.get(), 1);
            assert_eq!(declarations.variants.borrow()[&class].by_name.len(), 1);
            let mut single = HashMap::new();
            single.insert("V0", ());
            assert_eq!(
                declarations.variants.borrow()[&class].by_name.capacity(),
                single.capacity()
            );
            assert!(declarations.enum_is_payload_free(&class));
            assert_eq!(
                declarations
                    .enum_variant(&class, "V1", Some(0))
                    .unwrap()
                    .1
                    .tag,
                1
            );
            assert_eq!(VARIANTS_INDEXED.get(), 2);
            assert!(declarations.enum_variant(&class, "missing", None).is_none());
            assert_eq!(VARIANTS_INDEXED.get(), width);
        }
    }

    #[test]
    fn construction_lookups_preserve_name_and_arity_selection() {
        let mut hir = HirResolution::default();
        let class = TypeId::local("Repeated");
        hir.enums.insert(
            class,
            HirEnumInfo {
                type_params: Default::default(),
                variants: [0, 1, 1]
                    .into_iter()
                    .enumerate()
                    .map(|(tag, arity)| HirEnumVariant {
                        name: "One".into(),
                        tag: tag as i64,
                        payloads: vec![Type::Bool; arity],
                    })
                    .collect(),
            },
        );
        let declarations = LoweringDeclarations::new(&hir);
        VARIANTS_INDEXED.set(0);
        assert!(!declarations.enum_is_payload_free(&class));
        assert_eq!(VARIANTS_INDEXED.get(), 0);
        for (arity, tag) in [
            (None, Some(0)),
            (Some(0), Some(0)),
            (Some(1), Some(1)),
            (Some(2), None),
        ] {
            assert_eq!(
                declarations
                    .enum_variant(&class, "One", arity)
                    .map(|(_, variant)| variant.tag),
                tag
            );
        }
        assert_eq!(VARIANTS_INDEXED.get(), 3);
    }
}
