use cranelift_codegen::ir::{InstBuilder, MemFlagsData, condcodes::IntCC};
use cranelift_module::Module;

use super::*;

#[cfg(test)]
thread_local! {
    // Candidate queries and defining-class visits, isolated per compiler thread.
    static DISPATCH_WORK: std::cell::Cell<[usize; 2]> = const { std::cell::Cell::new([0; 2]) };
    // Reverse edges built and visited, independent of class-name aliases.
    static DESCENDANT_WORK: std::cell::Cell<[usize; 2]> = const { std::cell::Cell::new([0; 2]) };
}

/// Snapshot-local memoization: scoped metadata is immutable for the cache lifetime. Keep source spellings as keys/results so scoped
/// aliases retain the exact symbol lookup semantics of the uncached walk.
#[derive(Default)]
pub(super) struct DefiningClassCache {
    methods: HashMap<String, HashMap<String, Option<std::rc::Rc<str>>>>,
}

impl DefiningClassCache {
    pub(super) fn resolve(
        &mut self,
        class_name: &str,
        method_name: &str,
        mut visit: impl FnMut(&str) -> (bool, Option<String>),
    ) -> Option<String> {
        // Borrowed lookups keep cache hits allocation-free apart from returning
        // the owned result expected by call planning.
        if let Some(result) = self
            .methods
            .get(method_name)
            .and_then(|m| m.get(class_name))
        {
            return result.as_deref().map(str::to_owned);
        }
        let classes = self.methods.entry(method_name.to_owned()).or_default();
        let mut path = Vec::new();
        let mut search = Some(class_name.to_owned());
        let result = loop {
            let Some(name) = search else { break None };
            if let Some(result) = classes.get(&name) {
                break result.clone();
            }
            #[cfg(test)]
            DISPATCH_WORK.with(|work| {
                let [queries, visits] = work.get();
                work.set([queries, visits + 1]);
            });
            // A provisional miss also terminates malformed inheritance cycles.
            // No recursion/reentrant lookup occurs while visiting metadata.
            classes.insert(name.clone(), None);
            let (defines, parent) = visit(&name);
            path.push(name.clone());
            if defines {
                break Some(std::rc::Rc::<str>::from(name));
            }
            search = parent;
        };
        // Cache every suffix, not just the requested leaf: querying all nodes
        // in a deep chain then visits each class once per method per snapshot.
        for name in path {
            *classes.get_mut(&name).expect("visited class") = result.clone();
        }
        result.as_deref().map(str::to_owned)
    }
}

/// A scope or declaration change invalidates all three dependent analyses.
#[derive(Default)]
pub(super) struct DispatchCache {
    pub(super) defining: DefiningClassCache,
    plans: HashMap<String, HashMap<String, std::rc::Rc<VirtualCallPlan>>>,
    hierarchy: Option<std::rc::Rc<DispatchHierarchy>>,
    fallback_names: Option<std::rc::Rc<[TypeId]>>,
    fallback: HashMap<String, (DispatchSummary, Option<String>)>,
}

/// The questions code generation needs form a constant-size summary, not a
/// set of every target. Combining child summaries avoids quadratic storage and
/// traversal when calls use every receiver in a deep override chain.
#[derive(Clone, Copy, Default)]
struct DispatchSummary {
    first: Option<FuncId>,
    multiple: bool,
    may_panic: bool,
}

impl DispatchSummary {
    fn merge(&mut self, other: Self) {
        self.multiple |=
            other.multiple || matches!((self.first, other.first), (Some(a), Some(b)) if a != b);
        self.first = self.first.or(other.first);
        self.may_panic |= other.may_panic;
    }
}

struct DispatchHierarchy {
    children: HashMap<i64, Vec<i64>>,
    names: HashMap<i64, Vec<TypeId>>,
    summaries: std::cell::RefCell<HashMap<String, HashMap<i64, DispatchSummary>>>,
}

impl DispatchHierarchy {
    /// The `extends` graph of every class with a runtime `type_id`, projected
    /// into id space (willow-au5k): `classes` lists each class NAME with its
    /// id and `base_id_of` answers the id of a name's direct base. Every name
    /// for one class shares one id, so the projection collapses aliases onto
    /// exactly the relation the emitted dispatch chain tests. An edge to a
    /// class without an id contributes nothing: such a class is not a
    /// dispatch candidate in the first place.
    fn new(
        classes: impl IntoIterator<Item = (TypeId, i64)>,
        base_id_of: impl Fn(&TypeId) -> Option<i64>,
    ) -> Self {
        let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut names: HashMap<i64, Vec<TypeId>> = HashMap::new();
        let mut edges = HashSet::new();
        for (name, id) in classes {
            names.entry(id).or_default().push(name);
            // Two spellings of one class name one edge.
            if let Some(base) = base_id_of(&name)
                && edges.insert((id, base))
            {
                children.entry(base).or_default().push(id);
                #[cfg(test)]
                DESCENDANT_WORK.with(|w| {
                    let [b, v] = w.get();
                    w.set([b + 1, v]);
                });
            }
        }
        Self {
            children,
            names,
            summaries: Default::default(),
        }
    }

    fn summary(
        &self,
        receiver: i64,
        method: &str,
        mut own: impl FnMut(&str) -> DispatchSummary,
    ) -> DispatchSummary {
        let mut summaries = self.summaries.borrow_mut();
        if let Some(summary) = summaries.get(method).and_then(|all| all.get(&receiver)) {
            return *summary;
        }
        let summaries = summaries.entry(method.to_owned()).or_default();
        let mut pending = vec![(receiver, false)];
        let mut visiting = HashSet::new();
        while let Some((id, finish)) = pending.pop() {
            if summaries.contains_key(&id) {
                continue;
            }
            if !finish {
                assert!(
                    visiting.insert(id),
                    "compiler invariant violated: cyclic class hierarchy"
                );
                pending.push((id, true));
                if let Some(children) = self.children.get(&id) {
                    for &child in children {
                        #[cfg(test)]
                        DESCENDANT_WORK.with(|w| {
                            let [b, v] = w.get();
                            w.set([b, v + 1]);
                        });
                        pending.push((child, false));
                    }
                }
                continue;
            }
            let mut summary = DispatchSummary::default();
            if let Some(names) = self.names.get(&id) {
                for name in names {
                    summary.merge(own(&name.to_string()));
                }
            }
            if let Some(children) = self.children.get(&id) {
                for child in children {
                    summary.merge(summaries[child]);
                }
            }
            summaries.insert(id, summary);
            visiting.remove(&id);
        }
        summaries[&receiver]
    }
}

/// How one class-method call site must be emitted (willow-fm7t).
///
/// Produced by [`FuncGen::plan_virtual_call`] for LIR emission. The plan fixes
/// whether the call is virtual, its slot, and the implementation ABI.
pub(super) struct VirtualCallPlan {
    /// The nearest class in the receiver's ancestry that defines the method.
    /// Its signature describes every target, since an `override` may not change
    /// one.
    pub(super) static_class: String,
    /// The mangled symbol of `static_class`'s implementation: the direct
    /// callee, and the source of the return type, parameter modes and debug
    /// metadata for both call shapes.
    pub(super) mangled: String,
    /// Whether any reachable implementation may panic, reduced once per plan.
    pub(super) may_panic: bool,
    /// `Some(slot)` when the call must go through the descriptor; `None` when
    /// exactly one implementation exists and the call is direct.
    pub(super) virtual_slot: Option<usize>,
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Defense-in-depth check for the two raw pointers used by interface
    /// dispatch: the outer box and its concrete-object word. Safe Willow code
    /// cannot construct either invalid value, but checking this ABI boundary
    /// keeps a corrupt/test-only box from becoming an unchecked native load.
    pub(super) fn emit_interface_dispatch_nil_check(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        span: crate::diagnostics::Span,
        context: &str,
    ) {
        let zero = self
            .builder
            .ins()
            .iconst(reference_type(self.module.target_config()), 0);
        let is_nil = self.builder.ins().icmp(IntCC::Equal, ptr, zero);

        let nil_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_nil, nil_block, &[], ok_block, &[]);

        self.builder.switch_to_block(nil_block);
        self.builder.seal_block(nil_block);

        let source_file = self.source_file.to_string();
        let context_owned = context.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let ctx_ptr = self.emit_string_literal(&context_owned);
        let line_val = self.builder.ins().iconst(types::I32, span.line as i64);
        let col_val = self.builder.ins().iconst(types::I32, span.col as i64);

        self.emit_void_runtime_call("willow_nil_deref", &[file_ptr, line_val, col_val, ctx_ptr]);
        // The runtime helper raises and returns a neutral continuation. Reaching
        // this trap means its panic contract was violated.
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
    }

    /// Box a concrete object with an already-loaded vtable pointer (no vtable-id
    /// lookup). Used to re-box a `Self`-returning interface method result with the
    /// receiver's vtable (willow-1js.5). Layout matches `emit_interface_box`.
    pub(super) fn emit_box_with_vtable(
        &mut self,
        object: cranelift_codegen::ir::Value,
        vtable_ptr: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let object_root = self.emit_push_root(object);
        let box_ptr = self.emit_gc_alloc(GcLayoutMetadata::new(
            GcObjectKind::InterfaceBox,
            willow_abi::dispatch_layout::interface_bytes(
                reference_type(self.module.target_config()).bytes(),
            ) as i64,
            0,
            willow_abi::dispatch_layout::INTERFACE_GC_REF_MASK,
        ));
        let object = self.stack_load(reference_type(self.module.target_config()), object_root);
        self.emit_gc_heap_store_classified(
            box_ptr,
            0,
            object,
            true,
            GcStoreDestination::InterfaceObject,
        );
        self.emit_gc_heap_store_classified(
            box_ptr,
            willow_abi::dispatch_layout::vtable_offset(
                reference_type(self.module.target_config()).bytes(),
            ) as i32,
            vtable_ptr,
            false,
            GcStoreDestination::InterfaceObject,
        );
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        box_ptr
    }

    /// Widen by loading a path of shared static supertable pointers, then
    /// rebox the unchanged object. Static table pointers need no GC roots.
    pub(super) fn emit_interface_rewiden(
        &mut self,
        box_ptr: cranelift_codegen::ir::Value,
        path: &[usize],
    ) -> cranelift_codegen::ir::Value {
        let object = self.builder.ins().load(
            reference_type(self.module.target_config()),
            MemFlagsData::new(),
            box_ptr,
            0i32,
        );
        let vtable = self.builder.ins().load(
            reference_type(self.module.target_config()),
            MemFlagsData::new(),
            box_ptr,
            willow_abi::dispatch_layout::vtable_offset(
                reference_type(self.module.target_config()).bytes(),
            ) as i32,
        );
        let mut target_vtable = vtable;
        for &slot in path {
            target_vtable = self.builder.ins().load(
                reference_type(self.module.target_config()),
                MemFlagsData::new(),
                target_vtable,
                willow_abi::dispatch_layout::table_slot_offset(
                    slot as u32,
                    reference_type(self.module.target_config()).bytes(),
                ) as i32,
            );
        }
        self.emit_box_with_vtable(object, target_vtable)
    }

    fn own_dispatch_summary(&self, class: &str, method: &str) -> (DispatchSummary, Option<String>) {
        let Some(defining) = self.resolve_defining_class(class, method) else {
            return (DispatchSummary::default(), None);
        };
        let mangled = class_method_symbol_name(self.known_modules, &defining, method);
        (
            DispatchSummary {
                first: self.func_ids.get(&mangled).copied(),
                multiple: false,
                may_panic: self.user_function_may_panic(&mangled),
            },
            Some(defining),
        )
    }

    /// Runtime IDs collapse aliases, and FuncIds collapse alternate spellings
    /// of one implementation. Summaries retain polymorphism and panic effects
    /// without retaining or repeatedly sorting all reachable implementations.
    fn virtual_dispatch_summary(
        &self,
        class: &str,
        method: &str,
    ) -> (DispatchSummary, Option<String>) {
        #[cfg(test)]
        DISPATCH_WORK.with(|work| {
            let [queries, visits] = work.get();
            work.set([queries + 1, visits]);
        });
        let (mut summary, mut defining) = self.own_dispatch_summary(class, method);
        if let Some(receiver) = self.classes.type_id(class) {
            let hierarchy = {
                let mut cache = self.dispatch_cache.borrow_mut();
                cache
                    .hierarchy
                    .get_or_insert_with(|| {
                        // Over DECLARATION identities, not this unit's
                        // spellings: the graph is build-wide (willow-kd1v).
                        std::rc::Rc::new(DispatchHierarchy::new(
                            self.classes.runtime_classes(),
                            |name| {
                                self.classes
                                    .base_canonical(name)
                                    .and_then(|base| self.classes.type_id_canonical(&base))
                            },
                        ))
                    })
                    .clone()
            };
            summary.merge(hierarchy.summary(receiver, method, |name| {
                self.own_dispatch_summary(name, method).0
            }));
        } else {
            // Preserve the default-interface fallback's deterministic first
            // implementation, sharing its sorted names and result per method.
            let cached = self.dispatch_cache.borrow().fallback.get(method).cloned();
            let (all, first) = cached.unwrap_or_else(|| {
                let names = {
                    let mut cache = self.dispatch_cache.borrow_mut();
                    cache
                        .fallback_names
                        .get_or_insert_with(|| {
                            let mut names: Vec<_> = self
                                .classes
                                .runtime_classes()
                                .into_iter()
                                .map(|(name, _)| name)
                                .collect();
                            names.sort();
                            names.into()
                        })
                        .clone()
                };
                let mut all = DispatchSummary::default();
                let mut first = None;
                for name in names.iter() {
                    let (summary, defining) = self.own_dispatch_summary(&name.to_string(), method);
                    first = first.or(defining);
                    all.merge(summary);
                }
                self.dispatch_cache
                    .borrow_mut()
                    .fallback
                    .insert(method.to_owned(), (all, first.clone()));
                (all, first)
            });
            summary.merge(all);
            defining = defining.or(first);
        }
        (summary, defining)
    }

    /// How a call to `class_name::method_name` on a receiver of STATIC type
    /// `class_name` must be emitted (willow-fm7t).
    ///
    /// Resolve once per scoped (receiver, method) pair. Static initializers and
    /// emitted functions share plans until registration or resolution changes.
    pub(super) fn plan_virtual_call(
        &self,
        class_name: &str,
        method_name: &str,
    ) -> std::rc::Rc<VirtualCallPlan> {
        if let Some(plan) = self
            .dispatch_cache
            .borrow()
            .plans
            .get(class_name)
            .and_then(|m| m.get(method_name))
        {
            return plan.clone();
        }
        // A method with no slot is neither `open` nor an `override`. It can
        // neither be overridden nor override anything, so its callee is fixed
        // at compile time and a direct call is the whole answer.
        let vslot = self
            .classes
            .slots(class_name)
            .and_then(|slots| slots.slot_of(method_name));

        let (summary, defining) = if vslot.is_none() && self.classes.type_id(class_name).is_some() {
            self.own_dispatch_summary(class_name, method_name)
        } else {
            self.virtual_dispatch_summary(class_name, method_name)
        };
        let Some(static_class) = defining else {
            panic!(
                "compiler invariant violated: checked class method `{class_name}::{method_name}` has no dispatch target"
            );
        };
        let virtual_slot = match vslot {
            Some(slot) if summary.multiple => Some(slot),
            None if summary.multiple => panic!(
                "compiler invariant violated: method `{class_name}::{method_name}` has no virtual slot but multiple candidate implementations"
            ),
            _ => None,
        };
        let may_panic = summary.may_panic;
        let plan = std::rc::Rc::new(VirtualCallPlan {
            mangled: class_method_symbol_name(self.known_modules, &static_class, method_name),
            static_class,
            may_panic,
            virtual_slot,
        });
        self.dispatch_cache
            .borrow_mut()
            .plans
            .entry(class_name.to_owned())
            .or_default()
            .insert(method_name.to_owned(), plan.clone());
        plan
    }

    /// Load the function address in virtual slot `slot` of `self_ptr`'s class.
    ///
    /// Two dependent loads: word 0 of every object points at its class
    /// DESCRIPTOR, and slot `k` of that descriptor holds the k-th virtual
    /// method's address. The index is the one computed from the receiver's
    /// STATIC class, and it is valid for every class the receiver can actually
    /// be because a subclass's slot order EXTENDS its base's — an `override`
    /// rewrote that slot, an inherited method left the ancestor's address in
    /// it, and an unrelated class that merely shares the method NAME has its
    /// own descriptor and is never consulted.
    ///
    /// Emit this BEFORE evaluating arguments: an argument expression may itself
    /// allocate or dispatch, and reading the descriptor first keeps the two
    /// dependent loads next to each other.
    pub(super) fn emit_vtable_slot_load(
        &mut self,
        self_ptr: cranelift_codegen::ir::Value,
        slot: usize,
    ) -> cranelift_codegen::ir::Value {
        let ptr_ty = reference_type(self.module.target_config());
        let descriptor = self
            .builder
            .ins()
            .load(ptr_ty, MemFlagsData::new(), self_ptr, 0i32);
        let offset =
            willow_abi::dispatch_layout::class_slot_offset(slot as u32, ptr_ty.bytes()) as i32;
        self.builder
            .ins()
            .load(ptr_ty, MemFlagsData::new(), descriptor, offset)
    }

    /// The nearest class in `class_name`'s own ancestry — itself first — that
    /// defines `method_name`, so a subclass that INHERITS a method resolves to
    /// the implementation it actually inherits (willow-ftk).
    fn resolve_defining_class(&self, class_name: &str, method_name: &str) -> Option<String> {
        self.dispatch_cache
            .borrow_mut()
            .defining
            .resolve(class_name, method_name, |name| {
                let mangled = class_method_symbol_name(self.known_modules, name, method_name);
                let defines = self.func_ids.contains_key(&mangled);
                let parent = if defines {
                    None
                } else {
                    self.classes.base(name).map(|base| base.to_string())
                };
                (defines, parent)
            })
    }
}

/// Element-kind tag for the collection debug-display runtime calls
/// (willow-vwn6). Must match the runtime's `ELEM_KIND_*` constants.
pub(super) fn collection_elem_kind(ty: &Type) -> Option<i64> {
    match ty {
        Type::I64 => Some(0),
        Type::F64 => Some(1),
        Type::Bool => Some(2),
        Type::String => Some(3),
        _ => None,
    }
}

/// Whether a receiver whose static class has `ancestor_id` can hold an object
/// of `class` — the dispatch-chain filter's question — by walking `class`'s
/// `extends` chain through `base_of` and comparing runtime `type_id`s from
/// `id_of`.
///
/// The walk runs over whatever node the caller's tables are keyed by (names
/// in the LIR walker, ids in the perspectives below) but COMPARES ids, so a
/// class reached under an alias still meets its canonical spelling: every
/// name for one class shares one `type_id` (willow-au5k). The relation is
/// DIRECTED: a base class is not a candidate for a receiver typed as one of
/// its subclasses. The `seen` set makes a malformed `extends` cycle terminate
/// instead of hanging the compiler — a cycle is a checker error, and codegen
/// must not be the place where it turns into a hang. Cost is one step per
/// ancestor, independent of the number of classes in the program.
pub(super) fn is_self_or_descendant<N: Copy + Eq + std::hash::Hash>(
    class: N,
    ancestor_id: i64,
    id_of: impl Fn(N) -> Option<i64>,
    base_of: impl Fn(N) -> Option<N>,
) -> bool {
    let mut seen = HashSet::new();
    let mut current = Some(class);
    while let Some(node) = current {
        if id_of(node) == Some(ancestor_id) {
            return true;
        }
        if !seen.insert(node) {
            return false;
        }
        current = base_of(node);
    }
    false
}

/// [`is_self_or_descendant`] over an `extends` graph already in id space.
#[cfg(test)]
fn reaches(base_of: &HashMap<i64, i64>, class_id: i64, ancestor_id: i64) -> bool {
    is_self_or_descendant(class_id, ancestor_id, Some, |id| base_of.get(&id).copied())
}

/// Collect the receiver subtree once instead of walking every class's ancestry.
/// Reverse edges cost O(E) to build; the iterative walk costs O(Vr + Er) for
/// reachable vertices/edges. IDs preserve alias identity, and insertion before
/// enqueueing ensures even malformed cycles visit each vertex at most once.
#[cfg(test)]
fn descendant_ids(base_of: &HashMap<i64, i64>, ancestor_id: i64) -> HashSet<i64> {
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    for (&child, &base) in base_of {
        children.entry(base).or_default().push(child);
        #[cfg(test)]
        DESCENDANT_WORK.with(|work| {
            let [built, visited] = work.get();
            work.set([built + 1, visited]);
        });
    }
    let mut reachable = HashSet::from([ancestor_id]);
    let mut pending = vec![ancestor_id];
    while let Some(parent) = pending.pop() {
        if let Some(children) = children.get(&parent) {
            for &child in children {
                #[cfg(test)]
                DESCENDANT_WORK.with(|work| {
                    let [built, visited] = work.get();
                    work.set([built, visited + 1]);
                });
                if reachable.insert(child) {
                    pending.push(child);
                }
            }
        }
    }
    reachable
}

#[cfg(test)]
mod tests {
    use object::{Object, ObjectSection, ObjectSymbol, RelocationTarget};

    use super::*;

    /// The ancestry perspectives below run in `type_id` space, because that is
    /// what the emitted dispatch chain compares and what collapses a class's
    /// aliases onto one identity.
    ///
    /// ```text
    /// BASE(1) <- MIDDLE(2) <- LEAF(3)      UNRELATED(4)
    /// ```
    const BASE: i64 = 1;
    const MIDDLE: i64 = 2;
    const LEAF: i64 = 3;
    const UNRELATED: i64 = 4;

    fn hierarchy() -> HashMap<i64, i64> {
        HashMap::from([(MIDDLE, BASE), (LEAF, MIDDLE)])
    }

    #[test]
    fn descendant_walk_matches_ancestry_for_all_small_graphs() {
        // Each node either has no parent or points to any node, covering
        // disconnected trees, self edges, and cycles with incoming branches.
        for encoding in 0..5usize.pow(4) {
            let mut digits = encoding;
            let mut bases = HashMap::new();
            for child in 0..4i64 {
                let parent = digits % 5;
                digits /= 5;
                if parent < 4 {
                    bases.insert(child, parent as i64);
                }
            }
            for receiver in 0..=4 {
                let reachable = descendant_ids(&bases, receiver);
                for candidate in 0..=4 {
                    assert_eq!(
                        reachable.contains(&candidate),
                        reaches(&bases, candidate, receiver),
                        "graph={bases:?} receiver={receiver} candidate={candidate}"
                    );
                }
            }
        }
    }

    #[test]
    fn descendant_walk_has_linear_edge_counts() {
        for shape in ["chain", "fanout"] {
            for edges in [1, 32, 256, 16384] {
                let bases: HashMap<i64, i64> = (1..=edges)
                    .map(|child| (child, if shape == "chain" { child - 1 } else { 0 }))
                    .collect();
                for receiver in [0, edges, edges + 1] {
                    DESCENDANT_WORK.with(|work| work.set([0; 2]));
                    let reachable = descendant_ids(&bases, receiver);
                    let visited = if receiver == 0 { edges as usize } else { 0 };
                    let work = DESCENDANT_WORK.with(|work| work.get());
                    assert_eq!(work, [edges as usize, visited]);
                    assert_eq!(reachable.len(), visited + 1);
                    eprintln!(
                        "descendant-walk shape={shape} edges={edges} receiver={receiver} built={} visited={}",
                        work[0], work[1]
                    );
                }
            }
        }
    }

    #[test]
    fn virtual_descendant_work_scales_with_edges_and_calls() {
        for shape in ["chain", "fanout"] {
            for edges in [1, 8, 32] {
                for calls in [1, 4, 16] {
                    for functions in [1, 4] {
                        let mut source = String::from(
                            "open class C0 { pub open fn value(self) -> i64 { return 0; } }\n",
                        );
                        for child in 1..=edges {
                            let parent = if shape == "chain" { child - 1 } else { 0 };
                            source.push_str(&format!(
                                "open class C{child} extends C{parent} {{ pub open override fn value(self) -> i64 {{ return {child}; }} }}\n"
                            ));
                        }
                        for f in 0..functions {
                            source.push_str(&format!("fn probe{f}(value: C0) -> i64 {{\n"));
                            for _ in 1..calls {
                                source.push_str("value.value();\n");
                            }
                            source.push_str("return value.value(); }\n");
                        }
                        source.push_str("fn main() {}\n");
                        DESCENDANT_WORK.with(|work| work.set([0; 2]));
                        DISPATCH_WORK.with(|work| work.set([0; 2]));
                        compile_dispatch_fixture(&source);
                        let work = DESCENDANT_WORK.with(|work| work.get());
                        assert_eq!(work, [edges; 2], "{shape}, {edges}, {calls}, {functions}");
                        assert_eq!(DISPATCH_WORK.with(|w| w.get()), [1, 2 * (edges + 1)]);
                        eprintln!(
                            "virtual-descendants shape={shape} edges={edges} calls={calls} functions={functions} built={} visited={} candidate_queries=1 defining_visits={}",
                            work[0],
                            work[1],
                            2 * (edges + 1)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn subtree_summaries_share_work_across_every_receiver_and_method() {
        for shape in ["chain", "fanout"] {
            for count in [1, 32, 256, 4096] {
                let mut bases = TypeMap::default();
                let mut ids = TypeMap::default();
                for id in 0..count {
                    ids.insert(TypeId::local(format!("C{id}")), id as i64);
                    if id > 0 {
                        let base = if shape == "chain" { id - 1 } else { 0 };
                        bases.insert(
                            TypeId::local(format!("C{id}")),
                            TypeId::local(format!("C{base}")),
                        );
                    }
                }
                DESCENDANT_WORK.with(|w| w.set([0; 2]));
                let hierarchy =
                    DispatchHierarchy::new(ids.iter().map(|(name, id)| (*name, *id)), |name| {
                        bases
                            .get_canonical_id(name)
                            .and_then(|base| ids.get_canonical_id(base).copied())
                    });
                let mut visits = 0;
                for method in ["fixed", "override"] {
                    for _ in 0..4 {
                        for receiver in (0..count).rev() {
                            let summary = hierarchy.summary(receiver as i64, method, |name| {
                                visits += 1;
                                let id: u32 = name[1..].parse().unwrap();
                                DispatchSummary {
                                    first: Some(FuncId::from_u32(if method == "fixed" {
                                        0
                                    } else {
                                        id
                                    })),
                                    multiple: false,
                                    may_panic: id == (count - 1) as u32,
                                }
                            });
                            let has_descendant = if shape == "chain" {
                                receiver + 1 < count
                            } else {
                                receiver == 0 && count > 1
                            };
                            assert_eq!(summary.multiple, method == "override" && has_descendant);
                            let reaches_last =
                                shape == "chain" || receiver == 0 || receiver == count - 1;
                            assert_eq!(summary.may_panic, reaches_last);
                        }
                    }
                }
                assert_eq!(visits, 2 * count);
                assert_eq!(
                    DESCENDANT_WORK.with(|w| w.get()),
                    [count - 1, 2 * (count - 1)]
                );
                assert_eq!(
                    hierarchy
                        .summaries
                        .borrow()
                        .values()
                        .map(HashMap::len)
                        .sum::<usize>(),
                    2 * count
                );
                println!(
                    "summary shape={shape} nodes={count} methods=2 repeats=4 node_visits={visits} edges_built={} edges_visited={} entries={}",
                    count - 1,
                    2 * (count - 1),
                    2 * count
                );
            }
        }
    }

    #[test]
    fn dispatch_descriptors_remain_one_slot_per_override_class() {
        for count in [1, 8, 32] {
            let mut source =
                String::from("open class C0 { pub open fn value(self) -> i64 { return 0; } }\n");
            for child in 1..count {
                source.push_str(&format!("open class C{child} extends C{} {{ pub open override fn value(self) -> i64 {{ return {child}; }} }}\n", child - 1));
            }
            source.push_str("fn main() {}\n");
            let bytes = compile_dispatch_fixture(&source);
            let object = object::File::parse(&*bytes).unwrap();
            let expected = 8 + if object.is_64() { 8 } else { 4 };
            let mut measured = 0;
            for id in 0..count {
                let name = class_descriptor_symbol(&format!("C{id}"));
                let symbol = object
                    .symbol_by_name(&name)
                    .or_else(|| object.symbol_by_name(&format!("_{name}")))
                    .expect("descriptor symbol");
                // ELF gives exact data-symbol extents. COFF/Mach-O may report
                // zero; layout formula is covered by ABI tests on those hosts.
                if symbol.size() != 0 {
                    assert_eq!(symbol.size(), expected);
                    measured += symbol.size();
                }
            }
            println!(
                "descriptor classes={count} bytes_per_class={expected} measured_bytes={measured}"
            );
        }
    }

    #[test]
    fn defining_cache_follows_frozen_bases_and_function_alias_changes() {
        fn declare(cg: &mut Codegen, source: &str) {
            let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
            let (program, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(errors.is_empty());
            for item in program.items {
                if let Item::Class(class) = item {
                    cg.register_class_layout(&class).unwrap();
                    cg.declare_class_methods(&class).unwrap();
                }
            }
        }
        let mut cg = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
        declare(
            &mut cg,
            "open class A { pub fn value(self) -> i64 { return 1; } } open class X { pub fn value(self) -> i64 { return 2; } } class B extends A {}",
        );
        let a = cg.resolve_class_method_func_id("A", "value").unwrap();
        let x = cg.resolve_class_method_func_id("X", "value").unwrap();
        assert_ne!(a, x);
        for _ in 0..4 {
            assert_eq!(cg.resolve_class_method_func_id("B", "value"), Some(a));
        }
        // A declaration identity's base is frozen at its first registration
        // (willow-afb5.16): re-declaring `B` under another parent requests the
        // same edge again, so the inherited method stays `A::value`.
        declare(&mut cg, "class B extends X {}");
        assert_eq!(cg.resolve_class_method_func_id("B", "value"), Some(a));
        assert_eq!(cg.resolve_class_method_func_id("Alias", "value"), None);
        let alias = cg.class_method_symbol("Alias", "value");
        let a_name = cg.class_method_symbol("A", "value");
        let x_name = cg.class_method_symbol("X", "value");
        cg.alias_function_symbol(&alias, &a_name);
        assert_eq!(cg.resolve_class_method_func_id("Alias", "value"), Some(a));
        cg.alias_function_symbol(&alias, &x_name);
        assert_eq!(cg.resolve_class_method_func_id("Alias", "value"), Some(x));
        assert_eq!(cg.resolve_class_method_func_id("Late", "value"), None);
        let tokens =
            crate::lexer::Lexer::new("class Late { pub fn value(self) -> i64 { return 3; } }")
                .tokenize()
                .unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let Item::Class(class) = &program.items[0] else {
            panic!("class fixture")
        };
        // Declare methods after a cached miss without a layout registration
        // or scope change that would otherwise conceal missing invalidation.
        cg.declare_class_methods(class).unwrap();
        assert!(cg.resolve_class_method_func_id("Late", "value").is_some());
    }

    #[test]
    fn dispatch_01_a_class_is_a_candidate_for_its_own_type() {
        assert!(reaches(&hierarchy(), BASE, BASE));
        assert!(reaches(&hierarchy(), UNRELATED, UNRELATED));
    }

    #[test]
    fn dispatch_02_a_direct_subclass_is_a_candidate() {
        assert!(reaches(&hierarchy(), MIDDLE, BASE));
    }

    #[test]
    fn dispatch_03_a_transitive_subclass_is_a_candidate() {
        assert!(reaches(&hierarchy(), LEAF, BASE));
    }

    /// The whole point of the filter: a class that merely shares a method NAME
    /// with the receiver's class can never carry the receiver's type_id.
    #[test]
    fn dispatch_04_an_unrelated_class_is_not_a_candidate() {
        assert!(!reaches(&hierarchy(), UNRELATED, BASE));
        assert!(!reaches(&hierarchy(), BASE, UNRELATED));
    }

    /// Direction matters. A receiver typed `Leaf` holds a `Leaf`, never the
    /// `Base` it inherits from, so `Base` is not one of its candidates.
    #[test]
    fn dispatch_05_the_relation_is_directed() {
        assert!(reaches(&hierarchy(), LEAF, BASE));
        assert!(!reaches(&hierarchy(), BASE, LEAF));
    }

    /// A sibling branch is excluded even though both sides share an ancestor.
    #[test]
    fn dispatch_06_a_sibling_branch_is_not_a_candidate() {
        const OTHER: i64 = 5;
        let mut classes = hierarchy();
        classes.insert(OTHER, BASE);
        assert!(!reaches(&classes, OTHER, MIDDLE));
        assert!(!reaches(&classes, MIDDLE, OTHER));
        assert!(reaches(&classes, OTHER, BASE));
    }

    /// An `extends` cycle is a checker error. If one ever reaches codegen the
    /// walk must terminate — a hung compiler is a far worse failure than a
    /// wrong dispatch list.
    #[test]
    fn dispatch_07_an_extends_cycle_terminates() {
        let cyclic = HashMap::from([(1i64, 2i64), (2, 1)]);
        assert!(!reaches(&cyclic, 1, 99));
    }

    /// ...and still answers correctly when the target IS in the cycle.
    #[test]
    fn dispatch_08_a_cycle_still_reports_a_reachable_ancestor() {
        let cyclic = HashMap::from([(1i64, 2i64), (2, 1)]);
        assert!(reaches(&cyclic, 1, 2));
        assert!(reaches(&cyclic, 2, 1));
    }

    /// With no inheritance at all, identity is the only relation — which is
    /// what makes the filter collapse a flat program's chain to one entry.
    #[test]
    fn dispatch_09_without_inheritance_only_identity_holds() {
        let flat = HashMap::new();
        assert!(reaches(&flat, BASE, BASE));
        assert!(!reaches(&flat, UNRELATED, BASE));
    }

    /// A class the graph has never heard of resolves to nothing rather than to
    /// a default answer.
    #[test]
    fn dispatch_10_an_unknown_class_is_not_a_candidate() {
        assert!(!reaches(&hierarchy(), 404, BASE));
    }

    /// Depth is not bounded by anything in the language, so the walk must not
    /// be either.
    #[test]
    fn dispatch_11_a_deep_hierarchy_resolves_to_its_root() {
        let deep: HashMap<i64, i64> = (1..64).map(|level| (level, level - 1)).collect();
        assert!(reaches(&deep, 63, 0));
        assert!(reaches(&deep, 63, 62));
        assert!(!reaches(&deep, 0, 63));
    }

    /// The reason the graph is projected into id space at all: a directly
    /// imported class is registered under BOTH its canonical name and the local
    /// alias, and the two entries can record their base under different
    /// spellings. Over names, `zoo::Dog extends zoo::Animal` and a receiver
    /// typed `Animal` (the alias) never meet, the base looks like a leaf, and
    /// the subclass is filtered out of its own chain.
    #[test]
    fn dispatch_12_aliased_import_names_collapse_onto_one_id() {
        let type_ids = TypeMap::from([
            ("zoo::Animal".to_string(), 1i64),
            ("Animal".to_string(), 1),
            ("zoo::Dog".to_string(), 2),
            ("Dog".to_string(), 2),
        ]);
        let class_base = TypeMap::from([
            (
                "zoo::Dog".to_string(),
                TypeId::from_source_name("zoo::Animal"),
            ),
            ("Dog".to_string(), TypeId::from_source_name("zoo::Animal")),
        ]);

        let id_of = |name: TypeId| type_ids.get_canonical_id(&name).copied();
        let base_of = |name: TypeId| class_base.get_canonical_id(&name).copied();
        // The name walk meets the receiver's alias through the canonical id.
        assert!(is_self_or_descendant(
            TypeId::from_source_name("Dog"),
            type_ids["Animal"],
            id_of,
            base_of
        ));
        assert!(is_self_or_descendant(
            TypeId::from_source_name("zoo::Dog"),
            type_ids["Animal"],
            id_of,
            base_of
        ));
        assert!(!is_self_or_descendant(
            TypeId::from_source_name("Animal"),
            type_ids["Dog"],
            id_of,
            base_of
        ));
        // Projected into id space, the aliases collapse onto one edge.
        let hierarchy =
            DispatchHierarchy::new(type_ids.iter().map(|(name, id)| (*name, *id)), |name| {
                base_of(*name).and_then(id_of)
            });
        assert_eq!(hierarchy.children, HashMap::from([(1i64, vec![2i64])]));
        let base_ids: HashMap<i64, i64> = hierarchy
            .children
            .iter()
            .flat_map(|(base, children)| children.iter().map(move |child| (*child, *base)))
            .collect();
        assert_eq!(base_ids, HashMap::from([(2i64, 1i64)]));
        assert_eq!(
            descendant_ids(&base_ids, type_ids["Animal"]),
            HashSet::from([1, 2])
        );
    }

    /// An edge naming a class with no runtime id contributes nothing instead of
    /// a bogus relation.
    #[test]
    fn dispatch_12b_edges_without_ids_are_dropped() {
        let type_ids = TypeMap::from([("Known".to_string(), 1i64)]);
        let class_base = TypeMap::from([
            ("Known".to_string(), TypeId::from_source_name("Vanished")),
            ("Vanished".to_string(), TypeId::from_source_name("Known")),
        ]);
        let hierarchy =
            DispatchHierarchy::new(type_ids.iter().map(|(name, id)| (*name, *id)), |name| {
                class_base
                    .get_canonical_id(name)
                    .and_then(|base| type_ids.get_canonical_id(base).copied())
            });
        assert!(hierarchy.children.is_empty());
        assert!(!is_self_or_descendant(
            TypeId::from_source_name("Known"),
            2,
            |name| type_ids.get_canonical_id(&name).copied(),
            |name| class_base.get_canonical_id(&name).copied()
        ));
    }

    const INVALID_BOX_FIXTURE_SOURCE: &str = r#"
interface FixtureReader { fn read(self) -> i64; }
class FixtureReaderImpl implements FixtureReader {
    pub fn read(self) -> i64 { return 1; }
}
class DirectReader {
    pub value: i64;
    pub fn read(self) -> i64 { return self.value; }
}
fn interface_probe(reader: FixtureReader) -> i64 {
    return reader.read();
}
fn direct_field_probe(reader: DirectReader) -> i64 {
    return reader.value;
}
fn direct_method_probe(reader: DirectReader) -> i64 {
    return reader.read();
}
fn main() {}
"#;

    /// Compile an interface call without constructing its receiver in Willow
    /// source. The exported `interface_probe` parameter is the backend fixture
    /// boundary: its two-word representation can have a valid outer box with a
    /// zero object word, without preserving a safe-language path that creates
    /// that invalid state (willow-glaj.8).
    fn compile_interface_probe() -> Vec<u8> {
        compile_dispatch_fixture(INVALID_BOX_FIXTURE_SOURCE)
    }

    fn compile_dispatch_fixture(source: &str) -> Vec<u8> {
        let tokens = crate::lexer::Lexer::new(source)
            .tokenize()
            .expect("fixture should lex");
        let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
        assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");

        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).expect("prelude should register");
        checker.check_program(&program);
        assert!(
            checker.errors.is_empty(),
            "type errors: {:?}",
            checker.errors
        );

        let mut codegen =
            Codegen::for_tests(&CompilerOptions::debug()).expect("codegen should initialize");
        for (name, info) in &checker.symbols.enums {
            codegen.register_enum_info(name.to_string(), info.to_semantic());
        }
        for (name, info) in &checker.symbols.interfaces {
            codegen
                .register_interface_info(
                    name.to_string(),
                    TypeId::from_source_name(&info.name),
                    || info.to_semantic(),
                )
                .unwrap();
        }
        codegen.register_expr_types(
            checker
                .expr_types
                .iter()
                .map(|(id, ty)| (*id, ty.into()))
                .collect(),
        );
        let tables = crate::ir::lower::CheckerTables::from_checker(&checker);
        let (hir, gaps) = crate::ir::lower::lower_program_with(&program, &tables);
        assert!(gaps.is_empty(), "fixture lowering gaps: {gaps:?}");
        codegen.register_lir_functions(crate::ir::lowered::lower_program(&hir));
        codegen
            .compile_program(&program, "interface_invalid_box_fixture.wi")
            .expect("fixture should compile");
        codegen.finish().expect("fixture object should finish")
    }

    #[test]
    fn fixed_dispatch_skips_candidate_queries_at_increasing_sizes() {
        for shape in ["chain", "fanout"] {
            for classes in [1, 8, 32] {
                for calls in [1, 4, 16] {
                    let mut source =
                        String::from("open class C0 { pub fn fixed(self) -> i64 { return 7; } }\n");
                    for i in 1..=classes {
                        let parent = if shape == "chain" { i - 1 } else { 0 };
                        source.push_str(&format!("open class C{i} extends C{parent} {{}}\n"));
                    }
                    source.push_str(&format!("fn probe(value: C{classes}) -> i64 {{\n"));
                    for _ in 1..calls {
                        source.push_str("value.fixed();\n");
                    }
                    source.push_str("return value.fixed(); }\n");
                    source.push_str("fn root_probe(value: C0) -> i64 {\n");
                    for _ in 1..calls {
                        source.push_str("value.fixed();\n");
                    }
                    source.push_str("return value.fixed(); } fn main() {}\n");
                    DISPATCH_WORK.with(|work| work.set([0; 2]));
                    compile_dispatch_fixture(&source);
                    let work = DISPATCH_WORK.with(|work| work.get());
                    let depth = if shape == "chain" { classes + 1 } else { 2 };
                    assert_eq!(
                        work,
                        [0, depth],
                        "{shape}, classes={classes}, calls={calls}"
                    );
                    eprintln!(
                        "fixed-dispatch shape={shape} classes={classes} calls={calls} candidate_queries={} defining_visits={}",
                        work[0], work[1]
                    );
                }
            }
        }
    }

    #[test]
    fn defining_cache_visits_each_class_once_per_method() {
        for shape in ["chain", "fanout"] {
            for classes in [1, 32, 256] {
                for methods in [1, 4] {
                    for repeats in [1, 4, 16] {
                        let mut cache = DefiningClassCache::default();
                        let mut visits = 0;
                        for _ in 0..repeats {
                            for method in 0..methods {
                                // Odd methods miss: negative suffixes must also
                                // be reused. Query leaves before their ancestors.
                                let expected = (method % 2 == 0).then(|| "C0".to_owned());
                                for class in (0..=classes).rev() {
                                    let result = cache.resolve(
                                        &format!("C{class}"),
                                        &format!("m{method}"),
                                        |name| {
                                            visits += 1;
                                            let i: usize = name[1..].parse().unwrap();
                                            let parent = (i > 0).then(|| {
                                                let base = if shape == "chain" { i - 1 } else { 0 };
                                                format!("C{base}")
                                            });
                                            (i == 0 && method % 2 == 0, parent)
                                        },
                                    );
                                    assert_eq!(result, expected);
                                }
                            }
                        }
                        assert_eq!(visits, (classes + 1) * methods);
                        assert_eq!(
                            cache.methods.values().map(HashMap::len).sum::<usize>(),
                            visits
                        );
                        eprintln!(
                            "defining-cache shape={shape} classes={classes} methods={methods} repeats={repeats} visits={visits} entries={visits}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn defining_cache_preserves_overrides_spellings_and_cycle_misses() {
        let bases = HashMap::from([
            ("Leaf", "Override"),
            ("Override", "Base"),
            ("Alias", "pkg::Base"),
            ("CycleA", "CycleB"),
            ("CycleB", "CycleA"),
        ]);
        let mut cache = DefiningClassCache::default();
        for _ in 0..3 {
            for (class, method, expected) in [
                ("Leaf", "value", Some("Override")),
                ("Override", "value", Some("Override")),
                ("Base", "value", Some("Base")),
                ("Alias", "value", Some("pkg::Base")),
                ("Leaf", "missing", None),
                ("CycleA", "missing", None),
                ("CycleB", "missing", None),
                ("CycleA", "value", Some("CycleB")),
                ("CycleB", "value", Some("CycleB")),
            ] {
                assert_eq!(
                    cache
                        .resolve(class, method, |name| {
                            (
                                method == "value"
                                    && matches!(name, "Override" | "Base" | "pkg::Base" | "CycleB"),
                                bases.get(name).map(|base| (*base).to_owned()),
                            )
                        })
                        .as_deref(),
                    expected
                );
            }
        }
    }

    #[test]
    fn virtual_dispatch_still_queries_candidates() {
        DISPATCH_WORK.with(|work| work.set([0; 2]));
        compile_dispatch_fixture(
            r#"
            open class Base { pub open fn value(self) -> i64 { return 1; } }
            class Derived extends Base { pub override fn value(self) -> i64 { return 2; } }
            fn probe(value: Base) -> i64 { return value.value(); }
            fn main() {}
        "#,
        );
        assert_eq!(DISPATCH_WORK.with(|work| work.get()[0]), 1);
    }

    fn nil_check_relocations_in_symbol(bytes: &[u8], symbol_name: &str) -> usize {
        let file = object::File::parse(bytes).expect("fixture object should parse");
        let matches_name = |name: &str, expected: &str| {
            name == expected
                || (file.format() == object::BinaryFormat::MachO
                    && name.strip_prefix('_') == Some(expected))
        };
        let probe = file
            .symbols()
            .find(|symbol| {
                symbol
                    .name()
                    .is_ok_and(|name| matches_name(name, symbol_name))
            })
            .unwrap_or_else(|| panic!("{symbol_name} symbol should exist"));
        let section = file
            .section_by_index(probe.section_index().expect("probe should have a section"))
            .expect("probe section should exist");
        // Relocation offsets are section-relative, including in Mach-O where
        // the section and its function symbols can have nonzero addresses.
        let start = probe.address() - section.address();
        // COFF and Mach-O function symbols commonly report a size of zero. In that case,
        // use the next symbol in the same section as the function boundary.
        let end = if probe.size() != 0 {
            start + probe.size()
        } else {
            file.symbols()
                .filter(|symbol| symbol.section_index() == probe.section_index())
                .map(|symbol| symbol.address() - section.address())
                .filter(|address| *address > start)
                .min()
                .unwrap_or(section.size())
        };
        section
            .relocations()
            .filter(|(offset, relocation)| {
                if *offset < start || *offset >= end {
                    return false;
                }
                // AArch64 Mach-O PIC loads use a PAGE21/PAGEOFF12 pair
                // for one helper address. Count the page relocation once.
                if file.architecture() == object::Architecture::Aarch64
                    && matches!(
                        relocation.flags(),
                        object::RelocationFlags::MachO { r_type, .. }
                            if r_type == object::macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12
                                || r_type == object::macho::ARM64_RELOC_PAGEOFF12
                    )
                {
                    return false;
                }
                let RelocationTarget::Symbol(index) = relocation.target() else {
                    return false;
                };
                file.symbol_by_index(index)
                    .ok()
                    .and_then(|symbol| symbol.name().ok())
                    .is_some_and(|name| matches_name(name, "willow_nil_deref"))
            })
            .count()
    }

    fn assert_invalid_object_word_guard() {
        let bytes = compile_interface_probe();
        // One check validates the outer box and the second validates word 0.
        // A regression to checking only the box therefore drops this to one.
        assert_eq!(
            nil_check_relocations_in_symbol(&bytes, "interface_probe"),
            2
        );
        // The nil helper receives the method name, so a fault from the second
        // check retains `read` as its call-site context.
        assert!(
            bytes
                .windows(b"read\0".len())
                .any(|window| window == b"read\0"),
            "fixture object lost the interface method context"
        );
    }

    #[test]
    fn interface_guard_01_invalid_object_word_keeps_method_context() {
        assert_invalid_object_word_guard();
    }

    fn assert_direct_class_access_has_no_nil_guard() {
        let bytes = compile_interface_probe();
        assert_eq!(
            nil_check_relocations_in_symbol(&bytes, "direct_field_probe"),
            0,
            "direct class field access retained an obsolete nullable guard"
        );
        assert_eq!(
            nil_check_relocations_in_symbol(&bytes, "direct_method_probe"),
            0,
            "direct class method dispatch retained an obsolete nullable guard"
        );
    }

    #[test]
    fn interface_guard_02_direct_class_access_has_no_nil_guard() {
        assert_direct_class_access_has_no_nil_guard();
    }
}
