//! LIR-walking code generation — willow-0g8j.
//!
//! First stage of migrating the emit layer off the raw AST: a function whose
//! lowered IR uses only the supported scalar subset is compiled by walking its
//! [`LirFunction`] basic blocks directly (typed [`HirExpr`] trees inside), so
//! the backend never touches the AST body for it. Everything else falls back to
//! the existing AST-walking path, chosen per function in
//! `compile_function_named`. `WILLOW_LIR_BACKEND=0` disables the LIR path;
//! `WILLOW_LIR_REQUIRE=1` turns the fallback into a hard compile error, so a
//! test can pin a function to this path instead of passing vacuously when a
//! lowering or eligibility regression sends it back to the AST walker.
//!
//! Supported subset (v8): `i64`/`f64`/`bool`/`String`/`Array<T>` values,
//! SIMPLE class objects, interface values and NON-GENERIC enums, plus the
//! builtin collections `Map<K, V>`, `FrozenArray<T>` and `FrozenMap<K, V>`;
//! literals, variables, arithmetic/comparison, unary ops, string concatenation
//! and content comparison; array literals, indexing, index-assignment and the
//! builtin `len`/`push`/`pop`/`toString`/`freeze` methods; `Map::new()` and the
//! map methods `insert`/`contains`/`len`/`toString`/`freeze`; `new`, field
//! reads, field assignment, instance and static method calls; enum-variant
//! construction and `match`; direct calls to known non-async functions;
//! `print`/`println` of a scalar or a string; `let`/assign; the full block
//! control flow (jump/branch/return).
//!
//! A class is SIMPLE when it has no base class, is not itself a base, is
//! neither an interface nor an enum, has a known field layout, and every field
//! type is itself supported. Inheritance dispatches virtually, so a class that
//! takes part in an `extends` edge stays on the AST path. Nullable types,
//! async functions, `defer`, and closures that CAPTURE also stay on the AST
//! path for now. A capture-free lambda does not: willow-0g8j.2.2 lifts its
//! body to its own [`LirFunction`], so the walker compiles it like any other
//! function and materializes the value as a function address.
//!
//! Enums and `match` (willow-0g8j.8) mirror [`FuncGen::emit_match`]
//! instruction for instruction, including the rule that decides the
//! representation: an enum is a bare i64 tag when NO variant carries a
//! payload, and a `[tag | payload…]` GC object otherwise
//! ([`FuncGen::enum_is_gc_object_type`]). Generic enums came in with
//! willow-0g8j.2.1 (see the `Option`/`Result` note below), and willow-0g8j.2.5
//! admitted block-bodied arms in the one shape that needs no merge value: an
//! arm that DIVERGES, ending in `return` or `panic(...)`. An arm that produces
//! a value is still a single expression, because a `let` in one would bind a
//! name the walker's flat `vars` map cannot scope to that arm.
//!
//! Pattern bindings are plain Cranelift variables rather than rooted slots,
//! which is safe for exactly one reason and is worth stating: the SCRUTINEE is
//! rooted across every arm, Willow's collector is non-moving, and a payload is
//! reachable from the scrutinee — so a binding cannot be reclaimed and its
//! address cannot change while the arm runs. This is the same argument the AST
//! path relies on; if the collector ever moves objects, both emitters change
//! together.
//!
//! Interface boxing (willow-j260): an interface-typed slot holds a 16-byte
//! `[object | vtable]` GC box, so storing a class value into one is a real
//! conversion, not a reinterpretation. The walker now performs it, but only
//! where a store actually happens — `let` init, assignment, call argument,
//! `return`, field store, array element, `push` — and only through
//! [`FuncGen::coerce_to_target`], the same helper the AST path uses.
//! Eligibility admits such a store only when
//! [`super::emit::resolve_vtable_id`] finds the vtable the emitter will need,
//! because `coerce_to_target` silently yields the raw object when it does not
//! and an unboxed class pointer in an interface slot would crash dispatch.
//! Interface DISPATCH (willow-0g8j.6) is now in the subset too: a method call
//! on an interface-typed receiver loads `[object | vtable]` out of the box,
//! indexes the vtable by the method's declaration-order slot and issues a
//! `call_indirect`, mirroring [`FuncGen::emit_interface_dispatch`] including
//! the `Self`-returning re-box. Eligibility resolves that slot through
//! `LirTypeCtx::iface_method` — the AST emitter answers a call the interface
//! does not declare with a constant `0`, so admitting one would miscompile.
//! A method with a `&`/`&mut` parameter is refused: that parameter arrives as
//! a POINTER and the walker only passes values (willow-0g8j.9).
//! Field access through an interface is still outside the subset: `new` and
//! field reads go through `class_layout_of`, which has no layout for an
//! interface name. The boxing allocation is the one coercion that runs
//! AFTER its value expression, so every site that roots a live temporary
//! across the value must treat "this store boxes" as allocating too; see
//! [`FuncGen::lir_store_allocates`].
//!
//! GC rooting (willow-0g8j.1): the LIR has no block scopes — it is a flat
//! basic-block graph — so a per-`let` push/pop pairing like the AST path's
//! would grow the shadow root stack once per loop iteration. Instead every
//! GC-managed local gets ONE stack slot allocated and rooted at function
//! entry, null-initialized so a collection before the `let` runs sees an empty
//! slot; the slot is both the variable's storage and its root (the AST
//! invariant that keeps a reassignment from leaving a stale root), and all
//! roots are popped at each `return`. Expression temporaries that must survive
//! an allocating call are rooted exactly as the AST path roots them.
//!
//! Arrays (willow-0g8j.4) ride on the same discipline: an `Array<T>` value is a
//! GC handle whose pointer is stable across growth (a `push` reallocates only
//! the buffer), so the entry slot of an array local stays valid for the whole
//! function. Temporaries are rooted around any sub-expression that can run a
//! collection, decided by [`may_allocate`] so a literal index or a constant
//! element list costs no root traffic.
//!
//! Maps and frozen collections (willow-0g8j.7) are the same kind of GC handle,
//! so they reuse that discipline unchanged; what is new is the ABI they read
//! and write through. A `Map<K, V>` crosses the runtime boundary as raw 64-bit
//! words plus is-ref flags — `willow_map_insert(map, k, k_ref, v, v_ref)` —
//! rather than the array's fixed word layout, and `FrozenArray<T>` /
//! `FrozenMap<K, V>` are the SAME runtime objects as the collections they were
//! frozen from, so their reads lower to the very same `willow_array_*` /
//! `willow_map_*` calls. `freeze` is a copy, not a cast.
//!
//! One restriction on that subset is deliberate. Map keys are admitted only
//! as `String` or `i64`, because the runtime's `MapKey` is exactly
//! `Int(i64) | Str(String)` and it decides between them from the is-ref flag:
//! any other GC-managed key would be read as a `WillowString` pointer that it
//! is not.
//!
//! `Option<T>` and `Result<T, E>` are ordinary prelude enums, so willow-0g8j.2.1
//! admits them the same way as any other generic enum: construction, `match`,
//! the value-taking methods (`unwrap`, `unwrapOr`, `isSome`, …), `Map::get`, and
//! `?` propagation. The representation choice is NOT re-derived here — every
//! site asks [`super::option_repr::option_repr`], the same decision the AST
//! emitter and the runtime ABI are built on, so a niche `Option<String>` and a
//! boxed `Option<i64>` cannot be confused for one another.

use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::{
    AbiParam, InstBuilder, MemFlagsData, StackSlotData, StackSlotKind, condcodes::FloatCC,
    condcodes::IntCC, types,
};
use cranelift_module::Module;

use crate::diagnostics::span::Span;
use crate::ir::dump::binop_str;
use crate::ir::lowered::{LirBlock, LirFunction, LirInst, Terminator};
use crate::ir::typed_ast::{HirExpr, HirExprKind, HirMatchArm, HirPattern, HirStmt};
use crate::parser::ast::{BinOp, ParamMode, Type, UnaryOp};
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::ids::FunctionMap;
use crate::semantic::intrinsics::{self, Intrinsic};
use crate::semantic::type_checker::types::type_name;

use super::emit_interface::collection_elem_kind;
use super::gc_codegen::{GcLayoutMetadata, GcObjectKind, GcStoreDestination};
use super::option_repr::{OptionRepr, option_inner, option_repr};
use super::symbols::{class_method_symbol_name, class_name_for_object_type};
use super::type_helpers::{clif_type, is_gc_managed};
use super::{FuncGen, VarStorage, array_element_type, result_err_type, try_propagate_payload_type};

/// True when the environment does not disable the LIR backend.
pub(super) fn lir_backend_enabled() -> bool {
    std::env::var("WILLOW_LIR_BACKEND")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// True when the environment demands that every function compile through the
/// LIR path. A fallback to the AST emitter is then a compile error naming the
/// function (willow-0g8j.4 review): the differential tests compare "LIR on"
/// against "LIR off", and without this a function that stopped being eligible
/// would just compare the AST path with itself and still pass.
///
/// Opt-in, so ordinary builds keep falling back silently for everything the
/// walker does not support yet.
pub(super) fn lir_required() -> bool {
    std::env::var("WILLOW_LIR_REQUIRE")
        .map(|v| v != "0")
        .unwrap_or(false)
}

fn scalar(ty: &Type) -> bool {
    matches!(ty, Type::I64 | Type::F64 | Type::Bool)
}

/// Whether a *supported* type is a GC-managed heap reference — answerable
/// during eligibility, before a `FuncGen` exists, and therefore without the
/// enum table [`is_gc_managed`] needs.
///
/// It can do without that table because of where it is called from: the ONLY
/// caller is [`assignable_repr`]'s catch-all arm, which `Type::Named` never
/// reaches (two named types are compared by name above it). That matters now
/// that enums are in the subset (willow-0g8j.8) — an enum is `Type::Named` and
/// is GC-managed only when some variant carries a payload, which this function
/// has no way to know. Keep the `Named` arm ahead of the catch-all.
///
/// Every generic in the subset is a builtin collection handle, and all three
/// are real GC heap objects — the opaque runtime-pointer generics
/// (`Future`, `BlockingCell`, …) never pass [`LirTypeCtx::supported_type`].
fn gc_managed_supported(ty: &Type) -> bool {
    matches!(
        ty,
        Type::String | Type::Array(_) | Type::Named(_) | Type::Generic(_, _)
    )
}

/// The builtin collection generics the walker emits, split out so eligibility
/// and emission agree on exactly which `Type::Generic`s are in the subset.
///
/// Recognition goes through [`builtin_types::resolve`] rather than a name
/// comparison so a user type that happens to be called `Map` cannot be mistaken
/// for the builtin one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LirCollection {
    /// `FrozenArray<T>` — the immutable view of an `Array<T>`, backed by the
    /// same runtime array handle.
    FrozenArray,
    /// `Map<K, V>`.
    Map,
    /// `FrozenMap<K, V>` — the same runtime map object, without the writers.
    FrozenMap,
}

fn lir_collection(ty: &Type) -> Option<(LirCollection, Vec<Type>)> {
    let resolved = builtin_types::resolve(ty)?;
    let kind = match resolved.id {
        B::FrozenArray => LirCollection::FrozenArray,
        B::Map => LirCollection::Map,
        B::FrozenMap => LirCollection::FrozenMap,
        _ => return None,
    };
    Some((kind, resolved.args.to_vec()))
}

/// `Map<Void, Void>` — the type the checker gives `Map::new()`.
///
/// The empty-map constructor is genuinely untyped: `willow_map_new` takes no
/// arguments and records nothing about its keys or values, and the runtime does
/// not learn whether values are references until the first `insert`. So the
/// checker never needs a concrete instantiation here and does not compute one,
/// and the AST path has never cared. The walker does care, because it compares
/// representations before every store — hence this one narrow exemption, which
/// [`is_fresh_empty_map`] pairs with a node-level check so nothing ELSE can
/// present itself as an untyped map.
fn empty_map_type(ty: &Type) -> bool {
    matches!(lir_collection(ty), Some((LirCollection::Map, args))
        if args.as_slice() == [Type::Void, Type::Void])
}

/// Whether `e` is literally `Map::new()`, the only expression that may carry the
/// untyped [`empty_map_type`].
///
/// This is what keeps the type-level exemption honest. `supported_type` REJECTS
/// `Map<Void, Void>`, so it can never be a parameter, a `let`'s declared type or
/// a return type; the only way a value of that type can exist in an eligible
/// function is through this node, which is admitted here and nowhere else.
fn is_fresh_empty_map(e: &HirExpr) -> bool {
    matches!(&e.kind, HirExprKind::StaticCall { class, method, args }
        if class == "Map" && method == "new" && args.is_empty())
        && empty_map_type(&e.ty)
}

/// Whether `ty` can be a map key the walker emits.
///
/// The runtime's key is `Int(i64) | Str(String)` and it picks between them from
/// the is-ref flag the backend passes. `String` is the only reference key it can
/// read; `i64` is the only word key the language documents. Admitting anything
/// else — a `bool`, a class, an array — would hand the runtime a word it
/// interprets as one of those two, so everything else falls back.
fn map_key_supported(ty: &Type) -> bool {
    matches!(ty, Type::String | Type::I64)
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
fn assignable_repr(target: &Type, value: &Type) -> bool {
    match (target, value) {
        (Type::Named(a), Type::Named(b)) => a == b,
        (Type::Named(_), _) | (_, Type::Named(_)) => false,
        // Array handles also carry element semantics (`is_ref`), and interface
        // elements are boxed while class elements are raw object pointers.
        // Consequently two arrays are representation-compatible only when the
        // element types agree exactly. Empty `Array<Void>` literals do not reach
        // HIR today; supporting them later requires contextual element typing at
        // allocation time, not a blanket handle reinterpretation.
        (Type::Array(a), Type::Array(b)) => a == b,
        // The collection generics carry their element semantics the same way,
        // and they all lower to one pointer-sized word — so without an exact
        // test the fallback below would call a `Map<String, i64>` and a
        // `FrozenArray<i64>` interchangeable (willow-0g8j.7).
        (Type::Generic(..), Type::Generic(..)) => target == value,
        (Type::Generic(..), _) | (_, Type::Generic(..)) => false,
        _ => {
            clif_type(target) == clif_type(value)
                && gc_managed_supported(target) == gc_managed_supported(value)
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
pub(super) struct IfaceMethodSig {
    pub params: Vec<Type>,
    /// The declared passing mode of each parameter. A `&`/`&mut` parameter is a
    /// POINTER in the dispatch ABI, and the walker has no reference-argument
    /// emission at all — so eligibility rejects any method that has one, the
    /// same rule [`LirTypeCtx::callable`] applies to direct calls
    /// (willow-0g8j.9). HIR lowering refuses a reference ARGUMENT before that
    /// (k24), so this is the second line of defence: the one that still holds
    /// the day lowering learns them and the walker has not.
    pub modes: Vec<ParamMode>,
    pub ret: Type,
}

/// One variant of an enum as ELIGIBILITY needs it: the name it is selected by,
/// and the declared payload types in declaration order, which are also the
/// payload SLOT order in the heap object. The runtime tag is deliberately
/// absent — emission reads it from [`FuncGen::enum_variant_tag`], so there is
/// no second copy to drift.
#[derive(Clone)]
pub(super) struct LirEnumVariant {
    pub name: String,
    pub payloads: Vec<Type>,
}

/// An enum declaration as eligibility sees it (willow-0g8j.8).
#[derive(Clone)]
pub(super) struct LirEnumDef {
    /// Declared type parameters, in order. A non-empty list means the enum is
    /// GENERIC, and [`LirTypeCtx::supported_enum`] refuses it: `payloads` then
    /// holds type-parameter placeholders rather than real types, and only a
    /// concrete `Type::Generic` scrutinee carries the arguments that would
    /// resolve them.
    pub type_params: Vec<String>,
    pub variants: Vec<LirEnumVariant>,
}

impl LirEnumDef {
    fn variant(&self, name: &str) -> Option<&LirEnumVariant> {
        self.variants.iter().find(|v| v.name == name)
    }
}

/// The program facts eligibility needs beyond the lowered IR itself: which
/// named types are classes the walker can lay out, which symbols exist, and
/// what those symbols' signatures are. Built from the compiler's registration
/// tables at the dispatch site in `compile_function_named`.
#[derive(Clone, Copy)]
pub(super) struct LirTypeCtx<'x> {
    /// Whether a symbol name is a declared/linkable function.
    pub known_fn: &'x dyn Fn(&str) -> bool,
    pub class_layouts: &'x HashMap<String, Vec<(String, Type)>>,
    pub class_base: &'x HashMap<String, String>,
    /// Runtime `type_id` per class NAME. A direct type import (`import
    /// zoo::Animal;`) registers the imported class a second time under its
    /// unqualified name, sharing the canonical class's id — so this is what
    /// makes class IDENTITY comparable across those two names.
    pub class_type_ids: &'x HashMap<String, i64>,
    /// Whether a name is registered as an interface (never a class here).
    pub is_interface: &'x dyn Fn(&str) -> bool,
    /// Whether boxing `(class, interface)` resolves to a registered vtable —
    /// exactly what [`FuncGen::emit_interface_box`] will look up. A coercion it
    /// cannot build must not be admitted: the emitter's fallback is to pass the
    /// raw object through, which would put an unboxed class pointer in an
    /// interface slot (willow-j260).
    pub can_box: &'x dyn Fn(&str, &str) -> bool,
    /// The declaration of an enum NAME, or `None` when the name is not an enum.
    /// Read from the same `enum_infos` table [`FuncGen::enum_variant_tag`] and
    /// [`FuncGen::enum_is_gc_object_type`] answer from, so the tags and the
    /// representation eligibility vets are the ones emission uses
    /// (willow-0g8j.8). This is also the single source of "is this an enum?" —
    /// see [`LirTypeCtx::is_enum`] — so the two can never disagree.
    pub enum_def: &'x dyn Fn(&str) -> Option<LirEnumDef>,
    /// The vtable slot and signature of `(interface, method)`, i.e. exactly what
    /// [`FuncGen::emit_interface_dispatch`] indexes and calls. `None` when the
    /// name is not an interface, or the interface does not declare that method
    /// — the AST emitter answers such a call with a constant `0`, so a walker
    /// that admitted it would silently miscompile (willow-0g8j.6).
    pub iface_method: &'x dyn Fn(&str, &str) -> Option<IfaceMethodSig>,
    pub fn_types: &'x FunctionMap<Type>,
    pub func_param_modes: &'x FunctionMap<Vec<ParamMode>>,
    pub known_modules: &'x HashMap<String, String>,
    /// The `$lambda.N` symbol a lambda expression was lifted to, by the span of
    /// the lambda (willow-0g8j.2.2). `None` for a lambda the backend never
    /// declared — a lambda inside an imported module, which is not lifted at
    /// all — so the walker refuses rather than emitting the address of nothing.
    pub lambda_symbol: &'x dyn Fn(Span) -> Option<String>,
    /// The declared return type of the function being vetted. Unlike every
    /// other field this one is per-FUNCTION, and it is here because a `return`
    /// inside a `match` arm is checked deep inside `supported_expr`, where the
    /// enclosing [`LirFunction`] is out of reach (willow-0g8j.2.5).
    pub return_type: &'x Type,
}

/// Normalise a variant's payload list for a use site where every payload
/// substituted away to `void`.
///
/// `Result<void, E>::Ok()` is the case that forces this: the declared payload
/// is `T`, the instantiation makes it `void`, and the checker accepts the call
/// with ZERO arguments. The AST emitter already behaves this way — it derives
/// the heap layout from the ARGUMENTS and only indexes `payload_types`
/// positionally — so dropping the list here is what keeps eligibility, the LIR
/// emitter and [`FuncGen::emit_enum_variant_alloc`] describing one object.
///
/// A *mixed* list (`void` in one slot, a real type in another) is deliberately
/// left alone: the arity check downstream then fails and the function falls
/// back, rather than the walker silently renumbering payload slots.
fn normalize_void_payloads(payloads: &mut Vec<Type>) {
    if !payloads.is_empty() && payloads.iter().all(|t| matches!(t, Type::Void)) {
        payloads.clear();
    }
}

impl LirTypeCtx<'_> {
    /// Whether `name` is a declared enum. Answered from [`Self::enum_def`], so
    /// there is exactly one table deciding it.
    fn is_enum(&self, name: &str) -> bool {
        (self.enum_def)(name).is_some()
    }

    /// The enum `ty` denotes, with every payload already instantiated *at this
    /// use site* (willow-0g8j.2.1).
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
    fn enum_instance(&self, ty: &Type) -> Option<(String, LirEnumDef)> {
        let (name, args): (&str, &[Type]) = match ty {
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
        let map: HashMap<String, Type> = def
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
            name.to_string(),
            LirEnumDef {
                type_params: Vec::new(),
                variants,
            },
        ))
    }

    /// Types the LIR walker can hold in a value position: the scalars, `Void`,
    /// `String`, `Array<T>` over a supported `T`, a *simple class* (see
    /// [`Self::supported_class`], willow-0g8j.5), a plain interface name
    /// (willow-j260) and a non-generic enum (see [`Self::supported_enum`],
    /// willow-0g8j.8) or an instantiated generic enum, `Option<T>` and
    /// `Result<T, E>` included (willow-0g8j.2.1). Maps of unsupported shapes,
    /// other generics — `Task`, `Range`, `Future`, generic classes and generic
    /// interface instantiations, whose boxing the walker does not model — and
    /// function types still fall back to the AST path.
    ///
    /// Admitting an interface here makes it valid STORAGE, and — since
    /// willow-0g8j.6 — a valid method-call RECEIVER: [`supported_expr`] has a
    /// dedicated arm that resolves the call's vtable slot through
    /// [`Self::iface_method`]. What stays outside the subset is reading an
    /// interface's DATA: `class_layout_of` answers `None` for an interface, so
    /// every field access and every `new` whose type is one is still rejected.
    pub(super) fn supported_type(&self, ty: &Type) -> bool {
        let mut open = HashSet::new();
        self.supported_type_inner(ty, &mut open)
    }

    fn supported_type_inner(&self, ty: &Type, open: &mut HashSet<String>) -> bool {
        match ty {
            Type::Array(elem) => {
                !matches!(**elem, Type::Void) && self.supported_type_inner(elem, open)
            }
            Type::Named(name) => {
                (self.is_interface)(name)
                    || self.supported_enum_inner(ty, open)
                    || self.supported_class_inner(name, open)
            }
            // The builtin collections (willow-0g8j.7) and instantiated generic
            // enums — `Option<T>`, `Result<T, E>`, a user generic enum
            // (willow-0g8j.2.1). `Task`, `Range`, `Future` and generic
            // *classes* remain outside, so it is `lir_collection` and
            // `supported_enum_inner` that decide, not the shape of the type.
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
                None => self.supported_enum_inner(ty, open),
            },
            // A function value is a bare code pointer — no environment, since
            // lambdas capture nothing (willow-0g8j.2.2). What has to hold is
            // that every type in the SIGNATURE is one the walker can pass and
            // receive, because an indirect call builds its Cranelift signature
            // from exactly this type.
            Type::Fn(params, ret) => {
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
    fn storable(&self, target: &Type, value: &Type) -> bool {
        assignable_repr(target, value)
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
        let (Type::Named(iface), Type::Named(class)) = (target, value) else {
            return false;
        };
        (self.is_interface)(iface) && self.supported_class(class) && (self.can_box)(class, iface)
    }

    /// A non-generic enum the walker can construct and match on, by NAME.
    /// Shorthand for [`Self::supported_enum_type`] on `Type::Named(name)`;
    /// eligibility itself always has the use-site type, so this is the form the
    /// name-level tests below are written against.
    #[cfg(test)]
    pub(super) fn supported_enum(&self, name: &str) -> bool {
        self.supported_enum_type(&Type::Named(name.to_string()))
    }

    /// An enum *instance* the walker can construct and match on: `ty` names a
    /// declared enum, supplies exactly its declared type arguments, and every
    /// payload — after instantiation — is itself supported (willow-0g8j.2.1).
    ///
    /// Generic enums are in the subset as of this slice, which is what admits
    /// `Option<T>` and `Result<T, E>`. Both representations are emittable: the
    /// ordinary `[tag | payload…]` heap object, and the `Option` pointer niche
    /// where `Some(x)` IS `x` and `None` is the null word. Eligibility
    /// deliberately does not care which — [`option_repr`] decides that at
    /// emission, from the same instantiated type vetted here, so the walker and
    /// the AST emitter always pick the same one.
    pub(super) fn supported_enum_type(&self, ty: &Type) -> bool {
        let mut open = HashSet::new();
        self.supported_enum_inner(ty, &mut open)
    }

    fn supported_enum_inner(&self, ty: &Type, open: &mut HashSet<String>) -> bool {
        let Some((_, def)) = self.enum_instance(ty) else {
            return false;
        };
        // A self- or mutually-referential payload (`enum List { Cons(i64,
        // List), Nil }`) is fine — the payload slot is one word either way —
        // but must not recurse forever. The key is the INSTANTIATED type, so
        // `enum List<T> { Cons(T, List<T>), Nil }` at `List<i64>` closes the
        // cycle on itself while `Option<Option<i64>>` still walks its inner
        // type. Same guard, and the same shared `open` set, as the class walk
        // below.
        if !open.insert(type_name(ty)) {
            return true;
        }
        def.variants.iter().all(|v| {
            v.payloads
                .iter()
                .all(|t| self.supported_type_inner(t, open))
        })
    }

    /// A class the walker can emit: it has a registered field layout, is not an
    /// interface or an enum, takes no part in inheritance, and every field type
    /// is itself supported.
    ///
    /// Inheritance is the hard exclusion. With no base and no subclass, a
    /// receiver's static type is *exact*, so a direct call to `Class__method`
    /// is the whole of dispatch; the moment a class is extended, the AST path's
    /// runtime `type_id` dispatch chain becomes load-bearing and the walker
    /// would silently call the wrong implementation.
    pub(super) fn supported_class(&self, name: &str) -> bool {
        let mut open = HashSet::new();
        self.supported_class_inner(name, &mut open)
    }

    fn supported_class_inner(&self, name: &str, open: &mut HashSet<String>) -> bool {
        if (self.is_interface)(name) || self.is_enum(name) {
            return false;
        }
        let Some(layout) = self.class_layouts.get(name) else {
            return false;
        };
        if self.participates_in_inheritance(name) {
            return false;
        }
        // A self- or mutually-referential field (`class Node { next: Node; }`)
        // is fine — it is the same layout — but must not recurse forever.
        if !open.insert(name.to_string()) {
            return true;
        }
        layout
            .iter()
            .all(|(_, ty)| self.supported_type_inner(ty, open))
    }

    /// Whether `name` is the base or the subclass of some `extends` edge, under
    /// ANY of its names.
    ///
    /// A direct type import (`import zoo::Animal;`) copies the imported class's
    /// tables under the unqualified `Animal`, but `class_base` keeps CANONICAL
    /// names on both sides of every edge (`zoo::Dog` → `zoo::Animal`). Comparing
    /// `"Animal"` to those strings finds nothing, so a plain name comparison
    /// would call an imported base class a leaf and let the walker emit a direct
    /// `Animal__speak` for a receiver that is really a `Dog` — the exact
    /// miscompile the inheritance exclusion exists to prevent. Aliased and
    /// canonical names share one runtime `type_id`, so compare that instead and
    /// fall back to the name when a class has no id registered.
    fn participates_in_inheritance(&self, name: &str) -> bool {
        let id = self.class_type_ids.get(name).copied();
        let is_same = |other: &str| {
            other == name || (id.is_some() && self.class_type_ids.get(other).copied() == id)
        };
        self.class_base
            .iter()
            .any(|(sub, base)| is_same(sub) || is_same(base))
    }

    /// The declared field layout of a supported class named by `ty`.
    fn class_layout_of(&self, ty: &Type) -> Option<&Vec<(String, Type)>> {
        let Type::Named(name) = ty else { return None };
        if !self.supported_class(name) {
            return None;
        }
        self.class_layouts.get(name)
    }

    /// Whether a direct call to the symbol `mangled` is emittable with `args`:
    /// the symbol exists, no parameter is by-reference (the walker passes
    /// values, never addresses), and every declared parameter type is supported
    /// and accepts its argument — directly or by boxing it into an interface.
    /// `skip_self` drops the hidden receiver parameter that class methods and
    /// static calls carry.
    /// The declared `fn(...) -> ...` type of a symbol that is about to be used
    /// as a VALUE (willow-0g8j.2.2), or `None` when it cannot be.
    ///
    /// Three things have to hold, and all three are about the pointer being
    /// callable later through a signature built from the type alone: the symbol
    /// is linkable, every parameter is passed by value (a by-reference
    /// parameter has no spelling in a `fn(...)` type, so the call site could not
    /// reproduce it), and the whole signature is inside the subset.
    fn fn_value_of(&self, mangled: &str) -> Option<Type> {
        if !(self.known_fn)(mangled) {
            return None;
        }
        if self
            .func_param_modes
            .get(mangled)
            .is_some_and(|modes| modes.iter().any(|m| !matches!(m, ParamMode::Value)))
        {
            return None;
        }
        let ty @ Type::Fn(..) = self.fn_types.get(mangled)?.clone() else {
            return None;
        };
        self.supported_type(&ty).then_some(ty)
    }

    fn callable(&self, mangled: &str, args: &[HirExpr], skip_self: bool) -> bool {
        if !(self.known_fn)(mangled) {
            return false;
        }
        if self
            .func_param_modes
            .get(mangled)
            .is_some_and(|modes| modes.iter().any(|m| !matches!(m, ParamMode::Value)))
        {
            return false;
        }
        let Some(Type::Fn(params, _)) = self.fn_types.get(mangled) else {
            // No recorded signature (a runtime symbol, or a shape the front end
            // did not register): only argument types whose representation the
            // walker cannot get wrong may reach it.
            return args.iter().all(|a| !matches!(a.ty, Type::Named(_)));
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
            && params
                .iter()
                .zip(args)
                .all(|(p, a)| self.supported_type(p) && self.storable(p, &a.ty))
    }
}

/// Conservative "can evaluating this run a collection?" test, used to decide
/// whether a live GC temporary has to be rooted across a sub-expression. Only
/// the forms known to be allocation-free answer `false`; everything else —
/// including any expression form added later — answers `true`, so a new node
/// kind cannot silently drop a root.
fn may_allocate(e: &HirExpr) -> bool {
    match &e.kind {
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) => false,
        HirExprKind::Var(_) => false,
        // Taking a function's address is a relocation, not an allocation: a
        // lambda is a lifted top-level function with no captured environment
        // to build (willow-0g8j.2.2).
        HirExprKind::FnRef(_) | HirExprKind::Lambda { .. } => false,
        HirExprKind::Unary { operand, .. } => may_allocate(operand),
        // String concatenation allocates; equality/inequality only compare
        // bytes and collect only if evaluating an operand can allocate.
        HirExprKind::Binary { op, lhs, rhs } => {
            (lhs.ty == Type::String && matches!(op, BinOp::Add))
                || may_allocate(lhs)
                || may_allocate(rhs)
        }
        HirExprKind::Ternary {
            condition,
            then_expr,
            else_expr,
        } => may_allocate(condition) || may_allocate(then_expr) || may_allocate(else_expr),
        // `willow_array_get` only reads through the handle.
        HirExprKind::Index { array, index } => may_allocate(array) || may_allocate(index),
        // A field read is a plain load at a fixed offset from a statically
        // non-optional class receiver (willow-0g8j.5).
        HirExprKind::FieldAccess { object, .. } => may_allocate(object),
        _ => true,
    }
}

/// Conservative eligibility: every type, instruction, and expression must be in
/// the supported subset, every callee must be a known symbol, every variable
/// must be a parameter or a `let` of this function, and binding names must be
/// unique across it (LIR flattens block scopes, so shadowing across sibling
/// scopes — or over a parameter — would alias one variable).
///
/// This is [`lir_rejection_reason`] with the reason discarded, so the decision
/// and the message it reports can never disagree. Codegen calls the reason form
/// directly (it has to print it), leaving this as the shorthand the eligibility
/// tests below are written against.
#[cfg(test)]
pub(super) fn lir_supported_function(f: &LirFunction, ctx: &LirTypeCtx<'_>) -> bool {
    lir_rejection_reason(f, ctx).is_none()
}

/// Why the walker will not compile `f`, phrased to read after "fell back to the
/// AST backend: " — `None` when the function IS in the subset.
///
/// The single source of truth for eligibility (see [`lir_supported_function`]).
/// Under `WILLOW_LIR_REQUIRE=1` this is what the compile error prints, which is
/// the difference between "something in this function is unsupported" and a
/// construct, a type and a line to go and fix.
pub(super) fn lir_rejection_reason(f: &LirFunction, ctx: &LirTypeCtx<'_>) -> Option<String> {
    // The one per-function field, taken from the function under test rather
    // than from the caller, so a `return` inside a `match` arm is checked
    // against this function's declared type and no caller can get it wrong.
    let ctx = &LirTypeCtx {
        return_type: &f.return_type,
        ..*ctx
    };
    if !ctx.supported_type(&f.return_type) {
        return Some(format!(
            "its return type `{}` is outside the walker's subset",
            type_name(&f.return_type)
        ));
    }
    for p in &f.params {
        // Reference parameters (`&`/`&mut`) are pointers at the ABI level.
        if p.by_reference {
            return Some(format!("parameter `{}` is taken by reference", p.name));
        }
        if !ctx.supported_type(&p.ty) {
            return Some(format!(
                "parameter `{}` has type `{}`, outside the walker's subset",
                p.name,
                type_name(&p.ty)
            ));
        }
    }
    // Names the walker can resolve, mapped to the type they are BOUND with (not
    // the initialiser's type — see `LirInst::Let::ty`). Any other `Var` is
    // something the HIR spells like a variable but codegen must special-case —
    // a bare enum variant, a function used as a value — so the function falls
    // back (willow-0g8j.1).
    let mut names: HashMap<&str, &Type> = HashMap::new();
    for p in &f.params {
        if names.insert(p.name.as_str(), &p.ty).is_some() {
            return Some(format!("parameter `{}` is declared twice", p.name));
        }
    }
    for block in &f.blocks {
        for inst in &block.instrs {
            if let LirInst::Let { name, ty, .. } = inst
                && names.insert(name.as_str(), ty).is_some()
            {
                return Some(format!(
                    "`let {name}` reuses a name already bound in this function; \
                     LIR's flat scopes cannot tell the two bindings apart"
                ));
            }
        }
    }

    for block in &f.blocks {
        for inst in &block.instrs {
            match inst {
                LirInst::Let {
                    name, ty, value, ..
                } => {
                    // The binding type is the slot's type, so an annotation
                    // that widens the initialiser (`let a: Animal = new Dog();`)
                    // is where the boxing coercion goes (willow-j260).
                    if !ctx.supported_type(ty) {
                        return Some(format!(
                            "`let {name}` binds type `{}`, outside the walker's subset",
                            type_name(ty)
                        ));
                    }
                    if !ctx.storable(ty, &value.ty) {
                        return Some(store_reason(
                            &format!("`let {name}`"),
                            &value.ty,
                            ty,
                            value.span,
                        ));
                    }
                    if let Some(reason) = expr_rejection(value, ctx, &names) {
                        return Some(reason);
                    }
                }
                LirInst::Assign { name, value } => {
                    let Some(declared) = names.get(name.as_str()) else {
                        return Some(format!(
                            "`{name} = ...` at line {} assigns to a name that is not a \
                             parameter or a `let` of this function",
                            value.span.line
                        ));
                    };
                    if !ctx.storable(declared, &value.ty) {
                        return Some(store_reason(
                            &format!("`{name} = ...`"),
                            &value.ty,
                            declared,
                            value.span,
                        ));
                    }
                    if let Some(reason) = expr_rejection(value, ctx, &names) {
                        return Some(reason);
                    }
                }
                LirInst::Expr(e) => {
                    // Statement position is one of the two places a diverging
                    // `panic(...)` is emittable: the walker ends the Cranelift
                    // block there and drops the rest of this LIR block, which
                    // is dead code (willow-0g8j.2.5).
                    if !supported_divergent_expr(e, ctx, &names)
                        && let Some(reason) = expr_rejection(e, ctx, &names)
                    {
                        return Some(reason);
                    }
                }
                LirInst::IndexAssign {
                    array,
                    index,
                    value,
                } => {
                    let Type::Array(elem) = &array.ty else {
                        return Some(format!(
                            "the element store at line {} targets a `{}`, which is not an array",
                            array.span.line,
                            type_name(&array.ty)
                        ));
                    };
                    if !ctx.storable(elem, &value.ty) {
                        return Some(store_reason(
                            "the element store",
                            &value.ty,
                            elem,
                            value.span,
                        ));
                    }
                    if let Some(reason) = expr_rejection(array, ctx, &names) {
                        return Some(reason);
                    }
                    if let Some(reason) = expr_rejection(index, ctx, &names) {
                        return Some(reason);
                    }
                    if let Some(reason) = expr_rejection(value, ctx, &names) {
                        return Some(reason);
                    }
                }
                // `obj.field = value;` on a simple class (willow-0g8j.5).
                LirInst::FieldAssign {
                    object,
                    field,
                    value,
                } => {
                    let Some(field_ty) = ctx
                        .class_layout_of(&object.ty)
                        .and_then(|l| l.iter().find(|(n, _)| n == field))
                        .map(|(_, ty)| ty.clone())
                    else {
                        return Some(format!(
                            "the field store `.{field} = ...` at line {} targets a `{}`, whose \
                             layout the walker does not have",
                            object.span.line,
                            type_name(&object.ty)
                        ));
                    };
                    if !ctx.storable(&field_ty, &value.ty) {
                        return Some(store_reason(
                            &format!("the field store `.{field} = ...`"),
                            &value.ty,
                            &field_ty,
                            value.span,
                        ));
                    }
                    if let Some(reason) = expr_rejection(object, ctx, &names) {
                        return Some(reason);
                    }
                    if let Some(reason) = expr_rejection(value, ctx, &names) {
                        return Some(reason);
                    }
                }
                // Listed rather than caught by `_` so a new instruction has to
                // be given a decision here instead of silently falling back.
                LirInst::EnterDeferScope { .. }
                | LirInst::LeaveDeferScope
                | LirInst::FlushDefers { .. }
                | LirInst::Defer { .. } => {
                    return Some("it registers a `defer`".to_string());
                }
                LirInst::StaticFieldAssign { class, field, .. } => {
                    return Some(format!(
                        "it writes the static property `{class}::{field}`, which the walker \
                         has no storage for"
                    ));
                }
                LirInst::SuperInit { .. } => {
                    return Some("it calls `super.init(...)`".to_string());
                }
            }
        }
        match &block.terminator {
            Terminator::Branch { cond, .. } => {
                if let Some(reason) = expr_rejection(cond, ctx, &names) {
                    return Some(reason);
                }
            }
            Terminator::Return(Some(v)) => {
                // A `void`-typed return value has no slot in the Cranelift
                // signature; the walker would emit `return_(&[v])` on a
                // zero-result function.
                if v.ty == Type::Void {
                    return Some(format!(
                        "the `return` at line {} yields a `void` value, which has no slot in \
                         the function's signature",
                        v.span.line
                    ));
                }
                if !ctx.storable(&f.return_type, &v.ty) {
                    return Some(store_reason("the `return`", &v.ty, &f.return_type, v.span));
                }
                if let Some(reason) = expr_rejection(v, ctx, &names) {
                    return Some(reason);
                }
            }
            Terminator::Jump(_) | Terminator::Return(None) => {}
        }
    }
    None
}

/// The message for a value whose type the walker will not convert into the slot
/// it is being written to. Shared so every store site words it the same way.
fn store_reason(what: &str, from: &Type, to: &Type, span: Span) -> String {
    format!(
        "{what} at line {} puts a `{}` into a `{}` slot, a conversion the walker does not emit",
        span.line,
        type_name(from),
        type_name(to)
    )
}

/// `None` when `e` is in the subset, otherwise a reason naming the SMALLEST
/// sub-expression that is not. `supported_expr` is the oracle for the decision;
/// this only walks down to find where it first goes wrong, so the two can never
/// disagree about whether the expression is eligible.
fn expr_rejection<'e>(
    e: &'e HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'e str, &'e Type>,
) -> Option<String> {
    let node = minimal_unsupported_expr(e, ctx, names)?;
    let what = describe_expr(node);
    let line = node.span.line;
    // `is_fresh_empty_map` is the one node whose own type is unsupported yet
    // which the walker still emits, so blaming its type would be wrong.
    if !ctx.supported_type(&node.ty) && !is_fresh_empty_map(node) {
        Some(format!(
            "{what} at line {line} has type `{}`, outside the walker's subset",
            type_name(&node.ty)
        ))
    } else {
        Some(format!(
            "{what} at line {line} is outside the walker's subset"
        ))
    }
}

/// The deepest sub-expression of `e` that `supported_expr` rejects, or `None`
/// if it accepts `e`. Descending stops at the two kinds that carry their own
/// binding scope: inside a lambda body, or inside an arm whose pattern the
/// walker cannot bind, `names` does not describe what is actually in scope, so
/// a child would be blamed for a name the walker simply never sees.
fn minimal_unsupported_expr<'e>(
    e: &'e HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'e str, &'e Type>,
) -> Option<&'e HirExpr> {
    if supported_expr(e, ctx, names) {
        return None;
    }
    match &e.kind {
        HirExprKind::Lambda { .. } => Some(e),
        HirExprKind::Match { scrutinee, arms } => {
            if let Some(inner) = minimal_unsupported_expr(scrutinee, ctx, names) {
                return Some(inner);
            }
            for arm in arms {
                let mut arm_names = names.clone();
                if !supported_pattern(&arm.pattern, &scrutinee.ty, ctx, &mut arm_names) {
                    continue;
                }
                // A diverging arm is admissible on its own terms, so it must
                // not be reported as the reason a match around it failed.
                if arm_diverges(arm) && supported_divergent_body(&arm.body, ctx, &arm_names) {
                    continue;
                }
                for child in arm.body.iter().flat_map(HirStmt::child_exprs) {
                    if let Some(inner) = minimal_unsupported_expr(child, ctx, &arm_names) {
                        return Some(inner);
                    }
                }
            }
            Some(e)
        }
        _ => e
            .children()
            .into_iter()
            .find_map(|child| minimal_unsupported_expr(child, ctx, names))
            .or(Some(e)),
    }
}

/// How to name an expression in a fallback reason. Deliberately short: the line
/// number locates it, this says what to look for on that line.
fn describe_expr(e: &HirExpr) -> String {
    match &e.kind {
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) => {
            "a literal".to_string()
        }
        HirExprKind::Str(_) => "a string literal".to_string(),
        HirExprKind::Var(name) => format!("the variable `{name}`"),
        HirExprKind::FnRef(name) => format!("the function value `{name}`"),
        HirExprKind::Binary { op, .. } => format!("the `{}` operator", binop_str(op)),
        HirExprKind::Unary { .. } => "a unary operator".to_string(),
        HirExprKind::Call { callee, .. } => format!("the call to `{callee}`"),
        HirExprKind::Print { .. } => "a `print`".to_string(),
        HirExprKind::Array { .. } => "an array literal".to_string(),
        HirExprKind::Index { .. } => "an index read".to_string(),
        HirExprKind::Ternary { .. } => "a `?:` expression".to_string(),
        HirExprKind::New { class, .. } => format!("`new {class}`"),
        HirExprKind::FieldAccess { object, field } => {
            format!("the field read `.{field}` on a `{}`", type_name(&object.ty))
        }
        HirExprKind::MethodCall { object, method, .. } => {
            format!("the method `{method}` on a `{}`", type_name(&object.ty))
        }
        HirExprKind::ObjectLiteral { class, .. } => {
            format!("the object literal `{class} {{ .. }}`")
        }
        HirExprKind::StaticField { class, field } => {
            format!("the static property `{class}::{field}`")
        }
        HirExprKind::StaticCall { class, method, .. } => {
            format!("the static call `{class}::{method}`")
        }
        HirExprKind::Range { .. } => "a range".to_string(),
        HirExprKind::Await { .. } => "an `await`".to_string(),
        HirExprKind::TryPropagate { .. } => "a `?` propagation".to_string(),
        HirExprKind::Lambda { .. } => "a lambda".to_string(),
        HirExprKind::Match { scrutinee, .. } => {
            format!("the `match` on a `{}`", type_name(&scrutinee.ty))
        }
    }
}

/// Whether the walker can both TEST `pattern` against a `scrutinee_ty` value
/// and bind whatever it destructures (willow-0g8j.8). On success the pattern's
/// bindings are added to `names`, which is why this takes the map by `&mut`:
/// the arm body is checked against the extended scope, exactly as the emitter
/// will run it. On failure `names` may have been partly extended, so callers
/// pass a throwaway clone rather than their own map.
///
/// `ClassDowncast` is deliberately absent: it needs the interface dispatch
/// metadata the AST path carries, not a tag compare.
fn supported_pattern<'n>(
    pattern: &'n HirPattern,
    scrutinee_ty: &Type,
    ctx: &LirTypeCtx<'_>,
    names: &mut HashMap<&'n str, &'n Type>,
) -> bool {
    // The variant lookup every enum pattern needs: the scrutinee must BE this
    // enum (a pattern naming another enum is a checker bug, not something to
    // emit a tag compare for) and the enum must be one the walker admits.
    let variant_of = |enum_name: &str, variant: &str| {
        if !ctx.supported_enum_type(scrutinee_ty) {
            return None;
        }
        // The payloads come from the SCRUTINEE's type, so a generic enum's
        // placeholders are already resolved to the types this `match` will
        // actually load (willow-0g8j.2.1).
        let (name, def) = ctx.enum_instance(scrutinee_ty)?;
        if name != enum_name {
            return None;
        }
        def.variant(variant).cloned()
    };
    match pattern {
        HirPattern::Wildcard => true,
        HirPattern::Binding { name, ty } => {
            // The binding aliases the whole scrutinee, so it must hold the same
            // machine representation — no widening, no boxing.
            if !assignable_repr(ty, scrutinee_ty) || !ctx.supported_type(ty) {
                return false;
            }
            names.insert(name.as_str(), ty);
            true
        }
        HirPattern::LiteralBool(_) => *scrutinee_ty == Type::Bool,
        HirPattern::LiteralInt(_) => *scrutinee_ty == Type::I64,
        HirPattern::EnumVariant { enum_name, variant } => {
            variant_of(enum_name, variant).is_some_and(|v| v.payloads.is_empty())
        }
        HirPattern::EnumVariantTuple {
            enum_name,
            variant,
            bindings,
        } => {
            let Some(v) = variant_of(enum_name, variant) else {
                return false;
            };
            if v.payloads.len() != bindings.len() {
                return false;
            }
            // A payload is LOADED into the binding, so the binding's type has
            // to match the slot's representation rather than merely be
            // storable into it.
            for (slot, (name, ty)) in v.payloads.iter().zip(bindings) {
                if !assignable_repr(ty, slot) || !ctx.supported_type(ty) {
                    return false;
                }
                names.insert(name.as_str(), ty);
            }
            true
        }
        HirPattern::ClassDowncast { .. } => false,
    }
}

/// The result type of a builtin `Option`/`Result` method the walker emits, or
/// `None` when the receiver/method pair is outside the subset (willow-0g8j.2.1).
///
/// A `void` payload is excluded throughout because the unwrap family would read
/// a payload slot that an all-`void` variant does not allocate, and the
/// callable-taking combinators would build one.
///
/// The combinators (`map`/`map_err`/`and_then`/`or_else`) came with function
/// values in willow-0g8j.2.2. Each rule below reconstructs the type the SHARED
/// emitter actually builds; the caller then compares it against the checker's
/// own type for the expression, so a disagreement falls back to the AST path
/// instead of being reinterpreted here.
///
/// `args` are the call's argument types, so arity and the default/message
/// operand are vetted here rather than at two call sites.
/// The return type of a `fn(...) -> R` value. Eligibility has already proved
/// every combinator operand is one, so a non-function here is a compiler bug.
fn fn_return_type(f_ty: &Type) -> Type {
    match f_ty {
        Type::Fn(_, ret) => (**ret).clone(),
        _ => unreachable!("combinator operand vetted by eligibility is a function"),
    }
}

/// The interpolated operands of a `format(spec, ..)` call, or `None` when the
/// call does not have the shape the emitter assumes.
///
/// The spec is re-parsed here rather than trusted, and each operand is checked
/// against its own placeholder, for one reason: the emitter walks SEGMENTS and
/// pulls an operand per placeholder, so a spec and an argument list that
/// disagree would silently render the wrong argument — or render a GC pointer
/// through `willow_i64_to_string`. The checker enforces the same three rules
/// (E1401/E0201), so this rejects only synthesized nodes and keeps the walker
/// from having to trust them.
fn format_operands(args: &[HirExpr]) -> Option<&[HirExpr]> {
    let HirExprKind::Str(spec) = &args.first()?.kind else {
        return None;
    };
    let segments = crate::interpolate::parse_spec(spec).ok()?;
    let placeholders: Vec<_> = segments
        .iter()
        .filter(|s| !matches!(s, crate::interpolate::Segment::Literal(_)))
        .collect();
    let operands = &args[1..];
    if placeholders.len() != operands.len() {
        return None;
    }
    let renderable = placeholders.iter().zip(operands).all(|(seg, a)| match seg {
        // A precision placeholder passes its operand straight to an f64
        // formatting symbol, so nothing else may reach it.
        crate::interpolate::Segment::F64(_) => a.ty == Type::F64,
        _ => matches!(a.ty, Type::I64 | Type::F64 | Type::Bool | Type::String),
    });
    renderable.then_some(operands)
}

/// Whether `e` is a `panic(...)` the walker may emit, with a message shape it
/// can assemble (willow-0g8j.2.5).
///
/// Divergence is a property of the POSITION, not of the expression: the emitter
/// ends the current Cranelift block with a `trap`, so a `panic` nested inside
/// an operand would strand the instructions that consume its value after a
/// terminator. Callers therefore ask this only where nothing else follows in
/// the same block — a whole statement, or a whole `match` arm.
fn supported_panic<'e>(
    e: &'e HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'e str, &'e Type>,
) -> bool {
    let HirExprKind::Call { callee, args } = &e.kind else {
        return false;
    };
    // A local binding of the same name is an ordinary indirect call, not the
    // builtin, and it wins here exactly as it does in `supported_expr`.
    if callee != "panic" || e.ty != Type::Never || names.contains_key(callee.as_str()) {
        return false;
    }
    // The three message shapes the AST emitter accepts: none (it substitutes a
    // default literal), one `String`, or a literal spec plus its operands.
    match args.len() {
        0 => true,
        1 => args[0].ty == Type::String && supported_expr(&args[0], ctx, names),
        _ => format_operands(args)
            .is_some_and(|operands| operands.iter().all(|a| supported_expr(a, ctx, names))),
    }
}

/// The result type of a scalar `toString()`, or `None` when this receiver and
/// method are not one — the walker's only entry point into the intrinsic table.
///
/// The four scalar conversions are the whole set with a primitive receiver, and
/// they are matched by INTRINSIC rather than by name so that a future builtin
/// on `i64` does not silently inherit this lowering.
fn scalar_to_string(recv: &Type, method: &str, args: &[HirExpr]) -> Option<Type> {
    let resolved = intrinsics::resolve(recv, method, args.len())?;
    matches!(
        resolved.intrinsic,
        Intrinsic::I64ToString
            | Intrinsic::F64ToString
            | Intrinsic::BoolToString
            | Intrinsic::StringToString
    )
    .then(|| resolved.return_type(|i| args.get(i).map(|a| a.ty.clone())))
}

fn option_result_method(recv: &Type, method: &str, args: &[Type]) -> Option<Type> {
    let resolved = builtin_types::resolve(recv)?;
    let payload = |i: usize| -> Option<Type> {
        resolved
            .args
            .get(i)
            .filter(|t| !matches!(t, Type::Void))
            .cloned()
    };
    let no_args = |t: Type| args.is_empty().then_some(t);
    // `expect(msg)` takes exactly one `String`; `unwrap_or(default)` takes
    // exactly one value of the payload type. The AST emitter passes both
    // straight through with no coercion, so `assignable_repr` is the rule.
    let one_string = |t: Type| matches!(args, [Type::String]).then_some(t);
    let one_of = |t: Type| matches!(args, [a] if assignable_repr(&t, a)).then_some(t);
    // The single callable operand, given the parameter list the emitter feeds
    // it. Yields the callable's return type, which is what every combinator's
    // result is built from.
    let one_fn = |params: &[Type]| -> Option<Type> {
        match args {
            [Type::Fn(ps, ret)] if ps.as_slice() == params => Some((**ret).clone()),
            _ => None,
        }
    };
    let non_void = |t: Type| (!matches!(t, Type::Void)).then_some(t);
    let option_of = |t: Type| Type::Generic("Option".to_string(), vec![t]);
    let result_of = |ok: Type, err: Type| Type::Generic("Result".to_string(), vec![ok, err]);
    match resolved.id {
        B::Option => match method {
            "is_some" | "is_none" => no_args(Type::Bool),
            "unwrap" => no_args(payload(0)?),
            "expect" => one_string(payload(0)?),
            "unwrap_or" => one_of(payload(0)?),
            // `Option<T>::map(fn(T) -> U) -> Option<U>`.
            "map" => Some(option_of(non_void(one_fn(&[payload(0)?])?)?)),
            // `Option<T>::and_then(fn(T) -> Option<U>) -> Option<U>`: the
            // emitter reads `U` out of the callable's return type to build the
            // `None` arm, so that return type IS the result.
            "and_then" => {
                let produced = one_fn(&[payload(0)?])?;
                builtin_types::unary_arg(&produced, B::Option)
                    .filter(|u| !matches!(u, Type::Void))?;
                Some(produced)
            }
            // `Option<T>::or_else(fn() -> Option<T>) -> Option<T>`: the `Some`
            // arm passes the RECEIVER through, so the callable must produce the
            // receiver's payload type — that payload is what picks the pointer
            // niche over the box, so both arms merge one representation.
            "or_else" => {
                let produced = one_fn(&[])?;
                (builtin_types::unary_arg(&produced, B::Option) == Some(&payload(0)?))
                    .then_some(produced)
            }
            _ => None,
        },
        B::Result => match method {
            "is_ok" | "is_err" => no_args(Type::Bool),
            "unwrap" => no_args(payload(0)?),
            "unwrap_err" => no_args(payload(1)?),
            "expect" => one_string(payload(0)?),
            "unwrap_or" => one_of(payload(0)?),
            // `Result<T, E>::map(fn(T) -> U) -> Result<U, E>`; the `Err` arm
            // passes the receiver through, so `E` is unchanged.
            "map" => Some(result_of(non_void(one_fn(&[payload(0)?])?)?, payload(1)?)),
            // `Result<T, E>::map_err(fn(E) -> F) -> Result<T, F>`.
            "map_err" => Some(result_of(payload(0)?, non_void(one_fn(&[payload(1)?])?)?)),
            // `Result<T, E>::and_then(fn(T) -> Result<U, E>) -> Result<U, E>`:
            // the `Err` arm returns the receiver unchanged, so the result keeps
            // the receiver's error type. The callable's own error type is NOT
            // constrained — a lambda ending in `Result::Ok(0)` records
            // `Result<i64, void>` — and it does not have to be, because every
            // `Result` is the same two-word box whatever its type arguments.
            // `Result<T, E>::and_then(fn(T) -> Result<U, E>) -> Result<U, E>`.
            // The result is the CALLABLE's return type, which is also what the
            // checker records: the `Err` arm passes the receiver through, and
            // that is representation-safe because every `Result` is the same
            // two-word box whatever its type arguments. The callable's error
            // type is deliberately unconstrained — a lambda ending in
            // `Result::Ok(0)` records `Result<i64, void>`.
            "and_then" => {
                payload(1)?;
                let produced = one_fn(&[payload(0)?])?;
                let (ok, _) = builtin_types::binary_args(&produced, B::Result)?;
                non_void(ok.clone()).map(|_| produced)
            }
            // `Result<T, E>::or_else(fn(E) -> Result<T, F>) -> Result<T, F>`:
            // the mirror image — the `Ok` arm passes the receiver through, so
            // only the ok payload has to line up.
            "or_else" => {
                let produced = one_fn(&[payload(1)?])?;
                let (ok, _) = builtin_types::binary_args(&produced, B::Result)?;
                (*ok == payload(0)?).then_some(produced)
            }
            _ => None,
        },
        _ => None,
    }
}

/// The `match` rule, shared by the value form (through [`supported_expr`]) and
/// the diverging form (through [`supported_divergent_expr`]).
///
/// `e` is the match expression itself; its type is what every arm that reaches
/// the merge block must produce.
fn supported_match<'n>(
    e: &'n HirExpr,
    scrutinee: &'n HirExpr,
    arms: &'n [HirMatchArm],
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, &'n Type>,
) -> bool {
    // An arm-less match has no value to produce and the checker should
    // have rejected it; refusing here keeps the emitter's "seed the
    // result variable, then every arm overwrites it" invariant honest.
    if arms.is_empty() || !supported_expr(scrutinee, ctx, names) {
        return false;
    }
    // The three scrutinee shapes the walker can test. A `String`,
    // class or interface scrutinee needs content comparison or a
    // downcast the pattern emitter below does not have.
    let scrutinee_ok = match &scrutinee.ty {
        Type::I64 | Type::Bool => true,
        // Any enum instance, generic or not: `Color`, `Option<i64>`,
        // `Result<i64, String>` (willow-0g8j.2.1).
        Type::Named(_) | Type::Generic(..) => ctx.supported_enum_type(&scrutinee.ty),
        _ => false,
    };
    if !scrutinee_ok {
        return false;
    }
    // A match that produces a value needs at least one arm that reaches the
    // merge block, where the value is read. When EVERY arm diverges the match
    // itself is typed `!`, and it is admissible only in the positions
    // [`supported_divergent_expr`] is asked about.
    if e.ty != Type::Never && arms.iter().all(arm_diverges) {
        return false;
    }
    arms.iter().all(|arm| {
        let mut arm_names = names.clone();
        if !supported_pattern(&arm.pattern, &scrutinee.ty, ctx, &mut arm_names) {
            return false;
        }
        if arm_diverges(arm) {
            // A diverging arm hands nothing to the merge block, so the
            // representation agreement below is not asked of it.
            return supported_divergent_body(&arm.body, ctx, &arm_names);
        }
        // A block-bodied arm can `break` or declare a local, which are
        // statement forms the walker has no emitter for in expression
        // position. Only a single-expression arm produces a value.
        let [HirStmt::Expr(value)] = arm.body.as_slice() else {
            return false;
        };
        assignable_repr(&e.ty, &value.ty) && supported_expr(value, ctx, &arm_names)
    })
}

/// Shallow: this arm ends by leaving the function or by unwinding, so it hands
/// no value to the merge block. `!` is the type of every such tail expression,
/// and a `return` is one syntactically.
fn arm_diverges(arm: &HirMatchArm) -> bool {
    match arm.body.last() {
        Some(HirStmt::Return { .. }) => true,
        Some(HirStmt::Expr(e)) => e.ty == Type::Never,
        _ => false,
    }
}

/// Whether the walker can emit `body` where nothing follows it in the same
/// Cranelift block: a run of ordinary effect statements ending in something
/// that leaves — `return`, `panic(...)`, or a `match` all of whose arms do
/// (willow-0g8j.2.5).
fn supported_divergent_body<'n>(
    body: &'n [HirStmt],
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, &'n Type>,
) -> bool {
    let Some((last, leading)) = body.split_last() else {
        return false;
    };
    // Only effect statements may precede the tail. A `let` would bind a name
    // the flat `vars` map cannot scope to this arm, and any other statement
    // form would need its own emitter here. `supported_expr` refuses a
    // `!`-typed expression, so a leading statement cannot itself diverge.
    let leading_ok = leading
        .iter()
        .all(|s| matches!(s, HirStmt::Expr(e) if supported_expr(e, ctx, names)));
    let last_ok = match last {
        HirStmt::Return { value: None, .. } => true,
        HirStmt::Return {
            value: Some(value), ..
        } => ctx.storable(ctx.return_type, &value.ty) && supported_expr(value, ctx, names),
        HirStmt::Expr(e) => supported_divergent_expr(e, ctx, names),
        _ => false,
    };
    leading_ok && last_ok
}

/// An expression the walker emits for its EFFECT in a position where nothing
/// follows it in the same Cranelift block — a whole statement, or the tail of a
/// match arm. Both forms end the block, which is why the position matters:
/// nested in an operand, either would strand the instructions that consume its
/// value after a terminator.
fn supported_divergent_expr<'n>(
    e: &'n HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, &'n Type>,
) -> bool {
    if supported_panic(e, ctx, names) {
        return true;
    }
    match &e.kind {
        HirExprKind::Match { scrutinee, arms } if e.ty == Type::Never => {
            supported_match(e, scrutinee, arms, ctx, names)
        }
        _ => false,
    }
}

/// `'n` ties the borrowed names to the expression tree being vetted, so a
/// `match` arm can extend the map with its own pattern bindings — which live in
/// that same tree — before checking the arm body (willow-0g8j.8).
fn supported_expr<'n>(
    e: &'n HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, &'n Type>,
) -> bool {
    if !ctx.supported_type(&e.ty) && !is_fresh_empty_map(e) {
        return false;
    }
    match &e.kind {
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) => true,
        HirExprKind::Str(_) => true,
        HirExprKind::Var(name) => names.contains_key(name.as_str()),
        // A named function used as a value (willow-0g8j.2.2). The declared
        // signature must be the type this expression carries: the pointer is
        // later CALLED through a Cranelift signature built from `e.ty`, so a
        // disagreement would call the target under the wrong ABI.
        HirExprKind::FnRef(name) => ctx.fn_value_of(name).is_some_and(|ty| ty == e.ty),
        // A lambda evaluates to the address of its lifted function, so its BODY
        // is not this function's problem — it is compiled under its own symbol
        // and vetted on its own terms. What is checked here is the same thing
        // as for a named function: the symbol exists and its declared signature
        // is the type the expression carries.
        HirExprKind::Lambda { .. } => (ctx.lambda_symbol)(e.span)
            .and_then(|sym| ctx.fn_value_of(&sym))
            .is_some_and(|ty| ty == e.ty),
        HirExprKind::Binary { op, lhs, rhs } => {
            // On strings only `+` (concat) and content comparison are emitted.
            if lhs.ty == Type::String && !matches!(op, BinOp::Add | BinOp::Eq | BinOp::Ne) {
                return false;
            }
            if matches!(op, BinOp::Pow)
                && !matches!(
                    (&lhs.ty, &rhs.ty),
                    (Type::I64, Type::I64) | (Type::F64, Type::F64)
                )
            {
                return false;
            }
            // Class values have no operators: `==` on two objects would be an
            // identity comparison the walker does not emit.
            if matches!(lhs.ty, Type::Named(_)) || matches!(rhs.ty, Type::Named(_)) {
                return false;
            }
            supported_expr(lhs, ctx, names) && supported_expr(rhs, ctx, names)
        }
        // Both arms feed one Cranelift variable, and the walker inserts no
        // conversion between them — a `cond ? new Dog() : new Cat()` typed
        // `Animal` would define that variable with two raw class pointers.
        HirExprKind::Ternary {
            condition,
            then_expr,
            else_expr,
        } => {
            assignable_repr(&e.ty, &then_expr.ty)
                && assignable_repr(&e.ty, &else_expr.ty)
                && supported_expr(condition, ctx, names)
                && supported_expr(then_expr, ctx, names)
                && supported_expr(else_expr, ctx, names)
        }
        // `match` as an expression (willow-0g8j.8). Every arm feeds one
        // Cranelift variable, so the same no-conversion rule `Ternary` states
        // applies to each arm body.
        HirExprKind::Match { scrutinee, arms } => supported_match(e, scrutinee, arms, ctx, names),
        HirExprKind::Unary { operand, .. } => supported_expr(operand, ctx, names),
        HirExprKind::Call { callee, args } => {
            // HIR spells direct and indirect calls with the same node, and a
            // local fn-typed binding shadows a free function (willow-bv9.1).
            // The local wins here for the same reason it wins in the AST
            // emitter: whichever this resolves to is what the type checker
            // checked the call against.
            if let Some(local) = names.get(callee.as_str()) {
                let Type::Fn(params, ret) = local else {
                    return false;
                };
                return assignable_repr(ret, &e.ty)
                    && params.len() == args.len()
                    && params
                        .iter()
                        .zip(args)
                        .all(|(p, a)| ctx.supported_type(p) && ctx.storable(p, &a.ty))
                    && args.iter().all(|a| supported_expr(a, ctx, names));
            }
            // `format` is variadic and has no function symbol: it assembles a
            // string from a literal spec at the call site (willow-0g8j.2.5).
            if callee == "format" {
                return e.ty == Type::String
                    && format_operands(args).is_some_and(|operands| {
                        operands.iter().all(|a| supported_expr(a, ctx, names))
                    });
            }
            // These compiler-known control-flow operations require the AST
            // backend's lexical panic-scope metadata (willow-s9ej.3).
            !matches!(callee.as_str(), "panic" | "recover")
                && ctx.callable(callee.as_str(), args, false)
                && args.iter().all(|a| supported_expr(a, ctx, names))
        }
        HirExprKind::Print { value, newline: _ } => {
            (scalar(&value.ty) || value.ty == Type::String) && supported_expr(value, ctx, names)
        }
        // The element type is already vetted by `supported_type(&e.ty)` above.
        HirExprKind::Array { elements } => {
            let elem = array_element_type(&e.ty);
            elements
                .iter()
                .all(|el| ctx.storable(&elem, &el.ty) && supported_expr(el, ctx, names))
        }
        // `Array<T>` and `FrozenArray<T>` are the same runtime handle, so both
        // index through `willow_array_get` (willow-0g8j.7). `Range<i64>` also
        // spells a read this way but is not a handle at all, so it stays out.
        HirExprKind::Index { array, index } => {
            let indexable = matches!(array.ty, Type::Array(_))
                || matches!(
                    lir_collection(&array.ty),
                    Some((LirCollection::FrozenArray, _))
                );
            indexable
                && assignable_repr(&array_element_type(&array.ty), &e.ty)
                && supported_expr(array, ctx, names)
                && supported_expr(index, ctx, names)
        }
        // `new Class(args)` — explicit `Class__init` or the implicit memberwise
        // constructor (willow-0g8j.5).
        HirExprKind::New { class, args } => {
            let Some(layout) = ctx.class_layout_of(&e.ty) else {
                return false;
            };
            if !matches!(&e.ty, Type::Named(n) if n == class) {
                return false;
            }
            let mangled = class_method_symbol_name(ctx.known_modules, class, "init");
            let shape_ok = if (ctx.known_fn)(&mangled) {
                ctx.callable(&mangled, args, true)
            } else {
                // Memberwise: positional args fill the declared fields in order.
                args.len() == layout.len()
                    && layout
                        .iter()
                        .zip(args)
                        .all(|((_, fty), a)| ctx.storable(fty, &a.ty))
            };
            shape_ok && args.iter().all(|a| supported_expr(a, ctx, names))
        }
        // `Class { field: value, ... }` — the given names must be exactly the
        // declared fields, each once. Matching only the COUNT would accept
        // `Point { x: 1, x: 2 }`, which the emitter would store twice into `x`
        // and leave `y` at its zero value.
        //
        // The type checker rejects this syntax in source today (E0847), so this
        // arm only guards the node's internal use; the check stays exact so the
        // predicate and the emitter cannot drift apart if it comes back.
        HirExprKind::ObjectLiteral { class, fields } => {
            let Some(layout) = ctx.class_layout_of(&e.ty) else {
                return false;
            };
            if !matches!(&e.ty, Type::Named(n) if n == class) || fields.len() != layout.len() {
                return false;
            }
            let mut seen: HashSet<&str> = HashSet::new();
            fields.iter().all(|(name, value)| {
                seen.insert(name.as_str())
                    && layout
                        .iter()
                        .find(|(n, _)| n == name)
                        .is_some_and(|(_, fty)| ctx.storable(fty, &value.ty))
                    && supported_expr(value, ctx, names)
            })
        }
        // `object.field` on a simple class. `Range<i64>.start/.end` and every
        // other receiver stay on the AST path.
        HirExprKind::FieldAccess { object, field } => {
            ctx.class_layout_of(&object.ty)
                .and_then(|l| l.iter().find(|(n, _)| n == field))
                .is_some_and(|(_, fty)| assignable_repr(fty, &e.ty))
                && supported_expr(object, ctx, names)
        }
        // The builtin array methods the walker emits, plus a direct call to a
        // method of a simple class. Anything else on an array (`freeze`,
        // `map`, …) and every other receiver falls back.
        HirExprKind::MethodCall {
            object,
            method,
            args,
        } => match &object.ty {
            Type::Array(elem) => {
                let shape_ok = match method.as_str() {
                    "len" | "pop" => args.is_empty(),
                    "push" => args.len() == 1 && ctx.storable(elem, &args[0].ty),
                    // `toString` renders elements in the runtime, which only
                    // knows the four scalar/string element kinds.
                    "toString" => args.is_empty() && collection_elem_kind(elem).is_some(),
                    // `freeze` copies the handle into a `FrozenArray<T>` over
                    // the SAME element type. The result type is checked rather
                    // than assumed: the emitter returns the copy unchanged, so
                    // a different element type would be a reinterpretation.
                    "freeze" => {
                        args.is_empty()
                            && matches!(lir_collection(&e.ty), Some((LirCollection::FrozenArray, a))
                                if a.as_slice() == std::slice::from_ref(&**elem))
                    }
                    _ => false,
                };
                shape_ok
                    && supported_expr(object, ctx, names)
                    && args.iter().all(|a| supported_expr(a, ctx, names))
            }
            // The value-taking `Option`/`Result` methods (willow-0g8j.2.1).
            // Checked before the collection arm because both receivers are
            // `Type::Generic`; `lir_collection` never matches an enum, so the
            // two sets cannot overlap.
            Type::Generic(..) if ctx.supported_enum_type(&object.ty) => {
                let arg_tys: Vec<Type> = args.iter().map(|a| a.ty.clone()).collect();
                option_result_method(&object.ty, method, &arg_tys)
                    .is_some_and(|ret| assignable_repr(&ret, &e.ty))
                    && supported_expr(object, ctx, names)
                    && args.iter().all(|a| supported_expr(a, ctx, names))
            }
            // The builtin collections. `get` yields an `Option<V>`, which the
            // walker represents as of willow-0g8j.2.1.
            Type::Generic(..) => {
                let Some((kind, targs)) = lir_collection(&object.ty) else {
                    return false;
                };
                let shape_ok = match (kind, method.as_str()) {
                    // A frozen array answers `len` and nothing else; its reads
                    // are `Index`, handled above.
                    (LirCollection::FrozenArray, "len") => args.is_empty() && e.ty == Type::I64,
                    (LirCollection::Map | LirCollection::FrozenMap, "len") => {
                        args.is_empty() && e.ty == Type::I64
                    }
                    (LirCollection::Map | LirCollection::FrozenMap, "contains") => {
                        args.len() == 1
                            && e.ty == Type::Bool
                            && assignable_repr(&targs[0], &args[0].ty)
                    }
                    // `get` yields `Option<V>` over the map's OWN value type —
                    // checked rather than assumed, because the runtime picks
                    // the option representation from that type and the walker
                    // passes the choice across the ABI (willow-0g8j.2.1).
                    (LirCollection::Map | LirCollection::FrozenMap, "get") => {
                        args.len() == 1
                            && assignable_repr(&targs[0], &args[0].ty)
                            && matches!(&e.ty, Type::Generic(..)
                                if builtin_types::unary_arg(&e.ty, B::Option) == Some(&targs[1]))
                            && ctx.supported_type(&e.ty)
                    }
                    (LirCollection::Map, "insert") => {
                        args.len() == 2
                            && assignable_repr(&targs[0], &args[0].ty)
                            && ctx.storable(&targs[1], &args[1].ty)
                    }
                    // Rendered in the runtime, which knows only the four
                    // scalar/string value kinds — the AST path passes `0` for
                    // anything else, which would render a pointer as an `i64`.
                    (LirCollection::Map, "toString") => {
                        args.is_empty()
                            && e.ty == Type::String
                            && collection_elem_kind(&targs[1]).is_some()
                    }
                    // `freeze` copies into a `FrozenMap<K, V>` over the same
                    // pair, for the same reason `Array::freeze` checks its own.
                    (LirCollection::Map, "freeze") => {
                        args.is_empty()
                            && matches!(lir_collection(&e.ty), Some((LirCollection::FrozenMap, a))
                                if a == targs)
                    }
                    _ => false,
                };
                shape_ok
                    && supported_expr(object, ctx, names)
                    && args.iter().all(|a| supported_expr(a, ctx, names))
            }
            // Virtual dispatch through an interface box (willow-0g8j.6). The
            // receiver's box carries the vtable, so — unlike a class receiver —
            // this needs no knowledge of the concrete class, only that the
            // interface really declares the method at a known slot.
            Type::Named(iface) if (ctx.is_interface)(iface) => {
                let Some(sig) = (ctx.iface_method)(iface, method) else {
                    return false;
                };
                // A `Self`-returning method yields a concrete object of the
                // receiver's own class, which the emitter re-boxes with the
                // receiver's vtable — so the result is the receiver's interface
                // and nothing else.
                let ret_ok = if matches!(&sig.ret, Type::Named(n) if n == "Self") {
                    matches!(&e.ty, Type::Named(n) if n == iface)
                } else {
                    assignable_repr(&sig.ret, &e.ty)
                };
                ret_ok
                    && sig.params.len() == args.len()
                    // A `&`/`&mut` parameter arrives as a pointer to the
                    // caller's place. The walker only ever passes values, so
                    // admitting one would pass an integer the callee
                    // dereferences — `callable` rejects direct calls with
                    // reference parameters for the same reason (willow-0g8j.9).
                    && sig.modes.iter().all(|m| matches!(m, ParamMode::Value))
                    && sig.params.iter().all(|p| ctx.supported_type(p))
                    && sig
                        .params
                        .iter()
                        .zip(args)
                        .all(|(p, a)| ctx.storable(p, &a.ty))
                    && supported_expr(object, ctx, names)
                    && args.iter().all(|a| supported_expr(a, ctx, names))
            }
            // Scalar `toString()` — the only builtin method with a primitive
            // receiver. Which intrinsic a call denotes is answered by the
            // shared table rather than re-derived from the method name here,
            // so the walker cannot disagree with the checker about what the
            // call means or what it produces (willow-0g8j.2.5).
            Type::I64 | Type::F64 | Type::Bool | Type::String => {
                matches!(
                    scalar_to_string(&object.ty, method, args),
                    Some(ret) if ret == e.ty
                ) && supported_expr(object, ctx, names)
            }
            Type::Named(class) if ctx.supported_class(class) => {
                let mangled = class_method_symbol_name(ctx.known_modules, class, method);
                ctx.callable(&mangled, args, true)
                    && ctx.fn_types.get(&mangled).is_some_and(
                        |t| matches!(t, Type::Fn(_, ret) if assignable_repr(ret, &e.ty)),
                    )
                    && supported_expr(object, ctx, names)
                    && args.iter().all(|a| supported_expr(a, ctx, names))
            }
            _ => false,
        },
        // `Enum::Variant` with no payload, and the bare unqualified form the
        // checker resolved to one (`Red`, `None`) — both lower to a static
        // property read (willow-0g8j.8). A real static class property is NOT in
        // the subset: it lives in module data the walker never addresses.
        HirExprKind::StaticField { class, field } => {
            !ctx.known_modules.contains_key(class)
                && ctx.supported_enum_type(&e.ty)
                && ctx.enum_instance(&e.ty).is_some_and(|(name, def)| {
                    name == *class && def.variant(field).is_some_and(|v| v.payloads.is_empty())
                })
        }
        // `Class::method(args)` — a static method of a simple class, or an enum
        // variant construction. A module call (`math::add`) and a builtin
        // namespace (`fs`, `env`) spell themselves the same way and still need
        // the AST path's special cases.
        HirExprKind::StaticCall {
            class,
            method,
            args,
        } => {
            // `Map::new()` is the one builtin constructor in the subset. The
            // result type decides, not the spelling, so a user class called
            // `Map` cannot reach `willow_map_new`: it would have a `Named` type
            // and fall through to the class path below. The empty map is either
            // already instantiated (and then vetted by `supported_type` above)
            // or carries the untyped `Map<Void, Void>` the checker gives it.
            if class == "Map" && method == "new" && args.is_empty() {
                return matches!(lir_collection(&e.ty), Some((LirCollection::Map, _)));
            }
            if ctx.known_modules.contains_key(class) {
                return false;
            }
            // `Enum::Variant(payload…)`, and the qualified fieldless form,
            // which HIR also spells as a zero-argument static call. The
            // payloads are STORE positions (the emitter coerces each one into
            // its declared slot), so `storable` is the right test, not
            // `assignable_repr`.
            if ctx.is_enum(class) {
                if !ctx.supported_enum_type(&e.ty) {
                    return false;
                }
                // The result type is what instantiates the variant, so
                // `Option::Some(1)` typed `Option<i64>` vets its payload
                // against `i64` rather than the declaration's `T`.
                let Some((name, def)) = ctx.enum_instance(&e.ty) else {
                    return false;
                };
                let Some(variant) = def.variant(method) else {
                    return false;
                };
                return name == *class
                    && variant.payloads.len() == args.len()
                    && variant
                        .payloads
                        .iter()
                        .zip(args)
                        .all(|(slot, a)| ctx.storable(slot, &a.ty))
                    && args.iter().all(|a| supported_expr(a, ctx, names));
            }
            if !ctx.supported_class(class) {
                return false;
            }
            let mangled = class_method_symbol_name(ctx.known_modules, class, method);
            ctx.callable(&mangled, args, true)
                && ctx
                    .fn_types
                    .get(&mangled)
                    .is_some_and(|t| matches!(t, Type::Fn(_, ret) if assignable_repr(ret, &e.ty)))
                && args.iter().all(|a| supported_expr(a, ctx, names))
        }
        // `expr?` on an `Option`/`Result` (willow-0g8j.2.1). Two halves:
        //
        // * The SUCCESS value is this node's own type, which must be the
        //   operand's first type argument — the payload the emitter reads out
        //   of word 1 (or out of the niche pointer itself). Checked rather than
        //   assumed so a lowering that ever retyped the node cannot make the
        //   walker reinterpret a payload word.
        // * The PROPAGATED value is the enclosing function's own return value.
        //   `lir_rejection_reason` has already vetted `f.return_type` as a
        //   supported type, and the type checker guarantees it is the matching
        //   `Option`/`Result`, so no further test is possible here — this arm
        //   sees the operand, not the function.
        //
        // A `void` payload (`Result<void, E>`, from `f()?;` in statement
        // position) stays out: the walker's `Ok()` object is one word, so the
        // success path's word-1 load would read past it.
        HirExprKind::TryPropagate { inner } => {
            let Some(resolved) = builtin_types::resolve(&inner.ty) else {
                return false;
            };
            matches!(resolved.id, B::Option | B::Result)
                && ctx.supported_enum_type(&inner.ty)
                && resolved.args.first().is_some_and(|payload| {
                    !matches!(payload, Type::Void) && assignable_repr(payload, &e.ty)
                })
                && supported_expr(inner, ctx, names)
        }
        _ => false,
    }
}

/// Where a LIR instruction came from, for the debug-build fault site. LIR
/// instructions carry no span of their own, so this takes the span of the
/// sub-expression that actually runs the fault-capable code: the indexed array
/// for an element store, the stored value otherwise.
fn lir_inst_span(inst: &LirInst) -> Option<crate::diagnostics::Span> {
    match inst {
        LirInst::Expr(e) => Some(e.span),
        LirInst::Defer { span, .. } => Some(*span),
        LirInst::EnterDeferScope { .. }
        | LirInst::LeaveDeferScope
        | LirInst::FlushDefers { .. } => None,
        LirInst::Let { value, .. }
        | LirInst::Assign { value, .. }
        | LirInst::FieldAssign { value, .. }
        | LirInst::StaticFieldAssign { value, .. } => Some(value.span),
        LirInst::IndexAssign { array, .. } => Some(array.span),
        LirInst::SuperInit { args } => args.first().map(|a| a.span),
    }
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit a whole function body by walking its LIR block graph. The entry
    /// block's instructions land in the already-created Cranelift entry block
    /// (parameters are bound there); every other LIR block gets its own.
    /// All paths are terminated by the LIR, so the caller must skip its
    /// implicit-return epilogue.
    pub(super) fn emit_lir_function(&mut self, f: &LirFunction) {
        let entry = self.builder.current_block().expect("entry block active");
        self.bind_lir_gc_locals(f);
        let mut blocks = vec![entry];
        for _ in 1..f.blocks.len() {
            blocks.push(self.builder.create_block());
        }

        for (i, block) in f.blocks.iter().enumerate() {
            if i > 0 {
                self.builder.switch_to_block(blocks[i]);
            }
            // Each LIR block starts a fresh Cranelift block, so whatever the
            // previous one ended with (a `return`, a diverging statement) says
            // nothing about this one.
            self.terminated = false;
            self.emit_lir_block(block, &blocks, &f.return_type);
        }
        // The enclosing function compiler may append shared panic-return CFG
        // after the LIR body. It seals all blocks once that ABI edge exists
        // (willow-s9ej.4).
        self.terminated = true;
    }

    /// Give every GC-managed `let` of this function one entry-allocated, rooted
    /// stack slot (see the module docs). The slot is null-initialized so a
    /// collection that happens before the `let` executes reads an empty root
    /// rather than uninitialized stack memory. GC-managed *parameters* already
    /// got the same treatment from `bind_param`, so they are skipped here.
    fn bind_lir_gc_locals(&mut self, f: &LirFunction) {
        let ptr_ty = self.module.target_config().pointer_type();
        let mut null = None;
        for block in &f.blocks {
            for inst in &block.instrs {
                let LirInst::Let { name, ty, .. } = inst else {
                    continue;
                };
                if !is_gc_managed(ty, self.enum_infos) {
                    continue;
                }
                let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    0,
                ));
                let zero = *null.get_or_insert_with(|| self.builder.ins().iconst(ptr_ty, 0));
                self.stack_store(zero, slot);
                self.emit_push_root_slot(slot);
                self.vars.insert(
                    name.clone(),
                    VarStorage::Stack {
                        slot,
                        ty: ty.clone(),
                    },
                );
            }
        }
    }

    fn emit_lir_block(
        &mut self,
        block: &LirBlock,
        blocks: &[cranelift_codegen::ir::Block],
        return_type: &Type,
    ) {
        for inst in &block.instrs {
            // Debug builds report runtime-raised faults (array bounds, a
            // blocked channel op) at the location of the code that ran, so the
            // LIR path must publish its own site too. Without this the fault
            // would inherit the CALLER's statement, because the AST path
            // published one before calling in (willow-s9ej.7 review).
            if let Some(span) = lir_inst_span(inst) {
                self.fault_site_span = Some(span);
            }
            match inst {
                LirInst::Let {
                    name, ty, value, ..
                } => {
                    let val = self.emit_lir_store_value(value, ty);
                    // A GC-managed local already has its rooted slot from
                    // `bind_lir_gc_locals`; storing into it is the whole binding.
                    if let Some(storage @ VarStorage::Stack { .. }) =
                        self.vars.get(name.as_str()).cloned()
                    {
                        self.store_var(&storage, val);
                        continue;
                    }
                    let var = self.builder.declare_var(clif_type(ty));
                    self.builder.def_var(var, val);
                    self.vars.insert(
                        name.clone(),
                        VarStorage::Value {
                            var,
                            ty: ty.clone(),
                        },
                    );
                }
                LirInst::Assign { name, value } => {
                    // The declared slot type — not the value's — decides
                    // whether this store boxes (`a = new Dog();` where `a` is
                    // an `Animal` local).
                    let Some(storage) = self.vars.get(name.as_str()).cloned() else {
                        self.emit_lir_expr(value);
                        continue;
                    };
                    let val = self.emit_lir_store_value(value, &storage.ty().clone());
                    self.store_var(&storage, val);
                }
                LirInst::Expr(e) => {
                    self.emit_lir_expr(e);
                }
                LirInst::IndexAssign {
                    array,
                    index,
                    value,
                } => {
                    // Null and out-of-bounds are checked inside `willow_array_set`.
                    let elem_ty = array_element_type(&array.ty);
                    let arr = self.emit_lir_expr(array);
                    // The index and the value are evaluated after the array
                    // handle is in hand, so the handle needs a root if either
                    // of them can collect.
                    let rooted = may_allocate(index) || self.lir_value_allocates(value, &elem_ty);
                    if rooted {
                        self.emit_push_root(arr);
                    }
                    let idx = self.emit_lir_expr(index);
                    let val = self.emit_lir_store_value(value, &elem_ty);
                    let word = self.coerce_to_i64(val, &elem_ty);
                    self.emit_runtime_call_with_cleanup(
                        "willow_array_set",
                        &[arr, idx, word],
                        |this| {
                            if rooted {
                                this.emit_pop_roots_n(1);
                                this.gc_root_count -= 1;
                            }
                        },
                    );
                }
                LirInst::FieldAssign {
                    object,
                    field,
                    value,
                } => self.emit_lir_field_assign(object, field, value),
                // Filtered out by eligibility.
                _ => unreachable!("unsupported LIR instruction reached emission"),
            }
            if self.terminated {
                // A diverging statement (`panic(...)`) already gave this
                // Cranelift block its terminator. Everything after it in this
                // LIR block, including the block's own terminator, is
                // unreachable and must not be emitted — nothing may follow a
                // terminator (willow-0g8j.2.5). The flag is local to the
                // divergence, so it is cleared for the next LIR block.
                self.terminated = false;
                return;
            }
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                self.builder.ins().jump(blocks[b.0], &[]);
            }
            Terminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                self.fault_site_span = Some(cond.span);
                let c = self.emit_lir_expr(cond);
                self.builder
                    .ins()
                    .brif(c, blocks[then_block.0], &[], blocks[else_block.0], &[]);
            }
            Terminator::Return(v) => self.emit_lir_return(v.as_ref(), return_type),
        }
    }

    /// The function-exit sequence: pop every root this function pushed, then
    /// return. Shared by a block's `Return` terminator and by a `return` inside
    /// a `match` arm (willow-0g8j.2.5), which is why it sets `self.terminated`
    /// — the arm's Cranelift block ends here.
    ///
    /// `gc_root_count` is deliberately NOT decremented: the pops emitted here
    /// belong to this path only, and the counter still describes what the paths
    /// that did not return are holding.
    fn emit_lir_return(&mut self, value: Option<&HirExpr>, return_type: &Type) {
        match value {
            Some(v) => {
                // Evaluate (and box, for an interface-typed return) first: the
                // value may read through a rooted local, and the box allocates.
                self.fault_site_span = Some(v.span);
                let val = self.emit_lir_store_value(v, return_type);
                self.emit_pop_roots_n(self.gc_root_count);
                self.builder.ins().return_(&[val]);
            }
            None => {
                self.emit_pop_roots_n(self.gc_root_count);
                if *return_type == Type::Void {
                    self.builder.ins().return_(&[]);
                } else {
                    // Unreachable fall-through in a value function (the checker
                    // guarantees returns); satisfy the signature with a zero.
                    let zero = match clif_type(return_type) {
                        types::F64 => self.builder.ins().f64const(0.0),
                        ty => self.builder.ins().iconst(ty, 0),
                    };
                    self.builder.ins().return_(&[zero]);
                }
            }
        }
        self.terminated = true;
    }

    fn emit_lir_expr(&mut self, e: &HirExpr) -> cranelift_codegen::ir::Value {
        match &e.kind {
            HirExprKind::Int(n) => self.builder.ins().iconst(types::I64, *n),
            HirExprKind::Float(x) => self.builder.ins().f64const(*x),
            HirExprKind::Bool(b) => self.builder.ins().iconst(types::I8, i64::from(*b)),
            HirExprKind::Str(s) => self.emit_string_literal(s),
            HirExprKind::Var(name) => match self.vars.get(name.as_str()).cloned() {
                Some(storage) => self.load_var(&storage),
                // Same loud failure as the AST path (willow-thqe class): a
                // variable the eligibility check admitted must be bound.
                None => {
                    panic!("internal compiler error: variable `{name}` reached LIR codegen unbound")
                }
            },
            HirExprKind::Binary { op, lhs, rhs } if lhs.ty == Type::String => {
                self.emit_lir_string_binop(op, lhs, rhs)
            }
            HirExprKind::Binary { op, lhs, rhs } => match op {
                // Short-circuit: the rhs must not evaluate when the lhs decides.
                BinOp::And | BinOp::Or => {
                    let l = self.emit_lir_expr(lhs);
                    let result_var = self.builder.declare_var(types::I8);
                    let rhs_block = self.builder.create_block();
                    let short_block = self.builder.create_block();
                    let merge_block = self.builder.create_block();
                    if matches!(op, BinOp::And) {
                        self.builder.ins().brif(l, rhs_block, &[], short_block, &[]);
                    } else {
                        self.builder.ins().brif(l, short_block, &[], rhs_block, &[]);
                    }

                    self.builder.switch_to_block(rhs_block);
                    self.builder.seal_block(rhs_block);
                    let r = self.emit_lir_expr(rhs);
                    self.builder.def_var(result_var, r);
                    self.builder.ins().jump(merge_block, &[]);

                    self.builder.switch_to_block(short_block);
                    self.builder.seal_block(short_block);
                    let short_val = self
                        .builder
                        .ins()
                        .iconst(types::I8, i64::from(matches!(op, BinOp::Or)));
                    self.builder.def_var(result_var, short_val);
                    self.builder.ins().jump(merge_block, &[]);

                    self.builder.switch_to_block(merge_block);
                    self.builder.seal_block(merge_block);
                    self.builder.use_var(result_var)
                }
                _ => {
                    let float = lhs.ty == Type::F64;
                    let l = self.emit_lir_expr(lhs);
                    let r = self.emit_lir_expr(rhs);
                    if !float && matches!(op, BinOp::Div | BinOp::Rem) {
                        self.emit_int_div_guard(l, r, matches!(op, BinOp::Rem), e.span);
                    }
                    // `**` needs control flow for a dynamic exponent, so it is
                    // lowered before the straight-line operator table
                    // (willow-n5yv.3).
                    if matches!(op, BinOp::Pow) {
                        if float {
                            self.emit_pow_f64(l, r)
                        } else {
                            self.emit_pow_i64(l, r, e.span)
                        }
                    } else {
                        self.emit_lir_binop(op, l, r, float)
                    }
                }
            },
            HirExprKind::Ternary {
                condition,
                then_expr,
                else_expr,
            } => {
                let result_var = self.builder.declare_var(clif_type(&e.ty));
                let then_block = self.builder.create_block();
                let else_block = self.builder.create_block();
                let merge_block = self.builder.create_block();

                let cond = self.emit_lir_expr(condition);
                self.builder
                    .ins()
                    .brif(cond, then_block, &[], else_block, &[]);

                self.builder.switch_to_block(then_block);
                self.builder.seal_block(then_block);
                let t = self.emit_lir_expr(then_expr);
                self.builder.def_var(result_var, t);
                self.builder.ins().jump(merge_block, &[]);

                self.builder.switch_to_block(else_block);
                self.builder.seal_block(else_block);
                let f = self.emit_lir_expr(else_expr);
                self.builder.def_var(result_var, f);
                self.builder.ins().jump(merge_block, &[]);

                self.builder.switch_to_block(merge_block);
                self.builder.seal_block(merge_block);
                self.builder.use_var(result_var)
            }
            HirExprKind::Unary { op, operand } => {
                let val = self.emit_lir_expr(operand);
                match op {
                    UnaryOp::Neg if operand.ty == Type::F64 => self.builder.ins().fneg(val),
                    UnaryOp::Neg => self.builder.ins().ineg(val),
                    UnaryOp::Not => {
                        let one = self.builder.ins().iconst(types::I8, 1);
                        self.builder.ins().bxor(val, one)
                    }
                }
            }
            // A named function used as a value — the address of the compiled
            // function, exactly as the AST path emits it (willow-0g8j.2.2).
            HirExprKind::FnRef(name) => {
                let fid = self.func_ids[name.as_str()];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder
                    .ins()
                    .func_addr(super::type_helpers::FN_ADDR_TYPE, fref)
            }
            // A lambda is a lifted top-level function with no captured
            // environment, so its value is just that function's address. The
            // span is the same key the AST path uses, which is what lets the
            // walker name a symbol it never invented (willow-0g8j.2.2).
            HirExprKind::Lambda { .. } => {
                let name = self.lambda_names[&e.span].clone();
                let fid = self.func_ids[name.as_str()];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder
                    .ins()
                    .func_addr(super::type_helpers::FN_ADDR_TYPE, fref)
            }
            // A call through a local function value shadows every top-level
            // name, so it is tested before the direct-call path — the same
            // order the AST path and eligibility use (willow-bv9.1).
            HirExprKind::Call { callee, args }
                if self.vars.contains_key(callee.as_str())
                    && matches!(self.vars[callee.as_str()].ty(), Type::Fn(..)) =>
            {
                let storage = self.vars[callee.as_str()].clone();
                let Type::Fn(param_types, ret_type) = storage.ty().clone() else {
                    unreachable!("non-callable local call passed eligibility")
                };
                let callee_val = self.load_var(&storage);
                let (vals, temp_roots) = self.emit_lir_args_rooted(args, Some(&param_types));

                let mut sig = self.module.make_signature();
                for param_type in &param_types {
                    sig.params.push(AbiParam::new(clif_type(param_type)));
                }
                if *ret_type != Type::Void {
                    sig.returns.push(AbiParam::new(clif_type(&ret_type)));
                }
                let sig_ref = self.builder.import_signature(sig);
                let pushed = self.emit_callstack_push(callee, e.span);
                // An arbitrary function value has no statically known target,
                // so panic handling stays conservatively enabled.
                let panic_depth = self.emit_pre_willow_call_panic_depth();
                let call = self.builder.ins().call_indirect(sig_ref, callee_val, &vals);
                let result = self
                    .builder
                    .inst_results(call)
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0));
                if pushed {
                    self.emit_callstack_pop();
                }
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
                self.emit_post_willow_call_panic_check(panic_depth);
                result
            }
            // `format(spec, ..)` has no callee symbol — it assembles its
            // result at the call site, through the same emitter the AST path
            // uses so the two produce the same string (willow-0g8j.2.5).
            // The one `!`-typed expression the walker emits. Eligibility
            // admits it only in statement or match-arm position, where the
            // block-terminating unwind below is legal (willow-0g8j.2.5).
            HirExprKind::Call { callee, args } if callee == "panic" => {
                self.emit_lir_panic(args, e.span)
            }
            HirExprKind::Call { callee, args } if callee == "format" => {
                let operands = format_operands(args).expect("format call vetted by eligibility");
                let HirExprKind::Str(spec) = &args[0].kind else {
                    unreachable!("format spec vetted by eligibility")
                };
                let spec = spec.clone();
                self.emit_interpolated_with(&spec, operands.len(), |this, i| {
                    (this.emit_lir_expr(&operands[i]), operands[i].ty.clone())
                })
            }
            HirExprKind::Call { callee, args } => {
                let params = self.fn_param_types(callee);
                let (vals, temp_roots) = self.emit_lir_args_rooted(args, params.as_deref());
                let fid = self.func_ids[callee.as_str()];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                // Debug builds record the call on the panic call-chain stack,
                // exactly like the AST path (willow-992h).
                let pushed = self.emit_callstack_push(callee, e.span);
                let panic_depth = self.emit_pre_user_call_panic_depth(callee);
                let call = self.builder.ins().call(fref, &vals);
                let results = self.builder.inst_results(call);
                let result = results
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0));
                if pushed {
                    self.emit_callstack_pop();
                }
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
                self.emit_post_willow_call_panic_check(panic_depth);
                result
            }
            HirExprKind::Print { value, newline } => {
                let val = self.emit_lir_expr(value);
                let fn_name = match (&value.ty, newline) {
                    (Type::I64, false) => "willow_print_i64",
                    (Type::I64, true) => "willow_println_i64",
                    (Type::F64, false) => "willow_print_f64",
                    (Type::F64, true) => "willow_println_f64",
                    (Type::Bool, false) => "willow_print_bool",
                    (Type::Bool, true) => "willow_println_bool",
                    (Type::String, false) => "willow_print_string",
                    (Type::String, true) => "willow_println_string",
                    _ => unreachable!("unsupported print type passed eligibility"),
                };
                self.emit_void_runtime_call(fn_name, &[val]);
                self.builder.ins().iconst(types::I8, 0)
            }
            HirExprKind::Array { elements } => {
                self.emit_lir_array_literal(elements, &array_element_type(&e.ty))
            }
            HirExprKind::Index { array, index } => self.emit_lir_index(array, index),
            HirExprKind::MethodCall {
                object,
                method,
                args,
            } => match &object.ty {
                Type::Array(_) => self.emit_lir_array_method(object, method, args),
                Type::Generic(..) if lir_collection(&object.ty).is_some() => {
                    self.emit_lir_collection_method(object, method, args)
                }
                Type::Generic(..) => {
                    self.emit_lir_option_result_method(object, method, args, e.span)
                }
                Type::Named(n) if self.interface_infos.contains_key(n) => {
                    self.emit_lir_interface_call(object, method, args, e.span)
                }
                Type::I64 | Type::F64 | Type::Bool | Type::String => {
                    self.emit_lir_scalar_to_string(object, method, args)
                }
                _ => self.emit_lir_class_method(object, method, args, &e.ty, e.span),
            },
            HirExprKind::New { class, args } => self.emit_lir_new(class, args, e.span),
            HirExprKind::ObjectLiteral { class, fields } => {
                self.emit_lir_object_literal(class, fields)
            }
            HirExprKind::FieldAccess { object, field } => self.emit_lir_field_access(object, field),
            HirExprKind::StaticCall {
                class,
                method,
                args,
            } => self.emit_lir_static_call(class, method, args, &e.ty, e.span),
            // A fieldless variant written unqualified (`Red`, and the `None` of
            // a non-generic user enum) — the only static-property read in the
            // subset (willow-0g8j.8).
            HirExprKind::StaticField { class, field } => {
                self.emit_lir_enum_construction(class, field, &[], &e.ty)
            }
            HirExprKind::Match { scrutinee, arms } => self.emit_lir_match(scrutinee, arms, &e.ty),
            HirExprKind::TryPropagate { inner } => self.emit_lir_try_propagate(inner),
            _ => unreachable!("unsupported LIR expression reached emission"),
        }
    }

    /// `inner?` — extract the `Ok`/`Some` payload, or early-return the failure
    /// as this function's own return value (willow-0g8j.2.1).
    ///
    /// Mirrors [`FuncGen::emit_try_propagate`] minus the three exits eligibility
    /// has already ruled out, which is why this is a separate emitter rather
    /// than a shared one: the walker never sees an async function (they take the
    /// cooperative path before eligibility is asked), a `Result` main (only the
    /// parameterless `void` form is eligible) or a `defer` (`LirInst::Defer` is
    /// rejected), so the failure path here is the plain synchronous
    /// "pop this function's roots and return".
    ///
    /// The failure value is REBUILT rather than forwarded whenever the two sides
    /// can disagree about representation:
    ///
    /// * `Option`: the operand and the return type each pick their own niche
    ///   (`Option<String>` is a nullable pointer, `Option<i64>` is boxed), so
    ///   the destination's `None` is constructed from `self.return_type`.
    /// * `Result` with automatic error conversion (willow-1ow): when the
    ///   operand's `E1` differs from the function's `E2`, the payload goes
    ///   through `into()` and is re-wrapped as `Err(e2)`. Eligibility admits the
    ///   operand only if `Result<T, E1>` is a supported type, which excludes a
    ///   class in an inheritance hierarchy — so the dispatch inside
    ///   [`FuncGen::emit_into_conversion`] resolves to a single direct call.
    ///
    /// Only when `E1 == E2` is the operand pointer returned unchanged: both
    /// sides are then the same boxed `[tag | payload]` object.
    fn emit_lir_try_propagate(&mut self, inner: &HirExpr) -> cranelift_codegen::ir::Value {
        debug_assert!(
            self.coop_frame.is_none()
                && self.main_result_err_ty.is_none()
                && self.defer_stack.iter().all(|f| f.is_empty()),
            "the walker compiles no async, `Result` main or deferring function, \
             so `?` here is the plain synchronous early return"
        );
        let operand_ty = inner.ty.clone();
        let result_ptr = self.emit_lir_expr(inner);
        let payload_ty = try_propagate_payload_type(&operand_ty);
        // `Some(x)` IS `x` and `None` is 0 under the niche, so both the test and
        // the payload read below become pointer arithmetic instead of loads.
        let niche = option_inner(&operand_ty).is_some()
            && option_repr(&operand_ty, self.enum_infos) == Some(OptionRepr::NullableGcPointer);
        let is_option = option_inner(&operand_ty).is_some();
        let convert: Option<(String, Type)> = match (
            result_err_type(&operand_ty),
            result_err_type(&self.return_type),
        ) {
            (Some(Type::Named(e1)), Some(e2))
                if Type::Named(e1.clone()) != e2 && e2 != Type::Void =>
            {
                Some((e1, e2))
            }
            _ => None,
        };

        let is_ok = if niche {
            self.builder
                .ins()
                .icmp_imm_u(IntCC::NotEqual, result_ptr, 0)
        } else {
            // Boxed `Result` and boxed `Option` both use tag 0 for Ok/Some.
            let tag = self
                .builder
                .ins()
                .load(types::I64, MemFlagsData::new(), result_ptr, 0i32);
            let ok_tag = self.builder.ins().iconst(types::I64, 0);
            self.builder.ins().icmp(IntCC::Equal, tag, ok_tag)
        };

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        // The success arm is an independent predecessor: it continues with the
        // root depth from BEFORE the branch, not the one the failure arm left.
        let branch_root_depth = self.gc_root_count;
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        // ── Failure: build this function's return value and leave ────────────
        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let return_ptr = if is_option {
            let return_inner = option_inner(&self.return_type)
                .cloned()
                .expect("`?` on an Option is only checked inside an Option-returning function");
            self.emit_alloc_option_none(&return_inner)
        } else if let Some((e1_name, e2_ty)) = &convert {
            let e1_payload =
                self.builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), result_ptr, 8i32);
            let e1_is_gc = is_gc_managed(&Type::Named(e1_name.clone()), self.enum_infos);
            // `into` and the re-wrap both allocate, so a GC-managed payload has
            // to be reachable across them.
            if e1_is_gc {
                self.emit_push_root(e1_payload);
            }
            let e2_val = self.emit_into_conversion(e1_payload, e1_name);
            if e1_is_gc {
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
            }
            self.emit_alloc_enum_variant(1, e2_ty, e2_val)
        } else {
            result_ptr
        };
        // Same epilogue as `Terminator::Return`: the roots are live until the
        // value is built (it may allocate), and only then does the frame go.
        self.emit_pop_roots_n(self.gc_root_count);
        self.builder.ins().return_(&[return_ptr]);

        // ── Success: the payload becomes this expression's value ─────────────
        self.gc_root_count = branch_root_depth;
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let payload = if niche {
            result_ptr
        } else {
            self.builder
                .ins()
                .load(types::I64, MemFlagsData::new(), result_ptr, 8i32)
        };
        self.coerce_i64_to(payload, &payload_ty)
    }

    /// Build an enum value: a bare tag when NO variant of the enum carries a
    /// payload, the `Option` pointer niche when the instance uses it, and a
    /// `[tag | payload…]` GC object otherwise. Mirrors
    /// [`FuncGen::emit_enum_variant_alloc`] case for case (willow-0g8j.2.1).
    ///
    /// `enum_ty` is the type of the expression BEING BUILT, not the bare enum
    /// name: it is what instantiates a generic enum's payloads and what
    /// [`option_repr`] reads, so passing anything else would pick a different
    /// representation than the AST emitter and the two paths would disagree
    /// about the same value.
    fn emit_lir_enum_construction(
        &mut self,
        enum_name: &str,
        variant: &str,
        args: &[HirExpr],
        enum_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let tag = self.enum_variant_tag(enum_name, variant);
        // `Option<T>` over a non-nullable GC payload: `Some(x)` IS `x` and
        // `None` is the null word. No tag, and `None` allocates nothing.
        if option_repr(enum_ty, self.enum_infos) == Some(OptionRepr::NullableGcPointer) {
            return match tag {
                0 => {
                    let arg = args
                        .first()
                        .expect("eligibility admits `Option::Some` only with its one payload");
                    // The niche stores the payload directly, so a class value
                    // going into an interface-typed `Option` must still be
                    // boxed here — the same coercion the boxed layout does.
                    let stored_ty = option_inner(enum_ty)
                        .cloned()
                        .unwrap_or_else(|| arg.ty.clone());
                    self.emit_lir_store_value(arg, &stored_ty)
                }
                _ => self.builder.ins().iconst(types::I64, 0),
            };
        }
        if !self.enum_is_gc_object_type(enum_name) {
            return self.builder.ins().iconst(types::I64, tag);
        }

        let mut payload_types = self.resolve_variant_payload_types(enum_name, variant, enum_ty);
        // `Result<void, E>::Ok()` carries a substituted `void` payload and no
        // argument; eligibility normalises that list away, so emission must
        // too or the two would describe different objects.
        normalize_void_payloads(&mut payload_types);
        let slot_kinds = payload_types
            .iter()
            .map(|ty| {
                if is_gc_managed(ty, self.enum_infos) {
                    willow_abi::SlotKind::GcRef
                } else {
                    willow_abi::SlotKind::Word
                }
            })
            .collect::<Vec<_>>();
        let layout = willow_abi::EnumVariantLayout::new(tag as u32, &slot_kinds);
        let pointer_bytes = self.module.target_config().pointer_type().bytes();
        let ptr = self.emit_gc_alloc(GcLayoutMetadata::new(
            GcObjectKind::Enum,
            i64::from(layout.payload_bytes(pointer_bytes)),
            0,
            layout.gc_ref_mask(),
        ));
        let tag_val = self.builder.ins().iconst(types::I64, tag);
        self.builder
            .ins()
            .store(MemFlagsData::new(), tag_val, ptr, 0i32);
        // Root the half-built enum across payload evaluation: an argument can
        // allocate and collect, and the object is otherwise only in an SSA
        // register. The payload is alloc_zeroed, so tracing it before every
        // slot is stored is safe — an unstored ref slot reads as null.
        let needs_root = !args.is_empty();
        if needs_root {
            self.emit_push_root(ptr);
        }
        for (i, (arg, stored_ty)) in args.iter().zip(payload_types.iter()).enumerate() {
            let offset = layout.payload_byte_offset(pointer_bytes) as i32
                + (i as i32 * pointer_bytes as i32);
            // The declared payload is the STORAGE type: a class argument going
            // into an interface-typed slot must be boxed, not stored raw.
            let val = self.emit_lir_store_value(arg, stored_ty);
            let val_i64 = if matches!(stored_ty, Type::F64) {
                self.builder
                    .ins()
                    .bitcast(types::I64, MemFlagsData::new(), val)
            } else {
                val
            };
            self.emit_gc_heap_store(
                ptr,
                offset,
                val_i64,
                stored_ty,
                GcStoreDestination::EnumPayload,
            );
        }
        if needs_root {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        ptr
    }

    /// `match` in expression position, mirroring [`FuncGen::emit_match`]: one
    /// result variable, one merge block, and a chain of arm blocks each entered
    /// by a `brif` on its pattern test (willow-0g8j.8).
    ///
    /// Two simplifications the AST version cannot make, both bought by
    /// eligibility: every arm body is a single expression, so no arm can
    /// terminate its block and the merge block is always reachable; and the
    /// result type is `e.ty` directly, with no structural re-derivation.
    /// `panic(...)`: assemble the message, then hand off to the shared unwind
    /// so the LIR and AST paths cannot drift apart on panic protocol.
    ///
    /// The returned value is the unreachable placeholder every `panic` yields;
    /// `self.terminated` is set, and the caller must not emit anything more
    /// into this block.
    fn emit_lir_panic(
        &mut self,
        args: &[HirExpr],
        span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let msg = match args {
            [] => self.emit_string_literal("explicit panic"),
            [message] => self.emit_lir_expr(message),
            [_spec, operands @ ..] => {
                let HirExprKind::Str(spec) = &args[0].kind else {
                    unreachable!("panic spec vetted by eligibility")
                };
                let spec = spec.clone();
                self.emit_interpolated_with(&spec, operands.len(), |this, i| {
                    (this.emit_lir_expr(&operands[i]), operands[i].ty.clone())
                })
            }
        };
        self.emit_panic_with_message(msg, span)
    }

    fn emit_lir_match(
        &mut self,
        scrutinee: &HirExpr,
        arms: &[HirMatchArm],
        result_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let scrutinee_val = self.emit_lir_expr(scrutinee);
        // A GC enum scrutinee owns the payloads the arm bindings alias, and an
        // arm body may allocate before it reads one. Keep it rooted across
        // every arm.
        let rooted_scrutinee = is_gc_managed(&scrutinee.ty, self.enum_infos);
        if rooted_scrutinee {
            self.emit_push_root(scrutinee_val);
        }

        let result_clif = clif_type(result_ty);
        let result_var = self.builder.declare_var(result_clif);
        // Seed the result so a scrutinee matching no arm still leaves the
        // variable defined on the path into the merge block.
        let zero = match result_clif {
            types::F64 => self.builder.ins().f64const(0.0),
            ty => self.builder.ins().iconst(ty, 0),
        };
        self.builder.def_var(result_var, zero);

        let merge_block = self.builder.create_block();
        // A `match` whose every arm leaves the function (or unwinds) is typed
        // `!` and its merge block is unreachable — there is no reaching
        // definition of the result to read there, so it must not be read.
        let mut merge_reachable = false;
        for (i, arm) in arms.iter().enumerate() {
            let is_last = i + 1 == arms.len();
            let always_matches = matches!(
                arm.pattern,
                HirPattern::Wildcard | HirPattern::Binding { .. }
            );
            let arm_block = self.builder.create_block();
            let next_block = if always_matches || is_last {
                None
            } else {
                Some(self.builder.create_block())
            };

            if always_matches {
                self.builder.ins().jump(arm_block, &[]);
            } else {
                let cond = self.emit_lir_pattern_check(scrutinee_val, &scrutinee.ty, &arm.pattern);
                let fallthrough = next_block.unwrap_or(merge_block);
                merge_reachable |= fallthrough == merge_block;
                self.builder
                    .ins()
                    .brif(cond, arm_block, &[], fallthrough, &[]);
            }

            self.builder.switch_to_block(arm_block);
            self.builder.seal_block(arm_block);

            let saved_vars = self.bind_lir_pattern(scrutinee_val, &scrutinee.ty, &arm.pattern);
            match self.emit_lir_arm_body(&arm.body) {
                // No coercion: eligibility required `assignable_repr` between
                // the match's type and every arm's, exactly as it does for a
                // ternary, so an arm's value already has the result variable's
                // representation.
                Some(arm_val) => {
                    self.builder.def_var(result_var, arm_val);
                    self.builder.ins().jump(merge_block, &[]);
                    merge_reachable = true;
                }
                // `_ => panic("...")`, or an arm that returns: the arm's own
                // block is already terminated, so it neither defines the result
                // nor reaches the merge.
                None => self.terminated = false,
            }
            if let Some(saved) = saved_vars {
                self.vars = saved;
            }

            if let Some(next) = next_block {
                self.builder.switch_to_block(next);
                self.builder.seal_block(next);
            }
            if always_matches {
                break;
            }
        }

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        // The root pops happen on both paths so the compile-time root count
        // stays balanced for whatever follows; on the unreachable path they are
        // dead code, like the block that holds them.
        let result = if merge_reachable {
            self.builder.use_var(result_var)
        } else {
            match result_clif {
                types::F64 => self.builder.ins().f64const(0.0),
                ty => self.builder.ins().iconst(ty, 0),
            }
        };
        if rooted_scrutinee {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        if !merge_reachable {
            // Nothing branches here, but the block still needs a terminator,
            // and the caller must be told the match diverged so it stops
            // emitting into it.
            self.builder
                .ins()
                .trap(cranelift_codegen::ir::TrapCode::unwrap_user(1));
            self.terminated = true;
        }
        result
    }

    /// Emit one `match` arm's body. Returns the value it hands to the merge
    /// block, or `None` when the arm diverged — a `return`, a `panic(...)`, or
    /// a nested all-diverging `match` — and so terminated its own block.
    fn emit_lir_arm_body(&mut self, body: &[HirStmt]) -> Option<cranelift_codegen::ir::Value> {
        let mut value = None;
        for stmt in body {
            match stmt {
                HirStmt::Expr(e) => {
                    self.fault_site_span = Some(e.span);
                    value = Some(self.emit_lir_expr(e));
                }
                HirStmt::Return {
                    value: returned, ..
                } => {
                    let return_type = self.return_type.clone();
                    self.emit_lir_return(returned.as_ref(), &return_type);
                }
                // Eligibility admits only the two forms above.
                _ => unreachable!("unsupported match-arm statement reached emission"),
            }
            if self.terminated {
                return None;
            }
        }
        value
    }

    /// The `i8` "does this arm apply?" test. Mirrors
    /// [`FuncGen::emit_pattern_check`] without `ClassDowncast`, which
    /// eligibility does not admit.
    fn emit_lir_pattern_check(
        &mut self,
        scrutinee: cranelift_codegen::ir::Value,
        scrutinee_ty: &Type,
        pattern: &HirPattern,
    ) -> cranelift_codegen::ir::Value {
        match pattern {
            HirPattern::Wildcard | HirPattern::Binding { .. } => {
                self.builder.ins().iconst(types::I8, 1)
            }
            HirPattern::LiteralBool(b) => {
                let expected = self.builder.ins().iconst(types::I8, i64::from(*b));
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            HirPattern::LiteralInt(n) => {
                let expected = self.builder.ins().iconst(types::I64, *n);
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            HirPattern::EnumVariant { enum_name, variant }
            | HirPattern::EnumVariantTuple {
                enum_name, variant, ..
            } => {
                let tag = self.enum_variant_tag(enum_name, variant);
                // The `Option` pointer niche carries no tag word: `Some` is any
                // non-null payload and `None` is null (willow-0g8j.2.1).
                if enum_name == "Option"
                    && option_repr(scrutinee_ty, self.enum_infos)
                        == Some(OptionRepr::NullableGcPointer)
                {
                    let cc = if tag == 0 {
                        IntCC::NotEqual
                    } else {
                        IntCC::Equal
                    };
                    return self.builder.ins().icmp_imm_u(cc, scrutinee, 0);
                }
                let expected = self.builder.ins().iconst(types::I64, tag);
                // A payload-carrying enum is a heap object whose word 0 is the
                // tag; a fieldless one IS the tag.
                let actual = if self.enum_is_gc_object_type(enum_name) {
                    self.emit_load_enum_tag(scrutinee)
                } else {
                    scrutinee
                };
                self.builder.ins().icmp(IntCC::Equal, actual, expected)
            }
            HirPattern::ClassDowncast { .. } => {
                unreachable!("eligibility keeps downcast patterns out of the LIR subset")
            }
        }
    }

    /// Bring a pattern's bindings into scope for its arm body, returning the
    /// variable map to restore afterwards (`None` when the pattern binds
    /// nothing).
    ///
    /// Bindings are plain Cranelift variables, not rooted slots. That is safe
    /// because the scrutinee is rooted across the arm, Willow's collector is
    /// non-moving, and every payload is reachable from the scrutinee — the same
    /// argument the AST path relies on.
    fn bind_lir_pattern(
        &mut self,
        scrutinee: cranelift_codegen::ir::Value,
        scrutinee_ty: &Type,
        pattern: &HirPattern,
    ) -> Option<HashMap<String, VarStorage>> {
        match pattern {
            HirPattern::Binding { name, ty } => {
                let var = self.builder.declare_var(clif_type(ty));
                self.builder.def_var(var, scrutinee);
                let saved = self.vars.clone();
                self.vars.insert(
                    name.clone(),
                    VarStorage::Value {
                        var,
                        ty: ty.clone(),
                    },
                );
                Some(saved)
            }
            HirPattern::EnumVariantTuple {
                enum_name,
                variant,
                bindings,
            } => {
                let saved = self.vars.clone();
                // Read the DECLARED payload types rather than the pattern's
                // recorded ones, so the load width follows the layout the
                // constructor wrote.
                let mut payload_types =
                    self.resolve_variant_payload_types(enum_name, variant, scrutinee_ty);
                normalize_void_payloads(&mut payload_types);
                let niche = enum_name == "Option"
                    && option_repr(scrutinee_ty, self.enum_infos)
                        == Some(OptionRepr::NullableGcPointer);
                for (i, ((name, _), payload_ty)) in
                    bindings.iter().zip(payload_types.iter()).enumerate()
                {
                    let clif_ty = clif_type(payload_ty);
                    // In the niche the scrutinee IS the payload — there is no
                    // heap object to load word 1 from.
                    let raw = if niche && i == 0 {
                        scrutinee
                    } else {
                        let offset = (1 + i) as i32 * 8;
                        self.builder
                            .ins()
                            .load(types::I64, MemFlagsData::new(), scrutinee, offset)
                    };
                    let val = if clif_ty == types::F64 {
                        self.builder
                            .ins()
                            .bitcast(types::F64, MemFlagsData::new(), raw)
                    } else if clif_ty == types::I8 {
                        self.builder.ins().ireduce(types::I8, raw)
                    } else {
                        raw
                    };
                    let var = self.builder.declare_var(clif_ty);
                    self.builder.def_var(var, val);
                    self.vars.insert(
                        name.clone(),
                        VarStorage::Value {
                            var,
                            ty: payload_ty.clone(),
                        },
                    );
                }
                Some(saved)
            }
            _ => None,
        }
    }

    /// Evaluate call arguments left to right, rooting each GC-managed one as it
    /// is produced: a later argument (or the callee itself) can allocate and
    /// collect, and an already-evaluated argument is otherwise only held in an
    /// SSA register the GC cannot see (same rule as the AST path). Returns the
    /// values and the number of roots the caller must pop after the call.
    ///
    /// `params` are the callee's declared parameter types with the hidden
    /// receiver already dropped, so an argument passed into an interface-typed
    /// parameter is boxed here. The box is what gets rooted (the raw object it
    /// wraps is rooted inside [`FuncGen::emit_interface_box`] across its own
    /// allocation), and every earlier argument is already rooted, so that
    /// allocation is safe. `None` when the callee has no recorded signature —
    /// eligibility then admits no argument that could need a coercion.
    fn emit_lir_args_rooted(
        &mut self,
        args: &[HirExpr],
        params: Option<&[Type]>,
    ) -> (Vec<cranelift_codegen::ir::Value>, usize) {
        let mut vals = Vec::with_capacity(args.len());
        let mut temp_roots = 0usize;
        for (i, a) in args.iter().enumerate() {
            let val = match params.and_then(|p| p.get(i)) {
                Some(target) => self.emit_lir_store_value(a, &target.clone()),
                None => self.emit_lir_expr(a),
            };
            if is_gc_managed(&a.ty, self.enum_infos) {
                self.emit_push_root(val);
                temp_roots += 1;
            }
            vals.push(val);
        }
        (vals, temp_roots)
    }

    /// Emit `value` and convert it to `target_ty`, which for the walker means
    /// exactly one thing: boxing a class instance into an interface. Routed
    /// through the AST path's [`FuncGen::coerce_to_target`] so the two emitters
    /// cannot disagree about layout or vtable selection. Eligibility has
    /// already proved the vtable exists, so this never silently passes an
    /// unboxed object into an interface slot.
    fn emit_lir_store_value(
        &mut self,
        value: &HirExpr,
        target_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let val = self.emit_lir_expr(value);
        self.coerce_to_target(val, &value.ty, target_ty)
    }

    /// Whether storing a `value_ty` into a `target_ty` slot runs an allocation.
    /// Mirrors [`FuncGen::coerce_to_target`]'s decision: a *different* named
    /// type, the target an interface and the source a class with a layout.
    fn lir_store_allocates(&self, value_ty: &Type, target_ty: &Type) -> bool {
        let (Type::Named(iface), Type::Named(class)) = (target_ty, value_ty) else {
            return false;
        };
        iface != class
            && self.interface_infos.contains_key(iface)
            && self.class_layouts.contains_key(class)
    }

    /// Whether producing `value` for a `target_ty` slot can run a collection —
    /// evaluating it, or the interface box the store puts on top of it. Sites
    /// that root a live temporary across the value must use this, not
    /// [`may_allocate`] alone: the box allocates *after* the value expression
    /// is done, which `may_allocate` cannot see (willow-j260).
    fn lir_value_allocates(&self, value: &HirExpr, target_ty: &Type) -> bool {
        may_allocate(value) || self.lir_store_allocates(&value.ty, target_ty)
    }

    /// The class layout for a receiver/field-owner type. Eligibility already
    /// proved the type is a simple class with a registered layout.
    fn lir_class_layout(&self, ty: &Type) -> Vec<(String, Type)> {
        let class =
            class_name_for_object_type(ty).expect("class receiver type vetted by LIR eligibility");
        self.class_layouts
            .get(&class)
            .cloned()
            .expect("class layout vetted by LIR eligibility")
    }

    /// `new Class(args)`: allocate the object with its GC ref mask, stamp the
    /// runtime `type_id` into word 0, then run the explicit `Class__init` or —
    /// when the class has none — store the positional arguments memberwise.
    ///
    /// The fresh object is rooted for the whole construction: argument
    /// evaluation and the constructor body can both allocate, and until this
    /// returns nothing else refers to it.
    fn emit_lir_new(
        &mut self,
        class: &str,
        args: &[HirExpr],
        span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let layout = self
            .class_layouts
            .get(class)
            .cloned()
            .expect("class layout vetted by LIR eligibility");
        // Runtime type ids start at 1, so 0 is not a class — falling back to it
        // would stamp a bogus id into the descriptor and make every later
        // `is`/downcast on the object answer wrong instead of failing here. The
        // AST emitter treats a missing id the same way (willow-uqzx, catalog
        // item 14). The id still selects the GC layout; word 0 of the object
        // itself holds the descriptor (willow-fm7t).
        let type_id = self.class_type_ids.get(class).copied().unwrap_or_else(|| {
            panic!("compiler invariant violated: checked class `{class}` has no type id")
        });
        let gc_layout = GcLayoutMetadata::class(class, type_id, &layout, self.enum_infos);
        let ptr = self.emit_gc_alloc(gc_layout);
        self.emit_store_class_descriptor(ptr, class);
        self.emit_push_root(ptr);

        let mangled = class_method_symbol_name(self.known_modules, class, "init");
        if let Some(&init_fid) = self.func_ids.get(&mangled) {
            let params = self.method_param_types(&mangled);
            let (arg_vals, arg_roots) = self.emit_lir_args_rooted(args, params.as_deref());
            let init_ref = self
                .module
                .declare_func_in_func(init_fid, self.builder.func);
            let mut call_args = vec![ptr];
            call_args.extend(arg_vals);
            // Arguments are evaluated before the constructor call-chain frame is
            // installed, matching ordinary calls: a panic in an argument is not
            // attributed to an `init` body that never started.
            let pushed = self.emit_callstack_push("init", span);
            let panic_depth = self.emit_pre_user_call_panic_depth(&mangled);
            self.builder.ins().call(init_ref, &call_args);
            if pushed {
                self.emit_callstack_pop();
            }
            if arg_roots > 0 {
                self.emit_pop_roots_n(arg_roots);
                self.gc_root_count -= arg_roots;
            }
            self.emit_post_willow_call_panic_check(panic_depth);
        } else {
            // Implicit memberwise constructor. Each value is stored the instant
            // it exists, so it is unrooted only across an allocation-free window.
            for (i, arg) in args.iter().enumerate() {
                let field_ty = layout[i].1.clone();
                let val = self.emit_lir_store_value(arg, &field_ty);
                self.emit_gc_heap_store(
                    ptr,
                    (i as i32 + 1) * 8,
                    val,
                    &field_ty,
                    GcStoreDestination::ObjectField,
                );
            }
        }

        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        ptr
    }

    /// `Class { field: value, ... }`: the same allocation as `new`, with the
    /// fields addressed by name instead of position.
    fn emit_lir_object_literal(
        &mut self,
        class: &str,
        fields: &[(String, HirExpr)],
    ) -> cranelift_codegen::ir::Value {
        let layout = self
            .class_layouts
            .get(class)
            .cloned()
            .expect("class layout vetted by LIR eligibility");
        let type_id = self.class_type_ids.get(class).copied().unwrap_or_else(|| {
            panic!(
                "compiler invariant violated: checked object literal class `{class}` has no type id"
            )
        });
        let gc_layout = GcLayoutMetadata::class(class, type_id, &layout, self.enum_infos);
        let ptr = self.emit_gc_alloc(gc_layout);
        self.emit_store_class_descriptor(ptr, class);
        self.emit_push_root(ptr);

        for (name, value) in fields {
            let idx = layout
                .iter()
                .position(|(n, _)| n == name)
                .expect("object-literal field vetted by LIR eligibility");
            let field_ty = layout[idx].1.clone();
            let val = self.emit_lir_store_value(value, &field_ty);
            self.emit_gc_heap_store(
                ptr,
                (idx as i32 + 1) * 8,
                val,
                &field_ty,
                GcStoreDestination::ObjectField,
            );
        }

        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        ptr
    }

    /// `object.field`: a plain load at `(index + 1) * 8` — word 0 holds the
    /// runtime type_id. The type system guarantees a direct class value here.
    fn emit_lir_field_access(
        &mut self,
        object: &HirExpr,
        field: &str,
    ) -> cranelift_codegen::ir::Value {
        let layout = self.lir_class_layout(&object.ty);
        let ptr = self.emit_lir_expr(object);
        let idx = layout
            .iter()
            .position(|(n, _)| n == field)
            .expect("field vetted by LIR eligibility");
        let load_ty = clif_type(&layout[idx].1);
        self.builder
            .ins()
            .load(load_ty, MemFlagsData::new(), ptr, (idx as i32 + 1) * 8)
    }

    /// `object.field = value;` through the GC write barrier, so an old-space
    /// object that gains a young reference lands in the remembered set.
    fn emit_lir_field_assign(&mut self, object: &HirExpr, field: &str, value: &HirExpr) {
        let layout = self.lir_class_layout(&object.ty);
        let idx = layout
            .iter()
            .position(|(n, _)| n == field)
            .expect("field vetted by LIR eligibility");
        let field_ty = layout[idx].1.clone();
        let ptr = self.emit_lir_expr(object);
        // The owner is evaluated first, so it needs a root whenever producing
        // the value — including the interface box a widening store adds on top
        // of it — can collect.
        let rooted = self.lir_value_allocates(value, &field_ty);
        if rooted {
            self.emit_push_root(ptr);
        }
        let val = self.emit_lir_store_value(value, &field_ty);
        self.emit_gc_heap_store(
            ptr,
            (idx as i32 + 1) * 8,
            val,
            &field_ty,
            GcStoreDestination::ObjectField,
        );
        if rooted {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
    }

    /// `object.method(args)` on a simple class. Eligibility guarantees the
    /// receiver's static type is exact (the class neither inherits nor is
    /// inherited from), so this is a direct call to `Class__method` and needs
    /// none of the AST path's runtime type_id dispatch chain.
    fn emit_lir_class_method(
        &mut self,
        object: &HirExpr,
        method: &str,
        args: &[HirExpr],
        ret_ty: &Type,
        span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let class = class_name_for_object_type(&object.ty)
            .expect("class receiver type vetted by LIR eligibility");
        let self_ptr = self.emit_lir_expr(object);
        let mangled = class_method_symbol_name(self.known_modules, &class, method);
        let fid = self.func_ids[&mangled];

        let pushed = self.emit_callstack_push(method, span);
        // The receiver may be a temporary (`make().m(alloc())`) reachable only
        // through this register; an allocating argument could collect it.
        self.emit_push_root(self_ptr);
        let params = self.method_param_types(&mangled);
        let (arg_vals, arg_roots) = self.emit_lir_args_rooted(args, params.as_deref());
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let mut call_args = vec![self_ptr];
        call_args.extend(arg_vals);
        let panic_depth = self.emit_pre_user_call_panic_depth(&mangled);
        let call = self.builder.ins().call(fref, &call_args);
        let result = self
            .builder
            .inst_results(call)
            .first()
            .copied()
            .unwrap_or_else(|| self.builder.ins().iconst(clif_type(ret_ty), 0));
        if pushed {
            self.emit_callstack_pop();
        }
        self.emit_pop_roots_n(arg_roots + 1);
        self.gc_root_count -= arg_roots + 1;
        self.emit_post_willow_call_panic_check(panic_depth);
        result
    }

    /// `iface.method(args)` on an interface-typed receiver: an indirect call
    /// through the receiver box's vtable (willow-0g8j.6).
    ///
    /// The box is `[object | vtable]`, so the concrete class is not known
    /// statically and there is nothing to inline: load the object and the
    /// vtable, load the slot's function pointer, and call it with the object as
    /// the hidden receiver. Eligibility proved the interface declares the method
    /// and so fixed its slot; the argument types come from the interface's
    /// declaration, which is what a class argument gets boxed against.
    ///
    /// The OBJECT is rooted across argument evaluation, not the box: the callee
    /// receives the object, and a box whose only reference is this register can
    /// be collected without harming the call. Mirrors
    /// [`FuncGen::emit_interface_dispatch`] on the AST path.
    fn emit_lir_interface_call(
        &mut self,
        object: &HirExpr,
        method: &str,
        args: &[HirExpr],
        span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let iface_name = class_name_for_object_type(&object.ty)
            .expect("interface receiver type vetted by LIR eligibility");
        let info = self
            .interface_infos
            .get(&iface_name)
            .cloned()
            .expect("interface info vetted by LIR eligibility");
        let slot = info
            .method_order
            .iter()
            .position(|n| n == method)
            .expect("interface method slot vetted by LIR eligibility");
        let sig_info = info.methods[method].clone();
        let param_types = sig_info.params.clone();
        let ret_type = sig_info.return_type.clone();

        let box_ptr = self.emit_lir_expr(object);
        self.emit_interface_dispatch_nil_check(box_ptr, object.span, method);
        // Install the method frame before validating the concrete object so a
        // invalid boxed receiver retains the same method context as the AST path.
        let pushed = self.emit_callstack_push(method, span);
        let obj = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), box_ptr, 0i32);
        self.emit_interface_dispatch_nil_check(obj, object.span, method);
        let vtable = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), box_ptr, 8i32);
        let fnptr =
            self.builder
                .ins()
                .load(types::I64, MemFlagsData::new(), vtable, (slot * 8) as i32);

        // The frame was installed before receiver validation and remains active
        // through arguments, matching the AST instance-method path.
        self.emit_push_root(obj);
        let (arg_vals, arg_roots) = self.emit_lir_args_rooted(args, Some(&param_types));

        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        for pt in &param_types {
            sig.params.push(AbiParam::new(clif_type(pt)));
        }
        if ret_type != Type::Void {
            sig.returns.push(AbiParam::new(clif_type(&ret_type)));
        }
        let sig_ref = self.builder.import_signature(sig);

        let mut call_args = vec![obj];
        call_args.extend(arg_vals);
        // Interface dispatch is indirect; one implementation body being safe
        // cannot prove the runtime-selected target safe.
        let panic_depth = self.emit_pre_willow_call_panic_depth();
        let call = self.builder.ins().call_indirect(sig_ref, fnptr, &call_args);
        let mut result = if ret_type != Type::Void {
            self.builder.inst_results(call)[0]
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };

        if pushed {
            self.emit_callstack_pop();
        }
        self.emit_pop_roots_n(arg_roots + 1);
        self.gc_root_count -= arg_roots + 1;
        self.emit_post_willow_call_panic_check(panic_depth);
        // `-> Self` yields a bare object of the receiver's own class. Re-box it
        // only after the panic edge has rejected the neutral placeholder.
        if matches!(&ret_type, Type::Named(n) if n == "Self") {
            result = self.emit_box_with_vtable(result, vtable);
        }
        result
    }

    /// `Class::method(args)`: class methods always carry a hidden receiver
    /// parameter, so a static call passes a null `self` — the same convention
    /// the AST path uses.
    fn emit_lir_static_call(
        &mut self,
        class: &str,
        method: &str,
        args: &[HirExpr],
        ret_ty: &Type,
        span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        // The one builtin constructor in the subset (willow-0g8j.7). It takes
        // no arguments and allocates nothing that needs rooting first.
        if class == "Map" && method == "new" {
            return self.emit_value_runtime_call("willow_map_new", &[]);
        }
        // `Enum::Variant(payload…)`, and the qualified fieldless form, which
        // HIR also spells as a zero-argument static call (willow-0g8j.8).
        if self.enum_infos.contains_key(class) {
            return self.emit_lir_enum_construction(class, method, args, ret_ty);
        }
        let mangled = class_method_symbol_name(self.known_modules, class, method);
        let fid = self.func_ids[&mangled];
        let dummy_self = self.builder.ins().iconst(types::I64, 0);
        let params = self.method_param_types(&mangled);
        let (arg_vals, arg_roots) = self.emit_lir_args_rooted(args, params.as_deref());
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let mut call_args = vec![dummy_self];
        call_args.extend(arg_vals);
        let pushed = self.emit_callstack_push(method, span);
        let panic_depth = self.emit_pre_user_call_panic_depth(&mangled);
        let call = self.builder.ins().call(fref, &call_args);
        let result = self
            .builder
            .inst_results(call)
            .first()
            .copied()
            .unwrap_or_else(|| self.builder.ins().iconst(clif_type(ret_ty), 0));
        if pushed {
            self.emit_callstack_pop();
        }
        if arg_roots > 0 {
            self.emit_pop_roots_n(arg_roots);
            self.gc_root_count -= arg_roots;
        }
        self.emit_post_willow_call_panic_check(panic_depth);
        result
    }

    /// `String` `+` / `==` / `!=`. Concatenation allocates; equality only
    /// compares bytes. The left operand is rooted across evaluation of the
    /// right operand because that expression may allocate. Keeping that root
    /// through the allocation-free equality call is conservative and mirrors
    /// the AST path.
    fn emit_lir_string_binop(
        &mut self,
        op: &BinOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> cranelift_codegen::ir::Value {
        let l = self.emit_lir_expr(lhs);
        self.emit_push_root(l);
        let r = self.emit_lir_expr(rhs);
        let (rt, roots) = match op {
            BinOp::Add => {
                // The concat call allocates, so the right operand must be a
                // root too — it may itself be a fresh temporary.
                self.emit_push_root(r);
                ("willow_string_concat", 2)
            }
            BinOp::Eq | BinOp::Ne => ("willow_string_eq", 1),
            _ => unreachable!("non-concat/compare string operator passed eligibility"),
        };
        let fid = self.func_id(rt);
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[l, r]);
        let raw = self.builder.inst_results(call)[0];
        self.emit_pop_roots_n(roots);
        self.gc_root_count -= roots;
        match op {
            BinOp::Add => raw,
            // `willow_string_eq` answers in a word; the language's bool is i8.
            BinOp::Eq => self.builder.ins().ireduce(types::I8, raw),
            _ => {
                let inv = self.builder.ins().bxor_imm_s(raw, 1);
                self.builder.ins().ireduce(types::I8, inv)
            }
        }
    }

    /// `[e0, e1, ...]`: allocate the handle, then fill it in place. Elements
    /// cross the runtime ABI as raw 64-bit words. The fresh array is rooted
    /// while the elements are evaluated, but only when one of them can actually
    /// collect — a list of constants cannot.
    fn emit_lir_array_literal(
        &mut self,
        elements: &[HirExpr],
        elem_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let len_val = self.builder.ins().iconst(types::I64, elements.len() as i64);
        let is_ref = i64::from(is_gc_managed(elem_ty, self.enum_infos));
        let is_ref_val = self.builder.ins().iconst(types::I64, is_ref);
        let arr = self.emit_value_runtime_call("willow_array_new", &[len_val, is_ref_val]);

        let rooted = elements
            .iter()
            .any(|el| self.lir_value_allocates(el, elem_ty));
        if rooted {
            self.emit_push_root(arr);
        }
        for (i, el) in elements.iter().enumerate() {
            // Each element is stored immediately, so it is only unrooted for
            // the allocation-free window between its own value and the `set`.
            let val = self.emit_lir_store_value(el, elem_ty);
            let word = self.coerce_to_i64(val, elem_ty);
            let idx_val = self.builder.ins().iconst(types::I64, i as i64);
            self.emit_void_runtime_call("willow_array_set", &[arr, idx_val, word]);
        }
        if rooted {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        arr
    }

    /// `arr[index]`: bounds-checked element read, converted back from the
    /// uniform 64-bit word to the element type.
    fn emit_lir_index(&mut self, array: &HirExpr, index: &HirExpr) -> cranelift_codegen::ir::Value {
        let elem_ty = array_element_type(&array.ty);
        let arr = self.emit_lir_expr(array);
        // The array may be a temporary (`build()[i]`) that nothing else roots.
        let rooted = may_allocate(index);
        if rooted {
            self.emit_push_root(arr);
        }
        let idx = self.emit_lir_expr(index);
        let word = self
            .emit_runtime_call_with_cleanup("willow_array_get", &[arr, idx], |this| {
                if rooted {
                    this.emit_pop_roots_n(1);
                    this.gc_root_count -= 1;
                }
            })
            .expect("willow_array_get returns a value");
        self.coerce_i64_to(word, &elem_ty)
    }

    /// `42.toString()` and friends. `String::toString` is the identity and
    /// emits no code at all, which is why the receiver's value is returned
    /// rather than passed through a runtime call.
    fn emit_lir_scalar_to_string(
        &mut self,
        object: &HirExpr,
        method: &str,
        args: &[HirExpr],
    ) -> cranelift_codegen::ir::Value {
        let intrinsic = intrinsics::resolve(&object.ty, method, args.len())
            .expect("scalar `toString` vetted by eligibility")
            .intrinsic;
        let value = self.emit_lir_expr(object);
        match intrinsic {
            Intrinsic::StringToString => value,
            Intrinsic::I64ToString => {
                self.emit_value_runtime_call("willow_i64_to_string", &[value])
            }
            Intrinsic::F64ToString => {
                self.emit_value_runtime_call("willow_f64_to_string", &[value])
            }
            Intrinsic::BoolToString => {
                self.emit_value_runtime_call("willow_bool_to_string", &[value])
            }
            other => unreachable!("`{other:?}` is not a scalar `toString`"),
        }
    }

    /// The builtin `Array<T>` methods admitted by eligibility. `push` can grow
    /// the buffer, but the runtime roots both the handle and the pushed word
    /// across that reallocation, and the handle pointer itself never moves.
    fn emit_lir_array_method(
        &mut self,
        object: &HirExpr,
        method: &str,
        args: &[HirExpr],
    ) -> cranelift_codegen::ir::Value {
        let elem_ty = array_element_type(&object.ty);
        let arr = self.emit_lir_expr(object);
        match method {
            "len" => self.emit_value_runtime_call("willow_array_len", &[arr]),
            "push" => {
                let rooted = self.lir_value_allocates(&args[0], &elem_ty);
                if rooted {
                    self.emit_push_root(arr);
                }
                let v = self.emit_lir_store_value(&args[0], &elem_ty);
                let word = self.coerce_to_i64(v, &elem_ty);
                self.emit_runtime_call_with_cleanup("willow_array_push", &[arr, word], |this| {
                    if rooted {
                        this.emit_pop_roots_n(1);
                        this.gc_root_count -= 1;
                    }
                });
                self.builder.ins().iconst(types::I8, 0) // void
            }
            "pop" => {
                let word = self.emit_value_runtime_call("willow_array_pop", &[arr]);
                self.coerce_i64_to(word, &elem_ty)
            }
            "toString" => {
                let kind = collection_elem_kind(&elem_ty)
                    .expect("array toString element kind vetted by eligibility");
                let kind_val = self.builder.ins().iconst(types::I64, kind);
                // The receiver is rooted across the call: it builds a String,
                // and a receiver that is only a temporary (`build().toString()`)
                // is otherwise reachable only from a machine register the
                // collector does not scan.
                self.emit_push_root(arr);
                let s = self.emit_runtime_call_with_cleanup(
                    "willow_array_to_string",
                    &[arr, kind_val],
                    |this| {
                        this.emit_pop_roots_n(1);
                        this.gc_root_count -= 1;
                    },
                );
                s.expect("willow_array_to_string returns a value")
            }
            // `arr.freeze()` -> an independent immutable copy over the same
            // runtime array representation (willow-dgwo.7). The copy allocates,
            // so the source is rooted across it for the same reason.
            "freeze" => {
                self.emit_push_root(arr);
                let copy =
                    self.emit_runtime_call_with_cleanup("willow_array_copy", &[arr], |this| {
                        this.emit_pop_roots_n(1);
                        this.gc_root_count -= 1;
                    });
                copy.expect("willow_array_copy returns a value")
            }
            _ => unreachable!("unsupported array method passed eligibility"),
        }
    }

    /// The builtin `Map<K, V>`, `FrozenMap<K, V>` and `FrozenArray<T>` methods
    /// admitted by eligibility (willow-0g8j.7).
    ///
    /// A frozen collection IS the collection it was frozen from as far as the
    /// runtime is concerned — `freeze` copies, and the copy has the same shape —
    /// so the read methods of both kinds lower to one call each. Keys and values
    /// cross as raw 64-bit words with an is-ref flag telling the runtime whether
    /// to treat the word as a GC reference; the flags are computed from the
    /// collection's DECLARED type arguments, never from the argument
    /// expression's, so a boxed interface value is still flagged as a reference.
    fn emit_lir_collection_method(
        &mut self,
        object: &HirExpr,
        method: &str,
        args: &[HirExpr],
    ) -> cranelift_codegen::ir::Value {
        let (kind, targs) =
            lir_collection(&object.ty).expect("collection receiver vetted by eligibility");
        let handle = self.emit_lir_expr(object);
        match (kind, method) {
            // A `FrozenArray<T>` is backed by the same handle as the `Array<T>`
            // it was frozen from, so its length is the array length.
            (LirCollection::FrozenArray, "len") => {
                self.emit_value_runtime_call("willow_array_len", &[handle])
            }
            (LirCollection::Map | LirCollection::FrozenMap, "len") => {
                self.emit_value_runtime_call("willow_map_len", &[handle])
            }
            (LirCollection::Map | LirCollection::FrozenMap, "contains") => {
                let key_ty = targs[0].clone();
                // The map is rooted only while an allocating key expression
                // runs; the lookup itself allocates nothing.
                let rooted = self.lir_value_allocates(&args[0], &key_ty);
                if rooted {
                    self.emit_push_root(handle);
                }
                let k = self.emit_lir_expr(&args[0]);
                let k_word = self.coerce_to_i64(k, &key_ty);
                let k_ref = self.map_is_ref_flag(&key_ty);
                let raw =
                    self.emit_value_runtime_call("willow_map_contains", &[handle, k_word, k_ref]);
                if rooted {
                    self.emit_pop_roots_n(1);
                    self.gc_root_count -= 1;
                }
                self.builder.ins().ireduce(types::I8, raw) // bool
            }
            (LirCollection::Map, "insert") => {
                let (key_ty, val_ty) = (targs[0].clone(), targs[1].clone());
                // Everything stays rooted until the insert returns. The map is
                // rooted because evaluating either argument can collect; the key
                // is rooted because evaluating the VALUE can; and both are rooted
                // across the call itself, which may grow the table and collect
                // before it has stored them (the AST path's rule, willow-oewp.6).
                self.emit_push_root(handle);
                let mut roots = 1usize;
                let k = self.emit_lir_expr(&args[0]);
                let k_word = self.coerce_to_i64(k, &key_ty);
                if is_gc_managed(&key_ty, self.enum_infos) {
                    self.emit_push_root(k);
                    roots += 1;
                }
                let v = self.emit_lir_store_value(&args[1], &val_ty);
                let v_word = self.coerce_to_i64(v, &val_ty);
                if is_gc_managed(&val_ty, self.enum_infos) {
                    self.emit_push_root(v);
                    roots += 1;
                }
                let k_ref = self.map_is_ref_flag(&key_ty);
                let v_ref = self.map_is_ref_flag(&val_ty);
                self.emit_runtime_call_with_cleanup(
                    "willow_map_insert",
                    &[handle, k_word, k_ref, v_word, v_ref],
                    |this| {
                        this.emit_pop_roots_n(roots);
                        this.gc_root_count -= roots;
                    },
                );
                self.builder.ins().iconst(types::I8, 0) // void
            }
            (LirCollection::Map | LirCollection::FrozenMap, "get") => {
                let (key_ty, val_ty) = (targs[0].clone(), targs[1].clone());
                // Rooted across the whole call, not just an allocating key: the
                // lookup itself allocates the `Option<V>` result, and a
                // temporary map reachable only here must survive that
                // collection or the value it holds is reclaimed
                // (willow-oewp.6).
                self.emit_push_root(handle);
                let k = self.emit_lir_expr(&args[0]);
                let k_word = self.coerce_to_i64(k, &key_ty);
                let k_ref = self.map_is_ref_flag(&key_ty);
                // The runtime builds the option, so it has to be told which
                // representation to build — the same decision `option_repr`
                // makes here and in `emit_lir_enum_construction`.
                let option_ty = Type::Generic("Option".to_string(), vec![val_ty]);
                let niche = self.builder.ins().iconst(
                    types::I64,
                    i64::from(
                        option_repr(&option_ty, self.enum_infos)
                            == Some(OptionRepr::NullableGcPointer),
                    ),
                );
                let got = self.emit_runtime_call_with_cleanup(
                    "willow_map_get",
                    &[handle, k_word, k_ref, niche],
                    |this| {
                        this.emit_pop_roots_n(1);
                        this.gc_root_count -= 1;
                    },
                );
                got.expect("willow_map_get returns a value")
            }
            (LirCollection::Map, "toString") => {
                let kind = collection_elem_kind(&targs[1])
                    .expect("map toString value kind vetted by eligibility");
                let kind_val = self.builder.ins().iconst(types::I64, kind);
                self.emit_push_root(handle);
                let s = self.emit_runtime_call_with_cleanup(
                    "willow_map_to_string",
                    &[handle, kind_val],
                    |this| {
                        this.emit_pop_roots_n(1);
                        this.gc_root_count -= 1;
                    },
                );
                s.expect("willow_map_to_string returns a value")
            }
            (LirCollection::Map, "freeze") => {
                self.emit_push_root(handle);
                let copy =
                    self.emit_runtime_call_with_cleanup("willow_map_copy", &[handle], |this| {
                        this.emit_pop_roots_n(1);
                        this.gc_root_count -= 1;
                    });
                copy.expect("willow_map_copy returns a value")
            }
            _ => unreachable!("unsupported collection method `{method}` passed eligibility"),
        }
    }

    /// The value-taking `Option`/`Result` methods, routed through the same
    /// helpers [`FuncGen::emit_option_result_method_call`] uses so the two
    /// emitters cannot disagree about the niche, the tag or the panic message
    /// (willow-0g8j.2.1).
    fn emit_lir_option_result_method(
        &mut self,
        object: &HirExpr,
        method: &str,
        args: &[HirExpr],
        span: Span,
    ) -> cranelift_codegen::ir::Value {
        const OK_TAG: i64 = 0;
        const ERR_TAG: i64 = 1;
        let resolved = builtin_types::resolve(&object.ty)
            .expect("Option/Result receiver vetted by eligibility");
        let id = resolved.id;
        let payload = |i: usize| resolved.args.get(i).cloned().unwrap_or(Type::Void);
        let (ok_ty, err_ty) = (payload(0), payload(1));
        let recv = self.emit_lir_expr(object);
        // Every branch below either allocates a panic message or evaluates an
        // argument that may allocate, and the receiver is otherwise live only
        // in an SSA register — so it is rooted for the whole method, exactly as
        // the AST path roots it.
        self.emit_push_root(recv);
        let value = match (id, method) {
            (B::Option, "is_some") => self.emit_option_is_some(recv, &ok_ty),
            (B::Option, "is_none") => {
                let some = self.emit_option_is_some(recv, &ok_ty);
                let zero = self.builder.ins().iconst(types::I8, 0);
                self.builder.ins().icmp(IntCC::Equal, some, zero)
            }
            (B::Option, "unwrap") => {
                let msg = self.emit_string_literal("called `Option::unwrap()` on a `None` value");
                self.emit_option_unwrap(recv, &ok_ty, msg, Some(span))
            }
            (B::Option, "expect") => {
                let msg = self.emit_lir_expr(&args[0]);
                self.emit_option_unwrap(recv, &ok_ty, msg, Some(span))
            }
            (B::Option, "unwrap_or") => {
                let default_val = self.emit_lir_expr(&args[0]);
                self.emit_option_unwrap_or(recv, &ok_ty, default_val)
            }
            (B::Result, "is_ok") | (B::Result, "is_err") => {
                let tag = self.emit_load_enum_tag(recv);
                let want = self
                    .builder
                    .ins()
                    .iconst(types::I64, if method == "is_ok" { OK_TAG } else { ERR_TAG });
                self.builder.ins().icmp(IntCC::Equal, tag, want)
            }
            (B::Result, "unwrap") => {
                let msg = self.emit_string_literal("called `Result::unwrap()` on an `Err` value");
                self.emit_enum_unwrap(recv, &ok_ty, OK_TAG, msg, Some(span))
            }
            (B::Result, "unwrap_err") => {
                let msg =
                    self.emit_string_literal("called `Result::unwrap_err()` on an `Ok` value");
                self.emit_enum_unwrap(recv, &err_ty, ERR_TAG, msg, Some(span))
            }
            (B::Result, "expect") => {
                let msg = self.emit_lir_expr(&args[0]);
                self.emit_enum_unwrap(recv, &ok_ty, OK_TAG, msg, Some(span))
            }
            (B::Result, "unwrap_or") => {
                let default_val = self.emit_lir_expr(&args[0]);
                self.emit_enum_unwrap_or(recv, &ok_ty, OK_TAG, default_val)
            }
            // The callable-taking combinators (willow-0g8j.2.2). The receiver
            // is already rooted above, which is what makes calling an arbitrary
            // function — and allocating the new enum around its result — safe
            // here. Each one routes into the SHARED emitter the AST path uses,
            // so the two paths cannot disagree about tag layout, the pointer
            // niche, or the indirect-call ABI.
            (B::Option, "map") => {
                let (f_val, f_ty) = self.emit_lir_fn_operand(&args[0]);
                let produced = fn_return_type(&f_ty);
                self.emit_option_map(recv, &ok_ty, &produced, f_val, &f_ty)
            }
            (B::Option, "and_then") => {
                let (f_val, f_ty) = self.emit_lir_fn_operand(&args[0]);
                self.emit_option_and_then(recv, &ok_ty, f_val, &f_ty)
            }
            (B::Option, "or_else") => {
                let (f_val, f_ty) = self.emit_lir_fn_operand(&args[0]);
                self.emit_option_or_else(recv, &ok_ty, f_val, &f_ty)
            }
            (B::Result, "map") => {
                let (f_val, f_ty) = self.emit_lir_fn_operand(&args[0]);
                let produced = fn_return_type(&f_ty);
                self.emit_result_map(recv, &ok_ty, &err_ty, &produced, f_val, &f_ty)
            }
            (B::Result, "map_err") => {
                let (f_val, f_ty) = self.emit_lir_fn_operand(&args[0]);
                let produced = fn_return_type(&f_ty);
                self.emit_result_map_err(recv, &ok_ty, &err_ty, &produced, f_val, &f_ty)
            }
            (B::Result, "and_then") => {
                let (f_val, f_ty) = self.emit_lir_fn_operand(&args[0]);
                self.emit_result_and_then(recv, &ok_ty, f_val, &f_ty)
            }
            (B::Result, "or_else") => {
                let (f_val, f_ty) = self.emit_lir_fn_operand(&args[0]);
                self.emit_result_or_else(recv, &err_ty, f_val, &f_ty)
            }
            _ => unreachable!("unsupported `{method}` on an Option/Result passed eligibility"),
        };
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        value
    }

    /// Emit a combinator's callable operand and hand back its function type,
    /// which the shared emitters need to build the indirect-call signature
    /// (willow-0g8j.2.2).
    fn emit_lir_fn_operand(&mut self, arg: &HirExpr) -> (cranelift_codegen::ir::Value, Type) {
        (self.emit_lir_expr(arg), arg.ty.clone())
    }

    /// The is-ref flag a map key or value crosses the ABI with: 1 when the word
    /// is a GC reference the collector must trace (and, for a key, the pointer
    /// the runtime reads a `String` out of), 0 when it is a plain word.
    fn map_is_ref_flag(&mut self, ty: &Type) -> cranelift_codegen::ir::Value {
        let flag = i64::from(is_gc_managed(ty, self.enum_infos));
        self.builder.ins().iconst(types::I64, flag)
    }

    fn emit_lir_binop(
        &mut self,
        op: &BinOp,
        l: cranelift_codegen::ir::Value,
        r: cranelift_codegen::ir::Value,
        float: bool,
    ) -> cranelift_codegen::ir::Value {
        let ins = self.builder.ins();
        if float {
            return match op {
                BinOp::Add => ins.fadd(l, r),
                BinOp::Sub => ins.fsub(l, r),
                BinOp::Mul => ins.fmul(l, r),
                BinOp::Div => ins.fdiv(l, r),
                BinOp::Rem => unreachable!("f64 % is rejected by the checker"),
                BinOp::Eq => ins.fcmp(FloatCC::Equal, l, r),
                BinOp::Ne => ins.fcmp(FloatCC::NotEqual, l, r),
                BinOp::Lt => ins.fcmp(FloatCC::LessThan, l, r),
                BinOp::Le => ins.fcmp(FloatCC::LessThanOrEqual, l, r),
                BinOp::Gt => ins.fcmp(FloatCC::GreaterThan, l, r),
                BinOp::Ge => ins.fcmp(FloatCC::GreaterThanOrEqual, l, r),
                BinOp::And | BinOp::Or => unreachable!("short-circuit ops rejected"),
                BinOp::Pow => unreachable!("`f64 **` is lowered by emit_pow_f64"),
            };
        }
        match op {
            BinOp::Add => ins.iadd(l, r),
            BinOp::Sub => ins.isub(l, r),
            BinOp::Mul => ins.imul(l, r),
            BinOp::Div => ins.sdiv(l, r),
            BinOp::Rem => ins.srem(l, r),
            BinOp::Eq => ins.icmp(IntCC::Equal, l, r),
            BinOp::Ne => ins.icmp(IntCC::NotEqual, l, r),
            BinOp::Lt => ins.icmp(IntCC::SignedLessThan, l, r),
            BinOp::Le => ins.icmp(IntCC::SignedLessThanOrEqual, l, r),
            BinOp::Gt => ins.icmp(IntCC::SignedGreaterThan, l, r),
            BinOp::Ge => ins.icmp(IntCC::SignedGreaterThanOrEqual, l, r),
            BinOp::And | BinOp::Or => unreachable!("short-circuit ops rejected"),
            // `i64 **` is lowered by `emit_pow_i64` before this table is
            // reached, because a dynamic exponent needs its own blocks.
            BinOp::Pow => unreachable!("`i64 **` is lowered by emit_pow_i64"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::cranelift::symbols::{backend_symbol_component, class_member_symbol};
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    /// The registration tables [`LirTypeCtx`] borrows, derived from a parsed
    /// single-file program the same way `compile_program` derives them: class
    /// layouts and bases from `class`/`extends`, interface and enum names from
    /// their declarations, and one signature per declared symbol (free
    /// functions, `Class__method`, and `Class__init` for an explicit
    /// constructor). Modules are out of scope for these unit tests, so
    /// `known_modules` is empty and mangling is the plain `Class__method` form.
    /// One declared interface method:
    /// `(name, parameter types, parameter modes, return type)`.
    type IfaceMethod = (String, Vec<Type>, Vec<ParamMode>, Type);

    struct TestTables {
        known: HashSet<String>,
        class_layouts: HashMap<String, Vec<(String, Type)>>,
        class_base: HashMap<String, String>,
        class_type_ids: HashMap<String, i64>,
        interfaces: HashSet<String>,
        /// Declared methods per interface, in declaration order — the order the
        /// backend turns into vtable slots. Inherited (`extends`) methods appear
        /// here only when the caller DESUGARED first, since composing them into
        /// `Interface::methods` is desugaring's job; [`checked_lowering`] does,
        /// the raw [`eligible`] path does not.
        iface_methods: HashMap<String, Vec<IfaceMethod>>,
        /// `(class, interface)` pairs that have a vtable, standing in for the
        /// backend's `vtable_ids`. Populated from every `implements` clause,
        /// which is what `compile_program` emits a vtable for.
        vtables: HashSet<(String, String)>,
        /// Declared enums, with the same tag rule `register_enum` uses:
        /// declaration order, starting at zero.
        enums: HashMap<String, LirEnumDef>,
        fn_types: FunctionMap<Type>,
        param_modes: FunctionMap<Vec<ParamMode>>,
        known_modules: HashMap<String, String>,
        /// Stands in for the per-function `LirTypeCtx::return_type`. Every
        /// entry point that takes a `LirFunction` rebinds it from that
        /// function, so this default is only what a direct `supported_expr`
        /// call sees.
        ret: Type,
        /// Lifted lambda symbols by the span of the lambda expression, standing
        /// in for the backend's `lambda_names` (willow-0g8j.2.2). Registered
        /// from the lowered IR's own lambda list, so a test's symbol table and
        /// its lambda bodies cannot describe different signatures.
        lambdas: HashMap<Span, String>,
    }

    impl TestTables {
        fn build(
            program: &crate::parser::ast::Program,
            extra_fns: &[&str],
            lambdas: &[crate::ir::lowered::LirLambda],
        ) -> Self {
            use crate::parser::ast::Item;
            let mut t = TestTables {
                known: extra_fns.iter().map(|s| s.to_string()).collect(),
                class_layouts: HashMap::new(),
                class_base: HashMap::new(),
                class_type_ids: HashMap::new(),
                interfaces: HashSet::new(),
                iface_methods: HashMap::new(),
                vtables: HashSet::new(),
                enums: HashMap::new(),
                fn_types: FunctionMap::default(),
                param_modes: FunctionMap::default(),
                known_modules: HashMap::new(),
                ret: Type::Void,
                lambdas: HashMap::new(),
            };
            // Every lifted lambda is a declared, linkable symbol with the
            // signature its lowered body carries — what `declare_lambda` does
            // in the backend.
            for l in lambdas {
                let name = l.function.name.clone();
                t.known.insert(name.clone());
                t.fn_types.insert(
                    &name,
                    Type::Fn(
                        l.function.params.iter().map(|p| p.ty.clone()).collect(),
                        Box::new(l.function.return_type.clone()),
                    ),
                );
                t.param_modes.insert(
                    &name,
                    l.function.params.iter().map(|_| ParamMode::Value).collect(),
                );
                t.lambdas.insert(l.span, name);
            }
            let sig = |params: &[crate::parser::ast::Param], ret: &Type, with_self: bool| {
                let mut ps: Vec<Type> = Vec::new();
                if with_self {
                    ps.push(Type::I64);
                }
                ps.extend(params.iter().map(|p| p.ty.clone()));
                Type::Fn(ps, Box::new(ret.clone()))
            };
            let modes = |params: &[crate::parser::ast::Param], with_self: bool| {
                let mut ms: Vec<ParamMode> = Vec::new();
                if with_self {
                    ms.push(ParamMode::Value);
                }
                ms.extend(params.iter().map(|p| p.mode.clone()));
                ms
            };
            // The prelude's enums (`Option`, `Result`, `IoError`, …) are
            // registered the way `register_prelude` registers them with the
            // checker, before the program's own items — so a test source that
            // declares an enum of the same name still shadows them, and the
            // walker sees the same enum table the real dispatch site builds
            // from `enum_infos` (willow-0g8j.2.1).
            let prelude_tokens = Lexer::new(crate::prelude::PRELUDE_SOURCE)
                .tokenize()
                .expect("prelude lexes");
            let (prelude, prelude_errs) = Parser::new(prelude_tokens).parse();
            assert!(prelude_errs.is_empty(), "{prelude_errs:?}");
            for item in prelude.items.iter().chain(program.items.iter()) {
                match item {
                    // A free function's SIGNATURE is always recorded, but its
                    // name is a known symbol only when the test lists it: that
                    // is how a test models a callee the backend cannot link.
                    Item::Function(f) => {
                        t.fn_types
                            .insert(&f.name, sig(&f.params, &f.return_type, false));
                        t.param_modes.insert(&f.name, modes(&f.params, false));
                    }
                    Item::Interface(i) => {
                        t.interfaces.insert(i.name.clone());
                        t.iface_methods.insert(
                            i.name.clone(),
                            i.methods
                                .iter()
                                .map(|m| {
                                    (
                                        m.name.clone(),
                                        m.params.iter().map(|p| p.ty.clone()).collect(),
                                        m.params.iter().map(|p| p.mode.clone()).collect(),
                                        m.return_type.clone(),
                                    )
                                })
                                .collect(),
                        );
                    }
                    Item::Enum(e) => {
                        t.enums.insert(
                            e.name.clone(),
                            LirEnumDef {
                                type_params: e.type_params.clone(),
                                variants: e
                                    .variants
                                    .iter()
                                    .map(|v| LirEnumVariant {
                                        name: v.name.clone(),
                                        payloads: v.payload.clone(),
                                    })
                                    .collect(),
                            },
                        );
                    }
                    Item::Class(c) => {
                        t.class_layouts.insert(
                            c.name.clone(),
                            c.fields
                                .iter()
                                .filter(|f| !f.is_static)
                                .map(|f| (f.name.clone(), f.ty.clone()))
                                .collect(),
                        );
                        if let Some(base) = &c.base_class {
                            t.class_base.insert(c.name.clone(), base.name().to_string());
                        }
                        for iface in &c.implements {
                            if let Type::Named(n) | Type::Generic(n, _) = iface {
                                t.vtables.insert((c.name.clone(), n.clone()));
                            }
                        }
                        // Same rule as `register_class`: one id per class, in
                        // declaration order.
                        let next_id = t.class_type_ids.len() as i64 + 1;
                        t.class_type_ids.entry(c.name.clone()).or_insert(next_id);
                        // Every class method — instance, static, or a
                        // constructor lowered to `init` — carries a hidden
                        // `self` parameter in its signature; a STATIC one is
                        // simply called with a null receiver. `func_param_modes`
                        // records only the declared parameters, matching
                        // `declare_class_methods`.
                        for ctor in &c.constructors {
                            let mangled =
                                class_member_symbol(&backend_symbol_component(&c.name), "init");
                            t.known.insert(mangled.clone());
                            t.fn_types
                                .insert(&mangled, sig(&ctor.params, &Type::Void, true));
                            t.param_modes.insert(&mangled, modes(&ctor.params, false));
                        }
                        for m in &c.methods {
                            let mangled =
                                class_member_symbol(&backend_symbol_component(&c.name), &m.name);
                            t.known.insert(mangled.clone());
                            t.fn_types
                                .insert(&mangled, sig(&m.params, &m.return_type, true));
                            t.param_modes.insert(&mangled, modes(&m.params, false));
                        }
                    }
                }
            }
            t
        }

        /// The closures in [`LirTypeCtx`] are borrowed, so the context cannot
        /// outlive this call — hand it to the caller instead of returning it.
        fn with_ctx<R>(&self, body: impl FnOnce(&LirTypeCtx<'_>) -> R) -> R {
            body(&LirTypeCtx {
                known_fn: &|n| self.known.contains(n),
                class_layouts: &self.class_layouts,
                class_base: &self.class_base,
                class_type_ids: &self.class_type_ids,
                is_interface: &|n| self.interfaces.contains(n),
                can_box: &|class, iface| {
                    self.vtables
                        .contains(&(class.to_string(), iface.to_string()))
                },
                enum_def: &|n| self.enums.get(n).cloned(),
                lambda_symbol: &|span| self.lambdas.get(&span).cloned(),
                iface_method: &|iface, method| {
                    let methods = self.iface_methods.get(iface)?;
                    let slot = methods.iter().position(|(n, _, _, _)| n == method)?;
                    let (_, params, modes, ret) = &methods[slot];
                    Some(IfaceMethodSig {
                        params: params.clone(),
                        modes: modes.clone(),
                        ret: ret.clone(),
                    })
                },
                fn_types: &self.fn_types,
                func_param_modes: &self.param_modes,
                known_modules: &self.known_modules,
                return_type: &self.ret,
            })
        }
    }

    fn eligible(src: &str, name: &str, fns: &[&str]) -> bool {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        assert!(diags.is_empty(), "{diags:?}");
        let p = crate::ir::lowered::lower_program(&hir);
        let tables = TestTables::build(&program, fns, &p.lambdas);
        let f = p.functions.iter().find(|f| f.name == name).unwrap();
        tables.with_ctx(|ctx| lir_supported_function(f, ctx))
    }

    /// Like [`eligible`], but for forms the HIR may refuse to lower at all: a
    /// function with no lowered IR is by definition not claimed by the LIR path.
    fn eligible_lenient(src: &str, name: &str, fns: &[&str]) -> bool {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        if !diags.is_empty() {
            return false;
        }
        let p = crate::ir::lowered::lower_program(&hir);
        let tables = TestTables::build(&program, fns, &p.lambdas);
        match p.functions.iter().find(|f| f.name == name) {
            Some(f) => tables.with_ctx(|ctx| lir_supported_function(f, ctx)),
            None => false,
        }
    }

    /// Parse, desugar and TYPE CHECK, then lower with the checker's side
    /// tables — the pipeline `compile_program` runs, minus module resolution.
    ///
    /// The plain [`eligible`] path lowers with empty `CheckerTables`, which is
    /// enough for anything the lowering can derive structurally. An interface
    /// method call is not: `lower_expr` finds no such method on the receiver's
    /// class and falls back to `tables.expr_type(span)`, so without the checker
    /// it fails with E0800 and the function never reaches the LIR at all.
    /// Desugaring matters for the same reason — it is what composes an
    /// `extends` interface's inherited methods into the list the vtable slots
    /// come from.
    fn checked_lowering(src: &str, fns: &[&str]) -> (crate::ir::lowered::LirProgram, TestTables) {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (mut program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        crate::desugar::DesugarPass::run(&mut program, &mut []);
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).expect("prelude");
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|d| d.severity == crate::diagnostics::Severity::Error)
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
        let tables = crate::ir::lower::CheckerTables::from_checker(&checker);
        let (hir, diags) = crate::ir::lower::lower_program_with(&program, &tables);
        assert!(diags.is_empty(), "{diags:?}");
        let lir = crate::ir::lowered::lower_program(&hir);
        let tables = TestTables::build(&program, fns, &lir.lambdas);
        (lir, tables)
    }

    /// [`eligible`] for constructs that need the checker's types to lower.
    fn eligible_checked(src: &str, name: &str, fns: &[&str]) -> bool {
        let (p, tables) = checked_lowering(src, fns);
        match p.functions.iter().find(|f| f.name == name) {
            Some(f) => tables.with_ctx(|ctx| lir_supported_function(f, ctx)),
            None => false,
        }
    }

    /// The fallback reason for `name`, through the same checked pipeline as
    /// [`eligible_checked`]. Panics if the function did not survive lowering,
    /// which would mean the test is exercising a HIR gap and not this code.
    fn reason_of(src: &str, name: &str, fns: &[&str]) -> Option<String> {
        let (p, tables) = checked_lowering(src, fns);
        let f = p
            .functions
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("`{name}` has no lowered IR"));
        tables.with_ctx(|ctx| lir_rejection_reason(f, ctx))
    }

    /// The reason as a string, asserting there IS one.
    fn rejected(src: &str, name: &str, fns: &[&str]) -> String {
        reason_of(src, name, fns).unwrap_or_else(|| panic!("`{name}` was accepted"))
    }

    /// The lowered function plus its (mutable) registration tables, for tests
    /// that need to perturb a table the way a registration or desugaring bug
    /// would and re-ask the predicate. Source alone cannot produce such a state
    /// — the type checker rejects it long before lowering.
    fn lir_fn_and_tables(src: &str, name: &str, fns: &[&str]) -> (LirFunction, TestTables) {
        let (p, tables) = checked_lowering(src, fns);
        let f = p
            .functions
            .iter()
            .find(|f| f.name == name)
            .expect("function present in lowered IR")
            .clone();
        (f, tables)
    }

    fn returned_hir_expr(src: &str) -> HirExpr {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        assert!(diags.is_empty(), "{diags:?}");
        let function = hir.functions.first().expect("one function");
        match function.body.last().expect("return statement") {
            HirStmt::Return {
                value: Some(value), ..
            } => value.clone(),
            other => panic!("expected value return, got {other:?}"),
        }
    }

    #[test]
    fn may_allocate_distinguishes_string_comparison_from_concatenation() {
        let eq = returned_hir_expr("fn f(a: String, b: String) -> bool { return a == b; }");
        let ne = returned_hir_expr("fn f(a: String, b: String) -> bool { return a != b; }");
        let concat = returned_hir_expr("fn f(a: String, b: String) -> String { return a + b; }");

        assert!(
            !may_allocate(&eq) && !may_allocate(&ne),
            "willow_string_eq is an allocation-free byte comparison"
        );
        assert!(
            may_allocate(&concat),
            "willow_string_concat allocates its result"
        );
    }

    // 1. a scalar arithmetic function is eligible
    #[test]
    fn e01_scalar_fn_eligible() {
        assert!(eligible(
            "fn add(a: i64, b: i64) -> i64 { return a + b; }",
            "add",
            &["add"]
        ));
    }

    // 2. recursive control flow (fib) is eligible
    #[test]
    fn e02_fib_eligible() {
        let src = "fn fib(n: i64) -> i64 { if n <= 1 { return n; } return fib(n-1) + fib(n-2); }";
        assert!(eligible(src, "fib", &["fib"]));
    }

    // 3. print of a scalar is eligible
    #[test]
    fn e03_scalar_print_eligible() {
        assert!(eligible(
            "fn show(n: i64) { println(n * 2); }",
            "show",
            &["show"]
        ));
    }

    // 4. (updated by willow-0g8j.1) string values became eligible with GC
    // rooting; kept as a positive check so a regression here is loud.
    #[test]
    fn e04_string_now_eligible() {
        assert!(eligible("fn s() { println(\"hi\"); }", "s", &["s"]));
    }

    // 5. (updated) short-circuit operators became eligible with lazy block
    // emission; kept as a positive check so a regression here is loud.
    #[test]
    fn e05_short_circuit_now_eligible() {
        assert!(eligible(
            "fn f(a: bool, b: bool) -> bool { return a && b; }",
            "f",
            &["f"]
        ));
    }

    // 6. unknown callees are not eligible
    #[test]
    fn e06_unknown_callee_ineligible() {
        assert!(!eligible(
            "fn g() -> i64 { return 1; } fn f() -> i64 { return g(); }",
            "f",
            &[] // g not in the known set
        ));
    }

    // 7. shadowing a let across sibling scopes is not eligible (flattened LIR)
    #[test]
    fn e07_shadowing_ineligible() {
        let src = "fn f(c: bool) -> i64 { let x = 1; if c { let x = 2; print(x); } return x; }";
        assert!(!eligible(src, "f", &["f"]));
    }

    // 8. while/for loops stay eligible (control flow is blocks, not exprs)
    #[test]
    fn e08_loops_eligible() {
        let src =
            "fn sum_to(n: i64) -> i64 { let mut t = 0; for i in 0..n { t = t + i; } return t; }";
        assert!(eligible(src, "sum_to", &["sum_to"]));
    }

    // 9. (updated by willow-0g8j.4) array-typed values are now eligible
    #[test]
    fn e09_arrays_now_eligible() {
        let src = "fn f() -> i64 { let xs = [1, 2]; return xs.len(); }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 10. f64 arithmetic + comparison is eligible
    #[test]
    fn e10_f64_eligible() {
        let src = "fn half(x: f64) -> bool { return x / 2.0 > 1.0; }";
        assert!(eligible(src, "half", &["half"]));
    }

    // 11. reference-mode parameters are rejected by the HIR itself — the
    // eligibility check never consults the AST declaration (willow-0g8j fix).
    #[test]
    fn e11_reference_params_ineligible_via_hir() {
        let src = "fn bump(n: &mut i64) { n = n + 1; }";
        assert!(!eligible(src, "bump", &["bump"]));
        let src2 = "fn read(n: &i64) -> i64 { return n; }";
        assert!(!eligible(src2, "read", &["read"]));
    }

    // 12. short-circuit && / || are now eligible (lazy block emission)
    #[test]
    fn e12_short_circuit_eligible() {
        assert!(eligible(
            "fn f(a: bool, b: bool) -> bool { return a && b || !a; }",
            "f",
            &["f"]
        ));
    }

    // 13. scalar ternaries are eligible
    #[test]
    fn e13_ternary_eligible() {
        assert!(eligible(
            "fn f(c: bool) -> i64 { return c ? 1 : 2; }",
            "f",
            &["f"]
        ));
    }

    // 14. (updated by willow-0g8j.1) a String ternary is now eligible
    #[test]
    fn e14_string_ternary_now_eligible() {
        let src = "fn f(c: bool) -> String { let s = c ? \"a\" : \"b\"; return s; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // ---------------------------------------------------------------------
    // willow-0g8j.1 — GC-managed values and rooting in the LIR walker.
    //
    // Perspectives 1-12 below are the *eligibility* half (which functions the
    // LIR path claims); perspectives 13-32 live in `tests/integration` as
    // differential and GC-stress runs, because they are about emitted code, not
    // about the predicate.
    //
    //  1. a String parameter/return function is eligible
    //  2. String concatenation is eligible
    //  3. String equality/inequality is eligible
    //  4. `println` of a String is eligible
    //  5. a String ternary is eligible
    //  6. mixed scalar + String locals in one function are eligible
    //  7. a String `let` that is reassigned in a loop is eligible
    //  8. calling a String-returning function is eligible
    //  9. a `let` shadowing a PARAMETER is rejected (flattened scopes)
    // 10. a bare enum variant `Var` is rejected (needs the AST special case)
    // 11. an unsupported String operator (`<`) is rejected
    // 12. arrays / class objects / interfaces still fall back
    // ---------------------------------------------------------------------

    // 15. String parameters and returns are eligible
    #[test]
    fn e15_string_param_and_return_eligible() {
        let src = "fn id(s: String) -> String { return s; }";
        assert!(eligible(src, "id", &["id"]));
    }

    // 16. concatenation of strings is eligible
    #[test]
    fn e16_string_concat_eligible() {
        let src = "fn join(a: String, b: String) -> String { return a + b; }";
        assert!(eligible(src, "join", &["join"]));
    }

    // 17. string equality and inequality are eligible
    #[test]
    fn e17_string_compare_eligible() {
        let eq = "fn f(a: String, b: String) -> bool { return a == b; }";
        assert!(eligible(eq, "f", &["f"]));
        let ne = "fn f(a: String, b: String) -> bool { return a != b; }";
        assert!(eligible(ne, "f", &["f"]));
    }

    // 18. a String local reassigned inside a loop is eligible — the case the
    // entry-rooted slot design exists for (a per-`let` root would grow the
    // shadow stack once per iteration).
    #[test]
    fn e18_string_loop_accumulator_eligible() {
        let src = "fn rep(n: i64) -> String { let mut s = \"\"; let mut i = 0; \
                   while i < n { s = s + \"x\"; i = i + 1; } return s; }";
        assert!(eligible(src, "rep", &["rep"]));
    }

    // 19. mixed scalar and String locals in one function are eligible
    #[test]
    fn e19_mixed_scalar_and_gc_eligible() {
        let src = "fn f(n: i64) -> String { let tag = \"n=\"; let doubled = n * 2; \
                   let ok = doubled > 0; return ok ? tag : \"\"; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 20. a call that both takes and returns a String is eligible
    #[test]
    fn e20_string_call_eligible() {
        let src = "fn wrap(s: String) -> String { return \"[\" + s + \"]\"; } \
                   fn f() -> String { return wrap(\"a\"); }";
        assert!(eligible(src, "f", &["f", "wrap"]));
    }

    // 21. a `let` shadowing a PARAMETER is rejected: LIR has no block scopes,
    // so one name cannot have two storages (willow-0g8j.1).
    #[test]
    fn e21_let_shadowing_param_ineligible() {
        let src = "fn f(s: String) -> String { let s = \"other\"; return s; }";
        assert!(!eligible(src, "f", &["f"]));
    }

    // 22. (updated by willow-0g8j.8) the QUALIFIED variant form is now emitted,
    // so it is eligible. The bare form is still checked here because it does not
    // survive HIR lowering at all: without that guard a future lowering change
    // could quietly hand the walker a `Var` it would resolve to a local (the
    // `names` guard in `lir_supported_function` is the backstop).
    #[test]
    fn e22_bare_enum_variant_never_reaches_walker() {
        let bare = "enum Status { Open, Closed } fn f() -> Status { return Closed; }";
        let tokens = Lexer::new(bare).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (_, diags) = crate::ir::lower::lower_program(&program);
        assert!(!diags.is_empty(), "bare variant unexpectedly lowered");

        let qualified = "enum Status { Open, Closed } fn f() -> Status { return Status::Closed; }";
        assert!(eligible_checked(qualified, "f", &["f"]));
    }

    // 23. an ordering operator on strings is not emitted, so it is rejected
    // even though both operand types are supported.
    #[test]
    fn e23_string_ordering_ineligible() {
        let src = "fn f(a: String, b: String) -> bool { return a < b; }";
        // The checker may reject this outright; if it lowers, we must not claim it.
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 24. (updated by willow-0g8j.4) arrays of strings are now eligible
    #[test]
    fn e24_string_array_now_eligible() {
        let src = "fn f() -> i64 { let xs = [\"a\", \"b\"]; return xs.len(); }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 25. (updated by willow-0g8j.5) a static call returning a class object is
    // now claimed by the LIR path
    #[test]
    fn e25_class_object_now_eligible() {
        let src = "class Item { name: String; pub static fn make(n: String) -> Item \
                   { return new Item(n); } } \
                   fn f() -> Item { return Item::make(\"a\"); }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // ---------------------------------------------------------------------
    // willow-0g8j.4 — `Array<T>` in the LIR walker.
    //
    // Perspectives 1-15 below are the *eligibility* half (which functions the
    // LIR path claims); perspectives 16-38 live in `tests/integration` as
    // differential and `WILLOW_GC_STRESS=alloc` runs, because they are about
    // emitted code and rooting, not about the predicate.
    //
    //  1. an array literal + `len()` is eligible
    //  2. array parameters and array returns are eligible
    //  3. indexing an array is eligible
    //  4. index-assignment (`a[i] = v`) is eligible
    //  5. `push` / `pop` are eligible
    //  6. `toString()` is eligible for every renderable element kind
    //  7. an array of `String` is eligible (GC element type)
    //  8. `Array<Array<i64>>` is eligible (element type checked recursively)
    //  9. an empty array literal never reaches the predicate: HIR lowering
    //     rejects it before the walker sees the function
    // 10. `for x in arr` is eligible (desugars to `len`/index)
    // 11. an array of class objects is rejected (no interface boxing here)
    // 12. an unsupported array method (`freeze`) is rejected
    // 13. a `FrozenArray<T>` receiver is rejected (different runtime call)
    // 14. a `Map<K, V>` receiver/index is rejected
    // 15. `toString()` on a non-renderable element type is rejected
    // ---------------------------------------------------------------------

    // 26. array parameters and array returns are eligible
    #[test]
    fn e26_array_param_and_return_eligible() {
        let src = "fn f(xs: Array<i64>) -> Array<i64> { return xs; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 27. reading through an index is eligible
    #[test]
    fn e27_array_index_eligible() {
        let src = "fn f(xs: Array<i64>, i: i64) -> i64 { return xs[i]; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 28. index-assignment is eligible (a LIR instruction, not an expression)
    #[test]
    fn e28_index_assign_eligible() {
        let src = "fn f() -> i64 { let mut xs = [1, 2]; xs[0] = 9; return xs[0]; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 29. push and pop are eligible
    #[test]
    fn e29_push_pop_eligible() {
        let src = "fn f() -> i64 { let mut xs = [1]; xs.push(2); return xs.pop(); }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 30. toString is eligible for each element kind the runtime can render
    #[test]
    fn e30_to_string_eligible_for_scalar_kinds() {
        for (decl, lit) in [
            ("i64", "[1, 2]"),
            ("f64", "[1.5]"),
            ("bool", "[true]"),
            ("String", "[\"a\"]"),
        ] {
            let src = format!(
                "fn f() -> String {{ let xs: Array<{decl}> = {lit}; return xs.toString(); }}"
            );
            assert!(eligible(&src, "f", &["f"]), "{decl} array toString");
        }
    }

    // 31. an `Array<Array<i64>>` is eligible: the element type is itself checked
    #[test]
    fn e31_nested_array_eligible() {
        let src = "fn f() -> i64 { let xs = [[1, 2], [3]]; return xs[0][1]; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 32. an empty literal never reaches the walker: the HIR lowering refuses
    // it (there is no first element to take the type from), so the function has
    // no LIR at all and stays on the AST path.
    #[test]
    fn e32_empty_array_never_reaches_walker() {
        let src = "fn f() -> i64 { let mut xs: Array<i64> = []; xs.push(1); return xs.len(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 33. `for x in arr` is eligible: the LIR desugars it into `len()` + index
    #[test]
    fn e33_for_over_array_eligible() {
        let src =
            "fn f(xs: Array<i64>) -> i64 { let mut t = 0; for x in xs { t = t + x; } return t; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 34. arrays of SIMPLE class objects are eligible since willow-0g8j.5 (the
    // element is a plain GC handle); an array of an INTERFACE element type
    // joined them in willow-j260, once the walker learned to box on the way in.
    #[test]
    fn e34_class_and_interface_element_arrays_eligible() {
        let src = "class Item { pub name: String; } \
                   fn f(xs: Array<Item>) -> i64 { return xs.len(); }";
        assert!(eligible_lenient(src, "f", &["f"]));

        let iface = "interface Named { fn name(self) -> String; } \
                     fn f(xs: Array<Named>) -> i64 { return xs.len(); }";
        assert!(eligible_lenient(iface, "f", &["f"]));
    }

    // 35. (updated by willow-0g8j.7) `freeze` completed the array method
    // surface — `len`/`push`/`pop`/`toString`/`freeze` is all of it, so there is
    // no longer an array method that falls back. Kept as a positive check.
    #[test]
    fn e35_array_freeze_now_eligible() {
        let src = "fn f() -> i64 { let xs = [1, 2]; let ys = xs.freeze(); return ys.len(); }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // 36. (updated by willow-0g8j.7) a `Map<K, V>` receiver is not an ARRAY
    // receiver, but it is now a receiver the walker claims in its own right.
    #[test]
    fn e36_map_now_eligible() {
        let src = "import std::collections::Map; \
                   fn f() -> i64 { let m: Map<String, i64> = Map::new(); \
                   m.insert(\"a\", 1); return m.len(); }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // 37. `toString()` on an element type the runtime cannot render falls back
    #[test]
    fn e37_nested_array_to_string_ineligible() {
        let src = "fn f() -> String { let xs = [[1], [2]]; return xs.toString(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // ---------------------------------------------------------------------
    // willow-0g8j.5 — class objects and field access in the LIR walker.
    //
    // A "simple" class is the subset the walker claims: no base class, not
    // itself a base, not an interface or enum, a known field layout, and every
    // field type supported. Anything else — inheritance (virtual dispatch),
    // interfaces (boxing), enums (payload layout) — stays on the AST path.
    //
    // Perspectives 1-26 below are the *eligibility* half; perspectives 27-46
    // live in `tests/integration/codegen.rs` as differential and
    // `WILLOW_GC_STRESS=alloc` runs, because they are about the emitted code
    // and its GC rooting, not about the predicate.
    //
    //  1. a class-typed parameter and return is eligible
    //  2. `new C(..)` through the implicit memberwise constructor is eligible
    //  3. `new C(..)` through an explicit `init` is eligible
    //  4. an object literal `C { f: v }` is eligible
    //  5. an object literal missing a declared field is not claimed
    //  6. reading a field is eligible
    //  7. assigning a field is eligible
    //  8. a chained field read (`a.b.c`) is eligible
    //  9. an instance method call is eligible
    // 10. a static method call is eligible
    // 11. a GC-managed (`String`) field is eligible
    // 12. an `Array<T>` field is eligible
    // 13. a class local declared before a `while` keeps its entry root slot
    // 14. a subclass (`extends`) is rejected — virtual dispatch
    // 15. a base class (something extends it) is rejected — callers may be
    //     holding a subclass instance whose layout differs
    // 16. an interface-typed field (willow-j260 flipped this to eligible: the
    //     store into it boxes)
    // 17. an enum-typed field is rejected
    // 18. DISPATCHING through an interface-typed parameter is rejected
    // 19. `let x: Iface = new C();` followed by a dispatch is rejected — the
    //     BINDING type widens, which willow-j260 made emittable, but the
    //     virtual call on it is still out of subset
    // 20. a method with a `&mut` parameter is rejected (mode check)
    // 21. dispatching on the interface a method returned is rejected
    // 22. a self-referential field type is eligible — the support check is
    //     cycle-safe and must not recurse forever
    // 23. an array of simple class objects with a field read is eligible
    // 24. an OPTIONAL class type (`Node?` = `Option<Node>`) is rejected
    //     everywhere it appears because generic enums remain outside this stage
    // 25. a base class reached under an IMPORT ALIAS is still rejected: class
    //     identity is the runtime `type_id`, not the name
    // 26. an object literal naming the same field twice is rejected, not just
    //     one with the wrong field COUNT
    // ---------------------------------------------------------------------

    /// A minimal simple class, reused by the perspectives below.
    const POINT: &str = "class Point { pub x: i64; pub y: i64; } ";

    // 38. a class-typed parameter and a class-typed return are eligible
    #[test]
    fn e38_class_param_and_return_eligible() {
        let src = format!("{POINT} fn f(p: Point) -> Point {{ return p; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 39. `new` through the implicit memberwise constructor is eligible
    #[test]
    fn e39_new_memberwise_eligible() {
        let src = format!("{POINT} fn f() -> i64 {{ let p = new Point(1, 2); return p.x + p.y; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 40. `new` through an explicit `init` constructor is eligible
    #[test]
    fn e40_new_explicit_init_eligible() {
        let src = "class Counter { pub n: i64; \
                   pub init(self, n: i64) { self.n = n; } } \
                   fn f() -> i64 { let c = new Counter(7); return c.n; }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 41. an object literal is eligible: it lowers to the same field stores.
    //
    // The type checker rejects `C { f: v }` in source today (check_ops.rs:
    // "named field syntax is part of the old construction form"), so this and
    // the next perspective exercise the walker's handling directly from HIR —
    // the eligibility predicate must stay consistent with the emitter for the
    // node it can still be handed.
    #[test]
    fn e41_object_literal_eligible() {
        let src =
            format!("{POINT} fn f() -> i64 {{ let p = Point {{ x: 1, y: 2 }}; return p.y; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 42. an object literal that omits a declared field is not claimed: the
    // walker only emits a complete memberwise initialisation.
    #[test]
    fn e42_partial_object_literal_ineligible() {
        let src = format!("{POINT} fn f() -> i64 {{ let p = Point {{ x: 1 }}; return p.x; }}");
        assert!(!eligible_lenient(&src, "f", &["f"]));
    }

    // 43. reading a field is eligible
    #[test]
    fn e43_field_read_eligible() {
        let src = format!("{POINT} fn f(p: Point) -> i64 {{ return p.x; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 44. assigning a field is eligible (a LIR instruction, not an expression)
    #[test]
    fn e44_field_assign_eligible() {
        let src =
            format!("{POINT} fn f() -> i64 {{ let p = new Point(1, 2); p.x = 9; return p.x; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 45. a chained field read walks two statically non-optional objects
    #[test]
    fn e45_nested_field_read_eligible() {
        let src = "class Inner { pub v: i64; } class Outer { pub inner: Inner; } \
                   fn f(o: Outer) -> i64 { return o.inner.v; }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 46. an instance method call is eligible: a direct call to `Class__method`
    #[test]
    fn e46_instance_method_call_eligible() {
        let src = "class Counter { pub n: i64; \
                   pub fn get(self) -> i64 { return self.n; } } \
                   fn f(c: Counter) -> i64 { return c.get(); }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 47. a static method call is eligible (a null receiver is passed)
    #[test]
    fn e47_static_method_call_eligible() {
        let src = "class Counter { pub n: i64; \
                   pub static fn zero() -> i64 { return 0; } } \
                   fn f() -> i64 { return Counter::zero(); }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 48. a GC-managed field type is eligible: the store goes through the
    // object-field write path, not a plain store
    #[test]
    fn e48_string_field_eligible() {
        let src = "class Item { pub name: String; } \
                   fn f() -> String { let i = new Item(\"a\"); i.name = \"b\"; return i.name; }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 49. an `Array<T>` field is eligible: the element type is checked too
    #[test]
    fn e49_array_field_eligible() {
        let src = "class Bag { pub xs: Array<i64>; } \
                   fn f(b: Bag) -> i64 { return b.xs.len(); }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 50. a class local that lives across a loop is eligible: its root slot is
    // allocated once at entry, so the shadow stack does not grow per iteration
    #[test]
    fn e50_class_local_across_loop_eligible() {
        let src = format!(
            "{POINT} fn f() -> i64 {{ let p = new Point(0, 0); let mut i = 0; \
             while i < 3 {{ p.x = p.x + i; i = i + 1; }} return p.x; }}"
        );
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 51. a subclass is rejected: its methods dispatch virtually
    #[test]
    fn e51_subclass_ineligible() {
        let src = "pub open class Animal { pub age: i64; } \
                   pub class Dog extends Animal { } \
                   fn f(d: Dog) -> i64 { return d.age; }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 52. a base class is rejected too: a caller may hand it a subclass
    // instance, whose field layout the walker never sees
    #[test]
    fn e52_base_class_ineligible() {
        let src = "pub open class Animal { pub age: i64; } \
                   pub class Dog extends Animal { } \
                   fn f(a: Animal) -> i64 { return a.age; }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 53. an interface-typed field was rejected while the walker emitted no
    // boxing; since willow-j260 it is a supported field type (the store into it
    // boxes), so a class that has one is still SIMPLE.
    #[test]
    fn e53_interface_field_eligible() {
        let src = "interface Named { fn name(self) -> String; } \
                   class Holder { pub n: Named; } \
                   fn f(h: Holder) -> i64 { return 1; }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 54. (updated by willow-0g8j.8) an enum-typed field was rejected while the
    // walker had no enum layout; now that it does, a class holding one is SIMPLE
    // and reading the field is eligible.
    #[test]
    fn e54_enum_field_eligible() {
        let src = "enum Color { Red, Green } \
                   class Holder { pub c: Color; } \
                   fn f(h: Holder) -> i64 { return 1; }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // 55. DISPATCHING through an interface parameter is rejected. Since
    // willow-j260 the parameter TYPE is fine (see j03); it is the virtual call
    // through the box's vtable that the walker does not emit (willow-0g8j.6).
    #[test]
    fn e55_interface_param_ineligible() {
        let src = "interface Named { fn name(self) -> String; } \
                   fn f(n: Named) -> String { return n.name(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 56. a widening `let` annotation whose value is then DISPATCHED on is
    // rejected. The widening itself is emittable since willow-j260 (see j01) —
    // and it is exactly the case that makes `HirStmt::Let::ty` rather than
    // `value.ty` the type the walker must trust — but `x.name()` is not.
    #[test]
    fn e56_widening_let_annotation_ineligible() {
        let src = "interface Named { fn name(self) -> String; } \
                   class Item implements Named { pub n: String; \
                   pub fn name(self) -> String { return self.n; } } \
                   fn f() -> String { let x: Named = new Item(\"a\"); return x.name(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 57. a method with a by-reference parameter is rejected: the walker only
    // passes values, so it must never claim a callee expecting an address
    #[test]
    fn e57_reference_param_method_ineligible() {
        let src = "class Counter { pub n: i64; \
                   pub fn bump(self, v: &mut i64) { v = v + 1; } } \
                   fn f(c: Counter) -> i64 { let mut k = 1; c.bump(&k); return k; }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 58. calling a method ON the interface a method returned is rejected. The
    // interface-returning method itself is fine since willow-j260 (see j05);
    // the second `.name()` hop is the virtual dispatch that is not.
    #[test]
    fn e58_interface_returning_method_ineligible() {
        let src = "interface Named { fn name(self) -> String; } \
                   class Item implements Named { pub n: String; \
                   pub fn name(self) -> String { return self.n; } \
                   pub fn as_named(self) -> Named { return self; } } \
                   fn f(i: Item) -> String { return i.as_named().name(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 59. a self-referential field type must not send the support check into
    // infinite recursion; a linked node is a perfectly ordinary GC handle
    #[test]
    fn e59_self_referential_class_eligible() {
        let src = "class Node { pub v: i64; pub next: Node; } \
                   fn f(n: Node) -> i64 { return n.v; }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 60. an array of simple class objects, indexed and field-read, is eligible
    #[test]
    fn e60_class_array_field_read_eligible() {
        let src = format!("{POINT} fn f(ps: Array<Point>) -> i64 {{ return ps[0].x; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 61. Option-wrapped class types are in the subset as of willow-0g8j.2.1:
    // `Node?` is canonical `Option<Node>`, an enum instance whose payload is a
    // simple class. The self-reference through the option (`next: Node?`) must
    // still terminate the type walk, exactly as the direct one in perspective
    // 59 does.
    #[test]
    fn e61_optional_class_eligible() {
        let field = "class Node { pub v: i64; pub next: Node?; } \
                     fn f(n: Node) -> i64 { return n.v; }";
        assert!(eligible_lenient(field, "f", &["f"]));

        let param = format!("{POINT} fn f(p: Point?) -> i64 {{ return 1; }}");
        assert!(eligible_lenient(&param, "f", &["f"]));

        // An option over a type OUTSIDE the subset is still out: the payload is
        // vetted like any other storage type.
        let inherited = "open class Base { pub v: i64; } class Sub extends Base {} \
                         fn f(b: Base?) -> i64 { return 1; }";
        assert!(!eligible_lenient(inherited, "f", &["f"]));
    }
    // 62. a class reached through a DIRECT TYPE IMPORT is the same class as its
    // module-qualified self, so an imported base class must still be rejected.
    // `import zoo::Animal;` registers the class a second time under `Animal`,
    // sharing the canonical `type_id`, while `class_base` keeps canonical names
    // on both sides (`zoo::Dog` -> `zoo::Animal`). Comparing names alone would
    // call `Animal` a leaf and emit a direct `Animal__speak` for a receiver that
    // is really a `Dog`.
    #[test]
    fn e62_imported_base_class_alias_ineligible() {
        let src = "class Animal { pub value: i64; } fn f(a: Animal) -> i64 { return a.value; }";
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        assert!(diags.is_empty(), "{diags:?}");
        let p = crate::ir::lowered::lower_program(&hir);
        let f = p.functions.iter().find(|f| f.name == "f").unwrap();

        let mut tables = TestTables::build(&program, &["f"], &[]);
        // Without the module in the picture, `Animal` is an ordinary leaf.
        assert!(tables.with_ctx(|ctx| lir_supported_function(f, ctx)));

        // Now model the import: `Animal` is an alias of `zoo::Animal`, which
        // `zoo::Dog` extends. Only the canonical names appear in `class_base`.
        let animal_id = tables.class_type_ids["Animal"];
        tables
            .class_type_ids
            .insert("zoo::Animal".to_string(), animal_id);
        tables
            .class_type_ids
            .insert("zoo::Dog".to_string(), animal_id + 100);
        tables
            .class_base
            .insert("zoo::Dog".to_string(), "zoo::Animal".to_string());
        assert!(
            !tables.with_ctx(|ctx| lir_supported_function(f, ctx)),
            "an imported base class must not be treated as a leaf"
        );
    }

    // 63. an object literal that names the same field twice is rejected even
    // though the COUNT matches the layout: the emitter would store into that
    // field twice and leave the other one at its zero value.
    #[test]
    fn e63_object_literal_duplicate_field_ineligible() {
        let src =
            format!("{POINT} fn f() -> i64 {{ let p = Point {{ x: 1, x: 2 }}; return p.x; }}");
        assert!(!eligible_lenient(&src, "f", &["f"]));
    }

    // 64. an array handle's element contract is part of its representation:
    // scalar/reference classification and class/interface boxing differ even
    // though every source-level Array value is carried as one pointer.
    #[test]
    fn e64_array_representation_requires_exact_element_type() {
        let array = |element| Type::Array(Box::new(element));
        assert!(assignable_repr(&array(Type::I64), &array(Type::I64)));
        assert!(!assignable_repr(&array(Type::I64), &array(Type::String)));
        assert!(!assignable_repr(
            &array(Type::Named("Point".to_string())),
            &array(Type::Named("Other".to_string()))
        ));
        assert!(!assignable_repr(&array(Type::String), &array(Type::Void)));
    }

    // ---------------------------------------------------------------------
    // willow-j260 — class → interface boxing coercion in the LIR walker.
    //
    // An interface value is a 16-byte `[object | vtable]` GC box, so putting a
    // class instance in an interface-typed slot is a conversion, not a
    // reinterpretation. The walker now emits it at every STORE position, and
    // only there; reading THROUGH an interface (dispatch, field access) is
    // still the AST path's job (willow-0g8j.6).
    //
    // Perspectives j01-j21 below are the eligibility half; j22-j36 live in
    // `tests/integration/codegen.rs` as differential and
    // `WILLOW_GC_STRESS=alloc` runs, because they are about the emitted code
    // and its GC rooting.
    //
    // j01. `let x: Iface = new C();` — widening let init
    // j02. `x = new C();` — widening assignment to an interface local
    // j03. an interface-typed parameter, passed along without dispatching
    // j04. a class argument boxed into an interface parameter
    // j05. `return new C();` from an interface-returning function
    // j06. `h.n = new C();` — widening store into an interface-typed field
    // j07. `let xs: Array<Iface> = [new C()]` is REJECTED — an array literal is
    //      typed by its elements and there is no per-handle conversion
    // j08. `xs.push(new C())` on an `Array<Iface>`
    // j09. `new Holder(new C())` — memberwise constructor field boxing
    // j10. an explicit `init` with an interface parameter
    // j11. a static method with an interface parameter
    // j12. reading an interface-typed field is eligible (a plain load)
    // j13. `xs[0] = new C();` — index-assign into an `Array<Iface>`
    // j14. an interface value stored into the SAME interface needs no box
    // j15. a class with no vtable for that interface is rejected — the
    //      emitter's fallback is to pass the object through UNBOXED
    // j16. a class taking part in inheritance cannot be boxed by the walker
    // j17. interface → a DIFFERENT interface is rejected (no re-boxing)
    // j18. a generic interface instantiation (`Box<String>`) is rejected
    // j19. an optional interface (`Iface?`) is rejected
    // j20. a ternary whose arms are classes but whose type is the interface is
    //      rejected: both arms feed one variable and neither gets boxed
    // j21. `Array<Iface>.toString()` is rejected — no element kind
    // ---------------------------------------------------------------------

    /// An interface, a simple class implementing it, and a holder class with an
    /// interface-typed field. Reused by the perspectives below.
    const NAMED: &str = "interface Named { fn name(self) -> String; } \
                         class Item implements Named { pub n: String; \
                         pub fn name(self) -> String { return self.n; } } \
                         class Holder { pub n: Named; } ";

    // j01. a widening `let` initialiser is eligible on its own
    #[test]
    fn j01_widening_let_eligible() {
        let src = format!(
            "{NAMED} fn f() -> i64 {{ let x: Named = new Item(\"a\"); let y = 1; return y; }}"
        );
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // j02. a widening assignment to an interface-typed local is eligible
    #[test]
    fn j02_widening_assign_eligible() {
        let src = format!(
            "{NAMED} fn f(seed: Named) -> i64 {{ let mut x: Named = seed; \
             x = new Item(\"a\"); return 1; }}"
        );
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // j03. an interface-typed parameter is fine as long as nothing dispatches
    // on it: it is a GC handle like any other (contrast e55).
    #[test]
    fn j03_interface_param_passthrough_eligible() {
        let src = format!("{NAMED} fn f(n: Named) -> Named {{ return n; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // j04. a class argument is boxed into an interface parameter at the call
    #[test]
    fn j04_boxed_call_argument_eligible() {
        let src = format!(
            "{NAMED} fn g(n: Named) -> i64 {{ return 1; }} \
             fn f() -> i64 {{ return g(new Item(\"a\")); }}"
        );
        assert!(eligible_lenient(&src, "f", &["f", "g"]));
    }

    // j05. `return new Item(..)` from an interface-returning function boxes
    #[test]
    fn j05_boxed_return_eligible() {
        let src = format!("{NAMED} fn f() -> Named {{ return new Item(\"a\"); }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // j06. a widening store into an interface-typed field boxes
    #[test]
    fn j06_boxed_field_assign_eligible() {
        let src = format!("{NAMED} fn f(h: Holder) -> i64 {{ h.n = new Item(\"a\"); return 1; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // j07. an array LITERAL takes its type from its elements, so
    // `let xs: Array<Named> = [new Item("a")]` hands the walker an
    // `Array<Item>` value for an `Array<Named>` slot. Boxing is per element and
    // there is no per-handle conversion, so this must fall back — it is the
    // reason [`assignable_repr`] compares array ELEMENT types rather than
    // calling any two handles interchangeable.
    #[test]
    fn j07_widening_array_literal_rejected() {
        let src = format!(
            "{NAMED} fn f() -> i64 {{ let xs: Array<Named> = [new Item(\"a\")]; return xs.len(); }}"
        );
        assert!(!eligible_lenient(&src, "f", &["f"]));

        // An array literal whose elements ALREADY match the slot is fine.
        let exact = format!(
            "{NAMED} fn f(n: Named) -> i64 {{ let xs: Array<Named> = [n]; return xs.len(); }}"
        );
        assert!(eligible_lenient(&exact, "f", &["f"]));
    }

    // j08. `push` onto an `Array<Iface>` boxes its argument. The array comes in
    // as a parameter because an empty literal never reaches the walker at all
    // (see e32), which would mask the property under test.
    #[test]
    fn j08_boxed_array_push_eligible() {
        let src = format!(
            "{NAMED} fn f(xs: Array<Named>) -> i64 {{ \
             xs.push(new Item(\"a\")); return xs.len(); }}"
        );
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // j09. the implicit memberwise constructor boxes into an interface field
    #[test]
    fn j09_boxed_memberwise_new_eligible() {
        let src =
            format!("{NAMED} fn f() -> i64 {{ let h = new Holder(new Item(\"a\")); return 1; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // j10. an explicit `init` taking an interface parameter boxes at the call
    #[test]
    fn j10_boxed_explicit_init_eligible() {
        let src = format!(
            "{NAMED} class Wrap {{ pub n: Named; \
             pub init(self, n: Named) {{ self.n = n; }} }} \
             fn f() -> i64 {{ let w = new Wrap(new Item(\"a\")); return 1; }}"
        );
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // j11. a static method taking an interface parameter boxes at the call
    #[test]
    fn j11_boxed_static_call_argument_eligible() {
        let src = format!(
            "{NAMED} class Util {{ pub static fn count(n: Named) -> i64 {{ return 1; }} }} \
             fn f() -> i64 {{ return Util::count(new Item(\"a\")); }}"
        );
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // j12. READING an interface-typed field is a plain load, no coercion
    #[test]
    fn j12_interface_field_read_eligible() {
        let src = format!("{NAMED} fn f(h: Holder) -> Named {{ return h.n; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // j13. index-assignment into an `Array<Iface>` boxes the element
    #[test]
    fn j13_boxed_index_assign_eligible() {
        let src = format!(
            "{NAMED} fn f(xs: Array<Named>) -> i64 {{ xs[0] = new Item(\"a\"); return xs.len(); }}"
        );
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // j14. an interface value moved into the SAME interface is already boxed:
    // `storable` must accept it without asking for a second box.
    #[test]
    fn j14_same_interface_store_needs_no_box() {
        let src = format!("{NAMED} fn f(n: Named) -> i64 {{ let x: Named = n; return 1; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // j15. THE safety property: a class with no registered vtable for the
    // target interface must not be admitted. `coerce_to_target` answers a
    // missing vtable by returning the object UNBOXED, which would put a raw
    // class pointer in an interface slot and crash the first dispatch on it.
    // Source cannot express this (the checker demands `implements`), so drive
    // the predicate directly with the vtable table emptied.
    #[test]
    fn j15_boxing_without_a_vtable_is_rejected() {
        let src = format!("{NAMED} fn f() -> Named {{ return new Item(\"a\"); }}");
        let tokens = Lexer::new(&src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        assert!(diags.is_empty(), "{diags:?}");
        let p = crate::ir::lowered::lower_program(&hir);
        let f = p.functions.iter().find(|f| f.name == "f").unwrap();

        let mut tables = TestTables::build(&program, &["f"], &[]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(f, ctx)));
        tables.vtables.clear();
        assert!(!tables.with_ctx(|ctx| lir_supported_function(f, ctx)));
    }

    // j16. a class that takes part in inheritance is not SIMPLE, so it cannot
    // be the source of a walker-emitted box either
    #[test]
    fn j16_boxing_an_inheriting_class_is_rejected() {
        let src = "interface Named { fn name(self) -> String; } \
                   pub open class Animal { pub age: i64; } \
                   pub class Dog extends Animal implements Named { \
                   pub fn name(self) -> String { return \"dog\"; } } \
                   fn f() -> Named { return new Dog(1); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // j17. interface → a DIFFERENT interface is not a box the walker builds:
    // `coerce_to_target` only boxes a value whose type is a CLASS.
    #[test]
    fn j17_interface_to_other_interface_rejected() {
        let src = "interface A { fn a(self) -> i64; } \
                   interface B extends A { fn b(self) -> i64; } \
                   fn f(x: B) -> A { return x; }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // j18. a generic interface instantiation is a `Type::Generic`, outside the
    // supported set: the walker does not model its vtable selection
    #[test]
    fn j18_generic_interface_target_rejected() {
        let src = "interface Boxed<T> { fn get(self) -> T; } \
                   class SBox implements Boxed<String> { pub v: String; \
                   pub fn get(self) -> String { return self.v; } } \
                   fn f() -> Boxed<String> { return new SBox(\"a\"); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // j19. an optional interface is rejected: Option is outside the walker's
    // supported representation set.
    #[test]
    fn j19_optional_interface_rejected() {
        let src = format!(
            "{NAMED} enum Option<T> {{ Some(T), None, }} \
             fn f() -> i64 {{ let x: Option<Named> = Option::None; return 1; }}"
        );
        assert!(!eligible_lenient(&src, "f", &["f"]));
    }

    // j20. both ternary arms define ONE Cranelift variable and the walker
    // inserts no conversion between them, so a ternary that widens to the
    // interface must fall back rather than store two raw class pointers.
    #[test]
    fn j20_widening_ternary_rejected() {
        let src = format!(
            "{NAMED} class Other implements Named {{ pub m: String; \
             pub fn name(self) -> String {{ return self.m; }} }} \
             fn f(c: bool) -> Named {{ return c ? new Item(\"a\") : new Other(\"b\"); }}"
        );
        assert!(!eligible_lenient(&src, "f", &["f"]));
    }

    // j21. `toString()` on an `Array<Iface>` has no runtime element kind
    #[test]
    fn j21_interface_array_to_string_rejected() {
        let src = format!("{NAMED} fn f(xs: Array<Named>) -> String {{ return xs.toString(); }}");
        assert!(!eligible_lenient(&src, "f", &["f"]));
    }

    // ---------------------------------------------------------------------
    // willow-0g8j.6 — interface DISPATCH eligibility (k01..k27).
    //
    // The receiver is a box, so the walker never needs the concrete class; what
    // it does need is the vtable SLOT, and the only thing standing between a
    // wrong slot and silent miscompilation is that eligibility resolves the
    // method exactly the way the emitter will. Perspectives below therefore
    // split into: shapes the walker must CLAIM (k01..k13, k18, k23), shapes it
    // must REFUSE (k14..k17, k19), drift between the interface tables and the
    // call site, which source cannot express and which is driven through the
    // predicate directly (k20..k22), and parameter MODES, where the declared
    // `&`/`&mut` is part of the ABI the walker cannot emit (k24..k27,
    // willow-0g8j.9).
    // ---------------------------------------------------------------------

    /// A three-method interface: `Named` has a single method, so it cannot tell
    /// slot 0 from "the only slot there is". `describe` sits at slot 1 and
    /// takes arguments; `tally` at slot 2 returns void.
    const MULTI: &str = "interface Shape { \
                         fn area(self) -> i64; \
                         fn describe(self, prefix: String, n: i64) -> String; \
                         fn stamp(self); } \
                         class Sq implements Shape { pub side: i64; \
                         pub fn area(self) -> i64 { return self.side * self.side; } \
                         pub fn describe(self, prefix: String, n: i64) -> String { \
                         return prefix + n.toString(); } \
                         pub fn stamp(self) { println(self.side); } } ";

    /// Two interfaces, because an interface cannot name ITSELF as a return type
    /// (E0350): `Chain::next` hands back a `Leaf` box, which is then the
    /// receiver of a second dispatch.
    const CHAIN: &str = "interface Leaf { fn v(self) -> i64; } \
                         interface Chain { fn next(self) -> Leaf; } \
                         class L implements Leaf { pub k: i64; \
                         pub fn v(self) -> i64 { return self.k; } } \
                         class C implements Chain { pub k: i64; \
                         pub fn next(self) -> Leaf { return new L(self.k); } } ";

    // k01. the base case: dispatch on an interface-typed PARAMETER. Contrast
    // j03, which only passed the same value through without calling on it.
    #[test]
    fn k01_dispatch_on_interface_param_eligible() {
        let src = format!("{NAMED} fn f(n: Named) -> String {{ return n.name(); }}");
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // k02. dispatch on an interface LOCAL that the walker itself boxed: the box
    // it writes must be the box it then reads `[object | vtable]` out of.
    #[test]
    fn k02_dispatch_on_boxed_local_eligible() {
        let src = format!(
            "{NAMED} fn f() -> String {{ let x: Named = new Item(\"a\"); return x.name(); }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // k03. the receiver is an interface-typed FIELD read, not a variable
    #[test]
    fn k03_dispatch_on_field_receiver_eligible() {
        let src = format!("{NAMED} fn f(h: Holder) -> String {{ return h.n.name(); }}");
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // k04. the receiver is a temporary — the result of a call. Nothing else
    // holds it, so the emitter's object root is the only thing keeping it alive
    // across argument evaluation.
    #[test]
    fn k04_dispatch_on_call_result_receiver_eligible() {
        let src = format!(
            "{NAMED} fn make() -> Named {{ return new Item(\"a\"); }} \
             fn f() -> String {{ return make().name(); }}"
        );
        assert!(eligible_checked(&src, "f", &["f", "make"]));
    }

    // k05. the receiver is an element of an `Array<Iface>`. `Array` is a
    // collection type, so the checker demands the import even though these
    // tests never resolve a module.
    #[test]
    fn k05_dispatch_on_array_element_receiver_eligible() {
        let src = format!(
            "import std::collections::Array; {NAMED} \
             fn f(xs: Array<Named>) -> String {{ return xs[0].name(); }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // k06. a method at a NON-ZERO vtable slot, with arguments. `describe` is
    // slot 1 of three; picking slot 0 would call `area` with the wrong
    // signature, which is the failure this whole subset guards against.
    #[test]
    fn k06_dispatch_on_later_slot_with_args_eligible() {
        let src = format!("{MULTI} fn f(s: Shape) -> String {{ return s.describe(\"n=\", 3); }}");
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // k07. a VOID-returning method in statement position: the call produces no
    // Cranelift result and the walker must not read one.
    #[test]
    fn k07_dispatch_void_method_statement_eligible() {
        let src = format!("{MULTI} fn f(s: Shape) -> i64 {{ s.stamp(); return 1; }}");
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // k08. the argument to an interface method is itself widened to an
    // interface — boxing (willow-j260) and dispatch composed at one call
    #[test]
    fn k08_dispatch_with_boxed_argument_eligible() {
        let src = format!(
            "{NAMED} interface Visitor {{ fn visit(self, n: Named) -> i64; }} \
             class V implements Visitor {{ pub k: i64; \
             pub fn visit(self, n: Named) -> i64 {{ return self.k; }} }} \
             fn f(v: Visitor) -> i64 {{ return v.visit(new Item(\"a\")); }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // k09. an interface method returning ANOTHER interface value, then
    // dispatching on that result: the returned box is used as a receiver
    // without ever being stored.
    #[test]
    fn k09_chained_interface_dispatch_eligible() {
        let src = format!("{CHAIN} fn f(c: Chain) -> i64 {{ return c.next().v(); }}");
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // k10. a `Self`-returning method: the callee hands back a BARE object of
    // the receiver's class, which the emitter re-boxes with the receiver's own
    // vtable, so the result is the RECEIVER'S interface and nothing else.
    //
    // Source cannot reach this arm today: the checker resolves `Self` only on a
    // GENERIC interface (E0350 otherwise), and a generic receiver is a
    // `Type::Generic` the subset already refuses (k14). The arm is therefore a
    // guard for the day generic interfaces are admitted — and an interface
    // cannot name itself as a return type either, so the call site is built
    // directly rather than parsed.
    #[test]
    fn k10_self_returning_method_matches_receiver_interface() {
        let src = format!("{MULTI} fn f(s: Shape) -> i64 {{ return s.area(); }}");
        let (_, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
        tables.iface_methods.get_mut("Shape").unwrap().push((
            "itself".to_string(),
            Vec::new(),
            Vec::new(),
            Type::Named("Self".to_string()),
        ));
        let shape = Type::Named("Shape".to_string());
        let call = |ty: Type| HirExpr {
            kind: HirExprKind::MethodCall {
                object: Box::new(HirExpr {
                    kind: HirExprKind::Var("s".to_string()),
                    ty: shape.clone(),
                    span: crate::diagnostics::Span::dummy(),
                }),
                method: "itself".to_string(),
                args: Vec::new(),
            },
            ty,
            span: crate::diagnostics::Span::dummy(),
        };
        let names: HashMap<&str, &Type> = HashMap::from([("s", &shape)]);
        let as_receiver_iface = call(shape.clone());
        let as_other_type = call(Type::String);
        tables.with_ctx(|ctx| {
            assert!(supported_expr(&as_receiver_iface, ctx, &names));
            // Anything but the receiver's own interface: the re-box produces a
            // `Shape` box, so a `String` consumer would get a pointer.
            assert!(!supported_expr(&as_other_type, ctx, &names));
        });
    }

    // k11. the dispatch result feeds a further store that BOXES: interface in,
    // interface out, one more box on the way into the slot
    #[test]
    fn k11_dispatch_result_stored_in_interface_local_eligible() {
        let src =
            format!("{CHAIN} fn f(c: Chain) -> i64 {{ let n: Leaf = c.next(); return n.v(); }}");
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // k12. dispatch inside a loop: the receiver root and the call frame must
    // balance per iteration, not accumulate (the reason LIR roots are entry
    // slots rather than per-`let` pushes).
    #[test]
    fn k12_dispatch_in_loop_eligible() {
        let src = format!(
            "{MULTI} fn f(s: Shape) -> i64 {{ let mut i = 0; let mut t = 0; \
             while i < 3 {{ t = t + s.area(); i = i + 1; }} return t; }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // k13. dispatch as an ARGUMENT to another call, and in a condition — the
    // walker must handle it anywhere an expression is legal, not only in
    // `return`/`let` position.
    #[test]
    fn k13_dispatch_nested_in_call_and_condition_eligible() {
        let src = format!(
            "{MULTI} fn g(v: i64) -> i64 {{ return v; }} \
             fn f(s: Shape) -> i64 {{ if s.area() > 0 {{ return g(s.area()); }} return 0; }}"
        );
        assert!(eligible_checked(&src, "f", &["f", "g"]));
    }

    // k14. a GENERIC interface instantiation dispatches through a vtable keyed
    // by the bare interface name in the AST emitter; the walker's receiver type
    // is a `Type::Generic`, which `supported_type` rejects outright. Staying
    // rejected is what keeps the two from disagreeing (cf. j18).
    #[test]
    fn k14_generic_interface_receiver_rejected() {
        let src = "interface Boxed<T> { fn get(self) -> T; } \
                   class SBox implements Boxed<String> { pub v: String; \
                   pub fn get(self) -> String { return self.v; } } \
                   fn f(b: Boxed<String>) -> String { return b.get(); }";
        assert!(!eligible_checked(src, "f", &["f"]));
    }

    // k15. an OPTIONAL interface receiver: the payload is unwrapped by the
    // match and dispatched through the same vtable slot a bare interface
    // parameter would use. Both arms `return`, so the match itself is typed
    // `!` — admitted since willow-0g8j.2.5, which is what brought this shape
    // (and `example/option_interface_context.wi`) onto the walker.
    #[test]
    fn k15_optional_interface_receiver_eligible() {
        let src = format!(
            "{NAMED} enum Option<T> {{ Some(T), None, }} \
             fn f(n: Option<Named>) -> String {{ match n {{ \
             Some(value) => return value.name(), None => return \"x\", }} }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // k16. a CLASS receiver is unaffected by this arm: it still resolves the
    // concrete `Class__method` symbol, so the interface arm must be checked
    // first without swallowing the class case.
    #[test]
    fn k16_class_receiver_still_takes_the_class_path() {
        let src = format!("{NAMED} fn f(i: Item) -> String {{ return i.name(); }}");
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // k17. an interface whose method is not registered in the tables at all
    // (the whole interface unknown to `iface_method`) must be refused rather
    // than fall through to the class arm and mangle a `Named__name` symbol.
    #[test]
    fn k17_unregistered_interface_method_rejected() {
        let src = format!("{NAMED} fn f(n: Named) -> String {{ return n.name(); }}");
        let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
        tables.iface_methods.clear();
        assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    }

    // k18. a DEFAULT interface method (willow-1js.3) occupies a vtable slot
    // like any other: the implementing class inherits the body, so dispatch
    // through the box must find it without the class declaring anything.
    #[test]
    fn k18_default_interface_method_eligible() {
        let src = "interface Greeter { fn name(self) -> String; \
                   fn greet(self) -> String { return \"hi \" + self.name(); } } \
                   class Dog implements Greeter { pub k: String; \
                   pub fn name(self) -> String { return self.k; } } \
                   fn f(g: Greeter) -> String { return g.greet(); }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // k19. an interface method taking an argument the walker cannot store —
    // here a class argument with no vtable for the interface parameter — is
    // refused by the same `storable` gate that guards every other store site.
    #[test]
    fn k19_unboxable_argument_rejected() {
        let src = format!(
            "{NAMED} interface Visitor {{ fn visit(self, n: Named) -> i64; }} \
             class V implements Visitor {{ pub k: i64; \
             pub fn visit(self, n: Named) -> i64 {{ return self.k; }} }} \
             fn f(v: Visitor) -> i64 {{ return v.visit(new Item(\"a\")); }}"
        );
        let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
        tables.vtables.clear();
        assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    }

    // k20. TABLE DRIFT, arity: if the interface's recorded signature and the
    // call site disagree on argument count, the indirect call would be built
    // from the wrong signature. Source cannot express this (the checker rejects
    // it first), so drive the predicate with a doctored table.
    #[test]
    fn k20_arity_mismatch_rejected() {
        let src = format!("{MULTI} fn f(s: Shape) -> i64 {{ return s.area(); }}");
        let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
        let methods = tables.iface_methods.get_mut("Shape").unwrap();
        methods[0].1.push(Type::I64);
        assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    }

    // k21. TABLE DRIFT, return type: the call site's type and the declared
    // return must be representation-compatible, or the walker would hand a
    // caller a value of the wrong Cranelift type.
    #[test]
    fn k21_return_type_mismatch_rejected() {
        let src = format!("{MULTI} fn f(s: Shape) -> i64 {{ return s.area(); }}");
        let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
        let methods = tables.iface_methods.get_mut("Shape").unwrap();
        methods[0].3 = Type::String;
        assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    }

    // k22. TABLE DRIFT, unsupported parameter type: a method whose signature
    // mentions a type outside the subset must be refused even though the call
    // site itself looks fine.
    #[test]
    fn k22_unsupported_parameter_type_rejected() {
        // Arity is left alone so the parameter TYPE is the only thing that can
        // decide the outcome.
        let src =
            format!("{MULTI} fn f(s: Shape, k: i64) -> String {{ return s.describe(\"n=\", k); }}");
        let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
        let methods = tables.iface_methods.get_mut("Shape").unwrap();
        methods[1].1[1] = Type::Generic("Map".to_string(), vec![Type::String, Type::I64]);
        assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    }

    // k23. an interface that INHERITS a method through `extends` dispatches on
    // the COMPOSED slot list: desugaring folds the base interface's methods
    // into the child's declaration, and the vtable is laid out from that. A
    // table missing the inherited method must refuse the call rather than fall
    // through to some other slot.
    #[test]
    fn k23_inherited_interface_method_dispatches_on_composed_slots() {
        let src = "interface Base { fn base(self) -> i64; } \
                   interface Ext extends Base { fn ext(self) -> i64; } \
                   class Impl implements Ext { pub k: i64; \
                   pub fn base(self) -> i64 { return self.k; } \
                   pub fn ext(self) -> i64 { return self.k + 1; } } \
                   fn f(e: Ext) -> i64 { return e.base() + e.ext(); }";
        let (f, mut tables) = lir_fn_and_tables(src, "f", &["f"]);
        assert!(
            tables.iface_methods["Ext"]
                .iter()
                .any(|(n, _, _, _)| n == "base"),
            "desugaring must compose the inherited method into `Ext`"
        );
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));

        tables
            .iface_methods
            .get_mut("Ext")
            .unwrap()
            .retain(|(n, _, _, _)| n != "base");
        assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    }

    /// An interface whose slots differ ONLY in how their parameter is passed.
    /// Every method takes one `i64`, so a walker that looks at types alone
    /// cannot tell them apart — and the concrete methods behind two of these
    /// slots receive a POINTER (willow-0g8j.9).
    const MODES: &str = "interface Mode { \
                         fn by_value(self, v: i64) -> i64; \
                         fn by_mut(self, v: &mut i64); \
                         fn by_ref(self, v: & i64) -> i64; } \
                         class Impl implements Mode { pub k: i64; \
                         pub fn by_value(self, v: i64) -> i64 { return v + self.k; } \
                         pub fn by_mut(self, v: &mut i64) { v = v + self.k; } \
                         pub fn by_ref(self, v: & i64) -> i64 { return v + self.k; } } ";

    /// Lower `src` the checked way and report the diagnostics instead of
    /// asserting there are none: a reference ARGUMENT stops at HIR lowering,
    /// which is a fact about the subset worth pinning rather than working
    /// around.
    fn checked_lowering_diags(src: &str) -> Vec<crate::diagnostics::Diagnostic> {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (mut program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        crate::desugar::DesugarPass::run(&mut program, &mut []);
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).expect("prelude");
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|d| d.severity == crate::diagnostics::Severity::Error)
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
        let tables = crate::ir::lower::CheckerTables::from_checker(&checker);
        crate::ir::lower::lower_program_with(&program, &tables).1
    }

    // k24. FIRST line of defence: a `&`/`&mut` argument does not survive HIR
    // lowering at all, so a function that writes one never reaches the walker.
    // This is what keeps the modes below unreachable from source — and the
    // reason the eligibility rule under it can only be exercised through the
    // tables (k25, k26).
    #[test]
    fn k24_reference_argument_does_not_reach_the_lir() {
        let src =
            format!("{MODES} fn f(m: Mode) -> i64 {{ let mut x = 1; m.by_mut(&x); return x; }}");
        let diags = checked_lowering_diags(&src);
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("reference call argument")),
            "a reference argument must stop at lowering, got {diags:?}"
        );
    }

    // k25. TABLE DRIFT, `&mut` parameter: flip a by-value slot's MODE
    // underneath an already-eligible call, leaving the types untouched. The
    // callee would receive a pointer to the caller's place while the walker
    // passes a value, so the call must stop being claimed (willow-0g8j.9).
    #[test]
    fn k25_mut_reference_parameter_mode_rejected() {
        let src = format!("{MODES} fn f(m: Mode) -> i64 {{ return m.by_value(2); }}");
        let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));

        set_by_value_mode(
            &mut tables,
            ParamMode::Reference {
                mutable: true,
                ampersand_span: crate::diagnostics::Span::dummy(),
                mut_span: Some(crate::diagnostics::Span::dummy()),
            },
        );
        assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    }

    // k26. the same for a SHARED `&` parameter: the ABI is a pointer whether or
    // not the callee may write through it.
    #[test]
    fn k26_shared_reference_parameter_mode_rejected() {
        let src = format!("{MODES} fn f(m: Mode) -> i64 {{ return m.by_value(2); }}");
        let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
        set_by_value_mode(
            &mut tables,
            ParamMode::Reference {
                mutable: false,
                ampersand_span: crate::diagnostics::Span::dummy(),
                mut_span: None,
            },
        );
        assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    }

    // k27. the control: restoring the by-value mode — same interface, same
    // parameter TYPE, same call site — is claimed again, so k25/k26 cannot
    // pass for some unrelated reason.
    #[test]
    fn k27_value_parameter_mode_still_eligible() {
        let src = format!("{MODES} fn f(m: Mode) -> i64 {{ return m.by_value(2); }}");
        let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
        set_by_value_mode(&mut tables, ParamMode::Value);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    }

    // ── Exponentiation eligibility (willow-n5yv.2) ───────────────────────────
    //
    // Both same-typed scalar forms are emitted by the walker. Mixed forms stay
    // structurally ineligible as a final verifier-safety boundary even though
    // the type checker rejects them first.

    // p1. an i64 power is claimed by the LIR walker.
    #[test]
    fn p1_scalar_i64_pow_is_lir_eligible() {
        assert!(eligible(
            "fn f(a: i64, b: i64) -> i64 { return a ** b; }",
            "f",
            &["f"]
        ));
    }

    // p2. an f64 power is claimed by the generated native kernel.
    #[test]
    fn p2_scalar_f64_pow_is_lir_eligible() {
        assert!(eligible(
            "fn f(a: f64, b: f64) -> f64 { return a ** b; }",
            "f",
            &["f"]
        ));
    }

    // p2b. a mixed power stays out too, from either side, and the rest of the
    // function is what drops with it — no partial claim.
    #[test]
    fn p2b_mixed_float_pow_is_not_lir_eligible() {
        assert!(!eligible(
            "fn f(a: f64, b: i64) -> f64 { return a ** b; }",
            "f",
            &["f"]
        ));
        assert!(!eligible(
            "fn f(a: i64, b: f64) -> f64 { return a ** b; }",
            "f",
            &["f"]
        ));
    }

    // p2c. f64 power and ordinary float arithmetic can coexist on the walker.
    #[test]
    fn p2c_float_pow_and_float_arithmetic_are_lir_eligible() {
        assert!(eligible(
            "fn f(a: f64, b: f64, c: i64) -> i64 { let x = a ** b; return c + 1; }",
            "f",
            &["f"]
        ));
        assert!(eligible(
            "fn f(a: f64, b: f64, c: i64) -> i64 { let x = a * b; return c ** 2; }",
            "f",
            &["f"]
        ));
    }

    // p3. a right-associative chain is claimed as a whole.
    #[test]
    fn p3_pow_chain_is_lir_eligible() {
        assert!(eligible(
            "fn f(a: i64, b: i64, c: i64) -> i64 { return a ** b ** c; }",
            "f",
            &["f"]
        ));
    }

    // p4. a power mixed with the other arithmetic operators is claimed.
    #[test]
    fn p4_pow_mixed_with_arithmetic_is_lir_eligible() {
        assert!(eligible(
            "fn f(a: i64, b: i64) -> i64 { return a * b ** 2 + 1; }",
            "f",
            &["f"]
        ));
    }

    // p5. a power on String operands is NOT claimed — the walker emits only
    // `+`, `==` and `!=` for strings, so `**` there must fall back rather than
    // reach `emit_lir_binop` (which has no string path at all).
    #[test]
    fn p5_string_pow_is_not_lir_eligible() {
        assert!(!eligible(
            "fn f(a: String, b: String) -> String { return a ** b; }",
            "f",
            &["f"]
        ));
    }

    // ---------------------------------------------------------------------
    // willow-0g8j.7 — `Map<K, V>`, `FrozenArray<T>` and `FrozenMap<K, V>` in
    // the LIR walker, plus the array methods that were still missing.
    //
    // Two restrictions are deliberate and are what most of these perspectives
    // pin down:
    //
    //   * a map key is admitted only as `String` or `i64`, because the runtime
    //     `MapKey` is `Int(i64) | Str(String)` and picks between them from the
    //     is-ref flag the call site passes. Any other key type is representable
    //     on the AST path but not through this ABI, so it falls back.
    //   * `get` is absent from both map kinds: it yields `Option<V>`, which the
    //     walker has no representation for.
    //
    // Perspectives c01-c24 below are the *eligibility* half. The emitted-code
    // half — LIR-on/off differentials, including under `WILLOW_GC_STRESS=alloc`
    // — lives in `tests/integration/codegen.rs`, because rooting discipline is
    // not something the predicate can observe.
    //
    // c01. a `Map<String, i64>` parameter with `len()` is eligible
    // c02. an `i64` key and a GC-managed value is eligible
    // c03. `Map::new()` + `insert` is eligible despite its `Map<Void, Void>`
    // c04. `contains` is eligible
    // c05. `toString` on a renderable value type is eligible
    // c06. `toString` on a NON-renderable value type falls back
    // c07. `freeze` on a map is eligible
    // c08. `FrozenMap` answers `len` and `contains`
    // c09. `FrozenArray` answers `len`
    // c10. indexing a `FrozenArray` is eligible (it lowers like an array read)
    // c11. `freeze` on an array is eligible
    // c12. `Map::get` falls back — `Option<V>` has no representation
    // c13. `FrozenMap::get` falls back for the same reason
    // c14. a `bool` key falls back (the `MapKey` restriction)
    // c15. an `f64` key falls back too
    // c16. a value type outside the subset (a base class) falls back
    // c17. nested maps are eligible: the value type is checked recursively
    // c18. an `Array<T>` value type is eligible
    // c19. a `FrozenArray` of class elements is eligible
    // c20. `Map<Void, Void>` is NOT a declarable storage type — the exemption
    //      is scoped to the `Map::new()` node alone
    // c21. `is_fresh_empty_map` accepts only that node, not any `Map<Void,Void>`
    // c22. a map and a frozen map are not interchangeable representations
    // c23. an array and a frozen array are not interchangeable either
    // c24. a non-collection generic (`Option`, `Range`, `Task`, a user generic)
    //      is not mistaken for a collection

    /// Registration tables for a program that declares no items, for tests that
    /// ask the type predicate directly instead of through a function body.
    fn empty_tables() -> TestTables {
        let tokens = Lexer::new("fn f() {}").tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        TestTables::build(&program, &["f"], &[])
    }

    fn map_ty(key: Type, value: Type) -> Type {
        Type::Generic("Map".to_string(), vec![key, value])
    }

    fn frozen_map_ty(key: Type, value: Type) -> Type {
        Type::Generic("FrozenMap".to_string(), vec![key, value])
    }

    fn frozen_array_ty(elem: Type) -> Type {
        Type::Generic("FrozenArray".to_string(), vec![elem])
    }

    const MAP_IMPORT: &str = "import std::collections::Map;";

    // c01. the base case: a map parameter, read for its length
    #[test]
    fn c01_map_parameter_len_eligible() {
        let src = format!("{MAP_IMPORT} fn f(m: Map<String, i64>) -> i64 {{ return m.len(); }}");
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // c02. the other admitted key type, with a GC-managed value: the value goes
    // through the same store discipline as any other reference.
    #[test]
    fn c02_int_key_and_string_value_eligible() {
        let src = format!("{MAP_IMPORT} fn f(m: Map<i64, String>) -> i64 {{ return m.len(); }}");
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // c03. `Map::new()` types as `Map<Void, Void>` — the empty map really is
    // untyped, since the runtime records nothing until the first insert. The
    // walker exempts that one node so a `let` can still be claimed.
    #[test]
    fn c03_fresh_empty_map_eligible() {
        let src = format!(
            "{MAP_IMPORT} fn f() -> i64 {{ let m: Map<String, i64> = Map::new(); \
             m.insert(\"a\", 1); return m.len(); }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // c04. `contains` answers a `bool` from the same key ABI as `insert`
    #[test]
    fn c04_map_contains_eligible() {
        let src = format!(
            "{MAP_IMPORT} fn f(m: Map<String, i64>) -> bool {{ return m.contains(\"a\"); }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // c05. `toString` renders in the runtime, which knows the four scalar and
    // string value kinds
    #[test]
    fn c05_map_to_string_eligible() {
        let src =
            format!("{MAP_IMPORT} fn f(m: Map<String, i64>) -> String {{ return m.toString(); }}");
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // c06. a value kind the runtime cannot render must not be passed as kind
    // `0`, which would print a pointer as an `i64`. The checker owns the first
    // line of defence here (E1402 rejects the source outright), so the walker's
    // own guard is asked directly — it is what keeps the emitter's
    // `collection_elem_kind(..).expect(..)` honest if that check ever moves.
    #[test]
    fn c06_map_to_string_unrenderable_value_ineligible() {
        let tables = empty_tables();
        let span = crate::diagnostics::Span::dummy();
        let to_string = |value: Type| HirExpr {
            kind: HirExprKind::MethodCall {
                object: Box::new(HirExpr {
                    kind: HirExprKind::Var("m".to_string()),
                    ty: map_ty(Type::String, value),
                    span,
                }),
                method: "toString".to_string(),
                args: Vec::new(),
            },
            ty: Type::String,
            span,
        };
        let int_map = map_ty(Type::String, Type::I64);
        let array_map = map_ty(Type::String, Type::Array(Box::new(Type::I64)));
        let renderable = to_string(Type::I64);
        let unrenderable = to_string(Type::Array(Box::new(Type::I64)));
        tables.with_ctx(|ctx| {
            let names: HashMap<&str, &Type> = HashMap::from([("m", &int_map)]);
            assert!(supported_expr(&renderable, ctx, &names));
            let names: HashMap<&str, &Type> = HashMap::from([("m", &array_map)]);
            assert!(!supported_expr(&unrenderable, ctx, &names));
        });
    }

    // c07. `freeze` copies into a `FrozenMap<K, V>` over the SAME pair
    #[test]
    fn c07_map_freeze_eligible() {
        let src = format!(
            "{MAP_IMPORT} fn f(m: Map<String, i64>) -> i64 {{ let g = m.freeze(); \
             return g.len(); }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // c08. a frozen map is the same runtime object, so its reads lower to the
    // same calls as the mutable one's
    #[test]
    fn c08_frozen_map_reads_eligible() {
        let src = "fn f(m: FrozenMap<String, i64>) -> i64 { if m.contains(\"a\") { return m.len(); } \
                   return 0; }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // c09. `len` is the whole declared surface of a frozen array
    #[test]
    fn c09_frozen_array_len_eligible() {
        let src = "fn f(xs: FrozenArray<i64>) -> i64 { return xs.len(); }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // c10. indexing a frozen array is an `Index`, not a method call — the HIR
    // lowering had to learn the element type of a non-`Array` handle for this
    // to reach the walker at all.
    #[test]
    fn c10_frozen_array_index_eligible() {
        let src = "fn f(xs: FrozenArray<i64>) -> i64 { return xs[0]; }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // c11. the array side of the same pair
    #[test]
    fn c11_array_freeze_eligible() {
        let src = "fn f(xs: Array<i64>) -> i64 { let ys = xs.freeze(); return ys.len(); }";
        assert!(eligible_checked(
            &format!("import std::collections::Array; {src}"),
            "f",
            &["f"]
        ));
    }

    // c12. `get` yields `Option<V>`, which the walker gained a representation
    // for in willow-0g8j.2.1 — so the call is now claimed rather than costing
    // the function its LIR compilation.
    #[test]
    fn c12_map_get_eligible() {
        let src = format!(
            "{MAP_IMPORT} fn f(m: Map<String, i64>) -> i64 {{ \
             return m.get(\"a\").unwrap_or(0); }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));
        // the value type still has to be one the walker can represent: an
        // `Option` over a class in an inheritance hierarchy is not.
        let unsupported = format!(
            "{MAP_IMPORT} open class Base {{ pub v: i64; }} class Sub extends Base {{}} \
             fn f(m: Map<String, Base>) -> i64 {{ \
             let b = m.get(\"a\"); return m.len(); }}"
        );
        assert!(!eligible_checked(&unsupported, "f", &["f"]));
    }

    // c13. the frozen kind is claimed on the same terms
    #[test]
    fn c13_frozen_map_get_eligible() {
        let src = "fn f(m: FrozenMap<String, i64>) -> i64 { return m.get(\"a\").unwrap_or(0); }";
        assert!(eligible_checked(src, "f", &["f"]));
        let control = "fn f(m: FrozenMap<String, i64>) -> i64 { return m.len(); }";
        assert!(eligible_checked(control, "f", &["f"]));
    }

    // c14. a `bool` key type compiles on the AST path but has no `MapKey`
    // spelling the walker can pass, so it falls back.
    #[test]
    fn c14_bool_key_ineligible() {
        let src = format!("{MAP_IMPORT} fn f(m: Map<bool, i64>) -> i64 {{ return m.len(); }}");
        assert!(!eligible_checked(&src, "f", &["f"]));
    }

    // c15. and neither does an `f64` key: the flag would say "not a reference"
    // and the runtime would read the float's bit pattern as an integer key.
    #[test]
    fn c15_float_key_ineligible() {
        let src = format!("{MAP_IMPORT} fn f(m: Map<f64, i64>) -> i64 {{ return m.len(); }}");
        assert!(!eligible_checked(&src, "f", &["f"]));
    }

    // c16. the VALUE type is checked with the same rules as any other stored
    // type — a base class is outside the subset, so a map over one is too.
    #[test]
    fn c16_unsupported_value_type_ineligible() {
        let src = format!(
            "{MAP_IMPORT} open class Base {{ pub k: i64; }} class Sub extends Base {{ pub j: i64; }} \
             fn f(m: Map<String, Base>) -> i64 {{ return m.len(); }}"
        );
        assert!(!eligible_checked(&src, "f", &["f"]));
    }

    // c17. the value check recurses: a map of maps is admitted because the
    // inner map is itself an admitted storage type.
    #[test]
    fn c17_nested_map_eligible() {
        let src = format!(
            "{MAP_IMPORT} fn f(m: Map<String, Map<String, i64>>) -> i64 {{ return m.len(); }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // c18. an `Array<T>` value type is a plain GC handle in the slot
    #[test]
    fn c18_array_value_type_eligible() {
        let src = format!(
            "{MAP_IMPORT} import std::collections::Array; \
             fn f(m: Map<String, Array<i64>>) -> i64 {{ return m.len(); }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // c19. a frozen array of SIMPLE class elements, like the mutable one
    #[test]
    fn c19_frozen_array_of_class_elements_eligible() {
        let src = "class Item { pub name: String; } \
                   fn f(xs: FrozenArray<Item>) -> i64 { return xs.len(); }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // c20. the `Map<Void, Void>` exemption is scoped to one expression node: as
    // a declared storage type it is still rejected, which is what makes the
    // exemption sound — no parameter, local or return can ever have this type.
    #[test]
    fn c20_void_map_is_not_a_storage_type() {
        let tables = empty_tables();
        tables.with_ctx(|ctx| {
            assert!(!ctx.supported_type(&map_ty(Type::Void, Type::Void)));
            assert!(!ctx.supported_type(&map_ty(Type::String, Type::Void)));
            assert!(!ctx.supported_type(&frozen_array_ty(Type::Void)));
            // the control: the same shapes with real arguments are fine
            assert!(ctx.supported_type(&map_ty(Type::String, Type::I64)));
            assert!(ctx.supported_type(&frozen_map_ty(Type::I64, Type::String)));
            assert!(ctx.supported_type(&frozen_array_ty(Type::I64)));
        });
    }

    // c21. the exemption keys on the `Map::new()` CALL, not on the type: a
    // variable that somehow carried `Map<Void, Void>` is not exempt.
    #[test]
    fn c21_empty_map_exemption_is_node_scoped() {
        let void_map = map_ty(Type::Void, Type::Void);
        let span = crate::diagnostics::Span::dummy();
        let fresh = HirExpr {
            kind: HirExprKind::StaticCall {
                class: "Map".to_string(),
                method: "new".to_string(),
                args: Vec::new(),
            },
            ty: void_map.clone(),
            span,
        };
        assert!(is_fresh_empty_map(&fresh));

        // same type, different node
        let var = HirExpr {
            kind: HirExprKind::Var("m".to_string()),
            ty: void_map.clone(),
            span,
        };
        assert!(!is_fresh_empty_map(&var));

        // same node, a type that is not the empty map
        let typed = HirExpr {
            kind: HirExprKind::StaticCall {
                class: "Map".to_string(),
                method: "new".to_string(),
                args: Vec::new(),
            },
            ty: map_ty(Type::String, Type::I64),
            span,
        };
        assert!(!is_fresh_empty_map(&typed));

        // and a different static call with the empty-map type is not exempt
        let other = HirExpr {
            kind: HirExprKind::StaticCall {
                class: "Other".to_string(),
                method: "new".to_string(),
                args: Vec::new(),
            },
            ty: void_map,
            span,
        };
        assert!(!is_fresh_empty_map(&other));
    }

    // c22. a map and a frozen map share a runtime object but are DIFFERENT
    // types: a store from one into the other is not a representation match, or
    // `freeze` would be a no-op the checker never sanctioned.
    #[test]
    fn c22_map_and_frozen_map_are_distinct_representations() {
        let m = map_ty(Type::String, Type::I64);
        let f = frozen_map_ty(Type::String, Type::I64);
        assert!(!assignable_repr(&m, &f));
        assert!(!assignable_repr(&f, &m));
        assert!(assignable_repr(&m, &m));
        // the type arguments are part of the representation, too
        assert!(!assignable_repr(&m, &map_ty(Type::String, Type::String)));
        assert!(!assignable_repr(&m, &map_ty(Type::I64, Type::I64)));
    }

    // c23. the same for the array pair, in both directions
    #[test]
    fn c23_array_and_frozen_array_are_distinct_representations() {
        let a = Type::Array(Box::new(Type::I64));
        let f = frozen_array_ty(Type::I64);
        assert!(!assignable_repr(&a, &f));
        assert!(!assignable_repr(&f, &a));
        assert!(assignable_repr(&f, &f));
        assert!(!assignable_repr(&f, &frozen_array_ty(Type::String)));
    }

    // c24. `lir_collection` decides by BUILTIN IDENTITY, not by generic shape,
    // so no other generic — and no user generic — is read as a collection.
    #[test]
    fn c24_other_generics_are_not_collections() {
        // Not a collection AND not in the subset at all.
        for ty in [
            Type::Generic("Range".to_string(), vec![Type::I64]),
            Type::Generic("Task".to_string(), vec![Type::I64]),
            Type::Generic("Channel".to_string(), vec![Type::I64]),
            Type::Generic("Holder".to_string(), vec![Type::I64]),
        ] {
            assert!(lir_collection(&ty).is_none(), "{ty:?} is not a collection");
            let tables = empty_tables();
            tables.with_ctx(|ctx| assert!(!ctx.supported_type(&ty), "{ty:?} is not storable"));
        }
        // `Option`/`Result` are storable as of willow-0g8j.2.1, but they are
        // ENUMS, not collections: nothing may route them into the map/array
        // emission paths.
        for ty in [
            Type::Generic("Option".to_string(), vec![Type::I64]),
            Type::Generic("Result".to_string(), vec![Type::I64, Type::String]),
        ] {
            assert!(lir_collection(&ty).is_none(), "{ty:?} is not a collection");
            let tables = empty_tables();
            tables.with_ctx(|ctx| {
                assert!(ctx.supported_type(&ty), "{ty:?} is storable");
                assert!(ctx.supported_enum_type(&ty), "{ty:?} is an enum instance");
            });
        }
        // A collection name carrying the WRONG number of arguments resolves to
        // the builtin id — name resolution is by name — but has no admitted
        // shape, so nothing downstream can take it for a usable collection.
        let tables = empty_tables();
        for ty in [
            Type::Named("Map".to_string()),
            Type::Generic("Map".to_string(), vec![Type::String]),
            Type::Generic("FrozenArray".to_string(), vec![Type::I64, Type::I64]),
        ] {
            tables.with_ctx(|ctx| assert!(!ctx.supported_type(&ty), "{ty:?} is not storable"));
        }
        // the controls: the three that ARE collections
        assert!(matches!(
            lir_collection(&map_ty(Type::String, Type::I64)),
            Some((LirCollection::Map, _))
        ));
        assert!(matches!(
            lir_collection(&frozen_map_ty(Type::String, Type::I64)),
            Some((LirCollection::FrozenMap, _))
        ));
        assert!(matches!(
            lir_collection(&frozen_array_ty(Type::I64)),
            Some((LirCollection::FrozenArray, _))
        ));
    }

    /// Rewrite the declared mode of `Mode::by_value`'s single parameter,
    /// leaving its type alone.
    fn set_by_value_mode(tables: &mut TestTables, mode: ParamMode) {
        let methods = tables
            .iface_methods
            .get_mut("Mode")
            .expect("interface Mode");
        let slot = methods
            .iter()
            .position(|(n, _, _, _)| n == "by_value")
            .expect("by_value slot");
        methods[slot].2 = vec![mode];
    }

    // ── enum values and `match` (willow-0g8j.8) ────────────────────────────
    //
    // Perspectives m01-m28. The differential behaviour — that a LIR-compiled
    // enum program prints what the AST-compiled one prints, including under
    // WILLOW_GC_STRESS=alloc — is pinned in tests/integration/codegen.rs; these
    // pin the ELIGIBILITY boundary, which is what decides whether the walker
    // ever gets to run.

    /// The two enums nearly every perspective below needs: one with no payload
    /// anywhere (a bare `i64` tag) and one where a payload exists (so EVERY
    /// value of it is a `[tag | payload…]` heap object).
    const ENUMS: &str = "\
enum Color { Red, Green, Blue }
enum Shape { Nothing, Circle(i64), Rect(i64, i64), Labeled(String, f64) }
";

    fn enum_src(body: &str) -> String {
        format!("{ENUMS}{body}")
    }

    /// Assert that `name` is REFUSED — and that it was refused for a reason,
    /// not because the function never reached the lowered IR at all. A plain
    /// `!eligible_checked(..)` would pass vacuously if the source stopped
    /// compiling for an unrelated reason, which is exactly how a boundary test
    /// rots into a test of nothing.
    fn refused(src: &str, name: &str, fns: &[&str]) {
        let (f, tables) = lir_fn_and_tables(src, name, fns);
        assert!(
            tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)),
            "`{name}` must fall back to the AST emitter"
        );
    }

    // m01. the base case: a fieldless enum is constructed by its qualified name
    // and matched by variant. Nothing here loads a tag — the value IS the tag.
    #[test]
    fn m01_fieldless_enum_construction_and_match_eligible() {
        let src = enum_src(
            "fn f(c: Color) -> i64 {
                return match c {
                    Color::Red => 1,
                    Color::Green => 2,
                    _ => 3
                };
            }
            fn g() -> Color { return Color::Blue; }",
        );
        assert!(eligible_checked(&src, "f", &[]));
        assert!(eligible_checked(&src, "g", &[]));
    }

    // m02. a payload variant: construction takes arguments and the pattern
    // destructures them, which is the heap-object half of the representation
    // rule.
    #[test]
    fn m02_payload_enum_construction_and_tuple_pattern_eligible() {
        let src = enum_src(
            "fn f(s: Shape) -> i64 {
                return match s {
                    Shape::Rect(w, h) => w * h,
                    Shape::Circle(r) => r,
                    _ => 0
                };
            }
            fn g() -> Shape { return Shape::Rect(2, 3); }",
        );
        assert!(eligible_checked(&src, "f", &[]));
        assert!(eligible_checked(&src, "g", &[]));
    }

    // m03. a GENERIC enum is in the subset when the use site is concrete: the
    // scrutinee's type ARGUMENTS instantiate the declared placeholder payload,
    // and eligibility performs exactly the substitution
    // `resolve_variant_payload_types` performs at emission (willow-0g8j.2.1).
    #[test]
    fn m03_generic_user_enum_eligible() {
        let src = "enum Holder<T> { Empty, Full(T) }
            fn f(h: Holder<i64>) -> i64 {
                return match h {
                    Holder::Full(v) => v,
                    _ => 0
                };
            }
            fn g() -> Holder<i64> { return Holder::Full(1); }";
        assert!(eligible_checked(src, "f", &[]));
        assert!(eligible_checked(src, "g", &[]));

        // Instantiated with a payload OUTSIDE the subset, the same enum is out:
        // substitution is what makes the payload check meaningful.
        let inherited = "enum Holder<T> { Empty, Full(T) }
            open class Base { pub v: i64; }
            class Sub extends Base {}
            fn f(h: Holder<Base>) -> i64 {
                return match h {
                    Holder::Full(b) => b.v,
                    _ => 0
                };
            }";
        refused(inherited, "f", &[]);
    }

    // m04. `Option<T>` over a scalar is the ordinary `[tag | payload]` heap
    // object; over a GC payload it is the pointer niche, where `Some(x)` IS `x`
    // and `None` is null. Both are emittable, so both are in (willow-0g8j.2.1).
    #[test]
    fn m04_option_match_eligible() {
        let boxed = "fn f(x: Option<i64>) -> i64 {
                return match x {
                    Some(v) => v,
                    None => -1
                };
            }";
        assert!(eligible_checked(boxed, "f", &[]));

        let niche = "fn f(x: Option<String>) -> String {
                return match x {
                    Some(v) => v,
                    None => \"\"
                };
            }";
        assert!(eligible_checked(niche, "f", &[]));
    }

    // m05. the same for `Result`, which is generic in two parameters — so a
    // wrong-arity substitution would silently mis-slot the payload.
    #[test]
    fn m05_result_match_eligible() {
        let src = "fn f(r: Result<i64, String>) -> i64 {
                return match r {
                    Ok(v) => v,
                    Err(e) => -1
                };
            }";
        assert!(eligible_checked(src, "f", &[]));
    }

    // m06. a block-bodied arm can `return` out of the enclosing function, which
    // is a statement form with no emitter in expression position.
    #[test]
    fn m06_block_bodied_arm_ineligible() {
        // A DIVERGING block-bodied arm is admitted (willow-0g8j.2.5), but only
        // as effect statements ending in the departure. A `let` inside the arm
        // binds a name the walker's flat `vars` map cannot scope to this arm,
        // so it stays out.
        let src = enum_src(
            "fn f(c: Color) -> i64 {
                return match c {
                    Color::Red => { let t = 1; return t; },
                    _ => 2
                };
            }",
        );
        refused(&src, "f", &[]);
    }

    // m07. and a block arm that only declares a local is refused for the same
    // reason — it is the BODY SHAPE that is out, not the `return` specifically.
    #[test]
    fn m07_arm_declaring_a_local_ineligible() {
        let src = enum_src(
            "fn f(c: Color) {
                match c {
                    Color::Red => { let x = 1; println(x); },
                    _ => println(\"other\")
                }
            }",
        );
        refused(&src, "f", &[]);
    }

    // m08. a downcast pattern tests a runtime type id through an interface box,
    // not a tag, and the walker has no emitter for it.
    #[test]
    fn m08_class_downcast_pattern_ineligible() {
        let src = "interface Speaker { fn speak(self) -> String; }
            class Dog implements Speaker { pub fn speak(self) -> String { return \"woof\"; } }
            fn f(s: Speaker) -> String {
                return match s {
                    Dog(d) => d.speak(),
                    _ => \"other\"
                };
            }";
        refused(src, "f", &[]);
    }

    // m09. a String scrutinee is out even when every arm is unconditional: the
    // walker admits a scrutinee only when it can TEST one, and a String test
    // would be a content comparison rather than the integer compare the arm
    // chain emits.
    #[test]
    fn m09_string_scrutinee_ineligible() {
        let src = "fn f(s: String) -> i64 {
                return match s {
                    other => 1
                };
            }";
        refused(src, "f", &[]);
    }

    // m10. a class scrutinee would compare object identity, which no arm
    // pattern in the subset means to express.
    #[test]
    fn m10_class_scrutinee_ineligible() {
        let src = "class Cell { pub v: i64; }
            fn f(c: Cell) -> i64 {
                return match c {
                    other => other.v
                };
            }";
        refused(src, "f", &[]);
    }

    // m11. an `i64` scrutinee with literal arms: no tag load, the arm test
    // compares the scrutinee word itself.
    #[test]
    fn m11_int_literal_scrutinee_eligible() {
        let src = "fn f(n: i64) -> i64 {
                return match n {
                    0 => 100,
                    1 => 200,
                    k => k * 3
                };
            }";
        assert!(eligible_checked(src, "f", &[]));
    }

    // m12. a `bool` scrutinee compares an `i8`, so the arm's expected constant
    // has to be built at that width — a mismatch would be a verifier error, not
    // a wrong answer.
    #[test]
    fn m12_bool_literal_scrutinee_eligible() {
        let src = "fn f(b: bool) -> i64 {
                return match b {
                    true => 1,
                    false => 0
                };
            }";
        assert!(eligible_checked(src, "f", &[]));
    }

    // m13. a binding pattern aliases the WHOLE scrutinee and always matches, so
    // it both ends the arm chain and must be in scope for its own body.
    #[test]
    fn m13_binding_pattern_eligible_and_in_scope() {
        let src = enum_src(
            "fn f(s: Shape) -> i64 {
                return match s {
                    Shape::Circle(r) => r,
                    other => 0
                };
            }",
        );
        assert!(eligible_checked(&src, "f", &[]));
    }

    // m14. a binding is scoped to its OWN arm, and a later arm reading it never
    // reaches the walker at all: HIR lowering refuses the program outright, so
    // there is no lowered function for eligibility to claim. Pinning WHERE the
    // refusal happens matters — if lowering ever starts admitting this, the
    // `eligible_lenient` half below is what still keeps the walker out.
    #[test]
    fn m14_binding_does_not_leak_into_a_later_arm() {
        let src = enum_src(
            "fn f(s: Shape) -> i64 {
                return match s {
                    Shape::Circle(r) => r,
                    Shape::Rect(w, h) => r,
                    _ => 0
                };
            }",
        );
        let tokens = Lexer::new(&src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (_, diags) = crate::ir::lower::lower_program(&program);
        assert!(
            diags.iter().any(|d| d.message.contains("unbound variable")),
            "the leaked binding must be caught in lowering: {diags:?}"
        );
        assert!(!eligible_lenient(&src, "f", &[]));

        // the control: the same shape with each binding used in its own arm is
        // admitted, so the refusal above is about SCOPE and nothing else
        let ok = enum_src(
            "fn f(s: Shape) -> i64 {
                return match s {
                    Shape::Circle(r) => r,
                    Shape::Rect(w, h) => w,
                    _ => 0
                };
            }",
        );
        assert!(eligible_checked(&ok, "f", &[]));
    }

    // m15. the positive control for m14: the same name used inside the arm that
    // binds it is fine, and a second arm may reuse the name independently.
    #[test]
    fn m15_same_binding_name_in_two_arms_eligible() {
        let src = enum_src(
            "fn f(s: Shape) -> i64 {
                return match s {
                    Shape::Circle(v) => v,
                    Shape::Rect(v, h) => v + h,
                    _ => 0
                };
            }",
        );
        assert!(eligible_checked(&src, "f", &[]));
    }

    // m16. every arm feeds ONE result variable with no conversion inserted, so
    // two different enums — and an enum against a class — are never
    // representation-compatible even though both are `Type::Named`.
    #[test]
    fn m16_two_named_types_are_never_repr_compatible() {
        let color = Type::Named("Color".to_string());
        let shape = Type::Named("Shape".to_string());
        assert!(assignable_repr(&color, &color));
        assert!(!assignable_repr(&color, &shape));
        assert!(!assignable_repr(&shape, &color));
        assert!(!assignable_repr(&color, &Type::Named("Cell".to_string())));
        // and a named type never matches a scalar, in either direction, even
        // when both are one machine word
        assert!(!assignable_repr(&color, &Type::I64));
        assert!(!assignable_repr(&Type::I64, &color));
    }

    // m17. a `match` in statement position produces no value. The walker still
    // emits the arm chain and merge block; it just seeds and discards a
    // `Void`-typed result.
    #[test]
    fn m17_statement_position_match_eligible() {
        let src = enum_src(
            "fn f(c: Color) {
                match c {
                    Color::Red => println(\"red\"),
                    _ => println(\"other\")
                }
            }",
        );
        assert!(eligible_checked(&src, "f", &[]));
    }

    // m18. a payload type is vetted like any other storage type: a `String` is
    // in the subset, a generic `Option<i64>` payload is not — and one bad
    // payload on ONE variant takes the whole enum out.
    #[test]
    fn m18_payload_types_are_vetted() {
        let ok = enum_src(
            "fn f(s: Shape) -> String {
                return match s {
                    Shape::Labeled(name, scale) => name,
                    _ => \"\"
                };
            }",
        );
        assert!(eligible_checked(&ok, "f", &[]));

        // An `Option<i64>` payload is itself in the subset as of
        // willow-0g8j.2.1; a payload that takes part in inheritance is not, and
        // one bad payload on ONE variant still takes the whole enum out.
        let nested = "enum Wrapped { Nothing, Boxed(Option<i64>) }
            fn f(w: Wrapped) -> i64 {
                return match w {
                    Wrapped::Nothing => 0,
                    _ => 1
                };
            }";
        assert!(eligible_checked(nested, "f", &[]));

        let bad = "open class Base { pub v: i64; }
            class Sub extends Base {}
            enum Wrapped { Nothing, Boxed(Base) }
            fn f(w: Wrapped) -> i64 {
                return match w {
                    Wrapped::Nothing => 0,
                    _ => 1
                };
            }";
        refused(bad, "f", &[]);
    }

    // m19. a self-referential enum must terminate the payload walk rather than
    // recursing forever — the `open` set is what makes the check total, and the
    // enum is admitted rather than merely surviving the question.
    #[test]
    fn m19_self_referential_enum_terminates_and_is_admitted() {
        let src = "enum Chain { End, Link(i64, Chain) }
            fn f(c: Chain) -> i64 {
                return match c {
                    Chain::Link(v, rest) => v,
                    _ => 0
                };
            }";
        assert!(eligible_checked(src, "f", &[]));
        // mutual recursion through a second enum closes the other cycle shape
        let mutual = "enum Odd { Zero, Next(Even) }
            enum Even { One, Prev(Odd) }
            fn f(o: Odd) -> i64 {
                return match o {
                    Odd::Zero => 0,
                    _ => 1
                };
            }";
        assert!(eligible_checked(mutual, "f", &[]));
    }

    // m20. an enum field does not disqualify the class that holds it: the class
    // stays "simple", so `new` and field reads stay on the walker.
    #[test]
    fn m20_enum_as_class_field_keeps_the_class_simple() {
        let src = enum_src(
            "class Course { pub facing: Color; pub outline: Shape; }
            fn f() -> i64 {
                let c = new Course(Color::Red, Shape::Circle(2));
                return match c.outline {
                    Shape::Circle(r) => r,
                    _ => 0
                };
            }",
        );
        assert!(eligible_checked(&src, "f", &[]));
    }

    // m21. enums are array elements like any other value; a payload enum's
    // elements are GC references the array has to trace, and the element `is_ref`
    // flag comes from the enum-aware `is_gc_managed`.
    #[test]
    fn m21_enum_array_elements_eligible() {
        let src = format!(
            "import std::collections::Array;
            {ENUMS}
            fn f(xs: Array<Shape>) -> i64 {{
                return match xs[0] {{
                    Shape::Circle(r) => r,
                    _ => 0
                }};
            }}
            fn g(xs: Array<Color>) -> i64 {{
                return match xs[0] {{
                    Color::Red => 1,
                    _ => 0
                }};
            }}"
        );
        assert!(eligible_checked(&src, "f", &[]));
        assert!(eligible_checked(&src, "g", &[]));
    }

    // m22. an enum crosses a call boundary in both directions — as a parameter
    // and as a return type.
    #[test]
    fn m22_enum_parameter_and_return_eligible() {
        let src = enum_src(
            "fn make(n: i64) -> Shape { return Shape::Circle(n); }
            fn f(n: i64) -> i64 {
                return match make(n) {
                    Shape::Circle(r) => r,
                    _ => 0
                };
            }",
        );
        assert!(eligible_checked(&src, "f", &["make"]));
    }

    // m23. `supported_enum` and `supported_class` must not both claim a name.
    // An interface registration wins, because interface VALUES are boxes.
    #[test]
    fn m23_a_name_registered_as_an_interface_is_not_a_supported_enum() {
        let src = enum_src("fn f(c: Color) -> i64 { return 0; }");
        let (_, mut tables) = checked_lowering(&src, &[]);
        tables.with_ctx(|ctx| {
            assert!(ctx.supported_enum("Color"));
            assert!(ctx.is_enum("Color"));
            assert!(ctx.supported_type(&Type::Named("Color".to_string())));
        });
        tables.interfaces.insert("Color".to_string());
        tables.with_ctx(|ctx| {
            assert!(!ctx.supported_enum("Color"));
            // still an enum by declaration — which is exactly why the class
            // path must keep excluding it
            assert!(ctx.is_enum("Color"));
        });
    }

    // m24. an undeclared name is not an enum, so nothing routes a module call
    // or an unknown static call into enum construction.
    #[test]
    fn m24_unknown_names_are_not_enums() {
        let src = enum_src("fn f(c: Color) -> i64 { return 0; }");
        let (_, tables) = checked_lowering(&src, &[]);
        tables.with_ctx(|ctx| {
            for name in ["Cell", "math", "Map", "String"] {
                assert!(!ctx.is_enum(name), "{name} is not a declared enum");
                assert!(!ctx.supported_enum(name), "{name} is not a supported enum");
            }
            // `Option`/`Result` ARE declared (by the prelude), but only an
            // instantiation of one is a supported enum: the bare name supplies
            // no type arguments, so `enum_instance` refuses it and no
            // uninstantiated generic can reach construction or `match`.
            for name in ["Option", "Result"] {
                assert!(ctx.is_enum(name), "{name} is a prelude enum");
                assert!(
                    !ctx.supported_enum(name),
                    "bare `{name}` has no type arguments"
                );
            }
        });
    }

    // m25. arity is part of the pattern's contract: a destructuring pattern
    // whose binding count differs from the variant's payload count would read
    // (or skip) a slot that does not exist. Like m14 this is refused during
    // lowering, so the walker never sees it — and the `eligible_lenient` half
    // is what still holds if lowering ever learns to represent it.
    #[test]
    fn m25_wrong_pattern_arity_ineligible() {
        let src = enum_src(
            "fn f(s: Shape) -> i64 {
                return match s {
                    Shape::Rect(w) => w,
                    _ => 0
                };
            }",
        );
        let tokens = Lexer::new(&src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (_, diags) = crate::ir::lower::lower_program(&program);
        assert!(
            diags.iter().any(|d| d.message.contains("arity")),
            "a mis-arity pattern must be caught in lowering: {diags:?}"
        );
        assert!(!eligible_lenient(&src, "f", &[]));
    }

    // m26. the representation rule itself, read from the same place emission
    // reads it: ANY payload variant makes EVERY value of the enum a heap
    // object, and an enum with none is a bare tag that must NOT be rooted.
    #[test]
    fn m26_representation_follows_the_whole_enum_not_the_variant() {
        use crate::semantic::symbols::{EnumInfo, EnumVariantInfo};
        let variant = |name: &str, payloads: Vec<Type>, tag: i64| EnumVariantInfo {
            name: name.to_string(),
            payload_types: payloads,
            tag,
            declaration_span: crate::diagnostics::Span::dummy(),
        };
        let mut infos = HashMap::new();
        infos.insert(
            "Color".to_string(),
            EnumInfo {
                name: "Color".to_string(),
                public: true,
                type_params: vec![],
                declaration_span: crate::diagnostics::Span::dummy(),
                variants: vec![variant("Red", vec![], 0), variant("Green", vec![], 1)],
            },
        );
        infos.insert(
            "Shape".to_string(),
            EnumInfo {
                name: "Shape".to_string(),
                public: true,
                type_params: vec![],
                declaration_span: crate::diagnostics::Span::dummy(),
                variants: vec![
                    // the payload-less variant comes FIRST, so a per-variant
                    // rule would get this backwards
                    variant("Nothing", vec![], 0),
                    variant("Circle", vec![Type::I64], 1),
                ],
            },
        );
        assert!(!is_gc_managed(&Type::Named("Color".to_string()), &infos));
        assert!(is_gc_managed(&Type::Named("Shape".to_string()), &infos));
        // and an undeclared named type is a class, which always is
        assert!(is_gc_managed(&Type::Named("Cell".to_string()), &infos));
    }

    // m27. an interface-typed payload slot holds a BOX, and the box is built
    // from the DECLARED payload type rather than the argument's own type — so
    // a class argument is admitted only because the store position can convert.
    #[test]
    fn m27_interface_payload_is_eligible_and_boxed() {
        let src = "interface Named { fn describe(self) -> String; }
            class Marker implements Named {
                pub label: String;
                pub fn describe(self) -> String { return self.label; }
            }
            enum Tag { Untagged, Marked(Named) }
            fn make() -> Tag { return Tag::Marked(new Marker(\"m\")); }
            fn f(t: Tag) -> String {
                return match t {
                    Tag::Marked(n) => n.describe(),
                    _ => \"untagged\"
                };
            }";
        assert!(eligible_checked(src, "make", &[]));
        assert!(eligible_checked(src, "f", &[]));
        // a class value is NOT repr-compatible with the interface slot, which
        // is why the store position (and not an arm body) is where the box is
        // allowed to appear
        assert!(!assignable_repr(
            &Type::Named("Named".to_string()),
            &Type::Named("Marker".to_string())
        ));
    }

    // m28. a `match` may be an arm body of another `match`: the emitter is
    // re-entrant, and the inner one's merge block leaves the builder where the
    // outer one expects it.
    #[test]
    fn m28_nested_match_eligible() {
        let src = enum_src(
            "fn f(c: Color, s: Shape) -> i64 {
                return match c {
                    Color::Red => match s {
                        Shape::Circle(r) => r,
                        _ => 0
                    },
                    _ => match s {
                        Shape::Rect(w, h) => w * h,
                        _ => -1
                    }
                };
            }",
        );
        assert!(eligible_checked(&src, "f", &[]));
    }

    // ── `Option`, `Result` and `?` (willow-0g8j.2.1) ──────────────────────
    //
    // Perspectives p01-p24. `Option` and `Result` are ordinary prelude enums,
    // so the m-block above already covers "does the walker understand a generic
    // enum". What these pin is the part that is NOT generic-enum machinery: the
    // two representations an `Option` instance can have, the value-taking
    // methods, `Map::get` (the one builtin that hands back an `Option`), and
    // `?` — the only expression in the subset that leaves the function from the
    // middle of another expression. The differential behaviour lives in
    // tests/integration/codegen.rs; these pin the ELIGIBILITY boundary.

    // p01. the representation split is per INSTANTIATION, not per enum:
    // `Option<i64>` is a `[tag | payload]` object and `Option<String>` is the
    // pointer niche. Both are emittable, so a function may mix them.
    #[test]
    fn p01_both_option_representations_eligible() {
        let src = "fn f(a: Option<i64>, b: Option<String>) -> i64 {
                return match a {
                    Some(v) => v,
                    None => match b { Some(_) => 1, None => 0 }
                };
            }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // p02. an `Option` of an `Option` is never the niche — the inner `None`
    // would be indistinguishable from the outer one — and the walker still
    // admits it, because `option_repr` is what decides, not the shape.
    #[test]
    fn p02_nested_option_eligible() {
        let src = "fn f(x: Option<Option<i64>>) -> i64 {
                return match x {
                    Some(inner) => inner.unwrap_or(-1),
                    None => -2
                };
            }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // p03. construction: the payload slot is vetted against the INSTANCE, so
    // `Option::Some(1)` at type `Option<i64>` checks its argument against
    // `i64`, not against the declaration's `T`.
    #[test]
    fn p03_option_construction_vets_the_instantiated_payload() {
        let src = "fn f() -> Option<i64> { return Option::Some(1); }
            fn g() -> Option<String> { return Option::Some(\"a\"); }";
        assert!(eligible_checked(src, "f", &["f", "g"]));
        assert!(eligible_checked(src, "g", &["f", "g"]));
    }

    // p04. the fieldless variant of a generic enum: HIR spells a bare `None`
    // as a static-property read, and it has to instantiate the same way.
    #[test]
    fn p04_bare_none_is_a_variant_read() {
        let src = "fn f() -> Option<i64> { return None; }
            fn g() -> Option<String> { return Option::None; }";
        assert!(eligible_checked(src, "f", &["f", "g"]));
        assert!(eligible_checked(src, "g", &["f", "g"]));
    }

    // p05. `Result<void, E>::Ok()` takes ZERO arguments while the substituted
    // payload list is `[void]`. Eligibility and emission both normalise that
    // list away, so the arity check sees `0 == 0` and the object built is the
    // one-word one the AST emitter builds.
    #[test]
    fn p05_void_ok_payload_is_normalised_away() {
        let src = "fn f() -> Result<void, String> { return Ok(); }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // p06. a payload type outside the subset takes the whole instance with it.
    // The control is the SAME enum at a supported instantiation, so this
    // cannot pass by rejecting `Option` wholesale.
    #[test]
    fn p06_unsupported_payload_rejects_the_instance() {
        let src = "open class Base { pub v: i64; }
            class Sub extends Base {}
            fn f(x: Option<Base>) -> i64 {
                return match x { Some(b) => b.v, None => 0 };
            }
            fn g(x: Option<i64>) -> i64 {
                return match x { Some(v) => v, None => 0 };
            }";
        refused(src, "f", &["f", "g"]);
        assert!(eligible_checked(src, "g", &["f", "g"]));
    }

    // p07. the value-taking `Option` methods, all five of them. They are
    // emitted inline off the same two words the `match` reads, so admitting
    // `match` without them would be an arbitrary split.
    #[test]
    fn p07_option_value_methods_eligible() {
        let src = "fn a(x: Option<i64>) -> bool { return x.is_some(); }
            fn b(x: Option<i64>) -> bool { return x.is_none(); }
            fn c(x: Option<i64>) -> i64 { return x.unwrap(); }
            fn d(x: Option<i64>) -> i64 { return x.expect(\"boom\"); }
            fn e(x: Option<i64>) -> i64 { return x.unwrap_or(0); }";
        let fns = &["a", "b", "c", "d", "e"];
        for name in fns {
            assert!(eligible_checked(src, name, fns), "`{name}` must be in");
        }
    }

    // p08. the same for `Result`, including `unwrap_err`, which reads the
    // SECOND type argument — a wrong-slot substitution would return the Ok
    // payload's type here and the store check downstream would not notice.
    #[test]
    fn p08_result_value_methods_eligible() {
        let src = "fn a(r: Result<i64, String>) -> bool { return r.is_ok(); }
            fn b(r: Result<i64, String>) -> bool { return r.is_err(); }
            fn c(r: Result<i64, String>) -> i64 { return r.unwrap(); }
            fn d(r: Result<i64, String>) -> String { return r.unwrap_err(); }
            fn e(r: Result<i64, String>) -> i64 { return r.expect(\"boom\"); }
            fn g(r: Result<i64, String>) -> i64 { return r.unwrap_or(0); }";
        let fns = &["a", "b", "c", "d", "e", "g"];
        for name in fns {
            assert!(eligible_checked(src, name, fns), "`{name}` must be in");
        }
    }

    // p09. the value methods work over the niche representation too, where
    // `unwrap` is the identity on the receiver rather than a payload load.
    #[test]
    fn p09_option_value_methods_over_the_niche_eligible() {
        let src = "fn a(x: Option<String>) -> bool { return x.is_some(); }
            fn b(x: Option<String>) -> String { return x.unwrap(); }
            fn c(x: Option<String>) -> String { return x.unwrap_or(\"\"); }";
        let fns = &["a", "b", "c"];
        for name in fns {
            assert!(eligible_checked(src, name, fns), "`{name}` must be in");
        }
    }

    // p10. the closure-taking combinators joined the same arm once function
    // values existed (willow-0g8j.2.2). The method table is still a CLOSED
    // list, so a name outside it falls through rather than being emitted as an
    // unknown call.
    #[test]
    fn p10_closure_combinators_eligible() {
        let src = "fn f(x: Option<i64>) -> i64 { return x.map(|v: i64| v * 2).unwrap_or(0); }";
        assert!(eligible_checked(src, "f", &["f"]));
        // The list is closed against names the language does not have either,
        // so a future `Option::filter` cannot be emitted before it is written.
        let opt_i64 = Type::Generic("Option".to_string(), vec![Type::I64]);
        let pred = Type::Fn(vec![Type::I64], Box::new(Type::Bool));
        assert!(option_result_method(&opt_i64, "filter", &[pred]).is_none());
    }

    // p11. `unwrap_or`'s argument must have the payload's representation. A
    // `String` default for an `Option<i64>` would be handed to the merge as a
    // pointer in an integer slot.
    #[test]
    fn p11_unwrap_or_argument_type_is_checked() {
        let src = "fn f(x: Option<String>) -> String { return x.unwrap_or(\"d\"); }";
        assert!(eligible_checked(src, "f", &["f"]));

        // The arity is checked in the same place.
        let no_arg = "fn f(x: Option<i64>) -> i64 { return x.unwrap(); }";
        assert!(eligible_checked(no_arg, "f", &["f"]));
        let extra = "fn f(x: Option<i64>) -> bool { return x.is_some(); }";
        assert!(eligible_checked(extra, "f", &["f"]));
    }

    // p12. `Map::get` is the one builtin that hands back an `Option`, and the
    // representation the runtime builds is chosen from the map's OWN value
    // type — so the result type is checked against it rather than assumed.
    #[test]
    fn p12_map_get_yields_the_maps_own_option() {
        let src = "import std::collections::Map;
            fn f(m: Map<String, i64>) -> i64 { return m.get(\"k\").unwrap_or(-1); }
            fn g(m: Map<String, String>) -> String {
                return match m.get(\"k\") { Some(v) => v, None => \"\" };
            }";
        assert!(eligible_checked(src, "f", &["f", "g"]));
        assert!(eligible_checked(src, "g", &["f", "g"]));
    }

    // p13. a `get` whose result type is an `Option` over something OTHER than
    // the map's value type is refused: the two would disagree about the niche.
    // Built by rewriting the node's type, because the checker cannot produce
    // this state from source.
    #[test]
    fn p13_map_get_result_must_match_the_value_type() {
        let src = "import std::collections::Map;
            fn f(m: Map<String, String>) -> Option<String> { return m.get(\"k\"); }";
        assert!(eligible_checked(src, "f", &["f"]));

        let (mut f, tables) = lir_fn_and_tables(src, "f", &["f"]);
        let Some(Terminator::Return(Some(v))) = f.blocks.last_mut().map(|b| &mut b.terminator)
        else {
            panic!("the function returns the `get` result");
        };
        v.ty = Type::Generic("Option".to_string(), vec![Type::I64]);
        f.return_type = v.ty.clone();
        assert!(
            tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)),
            "a `get` typed over a different value type must fall back"
        );
    }

    // p14. `?` on a `Result` whose error type already matches the enclosing
    // function's: the failure path forwards the operand unchanged.
    #[test]
    fn p14_try_propagate_on_result_eligible() {
        let src = "fn src(n: i64) -> Result<i64, String> { return Result::Ok(n); }
            fn f(n: i64) -> Result<i64, String> {
                let v = src(n)?;
                return Result::Ok(v + 1);
            }";
        assert!(eligible_checked(src, "f", &["src", "f"]));
    }

    // p15. `?` on an `Option`, both directions across the representation
    // boundary: niche operand into a boxed return and boxed operand into a
    // niche return. The failure value is CONSTRUCTED for the destination in
    // both, which is why neither is a special case of the other.
    #[test]
    fn p15_try_propagate_across_option_representations_eligible() {
        let src = "fn niche(n: i64) -> Option<String> { return Option::Some(\"a\"); }
            fn boxed(n: i64) -> Option<i64> { return Option::Some(n); }
            fn f(n: i64) -> Option<i64> {
                let s = niche(n)?;
                return Option::Some(1);
            }
            fn g(n: i64) -> Option<String> {
                let v = boxed(n)?;
                return Option::Some(\"b\");
            }";
        let fns = &["niche", "boxed", "f", "g"];
        assert!(eligible_checked(src, "f", fns));
        assert!(eligible_checked(src, "g", fns));
    }

    // p16. `?` with automatic error conversion (willow-1ow): the operand's
    // error type differs from the function's, so the failure path calls
    // `into()` and re-wraps. Admitting it depends on `Result<i64, PortError>`
    // being a supported type, which is what keeps the `into` dispatch a single
    // direct call.
    #[test]
    fn p16_try_propagate_with_error_conversion_eligible() {
        let src = "class ConfigError { pub code: i64; }
            class PortError implements Into<ConfigError> {
                pub raw: i64;
                pub fn into(self) -> ConfigError { return new ConfigError(500); }
            }
            fn read(n: i64) -> Result<i64, PortError> { return Result::Ok(n); }
            fn f(n: i64) -> Result<i64, ConfigError> {
                let v = read(n)?;
                return Result::Ok(v);
            }";
        assert!(eligible_checked(src, "f", &["read", "f"]));
    }

    // p17. `?` on a `Result<void, E>` stays OUT. The walker's `Ok()` object is
    // one word, so the success path's word-1 load would read past it — and the
    // control proves the same function shape is otherwise fine.
    #[test]
    fn p17_try_propagate_on_a_void_result_rejected() {
        let src = "fn unit(n: i64) -> Result<void, String> { return Ok(); }
            fn valued(n: i64) -> Result<i64, String> { return Result::Ok(n); }
            fn f(n: i64) -> Result<i64, String> {
                unit(n)?;
                return Result::Ok(1);
            }
            fn g(n: i64) -> Result<i64, String> {
                let v = valued(n)?;
                return Result::Ok(v);
            }";
        let fns = &["unit", "valued", "f", "g"];
        refused(src, "f", fns);
        assert!(eligible_checked(src, "g", fns));
    }

    // p18. `?` propagates the rejection of its OPERAND: an unsupported inner
    // expression cannot be laundered by wrapping it in a `?`.
    #[test]
    fn p18_try_propagate_inherits_its_operands_rejection() {
        let src = "fn f(n: i64) -> Result<i64, String> {
                let v = unknown(n)?;
                return Result::Ok(v);
            }
            fn unknown(n: i64) -> Result<i64, String> { return Result::Ok(n); }";
        // `unknown` is deliberately absent from the known-symbol set.
        refused(src, "f", &["f"]);
    }

    // p19. `?` inside a loop body is still just an expression: the early
    // return leaves the loop and the function at once.
    #[test]
    fn p19_try_propagate_inside_a_loop_eligible() {
        let src = "fn step(n: i64) -> Result<i64, String> { return Result::Ok(n); }
            fn f(n: i64) -> Result<i64, String> {
                let mut total = 0;
                let mut i = 0;
                while i < n {
                    total = total + step(i)?;
                    i = i + 1;
                }
                return Result::Ok(total);
            }";
        assert!(eligible_checked(src, "f", &["step", "f"]));
    }

    // p20. the fallback reason names the `?` itself when the `?` is what
    // blocked the function, rather than blaming the enclosing statement.
    #[test]
    fn p20_try_propagate_is_named_in_the_reason() {
        let src = "fn unit(n: i64) -> Result<void, String> { return Ok(); }
            fn f(n: i64) -> Result<i64, String> {
                unit(n)?;
                return Result::Ok(1);
            }";
        let reason = rejected(src, "f", &["unit", "f"]);
        assert!(
            reason.contains("`?` propagation"),
            "the reason must name the `?`: {reason}"
        );
    }

    // p21. `Option` and `Result` are enums by REGISTRATION, not by name. With
    // the enum table emptied — the state a missing prelude registration would
    // produce — nothing about them is assumed.
    #[test]
    fn p21_option_is_not_special_cased_by_name() {
        let src = "fn f(x: Option<i64>) -> i64 {
                return match x { Some(v) => v, None => 0 };
            }";
        let (f, mut tables) = lir_fn_and_tables(src, "f", &["f"]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
        tables.enums.clear();
        assert!(
            tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)),
            "with no enum registered, `Option<i64>` must not be assumed"
        );
    }

    // p22. a generic that is NOT an enum keeps falling back, so admitting
    // `Type::Generic` for enums did not open the shape as a whole.
    #[test]
    fn p22_non_enum_generics_still_fall_back() {
        let tables = empty_tables();
        for ty in [
            Type::Generic("Task".to_string(), vec![Type::I64]),
            Type::Generic("Future".to_string(), vec![Type::I64]),
            Type::Generic("Range".to_string(), vec![Type::I64]),
        ] {
            tables.with_ctx(|ctx| {
                assert!(!ctx.supported_type(&ty), "{ty:?} is not an admitted enum");
            });
        }
    }

    // p23. an `Option` in every storage position an ordinary value has: a
    // class field, an array element, a parameter and a return.
    #[test]
    fn p23_options_in_ordinary_storage_positions_eligible() {
        let src = "import std::collections::Array;
            class Reading { pub value: Option<i64>; }
            fn field(r: Reading) -> i64 { return r.value.unwrap_or(0); }
            fn elem(xs: Array<Option<i64>>) -> i64 { return xs[0].unwrap_or(0); }
            fn pass(x: Option<String>) -> Option<String> { return x; }";
        let fns = &["field", "elem", "pass"];
        for name in fns {
            assert!(eligible_checked(src, name, fns), "`{name}` must be in");
        }
    }

    // p24. a recursive generic enum closes on itself at a concrete
    // instantiation. Without keying the guard on the INSTANTIATED name this
    // walk would not terminate, and `Option<Option<i64>>` (p02) proves the
    // guard is not simply "stop at the enum name".
    #[test]
    fn p24_recursive_generic_enum_terminates() {
        let src = "enum List<T> { Cons(T, List<T>), Nil }
            fn f(l: List<i64>) -> i64 {
                return match l { List::Cons(h, t) => h, List::Nil => 0 };
            }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // ---------------------------------------------------------------------
    // Fallback reasons (willow-0g8j.2 groundwork).
    //
    // `lir_rejection_reason` is what `WILLOW_LIR_REQUIRE=1` prints, and it is
    // also the single implementation of eligibility. Perspectives r1..r24:
    // agreement with the predicate, each rejection site in the function-level
    // scan (return type, by-ref param, param type, duplicate name, `let` type,
    // store conversion, unresolvable assign target, non-array element store,
    // unknown field layout, `void` return value, `defer`, static-field store,
    // `super.init`), minimality of the blamed sub-expression, the two scopes
    // the search must not descend into (lambda body, unbindable match arm),
    // line attribution, and the naming of each expression form.
    // ---------------------------------------------------------------------

    // r1. an eligible function has no reason at all — the wrapper and the
    // reason form are the same decision.
    #[test]
    fn r1_eligible_function_has_no_reason() {
        let src = "fn f(a: i64) -> i64 { let b = a * 2; return b + 1; }";
        assert_eq!(reason_of(src, "f", &["f"]), None);
    }

    // r2. the predicate and the reason can never disagree, over every source
    // the surrounding tests use in both directions.
    #[test]
    fn r2_reason_agrees_with_predicate() {
        let cases: &[&str] = &[
            "fn f(a: i64) -> i64 { return a; }",
            "fn f() -> Range<i64> { return 0..3; }",
            "fn f(x: &mut i64) { x = x + 1; }",
            "class C { pub v: i64; } fn f(c: C) -> i64 { return c.v; }",
            "fn f() { let a = 1; while a < 2 { let a = 2; print(a); } }",
            "fn f() -> i64 { return g(); } fn g() -> i64 { return 1; }",
        ];
        for src in cases {
            let (p, tables) = checked_lowering(src, &["f", "g"]);
            for lf in &p.functions {
                let (supported, reason) = tables.with_ctx(|ctx| {
                    (
                        lir_supported_function(lf, ctx),
                        lir_rejection_reason(lf, ctx),
                    )
                });
                assert_eq!(
                    supported,
                    reason.is_none(),
                    "`{}` disagreed: supported={supported} reason={reason:?}",
                    lf.name
                );
            }
        }
    }

    // r3. an unsupported return type is named, since that is the one blocker
    // no line number inside the body can point at.
    #[test]
    fn r3_return_type_reason_names_the_type() {
        let src = "fn f() -> Range<i64> { return 0..3; }";
        assert_eq!(
            rejected(src, "f", &["f"]),
            "its return type `Range<i64>` is outside the walker's subset"
        );
    }

    // r4. a by-reference parameter is reported as such, not as a bad type: the
    // type is fine, the ABI is not.
    #[test]
    fn r4_by_reference_parameter_reason() {
        let src = "fn f(x: &mut i64) { x = x + 1; }";
        assert_eq!(
            rejected(src, "f", &["f"]),
            "parameter `x` is taken by reference"
        );
    }

    // r5. an unsupported parameter type names the parameter AND the type.
    #[test]
    fn r5_parameter_type_reason_names_both() {
        let src = "fn f(r: Range<i64>) -> i64 { return 1; }";
        let reason = rejected(src, "f", &["f"]);
        assert!(reason.contains("parameter `r`"), "{reason}");
        assert!(reason.contains("`Range<i64>`"), "{reason}");
    }

    // r6. two bindings of one name are rejected by NAME: LIR's flat scopes are
    // the reason, and the message has to say so or it reads like a bug.
    #[test]
    fn r6_duplicate_binding_reason_names_the_binding() {
        let src = "fn f() { let mut i = 0; while i < 2 { let x = i; print(x); i = i + 1; } \
                   let mut j = 0; while j < 2 { let x = j; print(x); j = j + 1; } }";
        let reason = rejected(src, "f", &["f"]);
        assert!(reason.starts_with("`let x` reuses a name"), "{reason}");
    }

    // r7. a `let` whose BINDING type is unsupported blames the binding, not the
    // initialiser (the slot's type is what the walker cannot represent).
    #[test]
    fn r7_let_binding_type_reason() {
        let src = "fn f() { let r: Range<i64> = 0..3; print(1); }";
        let reason = rejected(src, "f", &["f"]);
        assert!(
            reason.starts_with("`let r` binds type `Range<i64>`"),
            "{reason}"
        );
    }

    // r8. the blamed node is the SMALLEST unsupported one: an unknown callee
    // nested three levels down is named, not the enclosing `let`.
    #[test]
    fn r8_reason_blames_the_innermost_node() {
        // `g` exists for the type checker but is NOT in the walker's known
        // symbols, which is what makes the call the unsupported node.
        let src = "fn g() -> i64 { return 2; } fn f() -> i64 { let a = 1 + (2 * g()); return a; }";
        let reason = rejected(src, "f", &["f"]);
        assert!(reason.starts_with("the call to `g` at line"), "{reason}");
    }

    // r9. the line is the offending node's, not the function's or the
    // statement's — the whole point is to be able to jump to it.
    #[test]
    fn r9_reason_reports_the_node_line() {
        let src = "fn g() -> i64 { return 2; }\n\
                   fn f() -> i64 {\n\
                       let a = 1;\n\
                       let b = a + g();\n\
                       return b;\n\
                   }";
        let reason = rejected(src, "f", &["f"]);
        assert!(reason.contains("at line 4"), "{reason}");
    }

    // r10. a node whose own type is outside the subset says so; that is a
    // different fix from an unsupported construct with a fine type.
    #[test]
    fn r10_unsupported_type_is_reported_with_the_node() {
        // The argument is the unsupported node: the call itself is `i64` and
        // its callee is known, so only the range can be at fault.
        let src = "fn take(r: Range<i64>) -> i64 { return 1; } \
                   fn f() -> i64 { return take(0..3); }";
        let reason = rejected(src, "f", &["f", "take"]);
        assert!(reason.starts_with("a range at line"), "{reason}");
        assert!(reason.contains("has type `Range<i64>`"), "{reason}");
    }

    // r11. a `defer` is a whole-function property (it changes every exit), so
    // it is reported without a line.
    #[test]
    fn r11_defer_reason() {
        let src = "fn g() {} fn f() { defer g(); print(1); }";
        assert_eq!(rejected(src, "f", &["f", "g"]), "it registers a `defer`");
    }

    // r12. a static-property STORE is named with its class and field.
    #[test]
    fn r12_static_field_store_reason() {
        let src = "class Counter { pub static mut total: i64 = 0; \
                   pub static fn bump() { Counter::total = Counter::total + 1; } }";
        let reason = rejected(src, "Counter::bump", &[]);
        assert!(reason.contains("`Counter::total`"), "{reason}");
    }

    // r13. `super.init(...)` is reported as itself. Built by hand: from source
    // a constructor with a `super.init` is in a subclass, so its `self`
    // parameter is rejected first and this arm is never reached. It still has
    // to be right for the day inheritance becomes supported.
    #[test]
    fn r13_super_init_reason() {
        let f = LirFunction {
            name: "Child::init".to_string(),
            params: Vec::new(),
            return_type: Type::Void,
            blocks: vec![LirBlock {
                id: crate::ir::lowered::BlockId(0),
                instrs: vec![LirInst::SuperInit { args: Vec::new() }],
                terminator: Terminator::Return(None),
            }],
        };
        let (program, errs) = Parser::new(Lexer::new("fn f() {}").tokenize().expect("lex")).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let tables = TestTables::build(&program, &[], &[]);
        assert_eq!(
            tables.with_ctx(|ctx| lir_rejection_reason(&f, ctx)),
            Some("it calls `super.init(...)`".to_string())
        );
    }

    // r14. a `return` of a `void` value has no slot in the signature; the
    // message says that rather than blaming the expression.
    #[test]
    fn r14_void_return_value_reason() {
        let src = "fn g() {} fn f() { return g(); }";
        let reason = rejected(src, "f", &["f", "g"]);
        assert!(reason.contains("yields a `void` value"), "{reason}");
    }

    // r15. when a lambda IS the blocker — here because its lifted symbol was
    // never registered, the state an imported module still produces — it is
    // reported as a lambda and NOT descended into: its body binds its own
    // parameters, so a name inside would be blamed as unbound.
    #[test]
    fn r15_lambda_is_not_descended_into() {
        let src = "fn apply(g: fn(i64) -> i64) -> i64 { return g(2); } \
                   fn caller() -> i64 { return apply(|x: i64| -> i64 { return x + 1; }); }";
        let fns = &["caller", "apply"];
        let (f, mut tables) = lir_fn_and_tables(src, "caller", fns);
        tables.lambdas.clear();
        let reason = tables
            .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
            .expect("an unregistered lambda must refuse");
        assert!(reason.contains("a lambda"), "{reason}");
        assert!(!reason.contains("the variable `x`"), "{reason}");
    }

    // r16. an arm whose pattern the walker cannot bind is skipped for the same
    // reason: its body's bindings are not in `names`, so the `match` itself is
    // the honest answer.
    #[test]
    fn r16_unbindable_arm_blames_the_match() {
        // `ClassDowncast` needs the interface dispatch metadata the walker
        // does not carry, so no arm here is bindable.
        let src = "interface Shape { fn area(self) -> i64; } \
                   class Sq implements Shape { \
                       pub s: i64; \
                       pub init(self, s: i64) { self.s = s; } \
                       pub fn area(self) -> i64 { return self.s * self.s; } \
                   } \
                   fn pick(s: Shape) -> i64 { return match s { Sq(q) => 1, _ => 0 }; }";
        let reason = rejected(src, "pick", &["pick"]);
        assert!(reason.starts_with("the `match` on a `Shape`"), "{reason}");
    }

    // r17. when the scrutinee is the problem, the scrutinee is blamed — the
    // arms may all be perfectly supported.
    #[test]
    fn r17_match_blames_its_scrutinee() {
        let src = "enum Color { Red, Green } \
                   fn g() -> Color { return Color::Red; } \
                   fn f() -> i64 { return match g() { Color::Red => 1, _ => 0 }; }";
        let reason = rejected(src, "f", &["f"]);
        assert!(reason.starts_with("the call to `g`"), "{reason}");
    }

    // r18. inside a bindable arm the search continues, so a bad expression in
    // an arm BODY is what gets named.
    #[test]
    fn r18_arm_body_expression_is_blamed() {
        let src = "enum Color { Red, Green } \
                   fn g() -> i64 { return 1; } \
                   fn f(c: Color) -> i64 { return match c { Color::Red => g(), _ => 0 }; }";
        let reason = rejected(src, "f", &["f"]);
        assert!(reason.starts_with("the call to `g`"), "{reason}");
    }

    // r19. an arm binding is IN scope while its body is searched: blaming the
    // pattern's own variable would be the classic false positive here.
    #[test]
    fn r19_arm_binding_is_in_scope_for_the_body() {
        let src = "enum Shape { Circle(i64) } \
                   fn g() -> i64 { return 1; } \
                   fn f(s: Shape) -> i64 { return match s { Shape::Circle(r) => r + g() }; }";
        let reason = rejected(src, "f", &["f"]);
        assert!(!reason.contains("the variable `r`"), "{reason}");
        assert!(reason.starts_with("the call to `g`"), "{reason}");
    }

    // r20. with several blockers the FIRST in program order wins, so a
    // whole-corpus histogram of reasons is stable between runs.
    #[test]
    fn r20_first_blocker_in_program_order_wins() {
        let src = "fn g() -> i64 { return 1; }\n\
                   fn h() -> i64 { return 2; }\n\
                   fn f() -> i64 {\n\
                       let a = g();\n\
                       let b = h();\n\
                       return a + b;\n\
                   }";
        let reason = rejected(src, "f", &["f"]);
        assert!(reason.starts_with("the call to `g` at line 4"), "{reason}");
    }

    // r21. a method call names both the method and the receiver's type: the
    // same method name on two receivers is two different gaps.
    #[test]
    fn r21_method_call_names_method_and_receiver() {
        // Scalar `toString` joined the subset in willow-0g8j.2.5, so the shape
        // that still gets refused is the one k17 covers: an interface method
        // the backend's tables do not register a slot for.
        let src = format!("{NAMED} fn f(n: Named) -> String {{ return n.name(); }}");
        let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
        tables.iface_methods.clear();
        let reason = tables
            .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
            .expect("`f` was accepted");
        assert!(
            reason.contains("the method `name` on a `Named`"),
            "{reason}"
        );
    }

    // r22. a static property READ is named with class and field, matching the
    // way the store form (r12) reads.
    #[test]
    fn r22_static_property_read_is_named() {
        let src = "class Config { pub static version: i64 = 1; } \
                   fn f() -> i64 { return Config::version; }";
        let reason = rejected(src, "f", &["f"]);
        assert!(
            reason.contains("the static property `Config::version`"),
            "{reason}"
        );
    }

    // r23. an operator is named by its source spelling, shared with the IR
    // dumper so one operator is never described two ways.
    #[test]
    fn r23_operator_named_by_source_spelling() {
        let src = "fn f() -> i64 { let a = 0..3; let b = 1 + 2; return b; }";
        // `**` is the interesting spelling; check the shared table directly so
        // this does not depend on which operators the walker currently takes.
        assert_eq!(binop_str(&BinOp::Pow), "**");
        assert!(reason_of(src, "f", &["f"]).is_some());
    }

    // r24. `new C` on a class outside the subset is named with the class, so a
    // whole-file scan groups by the class that needs the work.
    #[test]
    fn r24_new_names_its_class() {
        // In argument position, so the `let`'s binding type is not what gets
        // blamed first.
        let src = "open class Base { pub v: i64; pub init(self, v: i64) { self.v = v; } } \
                   class Dog extends Base { pub init(self) { super.init(1); } } \
                   fn show(d: Dog) { println(d.v); } \
                   fn caller() { show(new Dog()); }";
        let reason = rejected(src, "caller", &["caller", "show"]);
        assert!(reason.starts_with("`new Dog` at line"), "{reason}");
    }

    // ── willow-0g8j.2.2: function values, lambdas and indirect calls ────────
    //
    // Three HIR shapes arrived together and only make sense together: a named
    // function used as a VALUE (`FnRef`), a lambda expression (a lifted
    // top-level function with no captured environment — the checker rejects a
    // capture outright, E1002), and a call whose callee is a local function
    // value rather than a symbol. The `f*` tests pin the eligibility boundary;
    // the OUTPUT is pinned by the `lir_diff_*` differentials in
    // tests/integration/codegen.rs.

    /// Whether the walker admits each lifted lambda in `src`, in lowering
    /// order. A lambda is a function in its own right: its body is vetted under
    /// its own symbol, not as part of whoever takes its address.
    fn lambda_eligibility(src: &str, fns: &[&str]) -> Vec<bool> {
        let (p, tables) = checked_lowering(src, fns);
        p.lambdas
            .iter()
            .map(|l| tables.with_ctx(|ctx| lir_supported_function(&l.function, ctx)))
            .collect()
    }

    // f01. the base case: a named function used as a value. Spelled as a bare
    // identifier, so what makes it a function address rather than a variable
    // read is purely what the name resolves to.
    #[test]
    fn f01_named_function_as_a_value_eligible() {
        let src = "fn double(x: i64) -> i64 { return x * 2; }
                   fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(double, 10); }";
        let fns = &["double", "apply", "caller"];
        for name in fns {
            assert!(eligible_checked(src, name, fns), "`{name}` must be in");
        }
    }

    // f02. a `fn(...)` PARAMETER is a supported type — the walker has to accept
    // the type before it can accept the call through it.
    #[test]
    fn f02_fn_typed_parameter_is_a_supported_type() {
        let src = "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }";
        assert!(eligible_checked(src, "apply", &["apply"]));
    }

    // f03. a `fn`-typed `let`, then a call through it. The binding is an
    // ordinary local slot holding a code address.
    #[test]
    fn f03_fn_typed_let_and_indirect_call() {
        let src = "fn triple(x: i64) -> i64 { return x * 3; }
                   fn caller() -> i64 { let g: fn(i64) -> i64 = triple; return g(7); }";
        assert!(eligible_checked(src, "caller", &["triple", "caller"]));
    }

    // f04. a lambda expression is a value like any other, and the lifted body
    // is a function the walker compiles on its own terms.
    #[test]
    fn f04_lambda_value_and_its_lifted_body_are_both_eligible() {
        let src = "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(|x: i64| x + 1, 10); }";
        assert!(eligible_checked(src, "caller", &["apply", "caller"]));
        assert_eq!(lambda_eligibility(src, &["apply", "caller"]), vec![true]);
    }

    // f05. the two are INDEPENDENT: a lambda whose body is outside the subset
    // costs only the lambda its LIR compilation. Taking its address is a
    // relocation, so the enclosing function does not care what it contains.
    #[test]
    fn f05_unsupported_lambda_body_does_not_sink_its_taker() {
        // The lifted body calls a symbol the backend never declared, so it
        // falls back on its own terms. (`format` used to serve as the
        // unsupported body here; it joined the subset in willow-0g8j.2.5.)
        let src = "fn helper(x: i64) -> String { return \"h\"; }
                   fn apply(f: fn(i64) -> String, v: i64) -> String { return f(v); }
                   fn caller() -> String { return apply(|x: i64| helper(x), 1); }";
        let fns = &["apply", "caller"];
        assert_eq!(lambda_eligibility(src, fns), vec![false]);
        assert!(eligible_checked(src, "caller", fns));
    }

    // f06. a lambda nested inside a lambda is lifted too — the walk goes
    // through `HirExpr::children`, innermost first, so both bodies exist as
    // functions and neither is left inline in the other's block graph.
    #[test]
    fn f06_nested_lambdas_are_both_lifted() {
        let src = "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(|x: i64| apply(|y: i64| y * 2, x), 5); }";
        let fns = &["apply", "caller"];
        assert_eq!(lambda_eligibility(src, fns), vec![true, true]);
        assert!(eligible_checked(src, "caller", fns));
    }

    // f07. the walker never takes the address of a symbol it cannot name. The
    // lambda's SYMBOL comes from the backend's span-keyed table, not from the
    // IR, so an unregistered lambda (one inside an imported module, today)
    // must refuse rather than emit an address of nothing.
    #[test]
    fn f07_unregistered_lambda_symbol_refuses() {
        let src = "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(|x: i64| x + 1, 10); }";
        let fns = &["apply", "caller"];
        let (f, mut tables) = lir_fn_and_tables(src, "caller", fns);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
        tables.lambdas.clear();
        assert!(tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)));
    }

    // f08. the same rule for a NAMED function: the address is only taken when
    // the function is one the backend declared. A name the compiler knows a
    // type for but never declared would relocate against nothing.
    #[test]
    fn f08_unknown_function_value_refuses() {
        let src = "fn double(x: i64) -> i64 { return x * 2; }
                   fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(double, 10); }";
        let fns = &["double", "apply", "caller"];
        let (f, mut tables) = lir_fn_and_tables(src, "caller", fns);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
        tables.known.remove("double");
        assert!(tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)));
    }

    // f09. a function whose parameters are not all by-value has no honest
    // function-pointer value: the AST path passes such a parameter through a
    // different ABI, so its address must not be handed to an indirect call.
    #[test]
    fn f09_by_reference_parameters_are_not_function_values() {
        let src = "fn double(x: i64) -> i64 { return x * 2; }
                   fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(double, 10); }";
        let fns = &["double", "apply", "caller"];
        let (f, mut tables) = lir_fn_and_tables(src, "caller", fns);
        tables.param_modes.insert(
            "double",
            vec![ParamMode::Reference {
                mutable: false,
                ampersand_span: crate::diagnostics::Span::dummy(),
                mut_span: None,
            }],
        );
        assert!(tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)));
    }

    // f10. the function TYPE is vetted structurally: every parameter and the
    // return must themselves be supported, and a `void` parameter — which has
    // no ABI slot — is excluded explicitly.
    #[test]
    fn f10_function_type_is_vetted_structurally() {
        let (_, tables) = checked_lowering("fn f() {}", &["f"]);
        tables.with_ctx(|ctx| {
            let ok = Type::Fn(vec![Type::I64, Type::String], Box::new(Type::Bool));
            assert!(ctx.supported_type(&ok));
            // `void` in a parameter position has no slot to pass.
            let void_param = Type::Fn(vec![Type::Void], Box::new(Type::I64));
            assert!(!ctx.supported_type(&void_param));
            // a `void` RETURN is fine — that is an ordinary statement call.
            let void_ret = Type::Fn(vec![Type::I64], Box::new(Type::Void));
            assert!(ctx.supported_type(&void_ret));
            // an unsupported component sinks the whole type.
            let bad = Type::Fn(
                vec![Type::Named("Missing".to_string())],
                Box::new(Type::I64),
            );
            assert!(!ctx.supported_type(&bad));
        });
    }

    // f11. the value's type must be the one the walker would emit. `fn_value_of`
    // answers from the registered signature; if the expression's recorded type
    // disagrees, the address would be called through the wrong signature.
    #[test]
    fn f11_function_value_type_must_match_the_registration() {
        let src = "fn double(x: i64) -> i64 { return x * 2; }
                   fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   fn caller() -> i64 { return apply(double, 10); }";
        let fns = &["double", "apply", "caller"];
        let (f, mut tables) = lir_fn_and_tables(src, "caller", fns);
        tables
            .fn_types
            .insert("double", Type::Fn(vec![Type::String], Box::new(Type::I64)));
        assert!(tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)));
    }

    // f12. a local function value SHADOWS a top-level function of the same
    // name, and eligibility resolves the callee the same way the emitter does —
    // local first. Getting this order wrong would type-check one callee and
    // call the other.
    #[test]
    fn f12_local_function_value_shadows_a_top_level_name() {
        let src = "fn weigh(n: i64) -> i64 { return n * 3; }
                   fn caller() -> i64 { let weigh: fn(i64) -> i64 = |n: i64| n; return weigh(2); }";
        let fns = &["weigh", "caller"];
        assert!(eligible_checked(src, "caller", fns));
    }

    // f13. a `void`-returning function value called in statement position: the
    // indirect call has no result to merge, which is a different signature and
    // a different emission path from the valued one.
    #[test]
    fn f13_void_returning_function_value() {
        let src = "fn shout(n: i64) { println(n); }
                   fn run(f: fn(i64) -> void) { f(1); }
                   fn caller() { run(shout); }";
        let fns = &["shout", "run", "caller"];
        for name in fns {
            assert!(eligible_checked(src, name, fns), "`{name}` must be in");
        }
    }

    // f14. an indirect call's ARGUMENTS are vetted against the function type's
    // parameters, not against a symbol's signature — there is no symbol.
    #[test]
    fn f14_indirect_call_arguments_are_vetted_against_the_fn_type() {
        let src = "fn join(a: String, b: i64) -> String { return a + b.toString(); }
                   fn caller() -> String {
                       let g: fn(String, i64) -> String = join;
                       return g(\"n=\", 2);
                   }";
        assert!(eligible_checked(src, "caller", &["join", "caller"]));
    }

    // f15. an indirect call's RESULT must have the expression's type. Source
    // cannot produce a mismatch, so the recorded type is perturbed directly —
    // the state a desugaring bug would leave behind.
    #[test]
    fn f15_indirect_call_result_type_is_checked() {
        let src = "fn caller(g: fn(i64) -> i64) -> i64 { return g(7); }";
        let (mut f, tables) = lir_fn_and_tables(src, "caller", &["caller"]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
        let Terminator::Return(Some(call)) = &mut f.blocks[0].terminator else {
            panic!(
                "expected a returned indirect call, got {:?}",
                f.blocks[0].terminator
            );
        };
        call.ty = Type::String;
        assert!(tables.with_ctx(|ctx| !lir_supported_function(&f, ctx)));
    }

    // f16. function values flow through the ordinary type positions: a
    // parameter, a `let`, a return type, and an array element.
    #[test]
    fn f16_function_values_in_every_type_position() {
        let src = "import std::collections::Array;
                   fn double(x: i64) -> i64 { return x * 2; }
                   fn pick() -> fn(i64) -> i64 { return double; }
                   fn table() -> i64 {
                       let fs: Array<fn(i64) -> i64> = [double];
                       let g = fs[0];
                       return g(4);
                   }";
        let fns = &["double", "pick", "table"];
        for name in fns {
            assert!(eligible_checked(src, name, fns), "`{name}` must be in");
        }
    }

    // f17. a GC-managed payload crossing an indirect call: the argument is
    // rooted before the call exactly as a direct call's is, so a collection
    // inside the callee cannot lose it.
    #[test]
    fn f17_gc_managed_arguments_cross_an_indirect_call() {
        let src = "fn shout(s: String) -> String { return s + \"!\"; }
                   fn run(f: fn(String) -> String, s: String) -> String { return f(s); }
                   fn caller() -> String { return run(shout, \"hi\"); }";
        let fns = &["shout", "run", "caller"];
        for name in fns {
            assert!(eligible_checked(src, name, fns), "`{name}` must be in");
        }
    }

    // f18. a lambda inside a CLASS METHOD is lifted like any other — the
    // collection walks methods, not only free functions.
    #[test]
    fn f18_lambda_inside_a_class_method_is_lifted() {
        let src = "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); }
                   class Box2 {
                       pub v: i64;
                       pub fn scaled(self) -> i64 { return apply(|x: i64| x * 2, self.v); }
                   }";
        assert_eq!(lambda_eligibility(src, &["apply"]), vec![true]);
    }

    // f19. `Option::map` — the first combinator that CALLS its operand. The
    // result is `Option<U>` for the callable's return `U`, which is what
    // decides the new value's representation.
    #[test]
    fn f19_option_map_result_type_follows_the_callable() {
        let opt_i64 = Type::Generic("Option".to_string(), vec![Type::I64]);
        let to_string = Type::Fn(vec![Type::I64], Box::new(Type::String));
        assert_eq!(
            option_result_method(&opt_i64, "map", &[to_string]),
            Some(Type::Generic("Option".to_string(), vec![Type::String]))
        );
        // a `void`-returning callable would build a `Some` with no payload slot
        let to_void = Type::Fn(vec![Type::I64], Box::new(Type::Void));
        assert_eq!(option_result_method(&opt_i64, "map", &[to_void]), None);
        // the callable's parameter must be the payload type
        let wrong = Type::Fn(vec![Type::String], Box::new(Type::I64));
        assert_eq!(option_result_method(&opt_i64, "map", &[wrong]), None);
    }

    // f20. `and_then` and `or_else` MERGE one arm's receiver with the other
    // arm's callable result, so the payload types have to line up — but the
    // error type of a `Result` deliberately does not: a lambda ending in
    // `Result::Ok(0)` records `Result<i64, void>`, and every `Result` is the
    // same two-word box whatever its type arguments.
    #[test]
    fn f20_and_then_and_or_else_merge_rules() {
        let opt_i64 = Type::Generic("Option".to_string(), vec![Type::I64]);
        let opt_str = Type::Generic("Option".to_string(), vec![Type::String]);
        let to_opt_str = Type::Fn(vec![Type::I64], Box::new(opt_str.clone()));
        assert_eq!(
            option_result_method(&opt_i64, "and_then", &[to_opt_str]),
            Some(opt_str.clone())
        );
        // `or_else` takes NO argument and must produce the receiver's payload
        let same = Type::Fn(vec![], Box::new(opt_i64.clone()));
        assert_eq!(
            option_result_method(&opt_i64, "or_else", &[same]),
            Some(opt_i64.clone())
        );
        let other = Type::Fn(vec![], Box::new(opt_str));
        assert_eq!(option_result_method(&opt_i64, "or_else", &[other]), None);

        let res = Type::Generic("Result".to_string(), vec![Type::I64, Type::String]);
        let unresolved_err = Type::Generic("Result".to_string(), vec![Type::I64, Type::Void]);
        let recover = Type::Fn(vec![Type::String], Box::new(unresolved_err.clone()));
        assert_eq!(
            option_result_method(&res, "or_else", &[recover]),
            Some(unresolved_err)
        );
        // the OK payload still has to match — that arm passes the receiver on
        let mismatched = Type::Fn(
            vec![Type::String],
            Box::new(Type::Generic(
                "Result".to_string(),
                vec![Type::String, Type::String],
            )),
        );
        assert_eq!(option_result_method(&res, "or_else", &[mismatched]), None);
    }

    // f21. `map_err` is a `Result`-only combinator, and it rebuilds the ERROR
    // side while passing the ok payload through.
    #[test]
    fn f21_map_err_is_result_only() {
        let res = Type::Generic("Result".to_string(), vec![Type::I64, Type::String]);
        let wrap = Type::Fn(vec![Type::String], Box::new(Type::String));
        assert_eq!(
            option_result_method(&res, "map_err", std::slice::from_ref(&wrap)),
            Some(res.clone())
        );
        let opt = Type::Generic("Option".to_string(), vec![Type::I64]);
        assert_eq!(option_result_method(&opt, "map_err", &[wrap]), None);
    }

    // f22. a `void` payload has no slot for a combinator to read or write, so
    // `Result<void, E>` is excluded from all of them — the same rule the
    // unwrap family already followed.
    #[test]
    fn f22_void_payloads_are_excluded_from_combinators() {
        let res_void = Type::Generic("Result".to_string(), vec![Type::Void, Type::String]);
        let f = Type::Fn(vec![Type::Void], Box::new(Type::I64));
        for method in ["map", "and_then"] {
            assert_eq!(
                option_result_method(&res_void, method, std::slice::from_ref(&f)),
                None,
                "`{method}` must not claim a void payload"
            );
        }
    }

    // f23. the combinators are still reachable end to end from source, with a
    // lambda operand and with a named function value — the two spellings the
    // emitter has to accept.
    #[test]
    fn f23_combinators_from_source_with_both_operand_spellings() {
        let src = "fn twice(v: i64) -> i64 { return v * 2; }
                   fn with_lambda(x: Option<i64>) -> i64 { return x.map(|v: i64| v * 2).unwrap_or(0); }
                   fn with_fn_value(x: Option<i64>) -> i64 { return x.map(twice).unwrap_or(0); }";
        let fns = &["twice", "with_lambda", "with_fn_value"];
        for name in fns {
            assert!(eligible_checked(src, name, fns), "`{name}` must be in");
        }
    }

    // f24. recursion THROUGH a function value: the callee is a parameter, so
    // the call graph is not statically known and the panic-depth protocol has
    // to stay conservative. Eligibility must still admit it.
    #[test]
    fn f24_recursion_through_a_function_value() {
        let src = "fn step(n: i64) -> i64 { return n - 1; }
                   fn walk(f: fn(i64) -> i64, n: i64) -> i64 {
                       if n <= 0 { return 0; }
                       return 1 + walk(f, f(n));
                   }
                   fn caller() -> i64 { return walk(step, 3); }";
        let fns = &["step", "walk", "caller"];
        for name in fns {
            assert!(eligible_checked(src, name, fns), "`{name}` must be in");
        }
    }

    // ---------------------------------------------------------------------
    // d. divergence, scalar `toString` and `format` (willow-0g8j.2.5).
    //
    // Divergence is a property of the POSITION, not of the expression: a
    // `!`-typed expression ends its Cranelift block with a terminator, so the
    // walker admits one only where nothing follows it in the same block — as a
    // whole statement, or as the tail of a `match` arm. Everything else here
    // is the string machinery those panics need to build their messages.
    // ---------------------------------------------------------------------

    // d01. all four scalar receivers convert. `toString` is the only builtin
    // the walker resolves by intrinsic rather than by name, so each arm of that
    // table needs its own witness.
    #[test]
    fn d01_every_scalar_to_string_is_eligible() {
        let cases = [
            ("i64", "n: i64", "n"),
            ("f64", "n: f64", "n"),
            ("bool", "n: bool", "n"),
            ("String", "n: String", "n"),
        ];
        for (label, param, recv) in cases {
            let src = format!("fn f({param}) -> String {{ return {recv}.toString(); }}");
            assert!(eligible_checked(&src, "f", &["f"]), "{label} must convert");
        }
    }

    // d02. `String::toString` is the identity, not a no-op the resolver drops:
    // it must still resolve, and to `String`.
    #[test]
    fn d02_string_to_string_is_the_identity() {
        assert_eq!(
            scalar_to_string(&Type::String, "toString", &[]),
            Some(Type::String)
        );
    }

    // d03. arity is part of the match. A one-argument `toString` is not the
    // intrinsic, and admitting it would emit a call with a stranded operand.
    #[test]
    fn d03_scalar_to_string_is_arity_checked() {
        let arg = HirExpr {
            kind: HirExprKind::Int(1),
            ty: Type::I64,
            span: crate::diagnostics::Span::dummy(),
        };
        assert_eq!(
            scalar_to_string(&Type::I64, "toString", std::slice::from_ref(&arg)),
            None
        );
    }

    // d04. a non-scalar receiver never reaches the scalar table — collections
    // and class receivers have their own lowerings, and silently borrowing this
    // one would emit the wrong runtime symbol.
    #[test]
    fn d04_non_scalar_receivers_are_not_scalar_to_string() {
        for recv in [
            Type::Array(Box::new(Type::I64)),
            Type::Named("Widget".to_string()),
            Type::Void,
        ] {
            assert_eq!(scalar_to_string(&recv, "toString", &[]), None, "{recv:?}");
        }
    }

    // d05. `format` renders exactly the four scalar operand types, mixed
    // freely with literal text.
    #[test]
    fn d05_format_renders_every_scalar_operand() {
        let src = "fn f(a: i64, b: f64, c: bool, d: String) -> String {
                       return format(\"{} {} {} {}\", a, b, c, d);
                   }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // d06. a precision placeholder passes its operand straight to an f64
    // formatting symbol, so an i64 there would reinterpret the bits.
    #[test]
    fn d06_precision_placeholder_demands_f64() {
        let ok = "fn f(x: f64) -> String { return format(\"{:.6f}\", x); }";
        assert!(eligible_checked(ok, "f", &["f"]));

        let span = crate::diagnostics::Span::dummy();
        let spec = HirExpr {
            kind: HirExprKind::Str("{:.6f}".to_string()),
            ty: Type::String,
            span,
        };
        let int_operand = HirExpr {
            kind: HirExprKind::Int(1),
            ty: Type::I64,
            span,
        };
        assert_eq!(format_operands(&[spec, int_operand]), None);
    }

    // d07. placeholder count and operand count must agree in BOTH directions:
    // too few operands reads past the argument list, too many silently drops
    // an evaluated value.
    #[test]
    fn d07_format_arity_must_match_in_both_directions() {
        let span = crate::diagnostics::Span::dummy();
        let operand = |ty: Type| HirExpr {
            kind: HirExprKind::Int(1),
            ty,
            span,
        };
        let spec = |text: &str| HirExpr {
            kind: HirExprKind::Str(text.to_string()),
            ty: Type::String,
            span,
        };
        assert_eq!(format_operands(&[spec("{} {}"), operand(Type::I64)]), None);
        assert_eq!(
            format_operands(&[spec("{}"), operand(Type::I64), operand(Type::I64)]),
            None
        );
        assert_eq!(
            format_operands(&[spec("{}"), operand(Type::I64)]).map(<[HirExpr]>::len),
            Some(1)
        );
    }

    // d08. `{{` and `}}` are literal braces, not placeholders. Counting them
    // as placeholders would make a correct call look arity-mismatched.
    #[test]
    fn d08_escaped_braces_are_literals_not_placeholders() {
        let span = crate::diagnostics::Span::dummy();
        let spec = HirExpr {
            kind: HirExprKind::Str("{{literal}} {}".to_string()),
            ty: Type::String,
            span,
        };
        let operand = HirExpr {
            kind: HirExprKind::Int(9),
            ty: Type::I64,
            span,
        };
        assert_eq!(
            format_operands(&[spec, operand]).map(<[HirExpr]>::len),
            Some(1)
        );
    }

    // d09. the spec must be a literal. A computed spec cannot be parsed at
    // compile time, so the walker cannot know what to emit.
    #[test]
    fn d09_format_spec_must_be_a_literal() {
        let span = crate::diagnostics::Span::dummy();
        let computed = HirExpr {
            kind: HirExprKind::Var("s".to_string()),
            ty: Type::String,
            span,
        };
        assert_eq!(format_operands(std::slice::from_ref(&computed)), None);
    }

    // d10. an operand the runtime has no renderer for is refused even though
    // the arity agrees — the check is per operand, not just a count.
    #[test]
    fn d10_unrenderable_format_operand_is_refused() {
        let span = crate::diagnostics::Span::dummy();
        let spec = HirExpr {
            kind: HirExprKind::Str("{}".to_string()),
            ty: Type::String,
            span,
        };
        let array = HirExpr {
            kind: HirExprKind::Var("a".to_string()),
            ty: Type::Array(Box::new(Type::I64)),
            span,
        };
        assert_eq!(format_operands(&[spec, array]), None);
    }

    // d11. the base case: `panic(...)` as a whole statement. Nothing follows
    // it in its block, so the terminator it emits is safe.
    #[test]
    fn d11_statement_panic_is_eligible() {
        let src = "fn f(n: i64) -> i64 {
                       if n < 0 { panic(\"negative\"); }
                       return n;
                   }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // d12. the formatted form goes through the same operand vetting as
    // `format`, so its message can interpolate.
    #[test]
    fn d12_formatted_panic_is_eligible() {
        let src = "fn f(a: i64, b: i64) -> i64 {
                       if b == 0 { panic(\"cannot divide {} by {}\", a, b); }
                       return a / b;
                   }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // d13. the one-argument form takes an already-built `String`, so the
    // message may be any expression the walker can emit — including one that
    // allocates.
    #[test]
    fn d13_single_argument_panic_takes_a_computed_message() {
        let src = "fn f(n: i64) -> i64 {
                       if n <= 0 { panic(\"bad value: \" + n.toString()); }
                       return n;
                   }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // d14. THE position rule. In an operand position the panic's terminator
    // would strand the call that consumes its value, so the whole function
    // falls back — and the reason names the type, not the callee, because the
    // problem is the `!` and not `panic` itself.
    #[test]
    fn d14_operand_position_panic_is_refused() {
        let src = "fn f() -> i64 { println(panic(\"no\")); return 1; }";
        let reason = rejected(src, "f", &["f"]);
        assert!(
            reason.contains("`panic`") && reason.contains("has type `!`"),
            "{reason}"
        );
    }

    // d15. a local binding named `panic` is an ordinary indirect call through
    // a function value, not the builtin. Treating it as the builtin would emit
    // an unwind where the program expects a call.
    #[test]
    fn d15_a_local_named_panic_is_not_the_builtin() {
        let span = crate::diagnostics::Span::dummy();
        let call = HirExpr {
            kind: HirExprKind::Call {
                callee: "panic".to_string(),
                args: Vec::new(),
            },
            ty: Type::Never,
            span,
        };
        let tables = empty_tables();
        let local = Type::Fn(Vec::new(), Box::new(Type::Never));
        tables.with_ctx(|ctx| {
            assert!(supported_panic(&call, ctx, &HashMap::new()));
            let names: HashMap<&str, &Type> = HashMap::from([("panic", &local)]);
            assert!(!supported_panic(&call, ctx, &names));
        });
    }

    // d16. a `match` arm may end in a panic while its siblings produce values.
    // The arm is the tail of its own block, so the position rule holds.
    #[test]
    fn d16_a_panicking_arm_beside_value_arms() {
        let src = "fn f(n: i64) -> String {
                       return match n {
                           1 => \"low\",
                           2 => \"high\",
                           _ => panic(\"no level {}\", n),
                       };
                   }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // d17. when EVERY arm leaves, the `match` itself is typed `!` and has no
    // reachable merge. The emitter must not read the result variable there.
    #[test]
    fn d17_all_arms_return_is_eligible_as_a_statement() {
        let src = "fn f(n: i64) -> String {
                       match n {
                           0 => return \"zero\",
                           1 => return \"one\",
                           _ => return \"many\",
                       }
                   }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // d18. a diverging arm may run effect statements first — they precede the
    // terminator in the same block, which is legal.
    #[test]
    fn d18_a_diverging_arm_may_run_effects_first() {
        let src = "fn f(n: i64) -> String {
                       match n {
                           0 => { println(\"zero\"); return \"z\"; }
                           _ => { println(\"other\"); println(n); return \"o\"; }
                       }
                   }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // d19. but a `let` may not: the flat `vars` map has no way to scope a
    // binding to one arm, so the walker declines rather than leaking it.
    #[test]
    fn d19_a_let_in_a_diverging_arm_is_refused() {
        let src = "fn f(n: i64) -> i64 {
                       match n {
                           0 => { let t = 1; return t; }
                           _ => return 2,
                       }
                   }";
        assert!(reason_of(src, "f", &["f"]).is_some());
    }

    // d20. an arm's `return` value is checked against the FUNCTION's declared
    // return type, which is why `LirTypeCtx` carries it. Boxing a class into
    // the declared interface needs a vtable; without one the arm is refused
    // instead of returning a raw pointer.
    #[test]
    fn d20_arm_returns_are_checked_against_the_declared_return_type() {
        let src = format!(
            "{NAMED} fn f(b: bool) -> Named {{
                 match b {{
                     true => return new Item(\"x\"),
                     _ => return new Item(\"y\"),
                 }}
             }}"
        );
        let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
        assert!(
            tables
                .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
                .is_none()
        );

        tables.vtables.clear();
        assert!(
            tables
                .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
                .is_some(),
            "without a vtable the arm cannot box its return value"
        );
    }

    // d21. the position rule again, one level up: a `!`-typed `match` is
    // admitted as a statement but never as an operand.
    #[test]
    fn d21_a_never_typed_match_is_statement_only() {
        let src = "fn f(n: i64) -> i64 {
                       match n {
                           0 => return 1,
                           _ => return 2,
                       }
                   }";
        let (p, tables) = checked_lowering(src, &["f"]);
        let f = p.functions.iter().find(|f| f.name == "f").expect("lowered");
        tables.with_ctx(|ctx| {
            let ctx = &LirTypeCtx {
                return_type: &f.return_type,
                ..*ctx
            };
            let inst = f
                .blocks
                .iter()
                .flat_map(|b| b.instrs.iter())
                .find_map(|i| match i {
                    LirInst::Expr(e) if matches!(e.kind, HirExprKind::Match { .. }) => Some(e),
                    _ => None,
                })
                .expect("the match survives as a statement");
            assert_eq!(inst.ty, Type::Never);
            let i64_ty = Type::I64;
            let names: HashMap<&str, &Type> = HashMap::from([("n", &i64_ty)]);
            assert!(supported_divergent_expr(inst, ctx, &names));
            assert!(!supported_expr(inst, ctx, &names));
        });
    }

    // d22. divergence nests: an arm whose tail is itself an all-returning
    // `match` still ends its block exactly once.
    #[test]
    fn d22_diverging_arms_nest() {
        let src = "fn f(row: i64, col: i64) -> String {
                       match row {
                           0 => match col {
                               0 => return \"origin\",
                               _ => return \"top\",
                           },
                           _ => return \"body\",
                       }
                   }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // d23. the reason finder walks the same subset. An admitted diverging arm
    // must not be blamed for a rejection that lives elsewhere, or the
    // diagnostic points at working code.
    #[test]
    fn d23_the_reason_does_not_blame_an_admitted_diverging_arm() {
        let src = "class Config { pub static version: i64 = 7; }
                   fn f(n: i64) -> i64 {
                       match n {
                           0 => return 1,
                           _ => return Config::version,
                       }
                   }";
        let reason = rejected(src, "f", &["f"]);
        assert!(
            reason.contains("version"),
            "the reason must name the static property, got: {reason}"
        );
    }

    // d24. an arm-less `match` has no arm to jump to and no value to merge, so
    // it is refused before any arm-shaped reasoning runs.
    #[test]
    fn d24_an_empty_match_is_refused() {
        let span = crate::diagnostics::Span::dummy();
        let scrutinee = HirExpr {
            kind: HirExprKind::Var("n".to_string()),
            ty: Type::I64,
            span,
        };
        let empty = HirExpr {
            kind: HirExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms: Vec::new(),
            },
            ty: Type::I64,
            span,
        };
        let tables = empty_tables();
        let i64_ty = Type::I64;
        tables.with_ctx(|ctx| {
            let names: HashMap<&str, &Type> = HashMap::from([("n", &i64_ty)]);
            assert!(!supported_expr(&empty, ctx, &names));
        });
    }

    // d25. a VALUE-producing `match` whose arms all leave would reach its
    // merge with no predecessor, so `use_var` there has no reaching
    // definition. Such a shape is refused rather than emitted.
    #[test]
    fn d25_a_value_match_with_only_diverging_arms_is_refused() {
        let span = crate::diagnostics::Span::dummy();
        let arm = |value: i64| HirMatchArm {
            pattern: HirPattern::LiteralInt(value),
            body: vec![HirStmt::Return {
                value: Some(HirExpr {
                    kind: HirExprKind::Int(value),
                    ty: Type::I64,
                    span,
                }),
                span,
            }],
            ty: Type::Never,
            span,
        };
        let mut arms = vec![arm(0)];
        arms.push(HirMatchArm {
            pattern: HirPattern::Wildcard,
            ..arm(1)
        });
        let scrutinee = HirExpr {
            kind: HirExprKind::Var("n".to_string()),
            ty: Type::I64,
            span,
        };
        let as_value = HirExpr {
            kind: HirExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            // typed as a VALUE even though nothing can flow to the merge
            ty: Type::I64,
            span,
        };
        let mut tables = empty_tables();
        tables.ret = Type::I64;
        let i64_ty = Type::I64;
        tables.with_ctx(|ctx| {
            let names: HashMap<&str, &Type> = HashMap::from([("n", &i64_ty)]);
            assert!(!supported_expr(&as_value, ctx, &names));
        });
    }

    // d26. `arm_diverges` is deliberately SHALLOW — it reads the arm's last
    // statement only. A `return` in the middle of an arm is not a tail, and an
    // arm ending in an ordinary value does not leave.
    #[test]
    fn d26_arm_divergence_is_decided_by_the_tail_statement() {
        let span = crate::diagnostics::Span::dummy();
        let value = HirExpr {
            kind: HirExprKind::Int(1),
            ty: Type::I64,
            span,
        };
        let never = HirExpr {
            kind: HirExprKind::Call {
                callee: "panic".to_string(),
                args: Vec::new(),
            },
            ty: Type::Never,
            span,
        };
        let arm = |body: Vec<HirStmt>| HirMatchArm {
            pattern: HirPattern::Wildcard,
            body,
            ty: Type::Never,
            span,
        };
        assert!(arm_diverges(&arm(vec![HirStmt::Return {
            value: Some(value.clone()),
            span,
        }])));
        assert!(arm_diverges(&arm(vec![HirStmt::Expr(never.clone())])));
        assert!(!arm_diverges(&arm(vec![HirStmt::Expr(value.clone())])));
        assert!(!arm_diverges(&arm(Vec::new())));
        // a `return` that is not the tail does not make the ARM diverge
        assert!(!arm_diverges(&arm(vec![
            HirStmt::Return {
                value: Some(value.clone()),
                span,
            },
            HirStmt::Expr(value),
        ])));
    }

    // d27. the return type in `LirTypeCtx` is per FUNCTION, not per program:
    // `lir_rejection_reason` rebinds it from the function it was handed, so
    // two functions in one module are each checked against their own.
    #[test]
    fn d27_the_context_return_type_is_rebound_per_function() {
        let src = "fn as_int(n: i64) -> i64 { match n { 0 => return 1, _ => return 2, } }
                   fn as_text(n: i64) -> String {
                       match n { 0 => return \"a\", _ => return \"b\", }
                   }";
        let fns = &["as_int", "as_text"];
        for name in fns {
            assert!(eligible_checked(src, name, fns), "`{name}` must be in");
        }
    }
}
