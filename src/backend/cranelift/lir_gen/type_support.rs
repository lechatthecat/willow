//! Type representation and layout eligibility.

use super::*;

pub(super) fn scalar(ty: &Type) -> bool {
    matches!(ty, Type::I64 | Type::F64 | Type::Bool)
}

/// The builtin collection generics the walker emits, split out so eligibility
/// and emission agree on exactly which `Type::Generic`s are in the subset.
///
/// Recognition goes through [`builtin_types::resolve`] rather than a name
/// comparison so a user type that happens to be called `Map` cannot be mistaken
/// for the builtin one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum LirCollection {
    /// `FrozenArray<T>` — the immutable view of an `Array<T>`, backed by the
    /// same runtime array handle.
    FrozenArray,
    /// `Map<K, V>`.
    Map,
    /// `FrozenMap<K, V>` — the same runtime map object, without the writers.
    FrozenMap,
}

pub(super) fn lir_collection(ty: &Type) -> Option<(LirCollection, Vec<Type>)> {
    let resolved = builtin_types::resolve(ty)?;
    let kind = match resolved.id {
        B::FrozenArray => LirCollection::FrozenArray,
        B::Map => LirCollection::Map,
        B::FrozenMap => LirCollection::FrozenMap,
        _ => return None,
    };
    Some((kind, resolved.args.to_vec()))
}

/// `Range<i64>` — the only range shape either backend models: a two-word GC
/// object `[start, end]`, both `i64`, with no reference slots
///.
///
/// Recognised by name: `Range` is not a [`BuiltinTypeId`]. Eligibility and
/// field emission share this predicate to agree on the two-word layout.
///
/// [`BuiltinTypeId`]: crate::semantic::builtin_types::BuiltinTypeId
pub(super) fn range_i64(ty: &Type) -> bool {
    matches!(ty, Type::Generic(name, args) if name == &TypeId::local("Range") && args.as_slice() == [Type::I64])
}

/// The builtin namespace call `class::method` names, or `None` if it is not one.
///
/// `env`, `fs` and `net` are NAMESPACES, not classes: they have no layout, no
/// methods and no symbols of their own, so these become direct runtime calls.
/// One table serves eligibility and emission, so the walker
/// cannot admit a call it has no entry point for. A user module registered
/// under one of the names wins over the builtin, exactly as in
/// `emit_static_method_call`.
///
/// Only the runtime SYMBOL lives here. The signature comes from
/// [`stdlib_schema`], which is the same table the checker types these calls
/// from — so the walker cannot vet a call against a shape the checker never
/// agreed to. That is what lets `parallel::map` join them:
/// its shape — a frozen array and a function value in, a `Task` out — is the
/// schema's, and the function value needs no special handling because a Willow
/// lambda captures nothing and so IS an address.
///
/// `f64::to_string` and `f64::parse` join them because they ARE them — two
/// fixed-signature runtime calls spelled with a `::`. They
/// are not in the stdlib schema (no `import` reaches them), so their shapes
/// come from the same table [`static_call_return_type`] types them from, and
/// they are answered BEFORE the user-module gate because that is where
/// `emit_static_method_call` answers them: a module imported under the name
/// `f64` does not displace either one.
pub(super) fn namespace_builtin_call(
    known_modules: &ModuleSymbols,
    builtin_module_aliases: &HashMap<String, String>,
    class: &str,
    method: &str,
) -> Option<crate::semantic::intrinsics::NamespaceBuiltin> {
    if matches!(class, "f64" | "i64") {
        return crate::semantic::intrinsics::namespace_builtin(class, method);
    }
    if known_modules.contains_key(class) {
        return None;
    }
    let namespace = builtin_module_aliases
        .get(class)
        .map(String::as_str)
        .unwrap_or(class);
    crate::semantic::intrinsics::namespace_builtin(namespace, method)
}

/// A context-dependent empty map constructor. Eligibility admits it only as
/// a fresh value; emission uses the checked destination's type arguments to
/// initialize the runtime layout before the map can receive any entries.
fn empty_map_type(ty: &Type) -> bool {
    matches!(lir_collection(ty), Some((LirCollection::Map, args))
        if args.as_slice() == [Type::Void, Type::Void])
}

/// Whether `e` is literally `Map::new`, the only expression that may carry the
/// untyped [`empty_map_type`].
///
/// This is what keeps the type-level exemption honest. `supported_type` REJECTS
/// `Map<Void, Void>`, so it can never be a parameter, a `let`'s declared type or
/// a return type; the only way a value of that type can exist in an eligible
/// function is through this node, which is admitted here and nowhere else.
#[cfg(test)]
pub(super) fn is_fresh_empty_map(e: &HirExpr) -> bool {
    matches!(&e.kind, HirExprKind::StaticCall { class, method, args }
        if class == &TypeId::local("Map") && method == "new" && args.is_empty())
        && empty_map_type(&e.ty)
}

/// Whether `ty` can be a map key the walker emits.
///
/// Scalar keys arrive as words: `coerce_to_i64` widens bool and bitcasts f64.
/// The runtime uses the map's declared key kind to interpret each word. String
/// keys are copied and compared by content; other GC-managed types are rejected.
/// For f64, runtime validation rejects NaN and canonicalizes -0.0 to +0.0;
/// other finite and infinite values retain their IEEE 754 bits.
fn map_key_supported(ty: &Type) -> bool {
    matches!(ty, Type::String) || scalar(ty)
}

/// Whether a value of type `value` already HAS the representation of a slot
/// declared `target`, so moving it needs no conversion at all.
///
/// Two *different* named types never share a representation the walker may
/// assume: a class value in an interface slot must be boxed first. This is the
/// right test wherever the walker cannot insert a conversion — a node's own
/// type versus the type its emitter actually produces (a field read, a call
/// result, a ternary's branches). Store positions, where the walker *can* box,
/// use [`LirTypeCtx::storable`] instead.
pub(super) fn assignable_repr(target: &Type, value: &Type) -> bool {
    // This is deliberately local: it is not a general runtime-layout query.
    // Nominal types need metadata (fieldless enums are words), and generic
    // types include non-GC Future words and scalar ADT pairs. The arms
    // below must handle them before reaching this exhaustive fallback.
    fn gc_managed_for_assignable_catchall(ty: &Type) -> bool {
        match ty {
            Type::String | Type::FixedArray(_, _) | Type::Array(_) | Type::Closure(_, _) => true,
            Type::I64 | Type::F64 | Type::Bool | Type::Void | Type::Never | Type::Fn(_, _) => false,
            Type::Named(_) | Type::Generic(_, _) => {
                unreachable!("nominal type reached assignable representation catch-all")
            }
        }
    }
    match (target, value) {
        (Type::Named(a), Type::Named(b)) => a == b,
        (Type::Named(_), _) | (_, Type::Named(_)) => false,
        // Array handles also carry element semantics (`is_ref`), and interface
        // elements are boxed while class elements are raw object pointers.
        // Consequently two arrays are representation-compatible only when the
        // element types agree exactly. Empty `Array<Void>` literals do not reach
        // HIR today; supporting them later requires contextual element typing at
        // allocation time, not a blanket handle reinterpretation.
        (Type::Array(a) | Type::FixedArray(a, _), Type::Array(b) | Type::FixedArray(b, _)) => {
            a.same_array_storage_type(b)
        }
        // The collection generics carry their element semantics the same way,
        // and they all lower to one pointer-sized word — so without an exact
        // test the fallback below would call a `Map<String, i64>` and a
        // `FrozenArray<i64>` interchangeable.
        (Type::Generic(..), Type::Generic(..)) => target.same_array_storage_type(value),
        (Type::Generic(..), _) | (_, Type::Generic(..)) => false,
        // Both the calling convention and signature belong to the representation.
        (Type::Fn(..), Type::Fn(..)) | (Type::Closure(..), Type::Closure(..)) => {
            target.same_array_storage_type(value)
        }
        (Type::Fn(..) | Type::Closure(..), _) | (_, Type::Fn(..) | Type::Closure(..)) => false,
        _ => {
            clif_type(types::I64, target) == clif_type(types::I64, value)
                && gc_managed_for_assignable_catchall(target)
                    == gc_managed_for_assignable_catchall(value)
        }
    }
}

/// One interface method as dispatch sees it: the signature the indirect call is
/// built from. The receiver is implicit — the vtable's function pointers all
/// take the concrete object as their first argument.
///
/// Producing this at all is the existence proof eligibility needs: the lookup
/// resolves the method's vtable SLOT exactly as
/// [`FuncGen::emit_lir_interface_call`] does, and answers `None` when the
/// interface has no such slot.
pub(in crate::backend::cranelift) struct IfaceMethodSig {
    pub params: Vec<Type>,
    /// The declared passing mode of each parameter. A `&`/`&mut` parameter is a
    /// POINTER in the dispatch ABI, so eligibility pairs each mode with the
    /// shape of the argument in that position and admits the call only when
    /// they agree — the same rule [`LirTypeCtx::callable`] applies to direct
    /// calls. Nothing else can tell a
    /// pointer slot from a value slot here: the dispatch signature is built
    /// from this list, not from the callee.
    pub modes: Vec<ParamMode>,
    pub ret: Type,
}

/// One variant of an enum as ELIGIBILITY needs it: the name it is selected by,
/// and the declared payload types in declaration order, which are also the
/// payload SLOT order in the heap object. The runtime tag is deliberately
/// absent — emission reads it from [`FuncGen::enum_variant_tag`], so there is
/// no second copy to drift.
#[derive(Clone)]
pub(in crate::backend::cranelift) struct LirEnumVariant {
    pub name: String,
    pub payloads: Vec<Type>,
}

/// An enum declaration as eligibility sees it.
#[derive(Clone)]
pub(in crate::backend::cranelift) struct LirEnumDef {
    /// The enum's build-wide identity: the one name it answers to however this
    /// unit spells it. A unit that item-imports an enum registers
    /// the declaration under the local spelling as well — `Level`, or `Rank`
    /// under `import signal::Level as Rank;` — and every such entry carries the
    /// declaring module's `signal::Level` here, which is what makes two
    /// spellings comparable without going through their names.
    pub identity: TypeId,
    /// Declared type parameters, in order. A non-empty list means the enum is
    /// GENERIC, and [`LirTypeCtx::supported_enum`] refuses it: `payloads` then
    /// holds type-parameter placeholders rather than real types, and only a
    /// concrete `Type::Generic` scrutinee carries the arguments that would
    /// resolve them.
    pub type_params: Vec<TypeId>,
    pub variants: Vec<LirEnumVariant>,
}

impl LirEnumDef {
    pub(super) fn variant(&self, name: &str) -> Option<&LirEnumVariant> {
        self.variants.iter().find(|v| v.name == name)
    }
}

#[derive(Default)]
pub(super) struct SupportedTypes {
    pub(super) seen_types: HashSet<Type>,
    /// Generic definitions currently traversed, with their validated arity.
    pub(super) active_enums: HashMap<TypeId, usize>,
}

/// The program facts eligibility needs beyond the lowered IR itself: which
/// named types are classes the walker can lay out, which symbols exist, and
/// what those symbols' signatures are. Built from the compiler's registration
/// tables at the dispatch site in `compile_function_named`.
#[derive(Clone, Copy)]
pub(in crate::backend::cranelift) struct LirTypeCtx<'x> {
    /// Whether a symbol name is a declared/linkable function.
    pub known_fn: &'x dyn Fn(&str) -> bool,
    /// Resolved IR identities must never round-trip through display strings:
    /// display omits package identity, while emitted symbols retain it.
    pub known_fn_id: &'x dyn Fn(&FunctionId) -> bool,
    /// Field layouts, base edges and runtime `type_id`s per class NAME. A
    /// direct type import (`import zoo::Animal;`) aliases the imported class
    /// under its unqualified name, so both spellings answer with the canonical
    /// class's id — which is what makes class IDENTITY comparable across them.
    pub classes: &'x dyn ClassTables,
    /// Whether a name is registered as an interface (never a class here).
    pub is_interface: &'x dyn Fn(&TypeId) -> bool,
    /// The build-wide identity of an interface NAME, or `None` when the name is
    /// not an interface. An item import registers the interface under its local
    /// spelling too, carrying the declaring module's identity, so this is what
    /// makes two spellings of one interface comparable — the same role
    /// [`LirEnumDef::identity`] plays for enums.
    pub iface_identity: &'x dyn Fn(&TypeId) -> Option<TypeId>,
    /// Whether boxing `(class, interface)` resolves to a registered vtable —
    /// exactly what [`FuncGen::emit_interface_box`] will look up. A coercion it
    /// cannot build must not be admitted: the emitter's fallback is to pass the
    /// raw object through, which would put an unboxed class pointer in an
    /// interface slot.
    pub can_box: &'x dyn Fn(&TypeId, &TypeId) -> bool,
    /// The declaration of an enum NAME, or `None` when the name is not an enum.
    /// Read from the same `enum_infos` table [`FuncGen::enum_variant_tag`] and
    /// [`FuncGen::enum_is_gc_object_type`] answer from, so the tags and the
    /// representation eligibility vets are the ones emission uses
    ///. This is also the single source of "is this an enum?" —
    /// see [`LirTypeCtx::is_enum`] — so the two can never disagree.
    pub enum_def: &'x dyn Fn(&TypeId) -> Option<LirEnumDef>,
    /// The vtable slot and signature of `(interface, method)`, i.e. exactly what
    /// [`FuncGen::emit_lir_interface_call`] indexes and calls. `None` when the
    /// name is not an interface, or the interface does not declare that method
    /// — there is no slot to index, so a walker that admitted such a call would
    /// silently miscompile.
    pub iface_method: &'x dyn Fn(&Type, &str) -> Option<IfaceMethodSig>,
    /// The super-pointer slots to load, in order, to reach interface `target`'s
    /// vtable from interface `source`'s, or `None` when `target` is not a
    /// super-interface. An empty path is the interface itself and is
    /// representation-compatible; otherwise [`FuncGen::coerce_to_target`]
    /// follows the path and allocates a box holding the target's own shared
    /// table pointer.
    pub iface_widen_path: &'x dyn Fn(&TypeId, &TypeId) -> Option<Vec<usize>>,
    /// The declared type of the static property `(class, field)`, resolved
    /// through the class hierarchy exactly as
    /// [`FuncGen::emit_static_field_read`] resolves the storage it loads from —
    /// so an INHERITED static (`Widget::kind` declared on `Base`) is answered
    /// here iff the emitter will find its data slot. `None` when there is no
    /// such property.
    pub static_field: &'x dyn Fn(&str, &str) -> Option<Type>,
    pub fn_types: &'x FunctionMap<'x, Type>,
    pub func_param_modes: &'x FunctionMap<'x, Vec<ParamMode>>,
    pub known_modules: &'x ModuleSymbols<'x>,
    /// The module access names the file being vetted imports.
    /// `known_modules` is every module the build declared, so a bare class name
    /// is looked for in THESE first: an unrelated module that happens to
    /// declare the same class name must not make the name ambiguous.
    pub visible_modules: &'x HashSet<String>,
    /// Local alias -> canonical builtin schema module, for the file being
    /// compiled (`import std::fs as files;` records `files -> fs`). Declaration
    /// normalization folds these aliases into its program, but LIR lowering
    /// reads the raw frontend program, so a namespace call still
    /// carries whatever name the `import` spelled.
    pub builtin_module_aliases: &'x HashMap<String, String>,
    /// The `$lambda.N` symbol a lambda expression was lifted to, by the span of
    /// the lambda — `{module}.$lambda.N` for one lifted by a
    /// module's own declaration phase. `None` for a lambda the
    /// backend never declared, so the walker refuses rather than emitting the
    /// address of nothing.
    pub lambda_symbol: &'x dyn Fn(ExprId) -> Option<FunctionId>,
    /// The declared return type of the function being vetted. Unlike every
    /// other field this one is per-FUNCTION, and it is here because a `return`
    /// inside a `match` arm is checked deep inside `supported_expr`, where the
    /// enclosing [`LirFunction`] is out of reach.
    #[cfg(test)]
    pub return_type: &'x Type,
    /// The class whose body is being vetted, or `None` outside a class. The
    /// second per-FUNCTION field, set the same way `return_type` is, and it is
    /// what resolves `Self::` — `Self::twice(..)`, `Self::count`,
    /// `Self::count =..` — to the class the emitter will resolve it to
    ///. `None` leaves `Self` unresolved, every lookup on it
    /// misses and the function is refused (`E0800`), rather than the walker
    /// guessing.
    pub self_class: Option<&'x str>,
    /// The async functions compiled as cooperative LEAVES in this module.
    /// `await f(..)` on one of them is a direct constructor call plus a
    /// suspension, so it needs neither a `Task` value nor a `Task`-typed local
    ///.
    pub cooperative_leaves: &'x std::collections::HashSet<FunctionId>,
}

/// Normalise a variant's payload list for a use site where every payload
/// substituted away to `void`.
///
/// `Result<void, E>::Ok` is the case that forces this: the declared payload
/// is `T`, the instantiation makes it `void`, and the checker accepts the call
/// with ZERO arguments. The heap layout is derived from the arguments, with
/// `payload_types` indexed positionally. Dropping the list keeps validation,
/// the LIR emitter and [`FuncGen::emit_enum_variant_alloc`] describing one object.
///
/// A *mixed* list (`void` in one slot, a real type in another) is deliberately
/// left alone: the arity check downstream rejects the function rather than
/// silently renumbering payload slots.
pub(super) fn normalize_void_payloads(payloads: &mut Vec<Type>) {
    if !payloads.is_empty() && payloads.iter().all(|t| matches!(t, Type::Void)) {
        payloads.clear();
    }
}

/// The name the class tables are keyed on for `name`, or `None` when nothing
/// this compilation unit registered answers to it.
///
/// An entry program that says only `import shapes;` sees a module class by the
/// BARE name the module declared — `shapes::make(1, 2)` is typed `Point` —
/// while the tables the module unit contributed are keyed on the canonical
/// `shapes::Point`; only an item import (`import shapes::Point;`) copies them
/// under the short name (see `Codegen::register_item_import`). So a bare name
/// no table answers to is retried once per known module. Without this
/// the walker refused every entry function that so much as bound a module
/// class, and the fallback was silent.
///
/// A name the tables already carry is never re-resolved: an entry program that
/// declares its own `Point` means its own, whatever it imports.
///
/// Two DIFFERENT imported classes of the same bare name resolve to nothing,
/// keeping the body out of the subset rather than compiling it against the
/// wrong layout. Two spellings of the SAME class do not collide — one module
/// imported under two access names registers `c::Point` and `checks::Point`,
/// and they share a `type_id` because they are one runtime class, exactly as
/// `LirTypeCtx::class_widening` relies on.
///
/// `known_modules` is every module the whole build declared, so the retry runs
/// over the modules this unit can actually SEE first: a module
/// the file never imported must not make its class name ambiguous here, since
/// the only effect of that ambiguity is to cost an eligible body its lowering.
/// The all-modules scan is still what answers when the visible ones say
/// nothing, so no name that resolves today stops resolving — a class reached
/// through a module that only another module imports keeps working.
pub(super) fn resolve_class_key<Q: crate::backend::cranelift::type_index::TypeLookup + ?Sized>(
    classes: &dyn ClassTables,
    known_modules: &ModuleSymbols,
    visible_modules: &HashSet<String>,
    name: &Q,
) -> Option<TypeId> {
    let name = &name.type_id();
    if classes.is_class(name) {
        return Some(*name);
    }
    // One runtime class is one `type_id`, so two spellings that carry the same
    // id are one answer; a class with no id at all is never merged by name.
    let same_class =
        |a: &TypeId, b: &TypeId| match (classes.class_type_id(a), classes.class_type_id(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        };
    // `Err` is "these modules disagree", which is not the same answer as `None`
    // ("none of them has such a class") — only the first makes the name
    // genuinely ambiguous at this site.
    let scan = |visible_only: bool| -> Result<Option<TypeId>, ()> {
        let mut found: Option<TypeId> = None;
        for module in known_modules.keys() {
            if visible_only && !visible_modules.contains(module) {
                continue;
            }
            let qualified = (*name).in_namespace(module);
            if !classes.is_class(&qualified) {
                continue;
            }
            match &found {
                Some(prev) if !same_class(prev, &qualified) => return Err(()),
                Some(_) => {}
                None => found = Some(qualified),
            }
        }
        Ok(found)
    };
    match scan(true) {
        Err(()) => None,
        Ok(Some(key)) => Some(key),
        Ok(None) => scan(false).unwrap_or(None),
    }
}

#[willow_continuations::methods(
    same_enum,
    same_repr_inner,
    supported_class_inner,
    supported_enum_inner,
    supported_type_inner
)]
impl LirTypeCtx<'_> {
    /// `class` with `Self` resolved against the enclosing class body, exactly
    /// as [`FuncGen::static_call_class_name`] resolves it at emission time
    /// — so a `Self::` form is admitted under the very symbol
    /// the emitter will go on to resolve it to. Outside a class body, and in
    /// any other spelling, the name is returned unchanged.
    pub(super) fn resolved_class<'n>(&'n self, class: &'n str) -> &'n str {
        if class == "Self" {
            self.self_class.unwrap_or(class)
        } else {
            class
        }
    }

    /// Whether `name` is a declared enum. Answered from [`Self::enum_def`], so
    /// there is exactly one table deciding it.
    pub(super) fn is_enum<Q: crate::backend::cranelift::type_index::TypeLookup + ?Sized>(
        &self,
        name: &Q,
    ) -> bool {
        (self.enum_def)(&name.type_id()).is_some()
    }

    /// Whether `ty` is an enum whose values ARE the tag: every variant is
    /// payload-free, so [`FuncGen::enum_is_gc_object_type`] answers `false` and
    /// a value of it travels as a plain `i64` rather than a heap pointer. That
    /// is what makes `==` on two of them the integer comparison the emitter
    /// already produces, instead of the object-identity test a payload-carrying
    /// enum would need. A generic enum is excluded: its
    /// `payloads` hold type-parameter placeholders, not the real types.
    pub(super) fn tag_immediate_enum(&self, ty: &Type) -> bool {
        let Type::Named(name) = ty else {
            return false;
        };
        (self.enum_def)(name).is_some_and(|def| {
            def.type_params.is_empty() && def.variants.iter().all(|v| v.payloads.is_empty())
        })
    }

    /// The enum `ty` denotes, with every payload already instantiated *at this
    /// use site*.
    ///
    /// This is the single place the walker turns a type into enum structure, so
    /// a bare `Type::Named` enum and a `Type::Generic` instantiation — `Color`,
    /// `Option<i64>`, `Result<T, String>` — are read the same way. A type whose
    /// argument count does not match the declaration is not an instance of it
    /// at all and is refused: eligibility must never guess a payload.
    ///
    /// Substitution mirrors [`FuncGen::resolve_variant_payload_types`], which
    /// is what emission calls, so the payload types vetted here are the payload
    /// types stored.
    pub(super) fn enum_instance(&self, ty: &Type) -> Option<(TypeId, LirEnumDef)> {
        let (name, args): (&TypeId, &[Type]) = match ty {
            Type::Named(n) => (n, &[]),
            Type::Generic(n, a) => (n, a),
            _ => return None,
        };
        if (self.is_interface)(name) {
            return None;
        }
        let def = (self.enum_def)(name)?;
        if def.type_params.len() != args.len() {
            return None;
        }
        let map: HashMap<TypeId, Type> = def
            .type_params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        let variants = def
            .variants
            .iter()
            .map(|v| {
                let mut payloads: Vec<Type> = v
                    .payloads
                    .iter()
                    .map(|t| crate::semantic::symbols::substitute_type(t, &map))
                    .collect();
                normalize_void_payloads(&mut payloads);
                LirEnumVariant {
                    name: v.name.clone(),
                    payloads,
                }
            })
            .collect();
        Some((
            *name,
            LirEnumDef {
                identity: def.identity,
                type_params: Vec::new(),
                variants,
            },
        ))
    }

    /// Types the LIR walker can hold in a value position: the scalars, `Void`,
    /// `String`, `Array<T>` over a supported `T`, a class with a registered
    /// complete layout (including inherited layouts), plain or instantiated
    /// generic interfaces, plain or instantiated generic enums (`Option<T>` and
    /// `Result<T, E>` included), `Range<i64>`, `Channel<T>`, `Task<T>`,
    /// `JoinHandle<T>`, `Future<T>` and noncapturing function types.
    /// Unsupported collection shapes, generic classes and unresolved generic
    /// instantiations stay OUTSIDE the subset: a body that names one is
    /// refused with `E0800`, since there is no other emitter.
    ///
    /// Admitting an interface here makes it valid STORAGE and a valid
    /// method-call RECEIVER: [`supported_expr`] has a
    /// dedicated arm that resolves the call's vtable slot through
    /// [`Self::iface_method`]. What stays outside the subset is reading an
    /// interface's DATA: `class_layout_of` answers `None` for an interface, so
    /// every field access and every `new` whose type is one is still rejected.
    pub(in crate::backend::cranelift) fn supported_type(&self, ty: &Type) -> bool {
        let mut open = SupportedTypes::default();
        self.supported_type_inner(ty, &mut open)
    }

    fn supported_type_inner(&self, ty: &Type, open: &mut SupportedTypes) -> bool {
        match ty {
            Type::Array(elem) | Type::FixedArray(elem, _) => {
                !matches!(**elem, Type::Void) && self.supported_type_inner(elem, open)
            }
            // The atomic cells: a GC-allocated word with no
            // element type to vet. Admitted before the class path because the
            // prelude owns both names, so nothing else can answer to them.
            Type::Named(_) if atomic_cell(ty).is_some() => true,
            // The cancellation handles: opaque runtime
            // objects, admitted on the same grounds — no element type, and a
            // name the prelude owns.
            Type::Named(_) if cancellation_handle(ty).is_some() => true,
            Type::Named(name) => {
                (self.is_interface)(name)
                    || self.supported_enum_inner(ty, open)
                    || self.supported_class_inner(name, open)
                    // The socket handles: opaque runtime
                    // objects that only the `net::` calls above produce and
                    // consume, so there is no element type to vet. Tried LAST,
                    // unlike the atomic cells and the cancellation handles: the
                    // prelude does not own these two names, so anything the
                    // program declares under them answers first.
                    || matches!(name.name().as_ref(), "TcpListener" | "TcpStream")
            }
            // A `Range<i64>` value: two `i64` words, no
            // element type to vet and no other instantiation to admit.
            Type::Generic(..) if range_i64(ty) => true,
            // The native-blocking cells: a runtime cell
            // holding ONE word, so its element type is vetted exactly as a
            // channel's is. Admitted before the collection path below because
            // the prelude owns both names, so nothing else can answer to them.
            Type::Generic(..) if blocking_cell(ty).is_some() => {
                let (_, elem) = blocking_cell(ty).expect("guarded by the arm");
                !matches!(elem, Type::Void) && self.supported_type_inner(elem, open)
            }
            // Future<void> from sleep/yield is a copyable deadline word with
            // no allocation or GC roots. Other Future<T> types retain the
            // native handle ABI, but have no language-level producers.
            // Stored-value awaits call the runtime in place; native-stack
            // waits park through the scheduler without blocking its worker.
            Type::Generic(..) if builtin_types::unary_arg(ty, B::Future).is_some() => {
                builtin_types::unary_arg(ty, B::Future)
                    .is_some_and(|output| self.supported_type_inner(output, open))
            }
            // The builtin collections  and instantiated generic
            // enums — `Option<T>`, `Result<T, E>`, a user generic enum
            //. Generic *classes* remain outside, so named
            // generic cases are admitted explicitly below rather than from the
            // shape of the type alone.
            Type::Generic(name, args) if (self.is_interface)(name) => {
                !args.is_empty()
                    && args.iter().all(|arg| {
                        !matches!(arg, Type::Void) && self.supported_type_inner(arg, open)
                    })
            }
            Type::Generic(_, args) if builtin_types::unary_arg(ty, B::Channel).is_some() => {
                matches!(args.as_slice(), [elem]
                    if !matches!(elem, Type::Void) && self.supported_type_inner(elem, open))
            }
            // `TaskResult<T>` joins them because it IS one of them: `t.result`
            // is the identity on the async frame pointer, and only the static
            // type distinguishes `await t` from `await t.result`
            //.
            Type::Generic(_, args)
                if builtin_types::resolve(ty).is_some_and(|resolved| {
                    matches!(resolved.id, B::Task | B::JoinHandle | B::TaskResult)
                }) =>
            {
                matches!(args.as_slice(), [result] if self.supported_type_inner(result, open))
            }
            Type::Generic(..) => match lir_collection(ty) {
                Some((LirCollection::FrozenArray, args)) => {
                    matches!(args.as_slice(), [elem]
                        if !matches!(elem, Type::Void) && self.supported_type_inner(elem, open))
                }
                Some((LirCollection::Map | LirCollection::FrozenMap, args)) => {
                    matches!(args.as_slice(), [key, val]
                        if map_key_supported(key)
                            && !matches!(val, Type::Void)
                            && self.supported_type_inner(val, open))
                }
                // A scheduler-aware lock handle. Its
                // protected type is vetted like a cell's, because that is what
                // the acquisition loads out of the handle and the release
                // commits back into it. Tried LAST, like the socket handles
                // above: the prelude does not own these two names, so anything
                // the program declares under them answers first.
                None => {
                    self.supported_enum_inner(ty, open)
                        || scheduler_lock(ty).is_some_and(|(_, protected)| {
                            !matches!(protected, Type::Void)
                                && self.supported_type_inner(protected, open)
                        })
                }
            },
            // The two callable values. A
            // `fn` is a bare code pointer; a `closure` is a GC object whose
            // word 0 is the code pointer and whose remaining words are the
            // captured environment. Both are vetted the same way, because both
            // are CALLED through a Cranelift signature the walker builds from
            // exactly this type: every type in the signature must be one it can
            // pass and receive. The closure's hidden environment parameter adds
            // nothing to vet — it is the object itself.
            Type::Fn(params, ret) | Type::Closure(params, ret) => {
                params
                    .iter()
                    .all(|p| !matches!(p, Type::Void) && self.supported_type_inner(p, open))
                    && self.supported_type_inner(ret, open)
            }
            _ => scalar(ty) || matches!(ty, Type::Void | Type::String),
        }
    }

    /// Whether a value of type `value` can be STORED into a slot declared
    /// `target`: either it already has that representation, or the walker can
    /// box it (see [`Self::boxable`]). Use this at every position where the
    /// emitter passes the value through [`FuncGen::coerce_to_target`]; use the
    /// bare [`assignable_repr`] everywhere else.
    pub(super) fn storable(&self, target: &Type, value: &Type) -> bool {
        self.repr_compatible(target, value)
            || self.iface_widen_path(target, value).is_some()
            || self.boxable(target, value)
            // A fresh empty map fits any admitted map slot: it is one
            // representation with nothing recorded in it yet (see
            // [`empty_map_type`]). `supported_type` still has to accept the
            // TARGET, so this never widens which maps the walker will emit.
            || (empty_map_type(value)
                && matches!(lir_collection(target), Some((LirCollection::Map, _)))
                && self.supported_type(target))
    }

    /// Whether storing `value` into `target` is the class → interface boxing
    /// coercion, AND the vtable that coercion needs exists. Both halves matter:
    /// the emitter builds the box only for a class it has a layout for, and
    /// falls back to the raw object when the vtable lookup misses.
    fn boxable(&self, target: &Type, value: &Type) -> bool {
        let iface = match target {
            Type::Named(name) | Type::Generic(name, _) if (self.is_interface)(name) => name,
            _ => return false,
        };
        let Type::Named(class) = value else {
            return false;
        };
        self.supported_class(class) && (self.can_box)(class, iface)
    }

    /// A non-generic enum the walker can construct and match on, by NAME.
    /// Shorthand for [`Self::supported_enum_type`] on `Type::Named(name)`;
    /// eligibility itself always has the use-site type, so this is the form the
    /// name-level tests below are written against.
    #[cfg(test)]
    pub(in crate::backend::cranelift) fn supported_enum(&self, name: &str) -> bool {
        self.supported_enum_type(&Type::Named(name.to_string().into()))
    }

    /// An enum *instance* the walker can construct and match on: `ty` names a
    /// declared enum, supplies exactly its declared type arguments, and every
    /// payload — after instantiation — is itself supported.
    ///
    /// Generic enums are in the subset as of this slice, which is what admits
    /// `Option<T>` and `Result<T, E>`. Both representations are emittable: the
    /// ordinary `[tag | payload…]` heap object, and the `Option` pointer niche
    /// where `Some(x)` IS `x` and `None` is the null word. Eligibility
    /// deliberately does not care which — [`option_repr`] decides that at
    /// emission, from the same instantiated type vetted here, so validation and
    /// emission always pick the same representation.
    pub(in crate::backend::cranelift) fn supported_enum_type(&self, ty: &Type) -> bool {
        let mut open = SupportedTypes::default();
        self.supported_enum_inner(ty, &mut open)
    }

    pub(super) fn supported_enum_inner(&self, ty: &Type, open: &mut SupportedTypes) -> bool {
        // Validate the concrete arguments even when this definition is already
        // active. In particular Option<Option<unsupported>> cannot use the
        // outer Option's marker to hide its unsupported inner argument.
        let active_definition = if let Type::Generic(name, args) = ty {
            if !args.iter().all(|arg| self.supported_type_inner(arg, open)) {
                return false;
            }
            if let Some(arity) = open.active_enums.get(name) {
                // Check arity without substituting the recursive definition's
                // payloads again. Their concrete arguments were vetted above.
                return args.len() == *arity;
            }
            if open.seen_types.contains(ty) {
                return true;
            }
            Some((*name, args.len()))
        } else {
            None
        };
        let Some((_, def)) = self.enum_instance(ty) else {
            return false;
        };
        // Enum references occupy one payload word. Changing recursive type
        // arguments (`Grow<T> -> Grow<Array<T>>`) therefore need not expand
        // the same definition forever. The definition entry is active only
        // along this path; sibling instantiations still vet their own payloads.
        // Keep exact successful types as memo entries for repeated siblings.
        if !open.seen_types.insert(ty.clone()) {
            return true;
        }
        if let Some((name, arity)) = active_definition {
            open.active_enums.insert(name, arity);
        }
        let supported = def.variants.iter().all(|v| {
            v.payloads
                .iter()
                .all(|t| self.supported_type_inner(t, open))
        });
        if let Some((name, _)) = active_definition {
            open.active_enums.remove(&name);
        }
        if !supported {
            open.seen_types.remove(ty);
        }
        supported
    }

    /// A class the walker can emit: it has a registered field layout, is not an
    /// interface or an enum, and every field type is itself supported.
    ///
    /// Inheritance used to be a hard exclusion here, because a direct call to
    /// `Class__method` is only the whole of dispatch when the receiver's static
    /// type is exact. It is no longer: a virtual call goes through the class
    /// DESCRIPTOR at a compile-time slot index, and the walker
    /// takes that decision from the shared [`FuncGen::plan_virtual_call`]
    ///. What makes the rest safe is that a subclass EXTENDS
    /// rather than rearranges: its field layout starts with its base's, and its
    /// slot order starts with its base's, so code compiled against the base
    /// reads the right offsets and calls the right slots on a subclass
    /// instance.
    ///
    /// Only the class NAMED here has to be supported. A subclass with a field
    /// type the walker does not admit stays out of the subset on its own terms
    /// — no expression typed as the base ever touches that field.
    pub(in crate::backend::cranelift) fn supported_class<
        Q: crate::backend::cranelift::type_index::TypeLookup + ?Sized,
    >(
        &self,
        name: &Q,
    ) -> bool {
        let mut open = SupportedTypes::default();
        self.supported_class_inner(&name.type_id(), &mut open)
    }

    /// The name the class tables are keyed on for `name`, or `None` when
    /// nothing this compilation unit registered answers to it. See
    /// [`resolve_class_key`] — eligibility and emission must resolve a name the
    /// same way or the walker admits a body the emitter then cannot key.
    fn class_key(&self, name: &TypeId) -> Option<TypeId> {
        resolve_class_key(self.classes, self.known_modules, self.visible_modules, name)
    }

    fn supported_class_inner(&self, name: &TypeId, open: &mut SupportedTypes) -> bool {
        if (self.is_interface)(name) || self.is_enum(name) {
            return false;
        }
        let Some(key) = self.class_key(name) else {
            return false;
        };
        let Some(layout) = self.classes.class_fields(&key) else {
            return false;
        };
        // A self- or mutually-referential field (`class Node { next: Node; }`)
        // is fine — it is the same layout — but must not recurse forever. Keyed
        // on the resolved name, so the bare and qualified spellings of one
        // class close the same cycle.
        if !open.seen_types.insert(Type::Named(key)) {
            return true;
        }
        layout
            .iter()
            .all(|(_, ty)| self.supported_type_inner(ty, open))
    }

    /// Whether `name` names a class — as opposed to an interface, an enum, or
    /// nothing this compilation unit registered.
    pub(super) fn is_class(&self, name: &TypeId) -> bool {
        !(self.is_interface)(name) && !self.is_enum(name) && self.class_key(name).is_some()
    }

    /// Whether a value of class type `value` may be stored where class `target`
    /// is expected because `value` is a strict SUBCLASS of it
    ///.
    ///
    /// At run time the widening is nothing: both are raw object pointers. What
    /// it costs is exactness — the reader then works from `target`'s layout and
    /// `target`'s slot indices — and a subclass extends both rather than
    /// rearranging either, so both remain valid.
    ///
    /// The question is answered in `type_id` space rather than over NAMES. A
    /// direct type import (`import zoo::Animal;`) aliases the imported class
    /// under the unqualified `Animal`, while base edges keep CANONICAL names
    /// on both sides (`zoo::Dog` → `zoo::Animal`) — so comparing `"Animal"` to
    /// those strings finds nothing and an aliased base looks like a leaf.
    /// Aliased and canonical spellings share one runtime `type_id`, so the
    /// walk up `sub`'s chain compares ids at every step and costs one step per
    /// ancestor rather than a projection of the whole class graph.
    pub(super) fn class_widening(&self, target: &Type, value: &Type) -> bool {
        let (Type::Named(base), Type::Named(sub)) = (target, value) else {
            return false;
        };
        if !self.is_class(base) || !self.is_class(sub) {
            return false;
        }
        let (Some(base_key), Some(sub_key)) = (self.class_key(base), self.class_key(sub)) else {
            return false;
        };
        let (Some(base_id), Some(sub_id)) = (
            self.classes.class_type_id(&base_key),
            self.classes.class_type_id(&sub_key),
        ) else {
            return false;
        };
        // Identity is `assignable_repr`'s job; this arm answers only the
        // strictly-wider case.
        base_id != sub_id
            && is_self_or_descendant(
                sub_key,
                base_id,
                |name| self.classes.class_type_id(&name),
                |name| self.classes.class_base(&name),
            )
    }

    /// Whether a declared callable signature and the type a function value
    /// carries share one ABI. A generic instance's lifted lambda is declared
    /// with the `$type$mono$...` spelling of a nominal argument while its
    /// lowered body names the declaration itself.
    pub(super) fn same_callable(&self, known: &Type, ty: &Type) -> bool {
        match (known, ty) {
            (Type::Fn(a, a_out), Type::Fn(b, b_out))
            | (Type::Closure(a, a_out), Type::Closure(b, b_out)) => {
                a.len() == b.len()
                    && a.iter().zip(b).all(|(a, b)| self.same_repr(a, b))
                    && self.same_repr(a_out, b_out)
            }
            _ => known == ty,
        }
    }

    /// [`assignable_repr`], plus the one identity a string comparison cannot
    /// see: two spellings of the SAME class.
    ///
    /// A module's classes are keyed canonically — `declare_module` qualifies
    /// `Point` to `geom::Point`, fields and all — and aliased back under their
    /// bare local names while that module's bodies are compiled. So a field
    /// declared `at: Point` reaches the walker as `geom::Point` from the layout
    /// and as `Point` from the lowered body, and those are one type. Anything
    /// else defers to [`assignable_repr`], including the deliberate exactness
    /// of the collection generics.
    pub(super) fn same_repr(&self, target: &Type, value: &Type) -> bool {
        let mut open = HashSet::new();
        self.same_repr_inner(target, value, &mut open)
    }

    /// `open` carries the enum name pairs already being compared, so a
    /// recursive payload (`enum List { Cons(List), Nil }`) closes its cycle
    /// instead of recursing forever.
    fn same_repr_inner(
        &self,
        target: &Type,
        value: &Type,
        open: &mut HashSet<(TypeId, TypeId)>,
    ) -> bool {
        match (target, value) {
            (Type::Named(a), Type::Named(b)) => {
                a == b
                    || self.same_class(a, b)
                    || self.same_enum(a, b, open)
                    || self.same_iface(a, b)
            }
            (Type::Array(a) | Type::FixedArray(a, _), Type::Array(b) | Type::FixedArray(b, _)) => {
                self.same_repr_inner(a, b, open)
            }
            (Type::Generic(a, ta), Type::Generic(b, tb)) => {
                (a == b || self.same_iface(a, b) || self.same_enum(a, b, open))
                    && ta.len() == tb.len()
                    && ta
                        .iter()
                        .zip(tb)
                        .all(|(x, y)| self.same_repr_inner(x, y, open))
            }
            // A signature can name one class under two spellings, like any
            // other position.
            (Type::Fn(pa, ra), Type::Fn(pb, rb))
            | (Type::Closure(pa, ra), Type::Closure(pb, rb)) => {
                pa.len() == pb.len()
                    && pa
                        .iter()
                        .zip(pb)
                        .all(|(x, y)| self.same_repr_inner(x, y, open))
                    && self.same_repr_inner(ra, rb, open)
            }
            _ => assignable_repr(target, value),
        }
    }

    /// Whether two enum names are one declaration under two spellings. A module
    /// body spells its own enum bare (`Kind`), while the SIGNATURE of a method
    /// declared in that module carries the qualified spelling (`kinds::Kind`) —
    /// a class declaration is module-qualified whole before it is declared. One
    /// declaration, registered under both names, so the strings differ where
    /// the declaration does not.
    ///
    /// An item import settles it outright: `import signal::Level;` — or
    /// `as Rank` — registers the declaration under the local spelling too, and
    /// every entry carries the declaring module's identity, so two spellings of
    /// one declaration answer the same [`LirEnumDef::identity`]. An alias has
    /// no suffix in common with what it renames, so this is the ONLY test that
    /// settles `Rank` against `signal::Level`.
    ///
    /// Where no identity is shared, both halves are checked directly: one name
    /// must be the other under a module prefix this build declared, AND the two
    /// declarations must agree on variant order (the order IS the tag), variant
    /// names, and payload representation. Neither half alone would do — the
    /// prefix test cannot see that two files disagree, and shape alone would
    /// merge two unrelated enums.
    fn same_enum(&self, a: &TypeId, b: &TypeId, open: &mut HashSet<(TypeId, TypeId)>) -> bool {
        let (Some(da), Some(db)) = ((self.enum_def)(a), (self.enum_def)(b)) else {
            return false;
        };
        if da.identity == db.identity {
            return true;
        }
        let is_qualified_form = |bare: &TypeId, qualified: &TypeId| -> bool {
            bare.namespace().is_none()
                && bare.name() == qualified.name()
                && qualified
                    .namespace()
                    .is_some_and(|namespace| self.known_modules.contains_key(namespace.as_ref()))
        };
        if !is_qualified_form(a, b) && !is_qualified_form(b, a) {
            return false;
        }
        if !open.insert((*a, *b)) {
            return true;
        }
        da.type_params == db.type_params
            && da.variants.len() == db.variants.len()
            && da.variants.iter().zip(&db.variants).all(|(x, y)| {
                x.name == y.name
                    && x.payloads.len() == y.payloads.len()
                    && x.payloads
                        .iter()
                        .zip(&y.payloads)
                        .all(|(p, q)| self.same_repr_inner(p, q, open))
            })
    }

    /// Whether two interface names are one declaration under two spellings.
    /// A module that item-imported an interface writes it bare in its own
    /// signature while the unit reading that signature may hold the qualified
    /// spelling, and neither is wrong — only the registered identity settles it
    ///, exactly as [`Self::same_enum`] settles an enum alias.
    fn same_iface(&self, a: &TypeId, b: &TypeId) -> bool {
        match ((self.iface_identity)(a), (self.iface_identity)(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        }
    }

    /// Whether two class names name one runtime class. One class is one
    /// `type_id`, which is what the bare alias copies from the canonical entry,
    /// so the ids answer this even though the strings differ.
    fn same_class(&self, a: &TypeId, b: &TypeId) -> bool {
        if !self.is_class(a) || !self.is_class(b) {
            return false;
        }
        let (Some(ka), Some(kb)) = (self.class_key(a), self.class_key(b)) else {
            return false;
        };
        if ka == kb {
            return true;
        }
        matches!(
            (self.classes.class_type_id(&ka), self.classes.class_type_id(&kb)),
            (Some(x), Some(y)) if x == y
        )
    }

    /// [`Self::same_repr`] widened by class subtyping. Use it wherever a value
    /// flows into a declared slot whose type the emitter does NOT coerce to —
    /// an array element, a `return`, a field store, a branch of an `if`.
    pub(super) fn repr_compatible(&self, target: &Type, value: &Type) -> bool {
        self.same_repr(target, value)
            || self.class_widening(target, value)
            || self
                .iface_widen_path(target, value)
                .is_some_and(|path| path.is_empty())
    }

    /// The vtable-slot adjustment required to widen `value` to `target`.
    /// Identity is handled by [`assignable_repr`], so this answers only a
    /// strict interface-to-super-interface conversion.
    fn iface_widen_path(&self, target: &Type, value: &Type) -> Option<Vec<usize>> {
        // A generic interface widens to the non-generic supers it extends
        //; type arguments do not change its vtable.
        let (Type::Named(target_iface), Type::Named(value_iface) | Type::Generic(value_iface, _)) =
            (target, value)
        else {
            return None;
        };
        (target_iface != value_iface
            && (self.is_interface)(target_iface)
            && (self.is_interface)(value_iface))
        .then(|| (self.iface_widen_path)(target_iface, value_iface))
        .flatten()
    }

    /// The mangled symbol of the implementation a call to `class::method`
    /// resolves to: the nearest class in `class`'s own ancestry — itself first
    /// — that declares it, so a subclass that INHERITS a method resolves to the
    /// implementation it actually inherits.
    ///
    /// Mirrors [`FuncGen::resolve_defining_class`], which is what emission then
    /// uses through [`FuncGen::plan_virtual_call`]; answering `None` here is
    /// what keeps a call the emitter could not resolve out of the subset.
    pub(super) fn resolve_class_method(&self, class: &TypeId, method: &str) -> Option<String> {
        // Deliberately NOT run through [`resolve_class_key`]: emission resolves
        // a method through `FuncGen::resolve_defining_class` and
        // `plan_virtual_call`, which key on the class name AS WRITTEN, so
        // admitting a bare module-class receiver here would promise a dispatch
        // the emitter cannot resolve. Nothing is lost — the checker rejects
        // `p.m` on a module class the entry file never item-imported
        // (E0350), and an item import puts the short name in the tables
        //.
        let mut search = Some(*class);
        let mut seen = HashSet::new();
        while let Some(name) = search {
            if !seen.insert(name) {
                break;
            }
            let mangled = class_method_symbol_name(self.known_modules, &name.to_string(), method);
            if (self.known_fn)(&mangled) {
                return Some(mangled);
            }
            search = self.classes.class_base(&name);
        }
        None
    }

    /// The declared field layout of a supported class named by `ty`.
    pub(super) fn class_layout_of(&self, ty: &Type) -> Option<std::sync::Arc<Vec<(String, Type)>>> {
        let Type::Named(name) = ty else { return None };
        if !self.supported_class(name) {
            return None;
        }
        self.classes.class_fields(&self.class_key(name)?)
    }

    /// The declared callable type of a symbol that is about to be used as a
    /// VALUE — `fn(...) ->...`, or `closure(...) ->...` for a lifted
    /// capturing lambda — or `None` when it
    /// cannot be. The closure's hidden environment parameter is NOT part of
    /// this type: it is the value itself, not something a call site passes.
    ///
    /// Three things have to hold, and all three are about the pointer being
    /// callable later through a signature built from the type alone: the symbol
    /// is linkable, every parameter is passed by value (a by-reference
    /// parameter has no spelling in a `fn(...)` type, so the call site could not
    /// reproduce it), and the whole signature is inside the subset.
    pub(super) fn fn_value_of(&self, mangled: &FunctionId) -> Option<Type> {
        if !(self.known_fn_id)(mangled) {
            return None;
        }
        if self
            .func_param_modes
            .get_id(mangled)
            .is_some_and(|modes| modes.iter().any(|m| !matches!(m, ParamMode::Value)))
        {
            return None;
        }
        let ty @ (Type::Fn(..) | Type::Closure(..)) = self.fn_types.get_id(mangled)?.clone() else {
            return None;
        };
        self.supported_type(&ty).then_some(ty)
    }

    /// Whether a direct call to the symbol `mangled` is emittable with `args`:
    /// the symbol exists, every parameter's declared passing mode agrees with
    /// the shape of the argument in that position — a `&`/`&mut` parameter
    /// takes a `&place` and nothing else, and a by-value parameter takes
    /// anything but one — and every declared parameter type is supported and
    /// accepts its argument, directly or by boxing it into an interface.
    /// `skip_self` drops the hidden receiver parameter that class methods and
    /// static calls carry.
    ///
    /// A symbol with no recorded signature has no modes to agree with, so a
    /// `&place` is refused there outright rather than guessed at.
    #[cfg(test)]
    pub(super) fn callable(&self, mangled: &str, args: &[HirExpr], skip_self: bool) -> bool {
        if !(self.known_fn)(mangled) {
            return false;
        }
        let modes = self.func_param_modes.get(mangled);
        let Some(Type::Fn(params, _)) = self.fn_types.get(mangled) else {
            // No recorded signature (a runtime symbol, or a shape the front end
            // did not register): only argument types whose representation the
            // walker cannot get wrong may reach it.
            return modes.is_none()
                && args.iter().all(|a| {
                    !matches!(a.ty, Type::Named(_))
                        && !matches!(a.kind, HirExprKind::ReferenceArg { .. })
                });
        };
        let params: &[Type] = if skip_self {
            match params.split_first() {
                Some((_, rest)) => rest,
                None => return false,
            }
        } else {
            params
        };
        params.len() == args.len()
            && params.iter().zip(args).enumerate().all(|(idx, (p, a))| {
                let mode = modes.and_then(|m| m.get(idx)).unwrap_or(&ParamMode::Value);
                let reference_shape = matches!(a.kind, HirExprKind::ReferenceArg { .. });
                matches!(mode, ParamMode::Reference { .. }) == reference_shape
                    && self.supported_type(p)
                    && self.storable(p, &a.ty)
            })
    }
}
