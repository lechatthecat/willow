//! Cranelift code generation from operand-only LIR.
//!
//! User bodies, lifted lambdas, and cleanup regions are basic-block graphs.
//! Instructions consume local IDs or immediate values; the backend never
//! evaluates a HIR expression tree. Lowering owns ternary, match, short-circuit,
//! propagation, select, and nested-await control flow.
//!
//! Every local is bound at function entry, independently of block emission
//! order. GC values use rooted storage; async values live in the LIR-computed
//! frame when they survive suspension. ClearScopeRoots ends temporary lifetimes
//! without growing the root stack on loop iterations.
//!
//! Reference operands capture their place before later arguments run. Array
//! references retain the original buffer in an opaque GC-owner local, so resize,
//! collection, and suspension cannot redirect the reference to a new buffer.
//! The address is materialized from the rooted owner at the call.
//!
//! Calls and stores validate signatures and layouts before emission. Explicit
//! coercions preserve argument evaluation order, and heap reference writes use
//! the GC barrier. Option representation follows the shared ABI metadata.
//!
//! Suspension terminators implement the cooperative poll protocol. Prepared
//! method frames and reference diagnostic scopes follow CFG edges and are
//! restored on resume. Defer scopes and recovery edges likewise come from LIR;
//! cleanup graphs are replayed with their captured locals and fresh region state.

use super::type_index::TypeMap;
use super::{FlatReferenceDebug, ModuleSymbols};
use crate::semantic::ids::SemanticType as Type;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::{
    AbiParam, InstBuilder, MemFlagsData, StackSlotData, StackSlotKind, condcodes::FloatCC,
    condcodes::IntCC, types,
};
use cranelift_module::Module;

use crate::diagnostics::span::Span;
#[cfg(test)]
use crate::ir::dump::binop_str;
use crate::ir::lowered::{
    BlockId, LirBlock, LirFunction, LirInst, LirLocalId, LirLockSlots, LirSelectOp,
    LirSelectWaitOp, SuspendOp, Terminator,
};
#[cfg(test)]
use crate::ir::typed_ast::{
    HirCapture, HirExpr, HirExprKind, HirMatchArm, HirPattern, HirSelectCase, HirSelectCaseKind,
    HirStmt,
};
use crate::parser::ast::{BinOp, ExprId, ParamMode, UnaryOp};
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::ids::{FunctionId, FunctionMap, TypeId};
#[cfg(test)]
use crate::semantic::intrinsics;
use crate::semantic::intrinsics::Intrinsic;
#[cfg(test)]
use crate::semantic::type_checker::types::await_output_type;
use crate::semantic::type_checker::types::type_name;

use super::emit_interface::{class_base_ids, collection_elem_kind, is_self_or_descendant};
use super::gc_codegen::{GcLayoutMetadata, GcObjectKind, GcStoreDestination};
use super::option_repr::{OptionRepr, option_repr};
use super::symbols::{class_method_symbol_name, class_name_for_object_type, module_item_symbol};
use super::type_helpers::{
    builtin_call_runtime_name, clif_type, gc_stat_builtin_runtime_name, is_gc_managed,
    reference_type,
};
use super::{
    CoopSuspendPoints, FRAME_SLOT_TASK_ID, FuncGen, VarStorage, array_element_type,
    async_frame_slot_offset, channel_runtime_suffix,
};

/// Where a defer scope is opened: the LIR block and the position of its
/// `EnterDeferScope` in it. A scope is opened by exactly one instruction, so
/// this identifies it across the paths that reach it (willow-0g8j.2.15).
type LirScopeId = (usize, BlockPos);

/// Position of an instruction inside its block.
type BlockPos = usize;

/// One LIR-owned lexical defer scope, held open while the blocks it spans are
/// emitted and finished when the whole body is.
#[derive(Clone)]
struct LirDeferScopeFrame {
    /// Identity, so the same scope reached along two LIR edges compares equal.
    id: LirScopeId,
    scope: super::PanicScope,
    /// Where normal emission continues after `LeaveDeferScope`. This is the
    /// panic scope's resume block for ordinary synchronous scopes. A
    /// recovery-capable synchronous scope instead points `scope.resume` at its
    /// explicit LIR continuation, while this remains a private block in which
    /// emission can finish the current LIR block.
    normal_resume: cranelift_codegen::ir::Block,
    /// The registrations this scope owns. A synchronous `FlushDefers` names the
    /// sites an exit runs, and these are what say which OPEN scopes those sites
    /// belong to — the depth the flush starts from (willow-0g8j.2.15).
    sites: Vec<crate::ir::lowered::LirDeferId>,
    /// The compile-time GC root count at scope entry, restored on the way out.
    roots_before: usize,
    /// Synchronous defer flags shadowed by this scope.
    saved_flags: HashMap<Span, cranelift_codegen::ir::StackSlot>,
    /// Whether this scope is a `lock` body and therefore pushed the critical
    /// section its panic cleanup releases (willow-0g8j.2.13).
    owns_lock: bool,
}

/// The synchronous emitter's defer state at one point in the block walk.
///
/// LIR blocks are emitted in index order, which is NOT the order control
/// actually flows through them: a loop latch is emitted before the body that
/// jumps to it, and a block after a `return` may open with a completely
/// different set of scopes. So the state is snapshotted per LIR block and
/// restored on entry instead of being carried linearly (willow-0g8j.2.15).
#[derive(Clone, Default)]
struct LirDeferState {
    scopes: Vec<LirDeferScopeFrame>,
    entries: Vec<Vec<super::DeferEntry>>,
    panic_scopes: Vec<super::PanicScope>,
    flags: HashMap<Span, cranelift_codegen::ir::StackSlot>,
}

/// Scopes a synchronous function opened, so the ones no `LeaveDeferScope` ever
/// closes still get their panic cleanup emitted once at the end of the body.
#[derive(Default)]
struct LirDeferLedger {
    /// Every scope in the order its `EnterDeferScope` was emitted.
    opened: Vec<LirDeferScopeFrame>,
    /// Scopes a `LeaveDeferScope` already finished.
    closed: HashSet<LirScopeId>,
    /// The emitter state a flush-only exit left a scope in. Its panic cleanup
    /// re-runs the scope's registrations, so it needs the `defer_stack` shape
    /// that was live at that point, not whatever the last block happened to
    /// leave behind.
    dropped: HashMap<LirScopeId, LirDeferState>,
    /// Panic-cleanup blocks awaiting their seal. A scope's cleanup jumps to its
    /// PARENT's cleanup, and block order can finish an inner scope after the
    /// outer one, so sealing at the point a scope is finished would seal a
    /// block a later child still has to name (willow-0g8j.2.15).
    cleanups: Vec<cranelift_codegen::ir::Block>,
}

/// Mutable synchronous-defer context shared while one LIR block is emitted.
/// Keeping these coupled avoids passing three pieces of the same state through
/// every block-emission call independently.
struct LirBlockDeferCtx<'a> {
    block_index: usize,
    scopes: &'a mut Vec<LirDeferScopeFrame>,
    ledger: &'a mut LirDeferLedger,
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
/// The subset also holds generics that are NOT GC heap objects — the opaque
/// runtime-pointer ones (`Future<T>`, `BlockingCell<T>`, …), which this function
/// would answer `true` for. It never sees them either: [`assignable_repr`]
/// compares two generics by equality and rejects a generic against anything
/// else, so no `Type::Generic` reaches its catch-all arm at all.
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

/// `Range<i64>` — the only range shape either backend models: a two-word GC
/// object `[start, end]`, both `i64`, with no reference slots
/// (willow-0g8j.2.10).
///
/// Recognised by name: `Range` is not a [`BuiltinTypeId`]. Eligibility and
/// field emission share this predicate to agree on the two-word layout.
///
/// [`BuiltinTypeId`]: crate::semantic::builtin_types::BuiltinTypeId
fn range_i64(ty: &Type) -> bool {
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
/// agreed to. That is what lets `parallel::map` join them (willow-0g8j.2.13):
/// its shape — a frozen array and a function value in, a `Task` out — is the
/// schema's, and the function value needs no special handling because a Willow
/// lambda captures nothing and so IS an address.
///
/// `f64::to_string` and `f64::parse` join them because they ARE them — two
/// fixed-signature runtime calls spelled with a `::` (willow-0g8j.2.13). They
/// are not in the stdlib schema (no `import` reaches them), so their shapes
/// come from the same table [`static_call_return_type`] types them from, and
/// they are answered BEFORE the user-module gate because that is where
/// `emit_static_method_call` answers them: a module imported under the name
/// `f64` does not displace either one.
fn namespace_builtin_call(
    known_modules: &ModuleSymbols,
    builtin_module_aliases: &HashMap<String, String>,
    class: &str,
    method: &str,
) -> Option<crate::semantic::intrinsics::NamespaceBuiltin> {
    if class == "f64" {
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

/// Whether `e` is literally `Map::new()`, the only expression that may carry the
/// untyped [`empty_map_type`].
///
/// This is what keeps the type-level exemption honest. `supported_type` REJECTS
/// `Map<Void, Void>`, so it can never be a parameter, a `let`'s declared type or
/// a return type; the only way a value of that type can exist in an eligible
/// function is through this node, which is admitted here and nowhere else.
#[cfg(test)]
fn is_fresh_empty_map(e: &HirExpr) -> bool {
    matches!(&e.kind, HirExprKind::StaticCall { class, method, args }
        if class == &TypeId::local("Map") && method == "new" && args.is_empty())
        && empty_map_type(&e.ty)
}

/// Whether `ty` can be a map key the walker emits.
///
/// The runtime's key is `Int(i64) | Str(String)` and it picks between them from
/// the is-ref flag the backend passes. `String` is the only REFERENCE key it can
/// read — anything else GC-managed would be handed to it as a `WillowString`
/// pointer and read as one. Every scalar is fine: `coerce_to_i64` widens a
/// `bool` and bitcasts an `f64`, so each arrives as the one word `Int` holds
/// verbatim, and `map_to_string` is told the key's kind so it renders back into
/// the right one.
///
/// `f64` keys therefore match bit-for-bit: `0.0` and `-0.0` are distinct keys,
/// and a `NaN` key matches only a `NaN` with the same payload. That is the
/// runtime representation, not an additional eligibility restriction.
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
            clif_type(types::I64, target) == clif_type(types::I64, value)
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
    /// POINTER in the dispatch ABI, so eligibility pairs each mode with the
    /// shape of the argument in that position and admits the call only when
    /// they agree — the same rule [`LirTypeCtx::callable`] applies to direct
    /// calls (willow-0g8j.9, willow-0g8j.2.17). Nothing else can tell a
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
pub(super) struct LirEnumVariant {
    pub name: String,
    pub payloads: Vec<Type>,
}

/// An enum declaration as eligibility sees it (willow-0g8j.8).
#[derive(Clone)]
pub(super) struct LirEnumDef {
    /// The enum's build-wide identity: the one name it answers to however this
    /// unit spells it (willow-itcw). A unit that item-imports an enum registers
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
    pub class_layouts: &'x TypeMap<Vec<(String, Type)>>,
    pub class_base: &'x TypeMap<TypeId>,
    /// Runtime `type_id` per class NAME. A direct type import (`import
    /// zoo::Animal;`) registers the imported class a second time under its
    /// unqualified name, sharing the canonical class's id — so this is what
    /// makes class IDENTITY comparable across those two names.
    pub class_type_ids: &'x TypeMap<i64>,
    /// Whether a name is registered as an interface (never a class here).
    pub is_interface: &'x dyn Fn(&TypeId) -> bool,
    /// The build-wide identity of an interface NAME, or `None` when the name is
    /// not an interface. An item import registers the interface under its local
    /// spelling too, carrying the declaring module's identity, so this is what
    /// makes two spellings of one interface comparable — the same role
    /// [`LirEnumDef::identity`] plays for enums (willow-sxcp).
    pub iface_identity: &'x dyn Fn(&TypeId) -> Option<TypeId>,
    /// Whether boxing `(class, interface)` resolves to a registered vtable —
    /// exactly what [`FuncGen::emit_interface_box`] will look up. A coercion it
    /// cannot build must not be admitted: the emitter's fallback is to pass the
    /// raw object through, which would put an unboxed class pointer in an
    /// interface slot (willow-j260).
    pub can_box: &'x dyn Fn(&TypeId, &TypeId) -> bool,
    /// The declaration of an enum NAME, or `None` when the name is not an enum.
    /// Read from the same `enum_infos` table [`FuncGen::enum_variant_tag`] and
    /// [`FuncGen::enum_is_gc_object_type`] answer from, so the tags and the
    /// representation eligibility vets are the ones emission uses
    /// (willow-0g8j.8). This is also the single source of "is this an enum?" —
    /// see [`LirTypeCtx::is_enum`] — so the two can never disagree.
    pub enum_def: &'x dyn Fn(&TypeId) -> Option<LirEnumDef>,
    /// The vtable slot and signature of `(interface, method)`, i.e. exactly what
    /// [`FuncGen::emit_lir_interface_call`] indexes and calls. `None` when the
    /// name is not an interface, or the interface does not declare that method
    /// — there is no slot to index, so a walker that admitted such a call would
    /// silently miscompile (willow-0g8j.6).
    pub iface_method: &'x dyn Fn(&Type, &str) -> Option<IfaceMethodSig>,
    /// The slot offset at which interface `target`'s vtable is embedded in
    /// interface `source`'s, or `None` when `target` is not a super-interface.
    /// Offset zero is representation-compatible; a non-zero offset makes
    /// [`FuncGen::coerce_to_target`] allocate a box whose vtable pointer is
    /// advanced to the embedded target region (willow-1fc6).
    pub iface_widen_offset: &'x dyn Fn(&TypeId, &TypeId) -> Option<usize>,
    /// The declared type of the static property `(class, field)`, resolved
    /// through the class hierarchy exactly as
    /// [`FuncGen::emit_static_field_read`] resolves the storage it loads from —
    /// so an INHERITED static (`Widget::kind` declared on `Base`) is answered
    /// here iff the emitter will find its data slot. `None` when there is no
    /// such property.
    pub static_field: &'x dyn Fn(&str, &str) -> Option<Type>,
    pub fn_types: &'x FunctionMap<Type>,
    pub func_param_modes: &'x FunctionMap<Vec<ParamMode>>,
    pub known_modules: &'x ModuleSymbols,
    /// The module access names the file being vetted imports (willow-vtlr).
    /// `known_modules` is every module the build declared, so a bare class name
    /// is looked for in THESE first: an unrelated module that happens to
    /// declare the same class name must not make the name ambiguous.
    pub visible_modules: &'x HashSet<String>,
    /// Local alias -> canonical builtin schema module, for the file being
    /// compiled (`import std::fs as files;` records `files -> fs`). Declaration
    /// normalization folds these aliases into its program, but LIR lowering
    /// reads the raw frontend program, so a namespace call still
    /// carries whatever name the `import` spelled (willow-nswv).
    pub builtin_module_aliases: &'x HashMap<String, String>,
    /// The `$lambda.N` symbol a lambda expression was lifted to, by the span of
    /// the lambda (willow-0g8j.2.2) — `{module}.$lambda.N` for one lifted by a
    /// module's own declaration phase (willow-9yhi). `None` for a lambda the
    /// backend never declared, so the walker refuses rather than emitting the
    /// address of nothing.
    pub lambda_symbol: &'x dyn Fn(ExprId) -> Option<FunctionId>,
    /// The declared return type of the function being vetted. Unlike every
    /// other field this one is per-FUNCTION, and it is here because a `return`
    /// inside a `match` arm is checked deep inside `supported_expr`, where the
    /// enclosing [`LirFunction`] is out of reach (willow-0g8j.2.5).
    #[cfg(test)]
    pub return_type: &'x Type,
    /// The class whose body is being vetted, or `None` outside a class. The
    /// second per-FUNCTION field, set the same way `return_type` is, and it is
    /// what resolves `Self::` — `Self::twice(..)`, `Self::count`,
    /// `Self::count = ..` — to the class the emitter will resolve it to
    /// (willow-0g8j.13). `None` leaves `Self` unresolved, every lookup on it
    /// misses and the function is refused (`E0800`), rather than the walker
    /// guessing.
    pub self_class: Option<&'x str>,
    /// The async functions compiled as cooperative LEAVES in this module.
    /// `await f(..)` on one of them is a direct constructor call plus a
    /// suspension, so it needs neither a `Task` value nor a `Task`-typed local
    /// (willow-0g8j.2.11).
    pub cooperative_leaves: &'x std::collections::HashSet<FunctionId>,
}

/// Normalise a variant's payload list for a use site where every payload
/// substituted away to `void`.
///
/// `Result<void, E>::Ok()` is the case that forces this: the declared payload
/// is `T`, the instantiation makes it `void`, and the checker accepts the call
/// with ZERO arguments. The heap layout is derived from the arguments, with
/// `payload_types` indexed positionally. Dropping the list keeps validation,
/// the LIR emitter and [`FuncGen::emit_enum_variant_alloc`] describing one object.
///
/// A *mixed* list (`void` in one slot, a real type in another) is deliberately
/// left alone: the arity check downstream rejects the function rather than
/// silently renumbering payload slots.
fn normalize_void_payloads(payloads: &mut Vec<Type>) {
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
/// no table answers to is retried once per known module (willow-0g8j.2.19). Without this
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
/// over the modules this unit can actually SEE first (willow-vtlr): a module
/// the file never imported must not make its class name ambiguous here, since
/// the only effect of that ambiguity is to cost an eligible body its lowering.
/// The all-modules scan is still what answers when the visible ones say
/// nothing, so no name that resolves today stops resolving — a class reached
/// through a module that only another module imports keeps working.
fn resolve_class_key<Q: super::type_index::TypeLookup + ?Sized>(
    class_layouts: &TypeMap<Vec<(String, Type)>>,
    class_type_ids: &TypeMap<i64>,
    known_modules: &ModuleSymbols,
    visible_modules: &HashSet<String>,
    name: &Q,
) -> Option<TypeId> {
    let name = &name.type_id();
    if class_layouts.contains_key(name) {
        return Some(*name);
    }
    // One runtime class is one `type_id`, so two spellings that carry the same
    // id are one answer; a class with no id at all is never merged by name.
    let same_class = |a: &TypeId, b: &TypeId| match (class_type_ids.get(a), class_type_ids.get(b)) {
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
            if !class_layouts.contains_key(&qualified) {
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
    /// (willow-0g8j.13) — so a `Self::` form is admitted under the very symbol
    /// the emitter will go on to resolve it to. Outside a class body, and in
    /// any other spelling, the name is returned unchanged.
    fn resolved_class<'n>(&'n self, class: &'n str) -> &'n str {
        if class == "Self" {
            self.self_class.unwrap_or(class)
        } else {
            class
        }
    }

    /// Whether `name` is a declared enum. Answered from [`Self::enum_def`], so
    /// there is exactly one table deciding it.
    fn is_enum<Q: super::type_index::TypeLookup + ?Sized>(&self, name: &Q) -> bool {
        (self.enum_def)(&name.type_id()).is_some()
    }

    /// Whether `ty` is an enum whose values ARE the tag: every variant is
    /// payload-free, so [`FuncGen::enum_is_gc_object_type`] answers `false` and
    /// a value of it travels as a plain `i64` rather than a heap pointer. That
    /// is what makes `==` on two of them the integer comparison the emitter
    /// already produces, instead of the object-identity test a payload-carrying
    /// enum would need (willow-0g8j.3). A generic enum is excluded: its
    /// `payloads` hold type-parameter placeholders, not the real types.
    fn tag_immediate_enum(&self, ty: &Type) -> bool {
        let Type::Named(name) = ty else {
            return false;
        };
        (self.enum_def)(name).is_some_and(|def| {
            def.type_params.is_empty() && def.variants.iter().all(|v| v.payloads.is_empty())
        })
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
    fn enum_instance(&self, ty: &Type) -> Option<(TypeId, LirEnumDef)> {
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
    /// refused with `E0800`, since willow-0g8j.3 left no other emitter.
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

    fn supported_type_inner(&self, ty: &Type, open: &mut HashSet<Type>) -> bool {
        match ty {
            Type::Array(elem) => {
                !matches!(**elem, Type::Void) && self.supported_type_inner(elem, open)
            }
            // The atomic cells (willow-0g8j.2.13): a GC-allocated word with no
            // element type to vet. Admitted before the class path because the
            // prelude owns both names, so nothing else can answer to them.
            Type::Named(_) if atomic_cell(ty).is_some() => true,
            // The cancellation handles (willow-0g8j.2.13): opaque runtime
            // objects, admitted on the same grounds — no element type, and a
            // name the prelude owns.
            Type::Named(_) if cancellation_handle(ty).is_some() => true,
            Type::Named(name) => {
                (self.is_interface)(name)
                    || self.supported_enum_inner(ty, open)
                    || self.supported_class_inner(name, open)
                    // The socket handles (willow-0g8j.2.13): opaque runtime
                    // objects that only the `net::` calls above produce and
                    // consume, so there is no element type to vet. Tried LAST,
                    // unlike the atomic cells and the cancellation handles: the
                    // prelude does not own these two names, so anything the
                    // program declares under them answers first.
                    || matches!(name.name(), "TcpListener" | "TcpStream")
            }
            // A `Range<i64>` value (willow-0g8j.2.10): two `i64` words, no
            // element type to vet and no other instantiation to admit.
            Type::Generic(..) if range_i64(ty) => true,
            // The native-blocking cells (willow-0g8j.2.13): a runtime cell
            // holding ONE word, so its element type is vetted exactly as a
            // channel's is. Admitted before the collection path below because
            // the prelude owns both names, so nothing else can answer to them.
            Type::Generic(..) if blocking_cell(ty).is_some() => {
                let (_, elem) = blocking_cell(ty).expect("guarded by the arm");
                !matches!(elem, Type::Void) && self.supported_type_inner(elem, open)
            }
            // A runtime future (willow-0g8j.3): one opaque pointer word, like
            // the blocking cells above and, like them, NOT GC-managed
            // (`is_opaque_runtime_pointer_type`), so the walker copies it
            // around without ever rooting or dereferencing it. Its output type
            // is vetted anyway, because that is what an `await` of it would
            // produce; `Void` is admitted, since `Future<void>` is the only
            // instantiation the language can actually produce -- from `sleep`
            // and `yield`.
            //
            // Awaiting a future VALUE is NOT a suspension: lowering refuses
            // to split it (`lower_root_suspend`) and the walker emits the
            // blocking `willow_future_await_*` call in place, so a function
            // that stores a future and awaits it later stays inside the
            // subset (willow-0g8j.3).
            Type::Generic(..) if builtin_types::unary_arg(ty, B::Future).is_some() => {
                builtin_types::unary_arg(ty, B::Future)
                    .is_some_and(|output| self.supported_type_inner(output, open))
            }
            // The builtin collections (willow-0g8j.7) and instantiated generic
            // enums — `Option<T>`, `Result<T, E>`, a user generic enum
            // (willow-0g8j.2.1). Generic *classes* remain outside, so named
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
            // `TaskResult<T>` joins them because it IS one of them: `t.result()`
            // is the identity on the async frame pointer, and only the static
            // type distinguishes `await t` from `await t.result()`
            // (willow-0g8j.2.11).
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
                // A scheduler-aware lock handle (willow-0g8j.2.13). Its
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
            // The two callable values (willow-0g8j.2.2, willow-0g8j.2.12). A
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
    fn storable(&self, target: &Type, value: &Type) -> bool {
        self.repr_compatible(target, value)
            || self.iface_widen_offset(target, value).is_some()
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
    pub(super) fn supported_enum(&self, name: &str) -> bool {
        self.supported_enum_type(&Type::Named(name.to_string().into()))
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
    /// emission, from the same instantiated type vetted here, so validation and
    /// emission always pick the same representation.
    pub(super) fn supported_enum_type(&self, ty: &Type) -> bool {
        let mut open = HashSet::new();
        self.supported_enum_inner(ty, &mut open)
    }

    fn supported_enum_inner(&self, ty: &Type, open: &mut HashSet<Type>) -> bool {
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
        if !open.insert(ty.clone()) {
            return true;
        }
        def.variants.iter().all(|v| {
            v.payloads
                .iter()
                .all(|t| self.supported_type_inner(t, open))
        })
    }

    /// A class the walker can emit: it has a registered field layout, is not an
    /// interface or an enum, and every field type is itself supported.
    ///
    /// Inheritance used to be a hard exclusion here, because a direct call to
    /// `Class__method` is only the whole of dispatch when the receiver's static
    /// type is exact. It is no longer: a virtual call goes through the class
    /// DESCRIPTOR at a compile-time slot index (willow-fm7t), and the walker
    /// takes that decision from the shared [`FuncGen::plan_virtual_call`]
    /// (willow-0g8j.2.4). What makes the rest safe is that a subclass EXTENDS
    /// rather than rearranges: its field layout starts with its base's, and its
    /// slot order starts with its base's, so code compiled against the base
    /// reads the right offsets and calls the right slots on a subclass
    /// instance.
    ///
    /// Only the class NAMED here has to be supported. A subclass with a field
    /// type the walker does not admit stays out of the subset on its own terms
    /// — no expression typed as the base ever touches that field.
    pub(super) fn supported_class<Q: super::type_index::TypeLookup + ?Sized>(
        &self,
        name: &Q,
    ) -> bool {
        let mut open = HashSet::new();
        self.supported_class_inner(&name.type_id(), &mut open)
    }

    /// The name the class tables are keyed on for `name`, or `None` when
    /// nothing this compilation unit registered answers to it. See
    /// [`resolve_class_key`] — eligibility and emission must resolve a name the
    /// same way or the walker admits a body the emitter then cannot key.
    fn class_key(&self, name: &TypeId) -> Option<TypeId> {
        resolve_class_key(
            self.class_layouts,
            self.class_type_ids,
            self.known_modules,
            self.visible_modules,
            name,
        )
    }

    fn supported_class_inner(&self, name: &TypeId, open: &mut HashSet<Type>) -> bool {
        if (self.is_interface)(name) || self.is_enum(name) {
            return false;
        }
        let Some(key) = self.class_key(name) else {
            return false;
        };
        let Some(layout) = self.class_layouts.get(&key) else {
            return false;
        };
        // A self- or mutually-referential field (`class Node { next: Node; }`)
        // is fine — it is the same layout — but must not recurse forever. Keyed
        // on the resolved name, so the bare and qualified spellings of one
        // class close the same cycle.
        if !open.insert(Type::Named(key)) {
            return true;
        }
        layout
            .iter()
            .all(|(_, ty)| self.supported_type_inner(ty, open))
    }

    /// Whether `name` names a class — as opposed to an interface, an enum, or
    /// nothing this compilation unit registered.
    fn is_class(&self, name: &TypeId) -> bool {
        !(self.is_interface)(name) && !self.is_enum(name) && self.class_key(name).is_some()
    }

    /// Whether a value of class type `value` may be stored where class `target`
    /// is expected because `value` is a strict SUBCLASS of it
    /// (willow-0g8j.2.4).
    ///
    /// At run time the widening is nothing: both are raw object pointers. What
    /// it costs is exactness — the reader then works from `target`'s layout and
    /// `target`'s slot indices — and a subclass extends both rather than
    /// rearranging either, so both remain valid.
    ///
    /// The question is asked in `type_id` space rather than over NAMES. A
    /// direct type import (`import zoo::Animal;`) copies the imported class's
    /// tables under the unqualified `Animal`, while `class_base` keeps
    /// CANONICAL names on both sides of every edge (`zoo::Dog` → `zoo::Animal`)
    /// — so comparing `"Animal"` to those strings finds nothing and an aliased
    /// base looks like a leaf. Aliased and canonical spellings share one
    /// runtime `type_id`.
    fn class_widening(&self, target: &Type, value: &Type) -> bool {
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
            self.class_type_ids.get(&base_key).copied(),
            self.class_type_ids.get(&sub_key).copied(),
        ) else {
            return false;
        };
        // Identity is `assignable_repr`'s job; this arm answers only the
        // strictly-wider case.
        base_id != sub_id
            && is_self_or_descendant(
                &class_base_ids(self.class_base, self.class_type_ids),
                sub_id,
                base_id,
            )
    }

    /// [`assignable_repr`], plus the one identity a string comparison cannot
    /// see: two spellings of the SAME class (willow-0g8j.16).
    ///
    /// A module's classes are keyed canonically — `declare_module` qualifies
    /// `Point` to `geom::Point`, fields and all — and aliased back under their
    /// bare local names while that module's bodies are compiled. So a field
    /// declared `at: Point` reaches the walker as `geom::Point` from the layout
    /// and as `Point` from the lowered body, and those are one type. Anything
    /// else defers to [`assignable_repr`], including the deliberate exactness
    /// of the collection generics.
    fn same_repr(&self, target: &Type, value: &Type) -> bool {
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
            (Type::Array(a), Type::Array(b)) => self.same_repr_inner(a, b, open),
            (Type::Generic(a, ta), Type::Generic(b, tb)) => {
                (a == b || self.same_iface(a, b) || self.same_enum(a, b, open))
                    && ta.len() == tb.len()
                    && ta
                        .iter()
                        .zip(tb)
                        .all(|(x, y)| self.same_repr_inner(x, y, open))
            }
            _ => assignable_repr(target, value),
        }
    }

    /// Whether two enum names are one declaration under two spellings. A module
    /// body spells its own enum bare (`Kind`), while the SIGNATURE of a method
    /// declared in that module carries the qualified spelling (`kinds::Kind`) —
    /// a class declaration is module-qualified whole before it is declared. One
    /// declaration, registered under both names, so the strings differ where
    /// the declaration does not (willow-0g8j.3.2).
    ///
    /// An item import settles it outright: `import signal::Level;` — or
    /// `as Rank` — registers the declaration under the local spelling too, and
    /// every entry carries the declaring module's identity, so two spellings of
    /// one declaration answer the same [`LirEnumDef::identity`]. An alias has
    /// no suffix in common with what it renames, so this is the ONLY test that
    /// settles `Rank` against `signal::Level` (willow-0g8j.3).
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
                    .is_some_and(|namespace| self.known_modules.contains_key(namespace))
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
    /// (willow-sxcp), exactly as [`Self::same_enum`] settles an enum alias.
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
            (self.class_type_ids.get(&ka), self.class_type_ids.get(&kb)),
            (Some(x), Some(y)) if x == y
        )
    }

    /// [`Self::same_repr`] widened by class subtyping. Use it wherever a value
    /// flows into a declared slot whose type the emitter does NOT coerce to —
    /// an array element, a `return`, a field store, a branch of an `if`.
    fn repr_compatible(&self, target: &Type, value: &Type) -> bool {
        self.same_repr(target, value)
            || self.class_widening(target, value)
            || self.iface_widen_offset(target, value) == Some(0)
    }

    /// The vtable-slot adjustment required to widen `value` to `target`.
    /// Identity is handled by [`assignable_repr`], so this answers only a
    /// strict interface-to-super-interface conversion.
    fn iface_widen_offset(&self, target: &Type, value: &Type) -> Option<usize> {
        let (Type::Named(target_iface), Type::Named(value_iface)) = (target, value) else {
            return None;
        };
        (target_iface != value_iface
            && (self.is_interface)(target_iface)
            && (self.is_interface)(value_iface))
        .then(|| (self.iface_widen_offset)(target_iface, value_iface))
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
    fn resolve_class_method(&self, class: &TypeId, method: &str) -> Option<String> {
        // Deliberately NOT run through [`resolve_class_key`]: emission resolves
        // a method through `FuncGen::resolve_defining_class` and
        // `plan_virtual_call`, which key on the class name AS WRITTEN, so
        // admitting a bare module-class receiver here would promise a dispatch
        // the emitter cannot resolve. Nothing is lost — the checker rejects
        // `p.m()` on a module class the entry file never item-imported
        // (E0350), and an item import puts the short name in the tables
        // (willow-0g8j.2.19).
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
            search = self.class_base.get(&name).cloned();
        }
        None
    }

    /// The declared field layout of a supported class named by `ty`.
    fn class_layout_of(&self, ty: &Type) -> Option<&Vec<(String, Type)>> {
        let Type::Named(name) = ty else { return None };
        if !self.supported_class(name) {
            return None;
        }
        self.class_layouts.get(&self.class_key(name)?)
    }

    /// The declared callable type of a symbol that is about to be used as a
    /// VALUE — `fn(...) -> ...`, or `closure(...) -> ...` for a lifted
    /// capturing lambda (willow-0g8j.2.2, willow-0g8j.2.12) — or `None` when it
    /// cannot be. The closure's hidden environment parameter is NOT part of
    /// this type: it is the value itself, not something a call site passes.
    ///
    /// Three things have to hold, and all three are about the pointer being
    /// callable later through a signature built from the type alone: the symbol
    /// is linkable, every parameter is passed by value (a by-reference
    /// parameter has no spelling in a `fn(...)` type, so the call site could not
    /// reproduce it), and the whole signature is inside the subset.
    fn fn_value_of(&self, mangled: &FunctionId) -> Option<Type> {
        if !(self.known_fn)(&mangled.to_string()) {
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
    fn callable(&self, mangled: &str, args: &[HirExpr], skip_self: bool) -> bool {
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

/// Conservative "can evaluating this run a collection?" test, used to decide
/// whether a live GC temporary has to be rooted across a sub-expression. Only
/// the forms known to be allocation-free answer `false`; everything else —
/// including any expression form added later — answers `true`, so a new node
/// kind cannot silently drop a root.
#[cfg(test)]
fn may_allocate(e: &HirExpr) -> bool {
    let mut pending = vec![e];
    while let Some(expr) = pending.pop() {
        match &expr.kind {
            HirExprKind::Int(_)
            | HirExprKind::Float(_)
            | HirExprKind::Bool(_)
            | HirExprKind::Var(_)
            | HirExprKind::FnRef(_) => {}
            HirExprKind::ReferenceArg { place } => pending.push(place),
            // Only constructing a captured environment allocates. Never inspect
            // a lambda body: it executes later as a separate callable.
            HirExprKind::Lambda { .. } => {
                if matches!(expr.ty, Type::Closure(..)) {
                    return true;
                }
            }
            HirExprKind::Unary { operand, .. } => pending.push(operand),
            HirExprKind::Binary { op, lhs, rhs } => {
                if lhs.ty == Type::String && matches!(op, BinOp::Add) {
                    return true;
                }
                pending.extend([rhs.as_ref(), lhs.as_ref()]);
            }
            HirExprKind::Ternary {
                condition,
                then_expr,
                else_expr,
            } => {
                pending.extend([else_expr.as_ref(), then_expr.as_ref(), condition.as_ref()]);
            }
            HirExprKind::Index { array, index } => {
                pending.extend([index.as_ref(), array.as_ref()]);
            }
            HirExprKind::FieldAccess { object, .. } => pending.push(object),
            // Unknown forms conservatively retain GC roots.
            _ => return true,
        }
    }
    false
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

/// Whether every LIR block of a SYNCHRONOUS function can be emitted under the
/// defer-scope stack the emitter will actually be holding (willow-0g8j.2.15).
///
/// `emit_lir_function_inner` snapshots the Rust-side scope state at block
/// boundaries. That is sound exactly when the open scopes are a property of
/// the block rather than of the path taken to it: every ordinary predecessor
/// and every recovered-panic edge must agree on the block's entry stack.
///
/// A scope opened and closed inside one block satisfies this trivially, which
/// is every shape that was eligible before this check replaced a per-block
/// balance test. The interesting new one is a scope opened in the entry block
/// and left by a `return` in a later block: the stack is `[scope]` at every
/// block after the first. A scope opened in one arm of a branch and still open
/// at the join remains ambiguous and is refused.
///
/// Scopes are identified by where they were opened, so a stack comparison
/// distinguishes "the same scope is still open" from "one closed and another
/// opened", which a depth alone cannot.
/// The LIR blocks control can reach directly from `block`.
fn lir_block_successors(block: &LirBlock) -> Vec<usize> {
    match &block.terminator {
        Terminator::Jump(target) => vec![target.0],
        Terminator::Branch {
            then_block,
            else_block,
            ..
        } => vec![then_block.0, else_block.0],
        Terminator::Suspend { resume, .. } => vec![resume.0],
        Terminator::Return(_) | Terminator::CleanupReturn => Vec::new(),
    }
}

/// Function entry covers recursion; DFS backedge targets cover every cycle,
/// including irreducible control flow and recovery edges. Polling ordinary
/// forward blocks adds overhead without covering additional unbounded work.
fn lir_sync_poll_blocks(f: &LirFunction) -> Vec<bool> {
    let mut poll = vec![false; f.blocks.len()];
    if let Some(entry) = poll.first_mut() {
        *entry = true;
    }
    let edges: Vec<Vec<usize>> = f
        .blocks
        .iter()
        .map(|block| {
            let mut edges = lir_block_successors(block);
            edges.extend(block.recovery.iter().map(|target| target.0));
            edges
        })
        .collect();
    let mut color = vec![0u8; f.blocks.len()];
    for root in 0..f.blocks.len() {
        if color[root] != 0 {
            continue;
        }
        color[root] = 1;
        let mut pending = vec![(root, 0)];
        while let Some((block, next)) = pending.last_mut() {
            if *next == edges[*block].len() {
                color[*block] = 2;
                pending.pop();
                continue;
            }
            let target = edges[*block][*next];
            *next += 1;
            match color[target] {
                0 => {
                    color[target] = 1;
                    pending.push((target, 0));
                }
                1 => poll[target] = true,
                _ => {}
            }
        }
    }
    poll
}

/// A bounded scalar early return needs no safepoint. Move the entry poll to
/// the other branch, including its runtime address/context lookups. Restrict
/// this to acyclic bodies so loop guards keep their invocation-cached inputs.
fn lir_defer_entry_poll(f: &LirFunction, poll: &mut [bool]) -> bool {
    use crate::ir::lowered::LirRvalue as V;
    let scalar = |ty: &Type| matches!(ty, Type::I64 | Type::F64 | Type::Bool);
    if f.is_async
        || f.params.iter().any(|p| p.by_reference)
        || f.locals.iter().any(|l| l.is_gc_owner() || !scalar(&l.ty))
        || poll.iter().skip(1).any(|p| *p)
        || f.blocks
            .iter()
            .any(|b| !b.recovery.is_empty() || lir_block_successors(b).contains(&0))
    {
        return false;
    }
    let Some(entry) = f.blocks.first() else {
        return false;
    };
    let Terminator::Branch {
        then_block,
        else_block,
        ..
    } = &entry.terminator
    else {
        return false;
    };
    let bounded = |block: &LirBlock| {
        block.instrs.len() <= 8
            && block.instrs.iter().all(|inst| match inst {
                LirInst::Compute { value, .. } => match value {
                    V::Use(_) | V::Unary { .. } => true,
                    V::Binary { op, .. } => !matches!(op, BinOp::Div | BinOp::Rem | BinOp::Pow),
                    _ => false,
                },
                LirInst::Let { .. } | LirInst::Assign { .. } | LirInst::ClearScopeRoots { .. } => {
                    true
                }
                _ => false,
            })
    };
    let returns = |id: BlockId| {
        f.blocks
            .get(id.0)
            .is_some_and(|b| matches!(b.terminator, Terminator::Return(_)) && bounded(b))
    };
    if !bounded(entry) || (!returns(*then_block) && !returns(*else_block)) {
        return false;
    }
    poll[0] = false;
    for target in [*then_block, *else_block] {
        poll[target.0] = !returns(target);
    }
    true
}

/// Argument evaluation may cross basic blocks or a cooperative suspension.
/// Validate the matching preparations and retain the source frames at each
/// entry, independently of the order in which machine blocks are emitted.
fn lir_call_frame_entries(f: &LirFunction) -> Option<Vec<Vec<(String, Span)>>> {
    use crate::ir::lowered::{LirOperand, LirRvalue};
    type Stack = Vec<(crate::ir::lowered::LirLocalId, String, Span)>;
    let mut entries: Vec<Option<Stack>> = vec![None; f.blocks.len()];
    if entries.is_empty() {
        return Some(Vec::new());
    }
    entries[0] = Some(Vec::new());
    let mut work = vec![0];
    while let Some(index) = work.pop() {
        let mut stack = entries.get(index)?.clone()?;
        let mut edges = Vec::new();
        for instruction in &f.blocks[index].instrs {
            match instruction {
                LirInst::Compute {
                    local,
                    value: LirRvalue::PrepareMethod { method, .. },
                    span,
                } => stack.push((*local, method.clone(), *span)),
                LirInst::Compute {
                    value:
                        LirRvalue::MethodCall {
                            receiver, method, ..
                        },
                    ..
                } => {
                    let (prepared, expected, _) = stack.pop()?;
                    if *receiver != LirOperand::Local(prepared) || *method != expected {
                        return None;
                    }
                }
                LirInst::EnterDeferScope {
                    resume: Some(resume),
                    ..
                } => edges.push((resume.0, stack.clone())),
                _ => {}
            }
        }
        edges.extend(
            lir_block_successors(&f.blocks[index])
                .into_iter()
                .map(|target| (target, stack.clone())),
        );
        for (target, state) in edges {
            match entries.get_mut(target)? {
                Some(existing) if *existing != state => return None,
                Some(_) => {}
                entry @ None => {
                    *entry = Some(state);
                    work.push(target);
                }
            }
        }
    }
    Some(
        entries
            .into_iter()
            .map(|entry| {
                entry
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(_, name, span)| (name, span))
                    .collect()
            })
            .collect(),
    )
}

fn lir_reference_scope_entries(
    f: &LirFunction,
) -> Option<Vec<Vec<Vec<super::FlatReferenceDebug>>>> {
    use crate::ir::lowered::{LirOperand, LirRvalue as V};
    type Scopes = Vec<Vec<super::FlatReferenceDebug>>;
    let mut entries: Vec<Option<Scopes>> = vec![None; f.blocks.len()];
    if entries.is_empty() {
        return Some(Vec::new());
    }
    entries[0] = Some(Vec::new());
    let mut work = vec![0];
    while let Some(index) = work.pop() {
        let mut scopes = entries.get(index)?.clone()?;
        let mut edges = Vec::new();
        for instruction in &f.blocks[index].instrs {
            match instruction {
                LirInst::Compute {
                    value: V::BeginReferenceCall,
                    ..
                } => scopes.push(Vec::new()),
                LirInst::Compute {
                    value:
                        V::ReferenceDebug {
                            argument,
                            callee,
                            index,
                        },
                    ..
                } => scopes.last_mut()?.push(super::FlatReferenceDebug {
                    argument: argument.clone(),
                    callee: *callee,
                    index: *index,
                }),
                LirInst::Compute {
                    value:
                        V::DirectCall { args, .. }
                        | V::StaticCall { args, .. }
                        | V::MethodCall { args, .. }
                        | V::ConstructorCall { args, .. },
                    ..
                } if args
                    .iter()
                    .any(|arg| matches!(arg, LirOperand::Reference { .. })) =>
                {
                    scopes.pop()?;
                }
                LirInst::EnterDeferScope {
                    resume: Some(resume),
                    ..
                } => edges.push((resume.0, scopes.clone())),
                _ => {}
            }
        }
        edges.extend(
            lir_block_successors(&f.blocks[index])
                .into_iter()
                .map(|target| (target, scopes.clone())),
        );
        for (target, state) in edges {
            match entries.get_mut(target)? {
                Some(existing) if *existing != state => return None,
                Some(_) => {}
                entry @ None => {
                    *entry = Some(state);
                    work.push(target);
                }
            }
        }
    }
    Some(entries.into_iter().map(Option::unwrap_or_default).collect())
}

/// How many of the innermost open scopes a `FlushDefers` naming `sites` covers.
///
/// An abrupt exit names every registration it has to run, so the scopes it
/// unwinds are exactly the innermost run of open scopes whose own registrations
/// are all in that list. A scope only partly named is not being left, which
/// stops the count.
/// The scope identities a synchronous defer state holds open, outermost first.
///
/// The frames themselves carry Cranelift blocks and stack slots, so two states
/// are only ever compared by identity (willow-0g8j.2.15).
fn lir_scope_ids(state: &LirDeferState) -> Vec<LirScopeId> {
    state.scopes.iter().map(|frame| frame.id).collect()
}

fn lir_flushed_scope_count(
    sites: &[crate::ir::lowered::LirDeferId],
    open_sites: &[Vec<crate::ir::lowered::LirDeferId>],
) -> usize {
    open_sites
        .iter()
        .rev()
        .take_while(|scope| scope.iter().all(|id| sites.contains(id)))
        .count()
}

/// Whether the defer scopes open at each LIR block agree along every edge that
/// reaches it, which is what lets the synchronous emitter restore a block's
/// scope state instead of carrying one stack through the whole body.
///
/// The scope stack is a property of the path, and LIR blocks are not emitted in
/// the order control flows through them, so this is a fixpoint over the edges
/// rather than a walk in block order. `FlushDefers` is an abrupt exit — the
/// path it is on leaves the scopes it names, without a `LeaveDeferScope`
/// (willow-0g8j.2.15).
fn lir_sync_defer_stacks_agree(f: &LirFunction) -> bool {
    let touches_defers = f.blocks.iter().any(|block| {
        block.instrs.iter().any(|inst| {
            matches!(
                inst,
                LirInst::EnterDeferScope { .. }
                    | LirInst::LeaveDeferScope { .. }
                    | LirInst::Defer { .. }
                    | LirInst::FlushDefers { .. }
            )
        })
    });
    if !touches_defers {
        return true;
    }

    // Each open scope carries its own registrations, so a flush reached from a
    // block that did not open the scope can still tell whether it unwinds it.
    type OpenScope = (LirScopeId, Vec<crate::ir::lowered::LirDeferId>);
    let mut entry: Vec<Option<Vec<OpenScope>>> = vec![None; f.blocks.len()];
    entry[0] = Some(Vec::new());
    let mut worklist = vec![0usize];
    while let Some(index) = worklist.pop() {
        let Some(mut stack) = entry[index].clone() else {
            continue;
        };
        for (position, inst) in f.blocks[index].instrs.iter().enumerate() {
            match inst {
                LirInst::EnterDeferScope { sites, resume, .. } => {
                    // A recovered panic has finished this scope before it
                    // reaches the explicit continuation. Record that edge at
                    // the entry instruction, while `stack` still describes
                    // the enclosing scopes only.
                    if let Some(resume) = resume {
                        match &entry[resume.0] {
                            Some(existing) if *existing != stack => return false,
                            Some(_) => {}
                            None => {
                                entry[resume.0] = Some(stack.clone());
                                worklist.push(resume.0);
                            }
                        }
                    }
                    stack.push(((index, position), sites.iter().map(|(id, _)| *id).collect()));
                }
                LirInst::LeaveDeferScope { .. } => match stack.pop() {
                    Some(_) => {}
                    None => return false,
                },
                LirInst::FlushDefers { sites } => {
                    let open_sites: Vec<Vec<_>> =
                        stack.iter().map(|(_, scope)| scope.clone()).collect();
                    let flushed = lir_flushed_scope_count(sites, &open_sites);
                    stack.truncate(stack.len() - flushed);
                }
                LirInst::Defer { .. } if stack.is_empty() => return false,
                _ => {}
            }
        }
        for target in lir_block_successors(&f.blocks[index]) {
            match &entry[target] {
                Some(existing) if *existing != stack => return false,
                Some(_) => {}
                None => {
                    entry[target] = Some(stack.clone());
                    worklist.push(target);
                }
            }
        }
    }
    // An unreachable block is still emitted, under a scope state no edge ever
    // fixed.
    entry.iter().all(|state| state.is_some())
}

/// Why the walker will not compile `f`, phrased to read after "has invalid
/// lowered IR: " — `None` when the function IS in the subset.
///
/// The single source of truth for eligibility (see [`lir_supported_function`]).
/// This is what the compile error prints, which is the difference between
/// "something in this function is unsupported" and a construct, a type and a
/// line to go and fix.
pub(super) fn lir_rejection_reason(f: &LirFunction, ctx: &LirTypeCtx<'_>) -> Option<String> {
    if f.blocks.iter().enumerate().any(|(index, block)| {
        block.id.0 != index
            || lir_block_successors(block)
                .iter()
                .any(|target| *target >= f.blocks.len())
            || block
                .recovery
                .iter()
                .any(|target| target.0 >= f.blocks.len())
    }) {
        return Some("the LIR graph has an invalid block identity or target".into());
    }
    // Entry contains parameter/local initialization and is not a loop header.
    // Cleanup replay also suppresses its entry cancellation check so cleanup
    // can begin while cancellation is sticky; cycles need a separate header.
    if f.blocks.iter().any(|block| {
        lir_block_successors(block).contains(&0)
            || block.recovery.iter().any(|target| target.0 == 0)
    }) {
        return Some("the LIR entry block must not have incoming edges".into());
    }
    if lir_call_frame_entries(f).is_none() {
        return Some("method preparation frames disagree across control-flow edges".into());
    }
    if lir_reference_scope_entries(f).is_none() {
        return Some("reference argument scopes disagree across control-flow edges".into());
    }
    // The two per-function fields, taken from the function under test rather
    // than from the caller, so a `return` inside a `match` arm is checked
    // against this function's declared type and no caller can get it wrong.
    // The enclosing class comes from the lowered name (`Class::method`), which
    // is the same string `compile_class_method_inner` looks the body up under
    // and the same class the emitter sets as `FuncGen::current_class`.
    let owner = f.name.owner_type().map(|owner| owner.to_string());
    let ctx = &LirTypeCtx {
        #[cfg(test)]
        return_type: &f.return_type,
        // `rsplit`, not `split`: a module class is keyed by its qualified
        // name, so the method `shapes::Point::area` has `shapes::Point` as its
        // class and only the LAST separator divides the two (willow-0g8j.16).
        self_class: owner.as_deref().or_else(|| {
            if f.name.is_free_named("$defer") {
                ctx.self_class
            } else {
                None
            }
        }),
        ..*ctx
    };
    if !ctx.supported_type(&f.return_type) {
        return Some(format!(
            "its return type `{}` is outside the walker's subset",
            type_name(&f.return_type)
        ));
    }
    for p in &f.params {
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
    // a bare enum variant, a function used as a value — so an unresolved
    // form fails validation (willow-0g8j.1).
    let mut names: HashMap<&str, Cow<'_, Type>> = HashMap::new();
    let mut physical_names = HashSet::new();
    for local in &f.locals {
        if !physical_names.insert(local.name.as_str()) {
            return Some(format!(
                "LIR local `{}` reuses an existing lowered name",
                local.name
            ));
        }
        if local.is_gc_owner() {
            if !local.synthetic || local.parameter || local.ty != Type::Void {
                return Some(
                    "opaque GC owners must be synthetic storage without a language type".into(),
                );
            }
        } else {
            names.insert(local.name.as_str(), Cow::Borrowed(&local.ty));
        }
    }

    // Opaque owners carry no language type. Recover their checked capture
    // provenance so malformed LIR cannot reinterpret a buffer or replace its
    // bounds-checked index with another local.
    let mut array_captures = HashMap::new();
    for block in &f.blocks {
        for inst in &block.instrs {
            if let LirInst::Compute {
                local,
                value: crate::ir::lowered::LirRvalue::CaptureArrayOwner { array, index },
                ..
            } = inst
            {
                let Some(Type::Array(ref element)) = array.ty(&f.locals) else {
                    return Some("array reference owner has no array provenance".into());
                };
                if array_captures
                    .insert(*local, ((**element).clone(), index.clone()))
                    .is_some()
                {
                    return Some("array reference owner has multiple definitions".into());
                }
            }
        }
    }
    for block in &f.blocks {
        for inst in &block.instrs {
            if let LirInst::Compute { value, .. } = inst {
                let mut operands = value.operands();
                if let crate::ir::lowered::LirRvalue::ReferenceDebug { argument, .. } = value {
                    operands.push(argument);
                }
                for operand in operands {
                    if let crate::ir::lowered::LirOperand::Reference {
                        place:
                            crate::ir::lowered::LirPlace::ArrayElement {
                                owner,
                                index,
                                element,
                            },
                        ..
                    } = operand
                        && !array_captures
                            .get(owner)
                            .is_some_and(|(captured, checked_index)| {
                                ctx.same_repr(captured, element)
                                    && *checked_index
                                        == crate::ir::lowered::LirOperand::Local(*index)
                            })
                    {
                        return Some(
                            "array reference disagrees with its captured buffer or checked index"
                                .into(),
                        );
                    }
                }
            }
        }
    }

    // A synchronous function's defer scopes live on the emitter's own Rust-side
    // stack, so the walker can only compile one whose open scopes are the same
    // whichever way control reached a block AND are what the emitter is holding
    // when it gets there (willow-0g8j.2.15).
    if !f.is_async && !lir_sync_defer_stacks_agree(f) {
        return Some("its defer scope crosses LIR blocks".to_string());
    }

    // User binding types remain the primary diagnostic even after their
    // initializers have been split into preceding Compute instructions.
    for block in &f.blocks {
        for instruction in &block.instrs {
            if let LirInst::Let { name, ty, .. } = instruction
                && !ctx.supported_type(ty)
            {
                return Some(format!(
                    "`let {name}` binds type `{}`, outside the walker's subset",
                    type_name(ty)
                ));
            }
        }
    }
    for block in &f.blocks {
        for inst in &block.instrs {
            match inst {
                LirInst::Compute { local, value, span } => {
                    if let crate::ir::lowered::LirRvalue::DirectCall {
                        callee,
                        args,
                        params,
                        result,
                    } = value
                    {
                        if !(ctx.known_fn)(&callee.to_string()) {
                            return Some(format!(
                                "the call to `{callee}` at line {} has no declared function",
                                span.line
                            ));
                        }
                        if let Some(ty) = std::iter::once(result)
                            .chain(params)
                            .find(|ty| !ctx.supported_type(ty))
                        {
                            return Some(format!(
                                "the call to `{callee}` at line {} has type `{}`, outside the supported LIR types",
                                span.line,
                                type_name(ty)
                            ));
                        }
                        if !flat_argument_modes_match(
                            args,
                            ctx.func_param_modes
                                .get_id(callee)
                                .map(Vec::as_slice)
                                .unwrap_or(&[]),
                        ) {
                            return Some(format!(
                                "the call to `{callee}` has incompatible reference modes"
                            ));
                        }
                        if ctx.fn_types.get_id(callee).is_some_and(|signature| !matches!(signature,
                            Type::Fn(declared, output) if declared.len() == params.len()
                                && declared.iter().zip(params).all(|(declared, actual)| ctx.same_repr(declared, actual))
                                && ctx.same_repr(output, result)))
                             {
                            return Some(format!("the call to `{callee}` at line {} has an incompatible signature", span.line));
                        }
                    }
                    match value {
                        crate::ir::lowered::LirRvalue::FunctionRef { function, ty }
                            if !ctx.fn_value_of(function).is_some_and(|known| known == *ty) =>
                        {
                            return Some(format!(
                                "the function value `{function}` at line {} has an incompatible signature",
                                span.line
                            ));
                        }
                        crate::ir::lowered::LirRvalue::Closure { id, captures, ty }
                            if (captures.len() > super::OBJECT_FIELD_MASK_CAPACITY
                                || !(ctx.lambda_symbol)(*id)
                                    .and_then(|symbol| ctx.fn_value_of(&symbol))
                                    .is_some_and(|known| known == *ty)) =>
                        {
                            return Some(format!(
                                "a lambda at line {} has an unsupported environment or signature",
                                span.line
                            ));
                        }
                        _ => {}
                    }
                    if let crate::ir::lowered::LirRvalue::IntrinsicCall {
                        intrinsic,
                        receiver_ty,
                        arg_types,
                        result,
                        method,
                        ..
                    } = value
                        && !flat_intrinsic_supported(
                            *intrinsic,
                            receiver_ty,
                            arg_types,
                            result,
                            ctx,
                        )
                    {
                        return Some(format!(
                            "the `{method}` method at line {} uses an unsupported type",
                            span.line
                        ));
                    }
                    if !flat_rvalue_supported(value, &f.locals, ctx)
                        || !value.is_well_typed(&f.locals, *local)
                    {
                        use crate::ir::lowered::LirRvalue as V;
                        let operation = match value {
                            V::PrepareMethod {
                                receiver_ty,
                                method,
                                ..
                            }
                            | V::MethodCall {
                                receiver_ty,
                                method,
                                ..
                            }
                            | V::EnumMethod {
                                receiver_ty,
                                method,
                                ..
                            } => format!("the method `{method}` on a `{}`", type_name(receiver_ty)),
                            V::StaticField { class, field, .. }
                            | V::StaticStore { class, field, .. } => {
                                format!("the static property `{class}::{field}`")
                            }
                            V::ObjectAlloc { class } | V::ConstructorCall { class, .. } => {
                                format!("`new {class}`")
                            }
                            V::FieldLoad {
                                object_ty, field, ..
                            }
                            | V::FieldStore {
                                object_ty, field, ..
                            } => format!("the field `{field}` on a `{}`", type_name(object_ty)),
                            V::StaticCall { class, method, .. } => {
                                format!("the call to `{class}::{method}`")
                            }
                            V::StartTask { callee, .. } => {
                                format!("the task constructor `{callee}`")
                            }
                            _ => "a flat computation".into(),
                        };
                        return Some(format!(
                            "{operation} at line {} has incompatible operands, metadata or destination",
                            span.line
                        ));
                    }
                }
                LirInst::Let {
                    local,
                    name,
                    ty,
                    value,
                    ..
                } => {
                    if !f.locals.get(local.0 as usize).is_some_and(|slot| {
                        slot.name == *name && slot.ty == *ty && !slot.is_gc_owner()
                    }) || !value
                        .ty(&f.locals)
                        .is_some_and(|source| ctx.storable(ty, &source))
                        || matches!(value, crate::ir::lowered::LirOperand::Reference { .. })
                    {
                        return Some(format!(
                            "`let {name}` has incompatible LIR storage or operand"
                        ));
                    }
                }
                LirInst::Assign { local, name, value } => {
                    if !f.locals.get(local.0 as usize).is_some_and(|slot| {
                        slot.name == *name
                            && !slot.is_gc_owner()
                            && value
                                .ty(&f.locals)
                                .is_some_and(|source| ctx.storable(&slot.ty, &source))
                    }) || matches!(value, crate::ir::lowered::LirOperand::Reference { .. })
                    {
                        return Some(format!(
                            "`{name} = ...` has incompatible LIR storage or operand"
                        ));
                    }
                }
                LirInst::Unsupported { span, reason } => {
                    return Some(format!("{reason} at line {}", span.line));
                }
                // Listed rather than caught by `_` so a new instruction has to
                // be given an explicit validation decision here.
                LirInst::EnterDeferScope { .. } | LirInst::LeaveDeferScope { .. } => {}
                LirInst::FlushDefers { .. } => {}
                // Names locals this walker has already decided on, and stores
                // a constant into the slots it gave them (willow-0g8j.3.3).
                LirInst::ClearScopeRoots { .. } => {}
                // Every release has a matching acquisition in the same
                // function, and the decision is made there — on the terminator
                // that carries the type this reads back (willow-0g8j.2.13).
                LirInst::ReleaseLock { .. } => {}
                LirInst::Defer { body, .. } => {
                    let supported = lir_rejection_reason(&body.function, ctx).is_none();
                    if !supported {
                        return Some("it registers an unsupported `defer` body".to_string());
                    }
                }
                // A `match` lowering split into blocks (willow-0g8j.2.11.1).
                // The pattern is vetted here rather than through
                // `supported_expr`, because the arm bodies are already ordinary
                // LIR statements and their binding names are already in `names`
                // as locals of this function.
                LirInst::MatchTest {
                    scrutinee,
                    pattern,
                    span,
                    ..
                }
                | LirInst::MatchBind {
                    scrutinee,
                    pattern,
                    span,
                    ..
                } => {
                    let Some(scrutinee_local) = f
                        .locals
                        .get(scrutinee.0 as usize)
                        .filter(|local| !local.is_gc_owner())
                    else {
                        return Some("a match references an invalid scrutinee local".into());
                    };
                    let scrutinee_ty = &scrutinee_local.ty;
                    match inst {
                        LirInst::MatchTest { result, .. }
                            if !f.locals.get(result.0 as usize).is_some_and(|local| {
                                !local.is_gc_owner() && local.ty == Type::Bool
                            }) =>
                        {
                            return Some("a match test has an invalid boolean destination".into());
                        }
                        LirInst::MatchBind { bindings, .. }
                            if bindings.iter().any(|id| {
                                f.locals
                                    .get(id.0 as usize)
                                    .is_none_or(|local| local.is_gc_owner())
                            }) =>
                        {
                            return Some("a match binding references an invalid local".into());
                        }
                        _ => {}
                    }
                    let mut arm_names = names.clone();
                    if !supported_lir_pattern(pattern, scrutinee_ty, ctx, &mut arm_names) {
                        return Some(format!(
                            "the `match` arm at line {} tests a `{}` with a pattern outside the \
                             walker's subset",
                            span.line,
                            type_name(scrutinee_ty)
                        ));
                    }
                }
                LirInst::SelectInit { .. }
                | LirInst::SelectProbe { .. }
                | LirInst::SelectPick { .. }
                | LirInst::SelectUnregister { .. }
                | LirInst::SelectCommit { .. } => {}
            }
        }
        match &block.terminator {
            Terminator::Branch { cond, .. } => {
                if cond.ty(&f.locals) != Some(Type::Bool)
                    || matches!(cond, crate::ir::lowered::LirOperand::Reference { .. })
                {
                    return Some("a branch has an invalid boolean operand".into());
                }
            }
            Terminator::Return(Some(value)) => {
                if value.ty(&f.locals) == Some(Type::Void) {
                    return Some("the `return` yields a `void` value, which has no slot in the function's signature".into());
                }
                if !value
                    .ty(&f.locals)
                    .is_some_and(|ty| ty != Type::Void && ctx.storable(&f.return_type, &ty))
                    || matches!(value, crate::ir::lowered::LirOperand::Reference { .. })
                {
                    return Some("a return has an incompatible operand or signature".into());
                }
            }
            Terminator::Suspend { .. } if !f.is_async => {
                return Some("a synchronous function contains a suspension edge".to_string());
            }
            // A `lock` critical section (willow-0g8j.2.13). The protected value
            // is loaded into a frame slot on acquisition and committed back on
            // release, so the walker has to be able to represent it. The lock
            // HANDLE needs no check of its own: the lowerer hoisted it into a
            // `let`, which the instruction loop above already vetted.
            Terminator::Suspend {
                operation: SuspendOp::LockAcquire { slots, span },
                ..
            } => {
                let value_ty = &slots.value_ty;
                if !ctx.supported_type(value_ty) {
                    return Some(format!(
                        "the `lock` at line {} protects type `{}`, outside the walker's subset",
                        span.line,
                        type_name(value_ty)
                    ));
                }
            }
            Terminator::Jump(_)
            | Terminator::Suspend { .. }
            | Terminator::Return(None)
            | Terminator::CleanupReturn => {}
        }
    }
    None
}

/// Final LIR cannot embed expression-level suspension trees.
pub(super) fn lir_async_rejection_reason(f: &LirFunction) -> Option<String> {
    f.blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .find_map(|inst| match inst {
            LirInst::Unsupported { span, reason } => {
                Some(format!("{reason} at line {}", span.line))
            }
            _ => None,
        })
}

/// Does this expression suspend the running task where it stands? `await`,
/// `select`, and a channel `send`/`recv` all return control to the scheduler.
/// A lambda body is a separate function, so its contents do not count.
#[cfg(test)]
fn lir_suspends_here(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Await { inner }
            if builtin_types::unary_arg(&inner.ty, B::Future).is_some()
                && !matches!(&inner.kind, HirExprKind::Call { callee, .. } if matches!(callee.unqualified_name(), "sleep" | "yield")) =>
        {
            false
        }
        HirExprKind::Await { .. } | HirExprKind::Select { .. } => true,
        HirExprKind::MethodCall { object, method, .. } => {
            builtin_types::unary_arg(&object.ty, B::Channel).is_some()
                && matches!(method.as_str(), "send" | "recv")
        }
        _ => false,
    }
}

/// Whether a closure environment fits the inline GC reference mask.
///
/// The layout is the class layout: word 0 is not a reference (there, the class
/// descriptor; here, the lifted function's code address) and capture `i` is
/// word `i + 1`. The mask has one bit per word, so the last capture it can
/// describe is number 62 (willow-0g8j.2.12). A wider environment would need a
/// trace function, which the collector does not have yet.
#[cfg(test)]
fn closure_env_representable(captures: &[HirCapture]) -> bool {
    captures.len() <= super::OBJECT_FIELD_MASK_CAPACITY
}

/// Every suspension point inside `expr`, in evaluation order. An `await`'s own
/// operands are walked too: `await f(ch.recv())` holds two.
#[cfg(test)]
fn lir_collect_suspensions<'e>(expr: &'e HirExpr, out: &mut Vec<&'e HirExpr>) {
    let mut pending = vec![expr];
    while let Some(expr) = pending.pop() {
        if lir_suspends_here(expr) {
            out.push(expr);
        }
        if !matches!(expr.kind, HirExprKind::Lambda { .. }) {
            pending.extend(expr.children().into_iter().rev());
        }
    }
}

/// Whether `expr` suspends anywhere inside it.
#[cfg(test)]
fn lir_expr_suspends(expr: &HirExpr) -> bool {
    let mut found = Vec::new();
    lir_collect_suspensions(expr, &mut found);
    !found.is_empty()
}

/// Values the emitter can produce twice with the same result, so evaluating
/// them AFTER a park that the source ran them before is not observable. This is
/// deliberately the same set the former AST suspension normalizer's `bind` refuses to hoist into
/// a temp, which is what keeps that pass and this one evaluating the same
/// things in the same order.
#[cfg(test)]
fn lir_rematerializable(expr: &HirExpr) -> bool {
    matches!(
        expr.kind,
        HirExprKind::Int(_)
            | HirExprKind::Float(_)
            | HirExprKind::Bool(_)
            | HirExprKind::Str(_)
            | HirExprKind::Var(_)
    )
}

/// Forms whose children the emitter may evaluate zero times or on only one
/// path. A suspension underneath one of these would park on a path the source
/// never takes, so they end the search.
#[cfg(test)]
fn lir_evaluates_children_conditionally(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Ternary { .. }
        | HirExprKind::Match { .. }
        | HirExprKind::Select { .. }
        | HirExprKind::Lambda { .. } => true,
        HirExprKind::Binary { op, .. } => matches!(op, BinOp::And | BinOp::Or),
        _ => false,
    }
}

/// Whether `node` still evaluates correctly once `target` is pulled out in
/// front of it. `seen` tracks whether the walk has passed `target` yet: what
/// comes after it is emitted on the resume path and may be anything, while what
/// comes before it has to survive being moved after the park.
#[cfg(test)]
fn lir_hoistable_around(node: &HirExpr, target: &HirExpr, seen: &mut bool) -> bool {
    let Some(path) = node.path_to(target) else {
        return *seen || lir_rematerializable(node);
    };
    let ancestors: HashSet<*const HirExpr> = path.into_iter().map(std::ptr::from_ref).collect();
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if std::ptr::eq(node, target) {
            *seen = true;
            continue;
        }
        if !ancestors.contains(&std::ptr::from_ref(node)) {
            if !*seen && !lir_rematerializable(node) {
                return false;
            }
            continue;
        }
        if lir_evaluates_children_conditionally(node) {
            return false;
        }
        pending.extend(node.children().into_iter().rev());
    }
    true
}

/// An `await` the cooperative LIR path can split into a suspension
/// (willow-0g8j.2.11). Classified once, by the function both eligibility and
/// emission call, so an admitted await is always an emittable one.
#[cfg(test)]
enum LirAwaitSite<'a> {
    /// `await sleep(millis)`: a timer registration, no awaited frame.
    Sleep(&'a HirExpr),
    /// `await yield()`: an unconditional reschedule.
    Yield,
    /// `await f(..)` where `f` is a cooperative leaf. The callee's constructor
    /// runs here and hands back the frame this task waits on.
    LeafCall {
        callee: &'a FunctionId,
        args: &'a [HirExpr],
    },
}

/// The await forms this splitter takes — the ones that are still ordinary
/// expressions when the walker reaches them. `await <task value>` and
/// `await t.result()` are absent because lowering already turned them into a
/// [`Terminator::Suspend`] carrying [`crate::ir::lowered::SuspendOp::AwaitTask`],
/// which delivers its result into a LIR local, so nothing is left inside the
/// expression for this to find.
#[cfg(test)]
fn lir_await_site<'e>(
    expr: &'e HirExpr,
    cooperative_leaves: &std::collections::HashSet<FunctionId>,
) -> Option<LirAwaitSite<'e>> {
    if let Some(builtin) = lir_builtin_await(expr) {
        return Some(builtin);
    }
    let HirExprKind::Await { inner } = &expr.kind else {
        return None;
    };
    let HirExprKind::Call { callee, args } = &inner.kind else {
        return None;
    };
    if !cooperative_leaves.contains(&callee.clone()) {
        return None;
    }
    Some(LirAwaitSite::LeafCall { callee, args })
}

/// Recognise the two scheduler builtins whose await result is `void`. Keeping
/// this structural predicate beside LIR eligibility/emission gives both sides
/// exactly the same accepted shape.
#[cfg(test)]
fn lir_builtin_await(expr: &HirExpr) -> Option<LirAwaitSite<'_>> {
    let HirExprKind::Await { inner } = &expr.kind else {
        return None;
    };
    let HirExprKind::Call { callee, args } = &inner.kind else {
        return None;
    };
    if expr.ty != Type::Void
        || !builtin_types::unary_arg(&inner.ty, B::Future)
            .is_some_and(|output| *output == Type::Void)
    {
        return None;
    }
    match (callee.unqualified_name(), args.as_slice()) {
        ("sleep", [millis]) if millis.ty == Type::I64 => Some(LirAwaitSite::Sleep(millis)),
        ("yield", []) => Some(LirAwaitSite::Yield),
        _ => None,
    }
}

#[cfg(test)]
fn expr_rejection<'e>(
    e: &'e HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'e str, Cow<'e, Type>>,
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
#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn minimal_unsupported_expr<'e>(
    e: &'e HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'e str, Cow<'e, Type>>,
) -> Option<&'e HirExpr> {
    validate_expr(e, ctx, names, true).1
}

/// Diagnose a rejected scope boundary using the bindings visible inside it.
#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn minimal_unsupported_scoped_expr<'e>(
    e: &'e HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'e str, Cow<'e, Type>>,
) -> Option<&'e HirExpr> {
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
                if arm_diverges(arm)
                    && supported_divergent_body(&arm.body, ctx, &arm_names, BodyScope::Bracketed)
                {
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
        _ => {
            for child in e.children() {
                if let Some(rejected) = minimal_unsupported_expr(child, ctx, names) {
                    return Some(rejected);
                }
            }
            Some(e)
        }
    }
}

/// How to name an expression in a LIR validation diagnostic. Deliberately short: the line
/// number locates it, this says what to look for on that line.
#[cfg(test)]
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
        HirExprKind::ReferenceArg { .. } => "a reference argument".to_string(),
        HirExprKind::Range { .. } => "a range".to_string(),
        HirExprKind::Await { .. } => "an `await`".to_string(),
        HirExprKind::TryPropagate { .. } => "a `?` propagation".to_string(),
        HirExprKind::Lambda { .. } => "a lambda".to_string(),
        HirExprKind::Match { scrutinee, .. } => {
            format!("the `match` on a `{}`", type_name(&scrutinee.ty))
        }
        HirExprKind::Select { .. } => "a `select`".to_string(),
    }
}

/// Whether the walker can both TEST `pattern` against a `scrutinee_ty` value
/// and bind whatever it destructures (willow-0g8j.8). On success the pattern's
/// bindings are added to `names`, which is why this takes the map by `&mut`:
/// the arm body is checked against the extended scope, exactly as the emitter
/// will run it. On failure `names` may have been partly extended, so callers
/// pass a throwaway clone rather than their own map.
///
/// `ClassDowncast` tests the boxed object's runtime `type_id`, so it is
/// admitted only for a class the walker has a `type_id` and a layout for
/// (willow-0g8j.2.4).
fn supported_lir_pattern<'n>(
    pattern: &'n crate::ir::lowered::LirPattern,
    scrutinee_ty: &Type,
    ctx: &LirTypeCtx<'_>,
    names: &mut HashMap<&'n str, Cow<'n, Type>>,
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
        if name != TypeId::from_source_name(enum_name) {
            return None;
        }
        def.variant(variant).cloned()
    };
    match pattern {
        crate::ir::lowered::LirPattern::Wildcard => true,
        crate::ir::lowered::LirPattern::Binding { name, ty } => {
            // The binding aliases the whole scrutinee, so it must hold the same
            // machine representation — no widening, no boxing.
            if !ctx.same_repr(ty, scrutinee_ty) || !ctx.supported_type(ty) {
                return false;
            }
            names.insert(name.as_str(), Cow::Borrowed(ty));
            true
        }
        crate::ir::lowered::LirPattern::LiteralBool(_) => *scrutinee_ty == Type::Bool,
        crate::ir::lowered::LirPattern::LiteralInt(_) => *scrutinee_ty == Type::I64,
        crate::ir::lowered::LirPattern::EnumVariant { enum_name, variant } => {
            variant_of(&enum_name.to_string(), variant).is_some_and(|v| v.payloads.is_empty())
        }
        crate::ir::lowered::LirPattern::EnumVariantTuple {
            enum_name,
            variant,
            bindings,
        } => {
            let Some(v) = variant_of(&enum_name.to_string(), variant) else {
                return false;
            };
            // A variant whose payloads are ALL `void` carries no word at all:
            // `enum_instance` normalizes the list away, because there is
            // nothing to store. The source still spells one binding per
            // declared payload — `Ok(done)` on a `Result<void, Cancelled>`,
            // which is what `TaskScope::finish` produces — so the counts differ
            // by exactly that (willow-0g8j.2.13). The emitter zips bindings
            // against the same normalized list and so binds nothing here, which
            // is correct: the name denotes a value that does not exist. An arm
            // body that reads it finds no binding and fails LIR validation
            // rather than loading a word that was never written.
            if !bindings.is_empty()
                && v.payloads.is_empty()
                && bindings.iter().all(|(_, ty)| matches!(ty, Type::Void))
            {
                return true;
            }
            if v.payloads.len() != bindings.len() {
                return false;
            }
            // A payload is LOADED into the binding, so the binding's type has
            // to match the slot's representation rather than merely be
            // storable into it.
            for (slot, (name, ty)) in v.payloads.iter().zip(bindings) {
                if !ctx.same_repr(ty, slot) || !ctx.supported_type(ty) {
                    return false;
                }
                names.insert(name.as_str(), Cow::Borrowed(ty));
            }
            true
        }
        // `Iface(x)` — an interface-to-class downcast. The scrutinee is a box
        // whose word 0 is the concrete object, so the test is an exact
        // `type_id` compare and the binding is that object, unboxed. Exact, not
        // "is a descendant of": a downcast arm selects the arm's class itself,
        // so a descendant of it does not match.
        crate::ir::lowered::LirPattern::ClassDowncast {
            class_name,
            binding,
            binding_ty,
        } => {
            // `Box<i64>` as well as `Shape`: the type arguments name what the
            // interface's methods produce, and a `type_id` compare against
            // word 0 does not read them (willow-0g8j.3).
            let (Type::Named(iface) | Type::Generic(iface, _)) = scrutinee_ty else {
                return false;
            };
            if !(ctx.is_interface)(iface)
                || !matches!(binding_ty, Type::Named(n) if n == class_name)
                || !ctx.supported_class(&class_name.to_string())
                || !ctx.class_type_ids.contains_key(class_name)
            {
                return false;
            }
            names.insert(binding.as_str(), Cow::Borrowed(binding_ty));
            true
        }
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
/// own type for the expression, so a disagreement refuses the body rather
/// than being reinterpreted here.
///
/// `args` are the call's argument types, so arity and the default/message
/// operand are vetted here rather than at two call sites.
/// The return type of a `fn(...) -> R` value. Eligibility has already proved
/// every combinator operand is one, so a non-function here is a compiler bug.
#[cfg(test)]
fn supported_pattern<'n>(
    pattern: &'n HirPattern,
    scrutinee_ty: &Type,
    ctx: &LirTypeCtx<'_>,
    names: &mut HashMap<&'n str, Cow<'n, Type>>,
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
        if name != TypeId::from_source_name(enum_name) {
            return None;
        }
        def.variant(variant).cloned()
    };
    match pattern {
        HirPattern::Wildcard => true,
        HirPattern::Binding { name, ty } => {
            // The binding aliases the whole scrutinee, so it must hold the same
            // machine representation — no widening, no boxing.
            if !ctx.same_repr(ty, scrutinee_ty) || !ctx.supported_type(ty) {
                return false;
            }
            names.insert(name.as_str(), Cow::Borrowed(ty));
            true
        }
        HirPattern::LiteralBool(_) => *scrutinee_ty == Type::Bool,
        HirPattern::LiteralInt(_) => *scrutinee_ty == Type::I64,
        HirPattern::EnumVariant { enum_name, variant } => {
            variant_of(&enum_name.to_string(), variant).is_some_and(|v| v.payloads.is_empty())
        }
        HirPattern::EnumVariantTuple {
            enum_name,
            variant,
            bindings,
        } => {
            let Some(v) = variant_of(&enum_name.to_string(), variant) else {
                return false;
            };
            // A variant whose payloads are ALL `void` carries no word at all:
            // `enum_instance` normalizes the list away, because there is
            // nothing to store. The source still spells one binding per
            // declared payload — `Ok(done)` on a `Result<void, Cancelled>`,
            // which is what `TaskScope::finish` produces — so the counts differ
            // by exactly that (willow-0g8j.2.13). The emitter zips bindings
            // against the same normalized list and so binds nothing here, which
            // is correct: the name denotes a value that does not exist. An arm
            // body that reads it finds no binding and fails LIR validation
            // rather than loading a word that was never written.
            if !bindings.is_empty()
                && v.payloads.is_empty()
                && bindings.iter().all(|(_, ty)| matches!(ty, Type::Void))
            {
                return true;
            }
            if v.payloads.len() != bindings.len() {
                return false;
            }
            // A payload is LOADED into the binding, so the binding's type has
            // to match the slot's representation rather than merely be
            // storable into it.
            for (slot, (name, ty)) in v.payloads.iter().zip(bindings) {
                if !ctx.same_repr(ty, slot) || !ctx.supported_type(ty) {
                    return false;
                }
                names.insert(name.as_str(), Cow::Borrowed(ty));
            }
            true
        }
        // `Iface(x)` — an interface-to-class downcast. The scrutinee is a box
        // whose word 0 is the concrete object, so the test is an exact
        // `type_id` compare and the binding is that object, unboxed. Exact, not
        // "is a descendant of": a downcast arm selects the arm's class itself,
        // so a descendant of it does not match.
        HirPattern::ClassDowncast {
            class_name,
            binding,
            binding_ty,
        } => {
            // `Box<i64>` as well as `Shape`: the type arguments name what the
            // interface's methods produce, and a `type_id` compare against
            // word 0 does not read them (willow-0g8j.3).
            let (Type::Named(iface) | Type::Generic(iface, _)) = scrutinee_ty else {
                return false;
            };
            if !(ctx.is_interface)(iface)
                || !matches!(binding_ty, Type::Named(n) if n == class_name)
                || !ctx.supported_class(&class_name.to_string())
                || !ctx.class_type_ids.contains_key(class_name)
            {
                return false;
            }
            names.insert(binding.as_str(), Cow::Borrowed(binding_ty));
            true
        }
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
/// own type for the expression, so a disagreement refuses the body rather
/// than being reinterpreted here.
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
#[cfg(test)]
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
#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn supported_panic<'e>(
    e: &'e HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'e str, Cow<'e, Type>>,
) -> bool {
    let HirExprKind::Call { callee, args } = &e.kind else {
        return false;
    };
    // A local binding of the same name is an ordinary indirect call, not the
    // builtin, and it wins here exactly as it does in `supported_expr`.
    if !callee.is_free_named("panic")
        || e.ty != Type::Never
        || names.contains_key(callee.unqualified_name())
    {
        return false;
    }
    // The three accepted message shapes: none (emission substitutes a
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
#[cfg(test)]
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

/// The result type of a `Task<T>`/`JoinHandle<T>` cancellation method, or
/// `None` when this receiver and method are not one (willow-0g8j.2.11).
///
/// Matched by INTRINSIC rather than by name, for the same reason
/// [`scalar_to_string`] is: a future `Task` builtin spelled `cancel` must not
/// silently inherit this lowering.
#[cfg(test)]
fn task_handle_method(recv: &Type, method: &str, arity: usize) -> Option<Type> {
    let resolved = intrinsics::resolve(recv, method, arity)?;
    matches!(
        resolved.intrinsic,
        Intrinsic::TaskCancel | Intrinsic::TaskIsCancelled | Intrinsic::TaskResult
    )
    .then(|| resolved.return_type(|_| None))
}

/// One of the two atomic cells (willow-0g8j.2.13), and the width every operand
/// and result of its methods has.
///
/// `AtomicI64` and `AtomicBool` share one runtime shape — a GC-allocated word
/// the runtime reads and writes atomically — and differ only in that width and
/// in the arithmetic `AtomicBool` does not have. Carrying the distinction as a
/// value keeps the emitter from re-deriving it from the class spelling.
#[derive(Clone, Copy)]
struct AtomicCell {
    is_i64: bool,
}

impl AtomicCell {
    /// The type of the cell's word: the type of every operand it takes and of
    /// every non-`void` result it produces.
    fn word(self) -> Type {
        if self.is_i64 { Type::I64 } else { Type::Bool }
    }

    /// The runtime symbol suffix — `willow_atomic_{suffix}_{op}`.
    fn suffix(self) -> &'static str {
        if self.is_i64 { "i64" } else { "bool" }
    }

    /// The prelude name this cell is constructed through.
    fn class_name(self) -> &'static str {
        if self.is_i64 {
            "AtomicI64"
        } else {
            "AtomicBool"
        }
    }
}

/// Whether `ty` is an atomic cell, resolved through [`builtin_types`] rather
/// than by name: those two names are owned by the prelude, so a user class
/// cannot take them, and going through the resolver is what makes that true
/// here as well as in `intrinsics::resolve`.
fn atomic_cell(ty: &Type) -> Option<AtomicCell> {
    let resolved = builtin_types::resolve(ty)?;
    if !resolved.args.is_empty() {
        return None;
    }
    match resolved.id {
        B::AtomicI64 => Some(AtomicCell { is_i64: true }),
        B::AtomicBool => Some(AtomicCell { is_i64: false }),
        _ => None,
    }
}

/// The intrinsic and result type of an atomic cell's method, or `None` when
/// this receiver and method are not one (willow-0g8j.2.13).
///
/// Matched by INTRINSIC for the same reason [`task_handle_method`] is, and that
/// is also what keeps `AtomicBool::add` out of the walker: the resolver never
/// produces an arithmetic intrinsic for a bool cell, so the emitter never has
/// to know which operations each width implements.
#[cfg(test)]
fn atomic_method(recv: &Type, method: &str, arity: usize) -> Option<(Intrinsic, Type)> {
    let resolved = intrinsics::resolve(recv, method, arity)?;
    matches!(
        resolved.intrinsic,
        Intrinsic::AtomicLoad
            | Intrinsic::AtomicStore
            | Intrinsic::AtomicSwap
            | Intrinsic::AtomicAdd
            | Intrinsic::AtomicSub
    )
    .then(|| (resolved.intrinsic, resolved.return_type(|_| None)))
}

/// One of the two cancellation handles (willow-0g8j.2.13): a `CancellationToken`
/// or a `TaskScope`.
///
/// The two expose the same four operations over different runtime objects, so
/// what differs is the runtime symbol PREFIX rather than the operation. A scope
/// additionally has `finish`.
#[derive(Clone, Copy)]
struct CancelHandle {
    is_token: bool,
}

impl CancelHandle {
    /// The runtime symbol prefix — `{prefix}_{op}`.
    fn prefix(self) -> &'static str {
        if self.is_token {
            "willow_cancellation_token"
        } else {
            "willow_task_scope"
        }
    }

    /// The prelude name this handle is constructed through.
    fn class_name(self) -> &'static str {
        if self.is_token {
            "CancellationToken"
        } else {
            "TaskScope"
        }
    }
}

/// Whether `ty` is a cancellation handle, resolved through [`builtin_types`]
/// rather than by name, for the same reason [`atomic_cell`] is: the prelude owns
/// both names, and going through the resolver is what makes that true here as
/// well as in `intrinsics::resolve`.
fn cancellation_handle(ty: &Type) -> Option<CancelHandle> {
    let resolved = builtin_types::resolve(ty)?;
    if !resolved.args.is_empty() {
        return None;
    }
    match resolved.id {
        B::CancellationToken => Some(CancelHandle { is_token: true }),
        B::TaskScope => Some(CancelHandle { is_token: false }),
        _ => None,
    }
}

/// One of the two native-blocking cells (willow-0g8j.2.13): a `BlockingCell<T>`
/// or a `BlockingRwCell<T>`.
///
/// Unlike `Mutex<T>` and `RwLock<T>`, these two are single-operation cells with
/// no critical section: each accessor is one runtime call that blocks the OS
/// thread on contention rather than parking the task, which is why they are
/// callable from a synchronous function. What differs between them is the
/// runtime symbol PREFIX and the method names, and the method names come from
/// the resolver rather than from here.
#[derive(Clone, Copy)]
struct BlockingCellKind {
    is_rw: bool,
}

impl BlockingCellKind {
    /// The runtime symbol prefix — `{prefix}_{op}`.
    fn prefix(self) -> &'static str {
        if self.is_rw {
            "willow_blocking_rw_cell"
        } else {
            "willow_blocking_cell"
        }
    }

    /// The prelude name this cell is constructed through.
    fn class_name(self) -> &'static str {
        if self.is_rw {
            "BlockingRwCell"
        } else {
            "BlockingCell"
        }
    }
}

/// Whether `ty` is a blocking cell, and over which element type — resolved
/// through [`builtin_types`] rather than by name, for the same reason
/// [`atomic_cell`] is: the prelude owns both names.
///
/// The element type is returned because the cell's ABI is word-based: the
/// emitter has to coerce INTO a word on the way in and back OUT of one on the
/// way out, and only the element type says how.
fn blocking_cell(ty: &Type) -> Option<(BlockingCellKind, &Type)> {
    let resolved = builtin_types::resolve(ty)?;
    let [elem] = resolved.args else {
        return None;
    };
    match resolved.id {
        B::BlockingCell => Some((BlockingCellKind { is_rw: false }, elem)),
        B::BlockingRwCell => Some((BlockingCellKind { is_rw: true }, elem)),
        _ => None,
    }
}

/// Whether `ty` is a scheduler-aware lock, and over which protected type
/// (willow-0g8j.2.13).
///
/// Matched by NAME rather than through [`builtin_types`], because these two are
/// not builtin type ids: the checker keys `lock`/`read`/`write` on exactly these
/// two spellings too ([`crate::parser::ast::LockMode::lock_type_name`]).
///
/// The `&'static str` is the runtime symbol PREFIX, which is what distinguishes
/// the two state machines: `Mutex<T>` has one owner, `RwLock<T>` has readers and
/// a writer, and their acquire/poll/load/release entry points are separate.
fn scheduler_lock(ty: &Type) -> Option<(&'static str, &Type)> {
    let Type::Generic(name, args) = ty else {
        return None;
    };
    let [protected] = args.as_slice() else {
        return None;
    };
    match name.name() {
        "Mutex" => Some(("willow_async_mutex", protected)),
        "RwLock" => Some(("willow_async_rwlock", protected)),
        _ => None,
    }
}

/// The intrinsic and result type of a blocking cell's method, or `None` when
/// this receiver and method are not one (willow-0g8j.2.13).
///
/// Matched by INTRINSIC, like [`atomic_method`], and that is what keeps the two
/// cells' method names apart without the walker knowing them: the resolver
/// never answers `get`/`set` for a `BlockingRwCell` or `read`/`write` for a
/// `BlockingCell`.
#[cfg(test)]
fn blocking_cell_method(recv: &Type, method: &str, arity: usize) -> Option<(Intrinsic, Type)> {
    let resolved = intrinsics::resolve(recv, method, arity)?;
    matches!(
        resolved.intrinsic,
        Intrinsic::CellGet | Intrinsic::CellSet | Intrinsic::RwCellRead | Intrinsic::RwCellWrite
    )
    .then(|| (resolved.intrinsic, resolved.return_type(|_| None)))
}

/// The intrinsic and result type of a cancellation handle's method, or `None`
/// when this receiver and method are not one (willow-0g8j.2.13).
///
/// Matched by INTRINSIC, like [`atomic_method`], and that is also what keeps
/// `CancellationToken::finish` out of the walker for free: the resolver never
/// produces `ScopeFinish` for a token.
///
/// `attach` and `add` hand back the very task they were given, so their result
/// type is their argument's — `args` is what supplies it. Passing a closure that
/// answers `None` would silently type them `Task<void>`.
#[cfg(test)]
fn cancellation_method(recv: &Type, method: &str, args: &[HirExpr]) -> Option<(Intrinsic, Type)> {
    let resolved = intrinsics::resolve(recv, method, args.len())?;
    matches!(
        resolved.intrinsic,
        Intrinsic::TokenIsCancelled
            | Intrinsic::TokenCancel
            | Intrinsic::TokenChild
            | Intrinsic::TokenAttach
            | Intrinsic::ScopeIsCancelled
            | Intrinsic::ScopeCancel
            | Intrinsic::ScopeChild
            | Intrinsic::ScopeAdd
            | Intrinsic::ScopeFinish
    )
    .then(|| {
        let ret = resolved.return_type(|i| args.get(i).map(|a: &HirExpr| a.ty.clone()));
        (resolved.intrinsic, ret)
    })
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
    // exactly one value of the payload type. Emission passes both
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
    let option_of = |t: Type| Type::Generic("Option".to_string().into(), vec![t]);
    let result_of = |ok: Type, err: Type| Type::Generic("Result".to_string().into(), vec![ok, err]);
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
            // `Result<T, E>::and_then(fn(T) -> Result<U, E>) -> Result<U, E>`.
            // The result is the CALLABLE's return type, which is also what the
            // checker records: the `Err` arm passes the receiver through, and
            // that is representation-safe because every `Result` is the same
            // two-word box whatever its type arguments.
            //
            // NEITHER error type is constrained, the receiver's included. A
            // `Result::Ok(5)` written with nothing to infer `E` from records
            // `Result<i64, void>`, and requiring a non-void one here took the
            // whole enclosing function out of LIR for a receiver
            // whose error type the emitter never reads
            // (`emit_result_and_then` takes only the ok type) — willow-0g8j.3.
            "and_then" => {
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

/// Where a `match` sits, which decides whether it may be one that leaves.
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
enum MatchPosition {
    /// An operand: something after the `match` reads the value it hands to its
    /// merge block, so some path through it has to reach that block.
    Operand,
    /// A whole statement, or the tail of a body — nothing follows it in the
    /// same Cranelift block, so a `match` every arm of which leaves is fine.
    Divergent,
}

/// Whether every path through a `match` with these arms leaves, making its
/// merge block unreachable.
///
/// The checker's own type for the `match` is not the test. It types one `!`
/// where the surrounding code needs that, but the same all-arms-leave `match`
/// written as a function body's ending is typed by its arms instead, and the
/// walker used to refuse that shape (willow-0g8j.2.16).
#[cfg(test)]
fn match_diverges(arms: &[HirMatchArm]) -> bool {
    !arms.is_empty() && arms.iter().all(arm_diverges)
}

/// The `match` rule, shared by the value form (through [`supported_expr`]) and
/// the diverging form (through [`supported_divergent_expr`]).
///
/// `e` is the match expression itself; its type is what every arm that reaches
/// the merge block must produce.
#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn supported_match<'n>(
    e: &'n HirExpr,
    scrutinee: &'n HirExpr,
    arms: &'n [HirMatchArm],
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    position: MatchPosition,
) -> bool {
    // An arm-less match has no value to produce and the checker should
    // have rejected it; refusing here keeps the emitter's "seed the
    // result variable, then every arm overwrites it" invariant honest.
    if arms.is_empty() || !supported_expr(scrutinee, ctx, names) {
        return false;
    }
    // The scrutinee shapes the walker can test. A `String` or class scrutinee
    // needs a content comparison the pattern emitter below does not have.
    let scrutinee_ok = match &scrutinee.ty {
        Type::I64 | Type::Bool => true,
        // An interface box, matched by class downcast patterns
        // (willow-0g8j.2.4). Checked before the enum test because an interface
        // is also spelled `Type::Named`; the two sets cannot overlap. A GENERIC
        // interface instantiation — `Box<i64>` — is one too (willow-0g8j.3):
        // the box has the same two words whatever the type arguments say, and
        // the downcast reads word 0's `type_id` without consulting them.
        Type::Named(n) | Type::Generic(n, _) if (ctx.is_interface)(n) => true,
        // Any enum instance, generic or not: `Color`, `Option<i64>`,
        // `Result<i64, String>` (willow-0g8j.2.1).
        Type::Named(_) | Type::Generic(..) => ctx.supported_enum_type(&scrutinee.ty),
        _ => false,
    };
    if !scrutinee_ok {
        return false;
    }
    // A match that produces a value needs at least one arm that reaches the
    // merge block, where the value is read. One all of whose arms leave hands
    // nothing to anything, so it is admissible only in the positions
    // [`supported_divergent_expr`] is asked about.
    if position == MatchPosition::Operand && match_diverges(arms) {
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
            return supported_divergent_body(&arm.body, ctx, &arm_names, BodyScope::Bracketed);
        }
        if e.ty == Type::Void {
            return supported_effect_body(&arm.body, ctx, &arm_names, BodyScope::Bracketed);
        }
        // An arm that produces a value is a single expression. Not a walker
        // limit — the grammar's: a block-bodied arm's last statement needs a
        // `;` (E0101), so such an arm is typed `void` and is checked above.
        let [HirStmt::Expr(value)] = arm.body.as_slice() else {
            return false;
        };
        ctx.same_repr(&e.ty, &value.ty) && supported_expr(value, ctx, &arm_names)
    })
}

/// Which binding forms a body may contain. This is a property of the EMITTER
/// that will run the body, not of the statements themselves (willow-0g8j.2.13).
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
enum BodyScope {
    /// A `match` arm or a `select` case. The emitter snapshots `vars` and the
    /// GC root depth before the body and restores them after, so a `let` is
    /// admitted: its rooted slot is popped when the body ends, on the ordinary
    /// path by the bracket and on a `return` by [`FuncGen::emit_lir_return`]'s
    /// full-depth pop.
    Bracketed,
}

/// One statement of a body the walker runs for its effect.
///
/// Assignment and `let` join plain expressions here because a `match` used as a
/// statement is how the source spells a multi-way store — the `defer match
/// recover() { ... }` of `example/panic_recover_service.wi` is one
/// (willow-0g8j.2.13). On success a `let`'s name is added to `names`, so the
/// rest of the body is checked against the scope the emitter will actually run.
///
/// `if` is here for a different reason: LIR lowers a function-level `if` into
/// blocks of the graph, but these bodies stay in HIR shape, so the branches of
/// an `if` inside one have nowhere else to be decided (willow-0g8j.2.16).
///
/// Suspension is refused for both binding forms. An arm body has no cooperative
/// await split of its own, so an `await` in an initialiser would reach the
/// emitter unhoisted; and a binding held in a Cranelift variable does not
/// survive a park. The enclosing statement's own check
/// ([`lir_async_rejection_reason`]) already rejects a suspending `match` arm,
/// but it does not look inside a `defer` body, so the rule is stated here where
/// both are covered.
#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn supported_body_stmt<'n>(
    stmt: &'n HirStmt,
    ctx: &LirTypeCtx<'_>,
    names: &mut HashMap<&'n str, Cow<'n, Type>>,
    scope: BodyScope,
) -> bool {
    match stmt {
        HirStmt::Expr(e) => supported_expr(e, ctx, names),
        // The DECLARED type of the target decides the store, exactly as it does
        // for `LirInst::Assign`, so `a = new Dog();` on an `Animal` local boxes.
        // A name that is not in scope here is one the walker cannot resolve — a
        // bare enum variant, a function used as a value — not a missing binding.
        HirStmt::Assign { name, value, .. } => {
            let Some(declared) = names.get(name.as_str()).cloned() else {
                return false;
            };
            !lir_expr_suspends(value)
                && ctx.storable(&declared, &value.ty)
                && supported_expr(value, ctx, names)
        }
        // The same checked heap store as a function-level
        // `LirInst::FieldAssign`. Bracketed HIR islands can mutate an object
        // through the same LIR emission path as ordinary statements
        // (willow-wene). Deferred bodies use the same checked heap store.
        HirStmt::IndexAssign {
            array,
            index,
            value,
            ..
        } => {
            let Type::Array(elem) = &array.ty else {
                return false;
            };
            ctx.storable(elem, &value.ty)
                && index.ty == Type::I64
                && [array, index, value]
                    .iter()
                    .all(|e| !lir_expr_suspends(e) && supported_expr(e, ctx, names))
        }
        HirStmt::FieldAssign {
            object,
            field,
            value,
            ..
        } => {
            let Some(field_ty) = ctx
                .class_layout_of(&object.ty)
                .and_then(|layout| layout.iter().find(|(name, _)| name == field))
                .map(|(_, ty)| ty)
            else {
                return false;
            };
            !lir_expr_suspends(object)
                && !lir_expr_suspends(value)
                && ctx.storable(field_ty, &value.ty)
                && supported_expr(object, ctx, names)
                && supported_expr(value, ctx, names)
        }
        // `Class::prop = value;` inside a body the walker keeps as an HIR
        // island — a `defer` block, a `match` arm, a `select` case
        // (willow-0g8j.15). Vetted exactly as the function-level
        // `LirInst::StaticFieldAssign` is, and admitted in EITHER scope: the
        // store's destination is a data segment, so unlike a `let` it needs no
        // storage of its own, which is the only thing a deferred body cannot
        // give a statement.
        HirStmt::StaticFieldAssign {
            class,
            field,
            value,
            ..
        } => {
            // `Self::prop` resolves against the enclosing class, as
            // `emit_lir_static_field_assign` resolves it (willow-0g8j.13).
            let Some(field_ty) = (ctx.static_field)(ctx.resolved_class(&class.to_string()), field)
            else {
                return false;
            };
            !lir_expr_suspends(value)
                && ctx.supported_type(&field_ty)
                && ctx.storable(&field_ty, &value.ty)
                && supported_expr(value, ctx, names)
        }
        HirStmt::Let {
            name, ty, value, ..
        } => {
            if lir_expr_suspends(value)
                || !ctx.supported_type(ty)
                || !ctx.storable(ty, &value.ty)
                || !supported_expr(value, ctx, names)
            {
                return false;
            }
            // Shadowing is not a concern: HIR lowering renames a binding that
            // shadows another in the same function (`LowerCtx::bind`), so this
            // name is unique function-wide and cannot displace an outer one.
            names.insert(name.as_str(), Cow::Borrowed(ty));
            true
        }
        // A body kept as an HIR island gets its branches here rather than from
        // the LIR block graph, which only covers function-level control flow
        // (willow-0g8j.2.16). A binding made inside a branch is scoped to that
        // branch, so `names` is not extended from it.
        HirStmt::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            cond.ty == Type::Bool
                && !lir_expr_suspends(cond)
                && supported_expr(cond, ctx, names)
                && supported_branch_body(then_branch, ctx, names, scope)
                && else_branch
                    .as_deref()
                    .is_none_or(|body| supported_branch_body(body, ctx, names, scope))
        }
        // A `while` inside an island body, for the same reason an `if` is here:
        // the LIR block graph only covers function-level control flow, so this
        // loop has nowhere else to be decided (willow-0g8j.3). Its body is
        // checked as a plain run of effect statements — a `break`, a `continue`
        // or a `return` out of it is not admitted, which is what lets the
        // emitter give the loop two edges and no loop context.
        HirStmt::While { cond, body, .. } => {
            cond.ty == Type::Bool
                && !lir_expr_suspends(cond)
                && supported_expr(cond, ctx, names)
                && supported_effect_body(body, ctx, names, scope)
        }
        // A `for` in an island is desugared by its emitter just like the main
        // HIR -> LIR lowering desugars a function-level loop: the iterable is
        // evaluated once, then a range advances its value or an array advances
        // an index. `break`/`continue`/`return` remain excluded by the body's
        // effect-only check, so no island loop context is required.
        HirStmt::For {
            name,
            iterable,
            body,
            ..
        } => {
            let element = match &iterable.ty {
                Type::Array(element) => Some(element.as_ref()),
                ty if range_i64(ty) => Some(&Type::I64),
                _ => None,
            };
            let Some(element) = element else {
                return false;
            };
            if lir_expr_suspends(iterable) || !supported_expr(iterable, ctx, names) {
                return false;
            }
            let mut body_names = names.clone();
            body_names.insert(name.as_str(), Cow::Borrowed(element));
            supported_effect_body(body, ctx, &body_names, scope)
        }
        _ => false,
    }
}

/// One branch of an `if` inside a body: a run of effect statements that either
/// falls through to whatever follows the `if` or leaves the function.
///
/// The emitter gives each branch a Cranelift block of its own and jumps to the
/// join only from the branches that did not terminate, so the two endings need
/// no agreement with each other.
#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn supported_branch_body<'n>(
    body: &'n [HirStmt],
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    scope: BodyScope,
) -> bool {
    if supported_effect_body(body, ctx, names, scope) {
        return true;
    }
    supported_divergent_body(body, ctx, names, scope)
}

/// Whether the walker can emit `body` for its EFFECT: nothing reads a value
/// from it. A divergent expression ends emission, so statements after it do
/// not need an emission path (including dead statements in a deferred body).
///
/// The map is cloned rather than borrowed mutably because a `let` here is
/// scoped to this body; the caller's own scope must not gain the name.
#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn supported_effect_body<'n>(
    body: &'n [HirStmt],
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    scope: BodyScope,
) -> bool {
    let mut names = names.clone();
    for stmt in body {
        if let HirStmt::Expr(expr) = stmt
            && supported_divergent_expr(expr, ctx, &names)
        {
            return true;
        }
        if !supported_body_stmt(stmt, ctx, &mut names, scope) {
            return false;
        }
    }
    true
}

#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn supported_select<'n>(
    cases: &'n [HirSelectCase],
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
) -> bool {
    !cases.is_empty()
        && cases.iter().all(|case| {
            let mut case_names = names.clone();
            let kind_ok = match &case.kind {
                HirSelectCaseKind::Recv { binding, channel } => {
                    let Some(elem) = channel_element_type_ref(&channel.ty) else {
                        return false;
                    };
                    if binding != "_" {
                        case_names.insert(binding.as_str(), Cow::Borrowed(elem));
                    }
                    supported_expr(channel, ctx, names)
                }
                HirSelectCaseKind::Send { channel, value } => channel_element_type_ref(&channel.ty)
                    .is_some_and(|elem| {
                        ctx.storable(elem, &value.ty)
                            && supported_expr(channel, ctx, names)
                            && supported_expr(value, ctx, names)
                    }),
                HirSelectCaseKind::Timeout { millis } => {
                    millis.ty == Type::I64 && supported_expr(millis, ctx, names)
                }
                HirSelectCaseKind::Join { binding, task } => {
                    let Some(bound) = await_output_type(&task.ty) else {
                        return false;
                    };
                    // `await t.result()` binds `Result<T, Cancelled>`, a type no
                    // HIR node in this case holds, so the map owns its values.
                    if !ctx.supported_type(&bound) {
                        return false;
                    }
                    if binding != "_" && bound != Type::Void {
                        case_names.insert(binding.as_str(), Cow::Owned(bound));
                    }
                    supported_expr(task, ctx, names)
                }
                HirSelectCaseKind::Default => true,
            };
            // A case body may also LEAVE -- `... => { return "sent"; }` -- which
            // is what [`supported_branch_body`] adds over the plain effect body
            // (willow-0g8j.3). The select emitter already brackets each case and
            // only jumps to its merge from the ones that fall through, so a
            // returning case needs nothing more from it.
            kind_ok && supported_branch_body(&case.body, ctx, &case_names, BodyScope::Bracketed)
        })
}

#[cfg(test)]
fn channel_element_type_ref(ty: &Type) -> Option<&Type> {
    builtin_types::unary_arg(ty, B::Channel)
}

/// This arm ends by leaving the function or by unwinding, so it hands no value
/// to the merge block.
#[cfg(test)]
fn arm_diverges(arm: &HirMatchArm) -> bool {
    body_diverges(&arm.body)
}

/// Whether every path through `body` leaves.
///
/// A `return` is one syntactically and `!` is the type of every tail expression
/// that unwinds, but neither test sees an `if` both of whose branches leave —
/// the `if` itself is typed nothing, and the statement after it is unreachable
/// rather than absent. Reading that shape is what lets an arm guard a path with
/// an early `return` and still count as leaving (willow-0g8j.2.16).
#[cfg(test)]
fn body_diverges(body: &[HirStmt]) -> bool {
    let mut pending = vec![body];
    while let Some(body) = pending.pop() {
        match body.last() {
            Some(HirStmt::Return { .. }) => {}
            Some(HirStmt::Expr(e)) if e.ty == Type::Never => {}
            Some(HirStmt::Expr(HirExpr {
                kind: HirExprKind::Match { arms, .. },
                ..
            })) if !arms.is_empty() => {
                pending.extend(arms.iter().rev().map(|arm| arm.body.as_slice()));
            }
            Some(HirStmt::If {
                then_branch,
                else_branch: Some(else_branch),
                ..
            }) => {
                pending.push(else_branch);
                pending.push(then_branch);
            }
            _ => return false,
        }
    }
    true
}

/// Whether the walker can emit `body` where nothing follows it in the same
/// Cranelift block: a run of ordinary effect statements ending in something
/// that leaves — `return`, `panic(...)`, or a `match` all of whose arms do
/// (willow-0g8j.2.5).
#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn supported_divergent_body<'n>(
    body: &'n [HirStmt],
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    scope: BodyScope,
) -> bool {
    let Some((last, leading)) = body.split_last() else {
        return false;
    };
    // Only effect statements may precede the tail. One of them may still end a
    // path — an `if` whose branch returns — but never every path: `supported_expr`
    // refuses a `!`-typed expression, so no leading statement diverges outright.
    let mut names = names.clone();
    let names = &mut names;
    let leading_ok = leading
        .iter()
        .all(|s| supported_body_stmt(s, ctx, names, scope));
    let last_ok = match last {
        HirStmt::Return { value: None, .. } => true,
        HirStmt::Return {
            value: Some(value), ..
        } => ctx.storable(ctx.return_type, &value.ty) && supported_expr(value, ctx, names),
        HirStmt::Expr(e) => supported_divergent_expr(e, ctx, names),
        // An `if` both of whose branches leave. Its own rule lives in
        // [`supported_body_stmt`]; what this arm adds is that such an `if` may
        // be a body's ending, with nothing after it to fall through to.
        HirStmt::If { .. } if body_diverges(std::slice::from_ref(last)) => {
            supported_body_stmt(last, ctx, names, scope)
        }
        _ => false,
    };
    leading_ok && last_ok
}

/// An expression the walker emits for its EFFECT in a position where nothing
/// follows it in the same Cranelift block — a whole statement, or the tail of a
/// match arm. Both forms end the block, which is why the position matters:
/// nested in an operand, either would strand the instructions that consume its
/// value after a terminator.
#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn supported_divergent_expr<'n>(
    e: &'n HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
) -> bool {
    if supported_panic(e, ctx, names) {
        return true;
    }
    match &e.kind {
        HirExprKind::Match { scrutinee, arms } if e.ty == Type::Never || match_diverges(arms) => {
            supported_match(e, scrutinee, arms, ctx, names, MatchPosition::Divergent)
        }
        _ => false,
    }
}

/// `'n` ties the borrowed names to the expression tree being vetted, so a
/// `match` arm can extend the map with its own pattern bindings — which live in
/// that same tree — before checking the arm body (willow-0g8j.8).
#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn supported_expr<'n>(
    e: &'n HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
) -> bool {
    validate_expr(e, ctx, names, false).0
}

/// Cache results by node address for this borrowed tree and this lexical scope.
/// A rule may inspect a grandchild (await constructors and reference places),
/// or deliberately ignore a structural child (namespace receivers). Evaluating
/// the original rules against cached results preserves both cases; blindly
/// requiring every structural child to pass would change the accepted subset.
#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn validate_expr<'n>(
    e: &'n HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    diagnose: bool,
) -> (bool, Option<&'n HirExpr>) {
    let mut pending = vec![(e, false)];
    let mut results: HashMap<*const HirExpr, (bool, Option<&HirExpr>)> = HashMap::new();
    while let Some((expr, visited)) = pending.pop() {
        let scoped = matches!(
            expr.kind,
            HirExprKind::Lambda { .. } | HirExprKind::Match { .. } | HirExprKind::Select { .. }
        );
        if !visited && !scoped {
            pending.push((expr, true));
            pending.extend(
                expr.children()
                    .into_iter()
                    .rev()
                    .map(|child| (child, false)),
            );
            continue;
        }
        let accepted = supported_expr_node(expr, ctx, names, &|child| {
            results[&(child as *const HirExpr)].0
        });
        let rejection = if !diagnose || accepted {
            None
        } else if scoped {
            minimal_unsupported_scoped_expr(expr, ctx, names)
        } else {
            expr.children()
                .into_iter()
                .find_map(|child| results[&(child as *const HirExpr)].1)
                .or(Some(expr))
        };
        results.insert(expr, (accepted, rejection));
    }
    results[&(e as *const HirExpr)]
}

#[willow_continuations::function(
    minimal_unsupported_expr,
    minimal_unsupported_scoped_expr,
    supported_body_stmt,
    supported_branch_body,
    supported_divergent_body,
    supported_divergent_expr,
    supported_effect_body,
    supported_expr,
    supported_expr_node,
    supported_match,
    supported_panic,
    supported_select,
    validate_expr
)]
#[cfg(test)]
fn supported_expr_node<'n>(
    e: &'n HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    child_supported: &impl Fn(&'n HirExpr) -> bool,
) -> bool {
    if !ctx.supported_type(&e.ty) && !is_fresh_empty_map(e) {
        return false;
    }
    match &e.kind {
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) => true,
        HirExprKind::Str(_) => true,
        HirExprKind::Var(name) => names
            .get(name.as_str())
            .is_some_and(|bound| ctx.same_repr(bound, &e.ty)),
        HirExprKind::ReferenceArg { place } => {
            ctx.same_repr(&place.ty, &e.ty)
                && supported_reference_place(place, ctx, names, child_supported)
        }
        // A named function used as a value (willow-0g8j.2.2). The declared
        // signature must be the type this expression carries: the pointer is
        // later CALLED through a Cranelift signature built from `e.ty`, so a
        // disagreement would call the target under the wrong ABI.
        HirExprKind::FnRef(name) => ctx.fn_value_of(name).is_some_and(|ty| ty == e.ty),
        // A lambda evaluates to its lifted function — the address itself for a
        // `fn`, an environment object carrying that address for a `closure`. Its
        // BODY is not this function's problem: that is compiled under its own
        // symbol and vetted on its own terms. What is checked here is the same
        // thing as for a named function — the symbol exists and its declared
        // signature is the type the expression carries — plus, for a closure,
        // that every value the environment is built from is a name THIS
        // function can resolve and a word the walker can copy.
        HirExprKind::Lambda { id, captures, .. } => {
            (ctx.lambda_symbol)(*id)
                .and_then(|sym| ctx.fn_value_of(&sym))
                .is_some_and(|ty| ty == e.ty)
                && closure_env_representable(captures)
                && captures.iter().all(|c| {
                    names
                        .get(c.source.as_str())
                        .is_some_and(|bound| ctx.repr_compatible(&c.ty, bound))
                })
        }
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
            // identity comparison the walker does not emit. A payload-free enum
            // is the exception — the value IS its tag, so `==` and `!=` are the
            // integer comparison below and mean what the program wrote; both
            // sides must be the same enum, which is also what lets an aliased
            // spelling be compared with the canonical one (willow-0g8j.3).
            if matches!(lhs.ty, Type::Named(_)) || matches!(rhs.ty, Type::Named(_)) {
                let compares_tags = matches!(op, BinOp::Eq | BinOp::Ne)
                    && ctx.tag_immediate_enum(&lhs.ty)
                    && ctx.tag_immediate_enum(&rhs.ty)
                    && ctx.same_repr(&lhs.ty, &rhs.ty);
                if !compares_tags {
                    return false;
                }
            }
            child_supported(lhs) && child_supported(rhs)
        }
        // Both arms feed one Cranelift variable, and the walker inserts no
        // conversion between them — a `cond ? new Dog() : new Cat()` typed
        // `Animal` would define that variable with two raw class pointers.
        HirExprKind::Ternary {
            condition,
            then_expr,
            else_expr,
        } => {
            ctx.same_repr(&e.ty, &then_expr.ty)
                && ctx.same_repr(&e.ty, &else_expr.ty)
                && child_supported(condition)
                && child_supported(then_expr)
                && child_supported(else_expr)
        }
        // `match` as an expression (willow-0g8j.8). Every arm feeds one
        // Cranelift variable, so the same no-conversion rule `Ternary` states
        // applies to each arm body.
        HirExprKind::Match { scrutinee, arms } => {
            supported_match(e, scrutinee, arms, ctx, names, MatchPosition::Operand)
        }
        HirExprKind::Unary { operand, .. } => child_supported(operand),
        HirExprKind::Call { callee, args } => {
            // HIR spells direct and indirect calls with the same node, and a
            // local fn-typed binding shadows a free function (willow-bv9.1).
            // The local wins here because this must resolve to what the type checker
            // checked the call against.
            if let Some(local) = names.get(callee.unqualified_name()) {
                // Either callable value: a `closure` is called through the code
                // pointer in its word 0, with the object itself as the hidden
                // leading argument, so the only difference from a `fn` is in
                // the emitter (willow-0g8j.2.12).
                let (Type::Fn(params, ret) | Type::Closure(params, ret)) = local.as_ref() else {
                    return false;
                };
                return ctx.same_repr(ret, &e.ty)
                    && params.len() == args.len()
                    && args
                        .iter()
                        .all(|arg| !matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
                    && params
                        .iter()
                        .zip(args)
                        .all(|(p, a)| ctx.supported_type(p) && ctx.storable(p, &a.ty))
                    && args.iter().all(child_supported);
            }
            if matches!(callee.unqualified_name(), "sleep" | "yield") {
                return builtin_types::unary_arg(&e.ty, B::Future) == Some(&Type::Void)
                    && match (callee.unqualified_name(), args.as_slice()) {
                        ("sleep", [millis]) => millis.ty == Type::I64 && child_supported(millis),
                        ("yield", []) => true,
                        _ => false,
                    };
            }
            // `format` is variadic and has no function symbol: it assembles a
            // string from a literal spec at the call site (willow-0g8j.2.5).
            if callee.is_free_named("format") {
                return e.ty == Type::String
                    && format_operands(args)
                        .is_some_and(|operands| operands.iter().all(child_supported));
            }
            // `references.wi` deliberately collects inside reference-taking
            // functions to prove pointed-to GC values stay alive. These builtins
            // are zero-argument runtime calls and need no AST-only metadata.
            // `gc_minor_collect` is the MOVING one, and is what a test has to
            // reach to prove a binding survives evacuation (willow-10zt).
            if callee.is_free_named("gc_collect") || callee.is_free_named("gc_minor_collect") {
                return args.is_empty() && e.ty == Type::Void;
            }
            // Their read-only siblings: `gc_allocated_bytes` and the other
            // statistic counters. Same shape — a zero-argument runtime call
            // with no AST-only metadata — but they return the counter
            // (willow-0g8j.3.1).
            if gc_stat_builtin_runtime_name(callee.unqualified_name()).is_some() {
                return args.is_empty() && e.ty == Type::I64;
            }
            if callee.is_free_named("recover") {
                return args.is_empty()
                    && builtin_types::unary_arg(&e.ty, B::Option).is_some_and(|payload| {
                        payload == &Type::Named("PanicInfo".to_string().into())
                    });
            }
            // These compiler-known control-flow operations require lexical
            // panic-scope handling through the dedicated paths above (willow-s9ej.3).
            !matches!(callee.unqualified_name(), "panic" | "recover")
                && ctx.callable(callee.unqualified_name(), args, false)
                && args.iter().all(child_supported)
        }
        HirExprKind::Print { value, newline: _ } => {
            (scalar(&value.ty) || value.ty == Type::String) && child_supported(value)
        }
        // The element type is already vetted by `supported_type(&e.ty)` above.
        HirExprKind::Array { elements } => {
            let elem = array_element_type(&e.ty);
            elements
                .iter()
                .all(|el| ctx.storable(&elem, &el.ty) && child_supported(el))
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
                && ctx.same_repr(&array_element_type(&array.ty), &e.ty)
                && child_supported(array)
                && child_supported(index)
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
            let mangled = class_method_symbol_name(ctx.known_modules, &class.to_string(), "init");
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
            shape_ok && args.iter().all(child_supported)
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
                    && child_supported(value)
            })
        }
        // `object.field` on a simple class, or the two `i64` bounds of a
        // `Range<i64>` (willow-0g8j.2.10). Other receivers fail validation.
        HirExprKind::FieldAccess { object, field } => {
            if range_i64(&object.ty) {
                return matches!(field.as_str(), "start" | "end")
                    && e.ty == Type::I64
                    && child_supported(object);
            }
            ctx.class_layout_of(&object.ty)
                .and_then(|l| l.iter().find(|(n, _)| n == field))
                .is_some_and(|(_, fty)| ctx.same_repr(fty, &e.ty))
                && child_supported(object)
        }
        // The builtin array methods the walker emits, plus a direct call to a
        // method of a simple class. Anything else on an array (`freeze`,
        // `map`, …) and every other receiver is refused.
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
                shape_ok && child_supported(object) && args.iter().all(child_supported)
            }
            // The value-taking `Option`/`Result` methods (willow-0g8j.2.1).
            // Checked before the collection arm because both receivers are
            // `Type::Generic`; `lir_collection` never matches an enum, so the
            // two sets cannot overlap.
            Type::Generic(..) if ctx.supported_enum_type(&object.ty) => {
                let arg_tys: Vec<Type> = args.iter().map(|a| a.ty.clone()).collect();
                option_result_method(&object.ty, method, &arg_tys)
                    .is_some_and(|ret| ctx.same_repr(&ret, &e.ty))
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            Type::Generic(_, type_args)
                if builtin_types::unary_arg(&object.ty, B::Channel).is_some() =>
            {
                let [elem] = type_args.as_slice() else {
                    return false;
                };
                let shape_ok = match method.as_str() {
                    "send" => {
                        args.len() == 1 && e.ty == Type::Void && ctx.storable(elem, &args[0].ty)
                    }
                    "recv" => args.is_empty() && ctx.repr_compatible(&e.ty, elem),
                    "close" => args.is_empty() && e.ty == Type::Void,
                    _ => false,
                };
                shape_ok && child_supported(object) && args.iter().all(child_supported)
            }
            // `Task<T>`/`JoinHandle<T>` cancellation (willow-0g8j.2.11). Both
            // read only the frame header the handle already points at, so
            // there is nothing to vet beyond the receiver and the result type.
            Type::Generic(_, _)
                if builtin_types::resolve(&object.ty)
                    .is_some_and(|resolved| matches!(resolved.id, B::Task | B::JoinHandle))
                    && task_handle_method(&object.ty, method, args.len())
                        .is_some_and(|ret| ctx.same_repr(&ret, &e.ty)) =>
            {
                args.is_empty() && child_supported(object)
            }
            // The native-blocking cells (willow-0g8j.2.13). Each accessor is
            // one runtime call over a word-based ABI, so beyond the receiver
            // there is only the stored value to vet — and it is vetted against
            // the CELL's element type in both directions, because the emitter
            // coerces into a word on the way in and out of one on the way out.
            Type::Generic(..) if blocking_cell(&object.ty).is_some() => {
                let (_, elem) = blocking_cell(&object.ty).expect("guarded by the arm");
                let Some((_, ret)) = blocking_cell_method(&object.ty, method, args.len()) else {
                    return false;
                };
                ctx.same_repr(&ret, &e.ty)
                    && args.iter().all(|a| ctx.same_repr(elem, &a.ty))
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            // The builtin collections. `get` yields an `Option<V>`, which the
            // walker represents as of willow-0g8j.2.1.
            Type::Generic(name, _) if !(ctx.is_interface)(name) => {
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
                            && ctx.same_repr(&targs[0], &args[0].ty)
                    }
                    // `get` yields `Option<V>` over the map's OWN value type —
                    // checked rather than assumed, because the runtime picks
                    // the option representation from that type and the walker
                    // passes the choice across the ABI (willow-0g8j.2.1).
                    (LirCollection::Map | LirCollection::FrozenMap, "get") => {
                        args.len() == 1
                            && ctx.same_repr(&targs[0], &args[0].ty)
                            && matches!(&e.ty, Type::Generic(..)
                                if builtin_types::unary_arg(&e.ty, B::Option) == Some(&targs[1]))
                            && ctx.supported_type(&e.ty)
                    }
                    (LirCollection::Map, "insert") => {
                        args.len() == 2
                            && ctx.same_repr(&targs[0], &args[0].ty)
                            && ctx.storable(&targs[1], &args[1].ty)
                    }
                    // Rendered in the runtime, which knows only the four
                    // scalar/string kinds — the former AST path passed `0` for
                    // anything else, which would render a pointer as an `i64`.
                    // Both halves are vetted: an admitted KEY always has a kind
                    // (`map_key_supported` allows only those four), but the
                    // check is written out so the emitter's `expect` stays
                    // honest if either subset moves.
                    (LirCollection::Map, "toString") => {
                        args.is_empty()
                            && e.ty == Type::String
                            && collection_elem_kind(&targs[0]).is_some()
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
                shape_ok && child_supported(object) && args.iter().all(child_supported)
            }
            // The cancellation handles (willow-0g8j.2.13). `cancel`, `child`
            // and `is_cancelled` read or write the handle alone; `attach`/`add`
            // additionally take a task and hand the same handle back, so the
            // result type is the argument's and both have to be admitted.
            Type::Named(_) if cancellation_handle(&object.ty).is_some() => {
                let Some((_, ret)) = cancellation_method(&object.ty, method, args) else {
                    return false;
                };
                ctx.same_repr(&ret, &e.ty)
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            // The atomic cells (willow-0g8j.2.13). Every operation is one word
            // read or write inside a cell that already exists, so beyond the
            // receiver there is only the operand to vet — and both it and the
            // result must be the cell's own width, because the emitter passes
            // them through untouched.
            Type::Named(_) if atomic_cell(&object.ty).is_some() => {
                let cell = atomic_cell(&object.ty).expect("guarded by the arm");
                let Some((_, ret)) = atomic_method(&object.ty, method, args.len()) else {
                    return false;
                };
                ctx.same_repr(&ret, &e.ty)
                    && args.iter().all(|a| a.ty == cell.word())
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            // Virtual dispatch through an interface box (willow-0g8j.6). The
            // receiver's box carries the vtable, so — unlike a class receiver —
            // this needs no knowledge of the concrete class, only that the
            // interface really declares the method at a known slot.
            Type::Named(iface) if (ctx.is_interface)(iface) => {
                let Some(sig) = (ctx.iface_method)(&object.ty, method) else {
                    return false;
                };
                // A `Self`-returning method yields a concrete object of the
                // receiver's own class, which the emitter re-boxes with the
                // receiver's vtable — so the result is the receiver's interface
                // and nothing else.
                let ret_ok = if matches!(&sig.ret, Type::Named(n) if n == &TypeId::local("Self")) {
                    matches!(&e.ty, Type::Named(n) if n == iface)
                } else {
                    ctx.same_repr(&sig.ret, &e.ty)
                };
                ret_ok
                    && sig.params.len() == args.len()
                    && sig.params.iter().all(|p| ctx.supported_type(p))
                    && sig
                        .params
                        .iter()
                        .zip(&sig.modes)
                        .zip(args)
                        .all(|((p, mode), a)| {
                            matches!(mode, ParamMode::Reference { .. })
                                == matches!(a.kind, HirExprKind::ReferenceArg { .. })
                                && ctx.storable(p, &a.ty)
                        })
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            Type::Generic(iface, _) if (ctx.is_interface)(iface) => {
                let Some(sig) = (ctx.iface_method)(&object.ty, method) else {
                    return false;
                };
                let ret_ok = if matches!(&sig.ret, Type::Named(n) if n == &TypeId::local("Self")) {
                    sig.ret == e.ty
                } else {
                    ctx.same_repr(&sig.ret, &e.ty)
                };
                ret_ok
                    && sig.params.len() == args.len()
                    && sig.params.iter().all(|p| ctx.supported_type(p))
                    && sig
                        .params
                        .iter()
                        .zip(&sig.modes)
                        .zip(args)
                        .all(|((p, mode), a)| {
                            matches!(mode, ParamMode::Reference { .. })
                                == matches!(a.kind, HirExprKind::ReferenceArg { .. })
                                && ctx.storable(p, &a.ty)
                        })
                    && child_supported(object)
                    && args.iter().all(child_supported)
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
                ) && child_supported(object)
            }
            // A class receiver. The implementation is resolved through the
            // receiver's ancestry rather than assumed to be declared on the
            // receiver's own class, so an INHERITED method is in the subset
            // (willow-0g8j.2.4); whether the emitted call is direct or goes
            // through the descriptor slot is decided later, by the same
            // shared `plan_virtual_call` helper.
            Type::Named(class) if ctx.supported_class(class) => {
                let Some(mangled) = ctx.resolve_class_method(class, method) else {
                    return false;
                };
                ctx.callable(&mangled, args, true)
                    && ctx.fn_types.get(&mangled).is_some_and(
                        |t| matches!(t, Type::Fn(_, ret) if ctx.repr_compatible(ret, &e.ty)),
                    )
                    && child_supported(object)
                    && args.iter().all(child_supported)
            }
            _ => false,
        },
        // Two unrelated things share this node. `Enum::Variant` with no
        // payload, and the bare unqualified form the checker resolved to one
        // (`Red`, `None`), lower to a static property read (willow-0g8j.8) —
        // and so does a real `Class::property`, which loads from module data
        // (willow-0g8j.2.4).
        HirExprKind::StaticField { class, field } => {
            if ctx.known_modules.contains_key(&class.to_string()) {
                return false;
            }
            if ctx.is_enum(&class.to_string()) {
                return ctx.supported_enum_type(&e.ty)
                    && ctx.enum_instance(&e.ty).is_some_and(|(name, def)| {
                        name == *class && def.variant(field).is_some_and(|v| v.payloads.is_empty())
                    });
            }
            // `Self::prop` inside a method resolves against the enclosing
            // class, exactly as `emit_static_field_read` resolves it
            // (willow-0g8j.13).
            (ctx.static_field)(ctx.resolved_class(&class.to_string()), field)
                .is_some_and(|ty| ctx.supported_type(&ty) && ctx.repr_compatible(&e.ty, &ty))
        }
        // `Class::method(args)` — a static method of a simple class, or an enum
        // variant construction. A module call (`math::add`) and a builtin
        // namespace (`fs`, `env`) spell themselves the same way and still need
        // dedicated builtin dispatch.
        HirExprKind::StaticCall {
            class,
            method,
            args,
        } => {
            // `Self::method(..)` inside a class body names the class the body
            // is DECLARED in, which is what the emitter resolves it to as well
            // (willow-0g8j.13). Resolved once, here, so every test below —
            // the builtin spellings, the enum path, the class path and the
            // symbol they mangle — sees the one name.
            let class_spelling = class.to_string();
            let class = ctx.resolved_class(&class_spelling);
            // `Map::new()` is the one builtin constructor in the subset. The
            // result type decides, not the spelling, so a user class called
            // `Map` cannot reach `willow_map_new`: it would have a `Named` type
            // and fall through to the class path below. The empty map is either
            // already instantiated (and then vetted by `supported_type` above)
            // or carries the untyped `Map<Void, Void>` the checker gives it.
            if class == "Map" && method == "new" && args.is_empty() {
                return matches!(lir_collection(&e.ty), Some((LirCollection::Map, _)));
            }
            if class == "Channel"
                && matches!(method.as_str(), "new" | "with_capacity")
                && builtin_types::unary_arg(&e.ty, B::Channel).is_some()
                && ctx.supported_type(&e.ty)
            {
                return (method == "new" && args.is_empty())
                    || (method == "with_capacity"
                        && args.len() == 1
                        && args[0].ty == Type::I64
                        && child_supported(&args[0]));
            }
            // `AtomicI64::new(i64)` / `AtomicBool::new(bool)`
            // (willow-0g8j.2.13). The RESULT type picks the cell, and the
            // operand is vetted against that cell's word: the emitter passes it
            // to the runtime unchanged, so a mismatch would be a
            // reinterpretation rather than a coercion. Any other spelling —
            // a user `static fn new` that happens to return a cell — falls
            // through to the class path below and is compiled as the call it
            // is; the emitter keys on the same class name so the two stay in
            // step.
            if let Some(cell) = atomic_cell(&e.ty)
                && method == "new"
                && class == cell.class_name()
            {
                return matches!(args.as_slice(), [a] if a.ty == cell.word())
                    && child_supported(&args[0]);
            }
            // `BlockingCell<T>::new(v)` / `BlockingRwCell<T>::new(v)`
            // (willow-0g8j.2.13). Keyed on the class as well as the result
            // type, for the same reason the atomic constructor above is. The
            // initial value is a STORE into the cell's element slot, but the
            // emitter coerces it to a word rather than boxing it, so this is
            // `assignable_repr` rather than `storable`.
            if let Some((kind, elem)) = blocking_cell(&e.ty)
                && method == "new"
                && class == kind.class_name()
            {
                return matches!(args.as_slice(), [a] if ctx.same_repr(elem, &a.ty))
                    && child_supported(&args[0]);
            }
            // `Mutex<T>::new(v)` / `RwLock<T>::new(v)` (willow-0g8j.2.13).
            // Same shape as the blocking cells above — the handle is a
            // one-word cell built from an initial value — and keyed the same
            // way, on the class as well as the result type.
            if let Some((_, protected)) = scheduler_lock(&e.ty)
                && method == "new"
                && matches!(class, "Mutex" | "RwLock")
            {
                return matches!(args.as_slice(), [a] if ctx.same_repr(protected, &a.ty))
                    && child_supported(&args[0]);
            }
            // `CancellationToken::new()` / `TaskScope::new()`
            // (willow-0g8j.2.13). Keyed on the class as well as the result
            // type, for the same reason the cell constructor above is.
            if let Some(handle) = cancellation_handle(&e.ty)
                && method == "new"
                && class == handle.class_name()
            {
                return args.is_empty();
            }
            // The `env`, `fs` and `net` builtin namespaces and the two `f64::`
            // calls (willow-0g8j.2.10, willow-0g8j.2.13): fixed-signature
            // runtime calls, admitted from the same table the emitter
            // dispatches on, so a call the walker accepts is one it can emit.
            if let Some(entry) =
                namespace_builtin_call(ctx.known_modules, ctx.builtin_module_aliases, class, method)
            {
                return entry.params.len() == args.len()
                    && entry
                        .params
                        .iter()
                        .zip(args)
                        .all(|(slot, a)| ctx.storable(slot, &a.ty))
                    && ctx.supported_type(&e.ty)
                    && ctx.repr_compatible(&e.ty, &entry.ret)
                    && args.iter().all(child_supported);
            }
            // A call into an imported user module (`math::add(1, 2)`), which
            // HIR spells as a static call whose "class" is the module's access
            // name (willow-7nc6). It is a FREE function: its symbol is the
            // module item symbol, and it takes no hidden `self` — which is why
            // it cannot fall through to the class path below, where a module
            // name is not a class and `supported_class` would refuse it.
            if let Some(module_prefix) = ctx.known_modules.linker_prefix(class) {
                let mangled = module_item_symbol(module_prefix, method);
                return ctx.callable(&mangled, args, false)
                    && ctx.fn_types.get(&mangled).is_some_and(
                        |t| matches!(t, Type::Fn(_, ret) if ctx.same_repr(ret, &e.ty)),
                    )
                    && args.iter().all(child_supported);
            }
            // `Enum::Variant(payload…)`, and the qualified fieldless form,
            // which HIR also spells as a zero-argument static call. The
            // payloads are STORE positions (the emitter coerces each one into
            // its declared slot), so `storable` is the right test, not
            // `assignable_repr`.
            if ctx.is_enum(&class.to_string()) {
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
                return name == TypeId::from_source_name(class)
                    && variant.payloads.len() == args.len()
                    && variant
                        .payloads
                        .iter()
                        .zip(args)
                        .all(|(slot, a)| ctx.storable(slot, &a.ty))
                    && args.iter().all(child_supported);
            }
            if !ctx.supported_class(class) {
                return false;
            }
            let mangled = class_method_symbol_name(ctx.known_modules, class, method);
            ctx.callable(&mangled, args, true)
                && ctx
                    .fn_types
                    .get(&mangled)
                    .is_some_and(|t| matches!(t, Type::Fn(_, ret) if ctx.same_repr(ret, &e.ty)))
                && args.iter().all(child_supported)
        }
        // `start..end` as a VALUE (willow-0g8j.2.10). The bounds are the two
        // words of the object the emitter allocates, so both have to be `i64`
        // and the node's own type has to be the range it builds — a retyped
        // node must not make the walker store two words under another shape.
        HirExprKind::Range { start, end } => {
            range_i64(&e.ty)
                && start.ty == Type::I64
                && end.ty == Type::I64
                && child_supported(start)
                && child_supported(end)
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
        HirExprKind::TryPropagate { inner } => {
            let Some(resolved) = builtin_types::resolve(&inner.ty) else {
                return false;
            };
            matches!(resolved.id, B::Option | B::Result)
                && ctx.supported_enum_type(&inner.ty)
                && resolved.args.first().is_some_and(|payload| match payload {
                    // `Result<void, E>` — `f()?;` in statement position, which
                    // is how every `net::` and `fs::` write is spelled
                    // (willow-0g8j.2.13). The success arm has no payload to
                    // read: `enum_instance` normalizes the `void` away, so the
                    // `Ok` object is the tag word alone and a word-1 load would
                    // read past it. The emitter loads nothing in that case, and
                    // the node's own `void` type is what says so.
                    Type::Void => resolved.id == B::Result && e.ty == Type::Void,
                    _ => ctx.same_repr(payload, &e.ty),
                })
                && child_supported(inner)
        }
        HirExprKind::Await { inner }
            if builtin_types::unary_arg(&inner.ty, B::Future).is_some()
                && !lir_suspends_here(e) =>
        {
            builtin_types::unary_arg(&inner.ty, B::Future) == Some(&e.ty)
                && ctx.supported_type(&e.ty)
                && child_supported(inner)
        }
        HirExprKind::Await { .. } => match lir_await_site(e, ctx.cooperative_leaves) {
            Some(LirAwaitSite::Sleep(millis)) => child_supported(millis),
            Some(LirAwaitSite::Yield) => true,
            // `await f(..)` on a cooperative leaf. The constructor call is
            // vetted exactly as a synchronous call to `f` would be — same
            // parameter table, same argument rules — and the awaited value is
            // read back out of the callee frame's RESULT slot at this node's
            // own type.
            Some(LirAwaitSite::LeafCall { callee, args, .. }) => {
                ctx.callable(&callee.to_string(), args, false)
                    && ctx.supported_type(&e.ty)
                    && args.iter().all(child_supported)
            }
            None => false,
        },
        HirExprKind::Select { cases } => e.ty == Type::Void && supported_select(cases, ctx, names),
    }
}

/// The source-shaped name of a reference place. See [`lir_reference_place_kind`]
/// for why this must agree with the AST spelling exactly.
#[cfg(test)]
fn lir_reference_place_name(place: &HirExpr) -> String {
    let mut current = place;
    let mut suffixes = Vec::new();
    let mut out = loop {
        match &current.kind {
            HirExprKind::Var(name) => break name.clone(),
            HirExprKind::FieldAccess { object, field } => {
                suffixes.push((Some(field.as_str()), None));
                current = object;
            }
            HirExprKind::Index { array, index } => {
                suffixes.push((None, Some(index.as_ref())));
                current = array;
            }
            _ => break "<expression>".to_string(),
        }
    };
    for (field, index) in suffixes.into_iter().rev() {
        if let Some(field) = field {
            out.push('.');
            out.push_str(field);
        } else if let Some(index) = index {
            out.push('[');
            out.push_str(&lir_reference_index_name(index));
            out.push(']');
        }
    }
    out
}

#[cfg(test)]
fn lir_reference_index_name(index: &HirExpr) -> String {
    match &index.kind {
        HirExprKind::Int(value) => value.to_string(),
        HirExprKind::Var(name) => name.clone(),
        _ => "<expr>".to_string(),
    }
}

/// Whether `place` has stable address semantics for a `&`/`&mut` call
/// argument: a local/reference parameter, a class field, or an array element.
#[cfg(test)]
fn supported_reference_place<'n>(
    place: &'n HirExpr,
    ctx: &LirTypeCtx<'_>,
    names: &HashMap<&'n str, Cow<'n, Type>>,
    child_supported: &impl Fn(&'n HirExpr) -> bool,
) -> bool {
    match &place.kind {
        HirExprKind::Var(name) => names
            .get(name.as_str())
            .is_some_and(|bound| ctx.same_repr(bound, &place.ty)),
        HirExprKind::FieldAccess { object, field } => {
            ctx.class_layout_of(&object.ty)
                .and_then(|layout| layout.iter().find(|(name, _)| name == field))
                .is_some_and(|(_, ty)| ctx.same_repr(ty, &place.ty))
                && child_supported(object)
        }
        HirExprKind::Index { array, index } => {
            matches!(&array.ty, Type::Array(elem) if ctx.same_repr(elem, &place.ty))
                && index.ty == Type::I64
                && child_supported(array)
                && child_supported(index)
        }
        _ => false,
    }
}

/// Where a LIR instruction came from, for the debug-build fault site. LIR
/// instructions carry no span of their own, so this takes the span of the
/// sub-expression that actually runs the fault-capable code: the indexed array
/// for an element store, the stored value otherwise.
fn lir_inst_span(inst: &LirInst) -> Option<Span> {
    match inst {
        LirInst::Compute { span, .. }
        | LirInst::Let { span, .. }
        | LirInst::Defer { span, .. }
        | LirInst::MatchTest { span, .. }
        | LirInst::MatchBind { span, .. }
        | LirInst::Unsupported { span, .. } => Some(*span),
        _ => None,
    }
}

/// The edges of `f` that close a loop: the target DOMINATES the source, so
/// every path that reaches the source has already run the target.
///
/// A preemption safepoint belongs on a real back edge — a CPU-bound loop has to
/// stay preemptible — but nowhere else. The AST liveness pass models suspension
/// at exactly "loop backedges and statements that execute a call", so a
/// safepoint on any OTHER edge can park with a local the pass left in an SSA
/// value, and the resumed poll re-enters past that definition.
///
/// Neither block ids nor plain reachability can pick these out. Lowering
/// numbers an `if`'s join block before its `else` arm, so the else arm's jump
/// to the join runs backwards by id while closing no loop; and inside a loop
/// every block reaches every other, so "the target reaches the source" calls
/// that same join edge a loop. Dominance is the definition that separates them
/// (willow-0g8j.2.11).
#[cfg(test)]
fn lir_back_edges(f: &LirFunction) -> std::collections::HashSet<(usize, usize)> {
    fn successors(block: &LirBlock) -> Vec<usize> {
        match &block.terminator {
            Terminator::Jump(target) => vec![target.0],
            Terminator::Branch {
                then_block,
                else_block,
                ..
            } => vec![then_block.0, else_block.0],
            Terminator::Suspend { resume, .. } => vec![resume.0],
            Terminator::Return(_) | Terminator::CleanupReturn => Vec::new(),
        }
    }

    let n = f.blocks.len();
    let mut predecessors = vec![Vec::new(); n];
    for block in &f.blocks {
        for target in successors(block) {
            predecessors[target].push(block.id.0);
        }
    }

    // Iterative dominators over the block set. The entry is dominated only by
    // itself; every other block starts dominated by everything and shrinks to
    // its own fixpoint, which also leaves an unreachable block (no
    // predecessors) dominated by all of them — it emits no code that matters.
    let mut dominators: Vec<Vec<bool>> = vec![vec![true; n]; n];
    dominators[0] = (0..n).map(|i| i == 0).collect();
    let mut changed = true;
    while changed {
        changed = false;
        for at in 1..n {
            let mut next = vec![false; n];
            let mut first = true;
            for &pred in &predecessors[at] {
                if first {
                    next.copy_from_slice(&dominators[pred]);
                    first = false;
                } else {
                    for (slot, dominates) in next.iter_mut().zip(&dominators[pred]) {
                        *slot &= *dominates;
                    }
                }
            }
            if first {
                continue;
            }
            next[at] = true;
            if next != dominators[at] {
                dominators[at] = next;
                changed = true;
            }
        }
    }

    let mut edges = std::collections::HashSet::new();
    for block in &f.blocks {
        for target in successors(block) {
            if dominators[block.id.0][target] {
                edges.insert((block.id.0, target));
            }
        }
    }
    edges
}

#[cfg(test)]
fn lir_terminator_needs_preempt_safepoint(
    block: &LirBlock,
    back_edges: &std::collections::HashSet<(usize, usize)>,
) -> bool {
    let closes_loop = |target: &BlockId| back_edges.contains(&(block.id.0, target.0));
    match &block.terminator {
        Terminator::Jump(target) => closes_loop(target),
        Terminator::Branch {
            cond: _,
            then_block,
            else_block,
        } => closes_loop(then_block) || closes_loop(else_block),
        Terminator::Return(Some(_)) => false,
        Terminator::Suspend { .. } | Terminator::Return(None) | Terminator::CleanupReturn => false,
    }
}

#[willow_continuations::methods(
    emit_deferred_action,
    emit_flush_defers_from,
    emit_sync_try_defer_flush
)]
impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit a whole function body by walking its LIR block graph. The entry
    /// block's instructions land in the already-created Cranelift entry block
    /// (parameters are bound there); every other LIR block gets its own.
    /// All paths are terminated by the LIR, so the caller must skip its
    /// implicit-return epilogue.
    pub(super) fn emit_lir_function(&mut self, f: &LirFunction) {
        self.emit_lir_function_inner(f, None);
    }

    /// Replay a cleanup graph with fresh region storage and the enclosing
    /// panic/defer state. CleanupReturn rejoins this replay's continuation.
    pub(super) fn emit_lir_cleanup_region(&mut self, region: &LirFunction) {
        let vars = self.vars.clone();
        let roots = self.gc_root_count;
        let active_roots = self
            .coop_shadow_roots
            .as_ref()
            .map(|roots| roots.active.clone());
        let defers = self.defer_stack.clone();
        let panic_scopes = self.panic_scopes.clone();
        let flags = self.sync_defer_flags.clone();
        let frame_offsets = std::mem::take(&mut self.lir_frame_offsets);
        let before_exit = self.lir_cleanup_exit;
        let exit = self.builder.create_block();
        self.lir_cleanup_exit = Some((exit, roots, false));
        // Region temporaries can share a spelling with a subsequently declared
        // outer temporary; their lifetimes and storage are independent.
        for local in region.locals.iter().filter(|local| !local.parameter) {
            self.vars.remove(&local.name);
        }
        self.emit_lir_function_inner(region, None);
        let reached = self.lir_cleanup_exit.expect("cleanup exit installed").2;
        self.lir_cleanup_exit = before_exit;
        self.vars = vars;
        self.gc_root_count = roots;
        self.defer_stack = defers;
        self.panic_scopes = panic_scopes;
        self.sync_defer_flags = flags;
        self.lir_frame_offsets = frame_offsets;
        if let (Some(active), Some(state)) = (active_roots, self.coop_shadow_roots.as_mut()) {
            state.active = active;
        }
        if reached {
            self.builder.switch_to_block(exit);
            self.builder.seal_block(exit);
        }
        self.terminated = !reached;
    }

    /// Emit an async poll body from LIR while retaining the established
    /// cooperative ABI. Each instruction boundary gets a cancellable
    /// preemption transition; locals selected
    /// by async liveness are read from their heap-frame slots after resume.
    pub(super) fn emit_coop_lir_function(
        &mut self,
        f: &LirFunction,
        suspends: &mut CoopSuspendPoints,
        frame: cranelift_codegen::ir::Value,
    ) {
        self.emit_lir_function_inner(f, Some((suspends, frame)));
    }

    fn emit_lir_function_inner(
        &mut self,
        f: &LirFunction,
        mut coop: Option<(&mut CoopSuspendPoints, cranelift_codegen::ir::Value)>,
    ) {
        let incoming_frames =
            lir_call_frame_entries(f).expect("validated method preparation frames");
        let incoming_references =
            lir_reference_scope_entries(f).expect("validated reference argument scopes");
        let outer_references = self.lir_reference_scopes.clone();
        let outer_frames = self.lir_call_frames.clone();
        let outer_frame_depth = self.callstack_frame_depth;
        let entry = self.builder.current_block().expect("entry block active");
        let mut poll_blocks = lir_sync_poll_blocks(f);
        let deferred_poll = coop.is_none() && lir_defer_entry_poll(f, &mut poll_blocks);
        // SSA carries the delayed activity lookup through later joins and
        // inlined recursive bodies. The zero entry value is used only on the
        // bounded return path; every path containing calls crosses a poll.
        let deferred_active = deferred_poll.then(|| {
            let var = self.builder.declare_var(types::I32);
            let zero = self.builder.ins().iconst(types::I32, 0);
            self.builder.def_var(var, zero);
            var
        });
        let sync_poll = (coop.is_none() && !deferred_poll).then(|| {
            let active = self.emit_value_runtime_call("willow_sync_native_active", &[]);
            let stop = self.emit_value_runtime_call("willow_gc_stop_flag", &[]);
            (active, stop)
        });
        if coop.is_some() {
            self.bind_coop_lir_locals(f);
            // GC locals that are dead at every suspension deliberately stay
            // out of the heap frame, but still need a native shadow-stack
            // root while the current poll invocation can allocate.
            self.bind_lir_gc_locals(f);
        } else {
            self.bind_lir_gc_locals(f);
        }
        self.bind_lir_locals(f);
        let mut blocks = vec![entry];
        for _ in 1..f.blocks.len() {
            blocks.push(self.builder.create_block());
        }

        let mut lir_defer_scopes = Vec::new();
        let mut ledger = LirDeferLedger::default();
        // Synchronous defer state per LIR block, filled in along the edges as
        // the predecessors are emitted. Async functions rebuild every scope
        // from the LIR at each exit, so they do not need it.
        let sync_defers = coop.is_none();
        let mut block_state: Vec<Option<LirDeferState>> = vec![None; f.blocks.len()];
        let initial_state = if self.lir_cleanup_exit.is_some() {
            LirDeferState {
                scopes: Vec::new(),
                entries: self.defer_stack.clone(),
                panic_scopes: self.panic_scopes.clone(),
                flags: self.sync_defer_flags.clone(),
            }
        } else {
            LirDeferState::default()
        };
        if sync_defers {
            block_state[0] = Some(initial_state.clone());
        }
        // Emission ORDER. A LIR block index is not a position in any order
        // control can flow in: `if/else` lowers the merge block before the
        // `else` arm, so index order emits the merge before the only
        // predecessor that reaches it. The synchronous emitter carries its
        // defer scopes along the edges, so a block emitted before any
        // predecessor starts with no open scope at all — and its `flush
        // defers` then unwinds nothing, silently skipping every `defer` on
        // that path. So a block is emitted only once an edge has handed it a
        // state; any such order works. A block no edge ever reaches is emitted
        // last, with none (willow-fvt4).
        let mut ready = vec![0usize];
        let mut emitted = vec![false; f.blocks.len()];
        // Which entry states arrived along a normal CFG edge. A recovery
        // continuation legitimately reaches its resume block with the scopes it
        // unwound already gone, so only edge-provided states are held to the
        // agreement `lir_sync_defer_stacks_agree` vetted.
        let mut state_from_edge = vec![false; f.blocks.len()];
        let mut unreached = 0usize;
        loop {
            // Without sync defers there is no state to thread, so the worklist
            // is left unused and every block is taken in index order below.
            let next_ready = if sync_defers { ready.pop() } else { None };
            let i = match next_ready {
                Some(i) if emitted[i] => continue,
                Some(i) => i,
                None => {
                    while unreached < f.blocks.len() && emitted[unreached] {
                        unreached += 1;
                    }
                    if unreached == f.blocks.len() {
                        break;
                    }
                    unreached
                }
            };
            emitted[i] = true;
            let block = &f.blocks[i];
            if i > 0 {
                self.builder.switch_to_block(blocks[i]);
            }
            if sync_defers {
                let state = block_state[i]
                    .clone()
                    .unwrap_or_else(|| initial_state.clone());
                lir_defer_scopes = state.scopes;
                self.defer_stack = state.entries;
                self.panic_scopes = state.panic_scopes;
                self.sync_defer_flags = state.flags;
            }
            // Each LIR block starts a fresh Cranelift block, so whatever the
            // previous one ended with (a `return`, a diverging statement) says
            // nothing about this one.
            self.terminated = false;
            self.lir_call_frames = outer_frames.clone();
            self.lir_reference_scopes = outer_references.clone();
            self.lir_reference_scopes
                .extend(incoming_references[i].clone());
            let frames: Vec<_> = incoming_frames[i]
                .iter()
                .filter(|(method, _)| self.flat_method_frame_enabled(method))
                .cloned()
                .collect();
            self.lir_call_frames.extend(frames);
            self.callstack_frame_depth =
                outer_frame_depth + self.lir_call_frames.len() - outer_frames.len();
            let block_coop = coop
                .as_mut()
                .map(|(suspends, frame)| (&mut **suspends, *frame));
            let mut defer_ctx = LirBlockDeferCtx {
                block_index: i,
                scopes: &mut lir_defer_scopes,
                ledger: &mut ledger,
            };
            let block_poll = if deferred_poll && poll_blocks[i] {
                let active = self.emit_value_runtime_call("willow_sync_native_active", &[]);
                let stop = self.emit_value_runtime_call("willow_gc_stop_flag", &[]);
                self.builder
                    .def_var(deferred_active.expect("delayed poll activity"), active);
                Some((active, stop))
            } else {
                sync_poll.filter(|_| poll_blocks[i])
            };
            let outer_active = self.sync_native_active;
            self.sync_native_active = block_poll
                .or(sync_poll)
                .map(|(active, _)| active)
                .or_else(|| deferred_active.map(|var| self.builder.use_var(var)));
            let recovery_states = self.emit_lir_block(
                f,
                block,
                &blocks,
                &f.return_type,
                block_coop,
                block_poll,
                &mut defer_ctx,
            );
            self.sync_native_active = outer_active;
            if sync_defers {
                let exit = LirDeferState {
                    scopes: lir_defer_scopes.clone(),
                    entries: self.defer_stack.clone(),
                    panic_scopes: self.panic_scopes.clone(),
                    flags: self.sync_defer_flags.clone(),
                };
                for (target, state) in recovery_states {
                    if block_state[target].is_none() {
                        block_state[target] = Some(state);
                        ready.push(target);
                    }
                }
                for target in lir_block_successors(block) {
                    match &block_state[target] {
                        Some(existing) if state_from_edge[target] => debug_assert!(
                            lir_scope_ids(existing) == lir_scope_ids(&exit),
                            "LIR block bb{target} of `{}` is reached with two different \
                             defer scope stacks: {:?} and {:?}",
                            f.name,
                            lir_scope_ids(existing),
                            lir_scope_ids(&exit),
                        ),
                        Some(_) => {}
                        None => {
                            block_state[target] = Some(exit.clone());
                            state_from_edge[target] = true;
                            ready.push(target);
                        }
                    }
                }
            }
        }
        self.lir_call_frames = outer_frames;
        self.lir_reference_scopes = outer_references;
        self.callstack_frame_depth = outer_frame_depth;
        self.terminated = true;
        if sync_defers {
            // Scopes only ever left by a `return`, `break`, `continue` or `?`
            // have no `LeaveDeferScope` to close them. Their panic cleanup is
            // still owed, once each, innermost last opened first.
            while let Some(frame) = ledger.opened.pop() {
                if ledger.closed.contains(&frame.id) {
                    continue;
                }
                if let Some(state) = ledger.dropped.remove(&frame.id) {
                    self.defer_stack = state.entries;
                    self.panic_scopes = state.panic_scopes;
                    self.sync_defer_flags = state.flags;
                }
                self.terminated = true;
                self.finish_lir_defer_scope(frame, &mut ledger.cleanups);
            }
            for cleanup in ledger.cleanups.drain(..) {
                self.builder.seal_block(cleanup);
            }
        } else {
            while let Some(frame) = lir_defer_scopes.pop() {
                self.finish_lir_async_panic_scope(frame);
            }
        }
        // The enclosing function compiler may append shared panic-return CFG
        // after the LIR body. It seals all blocks once that ABI edge exists
        // (willow-s9ej.4).
        self.terminated = true;
    }

    /// Pre-bind the locals selected by LIR liveness to their LIR-owned frame
    /// slots. Source spans are intentionally absent from this lookup.
    fn bind_coop_lir_locals(&mut self, f: &LirFunction) {
        for local in &f.locals {
            if let Some(offset) = self.lir_frame_offsets.get(&local.id).copied() {
                self.vars.insert(
                    local.name.clone(),
                    VarStorage::Frame {
                        offset,
                        ty: local.ty.clone(),
                    },
                );
            }
        }
    }

    pub(super) fn load_lir_local(
        &mut self,
        function: &LirFunction,
        local: LirLocalId,
    ) -> cranelift_codegen::ir::Value {
        let name = &function.locals[local.0 as usize].name;
        let storage = self
            .vars
            .get(name)
            .cloned()
            .unwrap_or_else(|| panic!("LIR local `{name}` has no storage"));
        if function.locals[local.0 as usize].is_gc_owner() {
            let ptr_ty = reference_type(self.module.target_config());
            return match storage {
                VarStorage::Stack { slot, .. } => self.stack_load(ptr_ty, slot),
                VarStorage::Frame { offset, .. } => self.builder.ins().load(
                    ptr_ty,
                    MemFlagsData::new(),
                    self.async_frame.expect("GC owner frame"),
                    offset,
                ),
                _ => panic!("opaque GC owner must occupy rooted storage"),
            };
        }
        self.load_var(&storage)
    }

    fn store_lir_local(
        &mut self,
        function: &LirFunction,
        local: LirLocalId,
        value: cranelift_codegen::ir::Value,
    ) {
        let name = &function.locals[local.0 as usize].name;
        let storage = self
            .vars
            .get(name)
            .cloned()
            .unwrap_or_else(|| panic!("LIR local `{name}` has no storage"));
        if function.locals[local.0 as usize].is_gc_owner() {
            match storage {
                VarStorage::Stack { slot, .. } => self.stack_store(value, slot),
                VarStorage::Frame { offset, .. } => self.emit_gc_heap_store_classified(
                    self.async_frame.expect("GC owner frame"),
                    offset,
                    value,
                    true,
                    GcStoreDestination::AsyncFrameSlot,
                ),
                _ => panic!("opaque GC owner must occupy rooted storage"),
            }
            return;
        }
        self.store_var(&storage, value);
    }

    fn emit_lir_select_instruction(&mut self, function: &LirFunction, inst: &LirInst) {
        match inst {
            LirInst::SelectInit { operations } => {
                for operation in operations {
                    if let LirSelectOp::Timeout { millis, deadline } = operation {
                        let millis = self.load_lir_local(function, *millis);
                        let now = self.emit_value_runtime_call("willow_monotonic_millis", &[]);
                        let deadline_value = self.builder.ins().iadd(now, millis);
                        self.store_lir_local(function, *deadline, deadline_value);
                    }
                }
            }
            LirInst::SelectProbe { operations, ready } => {
                for (operation, ready_local) in operations.iter().zip(ready) {
                    let Some(ready_local) = ready_local else {
                        continue;
                    };
                    let ready_value = match operation {
                        LirSelectOp::Recv { channel, .. } => {
                            let channel = self.load_lir_local(function, *channel);
                            let raw = self
                                .emit_value_runtime_call("willow_channel_recv_ready", &[channel]);
                            self.builder.ins().icmp_imm_s(IntCC::NotEqual, raw, 0)
                        }
                        LirSelectOp::Send { channel, .. } => {
                            let channel = self.load_lir_local(function, *channel);
                            let raw = self
                                .emit_value_runtime_call("willow_channel_send_ready", &[channel]);
                            self.builder.ins().icmp_imm_s(IntCC::NotEqual, raw, 0)
                        }
                        LirSelectOp::Join { task, .. } => {
                            let task = self.load_lir_local(function, *task);
                            let id = self.builder.ins().load(
                                types::I64,
                                MemFlagsData::new(),
                                task,
                                async_frame_slot_offset(
                                    FRAME_SLOT_TASK_ID,
                                    reference_type(self.module.target_config()).bytes(),
                                ),
                            );
                            let raw =
                                self.emit_value_runtime_call("willow_frame_await", &[task, id]);
                            self.builder.ins().icmp_imm_s(IntCC::NotEqual, raw, 0)
                        }
                        LirSelectOp::Timeout { deadline, .. } => {
                            let deadline = self.load_lir_local(function, *deadline);
                            let now = self.emit_value_runtime_call("willow_monotonic_millis", &[]);
                            self.builder
                                .ins()
                                .icmp(IntCC::SignedGreaterThanOrEqual, now, deadline)
                        }
                        LirSelectOp::Default => unreachable!(),
                    };
                    let one = self.builder.ins().iconst(types::I8, 1);
                    let zero = self.builder.ins().iconst(types::I8, 0);
                    let ready_value = self.builder.ins().select(ready_value, one, zero);
                    self.store_lir_local(function, *ready_local, ready_value);
                }
            }
            LirInst::SelectPick { ready, chosen } => {
                let mut flags = Vec::with_capacity(ready.len());
                let mut total = self.builder.ins().iconst(types::I64, 0);
                for ready in ready {
                    let flag = if let Some(ready) = ready {
                        let ready = self.load_lir_local(function, *ready);
                        self.builder.ins().uextend(types::I64, ready)
                    } else {
                        self.builder.ins().iconst(types::I64, 0)
                    };
                    total = self.builder.ins().iadd(total, flag);
                    flags.push(flag);
                }
                // `willow_select_rotation` advances a PROCESS-WIDE counter, so
                // it may only be called on a probe that actually picks a case.
                // Calling it unconditionally would shift the fairness sequence
                // of every other select in the program.
                let none = self.builder.ins().icmp_imm_s(IntCC::Equal, total, 0);
                let unready = self.builder.ins().iconst(types::I64, -1);
                self.store_lir_local(function, *chosen, unready);
                let pick = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.ins().brif(none, done, &[], pick, &[]);

                self.builder.switch_to_block(pick);
                self.builder.seal_block(pick);
                let rotation = self.emit_value_runtime_call("willow_select_rotation", &[]);
                let rank = self.builder.ins().urem(rotation, total);
                let mut accumulated = self.builder.ins().iconst(types::I64, 0);
                let mut selected = self.builder.ins().iconst(types::I64, -1);
                for (index, flag) in flags.into_iter().enumerate() {
                    let is_ready = self.builder.ins().icmp_imm_s(IntCC::NotEqual, flag, 0);
                    let at_rank = self.builder.ins().icmp(IntCC::Equal, accumulated, rank);
                    let hit = self.builder.ins().band(is_ready, at_rank);
                    let index = self.builder.ins().iconst(types::I64, index as i64);
                    selected = self.builder.ins().select(hit, index, selected);
                    accumulated = self.builder.ins().iadd(accumulated, flag);
                }
                self.store_lir_local(function, *chosen, selected);
                self.builder.ins().jump(done, &[]);

                self.builder.switch_to_block(done);
                self.builder.seal_block(done);
            }
            LirInst::SelectUnregister { operations, winner } => {
                let (winner_raw, direction) = match &operations[*winner] {
                    LirSelectOp::Recv { channel, .. } => {
                        (self.load_lir_local(function, *channel), 0)
                    }
                    LirSelectOp::Send { channel, .. } => {
                        (self.load_lir_local(function, *channel), 1)
                    }
                    _ => (
                        self.builder
                            .ins()
                            .iconst(reference_type(self.module.target_config()), 0),
                        -1,
                    ),
                };
                let direction = self.builder.ins().iconst(types::I64, direction);
                let mut channels = Vec::new();
                for operation in operations {
                    match operation {
                        LirSelectOp::Recv { channel, .. } | LirSelectOp::Send { channel, .. } => {
                            let channel = self.load_lir_local(function, *channel);
                            // Source expressions can alias at runtime. Cleanup
                            // must run once per raw, retaining the winning
                            // direction even when it occurs in a later arm.
                            let cleanup = self.builder.create_block();
                            let done = self.builder.create_block();
                            let mut unique = self.builder.ins().iconst(types::I8, 1);
                            for previous in &channels {
                                let distinct =
                                    self.builder.ins().icmp(IntCC::NotEqual, channel, *previous);
                                unique = self.builder.ins().band(unique, distinct);
                            }
                            self.builder.ins().brif(unique, cleanup, &[], done, &[]);
                            self.builder.switch_to_block(cleanup);
                            self.builder.seal_block(cleanup);
                            self.emit_void_runtime_call(
                                "willow_channel_select_cleanup",
                                &[channel, winner_raw, direction],
                            );
                            self.builder.ins().jump(done, &[]);
                            self.builder.switch_to_block(done);
                            self.builder.seal_block(done);
                            channels.push(channel);
                        }
                        LirSelectOp::Join { task, .. } => {
                            let task = self.load_lir_local(function, *task);
                            let id = self.builder.ins().load(
                                types::I64,
                                MemFlagsData::new(),
                                task,
                                async_frame_slot_offset(
                                    FRAME_SLOT_TASK_ID,
                                    reference_type(self.module.target_config()).bytes(),
                                ),
                            );
                            self.emit_void_runtime_call(
                                "willow_sched_unregister_task_waiter",
                                &[id],
                            );
                        }
                        LirSelectOp::Timeout { .. } | LirSelectOp::Default => {}
                    }
                }
            }
            LirInst::SelectCommit { operation, success } => {
                let mut committed = true;
                match operation {
                    LirSelectOp::Recv {
                        channel,
                        binding,
                        elem_ty,
                    } => {
                        let channel = self.load_lir_local(function, *channel);
                        let runtime =
                            format!("willow_channel_recv_{}", channel_runtime_suffix(elem_ty));
                        let value = self.emit_value_runtime_call(&runtime, &[channel]);
                        if let Some(binding) = binding {
                            self.store_lir_local(function, *binding, value);
                        }
                    }
                    LirSelectOp::Send {
                        channel,
                        value,
                        elem_ty,
                    } => {
                        let channel = self.load_lir_local(function, *channel);
                        let raw = self.load_lir_local(function, *value);
                        let from_ty = &function.locals[value.0 as usize].ty;
                        let value = self.coerce_to_target(raw, from_ty, elem_ty);
                        let runtime = format!(
                            "willow_channel_try_send_{}",
                            channel_runtime_suffix(elem_ty)
                        );
                        let sent = self.emit_value_runtime_call(&runtime, &[channel, value]);
                        let sent = self.builder.ins().icmp_imm_s(IntCC::NotEqual, sent, 0);
                        let one = self.builder.ins().iconst(types::I8, 1);
                        let zero = self.builder.ins().iconst(types::I8, 0);
                        let sent = self.builder.ins().select(sent, one, zero);
                        self.store_lir_local(function, *success, sent);
                        committed = false;
                    }
                    LirSelectOp::Join {
                        task,
                        binding,
                        result_ty,
                        cancel_aware,
                    } => {
                        let task = self.load_lir_local(function, *task);
                        let id = self.builder.ins().load(
                            types::I64,
                            MemFlagsData::new(),
                            task,
                            async_frame_slot_offset(
                                FRAME_SLOT_TASK_ID,
                                reference_type(self.module.target_config()).bytes(),
                            ),
                        );
                        let value =
                            self.emit_task_terminal_value(task, id, result_ty, *cancel_aware);
                        if let (Some(binding), Some(value)) = (binding, value) {
                            self.store_lir_local(function, *binding, value);
                        }
                    }
                    LirSelectOp::Timeout { .. } | LirSelectOp::Default => {}
                }
                if committed {
                    let one = self.builder.ins().iconst(types::I8, 1);
                    self.store_lir_local(function, *success, one);
                }
            }
            _ => unreachable!(),
        }
    }

    fn emit_lir_suspend(
        &mut self,
        function: &LirFunction,
        operation: &SuspendOp,
        resume: BlockId,
        blocks: &[cranelift_codegen::ir::Block],
        suspends: &mut CoopSuspendPoints,
        frame: cranelift_codegen::ir::Value,
    ) {
        match operation {
            SuspendOp::Sleep { millis } => {
                let millis = self.load_lir_local(function, *millis);
                self.emit_coop_sleep_value(millis, suspends, frame);
            }
            SuspendOp::Yield => self.emit_coop_yield(suspends, frame),
            SuspendOp::Preempt => self.emit_coop_statement_safepoint(suspends, frame),
            SuspendOp::LockAcquire { slots, span } => {
                // All four slots are frame-backed by construction: the
                // operation reports them to LIR liveness, which is what makes a
                // local framed (willow-0g8j.2.13). Nothing here may live on the
                // native stack, because a contended acquisition returns out of
                // the poll function entirely.
                let offsets = self.lir_lock_offsets(slots);
                self.emit_lir_lock_acquire(
                    slots.mode,
                    offsets,
                    &slots.value_ty,
                    *span,
                    suspends,
                    frame,
                );
            }
            SuspendOp::AwaitTask {
                task,
                result,
                result_ty,
                cancel_aware,
            } => {
                let task_frame = self.load_lir_local(function, *task);
                self.emit_coop_frame_await(task_frame, None, None, suspends, frame);
                let task_frame = self.load_lir_local(function, *task);
                if let Some(result) = result {
                    let value = self
                        .emit_coop_awaited_result(task_frame, Some(result_ty), *cancel_aware)
                        .expect("value-producing await has a result");
                    self.store_lir_local(function, *result, value);
                } else {
                    self.emit_coop_awaited_result(task_frame, None, *cancel_aware);
                }
            }
            SuspendOp::ChannelRecv {
                channel,
                result,
                result_ty,
            } => {
                let check = self.builder.create_block();
                self.builder.ins().jump(check, &[]);
                let state = (suspends.len() + 1) as i64;
                self.record_coop_suspend(suspends, check);
                self.builder.switch_to_block(check);
                let channel_value = self.load_lir_local(function, *channel);
                let ready =
                    self.emit_value_runtime_call("willow_channel_recv_ready", &[channel_value]);
                let receive = self.builder.create_block();
                let park = self.builder.create_block();
                let ready = self.builder.ins().icmp_imm_s(IntCC::NotEqual, ready, 0);
                self.builder.ins().brif(ready, receive, &[], park, &[]);
                self.builder.switch_to_block(park);
                let state = self.builder.ins().iconst(types::I64, state);
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), state, frame, 0);
                self.emit_coop_unwind_poll_roots();
                let pending = self.builder.ins().iconst(types::I32, 0);
                self.builder.ins().return_(&[pending]);
                self.builder.switch_to_block(receive);
                let channel_value = self.load_lir_local(function, *channel);
                let runtime = format!("willow_channel_recv_{}", channel_runtime_suffix(result_ty));
                let value = self.emit_value_runtime_call(&runtime, &[channel_value]);
                if let Some(result) = result {
                    self.store_lir_local(function, *result, value);
                }
            }
            SuspendOp::ChannelSend {
                channel,
                value,
                elem_ty,
            } => {
                let check = self.builder.create_block();
                self.builder.ins().jump(check, &[]);
                let state = (suspends.len() + 1) as i64;
                self.record_coop_suspend(suspends, check);
                self.builder.switch_to_block(check);
                let channel_value = self.load_lir_local(function, *channel);
                let raw = self.load_lir_local(function, *value);
                let from_ty = &function.locals[value.0 as usize].ty;
                let sent_value = self.coerce_to_target(raw, from_ty, elem_ty);
                let runtime = format!(
                    "willow_channel_try_send_{}",
                    channel_runtime_suffix(elem_ty)
                );
                let sent = self.emit_value_runtime_call(&runtime, &[channel_value, sent_value]);
                let done = self.builder.create_block();
                let park = self.builder.create_block();
                let sent = self.builder.ins().icmp_imm_s(IntCC::NotEqual, sent, 0);
                self.builder.ins().brif(sent, done, &[], park, &[]);
                self.builder.switch_to_block(park);
                let state = self.builder.ins().iconst(types::I64, state);
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), state, frame, 0);
                self.emit_coop_unwind_poll_roots();
                let pending = self.builder.ins().iconst(types::I32, 0);
                self.builder.ins().return_(&[pending]);
                self.builder.switch_to_block(done);
            }
            SuspendOp::SelectWait { operations } => {
                let mut minimum = None;
                for operation in operations {
                    if let LirSelectWaitOp::Timeout { deadline } = operation {
                        let deadline = self.load_lir_local(function, *deadline);
                        minimum = Some(match minimum {
                            None => deadline,
                            Some(current) => {
                                let before = self.builder.ins().icmp(
                                    IntCC::SignedLessThan,
                                    deadline,
                                    current,
                                );
                                self.builder.ins().select(before, deadline, current)
                            }
                        });
                    }
                }
                if let Some(deadline) = minimum {
                    let now = self.emit_value_runtime_call("willow_monotonic_millis", &[]);
                    let remaining = self.builder.ins().isub(deadline, now);
                    let zero = self.builder.ins().iconst(types::I64, 0);
                    let negative = self
                        .builder
                        .ins()
                        .icmp(IntCC::SignedLessThan, remaining, zero);
                    let remaining = self.builder.ins().select(negative, zero, remaining);
                    self.emit_void_runtime_call("willow_sched_sleep", &[remaining]);
                }
                let state = (suspends.len() + 1) as i64;
                let state = self.builder.ins().iconst(types::I64, state);
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), state, frame, 0);
                self.emit_coop_unwind_poll_roots();
                let pending = self.builder.ins().iconst(types::I32, 0);
                self.builder.ins().return_(&[pending]);
                let wake = self.builder.create_block();
                self.record_coop_suspend(suspends, wake);
                self.builder.switch_to_block(wake);
            }
        }
        self.builder.ins().jump(blocks[resume.0], &[]);
    }

    /// Give every GC-managed `let` of this function one entry-allocated, rooted
    /// stack slot (see the module docs). The slot is null-initialized so a
    /// collection that happens before the `let` executes reads an empty root
    /// rather than uninitialized stack memory. GC-managed *parameters* already
    /// got the same treatment from `bind_param`, so they are skipped here.
    fn bind_lir_gc_locals(&mut self, f: &LirFunction) {
        let mut null = None;
        for block in &f.blocks {
            for inst in &block.instrs {
                let LirInst::Let { name, ty, .. } = inst else {
                    continue;
                };
                if !is_gc_managed(ty, self.enum_infos) {
                    continue;
                }
                if self.vars.contains_key(name) {
                    continue;
                }
                self.bind_lir_rooted_slot(name, ty, &mut null);
            }
        }
    }

    /// One entry-allocated, null-initialized, rooted stack slot for a
    /// GC-managed binding. `null` memoizes the zero across the slots of one
    /// function, so the entry block gets a single constant however many
    /// bindings need seeding.
    fn bind_lir_rooted_slot(
        &mut self,
        name: &str,
        ty: &Type,
        null: &mut Option<cranelift_codegen::ir::Value>,
    ) {
        let ptr_ty = reference_type(self.module.target_config());
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        let zero = *null.get_or_insert_with(|| self.builder.ins().iconst(ptr_ty, 0));
        self.stack_store(zero, slot);
        self.emit_push_root_slot(slot);
        self.track_coop_binding_root(slot);
        self.vars.insert(
            name.to_string(),
            VarStorage::Stack {
                slot,
                ty: ty.clone(),
            },
        );
    }

    /// Drop the GC roots a lexical scope owned, now that it has ended
    /// (willow-0g8j.3.3).
    ///
    /// A GC-managed local's slot is allocated and rooted once at function entry
    /// (see the module docs), so without this the last value a scope bound
    /// stays reachable until the function returns: a loop body's binding pins
    /// one object per call. Nulling the slot at scope exit lets the collector
    /// reclaim it while leaving the root registered and empty, as at entry.
    ///
    /// Two kinds of slot are cleared. A stack slot is the shadow-stack root
    /// itself. A [`VarStorage::Frame`] local lives in the heap async frame,
    /// traced by the frame's own map rather than by the shadow stack, and it
    /// needs the same treatment for the same reason: the frame outlives every
    /// scope in the function and is traced until the task completes, so a local
    /// that was live across an `await` would otherwise be held by a scope that
    /// ended. A [`VarStorage::Value`] local holds no root at all, and a
    /// [`VarStorage::ReferencePtr`] one points into storage the callee does not
    /// own. The `is_gc_managed` test is what separates a rooted slot from the
    /// plain stack slot an address-taken local gets: clearing that one would
    /// zero a live variable.
    fn emit_lir_clear_scope_roots(&mut self, function: &LirFunction, locals: &[LirLocalId]) {
        enum Root {
            Slot(cranelift_codegen::ir::StackSlot),
            Frame(i32),
        }
        let roots: Vec<Root> = locals
            .iter()
            .filter_map(|local| function.locals.get(local.0 as usize))
            .filter_map(|local| match self.vars.get(local.name.as_str()) {
                Some(VarStorage::Stack { slot, ty })
                    if local.is_gc_owner() || is_gc_managed(ty, self.enum_infos) =>
                {
                    Some(Root::Slot(*slot))
                }
                Some(VarStorage::Frame { offset, ty })
                    if local.is_gc_owner() || is_gc_managed(ty, self.enum_infos) =>
                {
                    Some(Root::Frame(*offset))
                }
                _ => None,
            })
            .collect();
        if roots.is_empty() {
            return;
        }
        let ptr_ty = reference_type(self.module.target_config());
        let zero = self.builder.ins().iconst(ptr_ty, 0);
        let frame_base = self.async_frame;
        for root in roots {
            match root {
                Root::Slot(slot) => self.stack_store(zero, slot),
                // Null creates no edge and needs no write barrier, but the
                // reference slot still synchronizes with concurrent GC readers.
                Root::Frame(offset) => {
                    if let Some(base) = frame_base {
                        let slot = self.builder.ins().iadd_imm_s(base, i64::from(offset));
                        self.builder
                            .ins()
                            .atomic_store(MemFlagsData::new(), zero, slot);
                    }
                }
            }
        }
    }

    /// Give storage to every LIR local that nothing else binds (willow-ht1h,
    /// willow-34su).
    ///
    /// Two kinds of local need it. Lowering declares locals that carry a value
    /// ACROSS blocks — a conditional's merge result, a `match` scrutinee, a
    /// `match` arm's pattern bindings. They are written by
    /// [`LirInst::Assign`] or by [`LirInst::MatchBind`] and never by a
    /// [`LirInst::Let`], so neither [`FuncGen::bind_lir_gc_locals`] nor
    /// [`FuncGen::bind_coop_lir_locals`] sees them.
    ///
    /// The rest are ordinary `let`s whose [`LirInst::Let`] is EMITTED after a
    /// block that names them. A block index is not a position in any order
    /// control flows in, and an async body is emitted in index order, so a
    /// `lock` body and its continuation can both come out before the block
    /// holding the `let` they read: `let mut got = 0; lock m as v { got = v; }`
    /// lowers the two lock blocks ahead of the block that binds `got`. Binding
    /// at the `Let` alone is therefore too late for them.
    ///
    /// Either way the consequence is the same: with no entry binding the write
    /// is silently discarded and the read reaches codegen unbound. So every
    /// local gets its storage here, and [`LirInst::Let`] stores into whatever
    /// this left it — a binding point that does not depend on emission order.
    ///
    /// A local async liveness put in the heap frame is already bound and is
    /// skipped here — and that set is exactly the one that has to survive a
    /// poll return, so a Cranelift variable is sound for everything left.
    fn bind_lir_locals(&mut self, f: &LirFunction) {
        for block in &f.blocks {
            for inst in &block.instrs {
                if let LirInst::Compute { value, .. } = inst {
                    for operand in value.operands() {
                        if let crate::ir::lowered::LirOperand::Reference {
                            place: crate::ir::lowered::LirPlace::Local(id),
                            ..
                        } = operand
                        {
                            self.address_taken
                                .insert(f.locals[id.0 as usize].name.clone());
                        }
                    }
                }
            }
        }
        let mut null = None;
        for local in &f.locals {
            if local.parameter || self.vars.contains_key(local.name.as_str()) {
                continue;
            }
            if local.is_gc_owner() || is_gc_managed(&local.ty, self.enum_infos) {
                self.bind_lir_rooted_slot(&local.name, &local.ty, &mut null);
                continue;
            }
            if self.address_taken.contains(local.name.as_str()) {
                // Give an address-taken local its definitive slot at function
                // entry so no `&` use inserts path-local promotion, and so the
                // one slot is the same one whichever block writes it first.
                let clif = clif_type(reference_type(self.module.target_config()), &local.ty);
                let zero = if clif == types::F64 {
                    self.builder.ins().f64const(0.0)
                } else {
                    self.builder.ins().iconst(clif, 0)
                };
                let storage = self.create_local_stack_slot(&local.ty, zero);
                self.vars.insert(local.name.clone(), storage);
                continue;
            }
            // Seeded so a path that reads the local without having run its
            // write — a `match` whose arms all diverge, a merge reached from a
            // branch that never assigned — still has a reaching definition,
            // including a match merge whose other arm diverges.
            let clif = clif_type(reference_type(self.module.target_config()), &local.ty);
            let var = self.builder.declare_var(clif);
            let zero = match clif {
                types::F64 => self.builder.ins().f64const(0.0),
                ty => self.builder.ins().iconst(ty, 0),
            };
            self.builder.def_var(var, zero);
            self.vars
                .insert(local.name.clone(), VarStorage::Value { var });
        }
    }

    // Keep explicit emission operands aligned with the LIR/runtime ABI.
    #[allow(clippy::too_many_arguments)]
    fn emit_lir_block(
        &mut self,
        function: &LirFunction,
        block: &LirBlock,
        blocks: &[cranelift_codegen::ir::Block],
        return_type: &Type,
        mut coop: Option<(&mut CoopSuspendPoints, cranelift_codegen::ir::Value)>,
        sync_poll: Option<(cranelift_codegen::ir::Value, cranelift_codegen::ir::Value)>,
        defers: &mut LirBlockDeferCtx<'_>,
    ) -> Vec<(usize, LirDeferState)> {
        let mut recovery_states = Vec::new();
        if let Some((active, stop)) = sync_poll
            && (self.lir_cleanup_exit.is_none() || block.id.0 != 0)
        {
            // Native stacks preserve SSA values and roots across this hook;
            // function entry and cycle headers cover recursion and loops.
            self.emit_sync_safepoint(active, stop);
        }
        for (inst_index, inst) in block.instrs.iter().enumerate() {
            // A panic/return has terminated the source path, but a
            // SYNCHRONOUS lexical scope still has to be closed here:
            // `finish_lir_defer_scope` emits the scope's cleanup and resume
            // blocks, and it switches blocks itself rather than appending to
            // the filled one.
            //
            // Nothing else may run. In particular a cooperative defer flush
            // emits instructions into the CURRENT block, which the diverging
            // statement already filled; the defers on that path are run by the
            // panic cleanup CFG instead, which is where a recovering scope
            // rejoins the LIR graph.
            if self.terminated
                && !(coop.is_none() && matches!(inst, LirInst::LeaveDeferScope { .. }))
            {
                continue;
            }
            // Debug builds report runtime-raised faults (array bounds, a
            // blocked channel op) at the location of the code that ran, so the
            // LIR path must publish its own site too. Without this the fault
            // would inherit the caller's statement (willow-s9ej.7 review).
            if let Some(span) = lir_inst_span(inst) {
                self.fault_site_span = Some(span);
            }
            match inst {
                LirInst::Compute { local, value, span } => {
                    if task_stack_boundary(value)
                        && let Some((suspends, frame)) = coop.as_mut()
                    {
                        let symbol = task_boundary_symbol(function, block.id.0, inst_index);
                        self.emit_task_boundary(Some(value), &symbol, suspends, *frame);
                        continue;
                    }
                    let result = self.emit_lir_rvalue(function, value, *span);
                    if !self.terminated
                        && (function.locals[local.0 as usize].is_gc_owner()
                            || !matches!(
                                function.locals[local.0 as usize].ty,
                                Type::Void | Type::Never
                            ))
                    {
                        self.store_lir_local(function, *local, result);
                    }
                }
                LirInst::EnterDeferScope {
                    sites,
                    resume,
                    lock,
                } => {
                    if coop.is_none()
                        && let Some(resume) = resume
                    {
                        // Recovery leaves this scope, so its LIR continuation
                        // inherits the state immediately before the push.
                        recovery_states.push((
                            resume.0,
                            LirDeferState {
                                scopes: defers.scopes.clone(),
                                entries: self.defer_stack.clone(),
                                panic_scopes: self.panic_scopes.clone(),
                                flags: self.sync_defer_flags.clone(),
                            },
                        ));
                    }
                    let saved_flags = self.sync_defer_flags.clone();
                    if coop.is_none() {
                        for (_, span) in sites {
                            let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                                StackSlotKind::ExplicitSlot,
                                8,
                                0,
                            ));
                            let zero = self.builder.ins().iconst(types::I64, 0);
                            self.stack_store(zero, slot);
                            self.sync_defer_flags.insert(*span, slot);
                        }
                    }
                    let roots_before = self.gc_root_count;
                    let defer_depth = self.defer_stack.len();
                    // A `lock` body's scope owns the critical section
                    // (willow-0g8j.2.13). Registering it HERE rather than at the
                    // acquisition is what makes the panic path work: the cleanup
                    // finds the section by this scope's `defer_depth`, and that
                    // depth counts scopes in block-INDEX order, which a
                    // suspension split reorders relative to the source — the
                    // acquisition can be emitted after the body it opens.
                    //
                    // Taking the cleanup order before the loop below is what
                    // orders an unwind: the cancel entry walks registrations
                    // backwards, so the section's own defers, registered after
                    // this, run before the release rather than after it.
                    let owns_lock = coop.is_some() && lock.is_some();
                    if let (true, Some(slots)) = (coop.is_some(), lock) {
                        let offsets = self.lir_lock_offsets(slots);
                        let order = self.next_cleanup_order();
                        self.collected_lock_sites.push(super::AsyncLockSite {
                            mode: slots.mode,
                            handle_offset: offsets[0],
                            token_offset: offsets[1],
                            phase_offset: offsets[2],
                            value_offset: offsets[3],
                            value_ty: slots.value_ty.clone(),
                            order,
                        });
                        self.lock_scopes.push(super::CoopLockScope {
                            mode: slots.mode,
                            handle_offset: offsets[0],
                            token_offset: offsets[1],
                            phase_offset: offsets[2],
                            value_offset: offsets[3],
                            value_ty: slots.value_ty.clone(),
                            defer_depth,
                        });
                    }
                    let mut entries = Vec::new();
                    if coop.is_some() {
                        for (site_id, _) in sites {
                            let body = function
                                .blocks
                                .iter()
                                .flat_map(|block| &block.instrs)
                                .find_map(|inst| match inst {
                                    LirInst::Defer { id, body, .. } if id == site_id => Some(body),
                                    _ => None,
                                })
                                .expect("LIR defer site has no registration instruction");
                            let action = body.clone();
                            let bindings: Vec<_> = self
                                .vars
                                .iter()
                                .filter_map(|(name, storage)| match storage {
                                    VarStorage::Frame { offset, ty } => {
                                        Some((name.clone(), *offset, ty.clone()))
                                    }
                                    _ => None,
                                })
                                .collect();
                            let flag_offset = self.lir_defer_offsets[site_id];
                            let recovery_capable = body.contains_recover();
                            let order = self.next_cleanup_order();
                            self.collected_defer_sites.push(super::AsyncDeferSite {
                                action: action.clone(),
                                flag_offset,
                                bindings: bindings.clone(),
                                recovery_capable,
                                order,
                            });
                            let id = self.defer_counter;
                            self.defer_counter += 1;
                            entries.push(super::DeferEntry {
                                id,
                                action,
                                flag_offset: Some(flag_offset),
                                sync_flag_slot: None,
                                bindings,
                                vars_at_registration: self.vars.clone(),
                                recovery_capable,
                            });
                        }
                    }
                    self.defer_stack.push(entries);
                    let normal_resume = self.builder.create_block();
                    let scope = super::PanicScope {
                        call_frames_at_entry: self.lir_call_frames.clone(),
                        reference_scopes_at_entry: self.lir_reference_scopes.clone(),
                        cleanup: self.builder.create_block(),
                        resume: resume
                            .map(|resume| blocks[resume.0])
                            .unwrap_or(normal_resume),
                        root_depth_at_entry: if coop.is_some() {
                            self.panic_function_root_depth
                                .expect("cooperative poll root depth snapshot")
                        } else {
                            self.emit_value_runtime_call("willow_root_depth", &[])
                        },
                        defer_depth,
                        vars_before: self.vars.clone(),
                        coop_root_depth_at_entry: coop.as_ref().map(|_| self.coop_root_depth()),
                    };
                    self.panic_scopes.push(scope.clone());
                    let frame = LirDeferScopeFrame {
                        id: (defers.block_index, inst_index),
                        scope,
                        normal_resume,
                        sites: sites.iter().map(|(id, _)| *id).collect(),
                        roots_before,
                        saved_flags,
                        owns_lock,
                    };
                    if coop.is_none() {
                        defers.ledger.opened.push(frame.clone());
                    }
                    defers.scopes.push(frame);
                }
                LirInst::Defer { id, body, span } => {
                    let action = body.clone();
                    let slot = coop
                        .is_none()
                        .then(|| self.sync_defer_flags.get(span).copied())
                        .flatten();
                    let flag_offset = coop.as_ref().map(|_| self.lir_defer_offsets[id]);
                    if let (Some(frame), Some(offset)) = (self.coop_frame, flag_offset) {
                        let one = self.builder.ins().iconst(types::I64, 1);
                        self.builder
                            .ins()
                            .store(MemFlagsData::new(), one, frame, offset);
                    } else if let Some(slot) = slot {
                        let one = self.builder.ins().iconst(types::I64, 1);
                        self.stack_store(one, slot);
                    }
                    if coop.is_none() {
                        let id = self.defer_counter;
                        self.defer_counter += 1;
                        self.defer_stack
                            .last_mut()
                            .expect("LIR defer outside scope")
                            .push(super::DeferEntry {
                                id,
                                action,
                                flag_offset,
                                sync_flag_slot: slot,
                                bindings: Vec::new(),
                                vars_at_registration: self.vars.clone(),
                                recovery_capable: body.contains_recover(),
                            });
                    }
                }
                LirInst::LeaveDeferScope { sites } => {
                    if coop.is_some() {
                        self.emit_lir_defer_sites(function, sites);
                    } else if let Some(frame) = defers.scopes.pop() {
                        defers.ledger.closed.insert(frame.id);
                        self.finish_lir_defer_scope(frame, &mut defers.ledger.cleanups);
                    } else {
                        panic!("LIR defer scope underflow");
                    }
                }
                LirInst::FlushDefers { sites } => {
                    if coop.is_some() {
                        self.emit_lir_defer_sites(function, sites);
                    } else {
                        self.emit_lir_sync_flush(sites, defers.scopes, defers.ledger);
                    }
                }
                LirInst::ClearScopeRoots { locals } => {
                    self.emit_lir_clear_scope_roots(function, locals);
                }
                // Commit the protected value and hand the lock back
                // (willow-0g8j.2.13). Idempotent at run time: the release is
                // guarded by the handle slot, which it clears, so a path that
                // reaches it after a recovered panic already released finds
                // nothing to do.
                LirInst::ReleaseLock(slots) => {
                    let offsets = self.lir_lock_offsets(slots);
                    self.emit_lock_frame_cleanup(slots.mode, offsets, &slots.value_ty, false);
                }
                LirInst::Let {
                    local, ty, value, ..
                } => {
                    let val = self.emit_lir_operand_store(function, value, ty);
                    self.store_lir_local(function, *local, val);
                }
                LirInst::Assign { local, value, .. } => {
                    let target = function.locals[local.0 as usize].ty.clone();
                    let val = self.emit_lir_operand_store(function, value, &target);
                    self.store_lir_local(function, *local, val);
                }
                LirInst::Unsupported { reason, .. } => {
                    unreachable!("rejected LIR reached backend: {reason}")
                }
                // One arm of a `match` lowering split into blocks
                // (willow-0g8j.2.11.1). Both read the scrutinee back out of
                // its own local, so every arm tests and destructures the same
                // value however many blocks the dispatch chain spans.
                LirInst::MatchTest {
                    scrutinee,
                    pattern,
                    result,
                    ..
                } => {
                    let scrutinee_ty = function.locals[scrutinee.0 as usize].ty.clone();
                    let value = self.load_lir_local(function, *scrutinee);
                    let matched = self.emit_flat_pattern_check(value, &scrutinee_ty, pattern);
                    self.store_lir_local(function, *result, matched);
                }
                LirInst::MatchBind {
                    scrutinee,
                    pattern,
                    bindings,
                    ..
                } => {
                    let scrutinee_ty = function.locals[scrutinee.0 as usize].ty.clone();
                    let value = self.load_lir_local(function, *scrutinee);
                    let bound = self.flat_pattern_binding_values(value, &scrutinee_ty, pattern);
                    debug_assert_eq!(
                        bound.len(),
                        bindings.len(),
                        "a `match` arm's bindings must line up with the locals lowering declared"
                    );
                    for ((_, _, val), local) in bound.into_iter().zip(bindings) {
                        self.store_lir_local(function, *local, val);
                    }
                }
                LirInst::SelectInit { .. }
                | LirInst::SelectProbe { .. }
                | LirInst::SelectPick { .. }
                | LirInst::SelectUnregister { .. }
                | LirInst::SelectCommit { .. } => self.emit_lir_select_instruction(function, inst),
            }
        }
        if self.terminated {
            return recovery_states;
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
                let c = self.emit_lir_operand(function, cond);
                self.builder
                    .ins()
                    .brif(c, blocks[then_block.0], &[], blocks[else_block.0], &[]);
            }
            Terminator::CleanupReturn => {
                let (exit, roots, _) = self
                    .lir_cleanup_exit
                    .expect("cleanup terminator outside cleanup region");
                self.emit_pop_roots_n(self.gc_root_count - roots);
                self.builder.ins().jump(exit, &[]);
                self.lir_cleanup_exit.as_mut().unwrap().2 = true;
                self.terminated = true;
            }
            Terminator::Return(v) => {
                self.emit_lir_operand_return(function, v.as_ref(), return_type);
            }
            Terminator::Suspend { operation, resume } => {
                let Some((suspends, frame)) = coop.as_mut() else {
                    unreachable!("explicit LIR suspension reached the synchronous emitter")
                };
                self.emit_lir_suspend(function, operation, *resume, blocks, suspends, *frame);
            }
        }
        recovery_states
    }

    /// The function-exit sequence: pop every root this function pushed, then
    /// return. Shared by a block's `Return` terminator and by a `return` inside
    /// a `match` arm (willow-0g8j.2.5), which is why it sets `self.terminated`
    /// — the arm's Cranelift block ends here.
    ///
    /// `gc_root_count` is deliberately NOT decremented: the pops emitted here
    /// belong to this path only, and the counter still describes what the paths
    /// that did not return are holding.
    /// The four async-frame offsets of a critical section's slots, in the order
    /// the runtime hooks take them: handle, token, phase, protected value.
    ///
    /// Every one is frame-backed by construction — the acquisition reports them
    /// to LIR liveness, which is what gives a local a slot (willow-0g8j.2.13).
    fn lir_lock_offsets(&self, slots: &LirLockSlots) -> [i32; 4] {
        slots.locals().map(|local| self.lir_frame_offsets[&local])
    }

    fn finish_lir_defer_scope(
        &mut self,
        frame: LirDeferScopeFrame,
        pending_seals: &mut Vec<cranelift_codegen::ir::Block>,
    ) {
        let LirDeferScopeFrame {
            id: _,
            scope,
            normal_resume,
            sites: _,
            roots_before,
            saved_flags,
            owns_lock,
        } = frame;
        debug_assert!(!owns_lock, "a `lock` body scope is cooperative by E2603");
        if !self.terminated {
            self.emit_flush_defers_from(scope.defer_depth);
        }
        let mut normal_reaches_resume = false;
        if !self.terminated {
            let roots = self.gc_root_count - roots_before;
            if roots > 0 {
                self.emit_pop_roots_n(roots);
            }
            self.builder.ins().jump(normal_resume, &[]);
            normal_reaches_resume = true;
        }
        self.emit_shared_panic_cleanup(&scope);
        pending_seals.push(scope.cleanup);
        self.panic_scopes.pop();
        self.defer_stack.pop();
        self.gc_root_count = roots_before;
        self.sync_defer_flags = saved_flags;
        let recovered = self.panic_recovery_targets.remove(&scope.resume);
        if normal_reaches_resume {
            self.builder.switch_to_block(normal_resume);
            self.builder.seal_block(normal_resume);
            self.terminated = false;
        } else if recovered && normal_resume == scope.resume {
            self.builder.switch_to_block(scope.resume);
            self.builder.seal_block(scope.resume);
            self.terminated = false;
        } else {
            self.terminated = true;
        }
    }

    /// Emit only the abnormal edge for an async LIR scope. Its normal cleanup
    /// is represented by `LeaveDeferScope`, while recovery branches directly
    /// to the scope's explicit LIR resume block.
    fn finish_lir_async_panic_scope(&mut self, frame: LirDeferScopeFrame) {
        let LirDeferScopeFrame {
            id: _,
            scope,
            normal_resume: _,
            sites: _,
            roots_before,
            saved_flags,
            owns_lock,
        } = frame;
        // Emitted BEFORE the critical section is dropped from `lock_scopes`:
        // the cleanup is exactly where an unwind out of the section releases
        // the lock, and it finds it by this scope's depth (willow-0g8j.2.13).
        self.emit_shared_panic_cleanup(&scope);
        if owns_lock {
            self.lock_scopes.pop();
        }
        self.builder.seal_block(scope.cleanup);
        self.panic_scopes.pop();
        self.defer_stack.pop();
        self.gc_root_count = roots_before;
        self.sync_defer_flags = saved_flags;
        self.panic_recovery_targets.remove(&scope.resume);
        self.terminated = true;
    }

    /// Run the registrations a synchronous exit leaves behind (willow-0g8j.2.15).
    ///
    /// The cooperative twin above rebuilds each site from the LIR, because an
    /// async frame's registrations are named by heap flags and no Rust-side
    /// stack survives a poll return. A synchronous function keeps its scopes on
    /// `self.defer_stack` for exactly as long as they are open, so the flush is
    /// the ordinary [`FuncGen::emit_flush_defers_from`] — all this has to do is
    /// find the DEPTH it starts from.
    ///
    /// `sites` names every registration this exit runs, which is always the
    /// sites of a suffix of the open scopes: `return` leaves all of them,
    /// `break` and `continue` leave the ones inside the loop. Walking the open
    /// scopes from the innermost outwards while their sites are all named finds
    /// that suffix without the emitter having to know which statement it is.
    fn emit_lir_sync_flush(
        &mut self,
        sites: &[crate::ir::lowered::LirDeferId],
        open: &mut Vec<LirDeferScopeFrame>,
        ledger: &mut LirDeferLedger,
    ) {
        let open_sites: Vec<Vec<_>> = open.iter().map(|frame| frame.sites.clone()).collect();
        let flushed = lir_flushed_scope_count(sites, &open_sites);
        if flushed == 0 {
            return;
        }
        let depth = open[open.len() - flushed].scope.defer_depth;
        self.emit_flush_defers_from(depth);
        // The path taken here has left those scopes; only the paths that reach
        // their `LeaveDeferScope` still hold them. Record what the scope looked
        // like from here so the end-of-body sweep can emit the panic cleanup of
        // any scope no `LeaveDeferScope` ever closes.
        for _ in 0..flushed {
            let frame = open.pop().expect("flushed scope count is bounded by open");
            let state = LirDeferState {
                scopes: Vec::new(),
                entries: self.defer_stack.clone(),
                panic_scopes: self.panic_scopes.clone(),
                flags: self.sync_defer_flags.clone(),
            };
            ledger.dropped.insert(frame.id, state);
            self.defer_stack.truncate(frame.scope.defer_depth);
            self.panic_scopes.pop();
            self.sync_defer_flags = frame.saved_flags.clone();
        }
    }

    /// Emit exactly the LIR defer sites named by a CFG exit. Runtime flags
    /// decide which registrations on that source path are active, so this is
    /// independent of Rust code-generation order between basic blocks.
    fn emit_lir_defer_sites(
        &mut self,
        function: &LirFunction,
        sites: &[crate::ir::lowered::LirDeferId],
    ) {
        if sites.is_empty() || self.coop_frame.is_none() {
            return;
        }
        let mut entries = Vec::with_capacity(sites.len());
        for site_id in sites {
            let body = function
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .find_map(|inst| match inst {
                    LirInst::Defer { id, body, .. } if id == site_id => Some(body),
                    _ => None,
                })
                .expect("LIR defer exit references an unknown site");
            let action = body.clone();
            let bindings = self
                .vars
                .iter()
                .filter_map(|(name, storage)| match storage {
                    VarStorage::Frame { offset, ty } => Some((name.clone(), *offset, ty.clone())),
                    _ => None,
                })
                .collect();
            let id = self.defer_counter;
            self.defer_counter += 1;
            entries.push(super::DeferEntry {
                id,
                action,
                flag_offset: Some(self.lir_defer_offsets[site_id]),
                sync_flag_slot: None,
                bindings,
                vars_at_registration: self.vars.clone(),
                recovery_capable: body.contains_recover(),
            });
        }
        let depth = self.defer_stack.len();
        self.defer_stack.push(entries);
        self.emit_flush_defers_from(depth);
        self.defer_stack.pop();
    }

    fn emit_lir_operand_store(
        &mut self,
        function: &LirFunction,
        value: &crate::ir::lowered::LirOperand,
        target: &Type,
    ) -> cranelift_codegen::ir::Value {
        let source = value.ty(&function.locals).expect("validated value operand");
        let result = self.emit_lir_operand(function, value);
        self.coerce_to_target(result, &source, target)
    }

    fn emit_lir_operand_return(
        &mut self,
        function: &LirFunction,
        value: Option<&crate::ir::lowered::LirOperand>,
        return_type: &Type,
    ) {
        if let Some(frame) = self.coop_frame {
            if let (Some(value), Some(offset)) = (value, self.coop_result_offset) {
                let result = self.emit_lir_operand_store(function, value, return_type);
                self.emit_gc_heap_store(
                    frame,
                    offset,
                    result,
                    return_type,
                    GcStoreDestination::AsyncFrameSlot,
                );
            } else if let Some(value) = value {
                // Async main is `void`, but preserve source evaluation if a
                // future front-end shape carries a value with no result slot.
                self.emit_lir_operand(function, value);
            }
            self.emit_flush_defers_from(0);
            if !self.terminated {
                // A return nested inside a LIR expression (notably a
                // statement-position `match` arm) may still hold temporary
                // roots owned by that expression. They do not survive this
                // poll return, so pop the complete runtime depth here; keep
                // the compile-time count intact for sibling CFG paths.
                self.emit_callstack_unwind_edge();
                self.emit_pop_roots_n(self.gc_root_count);
                let ready = self.builder.ins().iconst(types::I32, 1);
                self.builder.ins().return_(&[ready]);
                self.terminated = true;
            }
            return;
        }
        // `fn main() -> Result<void, E>`: `willow_user_main` is void, so a
        // `return` is an EXIT rather than a value handed back -- `Err` reports
        // its payload and exits non-zero, `Ok` exits 0 (willow-exg, willow-0g8j.2.14).
        if self.main_result_err_ty.is_some() {
            match value {
                Some(value) => {
                    let result = self.emit_lir_operand(function, value);
                    // The `Result` object outlives any allocating defer between
                    // here and the exit that reads its tag.
                    self.emit_push_root(result);
                    self.emit_flush_defers_from(0);
                    if self.terminated {
                        return;
                    }
                    self.emit_pop_roots_n(1);
                    self.gc_root_count -= 1;
                    // Pops the function's remaining roots on both of its arms.
                    self.emit_callstack_unwind_edge();
                    self.emit_main_result_exit(result);
                }
                // `return Result::Ok();` and a bare `return` are the same
                // success, and neither needs the object built to say so.
                None => {
                    self.emit_flush_defers_from(0);
                    if self.terminated {
                        return;
                    }
                    self.emit_callstack_unwind_edge();
                    self.emit_pop_roots_n(self.gc_root_count);
                    self.builder.ins().return_(&[]);
                }
            }
            self.terminated = true;
            return;
        }
        match value {
            Some(v) => {
                // Evaluate (and box, for an interface-typed return) first: the
                // value may read through a rooted local, and the box allocates.

                let val = self.emit_lir_operand_store(function, v, return_type);
                self.emit_callstack_unwind_edge();
                self.emit_pop_roots_n(self.gc_root_count);
                self.builder.ins().return_(&[val]);
            }
            None => {
                self.emit_callstack_unwind_edge();
                self.emit_pop_roots_n(self.gc_root_count);
                if *return_type == Type::Void {
                    self.builder.ins().return_(&[]);
                } else {
                    // Unreachable fall-through in a value function (the checker
                    // guarantees returns); satisfy the signature with a zero.
                    let zero =
                        match clif_type(reference_type(self.module.target_config()), return_type) {
                            types::F64 => self.builder.ins().f64const(0.0),
                            ty => self.builder.ins().iconst(ty, 0),
                        };
                    self.builder.ins().return_(&[zero]);
                }
            }
        }
        self.terminated = true;
    }

    fn emit_flat_pattern_check(
        &mut self,
        scrutinee: cranelift_codegen::ir::Value,
        scrutinee_ty: &Type,
        pattern: &crate::ir::lowered::LirPattern,
    ) -> cranelift_codegen::ir::Value {
        match pattern {
            crate::ir::lowered::LirPattern::Wildcard
            | crate::ir::lowered::LirPattern::Binding { .. } => {
                self.builder.ins().iconst(types::I8, 1)
            }
            crate::ir::lowered::LirPattern::LiteralBool(b) => {
                let expected = self.builder.ins().iconst(types::I8, i64::from(*b));
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            crate::ir::lowered::LirPattern::LiteralInt(n) => {
                let expected = self.builder.ins().iconst(types::I64, *n);
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            crate::ir::lowered::LirPattern::EnumVariant { enum_name, variant }
            | crate::ir::lowered::LirPattern::EnumVariantTuple {
                enum_name, variant, ..
            } => {
                let tag = self.enum_variant_tag(&enum_name.to_string(), variant);
                // The `Option` pointer niche carries no tag word: `Some` is any
                // non-null payload and `None` is null (willow-0g8j.2.1).
                if builtin_types::is(scrutinee_ty, B::Option)
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
                let actual = if self.enum_is_gc_object_type(&enum_name.to_string()) {
                    self.emit_load_enum_tag(scrutinee)
                } else {
                    scrutinee
                };
                self.builder.ins().icmp(IntCC::Equal, actual, expected)
            }
            // The scrutinee is an interface box `{object@0, vtable@8}`. Match
            // when the boxed object's runtime `type_id` — read through the
            // class descriptor its word 0 points at — equals the pattern
            // class's. Exactly [`FuncGen::emit_pattern_check`]'s
            // `Pattern::ClassDowncast`, including its exactness: a subclass
            // instance does NOT match its base's arm on either backend.
            crate::ir::lowered::LirPattern::ClassDowncast { class_name, .. } => {
                let type_id = self.class_type_ids.get(class_name).copied().unwrap_or_else(|| {
                    panic!(
                        "compiler invariant violated: checked downcast pattern class `{class_name}` has no type id"
                    )
                });
                let obj = self.builder.ins().load(
                    reference_type(self.module.target_config()),
                    MemFlagsData::new(),
                    scrutinee,
                    0i32,
                );
                let actual = self.emit_load_runtime_type_id(obj);
                let expected = self.builder.ins().iconst(types::I64, type_id);
                self.builder.ins().icmp(IntCC::Equal, actual, expected)
            }
        }
    }

    fn flat_pattern_binding_values(
        &mut self,
        scrutinee: cranelift_codegen::ir::Value,
        scrutinee_ty: &Type,
        pattern: &crate::ir::lowered::LirPattern,
    ) -> Vec<(String, Type, cranelift_codegen::ir::Value)> {
        match pattern {
            crate::ir::lowered::LirPattern::Binding { name, ty } => {
                vec![(name.clone(), ty.clone(), scrutinee)]
            }
            crate::ir::lowered::LirPattern::EnumVariantTuple {
                enum_name,
                variant,
                bindings,
            } => {
                // Read the DECLARED payload types rather than the pattern's
                // recorded ones, so the load width follows the layout the
                // constructor wrote.
                let mut payload_types = self.resolve_variant_payload_types(
                    &enum_name.to_string(),
                    variant,
                    scrutinee_ty,
                );
                normalize_void_payloads(&mut payload_types);
                let niche = builtin_types::is(scrutinee_ty, B::Option)
                    && option_repr(scrutinee_ty, self.enum_infos)
                        == Some(OptionRepr::NullableGcPointer);
                let mut out = Vec::with_capacity(payload_types.len());
                for (i, ((name, _), payload_ty)) in
                    bindings.iter().zip(payload_types.iter()).enumerate()
                {
                    let clif_ty =
                        clif_type(reference_type(self.module.target_config()), payload_ty);
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
                    out.push((name.clone(), payload_ty.clone(), val));
                }
                out
            }
            // The arm only runs when the check above proved the box holds this
            // class, so the binding is the box's object word, unboxed. It stays
            // reachable through the rooted scrutinee for the whole arm.
            crate::ir::lowered::LirPattern::ClassDowncast {
                binding,
                binding_ty,
                ..
            } => {
                let obj = self.builder.ins().load(
                    reference_type(self.module.target_config()),
                    MemFlagsData::new(),
                    scrutinee,
                    0i32,
                );
                vec![(binding.clone(), binding_ty.clone(), obj)]
            }
            crate::ir::lowered::LirPattern::Wildcard
            | crate::ir::lowered::LirPattern::LiteralBool(_)
            | crate::ir::lowered::LirPattern::LiteralInt(_)
            | crate::ir::lowered::LirPattern::EnumVariant { .. } => Vec::new(),
        }
    }

    pub(super) fn emit_lir_operand(
        &mut self,
        function: &LirFunction,
        operand: &crate::ir::lowered::LirOperand,
    ) -> cranelift_codegen::ir::Value {
        use crate::ir::lowered::LirOperand;
        match operand {
            LirOperand::Local(local) => self.load_lir_local(function, *local),
            LirOperand::Int(value) => self.builder.ins().iconst(types::I64, *value),
            LirOperand::Float(value) => self.builder.ins().f64const(*value),
            LirOperand::Bool(value) => self.builder.ins().iconst(types::I8, i64::from(*value)),
            LirOperand::Reference { place, .. } => {
                self.emit_flat_reference_address(function, place)
            }
        }
    }

    fn emit_lir_rvalue(
        &mut self,
        function: &LirFunction,
        value: &crate::ir::lowered::LirRvalue,
        span: Span,
    ) -> cranelift_codegen::ir::Value {
        use crate::ir::lowered::LirRvalue;
        match value {
            LirRvalue::BeginReferenceCall => {
                self.emit_debug_reference_call_scope_push();
                self.lir_reference_scopes.push(Vec::new());
                self.builder.ins().iconst(types::I8, 0)
            }
            LirRvalue::ReferenceDebug {
                argument,
                callee,
                index,
            } => {
                self.emit_flat_reference_debug(argument, callee, *index);
                self.lir_reference_scopes
                    .last_mut()
                    .expect("prepared reference scope")
                    .push(FlatReferenceDebug {
                        argument: argument.clone(),
                        callee: *callee,
                        index: *index,
                    });
                self.builder.ins().iconst(types::I8, 0)
            }
            LirRvalue::StartTask {
                callee,
                args,
                params,
                ..
            } => {
                let args: Vec<_> = args
                    .iter()
                    .map(|arg| self.emit_lir_operand(function, arg))
                    .collect();
                self.emit_flat_start_task(*callee, &args, params, span)
            }
            LirRvalue::AwaitFuture { future, result } => {
                let future = self.emit_lir_operand(function, future);
                self.emit_flat_await_future(future, result)
            }
            LirRvalue::SelectIdleWait { deadlines } => {
                let deadlines: Vec<_> = deadlines
                    .iter()
                    .map(|deadline| self.emit_lir_operand(function, deadline))
                    .collect();
                self.emit_flat_select_idle_wait(&deadlines)
            }
            LirRvalue::PrepareMethod {
                receiver,
                receiver_ty,
                method,
            } => {
                let receiver = self.emit_lir_operand(function, receiver);
                let receiver =
                    self.emit_flat_prepare_method(receiver, receiver_ty, method, span, true);
                if self.flat_method_frame_enabled(method) {
                    self.lir_call_frames.push((method.clone(), span));
                }
                receiver
            }
            LirRvalue::MethodCall {
                receiver,
                receiver_ty,
                method,
                args,
                result,
                ..
            } => {
                if self.flat_method_frame_enabled(method) {
                    let (prepared, _) = self.lir_call_frames.pop().expect("prepared method frame");
                    assert_eq!(prepared, *method);
                }
                // PrepareMethod snapshots the receiver before evaluating arguments.
                // A GC stack local is a direct root until after this instruction,
                // including when the original binding is reassigned by the call.
                // Heap-frame fields and borrowed storage do not pin an SSA alias.
                let receiver_rooted = match receiver {
                    crate::ir::lowered::LirOperand::Local(id) => matches!(
                        self.vars.get(&function.locals[id.0 as usize].name),
                        Some(VarStorage::Stack { ty, .. })
                            if is_gc_managed(ty, self.enum_infos)
                    ),
                    _ => false,
                };
                let receiver = self.emit_lir_operand(function, receiver);
                let (args, roots) = self.emit_flat_call_operands(function, args);
                let result = if matches!(receiver_ty, Type::Named(name) | Type::Generic(name, _) if self.interface_infos.contains_key(name))
                {
                    self.emit_flat_interface_call(
                        receiver,
                        receiver_ty,
                        method,
                        &args,
                        span,
                        true,
                        true,
                    )
                } else {
                    self.emit_flat_class_method(
                        receiver,
                        receiver_ty,
                        method,
                        &args,
                        result,
                        span,
                        receiver_rooted,
                        true,
                    )
                };
                self.emit_pop_roots_n(roots);
                self.gc_root_count -= roots;
                result
            }
            LirRvalue::Coerce {
                value,
                source,
                target,
            } => {
                let value = self.emit_lir_operand(function, value);
                self.coerce_to_target(value, source, target)
            }
            LirRvalue::ArrayAlloc { length, element } => {
                self.emit_flat_array_alloc(*length, element)
            }
            LirRvalue::CaptureArrayOwner { array, index } => {
                self.emit_flat_capture_array_owner(function, array, index)
            }
            LirRvalue::ArrayStore {
                array,
                index,
                value,
                element,
            } => self.emit_flat_array_store(function, array, index, value, element),
            LirRvalue::Index {
                array,
                index,
                element,
            } => self.emit_flat_index(function, array, index, element),
            LirRvalue::ObjectAlloc { class } => self.emit_flat_object_alloc(class),
            LirRvalue::FieldLoad {
                object,
                object_ty,
                field,
                ..
            } => self.emit_flat_field(function, object, field, object_ty),
            LirRvalue::FieldStore {
                object,
                object_ty,
                field,
                value,
            } => self.emit_flat_field_store(function, object, field, value, object_ty),
            LirRvalue::StaticField { class, field, .. } => {
                self.emit_flat_static_field(&class.to_string(), field)
            }
            LirRvalue::StaticStore {
                class,
                field,
                value,
            } => self.emit_flat_static_store(function, &class.to_string(), field, value),
            LirRvalue::StaticCall {
                class,
                method,
                args,
                arg_types,
                result,
            } => {
                let (args, roots) = self.emit_flat_call_operands(function, args);
                let result = self.emit_lir_static_call_values(
                    &class.to_string(),
                    method,
                    &args,
                    arg_types,
                    result,
                    span,
                );
                if !self.terminated {
                    self.emit_pop_roots_n(roots);
                }
                self.gc_root_count -= roots;
                result
            }
            LirRvalue::EnumAlloc {
                class,
                variant,
                enum_ty,
            } => self.emit_flat_enum_alloc(&class.to_string(), variant, enum_ty),
            LirRvalue::EnumPayloadStore {
                object,
                class,
                variant,
                index,
                value,
                source,
                enum_ty,
            } => {
                let object = self.emit_lir_operand(function, object);
                let value = self.emit_lir_operand(function, value);
                self.emit_flat_enum_payload_store(
                    object,
                    &class.to_string(),
                    variant,
                    *index,
                    value,
                    source,
                    enum_ty,
                )
            }
            LirRvalue::ConstructorCall {
                object,
                class,
                args,
                arg_types,
            } => self.emit_flat_constructor_call(function, object, class, args, arg_types, span),
            LirRvalue::Range { start, end } => {
                let start = self.emit_lir_operand(function, start);
                let end = self.emit_lir_operand(function, end);
                let ptr = self.emit_gc_alloc(GcLayoutMetadata::new(GcObjectKind::Range, 16, 0, 0));
                self.builder.ins().store(MemFlagsData::new(), start, ptr, 0);
                self.builder.ins().store(MemFlagsData::new(), end, ptr, 8);
                ptr
            }
            LirRvalue::EnumMethod {
                receiver,
                receiver_ty,
                method,
                args,
                arg_types,
                ..
            } => {
                let receiver = self.emit_lir_operand(function, receiver);
                let args: Vec<_> = args
                    .iter()
                    .map(|arg| self.emit_lir_operand(function, arg))
                    .collect();
                self.emit_flat_enum_method(receiver, receiver_ty, method, &args, arg_types, span)
            }
            LirRvalue::BuiltinCall { callee, args, .. } => {
                let args: Vec<_> = args
                    .iter()
                    .map(|arg| self.emit_lir_operand(function, arg))
                    .collect();
                self.emit_flat_builtin_call(callee, &args)
            }
            LirRvalue::FormatScalar { value, ty, format } => {
                let value = self.emit_lir_operand(function, value);
                self.emit_flat_format_scalar(value, ty, *format)
            }
            LirRvalue::Panic { message } => {
                let message = self.emit_lir_operand(function, message);
                self.emit_panic_with_message(message, span)
            }
            LirRvalue::Recover => self.emit_recover_call(),
            LirRvalue::RebindResultError { value, .. } => self.emit_lir_operand(function, value),
            LirRvalue::IntoError { value, source, .. } => {
                let value = self.emit_lir_operand(function, value);
                self.emit_push_root(value);
                let panic_depth = self.emit_pre_willow_call_panic_depth();
                let Type::Named(class) = source else {
                    unreachable!("Into error source class validated");
                };
                let converted = self.emit_into_conversion(value, &class.to_string());
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
                self.emit_post_willow_call_panic_check(panic_depth);
                converted
            }
            LirRvalue::IntrinsicCall {
                intrinsic,
                receiver,
                receiver_ty,
                args,
                arg_types,
                result,
                ..
            } => {
                let receiver = self.emit_lir_operand(function, receiver);
                let args: Vec<_> = args
                    .iter()
                    .map(|arg| self.emit_lir_operand(function, arg))
                    .collect();
                self.emit_flat_intrinsic(
                    *intrinsic,
                    receiver,
                    receiver_ty,
                    &args,
                    arg_types,
                    result,
                    span,
                )
            }
            LirRvalue::Use(operand) => self.emit_lir_operand(function, operand),
            LirRvalue::StringLiteral(value) => self.emit_string_literal(value),
            LirRvalue::FunctionRef { function, .. } => {
                let fid = *self
                    .func_ids
                    .get_id(function)
                    .expect("registered function reference");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder
                    .ins()
                    .func_addr(reference_type(self.module.target_config()), fref)
            }
            LirRvalue::Closure { id, captures, ty } => {
                let name = self.lambda_names[id];
                let fid = *self.func_ids.get_id(&name).expect("registered lambda");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                let code = self
                    .builder
                    .ins()
                    .func_addr(reference_type(self.module.target_config()), fref);
                if !matches!(ty, Type::Closure(..)) {
                    return code;
                }
                let mut mask = 0u64;
                for (i, capture) in captures.iter().enumerate() {
                    if is_gc_managed(
                        &capture.ty(&function.locals).expect("capture type"),
                        self.enum_infos,
                    ) {
                        mask |= 1u64 << (i + 1);
                    }
                }
                let env = self.emit_gc_alloc(GcLayoutMetadata::new(
                    GcObjectKind::Closure,
                    (captures.len() as i64 + 1)
                        * willow_abi::storage_word_bytes(
                            reference_type(self.module.target_config()).bytes(),
                        ) as i64,
                    0,
                    mask,
                ));
                self.emit_gc_heap_store_classified(
                    env,
                    0,
                    code,
                    false,
                    GcStoreDestination::ObjectField,
                );
                for (i, capture) in captures.iter().enumerate() {
                    let value = self.emit_lir_operand(function, capture);
                    let ty = capture.ty(&function.locals).expect("capture type");
                    self.emit_gc_heap_store(
                        env,
                        (i as i32 + 1)
                            * willow_abi::storage_word_bytes(
                                reference_type(self.module.target_config()).bytes(),
                            ) as i32,
                        value,
                        &ty,
                        GcStoreDestination::ObjectField,
                    );
                }
                env
            }
            LirRvalue::Print { value, ty, newline } => {
                let value = self.emit_lir_operand(function, value);
                let name = match (ty, newline) {
                    (Type::I64, false) => "willow_print_i64",
                    (Type::I64, true) => "willow_println_i64",
                    (Type::F64, false) => "willow_print_f64",
                    (Type::F64, true) => "willow_println_f64",
                    (Type::Bool, false) => "willow_print_bool",
                    (Type::Bool, true) => "willow_println_bool",
                    (Type::String, false) => "willow_print_string",
                    (Type::String, true) => "willow_println_string",
                    _ => unreachable!("print type validated"),
                };
                self.emit_void_runtime_call(name, &[value]);
                self.builder.ins().iconst(types::I8, 0)
            }
            LirRvalue::DirectCall { callee, args, .. } => {
                let has_references = args
                    .iter()
                    .any(|arg| matches!(arg, crate::ir::lowered::LirOperand::Reference { .. }));
                let (values, roots) = self.emit_flat_call_operands(function, args);
                let fid = *self
                    .func_ids
                    .get_id(callee)
                    .expect("flat direct call is registered");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                let pushed = self.emit_callstack_push(&callee.to_string(), span);
                let panic_depth = self.emit_pre_user_call_panic_depth(&callee.to_string());
                let call = self.builder.ins().call(fref, &values);
                let result = self
                    .builder
                    .inst_results(call)
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0));
                if pushed {
                    self.emit_callstack_pop();
                }
                if has_references {
                    self.emit_flat_reference_call_end();
                }
                self.emit_pop_roots_n(roots);
                self.gc_root_count -= roots;
                self.emit_post_willow_call_panic_check(panic_depth);
                result
            }
            LirRvalue::Unary { op, operand, ty } => {
                let operand = self.emit_lir_operand(function, operand);
                match op {
                    UnaryOp::Neg if *ty == Type::F64 => self.builder.ins().fneg(operand),
                    UnaryOp::Neg => self.builder.ins().ineg(operand),
                    UnaryOp::Not => {
                        let one = self.builder.ins().iconst(types::I8, 1);
                        self.builder.ins().bxor(operand, one)
                    }
                }
            }
            LirRvalue::IndirectCall {
                callee,
                name,
                args,
                params,
                result,
            } => {
                let closure = matches!(callee.ty(&function.locals), Some(Type::Closure(..)));
                let target = self.emit_lir_operand(function, callee);
                let mut roots = 0;
                if closure {
                    self.emit_push_root(target);
                    roots += 1;
                }
                let code = if closure {
                    self.builder.ins().load(
                        reference_type(self.module.target_config()),
                        MemFlagsData::trusted(),
                        target,
                        0,
                    )
                } else {
                    target
                };
                let mut values = Vec::with_capacity(args.len() + usize::from(closure));
                if closure {
                    values.push(target);
                }
                for (argument, ty) in args.iter().zip(params) {
                    let value = self.emit_lir_operand(function, argument);
                    if is_gc_managed(ty, self.enum_infos) {
                        self.emit_push_root(value);
                        roots += 1;
                    }
                    values.push(value);
                }
                let mut signature = self.module.make_signature();
                if closure {
                    signature
                        .params
                        .push(AbiParam::new(reference_type(self.module.target_config())));
                }
                signature.params.extend(params.iter().map(|ty| {
                    AbiParam::new(clif_type(reference_type(self.module.target_config()), ty))
                }));
                if *result != Type::Void {
                    signature.returns.push(AbiParam::new(clif_type(
                        reference_type(self.module.target_config()),
                        result,
                    )));
                }
                let signature = self.builder.import_signature(signature);
                let pushed = self.emit_callstack_push(&name.to_string(), span);
                let panic_depth = self.emit_pre_willow_call_panic_depth();
                let call = self.builder.ins().call_indirect(signature, code, &values);
                let result = self
                    .builder
                    .inst_results(call)
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0));
                if pushed {
                    self.emit_callstack_pop();
                }
                self.emit_pop_roots_n(roots);
                self.gc_root_count -= roots;
                self.emit_post_willow_call_panic_check(panic_depth);
                result
            }
            LirRvalue::Binary {
                op,
                lhs,
                rhs,
                operand_ty,
            } => {
                let lhs = self.emit_lir_operand(function, lhs);
                let rhs = self.emit_lir_operand(function, rhs);
                if *operand_ty == Type::String {
                    let name = if matches!(op, BinOp::Add) {
                        "willow_string_concat"
                    } else {
                        "willow_string_eq"
                    };
                    self.emit_push_root(lhs);
                    self.emit_push_root(rhs);
                    let raw = self.emit_value_runtime_call(name, &[lhs, rhs]);
                    self.emit_pop_roots_n(2);
                    self.gc_root_count -= 2;
                    return match op {
                        BinOp::Add => raw,
                        BinOp::Eq => self.builder.ins().ireduce(types::I8, raw),
                        BinOp::Ne => {
                            let raw = self.builder.ins().bxor_imm_s(raw, 1);
                            self.builder.ins().ireduce(types::I8, raw)
                        }
                        _ => unreachable!("string operator validated"),
                    };
                }
                let float = *operand_ty == Type::F64;
                if !float && matches!(op, BinOp::Div | BinOp::Rem) {
                    self.emit_int_div_guard(lhs, rhs, matches!(op, BinOp::Rem), span);
                }
                if matches!(op, BinOp::Pow) {
                    if float {
                        self.emit_pow_f64(lhs, rhs)
                    } else {
                        self.emit_pow_i64(lhs, rhs, span)
                    }
                } else {
                    self.emit_lir_binop(op, lhs, rhs, float)
                }
            }
        }
    }

    pub(super) fn lir_class_layout(&self, ty: &Type) -> Vec<(String, Type)> {
        let class =
            class_name_for_object_type(ty).expect("class receiver type vetted by LIR eligibility");
        // Resolved exactly as eligibility resolved it, so a bare module class
        // name reaches the layout the module registered (willow-0g8j.2.19).
        let key = resolve_class_key(
            self.class_layouts,
            self.class_type_ids,
            self.known_modules,
            self.visible_modules,
            &class,
        )
        .expect("class layout vetted by LIR eligibility");
        self.class_layouts
            .get(&key)
            .cloned()
            .expect("class layout vetted by LIR eligibility")
    }
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

/// Representation checks for resolved flat builtin methods. Receiver/result
/// identity and arity are checked against the semantic intrinsic table first.
fn flat_intrinsic_supported(
    intrinsic: Intrinsic,
    receiver: &Type,
    args: &[Type],
    result: &Type,
    ctx: &LirTypeCtx<'_>,
) -> bool {
    use Intrinsic::*;
    if !ctx.supported_type(receiver)
        || !ctx.supported_type(result)
        || !args.iter().all(|ty| ctx.supported_type(ty))
    {
        return false;
    }
    match intrinsic {
        ArrayPush | ChannelSend => {
            let element = match receiver {
                Type::Array(element) => &**element,
                _ => match builtin_types::unary_arg(receiver, B::Channel) {
                    Some(element) => element,
                    None => return false,
                },
            };
            args.len() == 1 && ctx.storable(element, &args[0])
        }
        ArrayToString => {
            matches!(receiver, Type::Array(element) if collection_elem_kind(element).is_some())
        }
        MapContains | FrozenMapContains | MapGet | FrozenMapGet | MapInsert | MapToString => {
            let Some((_, types)) = lir_collection(receiver) else {
                return false;
            };
            if types.len() != 2 {
                return false;
            }
            match intrinsic {
                MapToString => types.iter().all(|ty| collection_elem_kind(ty).is_some()),
                MapInsert => {
                    args.len() == 2
                        && ctx.same_repr(&types[0], &args[0])
                        && ctx.storable(&types[1], &args[1])
                }
                _ => args.len() == 1 && ctx.same_repr(&types[0], &args[0]),
            }
        }
        AtomicLoad | AtomicStore | AtomicSwap | AtomicAdd | AtomicSub => {
            atomic_cell(receiver).is_some_and(|cell| args.iter().all(|arg| *arg == cell.word()))
        }
        CellGet | CellSet | RwCellRead | RwCellWrite => blocking_cell(receiver)
            .is_some_and(|(_, elem)| args.iter().all(|arg| ctx.same_repr(elem, arg))),
        _ => true,
    }
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Operands are already evaluated, typed, and rooted by LIR. This emitter
    /// performs one resolved operation; it never walks executable syntax.
    // Keep explicit emission operands aligned with the LIR/runtime ABI.
    #[allow(clippy::too_many_arguments)]
    fn emit_flat_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        receiver: cranelift_codegen::ir::Value,
        receiver_ty: &Type,
        args: &[cranelift_codegen::ir::Value],
        arg_types: &[Type],
        result: &Type,
        span: Span,
    ) -> cranelift_codegen::ir::Value {
        // Frame-backed references can otherwise move during a coercion or a
        // blocking runtime call. Direct roots pin every loaded SSA operand.
        let roots_before = self.gc_root_count;
        if is_gc_managed(receiver_ty, self.enum_infos) {
            self.emit_push_root(receiver);
        }
        for (&value, ty) in args.iter().zip(arg_types) {
            if is_gc_managed(ty, self.enum_infos) {
                self.emit_push_root(value);
            }
        }
        let value = self.emit_flat_intrinsic_inner(
            intrinsic,
            receiver,
            receiver_ty,
            args,
            arg_types,
            result,
            span,
        );
        self.emit_pop_roots_n(self.gc_root_count - roots_before);
        self.gc_root_count = roots_before;
        value
    }

    // Keep explicit emission operands aligned with the LIR/runtime ABI.
    #[allow(clippy::too_many_arguments)]
    fn emit_flat_intrinsic_inner(
        &mut self,
        intrinsic: Intrinsic,
        receiver: cranelift_codegen::ir::Value,
        receiver_ty: &Type,
        args: &[cranelift_codegen::ir::Value],
        arg_types: &[Type],
        _result: &Type,
        _span: Span,
    ) -> cranelift_codegen::ir::Value {
        use Intrinsic::*;
        match intrinsic {
            StringToString | TaskResult => receiver,
            I64ToString => self.emit_value_runtime_call("willow_i64_to_string", &[receiver]),
            F64ToString => self.emit_value_runtime_call("willow_f64_to_string", &[receiver]),
            BoolToString => self.emit_value_runtime_call("willow_bool_to_string", &[receiver]),
            TaskCancel => {
                let id = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    receiver,
                    async_frame_slot_offset(
                        FRAME_SLOT_TASK_ID,
                        reference_type(self.module.target_config()).bytes(),
                    ),
                );
                self.emit_void_runtime_call("willow_sched_cancel", &[id]);
                self.builder.ins().iconst(types::I8, 0)
            }
            TaskIsCancelled => {
                let raw = self.emit_value_runtime_call("willow_frame_is_cancelled", &[receiver]);
                self.builder.ins().ireduce(types::I8, raw)
            }
            TokenIsCancelled | ScopeIsCancelled | TokenCancel | ScopeCancel | TokenChild
            | ScopeChild | TokenAttach | ScopeAdd | ScopeFinish => {
                let handle =
                    cancellation_handle(receiver_ty).expect("validated cancellation intrinsic");
                let suffix = match intrinsic {
                    TokenIsCancelled | ScopeIsCancelled => "is_cancelled",
                    TokenCancel | ScopeCancel => "cancel",
                    TokenChild | ScopeChild => "child",
                    TokenAttach => "attach",
                    ScopeAdd => "add",
                    ScopeFinish => "finish",
                    _ => unreachable!(),
                };
                let mut values = vec![receiver];
                values.extend_from_slice(args);
                let symbol = format!("{}_{suffix}", handle.prefix());
                let value = self.emit_runtime_call_with_cleanup(&symbol, &values, |_| {});
                match intrinsic {
                    TokenIsCancelled | ScopeIsCancelled => {
                        self.builder.ins().ireduce(types::I8, value.unwrap())
                    }
                    _ => value.unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0)),
                }
            }
            AtomicLoad | AtomicStore | AtomicSwap | AtomicAdd | AtomicSub => {
                let cell = atomic_cell(receiver_ty).expect("validated atomic intrinsic");
                let operation = match intrinsic {
                    AtomicLoad => "load",
                    AtomicStore => "store",
                    AtomicSwap => "swap",
                    AtomicAdd => "add",
                    AtomicSub => "sub",
                    _ => unreachable!(),
                };
                let mut values = vec![receiver];
                values.extend_from_slice(args);
                self.emit_runtime_call_with_cleanup(
                    &format!("willow_atomic_{}_{operation}", cell.suffix()),
                    &values,
                    |_| {},
                )
                .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0))
            }
            CellGet | CellSet | RwCellRead | RwCellWrite => {
                let (kind, element) = blocking_cell(receiver_ty).expect("validated cell intrinsic");
                let operation = match intrinsic {
                    CellGet => "get",
                    CellSet => "set",
                    RwCellRead => "read",
                    RwCellWrite => "write",
                    _ => unreachable!(),
                };
                let mut values = vec![receiver];
                if let Some(&value) = args.first() {
                    values.push(self.coerce_to_i64(value, element));
                }
                match self.emit_runtime_call_with_cleanup(
                    &format!("{}_{operation}", kind.prefix()),
                    &values,
                    |_| {},
                ) {
                    Some(value) => self.coerce_i64_to(value, element),
                    None => self.builder.ins().iconst(types::I8, 0),
                }
            }
            ChannelSend | ChannelRecv | ChannelClose => {
                let element = builtin_types::unary_arg(receiver_ty, B::Channel)
                    .expect("validated channel intrinsic");
                let operation = match intrinsic {
                    ChannelSend => "send",
                    ChannelRecv => "recv",
                    ChannelClose => "close",
                    _ => unreachable!(),
                };
                let symbol = if intrinsic == ChannelClose {
                    "willow_channel_close".to_string()
                } else {
                    format!(
                        "willow_channel_{operation}_{}",
                        channel_runtime_suffix(element)
                    )
                };
                let mut values = vec![receiver];
                if let Some(&value) = args.first() {
                    let value = self.coerce_to_target(value, &arg_types[0], element);
                    if is_gc_managed(element, self.enum_infos) {
                        self.emit_push_root(value);
                    }
                    values.push(value);
                }
                self.emit_runtime_call_with_cleanup(&symbol, &values, |_| {})
                    .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0))
            }
            ArrayLen | FrozenArrayLen => {
                self.emit_value_runtime_call("willow_array_len", &[receiver])
            }
            ArrayPush => {
                let element = array_element_type(receiver_ty);
                let value = self.coerce_to_target(args[0], &arg_types[0], &element);
                if is_gc_managed(&element, self.enum_infos) {
                    self.emit_push_root(value);
                }
                let word = self.coerce_to_i64(value, &element);
                self.emit_void_runtime_call("willow_array_push", &[receiver, word]);
                self.builder.ins().iconst(types::I8, 0)
            }
            ArrayPop => {
                let word = self.emit_value_runtime_call("willow_array_pop", &[receiver]);
                self.coerce_i64_to(word, &array_element_type(receiver_ty))
            }
            ArrayToString => {
                let kind = collection_elem_kind(&array_element_type(receiver_ty))
                    .expect("validated array rendering kind");
                let kind = self.builder.ins().iconst(types::I64, kind);
                self.emit_value_runtime_call("willow_array_to_string", &[receiver, kind])
            }
            ArrayFreeze => self.emit_value_runtime_call("willow_array_copy", &[receiver]),
            MapLen | FrozenMapLen => self.emit_value_runtime_call("willow_map_len", &[receiver]),
            MapToString => self.emit_value_runtime_call("willow_map_to_string", &[receiver]),
            MapFreeze => self.emit_value_runtime_call("willow_map_copy", &[receiver]),
            MapContains | FrozenMapContains | MapGet | FrozenMapGet | MapInsert => {
                let (_, parameters) = lir_collection(receiver_ty).expect("validated map intrinsic");
                let (key_ty, value_ty) = (&parameters[0], &parameters[1]);
                let key = self.coerce_to_i64(args[0], key_ty);
                let key_ref = self.map_is_ref_flag(key_ty);
                match intrinsic {
                    MapContains | FrozenMapContains => {
                        let value = self.emit_value_runtime_call(
                            "willow_map_contains",
                            &[receiver, key, key_ref],
                        );
                        self.builder.ins().ireduce(types::I8, value)
                    }
                    MapGet | FrozenMapGet => {
                        let option = Type::Generic("Option".into(), vec![value_ty.clone()]);
                        let niche = self.builder.ins().iconst(
                            types::I64,
                            i64::from(
                                option_repr(&option, self.enum_infos)
                                    == Some(OptionRepr::NullableGcPointer),
                            ),
                        );
                        self.emit_value_runtime_call(
                            "willow_map_get",
                            &[receiver, key, key_ref, niche],
                        )
                    }
                    MapInsert => {
                        let value = self.coerce_to_target(args[1], &arg_types[1], value_ty);
                        if is_gc_managed(value_ty, self.enum_infos) {
                            self.emit_push_root(value);
                        }
                        let word = self.coerce_to_i64(value, value_ty);
                        let value_ref = self.map_is_ref_flag(value_ty);
                        self.emit_void_runtime_call(
                            "willow_map_insert",
                            &[receiver, key, key_ref, word, value_ref],
                        );
                        self.builder.ins().iconst(types::I8, 0)
                    }
                    _ => unreachable!(),
                }
            }
        }
    }
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Resolved enum combinators consume already-evaluated, typed operands.
    fn emit_flat_enum_method(
        &mut self,
        receiver: cranelift_codegen::ir::Value,
        receiver_ty: &Type,
        method: &str,
        args: &[cranelift_codegen::ir::Value],
        arg_types: &[Type],
        span: Span,
    ) -> cranelift_codegen::ir::Value {
        const OK_TAG: i64 = 0;
        const ERR_TAG: i64 = 1;
        let resolved = builtin_types::resolve(receiver_ty)
            .expect("Option/Result receiver vetted by eligibility");
        let id = resolved.id;
        let payload = |i: usize| resolved.args.get(i).cloned().unwrap_or(Type::Void);
        let (ok_ty, err_ty) = (payload(0), payload(1));
        let recv = receiver;
        let roots_before = self.gc_root_count;
        for (&value, ty) in args.iter().zip(arg_types) {
            if is_gc_managed(ty, self.enum_infos) {
                self.emit_push_root(value);
            }
        }
        // Every branch below either allocates a panic message or evaluates an
        // argument that may allocate, and the receiver is otherwise live only
        // in an SSA register — so it is rooted for the whole method.
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
                let msg = args[0];
                self.emit_option_unwrap(recv, &ok_ty, msg, Some(span))
            }
            (B::Option, "unwrap_or") => {
                let default_val = args[0];
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
                let msg = args[0];
                self.emit_enum_unwrap(recv, &ok_ty, OK_TAG, msg, Some(span))
            }
            (B::Result, "unwrap_or") => {
                let default_val = args[0];
                self.emit_enum_unwrap_or(recv, &ok_ty, OK_TAG, default_val)
            }
            // The callable-taking combinators (willow-0g8j.2.2). The receiver
            // is already rooted above, which is what makes calling an arbitrary
            // function — and allocating the new enum around its result — safe
            // here. Shared helpers enforce tag layout, the pointer niche,
            // and the indirect-call ABI.
            (B::Option, "map") => {
                let (f_val, f_ty) = (args[0], arg_types[0].clone());
                let produced = fn_return_type(&f_ty);
                self.emit_option_map(recv, &ok_ty, &produced, f_val, &f_ty)
            }
            (B::Option, "and_then") => {
                let (f_val, f_ty) = (args[0], arg_types[0].clone());
                self.emit_option_and_then(recv, &ok_ty, f_val, &f_ty)
            }
            (B::Option, "or_else") => {
                let (f_val, f_ty) = (args[0], arg_types[0].clone());
                self.emit_option_or_else(recv, &ok_ty, f_val, &f_ty)
            }
            (B::Result, "map") => {
                let (f_val, f_ty) = (args[0], arg_types[0].clone());
                let produced = fn_return_type(&f_ty);
                self.emit_result_map(recv, &ok_ty, &err_ty, &produced, f_val, &f_ty)
            }
            (B::Result, "map_err") => {
                let (f_val, f_ty) = (args[0], arg_types[0].clone());
                let produced = fn_return_type(&f_ty);
                self.emit_result_map_err(recv, &ok_ty, &err_ty, &produced, f_val, &f_ty)
            }
            (B::Result, "and_then") => {
                let (f_val, f_ty) = (args[0], arg_types[0].clone());
                self.emit_result_and_then(recv, &ok_ty, f_val, &f_ty)
            }
            (B::Result, "or_else") => {
                let (f_val, f_ty) = (args[0], arg_types[0].clone());
                self.emit_result_or_else(recv, &err_ty, f_val, &f_ty)
            }
            _ => unreachable!("unsupported `{method}` on an Option/Result passed eligibility"),
        };
        self.emit_pop_roots_n(self.gc_root_count - roots_before);
        self.gc_root_count = roots_before;
        value
    }
}

impl<'a, 'b> FuncGen<'a, 'b> {
    fn emit_flat_format_scalar(
        &mut self,
        value: cranelift_codegen::ir::Value,
        ty: &Type,
        format: Option<crate::interpolate::F64Format>,
    ) -> cranelift_codegen::ir::Value {
        if let Some(format) = format {
            assert_eq!(*ty, Type::F64, "validated f64 format operand");
            return self.emit_value_runtime_call(format.runtime_symbol(), &[value]);
        }
        match ty {
            Type::String => value,
            Type::I64 => self.emit_value_runtime_call("willow_i64_to_string", &[value]),
            Type::F64 => self.emit_value_runtime_call("willow_f64_to_string", &[value]),
            Type::Bool => self.emit_value_runtime_call("willow_bool_to_string", &[value]),
            _ => unreachable!("validated display format operand"),
        }
    }

    fn emit_flat_builtin_call(
        &mut self,
        callee: &crate::semantic::ids::FunctionId,
        args: &[cranelift_codegen::ir::Value],
    ) -> cranelift_codegen::ir::Value {
        let runtime = builtin_call_runtime_name(callee.unqualified_name())
            .expect("resolved builtin function");
        self.emit_runtime_call_with_cleanup(runtime, args, |_| {})
            .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0))
    }
}

impl<'a, 'b> FuncGen<'a, 'b> {
    fn emit_lir_static_call_values(
        &mut self,
        class: &str,
        method: &str,
        args: &[cranelift_codegen::ir::Value],
        arg_types: &[Type],
        ret_ty: &Type,
        span: Span,
    ) -> cranelift_codegen::ir::Value {
        self.emit_lir_static_call_values_inner(class, method, args, arg_types, ret_ty, span)
    }
    fn emit_lir_static_call_values_inner(
        &mut self,
        class: &str,
        method: &str,
        args: &[cranelift_codegen::ir::Value],
        _arg_types: &[Type],
        ret_ty: &Type,
        span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let resolved_class = self.static_call_class_name(class);
        let class = resolved_class.as_str();
        if class == "Map" && method == "new" {
            let (key, value) = builtin_types::binary_args(ret_ty, B::Map)
                .expect("map constructor must carry checked type arguments");
            return self.emit_map_new(key, value);
        }
        if class == "Channel"
            && let Type::Generic(_, type_args) = ret_ty
            && let Some(element_ty) = type_args.first()
        {
            let is_ref = self.builder.ins().iconst(
                types::I64,
                i64::from(is_gc_managed(element_ty, self.enum_infos)),
            );
            if method == "new" {
                return self.emit_value_runtime_call("willow_channel_new", &[is_ref]);
            }
            if method == "with_capacity" {
                let capacity = args[0];
                return self
                    .emit_value_runtime_call("willow_channel_new_bounded", &[is_ref, capacity]);
            }
        }
        if let Some(handle) = cancellation_handle(ret_ty)
            && method == "new"
            && class == handle.class_name()
        {
            let runtime = format!("{}_new", handle.prefix());
            return self.emit_value_runtime_call(&runtime, &[]);
        }
        if let Some(cell) = atomic_cell(ret_ty)
            && method == "new"
            && class == cell.class_name()
        {
            let initial = args[0];
            let runtime = format!("willow_atomic_{}_new", cell.suffix());
            return self.emit_value_runtime_call(&runtime, &[initial]);
        }
        if let Some((kind, elem)) = blocking_cell(ret_ty)
            && method == "new"
            && class == kind.class_name()
        {
            let elem = elem.clone();
            let initial = args[0];
            let word = self.coerce_to_i64(initial, &elem);
            let is_ref = is_gc_managed(&elem, self.enum_infos);
            let flag = self.builder.ins().iconst(types::I64, is_ref as i64);
            let runtime = format!("{}_new", kind.prefix());
            return self.emit_value_runtime_call(&runtime, &[word, flag]);
        }
        if let Some((prefix, protected)) = scheduler_lock(ret_ty)
            && method == "new"
            && matches!(class, "Mutex" | "RwLock")
        {
            let protected = protected.clone();
            let mut initial = args[0];
            let is_ref = is_gc_managed(&protected, self.enum_infos);
            if is_ref {
                let slot = self.emit_push_root(initial);
                initial = self.stack_load(reference_type(self.module.target_config()), slot);
            }
            let word = self.coerce_to_i64(initial, &protected);
            let flag = self.builder.ins().iconst(types::I64, is_ref as i64);
            let runtime = format!("{prefix}_new");
            let handle = self.emit_value_runtime_call(&runtime, &[word, flag]);
            if is_ref {
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
            }
            return handle;
        }
        if self.enum_infos.contains_key(class) {
            return self.emit_lir_enum_construction_values(class, method, args, ret_ty);
        }
        if let Some(entry) = namespace_builtin_call(
            self.known_modules,
            self.builtin_module_aliases,
            class,
            method,
        ) {
            let (arg_vals, arg_roots) = (args.to_vec(), 0usize);
            let result = self
                .emit_runtime_call_with_cleanup(entry.runtime, &arg_vals, |this| {
                    if arg_roots > 0 {
                        this.emit_pop_roots_n(arg_roots);
                        this.gc_root_count -= arg_roots;
                    }
                })
                .expect("every builtin namespace entry returns a value");
            return if entry.narrow_to_bool {
                self.builder.ins().ireduce(types::I8, result)
            } else {
                result
            };
        }
        if let Some(module_prefix) = self.known_modules.linker_prefix(class).cloned() {
            let mangled = module_item_symbol(&module_prefix, method);
            let has_reference_args = self.func_param_modes.get(&mangled).is_some_and(|modes| {
                modes
                    .iter()
                    .any(|mode| matches!(mode, ParamMode::Reference { .. }))
            });
            let user_callee = format!("{class}::{method}");
            let (arg_vals, arg_roots) = (args.to_vec(), 0usize);
            let fid = *self.func_ids.get(mangled.as_str()).unwrap_or_else(|| {
                panic!("eligible LIR module call `{class}::{method}` has no declared function")
            });
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let pushed = self.emit_callstack_push(&user_callee, span);
            let panic_depth = self.emit_pre_user_call_panic_depth(&mangled);
            let call = self.builder.ins().call(fref, &arg_vals);
            let result = self
                .builder
                .inst_results(call)
                .first()
                .copied()
                .unwrap_or_else(|| {
                    self.builder.ins().iconst(
                        clif_type(reference_type(self.module.target_config()), ret_ty),
                        0,
                    )
                });
            if pushed {
                self.emit_callstack_pop();
            }
            if has_reference_args {
                self.emit_flat_reference_call_end();
            }
            if arg_roots > 0 {
                self.emit_pop_roots_n(arg_roots);
                self.gc_root_count -= arg_roots;
            }
            self.emit_post_willow_call_panic_check(panic_depth);
            return result;
        }
        let mangled = class_method_symbol_name(self.known_modules, class, method);
        let fid = self.func_ids[&mangled];
        let dummy_self = self
            .builder
            .ins()
            .iconst(reference_type(self.module.target_config()), 0);
        let has_reference_args = self.func_param_modes.get(&mangled).is_some_and(|modes| {
            modes
                .iter()
                .any(|mode| matches!(mode, ParamMode::Reference { .. }))
        });
        let (arg_vals, arg_roots) = (args.to_vec(), 0usize);
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
            .unwrap_or_else(|| {
                self.builder.ins().iconst(
                    clif_type(reference_type(self.module.target_config()), ret_ty),
                    0,
                )
            });
        if pushed {
            self.emit_callstack_pop();
        }
        if has_reference_args {
            self.emit_flat_reference_call_end();
        }
        if arg_roots > 0 {
            self.emit_pop_roots_n(arg_roots);
            self.gc_root_count -= arg_roots;
        }
        self.emit_post_willow_call_panic_check(panic_depth);
        result
    }

    fn emit_lir_enum_construction_values(
        &mut self,
        enum_name: &str,
        variant: &str,
        args: &[cranelift_codegen::ir::Value],
        enum_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let tag = self.enum_variant_tag(enum_name, variant);
        if option_repr(enum_ty, self.enum_infos) == Some(OptionRepr::NullableGcPointer) {
            return if tag == 0 {
                args[0]
            } else {
                self.builder
                    .ins()
                    .iconst(reference_type(self.module.target_config()), 0)
            };
        }
        if !self.enum_is_gc_object_type(enum_name) {
            return self.builder.ins().iconst(types::I64, tag);
        }
        let mut payloads = self.resolve_variant_payload_types(enum_name, variant, enum_ty);
        normalize_void_payloads(&mut payloads);
        let kinds = payloads
            .iter()
            .map(|ty| {
                if is_gc_managed(ty, self.enum_infos) {
                    willow_abi::SlotKind::GcRef
                } else {
                    willow_abi::SlotKind::Word
                }
            })
            .collect::<Vec<_>>();
        let layout = willow_abi::EnumVariantLayout::new(tag as u32, &kinds);
        let bytes = reference_type(self.module.target_config()).bytes();
        let ptr = self.emit_gc_alloc(GcLayoutMetadata::new(
            GcObjectKind::Enum,
            i64::from(layout.payload_bytes(bytes)),
            0,
            layout.gc_ref_mask(),
        ));
        let tag_value = self.builder.ins().iconst(types::I64, tag);
        self.builder
            .ins()
            .store(MemFlagsData::new(), tag_value, ptr, 0i32);
        for (index, (&value, ty)) in args.iter().zip(&payloads).enumerate() {
            let word = self.coerce_to_i64(value, ty);
            self.emit_gc_heap_store(
                ptr,
                layout.payload_byte_offset(bytes) as i32 + index as i32 * bytes as i32,
                word,
                ty,
                GcStoreDestination::EnumPayload,
            );
        }
        ptr
    }
}

fn flat_static_call_supported(
    class: &str,
    method: &str,
    args: &[Type],
    result: &Type,
    ctx: &LirTypeCtx<'_>,
) -> bool {
    let class = ctx.resolved_class(class);
    if !ctx.supported_type(result) || args.iter().any(|ty| !ctx.supported_type(ty)) {
        return false;
    }
    if class == "Map" && method == "new" && args.is_empty() {
        return matches!(lir_collection(result), Some((LirCollection::Map, _)));
    }
    if class == "Channel" && builtin_types::unary_arg(result, B::Channel).is_some() {
        return (method == "new" && args.is_empty())
            || (method == "with_capacity" && args == [Type::I64]);
    }
    if let Some(cell) = atomic_cell(result)
        && class == cell.class_name()
        && method == "new"
    {
        return args == [cell.word()];
    }
    if let Some((kind, elem)) = blocking_cell(result)
        && class == kind.class_name()
        && method == "new"
    {
        return args.len() == 1 && ctx.same_repr(elem, &args[0]);
    }
    if let Some((_, elem)) = scheduler_lock(result)
        && matches!(class, "Mutex" | "RwLock")
        && method == "new"
    {
        return args.len() == 1 && ctx.same_repr(elem, &args[0]);
    }
    if let Some(handle) = cancellation_handle(result)
        && class == handle.class_name()
        && method == "new"
    {
        return args.is_empty();
    }
    if let Some(entry) =
        namespace_builtin_call(ctx.known_modules, ctx.builtin_module_aliases, class, method)
    {
        return entry.params == args && ctx.repr_compatible(result, &entry.ret);
    }
    if ctx.is_enum(class) {
        let Some((name, definition)) = ctx.enum_instance(result) else {
            return false;
        };
        let Some(variant) = definition.variant(method) else {
            return false;
        };
        return name == TypeId::from_source_name(class)
            && variant.payloads.len() == args.len()
            && variant
                .payloads
                .iter()
                .zip(args)
                .all(|(slot, arg)| ctx.same_repr(slot, arg));
    }
    let (symbol, skip_self) = if let Some(prefix) = ctx.known_modules.linker_prefix(class) {
        (module_item_symbol(prefix, method), false)
    } else {
        if !ctx.supported_class(class) {
            return false;
        }
        (
            class_method_symbol_name(ctx.known_modules, class, method),
            true,
        )
    };
    if !(ctx.known_fn)(&symbol) {
        return false;
    }
    if ctx
        .func_param_modes
        .get(&symbol)
        .is_some_and(|modes| modes.iter().any(|mode| !matches!(mode, ParamMode::Value)))
    {
        return false;
    }
    let Some(Type::Fn(params, ret)) = ctx.fn_types.get(&symbol) else {
        return false;
    };
    let params = if skip_self {
        let Some((_, params)) = params.split_first() else {
            return false;
        };
        params
    } else {
        params.as_slice()
    };
    params.len() == args.len()
        && params
            .iter()
            .zip(args)
            .all(|(param, arg)| ctx.same_repr(param, arg))
        && ctx.same_repr(ret, result)
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Allocate and initialise the tag before evaluating any source payload.
    /// Nullable Option representations reserve no object: the payload store
    /// below replaces this null placeholder with the actual pointer.
    fn emit_flat_enum_alloc(
        &mut self,
        class: &str,
        variant: &str,
        enum_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let tag = self.enum_variant_tag(class, variant);
        if option_repr(enum_ty, self.enum_infos) == Some(OptionRepr::NullableGcPointer) {
            return self
                .builder
                .ins()
                .iconst(reference_type(self.module.target_config()), 0);
        }
        if !self.enum_is_gc_object_type(class) {
            return self.builder.ins().iconst(types::I64, tag);
        }
        let mut payloads = self.resolve_variant_payload_types(class, variant, enum_ty);
        normalize_void_payloads(&mut payloads);
        let kinds = payloads
            .iter()
            .map(|ty| {
                if is_gc_managed(ty, self.enum_infos) {
                    willow_abi::SlotKind::GcRef
                } else {
                    willow_abi::SlotKind::Word
                }
            })
            .collect::<Vec<_>>();
        let layout = willow_abi::EnumVariantLayout::new(tag as u32, &kinds);
        let bytes = reference_type(self.module.target_config()).bytes();
        let object = self.emit_gc_alloc(GcLayoutMetadata::new(
            GcObjectKind::Enum,
            i64::from(layout.payload_bytes(bytes)),
            0,
            layout.gc_ref_mask(),
        ));
        let tag_value = self.builder.ins().iconst(types::I64, tag);
        self.builder
            .ins()
            .store(MemFlagsData::new(), tag_value, object, 0i32);
        object
    }

    /// A payload store is its own LIR operation so coercion occurs before the
    /// next payload is evaluated. The returned representation is written back
    /// to the enum local (necessary for nullable Option::Some).
    // Keep explicit emission operands aligned with the LIR/runtime ABI.
    #[allow(clippy::too_many_arguments)]
    fn emit_flat_enum_payload_store(
        &mut self,
        object: cranelift_codegen::ir::Value,
        class: &str,
        variant: &str,
        index: usize,
        value: cranelift_codegen::ir::Value,
        source_ty: &Type,
        enum_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let before = self.gc_root_count;
        if is_gc_managed(enum_ty, self.enum_infos) {
            self.emit_push_root(object);
        }
        if is_gc_managed(source_ty, self.enum_infos) {
            self.emit_push_root(value);
        }
        let mut payloads = self.resolve_variant_payload_types(class, variant, enum_ty);
        normalize_void_payloads(&mut payloads);
        let target = &payloads[index];
        let value = self.coerce_to_target(value, source_ty, target);
        let result = if option_repr(enum_ty, self.enum_infos) == Some(OptionRepr::NullableGcPointer)
        {
            assert_eq!(index, 0);
            value
        } else {
            let kinds = payloads
                .iter()
                .map(|ty| {
                    if is_gc_managed(ty, self.enum_infos) {
                        willow_abi::SlotKind::GcRef
                    } else {
                        willow_abi::SlotKind::Word
                    }
                })
                .collect::<Vec<_>>();
            let tag = self.enum_variant_tag(class, variant);
            let layout = willow_abi::EnumVariantLayout::new(tag as u32, &kinds);
            let bytes = reference_type(self.module.target_config()).bytes();
            let word = self.coerce_to_i64(value, target);
            self.emit_gc_heap_store(
                object,
                layout.payload_byte_offset(bytes) as i32 + index as i32 * bytes as i32,
                word,
                target,
                GcStoreDestination::EnumPayload,
            );
            object
        };
        self.emit_pop_roots_n(self.gc_root_count - before);
        self.gc_root_count = before;
        result
    }
}

/// Validate metadata that operand type equality alone cannot establish.
fn flat_rvalue_supported(
    value: &crate::ir::lowered::LirRvalue,
    locals: &[crate::ir::lowered::LirLocal],
    ctx: &LirTypeCtx<'_>,
) -> bool {
    use crate::ir::lowered::LirRvalue as V;
    let ty = |operand: &crate::ir::lowered::LirOperand| operand.ty(locals);
    let field_type = |object: &Type, field: &str| -> Option<Type> {
        if range_i64(object) {
            return matches!(field, "start" | "end").then_some(Type::I64);
        }
        ctx.class_layout_of(object)?
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, ty)| ty.clone())
    };
    if value.operands().iter().any(|operand| matches!(operand, crate::ir::lowered::LirOperand::Reference { place, .. } if !flat_reference_place_supported(place, locals, ctx))) { return false; }
    match value {
        V::ReferenceDebug { argument, .. } => matches!(argument, crate::ir::lowered::LirOperand::Reference { place, .. } if flat_reference_place_supported(place, locals, ctx)),
        V::StartTask { callee, params, output, .. } => ctx.cooperative_leaves.contains(&ctx.fn_types.scope().resolve(callee))
            && (ctx.known_fn)(&callee.to_string())
            && ctx.supported_type(output) && params.iter().all(|ty| ctx.supported_type(ty))
            && ctx.fn_types.get_id(callee).is_some_and(|signature| matches!(signature, Type::Fn(declared, result) if declared.len() == params.len() && declared.iter().zip(params).all(|(expected, actual)| ctx.same_repr(expected, actual)) && builtin_types::unary_arg(result, B::Task).is_some_and(|payload| ctx.same_repr(payload, output))))
            && ctx.func_param_modes.get_id(callee).is_none_or(|modes| modes.iter().all(|mode| matches!(mode, ParamMode::Value))),
        V::AwaitFuture { result, .. } => ctx.supported_type(result),
        V::SelectIdleWait { .. } => true,
        V::PrepareMethod { receiver_ty, method, .. } => flat_method_signature(receiver_ty, method, ctx).is_some(),
        V::MethodCall { receiver_ty, method, args, arg_types, result, .. } => flat_argument_modes_match(args, &flat_method_modes(receiver_ty, method, ctx)) && flat_method_signature(receiver_ty, method, ctx).is_some_and(|(params, ret)| params.len() == arg_types.len() && params.iter().zip(arg_types).all(|(param, arg)| ctx.same_repr(param, arg)) && ctx.same_repr(&ret, result)),
        V::IndirectCall { callee, params, result, .. } => ty(callee).is_some_and(|callee| ctx.supported_type(&callee)) && params.iter().all(|param| ctx.supported_type(param)) && ctx.supported_type(result),
        V::RebindResultError { source, target, .. } => ctx.supported_enum_type(source) && ctx.supported_enum_type(target)
            && builtin_types::binary_args(source, B::Result).zip(builtin_types::binary_args(target, B::Result)).is_some_and(|((_, source), (_, target))| ctx.same_repr(source, target)),
        V::IntoError { source, target, .. } => {
            let Type::Named(class) = source else { return false; };
            ctx.supported_class(class) && ctx.supported_type(target) && ctx.resolve_class_method(class, "into").is_some_and(|symbol|
                ctx.fn_types.get(&symbol).is_some_and(|signature| matches!(signature, Type::Fn(params, result) if params.len() == 1 && ctx.same_repr(result, target))))
        }
        V::Coerce { source, target, .. } => ctx.supported_type(source) && ctx.supported_type(target) && ctx.storable(target, source),
        V::CaptureArrayOwner { array, index } => ty(array).is_some_and(|array| matches!(array, Type::Array(_)) && ctx.supported_type(&array)) && ty(index) == Some(Type::I64),
        V::ArrayAlloc { length, element } => *length <= i64::MAX as usize && ctx.supported_type(&Type::Array(Box::new(element.clone()))),
        V::ArrayStore { array, value, element, .. } => ty(array).is_some_and(|array| ctx.supported_type(&array) && matches!(&array, Type::Array(inner) if ctx.same_repr(inner, element))) && ctx.supported_type(element) && ty(value).is_some_and(|value| ctx.storable(element, &value)),
        V::Index { array, element, .. } => ctx.supported_type(element) && ty(array).is_some_and(|array| ctx.supported_type(&array) && (matches!(&array, Type::Array(_)) || matches!(lir_collection(&array), Some((LirCollection::FrozenArray, _)))) && ctx.same_repr(&array_element_type(&array), element)),
        V::ObjectAlloc { class } => ctx.supported_class(class) && ctx.class_layouts.get(class).is_some() && ctx.class_type_ids.get(class).is_some(),
        V::FieldLoad { object_ty, field, result, .. } => ctx.supported_type(object_ty) && ctx.supported_type(result) && field_type(object_ty, field).is_some_and(|field| ctx.same_repr(&field, result)),
        V::FieldStore { object_ty, field, value, .. } => ctx.class_layout_of(object_ty).is_some() && field_type(object_ty, field).is_some_and(|field| ty(value).is_some_and(|value| ctx.supported_type(&value) && ctx.storable(&field, &value))),
        V::StaticField { class, field, result } => (ctx.static_field)(ctx.resolved_class(&class.to_string()), field).is_some_and(|field| ctx.supported_type(&field) && ctx.same_repr(&field, result)),
        V::StaticStore { class, field, value } => (ctx.static_field)(ctx.resolved_class(&class.to_string()), field).is_some_and(|field| ctx.supported_type(&field) && ty(value).is_some_and(|value| ctx.supported_type(&value) && ctx.storable(&field, &value))),
        V::ConstructorCall { class, object, args, arg_types } => {
            if !ctx.supported_class(class) || !ty(object).is_some_and(|ty| matches!(&ty, Type::Named(name) if name == class)) || args.len() != arg_types.len() { return false; }
            let symbol = class_method_symbol_name(ctx.known_modules, &class.to_string(), "init");
            if !(ctx.known_fn)(&symbol) || !flat_argument_modes_match(args, ctx.func_param_modes.get(&symbol).map(Vec::as_slice).unwrap_or(&[])) { return false; }
            let Some(Type::Fn(params, result)) = ctx.fn_types.get(&symbol) else { return false; };
            let Some((_, params)) = params.split_first() else { return false; };
            **result == Type::Void && params.len() == arg_types.len() && params.iter().zip(arg_types).all(|(param, arg)| ctx.supported_type(param) && ctx.same_repr(param, arg))
        }
        V::StaticCall { class, method, args, arg_types, result } => {
            if args.iter().any(|arg| matches!(arg, crate::ir::lowered::LirOperand::Reference { .. })) { flat_static_reference_call_supported(&class.to_string(), method, args, arg_types, result, ctx) }
            else { flat_static_call_supported(&class.to_string(), method, arg_types, result, ctx) }
        },
        V::EnumAlloc { class, variant, enum_ty } => ctx.supported_enum_type(enum_ty) && ctx.enum_instance(enum_ty).is_some_and(|(name, definition)| name == *class && definition.variant(variant).is_some()),
        V::EnumPayloadStore { class, variant, index, source, enum_ty, .. } => ctx.supported_enum_type(enum_ty) && ctx.supported_type(source) && ctx.enum_instance(enum_ty).is_some_and(|(name, definition)| name == *class && definition.variant(variant).and_then(|variant| variant.payloads.get(*index)).is_some_and(|slot| ctx.storable(slot, source))),
        V::Range { start, end } => ty(start) == Some(Type::I64) && ty(end) == Some(Type::I64),
        V::EnumMethod { receiver_ty, method, arg_types, result, .. } => ctx.supported_enum_type(receiver_ty) && ctx.supported_type(result) && arg_types.iter().all(|ty| ctx.supported_type(ty)) && option_result_method(receiver_ty, method, arg_types).is_some_and(|ret| ctx.same_repr(&ret, result)),
        V::BuiltinCall { callee, params, result, .. } => {
            let void_future = *result == Type::Void || builtin_types::unary_arg(result, B::Future) == Some(&Type::Void);
            if callee.is_free_named("sleep") { return params == &[Type::I64] && void_future; }
            if callee.is_free_named("yield") { return params.is_empty() && void_future; }
            if !params.is_empty() { return false; }
            if callee.is_free_named("gc_collect") || callee.is_free_named("gc_minor_collect") { *result == Type::Void }
            else { callee.is_free_named(callee.unqualified_name()) && gc_stat_builtin_runtime_name(callee.unqualified_name()).is_some() && *result == Type::I64 }
        }
        V::FormatScalar { ty, format, .. } => matches!(ty, Type::I64 | Type::F64 | Type::Bool | Type::String) && (format.is_none() || *ty == Type::F64),
        V::Panic { message } => ty(message) == Some(Type::String),
        V::Binary { op, operand_ty, .. } if matches!(operand_ty, Type::Named(_) | Type::Generic(..)) => matches!(op, BinOp::Eq | BinOp::Ne) && ctx.tag_immediate_enum(operand_ty),
        V::Recover => true,
        _ => true,
    }
}

/// The by-value dispatch ABI, including inherited methods and interface Self.
fn flat_method_signature(
    receiver: &Type,
    method: &str,
    ctx: &LirTypeCtx<'_>,
) -> Option<(Vec<Type>, Type)> {
    if !ctx.supported_type(receiver) {
        return None;
    }
    let name = match receiver {
        Type::Named(name) | Type::Generic(name, _) => name,
        _ => return None,
    };
    let (params, ret) = if (ctx.is_interface)(name) {
        let sig = (ctx.iface_method)(receiver, method)?;
        let ret = if matches!(&sig.ret, Type::Named(name) if *name == TypeId::local("Self")) {
            receiver.clone()
        } else {
            sig.ret
        };
        (sig.params, ret)
    } else {
        if !ctx.supported_class(name) {
            return None;
        }
        let symbol = ctx.resolve_class_method(name, method)?;
        let Type::Fn(params, ret) = ctx.fn_types.get(&symbol)? else {
            return None;
        };
        let (_, params) = params.split_first()?;
        (params.to_vec(), (**ret).clone())
    };
    (ctx.supported_type(&ret) && params.iter().all(|param| ctx.supported_type(param)))
        .then_some((params, ret))
}

fn flat_argument_modes_match(args: &[crate::ir::lowered::LirOperand], modes: &[ParamMode]) -> bool {
    args.iter().enumerate().all(|(index, arg)| {
        matches!(arg, crate::ir::lowered::LirOperand::Reference { .. })
            == matches!(modes.get(index), Some(ParamMode::Reference { .. }))
    })
}
fn flat_method_modes(receiver: &Type, method: &str, ctx: &LirTypeCtx<'_>) -> Vec<ParamMode> {
    let name = match receiver {
        Type::Named(name) | Type::Generic(name, _) => name,
        _ => return vec![],
    };
    if (ctx.is_interface)(name) {
        return (ctx.iface_method)(receiver, method)
            .map(|sig| sig.modes)
            .unwrap_or_default();
    }
    ctx.resolve_class_method(name, method)
        .and_then(|symbol| ctx.func_param_modes.get(&symbol).cloned())
        .unwrap_or_default()
}
fn flat_reference_place_supported(
    place: &crate::ir::lowered::LirPlace,
    locals: &[crate::ir::lowered::LirLocal],
    ctx: &LirTypeCtx<'_>,
) -> bool {
    use crate::ir::lowered::LirPlace;
    let Some(ty) = place.ty(locals) else {
        return false;
    };
    if !ctx.supported_type(&ty) || matches!(ty, Type::Void | Type::Never) {
        return false;
    }
    match place {
        LirPlace::Local(_) | LirPlace::ArrayElement { .. } => true,
        LirPlace::Field {
            object_ty, field, ..
        } => ctx.class_layout_of(object_ty).is_some_and(|layout| {
            layout
                .iter()
                .any(|(name, field_ty)| name == field && ctx.same_repr(field_ty, &ty))
        }),
    }
}
fn flat_static_reference_call_supported(
    class: &str,
    method: &str,
    args: &[crate::ir::lowered::LirOperand],
    types: &[Type],
    result: &Type,
    ctx: &LirTypeCtx<'_>,
) -> bool {
    let class = ctx.resolved_class(class);
    let (symbol, skip_self) = if let Some(prefix) = ctx.known_modules.linker_prefix(class) {
        (module_item_symbol(prefix, method), false)
    } else {
        if !ctx.supported_class(class) {
            return false;
        }
        (
            class_method_symbol_name(ctx.known_modules, class, method),
            true,
        )
    };
    if !(ctx.known_fn)(&symbol)
        || !flat_argument_modes_match(
            args,
            ctx.func_param_modes
                .get(&symbol)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )
    {
        return false;
    }
    let Some(Type::Fn(params, ret)) = ctx.fn_types.get(&symbol) else {
        return false;
    };
    let params = if skip_self {
        let Some((_, tail)) = params.split_first() else {
            return false;
        };
        tail
    } else {
        params.as_slice()
    };
    params.len() == types.len()
        && params
            .iter()
            .zip(types)
            .all(|(param, arg)| ctx.supported_type(param) && ctx.same_repr(param, arg))
        && ctx.same_repr(ret, result)
}

/// User calls can run synchronous safepoints. Builtin scalar/collection
/// operations and task creation remain on the ordinary poll stack.
pub(super) fn task_stack_boundary(value: &crate::ir::lowered::LirRvalue) -> bool {
    use crate::ir::lowered::LirRvalue as R;
    let params = match value {
        R::StaticCall { arg_types, .. }
        | R::ConstructorCall { arg_types, .. }
        | R::MethodCall { arg_types, .. }
        | R::EnumMethod { arg_types, .. }
        | R::IntrinsicCall { arg_types, .. } => arg_types.as_slice(),
        R::BuiltinCall { params, .. } => params.as_slice(),
        _ => &[],
    };
    if params
        .iter()
        .any(|ty| matches!(ty, Type::Fn(..) | Type::Closure(..)))
    {
        return true;
    }
    match value {
        R::DirectCall { .. } | R::IndirectCall { .. } | R::IntoError { .. } => true,
        R::StaticCall { class, .. } | R::ConstructorCall { class, .. } => {
            crate::semantic::builtin_types::resolve(&Type::Named(*class)).is_none()
        }
        R::MethodCall { receiver_ty, .. } | R::EnumMethod { receiver_ty, .. } => {
            crate::semantic::builtin_types::resolve(receiver_ty).is_none()
        }
        _ => false,
    }
}

pub(super) fn task_boundary_symbol(
    function: &LirFunction,
    block: usize,
    instruction: usize,
) -> String {
    format!("{}$task_boundary.{block}.{instruction}", function.name)
}

/// A bounded cleanup can run directly on the cooperative poll stack. Calls
/// into user code and cycles need a resumable native stack, including those
/// nested inside another deferred region.
pub(super) fn cleanup_needs_task_stack(function: &LirFunction) -> bool {
    lir_sync_poll_blocks(function)
        .iter()
        .skip(1)
        .any(|poll| *poll)
        || function.blocks.iter().any(|block| {
            lir_block_successors(block).contains(&0)
                || block.recovery.iter().any(|target| target.0 == 0)
        })
        || function
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .any(|inst| match inst {
                LirInst::Compute { value, .. } => task_stack_boundary(value),
                LirInst::Defer { body, .. } => cleanup_needs_task_stack(&body.function),
                _ => false,
            })
}

pub(super) fn task_cleanup_symbol(poll: u32, flag: i32, recovery: bool) -> String {
    format!("__willow_task_cleanup.{poll}.{flag}.{}", u8::from(recovery))
}

impl FuncGen<'_, '_> {
    pub(super) fn emit_task_cleanup_callback(&mut self, region: &LirFunction, recovery: bool) {
        self.recover_eligible_depth = usize::from(recovery);
        let panic_exit = self.builder.create_block();
        self.panic_return_block = Some(panic_exit);
        self.emit_lir_cleanup_region(region);
        if !self.terminated {
            let ready = self.builder.ins().iconst(types::I32, 1);
            self.builder.ins().return_(&[ready]);
        }
        self.builder.switch_to_block(panic_exit);
        let depth = self.emit_value_runtime_call("willow_root_depth", &[]);
        let baseline = self
            .panic_function_root_depth
            .expect("cleanup root baseline");
        let count = self.builder.ins().isub(depth, baseline);
        self.emit_void_runtime_call("willow_pop_roots", &[count]);
        let panicked = self
            .builder
            .ins()
            .iconst(types::I32, willow_abi::RuntimePollResult::Panicked as i64);
        self.builder.ins().return_(&[panicked]);
    }

    pub(super) fn emit_task_boundary_callback(
        &mut self,
        function: &LirFunction,
        local: LirLocalId,
        value: &crate::ir::lowered::LirRvalue,
        span: Span,
    ) {
        use crate::ir::lowered::{LirOperand, LirRvalue as R};
        self.bind_coop_lir_locals(function);
        self.bind_lir_locals(function);
        let panic_exit = self.builder.create_block();
        self.panic_return_block = Some(panic_exit);
        if let R::MethodCall { method, .. } = value
            && self.flat_method_frame_enabled(method)
        {
            self.lir_call_frames.push((method.clone(), span));
            self.callstack_frame_depth = 1;
        }
        if value
            .operands()
            .iter()
            .any(|arg| matches!(arg, LirOperand::Reference { .. }))
        {
            self.lir_reference_scopes.push(Vec::new());
        }
        let result = self.emit_lir_rvalue(function, value, span);
        if !self.terminated {
            if function.locals[local.0 as usize].is_gc_owner()
                || !matches!(
                    function.locals[local.0 as usize].ty,
                    Type::Void | Type::Never
                )
            {
                self.store_lir_local(function, local, result);
            }
            let ready = self.builder.ins().iconst(types::I32, 1);
            self.builder.ins().return_(&[ready]);
        }
        self.builder.switch_to_block(panic_exit);
        let depth = self.emit_value_runtime_call("willow_root_depth", &[]);
        let baseline = self
            .panic_function_root_depth
            .expect("boundary root baseline");
        let count = self.builder.ins().isub(depth, baseline);
        self.emit_void_runtime_call("willow_pop_roots", &[count]);
        let ready = self
            .builder
            .ins()
            .iconst(types::I32, willow_abi::RuntimePollResult::Panicked as i64);
        self.builder.ins().return_(&[ready]);
    }

    pub(super) fn emit_task_boundary(
        &mut self,
        value: Option<&crate::ir::lowered::LirRvalue>,
        symbol: &str,
        suspends: &mut CoopSuspendPoints,
        frame: cranelift_codegen::ir::Value,
    ) {
        use crate::ir::lowered::{LirOperand, LirRvalue as R};
        let resume = self.builder.create_block();
        self.builder.ins().jump(resume, &[]);
        self.builder.switch_to_block(resume);
        let callback = self.func_ids[symbol];
        let callback = self
            .module
            .declare_func_in_func(callback, self.builder.func);
        let address = self
            .builder
            .ins()
            .func_addr(reference_type(self.module.target_config()), callback);
        // Explicitly handle suspension before panic propagation or observing
        // the result slot. Runtime-call wrappers cannot express this split.
        let enter = self.func_ids["willow_task_stack_enter"];
        let enter = self.module.declare_func_in_func(enter, self.builder.func);
        let call = self.builder.ins().call(enter, &[address, frame]);
        let status = self.builder.inst_results(call)[0];
        self.emit_void_runtime_call("willow_task_stack_leave", &[]);
        let ready = self.builder.create_block();
        let pending = self.builder.create_block();
        let ready_status = self.builder.ins().icmp_imm_s(IntCC::Equal, status, 1);
        let panic_status = self.builder.ins().icmp_imm_s(
            IntCC::Equal,
            status,
            willow_abi::RuntimePollResult::Panicked as i64,
        );
        let done = self.builder.ins().bor(ready_status, panic_status);
        self.builder.ins().brif(done, ready, &[], pending, &[]);
        self.builder.switch_to_block(pending);
        let state = self
            .builder
            .ins()
            .iconst(types::I64, (suspends.len() + 1) as i64);
        self.builder
            .ins()
            .store(MemFlagsData::new(), state, frame, 0);
        self.emit_coop_unwind_poll_roots();
        let preempted = self
            .builder
            .ins()
            .iconst(types::I32, super::COOP_POLL_PREEMPTED);
        self.builder.ins().return_(&[preempted]);
        self.record_coop_suspend(suspends, resume);
        self.builder.switch_to_block(ready);
        if let Some(R::MethodCall { method, .. }) = value
            && self.flat_method_frame_enabled(method)
        {
            self.lir_call_frames
                .pop()
                .expect("prepared boundary method");
            self.emit_callstack_pop();
        }
        if value.is_some_and(|value| {
            value
                .operands()
                .iter()
                .any(|arg| matches!(arg, LirOperand::Reference { .. }))
        }) {
            self.emit_flat_reference_call_end();
        }
        // The callback reports an unwind explicitly. Comparing TLS depth
        // here would lose a panic raised before an earlier suspension.
        let panicked = self.builder.create_block();
        let normal = self.builder.create_block();
        self.builder
            .ins()
            .brif(panic_status, panicked, &[], normal, &[]);
        self.builder.switch_to_block(panicked);
        self.emit_sync_panic_unwind();
        self.builder.switch_to_block(normal);
        self.terminated = false;
        self.panic_depth_snapshot = None;
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
        class_layouts: TypeMap<Vec<(String, Type)>>,
        static_fields: HashMap<(String, String), Type>,
        class_base: TypeMap<TypeId>,
        class_type_ids: TypeMap<i64>,
        interfaces: HashSet<String>,
        iface_type_params: HashMap<String, Vec<String>>,
        /// Direct super-interfaces, in declaration order. The real backend
        /// keeps this on `InterfaceInfo::extends`; tests retain it separately
        /// so their widening offsets use the same layout algorithm.
        iface_supers: HashMap<String, Vec<String>>,
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
        known_modules: ModuleSymbols,
        /// The module access names the unit under test imports (willow-vtlr).
        /// `TestTables::build` leaves it empty because the plain test programs
        /// declare no modules; a test that populates `known_modules` says here
        /// which of them the importing file can see.
        visible_modules: HashSet<String>,
        /// Builtin namespace aliases declared by the test program's own
        /// `import`s, standing in for the backend's per-file map (willow-nswv).
        builtin_module_aliases: HashMap<String, String>,
        /// Stands in for the per-function `LirTypeCtx::return_type`. Every
        /// entry point that takes a `LirFunction` rebinds it from that
        /// function, so this default is only what a direct `supported_expr`
        /// call sees.
        ret: Type,
        /// Lifted lambda symbols by the span of the lambda expression, standing
        /// in for the backend's `lambda_names` (willow-0g8j.2.2). Registered
        /// from the lowered IR's own lambda list, so a test's symbol table and
        /// its lambda bodies cannot describe different signatures.
        lambdas: HashMap<ExprId, String>,
        /// Cooperative leaf async functions, standing in for the backend's
        /// `cooperative_leaves` (willow-0g8j.2.11). Registered from every
        /// `async fn` the test program declares, which is what
        /// `compile_program` compiles as a cooperative leaf.
        cooperative_leaves: HashSet<FunctionId>,
    }

    impl crate::backend::cranelift::vtable_layout::IfaceShapes for TestTables {
        fn canonical(&self, iface: &TypeId) -> TypeId {
            *iface
        }

        fn supers(&self, iface: &TypeId) -> Vec<TypeId> {
            self.iface_supers
                .get(&iface.to_string())
                .map(|supers| {
                    supers
                        .iter()
                        .map(|name| TypeId::from_source_name(name))
                        .collect()
                })
                .unwrap_or_default()
        }

        fn methods(&self, iface: &TypeId) -> Vec<String> {
            self.iface_methods
                .get(&iface.to_string())
                .map(|methods| methods.iter().map(|(name, ..)| name.clone()).collect())
                .unwrap_or_default()
        }
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
                cooperative_leaves: HashSet::new(),
                class_layouts: TypeMap::new(),
                static_fields: HashMap::new(),
                class_base: TypeMap::new(),
                class_type_ids: TypeMap::new(),
                interfaces: HashSet::new(),
                iface_type_params: HashMap::new(),
                iface_supers: HashMap::new(),
                iface_methods: HashMap::new(),
                vtables: HashSet::new(),
                enums: HashMap::new(),
                fn_types: FunctionMap::default(),
                param_modes: FunctionMap::default(),
                known_modules: ModuleSymbols::default(),
                visible_modules: HashSet::new(),
                builtin_module_aliases:
                    crate::backend::cranelift::std_collection::builtin_module_aliases(program),
                ret: Type::Void,
                lambdas: HashMap::new(),
            };
            // Every lifted lambda is a declared, linkable symbol with the
            // signature its lowered body carries — what `declare_lambda` does
            // in the backend.
            for l in lambdas {
                let name = l.function.name.to_string();
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
                t.lambdas.insert(l.id, name);
            }
            let sig = |params: &[crate::parser::ast::Param],
                       ret: &crate::parser::ast::Type,
                       with_self: bool| {
                let mut ps: Vec<Type> = Vec::new();
                if with_self {
                    ps.push(Type::I64);
                }
                ps.extend(params.iter().map(|p| Type::from(&p.ty)));
                Type::Fn(ps, Box::new(ret.into()))
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
                        if f.is_async {
                            t.cooperative_leaves
                                .insert(FunctionId::free_from_source_name(&f.name));
                        }
                    }
                    Item::Interface(i) => {
                        t.interfaces.insert(i.name.clone());
                        t.iface_type_params
                            .insert(i.name.clone(), i.type_params.clone());
                        t.iface_supers.insert(i.name.clone(), i.extends.clone());
                        t.iface_methods.insert(
                            i.name.clone(),
                            i.methods
                                .iter()
                                .map(|m| {
                                    (
                                        m.name.clone(),
                                        m.params.iter().map(|p| Type::from(&p.ty)).collect(),
                                        m.params.iter().map(|p| p.mode.clone()).collect(),
                                        m.return_type.clone().into(),
                                    )
                                })
                                .collect(),
                        );
                    }
                    Item::Enum(e) => {
                        t.enums.insert(
                            e.name.clone(),
                            LirEnumDef {
                                identity: e.name.clone().into(),
                                type_params: e.type_params.iter().map(TypeId::local).collect(),
                                variants: e
                                    .variants
                                    .iter()
                                    .map(|v| LirEnumVariant {
                                        name: v.name.clone(),
                                        payloads: v.payload.iter().map(Into::into).collect(),
                                    })
                                    .collect(),
                            },
                        );
                    }
                    Item::Class(c) => {
                        for field in c.fields.iter().filter(|f| f.is_static) {
                            t.static_fields.insert(
                                (c.name.clone(), field.name.clone()),
                                field.ty.clone().into(),
                            );
                        }
                        t.class_layouts.insert(
                            c.name.clone(),
                            c.fields
                                .iter()
                                .filter(|f| !f.is_static)
                                .map(|f| (f.name.clone(), f.ty.clone().into()))
                                .collect(),
                        );
                        if let Some(base) = &c.base_class {
                            t.class_base
                                .insert(c.name.clone(), base.name().to_string().into());
                        }
                        for iface in &c.implements {
                            if let crate::parser::ast::Type::Named(n)
                            | crate::parser::ast::Type::Generic(n, _) = iface
                            {
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
                            t.fn_types.insert(
                                &mangled,
                                sig(&ctor.params, &crate::parser::ast::Type::Void, true),
                            );
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
            // Prepend each class's inherited fields, root-down, exactly as
            // `Codegen::finalize_class_layouts` does. A separate pass because a
            // subclass may be DECLARED before its base (willow-59gx); without
            // it these tables would model an inheriting class as owning only
            // its own fields, which is not what the walker is handed.
            let own = t.class_layouts.clone();
            for (class_name, _) in own.iter() {
                let mut chain = vec![*class_name];
                let mut seen = HashSet::from([*class_name]);
                while let Some(base) = t.class_base.get(chain.last().expect("non-empty")) {
                    if !seen.insert(*base) {
                        break;
                    }
                    chain.push(*base);
                }
                let mut fields: Vec<(String, Type)> = Vec::new();
                for ancestor in chain.iter().rev() {
                    let Some(ancestor_own) = own.get(ancestor) else {
                        continue;
                    };
                    for (name, ty) in ancestor_own {
                        if !fields.iter().any(|(n, _)| n == name) {
                            fields.push((name.clone(), ty.clone()));
                        }
                    }
                }
                t.class_layouts.insert(*class_name, fields);
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
                is_interface: &|n| self.interfaces.contains(&n.to_string()),
                iface_identity: &|n| self.interfaces.contains(&n.to_string()).then_some(*n),
                can_box: &|class, iface| {
                    self.vtables
                        .contains(&(class.to_string(), iface.to_string()))
                },
                enum_def: &|n| self.enums.get(&n.to_string()).cloned(),
                lambda_symbol: &|id| self.lambdas.get(&id).map(FunctionId::free),
                cooperative_leaves: &self.cooperative_leaves,
                iface_method: &|iface_ty, method| {
                    let (iface, args): (&TypeId, &[Type]) = match iface_ty {
                        Type::Named(name) => (name, &[]),
                        Type::Generic(name, args) => (name, args),
                        _ => return None,
                    };
                    let methods = self.iface_methods.get(&iface.to_string())?;
                    let type_params = self.iface_type_params.get(&iface.to_string())?;
                    if type_params.len() != args.len() {
                        return None;
                    }
                    let mut substitutions: HashMap<TypeId, Type> = type_params
                        .iter()
                        .map(TypeId::local)
                        .zip(args.iter().cloned())
                        .collect();
                    substitutions.insert(TypeId::local("Self"), iface_ty.clone());
                    let slot = methods.iter().position(|(n, _, _, _)| n == method)?;
                    let (_, params, modes, ret) = &methods[slot];
                    Some(IfaceMethodSig {
                        params: params
                            .iter()
                            .map(|ty| crate::semantic::symbols::substitute_type(ty, &substitutions))
                            .collect(),
                        modes: modes.clone(),
                        ret: crate::semantic::symbols::substitute_type(ret, &substitutions),
                    })
                },
                static_field: &|class, field| {
                    let mut search = Some(class.to_string());
                    let mut seen = HashSet::new();
                    while let Some(name) = search {
                        if !seen.insert(name.clone()) {
                            break;
                        }
                        if let Some(ty) = self.static_fields.get(&(name.clone(), field.to_string()))
                        {
                            return Some(ty.clone());
                        }
                        search = self.class_base.get(&name).map(ToString::to_string);
                    }
                    None
                },
                iface_widen_offset: &|target, source| {
                    crate::backend::cranelift::vtable_layout::super_offset(self, source, target)
                },
                fn_types: &self.fn_types,
                func_param_modes: &self.param_modes,
                known_modules: &self.known_modules,
                visible_modules: &self.visible_modules,
                builtin_module_aliases: &self.builtin_module_aliases,
                return_type: &self.ret,
                self_class: None,
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
        let f = p
            .functions
            .iter()
            .find(|f| f.name.to_string() == name)
            .unwrap();
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
        match p.functions.iter().find(|f| f.name.to_string() == name) {
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

    #[test]
    fn flat_graph_rejects_normal_and_recovery_edges_to_entry() {
        let (program, tables) = checked_lowering("fn main() {}", &["main"]);
        let original = &program.functions[0];
        for recovery in [false, true] {
            let mut function = original.clone();
            if recovery {
                function.blocks[0].recovery = vec![BlockId(0)];
            } else {
                function.blocks[0].terminator = Terminator::Jump(BlockId(0));
            }
            let reason = tables.with_ctx(|ctx| lir_rejection_reason(&function, ctx));
            assert_eq!(
                reason.as_deref(),
                Some("the LIR entry block must not have incoming edges")
            );
        }
    }

    #[test]
    fn flat_array_references_reject_forged_capture_metadata() {
        use crate::ir::lowered::{LirOperand as O, LirPlace, LirRvalue as V};
        let (program, tables) = checked_lowering(
            "import std::collections::Array; fn read(x: & i64) -> i64 { return x; } fn f(a: Array<i64>, i: i64) -> i64 { return read(&a[i]); }",
            &["read", "f"],
        );
        let original = program
            .functions
            .iter()
            .find(|f| f.name.to_string() == "f")
            .unwrap();
        let original_reason = tables.with_ctx(|ctx| lir_rejection_reason(original, ctx));
        assert!(original_reason.is_none(), "{original_reason:?}");
        for forge_element in [false, true] {
            let mut function = original.clone();
            let mut changed = false;
            for block in &mut function.blocks {
                for inst in &mut block.instrs {
                    if let LirInst::Compute {
                        value: V::DirectCall { args, .. },
                        ..
                    } = inst
                    {
                        for argument in args {
                            if let O::Reference {
                                place: LirPlace::ArrayElement { index, element, .. },
                                ..
                            } = argument
                            {
                                if forge_element {
                                    *element = Type::String;
                                } else {
                                    *index = original
                                        .locals
                                        .iter()
                                        .find(|local| local.parameter && local.ty == Type::I64)
                                        .unwrap()
                                        .id;
                                }
                                changed = true;
                            }
                        }
                    }
                }
            }
            assert!(changed);
            let reason = tables
                .with_ctx(|ctx| lir_rejection_reason(&function, ctx))
                .expect("forged reference rejected");
            assert!(
                reason.contains("captured buffer or checked index"),
                "{reason}"
            );
        }
    }

    #[test]
    fn flat_rvalues_reject_unknown_layouts_and_invalid_coercions() {
        use crate::ir::lowered::{LirOperand as O, LirRvalue as V};
        let (_, tables) = checked_lowering(
            "class Point { pub x: i64; pub fn get(self) -> i64 { return self.x; } } fn main() {}",
            &["main"],
        );
        tables.with_ctx(|ctx| {
            let point = Type::Named(TypeId::local("Point"));
            assert!(flat_method_signature(&point, "get", ctx).is_some());
            assert!(flat_method_signature(&point, "absent", ctx).is_none());
            assert!(!flat_rvalue_supported(
                &V::MethodCall {
                    receiver: O::Int(0),
                    receiver_ty: point.clone(),
                    method: "get".into(),
                    args: vec![],
                    arg_types: vec![],
                    result: Type::String
                },
                &[],
                ctx
            ));
            assert!(flat_rvalue_supported(
                &V::ObjectAlloc {
                    class: TypeId::local("Point")
                },
                &[],
                ctx
            ));
            assert!(!flat_rvalue_supported(
                &V::ObjectAlloc {
                    class: TypeId::local("Missing")
                },
                &[],
                ctx
            ));
            assert!(!flat_rvalue_supported(
                &V::Coerce {
                    value: O::Int(1),
                    source: Type::I64,
                    target: Type::String
                },
                &[],
                ctx
            ));
            assert!(!flat_rvalue_supported(
                &V::FieldLoad {
                    object: O::Int(0),
                    object_ty: point.clone(),
                    field: "missing".into(),
                    result: Type::I64
                },
                &[],
                ctx
            ));
            assert!(!flat_rvalue_supported(
                &V::FieldLoad {
                    object: O::Int(0),
                    object_ty: point,
                    field: "x".into(),
                    result: Type::String
                },
                &[],
                ctx
            ));
            assert!(!flat_rvalue_supported(
                &V::ArrayAlloc {
                    length: 1,
                    element: Type::Void
                },
                &[],
                ctx
            ));
            assert!(!flat_rvalue_supported(
                &V::StaticField {
                    class: TypeId::local("Point"),
                    field: "missing".into(),
                    result: Type::I64
                },
                &[],
                ctx
            ));
        });
    }

    #[test]
    fn flat_rvalues_reject_forged_runtime_and_enum_metadata() {
        use crate::ir::lowered::{LirOperand as O, LirRvalue as V};
        let (_, tables) = checked_lowering("fn main() {}", &["main"]);
        tables.with_ctx(|ctx| {
            assert!(flat_rvalue_supported(
                &V::BuiltinCall {
                    callee: FunctionId::free("gc_collect"),
                    args: vec![],
                    params: vec![],
                    result: Type::Void
                },
                &[],
                ctx
            ));
            assert!(!flat_rvalue_supported(
                &V::BuiltinCall {
                    callee: FunctionId::free("gc_collect"),
                    args: vec![],
                    params: vec![],
                    result: Type::I64
                },
                &[],
                ctx
            ));
            assert!(!flat_rvalue_supported(
                &V::BuiltinCall {
                    callee: FunctionId::free("unknown"),
                    args: vec![],
                    params: vec![],
                    result: Type::Void
                },
                &[],
                ctx
            ));
            assert!(!flat_rvalue_supported(
                &V::EnumAlloc {
                    class: TypeId::local("Missing"),
                    variant: "Some".into(),
                    enum_ty: Type::Named(TypeId::local("Missing"))
                },
                &[],
                ctx
            ));
            assert!(!flat_rvalue_supported(
                &V::EnumMethod {
                    receiver: O::Int(0),
                    receiver_ty: Type::I64,
                    method: "unwrap".into(),
                    args: vec![],
                    arg_types: vec![],
                    result: Type::I64
                },
                &[],
                ctx
            ));
        });
    }

    /// [`eligible`] for constructs that need the checker's types to lower.
    fn eligible_checked(src: &str, name: &str, fns: &[&str]) -> bool {
        let (p, tables) = checked_lowering(src, fns);
        match p.functions.iter().find(|f| f.name.to_string() == name) {
            Some(f) => tables.with_ctx(|ctx| lir_supported_function(f, ctx)),
            None => false,
        }
    }

    /// The validation diagnostic for `name`, through the same checked pipeline as
    /// [`eligible_checked`]. Panics if the function did not survive lowering,
    /// which would mean the test is exercising a HIR gap and not this code.
    fn reason_of(src: &str, name: &str, fns: &[&str]) -> Option<String> {
        let (p, tables) = checked_lowering(src, fns);
        let f = p
            .functions
            .iter()
            .find(|f| f.name.to_string() == name)
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
    #[test]
    fn bounded_scalar_return_defers_poll_but_preserves_recursive_path() {
        let (f, _) = lir_fn_and_tables(
            "fn fib(n: i64) -> i64 { if n < 2 { return n; } return fib(n-1) + fib(n-2); }",
            "fib",
            &["fib"],
        );
        let mut poll = lir_sync_poll_blocks(&f);
        assert!(lir_defer_entry_poll(&f, &mut poll));
        assert!(!poll[0]);
        let mut pending = vec![0];
        let mut visited = HashSet::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) || poll[id] {
                continue;
            }
            let block = &f.blocks[id];
            assert!(
                !block.instrs.iter().any(|i| matches!(
                    i,
                    LirInst::Compute {
                        value: crate::ir::lowered::LirRvalue::DirectCall { .. },
                        ..
                    }
                )),
                "a recursive call must be preceded by a poll"
            );
            pending.extend(lir_block_successors(block));
        }
    }

    #[test]
    fn entry_poll_stays_before_faults_cleanup_and_cycles() {
        for source in [
            "fn f(n: i64) -> i64 { if 10 / n < 2 { return n; } return f(n-1) + 1; }",
            "fn f(n: i64) -> i64 { defer { println(n); } if n < 2 { return n; } return f(n-1) + 1; }",
            "fn f(n: i64) -> i64 { if n < 2 { return n; } while true {} return n; }",
        ] {
            let (f, _) = lir_fn_and_tables(source, "f", &["f"]);
            let mut poll = lir_sync_poll_blocks(&f);
            let original = poll.clone();
            assert!(!lir_defer_entry_poll(&f, &mut poll), "{source}");
            assert_eq!(poll, original);
        }
    }

    fn lir_fn_and_tables(src: &str, name: &str, fns: &[&str]) -> (LirFunction, TestTables) {
        let (p, tables) = checked_lowering(src, fns);
        let f = p
            .functions
            .iter()
            .find(|f| f.name.to_string() == name)
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
    fn may_allocate_preserves_twenty_wrapper_perspectives() {
        fn node(kind: HirExprKind) -> HirExpr {
            HirExpr {
                kind,
                ty: Type::I64,
                span: Span::new(0, 0, 1, 1),
            }
        }
        fn scalar() -> Box<HirExpr> {
            Box::new(node(HirExprKind::Int(0)))
        }
        // Ten wrapper positions, each with an allocation-free scalar and an
        // allocating call. All wrappers must propagate the same conservative bit.
        for position in 0..10 {
            for allocating in [false, true] {
                let leaf = Box::new(node(if allocating {
                    HirExprKind::Call {
                        callee: "value".into(),
                        args: vec![],
                    }
                } else {
                    HirExprKind::Int(1)
                }));
                let expr = node(match position {
                    0 => HirExprKind::Unary {
                        op: UnaryOp::Neg,
                        operand: leaf,
                    },
                    1 => HirExprKind::ReferenceArg { place: leaf },
                    2 => HirExprKind::FieldAccess {
                        object: leaf,
                        field: "field".into(),
                    },
                    3 => HirExprKind::Index {
                        array: leaf,
                        index: scalar(),
                    },
                    4 => HirExprKind::Index {
                        array: scalar(),
                        index: leaf,
                    },
                    5 => HirExprKind::Binary {
                        op: BinOp::Add,
                        lhs: leaf,
                        rhs: scalar(),
                    },
                    6 => HirExprKind::Binary {
                        op: BinOp::Add,
                        lhs: scalar(),
                        rhs: leaf,
                    },
                    7 => HirExprKind::Ternary {
                        condition: leaf,
                        then_expr: scalar(),
                        else_expr: scalar(),
                    },
                    8 => HirExprKind::Ternary {
                        condition: scalar(),
                        then_expr: leaf,
                        else_expr: scalar(),
                    },
                    _ => HirExprKind::Ternary {
                        condition: scalar(),
                        then_expr: scalar(),
                        else_expr: leaf,
                    },
                });
                assert_eq!(may_allocate(&expr), allocating, "position={position}");
            }
        }
        for kind in [
            HirExprKind::Float(1.0),
            HirExprKind::Bool(true),
            HirExprKind::Var("x".into()),
            HirExprKind::FnRef("f".into()),
        ] {
            assert!(!may_allocate(&node(kind)));
        }
        // Even an allocation-free child does not make an unlisted wrapper safe.
        assert!(may_allocate(&node(HirExprKind::TryPropagate {
            inner: scalar()
        })));
        assert!(may_allocate(&node(HirExprKind::Str("literal".into()))));
        for closure in [false, true] {
            let mut lambda = node(HirExprKind::Lambda {
                id: crate::parser::ast::ExprId::fresh(),
                params: vec![],
                captures: vec![],
                body: vec![HirStmt::Return {
                    value: Some(node(HirExprKind::Call {
                        callee: "allocates".into(),
                        args: vec![],
                    })),
                    span: Span::new(0, 0, 1, 1),
                }],
            });
            lambda.ty = if closure {
                Type::Closure(vec![], Box::new(Type::I64))
            } else {
                Type::Fn(vec![], Box::new(Type::I64))
            };
            assert_eq!(may_allocate(&lambda), closure);
        }
    }

    fn eligibility_wrapper_tables() -> TestTables {
        let tokens = Lexer::new(
            "class Box { pub value: i64; } fn pair(a: i64, b: i64) -> i64 { return a + b; }",
        )
        .tokenize()
        .unwrap();
        let (program, errors) = Parser::new(tokens).parse();
        assert!(errors.is_empty());
        TestTables::build(&program, &["pair"], &[])
    }

    fn eligibility_node(kind: HirExprKind, ty: Type) -> HirExpr {
        HirExpr {
            kind,
            ty,
            span: Span::new(0, 0, 1, 1),
        }
    }

    fn eligibility_wrapper(leaf: HirExpr, position: usize) -> HirExpr {
        let scalar = || eligibility_node(HirExprKind::Int(1), Type::I64);
        let array = |elements| {
            eligibility_node(
                HirExprKind::Array { elements },
                Type::Array(Box::new(Type::I64)),
            )
        };
        let range = |start, end| {
            eligibility_node(
                HirExprKind::Range {
                    start: Box::new(start),
                    end: Box::new(end),
                },
                Type::Generic("Range".into(), vec![Type::I64]),
            )
        };
        let object = |kind| eligibility_node(kind, Type::Named("Box".into()));
        match position {
            0 | 1 => eligibility_node(
                HirExprKind::Call {
                    callee: "pair".into(),
                    args: if position == 0 {
                        vec![leaf, scalar()]
                    } else {
                        vec![scalar(), leaf]
                    },
                },
                Type::I64,
            ),
            2 | 3 => eligibility_node(
                HirExprKind::Index {
                    array: Box::new(array(if position == 2 {
                        vec![leaf, scalar()]
                    } else {
                        vec![scalar(), leaf]
                    })),
                    index: Box::new(scalar()),
                },
                Type::I64,
            ),
            4 => eligibility_node(
                HirExprKind::Index {
                    array: Box::new(array(vec![scalar()])),
                    index: Box::new(leaf),
                },
                Type::I64,
            ),
            5 | 6 => eligibility_node(
                HirExprKind::FieldAccess {
                    object: Box::new(if position == 5 {
                        range(leaf, scalar())
                    } else {
                        range(scalar(), leaf)
                    }),
                    field: "start".into(),
                },
                Type::I64,
            ),
            7 | 8 => eligibility_node(
                HirExprKind::FieldAccess {
                    object: Box::new(object(if position == 7 {
                        HirExprKind::New {
                            class: "Box".into(),
                            args: vec![leaf],
                        }
                    } else {
                        HirExprKind::ObjectLiteral {
                            class: "Box".into(),
                            fields: vec![("value".into(), leaf)],
                        }
                    })),
                    field: "value".into(),
                },
                Type::I64,
            ),
            _ => eligibility_node(
                HirExprKind::Print {
                    value: Box::new(leaf),
                    newline: true,
                },
                Type::Void,
            ),
        }
    }

    #[test]
    fn reference_names_match_ast_and_hir_for_twenty_shapes() {
        use crate::parser::ast::{Expr, ExprId};
        let span = Span::dummy();
        // Four roots times five suffix forms, including both placeholders.
        for root in 0..4 {
            for suffix in 0..5 {
                let (ast, hir, expected) = match root {
                    0 => (
                        Expr::Var("root".into(), span, ExprId::fresh()),
                        HirExprKind::Var("root".into()),
                        "root",
                    ),
                    1 => (
                        Expr::Var("名".into(), span, ExprId::fresh()),
                        HirExprKind::Var("名".into()),
                        "名",
                    ),
                    2 => (
                        Expr::Integer(7, span, ExprId::fresh()),
                        HirExprKind::Int(7),
                        "<expression>",
                    ),
                    _ => (
                        Expr::Bool(true, span, ExprId::fresh()),
                        HirExprKind::Bool(true),
                        "<expression>",
                    ),
                };
                let mut ast =
                    Expr::FieldAccess(Box::new(ast), "field".into(), span, ExprId::fresh());
                let mut hir = eligibility_node(
                    HirExprKind::FieldAccess {
                        object: Box::new(eligibility_node(hir, Type::I64)),
                        field: "field".into(),
                    },
                    Type::I64,
                );
                let (ast_index, hir_index, index_name) = match suffix {
                    0 => (
                        Expr::Integer(0, span, ExprId::fresh()),
                        HirExprKind::Int(0),
                        "0",
                    ),
                    1 => (
                        Expr::Integer(-7, span, ExprId::fresh()),
                        HirExprKind::Int(-7),
                        "-7",
                    ),
                    2 => (
                        Expr::Var("i".into(), span, ExprId::fresh()),
                        HirExprKind::Var("i".into()),
                        "i",
                    ),
                    3 => (
                        Expr::Bool(true, span, ExprId::fresh()),
                        HirExprKind::Bool(true),
                        "<expr>",
                    ),
                    _ => (
                        Expr::Var("添字".into(), span, ExprId::fresh()),
                        HirExprKind::Var("添字".into()),
                        "添字",
                    ),
                };
                ast = Expr::Index(Box::new(ast), Box::new(ast_index), span, ExprId::fresh());
                hir = eligibility_node(
                    HirExprKind::Index {
                        array: Box::new(hir),
                        index: Box::new(eligibility_node(hir_index, Type::I64)),
                    },
                    Type::I64,
                );
                let expected = format!("{expected}.field[{index_name}]");
                assert_eq!(super::super::reference_place_name(&ast), expected);
                assert_eq!(lir_reference_place_name(&hir), expected);
            }
        }
    }

    #[test]
    fn reference_names_use_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                use crate::parser::ast::{Expr, ExprId};
                let span = Span::dummy();
                let mut ast = Expr::Var("root".into(), span, ExprId::fresh());
                let mut hir = eligibility_node(HirExprKind::Var("root".into()), Type::I64);
                let mut expected = String::from("root");
                for depth in 0..50_000 {
                    if depth % 2 == 0 {
                        ast = Expr::FieldAccess(Box::new(ast), "x".into(), span, ExprId::fresh());
                        hir = eligibility_node(
                            HirExprKind::FieldAccess {
                                object: Box::new(hir),
                                field: "x".into(),
                            },
                            Type::I64,
                        );
                        expected.push_str(".x");
                    } else {
                        ast = Expr::Index(
                            Box::new(ast),
                            Box::new(Expr::Integer(0, span, ExprId::fresh())),
                            span,
                            ExprId::fresh(),
                        );
                        hir = eligibility_node(
                            HirExprKind::Index {
                                array: Box::new(hir),
                                index: Box::new(eligibility_node(HirExprKind::Int(0), Type::I64)),
                            },
                            Type::I64,
                        );
                        expected.push_str("[0]");
                    }
                }
                let ast_name = super::super::reference_place_name(&ast);
                let hir_name = lir_reference_place_name(&hir);
                drop(ast);
                drop(hir);
                assert_eq!(ast_name, expected);
                assert_eq!(hir_name, expected);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn divergence_nested_if_and_match_use_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let span = Span::dummy();
                for diverges in [false, true] {
                    let returns = || HirStmt::Return { value: None, span };
                    let mut body = if diverges { vec![returns()] } else { vec![] };
                    for depth in 0..50_000 {
                        body = if depth % 2 == 0 {
                            vec![HirStmt::If {
                                cond: eligibility_node(HirExprKind::Bool(true), Type::Bool),
                                then_branch: body,
                                else_branch: Some(vec![returns()]),
                                span,
                            }]
                        } else {
                            vec![HirStmt::Expr(eligibility_node(
                                HirExprKind::Match {
                                    scrutinee: Box::new(eligibility_node(
                                        HirExprKind::Int(0),
                                        Type::I64,
                                    )),
                                    arms: vec![HirMatchArm {
                                        pattern: HirPattern::Wildcard,
                                        body,
                                        ty: Type::Void,
                                        span,
                                    }],
                                },
                                Type::Void,
                            ))]
                        };
                    }
                    let actual = body_diverges(&body);

                    drop(body);
                    assert_eq!(actual, diverges);
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn eligibility_nonoperator_twenty_operand_perspectives() {
        let tables = eligibility_wrapper_tables();
        let names = HashMap::from([("value", Cow::Owned(Type::I64))]);
        // Each operand position must accept a visible local and blame the same
        // leaf when its binding is absent, through all intervening wrappers.
        for position in 0..10 {
            for known in [false, true] {
                let leaf = eligibility_node(
                    HirExprKind::Var(if known { "value" } else { "missing" }.into()),
                    Type::I64,
                );
                let expr = eligibility_wrapper(leaf, position);
                tables.with_ctx(|ctx| {
                    assert_eq!(supported_expr(&expr, ctx, &names), known, "position={position}");
                    let rejected = minimal_unsupported_expr(&expr, ctx, &names);
                    if known {
                        assert!(rejected.is_none(), "position={position}");
                    } else {
                        assert!(matches!(&rejected.unwrap().kind, HirExprKind::Var(n) if n == "missing"), "position={position}");
                    }
                });
            }
        }
    }

    #[test]
    fn eligibility_mixed_nonoperator_chains_use_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let tables = eligibility_wrapper_tables();
                for known in [false, true] {
                    let mut expr = eligibility_node(HirExprKind::Var("value".into()), Type::I64);
                    for depth in 0..50_000 {
                        expr = eligibility_wrapper(expr, depth % 9);
                    }
                    let names = if known {
                        HashMap::from([("value", Cow::Owned(Type::I64))])
                    } else {
                        HashMap::new()
                    };
                    let (accepted, reason) = tables.with_ctx(|ctx| {
                        (
                            supported_expr(&expr, ctx, &names),
                            expr_rejection(&expr, ctx, &names),
                        )
                    });

                    drop(expr);
                    assert_eq!(accepted, known);
                    assert_eq!(reason.is_none(), known);
                    if let Some(reason) = reason {
                        assert!(reason.contains("value"));
                    }
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn eligibility_deep_scoped_match_and_select_use_one_megabyte() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let tables = eligibility_wrapper_tables();
                for select in [false, true] {
                    for known in [false, true] {
                        let mut expr =
                            eligibility_node(HirExprKind::Var("value".into()), Type::I64);
                        for _ in 0..512 {
                            let span = expr.span;
                            expr = if select {
                                eligibility_node(
                                    HirExprKind::Select {
                                        cases: vec![HirSelectCase {
                                            kind: HirSelectCaseKind::Default,
                                            body: vec![HirStmt::Expr(expr)],
                                            span,
                                        }],
                                    },
                                    Type::Void,
                                )
                            } else {
                                eligibility_node(
                                    HirExprKind::Match {
                                        scrutinee: Box::new(eligibility_node(
                                            HirExprKind::Int(1),
                                            Type::I64,
                                        )),
                                        arms: vec![HirMatchArm {
                                            pattern: HirPattern::Wildcard,
                                            body: vec![HirStmt::Expr(expr)],
                                            ty: Type::I64,
                                            span,
                                        }],
                                    },
                                    Type::I64,
                                )
                            };
                        }
                        let names = if known {
                            HashMap::from([("value", Cow::Owned(Type::I64))])
                        } else {
                            HashMap::new()
                        };
                        tables.with_ctx(|ctx| {
                            assert_eq!(
                                supported_expr(&expr, ctx, &names),
                                known,
                                "select={select}"
                            );
                            let reason = expr_rejection(&expr, ctx, &names);
                            assert_eq!(reason.is_none(), known, "select={select}: {reason:?}");
                        });
                        drop(expr);
                    }
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn eligibility_operator_positions_preserve_leaf_validation() {
        let program = crate::parser::ast::Program {
            module: None,
            imports: vec![],
            items: vec![],
        };
        let tables = TestTables::build(&program, &[], &[]);
        let span = Span::new(0, 0, 1, 1);
        let scalar = || HirExpr {
            kind: HirExprKind::Int(1),
            ty: Type::I64,
            span,
        };
        // Twenty perspectives: five operand positions times literal, bound
        // local, unknown local, and unsupported expression forms.
        for position in 0..5 {
            for leaf_case in 0..4 {
                let leaf = HirExpr {
                    kind: match leaf_case {
                        0 => HirExprKind::Int(1),
                        1 => HirExprKind::Var("bound".into()),
                        2 => HirExprKind::Var("missing".into()),
                        _ => HirExprKind::FnRef("missing_function".into()),
                    },
                    ty: Type::I64,
                    span,
                };
                let kind = match position {
                    0 => HirExprKind::Binary {
                        op: BinOp::Add,
                        lhs: Box::new(leaf),
                        rhs: Box::new(scalar()),
                    },
                    1 => HirExprKind::Binary {
                        op: BinOp::Add,
                        lhs: Box::new(scalar()),
                        rhs: Box::new(leaf),
                    },
                    2 => HirExprKind::Unary {
                        op: UnaryOp::Neg,
                        operand: Box::new(leaf),
                    },
                    3 => HirExprKind::Ternary {
                        condition: Box::new(HirExpr {
                            kind: HirExprKind::Bool(true),
                            ty: Type::Bool,
                            span,
                        }),
                        then_expr: Box::new(leaf),
                        else_expr: Box::new(scalar()),
                    },
                    _ => HirExprKind::Ternary {
                        condition: Box::new(HirExpr {
                            kind: HirExprKind::Bool(true),
                            ty: Type::Bool,
                            span,
                        }),
                        then_expr: Box::new(scalar()),
                        else_expr: Box::new(leaf),
                    },
                };
                let expr = HirExpr {
                    kind,
                    ty: Type::I64,
                    span,
                };
                let names = HashMap::from([("bound", Cow::Owned(Type::I64))]);
                assert_eq!(
                    tables.with_ctx(|ctx| supported_expr(&expr, ctx, &names)),
                    leaf_case < 2,
                    "position={position}, leaf={leaf_case}"
                );
                let rejected = tables.with_ctx(|ctx| minimal_unsupported_expr(&expr, ctx, &names));
                if leaf_case < 2 {
                    assert!(rejected.is_none());
                } else {
                    assert!(
                        matches!(
                            &rejected.unwrap().kind,
                            HirExprKind::Var(name) if name == "missing"
                        ) || matches!(
                            &rejected.unwrap().kind,
                            HirExprKind::FnRef(name) if name.is_free_named("missing_function")
                        )
                    );
                }
            }
        }
    }

    #[test]
    fn operator_rejection_preserves_first_child_and_parent_fallback() {
        let program = crate::parser::ast::Program {
            module: None,
            imports: vec![],
            items: vec![],
        };
        let tables = TestTables::build(&program, &[], &[]);
        let span = Span::new(0, 0, 1, 1);
        for missing in [None, Some(0), Some(1), Some(2)] {
            let child = |position, ty| HirExpr {
                kind: if missing.is_some_and(|first| position >= first) {
                    HirExprKind::Var(format!("missing{position}"))
                } else if ty == Type::Bool {
                    HirExprKind::Bool(true)
                } else {
                    HirExprKind::Int(1)
                },
                ty,
                span,
            };
            let expr = HirExpr {
                kind: HirExprKind::Ternary {
                    condition: Box::new(child(0, Type::Bool)),
                    then_expr: Box::new(child(1, Type::I64)),
                    else_expr: Box::new(child(2, Type::I64)),
                },
                // The parent itself fails the branch representation check.
                ty: Type::Bool,
                span,
            };
            let names = HashMap::new();
            let rejected = tables
                .with_ctx(|ctx| minimal_unsupported_expr(&expr, ctx, &names))
                .unwrap();
            if let Some(first) = missing {
                assert!(
                    matches!(&rejected.kind, HirExprKind::Var(name) if name == &format!("missing{first}"))
                );
            } else {
                assert!(std::ptr::eq(rejected, &expr));
            }
        }
    }

    #[test]
    fn eligibility_operator_chains_use_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let program = crate::parser::ast::Program {
                    module: None,
                    imports: vec![],
                    items: vec![],
                };
                let tables = TestTables::build(&program, &[], &[]);
                let span = Span::new(0, 0, 1, 1);
                for known in [false, true] {
                    let mut expr = HirExpr {
                        kind: HirExprKind::Var("value".into()),
                        ty: Type::I64,
                        span,
                    };
                    for depth in 0..50_000 {
                        let kind = match depth % 3 {
                            0 => HirExprKind::Unary {
                                op: UnaryOp::Neg,
                                operand: Box::new(expr),
                            },
                            1 => HirExprKind::Binary {
                                op: BinOp::Add,
                                lhs: Box::new(expr),
                                rhs: Box::new(HirExpr {
                                    kind: HirExprKind::Int(1),
                                    ty: Type::I64,
                                    span,
                                }),
                            },
                            _ => HirExprKind::Ternary {
                                condition: Box::new(HirExpr {
                                    kind: HirExprKind::Bool(true),
                                    ty: Type::Bool,
                                    span,
                                }),
                                then_expr: Box::new(expr),
                                else_expr: Box::new(HirExpr {
                                    kind: HirExprKind::Int(1),
                                    ty: Type::I64,
                                    span,
                                }),
                            },
                        };
                        expr = HirExpr {
                            kind,
                            ty: Type::I64,
                            span,
                        };
                    }
                    let names = if known {
                        HashMap::from([("value", Cow::Owned(Type::I64))])
                    } else {
                        HashMap::new()
                    };
                    let accepted = tables.with_ctx(|ctx| supported_expr(&expr, ctx, &names));
                    let rejection = tables.with_ctx(|ctx| expr_rejection(&expr, ctx, &names));

                    drop(expr);
                    assert_eq!(accepted, known);
                    if known {
                        assert!(rejection.is_none());
                    } else {
                        assert!(rejection.unwrap().contains("value"));
                    }
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn may_allocate_uses_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let span = Span::new(0, 0, 1, 1);
                let mut expr = HirExpr {
                    kind: HirExprKind::Int(0),
                    ty: Type::I64,
                    span,
                };
                for _ in 0..50_000 {
                    expr = HirExpr {
                        kind: HirExprKind::Unary {
                            op: UnaryOp::Neg,
                            operand: Box::new(expr),
                        },
                        ty: Type::I64,
                        span,
                    };
                }
                let allocates = may_allocate(&expr);
                drop(expr);
                assert!(!allocates);
            })
            .unwrap()
            .join()
            .unwrap();
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
            "fn g() -> i64 { println(0); return 1; } fn f() -> i64 { return g(); }",
            "f",
            &[] // g not in the known set
        ));
    }

    // 7. (updated by willow-0g8j.2.10) shadowing a `let` in an inner scope is
    // now eligible: HIR lowering α-renames the second binding, so the flat LIR
    // namespace never sees two `x` slots.
    #[test]
    fn e07_shadowing_eligible() {
        let src = "fn f(c: bool) -> i64 { let x = 1; if c { let x = 2; print(x); } return x; }";
        assert!(eligible(src, "f", &["f"]));
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

    // 11. both shared and mutable reference parameters use the pointer ABI and
    // are ordinary eligible LIR functions (willow-0g8j.2.7).
    #[test]
    fn e11_reference_params_eligible_via_hir() {
        let src = "fn bump(n: &mut i64) { n = n + 1; }";
        assert!(eligible(src, "bump", &["bump"]));
        let src2 = "fn read(n: &i64) -> i64 { return n; }";
        assert!(eligible(src2, "read", &["read"]));
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
    // 10. enum variants need resolved enum metadata
    // 11. an unsupported String operator (`<`) is rejected
    // 12. array, class, and interface representations are checked separately
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

    // 21. (updated by willow-0g8j.2.10) a `let` shadowing a PARAMETER is now
    // eligible for the same reason as e07 — the `let` is renamed, so the
    // parameter's storage keeps the source name to itself.
    #[test]
    fn e21_let_shadowing_param_eligible() {
        let src = "fn f(s: String) -> String { let s = \"other\"; return s; }";
        assert!(eligible(src, "f", &["f"]));
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

    // 32. (updated by willow-0g8j.2.10) an annotated empty literal reaches the
    // walker with the element type recorded by the checker.
    #[test]
    fn e32_annotated_empty_array_eligible() {
        let src = "import std::collections::Array; \
                   fn f() -> i64 { let mut xs: Array<i64> = []; xs.push(1); return xs.len(); }";
        assert!(eligible_checked(src, "f", &["f"]));
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

    // 37. `toString()` on an element type the runtime cannot render is rejected
    #[test]
    fn e37_nested_array_to_string_ineligible() {
        let src = "fn f() -> String { let xs = [[1], [2]]; return xs.toString(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // ---------------------------------------------------------------------
    // willow-0g8j.2.10 — remaining one-off LIR blockers.
    //
    // Perspectives 1-20 make the four acceptance surfaces explicit:
    //  1. a Range<i64> literal may be returned as a value
    //  2. Range<i64> may cross a parameter boundary
    //  3. Range.start and Range.end are eligible field reads
    //  4. a for loop may consume a stored Range value
    //  5. a for loop may consume a call-returned Range value
    //  6. an annotated empty Array<i64> literal is eligible
    //  7. an annotated empty Array<String> literal is eligible
    //  8. an annotated empty nested Array literal is eligible
    //  9. push after empty construction remains eligible
    // 10. an empty array may be returned from a function
    // 11. sibling for loops may reuse the same binding name
    // 12. a nested block may shadow an outer binding
    // 13. a let binding may shadow a parameter
    // 14. three sibling loops receive three distinct HIR names
    // 15. renamed bindings preserve the outer value after the inner scope
    // 16. void main(args: Array<String>) is eligible
    // 17. main may index its args array
    // 18. env::args() is eligible on the same path
    // 19. env::args_len() is eligible on the same path
    // 20. env::program_name() is eligible on the same path
    // ---------------------------------------------------------------------

    #[test]
    fn one_off_01_range_values_are_eligible() {
        let cases = [
            "fn f() -> Range<i64> { return 1..4; }",
            "fn id(r: Range<i64>) -> Range<i64> { return r; }",
            "fn f(r: Range<i64>) -> i64 { return r.end - r.start; }",
            "fn f(r: Range<i64>) -> i64 { let mut n = 0; for x in r { n = n + x; } return n; }",
            "fn make() -> Range<i64> { return 0..3; } fn f() -> i64 { let mut n = 0; for x in make() { n = n + x; } return n; }",
        ];
        for src in cases {
            let name = if src.starts_with("fn id") { "id" } else { "f" };
            assert!(eligible_checked(src, name, &["make", "id", "f"]), "{src}");
        }
    }

    #[test]
    fn one_off_02_context_typed_empty_arrays_are_eligible() {
        let cases = [
            "import std::collections::Array; fn f() -> i64 { let xs: Array<i64> = []; return xs.len(); }",
            "import std::collections::Array; fn f() -> i64 { let xs: Array<String> = []; return xs.len(); }",
            "import std::collections::Array; fn f() -> i64 { let xs: Array<Array<i64>> = []; return xs.len(); }",
            "import std::collections::Array; fn f() -> i64 { let xs: Array<i64> = []; xs.push(7); return xs[0]; }",
            "import std::collections::Array; fn f() -> Array<i64> { let xs: Array<i64> = []; return xs; }",
        ];
        for src in cases {
            assert!(eligible_checked(src, "f", &["f"]), "{src}");
        }
    }

    #[test]
    fn one_off_03_shadowed_bindings_are_alpha_renamed() {
        let cases = [
            "fn f() -> i64 { let xs = [1]; let mut n = 0; for x in xs { n = n + x; } for x in xs { n = n + x; } return n; }",
            "fn f(c: bool) -> i64 { let x = 1; if c { let x = 2; print(x); } return x; }",
            "fn f(x: i64) -> i64 { let x = x + 1; return x; }",
            "fn f() -> i64 { let xs = [1]; let mut n = 0; for x in xs { n = n + x; } for x in xs { n = n + x; } for x in xs { n = n + x; } return n; }",
            "fn f(c: bool) -> i64 { let x = 7; if c { let x = 9; print(x); } return x; }",
        ];
        for src in cases {
            assert!(eligible_checked(src, "f", &["f"]), "{src}");
        }
    }

    #[test]
    fn one_off_04_main_args_and_env_calls_are_eligible() {
        let cases = [
            "import std::collections::Array; fn main(args: Array<String>) { println(args.len()); }",
            "import std::collections::Array; fn main(args: Array<String>) { if args.len() > 0 { println(args[0]); } }",
            "import std::collections::Array; fn main(args: Array<String>) { let xs = env::args(); println(xs.len()); }",
            "import std::collections::Array; fn main(args: Array<String>) { println(env::args_len()); }",
            "import std::collections::Array; fn main(args: Array<String>) { println(env::program_name()); }",
        ];
        for src in cases {
            assert!(eligible_checked(src, "main", &["main"]), "{src}");
        }
    }

    // ---------------------------------------------------------------------
    // willow-nswv — a builtin namespace reached through an `import` alias.
    //
    // Declaration normalization folds aliases with `normalize_std_collection_program`.
    // The walker lowers to HIR from the
    // RAW frontend program, so it does, and resolves the alias at the point of
    // dispatch — before the gate that lets a user module of the same name win,
    // which is the order those two passes apply between them.
    //
    //  na1 every aliasable namespace is admitted through its alias
    //  na2 an alias nobody declared is not invented
    //  na3 a self-alias resolves to itself
    //  na4 the alias reaches the same table entry as the canonical name
    //  na5 a user module of the canonical name still wins
    //  na6 `f64::` is answered before the alias map

    #[test]
    fn na1_aliased_namespace_calls_are_eligible() {
        let cases = [
            "import std::fs as files; fn f(p: String) -> bool { return files::exists(p); }",
            "import std::env as sys; fn f() -> i64 { return sys::args_len(); }",
            "import std::env as sys; fn f() -> String { return sys::program_name(); }",
            "import std::net as sock; fn f(a: String) -> Result<TcpListener, IoError> { return sock::bind(a); }",
        ];
        for src in cases {
            assert!(eligible_checked(src, "f", &["f"]), "{src}");
        }
    }

    #[test]
    fn na2_an_undeclared_alias_is_not_a_namespace() {
        // No `import` records `files`, so the call is an ordinary static call
        // to a module that does not exist. Admitting it would mean emitting a
        // runtime call the program never asked for.
        let src = "fn f(p: String) -> bool { return files::exists(p); }";
        assert!(!eligible_lenient(src, "f", &["f"]), "{src}");
    }

    #[test]
    fn na3_a_self_alias_resolves_to_itself() {
        let src = "import std::fs as fs; fn f(p: String) -> bool { return fs::exists(p); }";
        assert!(eligible_checked(src, "f", &["f"]), "{src}");
    }

    #[test]
    fn na4_an_alias_reaches_the_same_entry_as_the_canonical_name() {
        let mut aliases = HashMap::new();
        aliases.insert("files".to_string(), "fs".to_string());
        aliases.insert("par".to_string(), "parallel".to_string());
        let empty = HashMap::new();
        for (alias, canonical, method) in [
            ("files", "fs", "exists"),
            ("files", "fs", "read_to_string"),
            ("par", "parallel", "map"),
        ] {
            let through =
                namespace_builtin_call(&ModuleSymbols::default(), &aliases, alias, method)
                    .unwrap_or_else(|| panic!("{alias}::{method} has no entry"));
            let direct =
                namespace_builtin_call(&ModuleSymbols::default(), &empty, canonical, method)
                    .unwrap_or_else(|| panic!("{canonical}::{method} has no entry"));
            assert_eq!(through.runtime, direct.runtime, "{alias}::{method}");
            assert_eq!(through.params, direct.params, "{alias}::{method}");
            assert_eq!(through.ret, direct.ret, "{alias}::{method}");
            assert_eq!(
                through.narrow_to_bool, direct.narrow_to_bool,
                "{alias}::{method}"
            );
        }
    }

    #[test]
    fn na5_a_user_module_wins_only_through_its_access_name() {
        // `files` and `fs` are distinct access paths even though the std alias
        // maps to the same canonical namespace name (willow-0g8j.3).
        let mut known = ModuleSymbols::default();
        known.insert("fs".to_string(), "fs__".to_string());
        let mut aliases = HashMap::new();
        aliases.insert("files".to_string(), "fs".to_string());
        assert!(namespace_builtin_call(&known, &aliases, "files", "exists").is_some());
        assert!(namespace_builtin_call(&known, &aliases, "fs", "exists").is_none());
        // Without the user module the same call IS the builtin.
        assert!(
            namespace_builtin_call(&ModuleSymbols::default(), &aliases, "files", "exists")
                .is_some()
        );
    }

    #[test]
    fn na6_f64_is_answered_before_the_alias_map() {
        // `f64::` reaches no module and is not in the stdlib schema, so it is
        // answered ahead of everything an `import` could have renamed.
        let mut aliases = HashMap::new();
        aliases.insert("f64".to_string(), "fs".to_string());
        let entry = namespace_builtin_call(&ModuleSymbols::default(), &aliases, "f64", "to_string")
            .expect("f64::to_string has an entry");
        assert_eq!(entry.runtime, "willow_f64_to_string");
        assert!(
            namespace_builtin_call(&ModuleSymbols::default(), &aliases, "f64", "exists").is_none()
        );
    }

    // ---------------------------------------------------------------------
    // willow-vtlr — a bare module class name resolves against the modules the
    // unit being compiled can SEE, and only then against every module the
    // build declared.
    //
    // Perspectives 1-10:
    //  1. a name the unit's own tables answer is never re-resolved
    //  2. the one visible module that declares it answers
    //  3. an unrelated INVISIBLE module of the same name no longer blocks it
    //  4. two VISIBLE modules with different classes of that name: ambiguous
    //  5. two spellings of ONE class (same type_id) are one answer
    //  6. nothing visible declares it: the all-modules scan still answers
    //  7. nothing visible, and the invisible ones disagree: ambiguous
    //  8. a class no module declares at all
    //  9. a candidate with no type_id is never merged by name
    // 10. a visible name that is not a module is ignored

    /// The two tables [`resolve_class_key`] reads: layouts by class key, and
    /// the runtime type id of each (a negative id in the fixture means the
    /// class has none).
    type ClassTables = (TypeMap<Vec<(String, Type)>>, TypeMap<i64>);

    /// `class_layouts` and `class_type_ids` holding one empty class per entry.
    fn class_tables(classes: &[(&str, i64)]) -> ClassTables {
        let mut layouts = TypeMap::new();
        let mut ids = TypeMap::new();
        for (name, id) in classes {
            layouts.insert((*name).to_string(), Vec::new());
            if *id >= 0 {
                ids.insert((*name).to_string(), *id);
            }
        }
        (layouts, ids)
    }

    fn modules(names: &[&str]) -> ModuleSymbols {
        names
            .iter()
            .map(|n| ((*n).to_string(), format!("{n}.")))
            .collect()
    }

    fn visible(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn vm1_an_own_class_is_never_re_resolved() {
        let (layouts, ids) = class_tables(&[("Point", 1), ("a::Point", 2)]);
        assert_eq!(
            resolve_class_key(&layouts, &ids, &modules(&["a"]), &visible(&["a"]), "Point"),
            Some(TypeId::from_source_name("Point"))
        );
    }

    #[test]
    fn vm2_the_visible_module_answers() {
        let (layouts, ids) = class_tables(&[("a::Point", 1)]);
        assert_eq!(
            resolve_class_key(&layouts, &ids, &modules(&["a"]), &visible(&["a"]), "Point"),
            Some(TypeId::from_source_name("a::Point"))
        );
    }

    #[test]
    fn vm3_an_unimported_module_of_the_same_name_does_not_block() {
        // The bug this fixes: `b` is a module the entry never imported, and its
        // unrelated `Point` used to make the name ambiguous and cost the body
        // its lowering.
        let (layouts, ids) = class_tables(&[("a::Point", 1), ("b::Point", 2)]);
        assert_eq!(
            resolve_class_key(
                &layouts,
                &ids,
                &modules(&["a", "b"]),
                &visible(&["a"]),
                "Point"
            ),
            Some(TypeId::from_source_name("a::Point"))
        );
    }

    #[test]
    fn vm4_two_visible_modules_of_the_same_name_are_ambiguous() {
        // Both are in scope at this site, so nothing here can say which layout
        // the name means: refuse, rather than pick one.
        let (layouts, ids) = class_tables(&[("a::Point", 1), ("b::Point", 2)]);
        assert_eq!(
            resolve_class_key(
                &layouts,
                &ids,
                &modules(&["a", "b"]),
                &visible(&["a", "b"]),
                "Point"
            ),
            None
        );
    }

    #[test]
    fn vm5_two_spellings_of_one_class_are_one_answer() {
        // One module imported under two access names is one runtime class, so
        // the shared type_id makes the two keys agree.
        let (layouts, ids) = class_tables(&[("c::Point", 7), ("checks::Point", 7)]);
        let key = resolve_class_key(
            &layouts,
            &ids,
            &modules(&["c", "checks"]),
            &visible(&["c", "checks"]),
            "Point",
        )
        .expect("one class, two spellings");
        assert!(
            key == "c::Point".into() || key == "checks::Point".into(),
            "{key}"
        );
    }

    #[test]
    fn vm6_an_invisible_module_still_answers_when_nothing_visible_does() {
        // A class reached through a module only ANOTHER module imports keeps
        // resolving: the visible pass is a preference, not a filter.
        let (layouts, ids) = class_tables(&[("deep::Point", 1)]);
        assert_eq!(
            resolve_class_key(
                &layouts,
                &ids,
                &modules(&["deep"]),
                &visible(&["shallow"]),
                "Point"
            ),
            Some(TypeId::from_source_name("deep::Point"))
        );
    }

    #[test]
    fn vm7_invisible_modules_that_disagree_are_still_ambiguous() {
        let (layouts, ids) = class_tables(&[("a::Point", 1), ("b::Point", 2)]);
        assert_eq!(
            resolve_class_key(
                &layouts,
                &ids,
                &modules(&["a", "b"]),
                &HashSet::new(),
                "Point"
            ),
            None
        );
    }

    #[test]
    fn vm8_a_class_no_module_declares_resolves_to_nothing() {
        let (layouts, ids) = class_tables(&[("a::Point", 1)]);
        assert_eq!(
            resolve_class_key(&layouts, &ids, &modules(&["a"]), &visible(&["a"]), "Rect"),
            None
        );
    }

    #[test]
    fn vm9_a_candidate_without_a_type_id_is_never_merged() {
        // No id means no proof the two names are one class, so they cannot be
        // merged even when they are both visible.
        let (layouts, ids) = class_tables(&[("a::Point", -1), ("b::Point", -1)]);
        assert_eq!(
            resolve_class_key(
                &layouts,
                &ids,
                &modules(&["a", "b"]),
                &visible(&["a", "b"]),
                "Point"
            ),
            None
        );
    }

    #[test]
    fn vm10_a_visible_name_that_is_not_a_module_is_ignored() {
        // A unit's visible set can hold names that are not modules of this
        // build at all; only the ones `known_modules` knows select a candidate.
        let (layouts, ids) = class_tables(&[("a::Point", 1)]);
        assert_eq!(
            resolve_class_key(
                &layouts,
                &ids,
                &modules(&["a"]),
                &visible(&["fs", "Point", "a"]),
                "Point"
            ),
            Some(TypeId::from_source_name("a::Point"))
        );
    }

    // ---------------------------------------------------------------------
    // willow-0g8j.2.6 — static-property reads, stores and initialization.
    //
    // Perspectives 1-20:
    //  1. read an i64 static
    //  2. assign an i64 static mut
    //  3. update a static relative to its previous value
    //  4. assign a bool static mut
    //  5. assign an f64 static mut
    //  6. assign a String static mut
    //  7. a static method assigns its class static
    //  8. an instance method assigns its class static
    //  9. a subclass-qualified store resolves inherited storage
    // 10. a String RHS may allocate before the global store
    // 11. a call result may be stored
    // 12. a store in a while loop is eligible
    // 13. a store in an if branch is eligible
    // 14. repeated stores in one function are eligible
    // 15. two static properties keep distinct storage
    // 16. a static initializer may read an earlier static of the same class
    // 17. a chain of same-class initializers preserves declaration order
    // 18. a later class initializer may read an earlier class's static
    // 19. an Array static mut may be replaced
    // 20. an Option static mut may be replaced
    //
    // Initializers themselves are emitted by the shared `__willow_static_init`
    // path, in `static_init_order`; perspectives 16-18 prove the functions that
    // consume those initialized slots remain eligible. The forced-LIR runnable
    // examples prove their runtime order and output end to end.
    // ---------------------------------------------------------------------

    #[test]
    fn static_lir_01_scalar_and_gc_stores_are_eligible() {
        let cases = [
            "class S { pub static n: i64 = 1; } fn f() -> i64 { return S::n; }",
            "class S { pub static mut n: i64 = 1; } fn f() { S::n = 2; }",
            "class S { pub static mut n: i64 = 1; } fn f() { S::n = S::n + 1; }",
            "class S { pub static mut b: bool = false; } fn f() { S::b = true; }",
            "class S { pub static mut x: f64 = 1.0; } fn f() { S::x = 2.5; }",
            "class S { pub static mut s: String = \"a\"; } fn f() { S::s = \"b\"; }",
        ];
        for src in cases {
            assert!(eligible_checked(src, "f", &["f"]), "{src}");
        }
    }

    #[test]
    fn static_lir_02_method_and_inherited_stores_are_eligible() {
        let cases = [
            (
                "class S { pub static mut n: i64 = 0; pub static fn bump() { S::n = S::n + 1; } }",
                "S::bump",
            ),
            (
                "class S { pub static mut n: i64 = 0; pub fn bump(self) { S::n = S::n + 1; } }",
                "S::bump",
            ),
            (
                "open class Base { pub static mut n: i64 = 0; } class Child extends Base {} fn f() { Child::n = 3; }",
                "f",
            ),
            (
                "class S { pub static mut s: String = \"a\"; } fn f() { S::s = S::s + \"b\"; }",
                "f",
            ),
            (
                "class S { pub static mut n: i64 = 0; } fn value() -> i64 { return 7; } fn f() { S::n = value(); }",
                "f",
            ),
        ];
        for (src, name) in cases {
            assert!(eligible_checked(src, name, &["f", "value"]), "{src}");
        }
    }

    #[test]
    fn static_lir_03_control_flow_and_distinct_slots_are_eligible() {
        let cases = [
            "class S { pub static mut n: i64 = 0; } fn f() { let mut i = 0; while i < 3 { S::n = i; i = i + 1; } }",
            "class S { pub static mut n: i64 = 0; } fn f(c: bool) { if c { S::n = 1; } }",
            "class S { pub static mut n: i64 = 0; } fn f() { S::n = 1; S::n = 2; }",
            "class S { pub static mut a: i64 = 0; pub static mut b: i64 = 0; } fn f() { S::a = 1; S::b = 2; }",
        ];
        for src in cases {
            assert!(eligible_checked(src, "f", &["f"]), "{src}");
        }
    }

    #[test]
    fn static_lir_04_initializer_order_and_composite_slots_are_eligible() {
        let cases = [
            "class S { pub static a: i64 = 2; pub static b: i64 = S::a + 3; } fn f() -> i64 { return S::b; }",
            "class S { pub static a: i64 = 2; pub static b: i64 = S::a + 3; pub static c: i64 = S::b + 4; } fn f() -> i64 { return S::c; }",
            "class A { pub static n: i64 = 2; } class B { pub static n: i64 = A::n + 3; } fn f() -> i64 { return B::n; }",
            "import std::collections::Array; class S { pub static mut xs: Array<i64> = [1]; } fn f() { S::xs = [2, 3]; }",
            "class S { pub static mut x: Option<i64> = Option::None; } fn f() { S::x = Option::Some(7); }",
        ];
        for src in cases {
            assert!(eligible_checked(src, "f", &["f"]), "{src}");
        }
    }

    // ---------------------------------------------------------------------
    // willow-0g8j.5 — class objects and field access in the LIR walker.
    //
    // This block began with the original "simple class" subset. The current
    // walker also claims inherited layouts and virtual dispatch (2.4),
    // interface boxing/dispatch, and enum payloads; these tests remain the
    // baseline for direct class layout and field operations.
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

    // 51. (updated by willow-0g8j.2.4) a subclass is IN the subset. Its layout
    // is its base's fields followed by its own — the order
    // `finalize_class_layouts` builds and the one the walker now models — and a
    // method call on it is routed by `plan_virtual_call`, the one function that
    // decides which implementation a call site runs.
    #[test]
    fn e51_subclass_eligible() {
        let src = "pub open class Animal { pub age: i64; } \
                   pub class Dog extends Animal { } \
                   fn f(d: Dog) -> i64 { return d.age; }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 52. (updated by willow-0g8j.2.4) a BASE-typed slot is in too. A caller
    // may hand it a subclass instance, but a subclass's layout EXTENDS its
    // base's, so `a.age` sits at the same offset either way; anything virtual
    // on `a` goes through the descriptor slot rather than being bound to
    // `Animal__..` by name.
    #[test]
    fn e52_base_class_eligible() {
        let src = "pub open class Animal { pub age: i64; } \
                   pub class Dog extends Animal { } \
                   fn f(a: Animal) -> i64 { return a.age; }";
        assert!(eligible_lenient(src, "f", &["f"]));
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

    // 57. a method call passes the address of the caller's place for a
    // by-reference parameter.
    #[test]
    fn e57_reference_param_method_eligible() {
        let src = "class Counter { pub n: i64; \
                   pub fn bump(self, v: &mut i64) { v = v + 1; } } \
                   fn f(c: Counter) -> i64 { let mut k = 1; c.bump(&k); return k; }";
        assert!(
            eligible_lenient(src, "f", &["f"]),
            "{:?}",
            reason_of(src, "f", &["f"])
        );
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

        // A class in an inheritance hierarchy is in the subset as of
        // willow-0g8j.2.4, so an option over one is in too: the payload is one
        // object pointer whichever concrete class it holds.
        let inherited = "open class Base { pub v: i64; } class Sub extends Base {} \
                         fn f(b: Base?) -> i64 { return 1; }";
        assert!(eligible_lenient(inherited, "f", &["f"]));

        // A generic interface remains a supported pointer payload in Option.
        let generic_interface = "interface Boxed<T> { fn get(self) -> T; } \
                           fn f(b: Boxed<String>?) -> i64 { return 1; }";
        assert!(eligible_lenient(generic_interface, "f", &["f"]));
    }
    // 62. a class reached through a DIRECT TYPE IMPORT is the same class as its
    // module-qualified self, so every ancestry question about it must be asked
    // in `type_id` space. `import zoo::Animal;` registers the class a second
    // time under `Animal`, sharing the canonical `type_id`, while `class_base`
    // keeps canonical names on both sides (`zoo::Dog` -> `zoo::Animal`) — so
    // the name `Animal` never appears in the base chain at all. Comparing names
    // would call the two unrelated and reject a valid store.
    #[test]
    fn e62_imported_base_class_alias_widens_by_type_id() {
        let src = "class Animal { pub value: i64; } fn f(a: Animal) -> i64 { return a.value; }";
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        assert!(diags.is_empty(), "{diags:?}");
        let p = crate::ir::lowered::lower_program(&hir);
        let f = p
            .functions
            .iter()
            .find(|f| f.name.to_string() == "f")
            .unwrap();

        let mut tables = TestTables::build(&program, &["f"], &[]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(f, ctx)));

        // Now model the import: `Animal` is an alias of `zoo::Animal`, which
        // `zoo::Dog` extends. Only the canonical names appear in `class_base`.
        let animal_id = tables.class_type_ids["Animal"];
        let animal_layout = tables.class_layouts["Animal"].clone();
        tables
            .class_type_ids
            .insert("zoo::Animal".to_string(), animal_id);
        tables
            .class_type_ids
            .insert("zoo::Dog".to_string(), animal_id + 100);
        tables
            .class_layouts
            .insert("zoo::Animal".to_string(), animal_layout.clone());
        tables
            .class_layouts
            .insert("zoo::Dog".to_string(), animal_layout);
        tables
            .class_base
            .insert("zoo::Dog".to_string(), "zoo::Animal".to_string().into());

        let dog = Type::Named("zoo::Dog".to_string().into());
        let animal = Type::Named("Animal".to_string().into());
        assert!(
            tables.with_ctx(|ctx| ctx.class_widening(&animal, &dog)),
            "a subclass must fit its base's slot through the alias spelling"
        );
        // ... and the relation stays directional: the base does not fit a
        // subclass slot, whichever name it is reached by.
        assert!(!tables.with_ctx(|ctx| ctx.class_widening(&dog, &animal)));
        assert!(!tables.with_ctx(|ctx| {
            ctx.class_widening(&dog, &Type::Named("zoo::Animal".to_string().into()))
        }));
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
            &array(Type::Named("Point".to_string().into())),
            &array(Type::Named("Other".to_string().into()))
        ));
        assert!(!assignable_repr(&array(Type::String), &array(Type::Void)));
    }

    // ---------------------------------------------------------------------
    // willow-j260 — class → interface boxing coercion in the LIR walker.
    //
    // An interface value is a 16-byte `[object | vtable]` GC box, so putting a
    // class instance in an interface-typed slot is a conversion, not a
    // reinterpretation. The walker now emits it at every STORE position, and
    // only there. Interface dispatch is covered separately (willow-0g8j.6);
    // interfaces do not expose a concrete field layout.
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

    // j07. (updated by willow-0g8j.2.4) an ANNOTATED array literal takes the
    // annotation's element type — `lower.rs` retypes it, as the checker's
    // `check_array_literal_expecting` already did — so
    // `let xs: Array<Named> = [new Item("a")]` reaches the walker as an
    // `Array<Named>` whose ELEMENTS each need boxing, not as an `Array<Item>`
    // handle needing a conversion that does not exist. The literal emitter
    // stores every element through `coerce_to_target`, so each one is boxed
    // individually and the handle itself is already the right thing.
    #[test]
    fn j07_widening_array_literal_boxes_each_element() {
        let src = format!(
            "import std::collections::Array; {NAMED} \
             fn f() -> i64 {{ let xs: Array<Named> = [new Item(\"a\")]; return xs.len(); }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));

        // An array literal whose elements ALREADY match the slot is fine.
        let exact = format!(
            "import std::collections::Array; {NAMED} \
             fn f(n: Named) -> i64 {{ let xs: Array<Named> = [n]; return xs.len(); }}"
        );
        assert!(eligible_checked(&exact, "f", &["f"]));

        // The per-element boxing is a real requirement, not an assumption: with
        // the vtable table emptied there is no box to build, and the same
        // literal fails validation rather than storing a bare `Item` into an
        // interface-typed element slot.
        let (f, mut tables) = lir_fn_and_tables(&src, "f", &["f"]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
        tables.vtables.clear();
        assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));

        // Element types are still compared exactly where no coercion runs: it
        // is why [`assignable_repr`] looks INSIDE an array handle rather than
        // calling any two of them interchangeable.
        assert!(!assignable_repr(
            &Type::Array(Box::new(Type::Named("Named".to_string().into()))),
            &Type::Array(Box::new(Type::Named("Item".to_string().into())))
        ));
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
        let f = p
            .functions
            .iter()
            .find(|f| f.name.to_string() == "f")
            .unwrap();

        let mut tables = TestTables::build(&program, &["f"], &[]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(f, ctx)));
        tables.vtables.clear();
        assert!(!tables.with_ctx(|ctx| lir_supported_function(f, ctx)));
    }

    // j16. (updated by willow-0g8j.2.4) a class that takes part in inheritance
    // is an ordinary box SOURCE: the box carries the object pointer and the
    // interface vtable of the CONCRETE class, which is emitted per class
    // regardless of what that class extends.
    #[test]
    fn j16_boxing_an_inheriting_class_is_eligible() {
        let src = "interface Named { fn name(self) -> String; } \
                   pub open class Animal { pub age: i64; } \
                   pub class Dog extends Animal implements Named { \
                   pub fn name(self) -> String { return \"dog\"; } } \
                   fn f() -> Named { return new Dog(1); }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // j17. (updated by willow-0g8j.2.4) interface → SUPER-interface is the
    // coercion `coerce_to_target` performs by doing NOTHING: the same box is
    // reused. That is sound exactly when the target's vtable slots are a PREFIX
    // of the source's, which is what the walker tests — not "is a super",
    // because composition puts a second super's methods AFTER the first's.
    //
    // Checked, not lenient: without desugar the interface method order is
    // uncomposed, so a lenient run would answer from a `B` that does not yet
    // carry `A`'s methods at all — passing for a reason unrelated to the rule.
    #[test]
    fn j17_interface_to_super_interface_eligible() {
        let single = "interface A { fn a(self) -> i64; } \
                      interface B extends A { fn b(self) -> i64; } \
                      fn f(x: B) -> A { return x; }";
        assert!(eligible_checked(single, "f", &["f"]));

        // The prefix runs one way only: an `A` box has no slot for `b`.
        let narrowing = "interface A { fn a(self) -> i64; } \
                         interface B extends A { fn b(self) -> i64; } \
                         fn f(x: B) -> A { return x; } \
                         fn g(x: A) -> i64 { return x.a(); }";
        assert!(eligible_checked(narrowing, "g", &["f", "g"]));
    }

    // j17b. with TWO supers the second one's table starts at a non-zero offset.
    // The walker admits it now that `coerce_to_target` allocates a rewidened
    // box pointing at that embedded region (willow-1fc6).
    #[test]
    fn j17b_interface_to_second_super_eligible() {
        let decls = "interface A { fn a(self) -> i64; } \
                     interface B { fn b(self) -> i64; } \
                     interface C extends A, B { fn c(self) -> i64; } ";
        let second = format!("{decls} fn f(x: C) -> B {{ return x; }}");
        assert!(eligible_checked(&second, "f", &["f"]));

        // ... while widening the same `C` to its FIRST super stays in, so this
        // does not pass by rejecting multi-super interfaces wholesale.
        let first = format!("{decls} fn f(x: C) -> A {{ return x; }}");
        assert!(eligible_checked(&first, "f", &["f"]));
    }

    // j18. a class can be boxed into a concrete generic interface target.
    #[test]
    fn j18_generic_interface_target_eligible() {
        let src = "interface Boxed<T> { fn get(self) -> T; } \
                   class SBox implements Boxed<String> { pub v: String; \
                   pub fn get(self) -> String { return self.v; } } \
                   fn f() -> Boxed<String> { return new SBox(\"a\"); }";
        assert!(eligible_lenient(src, "f", &["f"]));
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
    // interface must fail validation rather than store two raw class pointers.
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
            Type::Named("Self".to_string().into()),
        ));
        let shape = Type::Named("Shape".to_string().into());
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
        let names: HashMap<&str, Cow<'_, Type>> = HashMap::from([("s", Cow::Borrowed(&shape))]);
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

    // k14. a GENERIC interface dispatches through the bare-name vtable with a
    // signature instantiated from the receiver's type arguments.
    #[test]
    fn k14_generic_interface_receiver_eligible() {
        let src = "interface Boxed<T> { fn get(self) -> T; } \
                   class SBox implements Boxed<String> { pub v: String; \
                   pub fn get(self) -> String { return self.v; } } \
                   fn f(b: Boxed<String>) -> String { return b.get(); }";
        assert!(eligible_checked(src, "f", &["f"]));
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
        methods[1].1[1] = Type::Generic("Map".to_string().into(), vec![Type::String, Type::I64]);
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

    // k24. Reference arguments survive lowering as explicit places, allowing
    // the walker to preserve address semantics through virtual dispatch.
    #[test]
    fn k24_reference_argument_reaches_the_lir() {
        let src = "fn bump(x: &mut i64) { x = x + 1; } \
                   fn f() -> i64 { let mut x = 1; bump(&x); return x; }";
        let diags = checked_lowering_diags(src);
        assert!(diags.is_empty(), "reference lowering failed: {diags:?}");
        assert!(
            eligible_lenient(src, "f", &["bump", "f"]),
            "{:?}",
            reason_of(src, "f", &["bump", "f"])
        );
    }

    // k25. TABLE DRIFT, `&mut` parameter: a by-value argument must not be sent
    // to a slot whose ABI now expects a pointer.
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
    // `+`, `==` and `!=` for strings, so `**` there must be rejected rather than
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
    // Map keys follow the runtime `MapKey::Int | Str` representation: String
    // keys carry the reference flag; i64, bool, and f64 keys occupy one raw
    // word. Other key types fail validation. `get` returns the Option
    // representation selected for the map's value type.
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
    // c06. `toString` on a non-renderable value type is rejected
    // c07. `freeze` on a map is eligible
    // c08. `FrozenMap` answers `len` and `contains`
    // c09. `FrozenArray` answers `len`
    // c10. indexing a `FrozenArray` is eligible (it lowers like an array read)
    // c11. `freeze` on an array is eligible
    // c12. `Map::get` returns the value type's Option representation
    // c13. `FrozenMap::get` uses the same Option representation
    // c14. a `bool` key uses the raw-word MapKey representation
    // c15. an `f64` key uses its raw bits as the key word
    // c16. unsupported value layouts are rejected
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
        Type::Generic("Map".to_string().into(), vec![key, value])
    }

    fn frozen_map_ty(key: Type, value: Type) -> Type {
        Type::Generic("FrozenMap".to_string().into(), vec![key, value])
    }

    fn frozen_array_ty(elem: Type) -> Type {
        Type::Generic("FrozenArray".to_string().into(), vec![elem])
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
            let names: HashMap<&str, Cow<'_, Type>> =
                HashMap::from([("m", Cow::Borrowed(&int_map))]);
            assert!(supported_expr(&renderable, ctx, &names));
            let names: HashMap<&str, Cow<'_, Type>> =
                HashMap::from([("m", Cow::Borrowed(&array_map))]);
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
        // A generic interface remains representable when nested in the map's
        // Option result.
        let generic_interface = format!(
            "{MAP_IMPORT} interface Boxed<T> {{ fn get(self) -> T; }} \
             fn f(m: Map<String, Boxed<String>>) -> i64 {{ \
             let b = m.get(\"a\"); return m.len(); }}"
        );
        assert!(eligible_checked(&generic_interface, "f", &["f"]));
    }

    // c13. the frozen kind is claimed on the same terms
    #[test]
    fn c13_frozen_map_get_eligible() {
        let src = "fn f(m: FrozenMap<String, i64>) -> i64 { return m.get(\"a\").unwrap_or(0); }";
        assert!(eligible_checked(src, "f", &["f"]));
        let control = "fn f(m: FrozenMap<String, i64>) -> i64 { return m.len(); }";
        assert!(eligible_checked(control, "f", &["f"]));
    }

    // c14. a `bool` key is one word the runtime stores verbatim, so it is in
    // the subset like every other scalar key.
    #[test]
    fn c14_bool_key_eligible() {
        let src = format!("{MAP_IMPORT} fn f(m: Map<bool, i64>) -> i64 {{ return m.len(); }}");
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // c15. so is an `f64` key: the flag says "not a reference" and the bits go
    // in as the key word, preserving its runtime representation.
    #[test]
    fn c15_float_key_eligible() {
        let src = format!("{MAP_IMPORT} fn f(m: Map<f64, i64>) -> i64 {{ return m.len(); }}");
        assert!(eligible_checked(&src, "f", &["f"]));
    }

    // c15b. a REFERENCE key that is not a `String` is the one the runtime
    // cannot read: it would take the inner map's pointer for a `WillowString`.
    #[test]
    fn c15b_reference_key_other_than_string_ineligible() {
        let src = format!(
            "{MAP_IMPORT} fn f(m: Map<Map<String, i64>, i64>) -> i64 {{ return m.len(); }}"
        );
        assert!(!eligible_checked(&src, "f", &["f"]));
    }

    // c16. a generic interface is an admitted GC value and can be a map value.
    #[test]
    fn c16_generic_interface_value_type_eligible() {
        let src = format!(
            "{MAP_IMPORT} interface Boxed<T> {{ fn get(self) -> T; }} \
             fn f(m: Map<String, Boxed<String>>) -> i64 {{ return m.len(); }}"
        );
        assert!(eligible_checked(&src, "f", &["f"]));

        // The control: the same map over a type the walker does represent, so
        // this cannot pass by rejecting `Map` wholesale.
        let control =
            format!("{MAP_IMPORT} fn f(m: Map<String, String>) -> i64 {{ return m.len(); }}");
        assert!(eligible_checked(&control, "f", &["f"]));
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
                class: "Map".to_string().into(),
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
                class: "Map".to_string().into(),
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
                class: "Other".to_string().into(),
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
            // `Range<i64>` is in the subset as of willow-0g8j.2.10; every
            // OTHER instantiation of the name is still out, which is what
            // makes the point that the name alone decides nothing.
            Type::Generic("Range".to_string().into(), vec![Type::String]),
            Type::Generic("Holder".to_string().into(), vec![Type::I64]),
        ] {
            assert!(lir_collection(&ty).is_none(), "{ty:?} is not a collection");
            let tables = empty_tables();
            tables.with_ctx(|ctx| assert!(!ctx.supported_type(&ty), "{ty:?} is not storable"));
        }
        let channel = Type::Generic("Channel".to_string().into(), vec![Type::I64]);
        let task = Type::Generic("Task".to_string().into(), vec![Type::I64]);
        assert!(lir_collection(&channel).is_none());
        assert!(lir_collection(&task).is_none());
        let tables = empty_tables();
        tables.with_ctx(|ctx| {
            assert!(ctx.supported_type(&channel));
            assert!(ctx.supported_type(&task));
        });
        // `Option`/`Result` are storable as of willow-0g8j.2.1, but they are
        // ENUMS, not collections: nothing may route them into the map/array
        // emission paths.
        for ty in [
            Type::Generic("Option".to_string().into(), vec![Type::I64]),
            Type::Generic("Result".to_string().into(), vec![Type::I64, Type::String]),
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
            Type::Named("Map".to_string().into()),
            Type::Generic("Map".to_string().into(), vec![Type::String]),
            Type::Generic("FrozenArray".to_string().into(), vec![Type::I64, Type::I64]),
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
    // enum program retained the former AST emitter's behavior, including under
    // WILLOW_GC_STRESS=alloc — is pinned in tests/integration/codegen.rs; these
    // pin the validation boundary that every emitted body must satisfy.

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

        // A generic interface is also a supported concrete payload.
        let generic_interface = "enum Holder<T> { Empty, Full(T) }
            interface Boxed<U> { fn get(self) -> U; }
            fn f(h: Holder<Boxed<String>>) -> i64 {
                return match h {
                    Holder::Full(b) => 1,
                    _ => 0
                };
            }";
        assert!(eligible_checked(generic_interface, "f", &[]));
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

    // m06. (updated by willow-0g8j.2.13) a block-bodied arm may declare a local
    // and then leave through it. The arm is bracketed, so the binding is scoped
    // to it.
    #[test]
    fn m06_block_bodied_arm_with_a_let_eligible() {
        let src = enum_src(
            "fn f(c: Color) -> i64 {
                return match c {
                    Color::Red => { let t = 1; return t; },
                    _ => 2
                };
            }",
        );
        assert!(eligible_checked(&src, "f", &[]));
    }

    // m07. and a block arm that declares a local and then uses it — the same
    // body shape without the departure.
    #[test]
    fn m07_arm_declaring_a_local_eligible() {
        let src = enum_src(
            "fn f(c: Color) {
                match c {
                    Color::Red => { let x = 1; println(x); },
                    _ => println(\"other\")
                }
            }",
        );
        assert!(eligible_checked(&src, "f", &[]));
    }

    // m08. (updated by willow-0g8j.2.4) a downcast pattern tests a runtime type
    // id through an interface box rather than a tag, and the walker emits it:
    // the scrutinee's word 0 IS the concrete object, so the test is an exact
    // `type_id` compare against the arm's class and the binding is that object,
    // unboxed. Exact rather than "is a descendant of" — a descendant of the
    // arm's class does not match it.
    #[test]
    fn m08_class_downcast_pattern_eligible() {
        let src = "interface Speaker { fn speak(self) -> String; }
            class Dog implements Speaker { pub fn speak(self) -> String { return \"woof\"; } }
            fn f(s: Speaker) -> String {
                return match s {
                    Dog(d) => d.speak(),
                    _ => \"other\"
                };
            }";
        assert!(eligible_checked(src, "f", &[]));
    }

    // m08b. the downcast needs the arm class's `type_id` to compare against.
    // With the id table emptied there is nothing to compare and the `match`
    // fails validation rather than inventing a constant.
    #[test]
    fn m08b_class_downcast_needs_a_type_id() {
        let src = "interface Speaker { fn speak(self) -> String; }
            class Dog implements Speaker { pub fn speak(self) -> String { return \"woof\"; } }
            fn f(s: Speaker) -> String {
                return match s {
                    Dog(d) => d.speak(),
                    _ => \"other\"
                };
            }";
        let (f, mut tables) = lir_fn_and_tables(src, "f", &[]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
        tables.class_type_ids.clear();
        assert!(!tables.with_ctx(|ctx| lir_supported_function(&f, ctx)));
    }

    // An unconditional CFG binding copies the scrutinee without comparing it.
    // String content-pattern comparisons are still outside the pattern subset.
    #[test]
    fn m09_unconditional_string_scrutinee_eligible() {
        let src = "fn f(s: String) -> i64 {
                return match s {
                    other => 1
                };
            }";
        assert!(eligible_checked(src, "f", &[]));
    }

    // An unconditional class binding needs no object identity comparison.
    #[test]
    fn m10_unconditional_class_scrutinee_eligible() {
        let src = "class Cell { pub v: i64; }
            fn f(c: Cell) -> i64 {
                return match c {
                    other => other.v
                };
            }";
        assert!(eligible_checked(src, "f", &[]));
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
        let color = Type::Named("Color".to_string().into());
        let shape = Type::Named("Shape".to_string().into());
        assert!(assignable_repr(&color, &color));
        assert!(!assignable_repr(&color, &shape));
        assert!(!assignable_repr(&shape, &color));
        assert!(!assignable_repr(
            &color,
            &Type::Named("Cell".to_string().into())
        ));
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

        // Both `Option<i64>` and a generic interface are supported payloads.
        let nested = "enum Wrapped { Nothing, Boxed(Option<i64>) }
            fn f(w: Wrapped) -> i64 {
                return match w {
                    Wrapped::Nothing => 0,
                    _ => 1
                };
            }";
        assert!(eligible_checked(nested, "f", &[]));

        let generic_interface = "interface Boxed<T> { fn get(self) -> T; }
            enum Wrapped { Nothing, Full(Boxed<String>) }
            fn f(w: Wrapped) -> i64 {
                return match w {
                    Wrapped::Nothing => 0,
                    _ => 1
                };
            }";
        assert!(eligible_checked(generic_interface, "f", &[]));
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
            assert!(ctx.supported_type(&Type::Named("Color".to_string().into())));
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
        use super::super::EnumInfo;
        use crate::semantic::symbols::EnumVariantInfo;
        let variant = |name: &str, payloads: Vec<Type>, tag: i64| EnumVariantInfo {
            name: name.to_string(),
            payload_types: payloads,
            tag,
            declaration_span: crate::diagnostics::Span::dummy(),
        };
        let mut infos = TypeMap::new();
        infos.insert(
            "Color".to_string(),
            EnumInfo {
                name: "Color".into(),
                public: true,
                type_params: vec![],
                declaration_span: crate::diagnostics::Span::dummy(),
                variants: vec![variant("Red", vec![], 0), variant("Green", vec![], 1)],
            },
        );
        infos.insert(
            "Shape".to_string(),
            EnumInfo {
                name: "Shape".into(),
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
        assert!(!is_gc_managed(
            &Type::Named("Color".to_string().into()),
            &infos
        ));
        assert!(is_gc_managed(
            &Type::Named("Shape".to_string().into()),
            &infos
        ));
        // and an undeclared named type is a class, which always is
        assert!(is_gc_managed(
            &Type::Named("Cell".to_string().into()),
            &infos
        ));
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
            &Type::Named("Named".to_string().into()),
            &Type::Named("Marker".to_string().into())
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
    // one-word object required by the enum layout.
    #[test]
    fn p05_void_ok_payload_is_normalised_away() {
        let src = "fn f() -> Result<void, String> { return Ok(); }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // p06. a generic interface remains supported through an Option payload.
    #[test]
    fn p06_generic_interface_payload_is_eligible() {
        let src = "interface Boxed<T> { fn get(self) -> T; }
            fn f(x: Option<Boxed<String>>) -> i64 {
                return match x { Some(b) => 1, None => 0 };
            }
            fn g(x: Option<i64>) -> i64 {
                return match x { Some(v) => v, None => 0 };
            }";
        assert!(eligible_checked(src, "f", &["f", "g"]));
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
        let opt_i64 = Type::Generic("Option".to_string().into(), vec![Type::I64]);
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
        let crate::ir::lowered::LirOperand::Local(result) = v else {
            panic!("materialized map result");
        };
        let forged = Type::Generic("Option".to_string().into(), vec![Type::I64]);
        f.locals[result.0 as usize].ty = forged.clone();
        f.return_type = forged;
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

    // p17. `?` on a `Result<void, E>` is IN (willow-0g8j.2.13). The `Ok()`
    // object is the tag word alone — `enum_instance` normalizes the `void`
    // payload away — so the success arm reads nothing rather than loading a
    // word 1 that was never written. The control is the same shape carrying a
    // real payload, which does load it.
    #[test]
    fn p17_try_propagate_on_a_void_result_eligible() {
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
        assert!(eligible_checked(src, "f", fns));
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

    // p20. Option<void> propagation now has explicit tag-based LIR control flow.
    #[test]
    fn p20_void_option_propagation_is_eligible() {
        let src = "fn unit(n: i64) -> Option<void> { return Some(); }
            fn f(n: i64) -> Option<i64> {
                unit(n)?;
                return Some(1);
            }";
        assert!(eligible_checked(src, "f", &["unit", "f"]));
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

    // p22. an unsupported generic remains rejected, so admitting
    // `Type::Generic` for enums did not open the shape as a whole.
    #[test]
    fn p22_non_enum_generics_still_fall_back() {
        let tables = empty_tables();
        for ty in [
            // A generic CLASS instantiation: the walker admits generic enums
            // and the builtin handles by NAME, never a `Type::Generic` for its
            // shape alone. Not `Future<i64>`: that one IS admitted as of
            // willow-0g8j.3, as an opaque runtime pointer word.
            Type::Generic("Holder".to_string().into(), vec![Type::I64]),
            // Not `Range<i64>`: that one IS admitted as of willow-0g8j.2.10.
            Type::Generic("Range".to_string().into(), vec![Type::String]),
        ] {
            tables.with_ctx(|ctx| {
                assert!(!ctx.supported_type(&ty), "{ty:?} is not an admitted enum");
            });
        }
    }

    // p22b. the opaque runtime-pointer generics ARE admitted, and their output
    // type is vetted like any element type (willow-0g8j.3). `Future<void>` is
    // the only instantiation `sleep`/`yield` can produce; the walker takes a
    // declaration of any of them, since it only ever copies the word.
    #[test]
    fn p22b_future_is_admitted_and_vets_its_output() {
        let tables = empty_tables();
        tables.with_ctx(|ctx| {
            assert!(ctx.supported_type(&Type::Generic(
                "Future".to_string().into(),
                vec![Type::Void]
            )));
            assert!(ctx.supported_type(&Type::Generic(
                "Future".to_string().into(),
                vec![Type::String]
            )));
            assert!(
                !ctx.supported_type(&Type::Generic(
                    "Future".to_string().into(),
                    vec![Type::Generic("Holder".to_string().into(), vec![Type::I64])]
                )),
                "an output type outside the subset keeps the future out"
            );
        });
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
    // Rejection reasons (willow-0g8j.2 groundwork).
    //
    // `lir_rejection_reason` is what the compile error prints, and it is
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
        let src = "import std::collections::Map; \
                   fn f() -> Map<Map<String, i64>, i64> { return Map::new(); }";
        assert_eq!(
            rejected(src, "f", &["f"]),
            "its return type `Map<Map<String, i64>, i64>` is outside the walker's subset"
        );
    }

    // r4. a by-reference parameter itself is no longer a rejection reason.
    #[test]
    fn r4_by_reference_parameter_has_no_rejection_reason() {
        let src = "fn f(x: &mut i64) { x = x + 1; }";
        let (f, tables) = lir_fn_and_tables(src, "f", &["f"]);
        assert!(tables.with_ctx(|ctx| lir_rejection_reason(&f, ctx).is_none()));
    }

    // r5. an unsupported parameter type names the parameter AND the type.
    #[test]
    fn r5_parameter_type_reason_names_both() {
        let src = "import std::collections::Map; \
                   fn f(m: Map<Map<String, i64>, i64>) -> i64 { return 1; }";
        let reason = rejected(src, "f", &["f"]);
        assert!(reason.contains("parameter `m`"), "{reason}");
        assert!(reason.contains("`Map<Map<String, i64>, i64>`"), "{reason}");
    }

    // r6. two bindings of one name are rejected by NAME: LIR's flat scopes are
    // the reason, and the message has to say so or it reads like a bug.
    //
    // (updated by willow-0g8j.2.10) Source can no longer produce this state —
    // the lowering α-renames the second binding, and t01 checks that it does —
    // so the collision is introduced here by UNDOING the rename. The guard is a
    // backstop against a lowering that forgets to rename, and this is what such
    // a lowering would hand the walker.
    #[test]
    fn r6_duplicate_binding_reason_names_the_binding() {
        let src = "fn f() { let mut i = 0; while i < 2 { let x = i; print(x); i = i + 1; } \
                   let mut j = 0; while j < 2 { let x = j; print(x); j = j + 1; } }";
        let (mut f, tables) = lir_fn_and_tables(src, "f", &["f"]);
        let mut undone = false;
        for block in &mut f.blocks {
            for inst in &mut block.instrs {
                if let LirInst::Let { name, .. } = inst
                    && let Some(source) = name.split_once('$').map(|(base, _)| base.to_string())
                {
                    *name = source;
                    undone = true;
                }
            }
        }
        for local in &mut f.locals {
            if let Some((base, _)) = local.name.split_once('$') {
                local.name = base.to_string();
            }
        }
        assert!(undone, "the lowering did not rename the second `let x`");
        let reason = tables
            .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
            .expect("a duplicated binding is rejected");
        assert!(
            reason.starts_with("LIR local `x` reuses an existing lowered name"),
            "{reason}"
        );
    }

    // r7. a `let` whose BINDING type is unsupported blames the binding, not the
    // initialiser (the slot's type is what the walker cannot represent).
    #[test]
    fn r7_let_binding_type_reason() {
        let src = "import std::collections::Map; \
                   fn f() { let m: Map<Map<String, i64>, i64> = Map::new(); print(1); }";
        let reason = rejected(src, "f", &["f"]);
        assert!(
            reason.starts_with("`let m` binds type `Map<Map<String, i64>, i64>`"),
            "{reason}"
        );
    }

    // r8. the blamed node is the SMALLEST unsupported one: an unknown callee
    // nested three levels down is named, not the enclosing `let`.
    #[test]
    fn r8_reason_blames_the_innermost_node() {
        // `g` exists for the type checker but is NOT in the walker's known
        // symbols, which is what makes the call the unsupported node.
        let src = "fn g() -> i64 { println(0); return 2; } fn f() -> i64 { let a = 1 + (2 * g()); return a; }";
        let reason = rejected(src, "f", &["f"]);
        assert!(reason.starts_with("the call to `g` at line"), "{reason}");
    }

    // r9. the line is the offending node's, not the function's or the
    // statement's — the whole point is to be able to jump to it.
    #[test]
    fn r9_reason_reports_the_node_line() {
        let src = "fn g() -> i64 { println(0); return 2; }\n\
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
        // The argument is the unsupported node: the outer call is `i64` and
        // its callee is known, so only the `make()` inside it can be at fault.
        let src = "import std::collections::Map; \
                   fn make() -> Map<Map<String, i64>, i64> { return Map::new(); } \
                   fn take(m: Map<Map<String, i64>, i64>) -> i64 { return 1; } \
                   fn f() -> i64 { return take(make()); }";
        let reason = rejected(src, "f", &["f", "take", "make"]);
        assert!(reason.starts_with("the call to `make` at line"), "{reason}");
        assert!(
            reason.contains("has type `Map<Map<String, i64>, i64>`"),
            "{reason}"
        );
    }

    // r11. a straight-line defer scope is admitted by the LIR walker.
    #[test]
    fn r11_straight_line_defer_is_eligible() {
        let src = "fn f() { defer { print(2); } print(1); }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // r11b. An explicit return lowers the scope exit to `FlushDefers` in a
    // different block. The dataflow eligibility check must carry the open
    // scope to that block and treat the flush as closing it on that path.
    #[test]
    fn r11b_cross_block_defer_with_early_return_is_eligible() {
        let src = "fn f(n: i64) -> i64 { defer print(n); if n > 0 { return 1; } return 0; }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // r11d. `break` leaves a loop body's defer scope with a `FlushDefers` and no
    // matching `LeaveDeferScope`, and the block it jumps to is also reached from
    // the loop header with the scope closed. Both edges agree only once the
    // flush is read as closing the scope on its own path.
    #[test]
    fn r11d_break_out_of_a_defer_scope_is_eligible() {
        let src = "fn f(n: i64) -> i64 {
    let mut t = 0;
    for i in 0..n { defer print(i); if i == 2 { break; } t = t + i; }
    return t;
}";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // r11e. `continue` is the same exit against the loop latch, which the LIR
    // emits BEFORE the body that jumps to it.
    #[test]
    fn r11e_continue_out_of_a_defer_scope_is_eligible() {
        let src = "fn f(n: i64) -> i64 {
    let mut t = 0;
    for i in 0..n { defer print(i); if i == 1 { continue; } t = t + i; }
    return t;
}";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // r11f. `defer` combined with `?` was rejected outright, because the
    // walker's propagating exit had no unwinding. It emits one now.
    #[test]
    fn r11f_defer_combined_with_try_is_eligible() {
        let src = "fn g(n: i64) -> Result<i64, String> { return Ok(n); }
fn f(n: i64) -> Result<i64, String> {
    defer print(n);
    let v = g(n)?;
    return Ok(v);
}";
        assert!(eligible_checked(src, "f", &["f", "g"]));
    }

    // r11g. Three scopes deep with the exit in the innermost: the flush names
    // every open scope at once, and the block that closes the middle one is
    // emitted before the block that closes the inner one.
    #[test]
    fn r11g_three_defer_scopes_with_an_inner_return_are_eligible() {
        let src = "fn f(n: i64) -> i64 {
    defer print(0);
    for i in 0..n {
        defer print(i);
        if i > 0 { defer print(9); if i == 2 { return i; } }
    }
    return -1;
}";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // r11h. A scope opened on only ONE side of a branch leaves the two edges
    // into the join disagreeing about what is open. No source program lowers to
    // this — `defer` is lexical — so it is built by hand, and it is what the
    // agreement check rejects. The blocks
    // are all reachable, so this is the disagreement and not the separate
    // unreachable-block rejection.
    #[test]
    fn r11h_unbalanced_defer_scopes_across_a_join_fall_back() {
        let block = |id: usize, instrs: Vec<LirInst>, terminator: Terminator| LirBlock {
            id: BlockId(id),
            instrs,
            terminator,
            recovery: Vec::new(),
        };
        let enter = LirInst::EnterDeferScope {
            sites: vec![(crate::ir::lowered::LirDeferId(0), Span::dummy())],
            resume: None,
            lock: None,
        };
        let f = LirFunction {
            name: "f".to_string().into(),
            params: Vec::new(),
            return_type: Type::Void,
            is_async: false,
            locals: Vec::new(),
            captures: Vec::new(),
            async_frame: Default::default(),
            blocks: vec![
                block(
                    0,
                    Vec::new(),
                    Terminator::Branch {
                        cond: crate::ir::lowered::LirOperand::Bool(true),
                        then_block: BlockId(1),
                        else_block: BlockId(2),
                    },
                ),
                block(1, vec![enter], Terminator::Jump(BlockId(3))),
                block(2, Vec::new(), Terminator::Jump(BlockId(3))),
                block(3, Vec::new(), Terminator::Return(None)),
            ],
        };
        assert!(!lir_sync_defer_stacks_agree(&f));

        // The same graph with the scope opened before the branch instead — the
        // edges agree, and it is accepted.
        let mut balanced = f;
        balanced.blocks[0].instrs = std::mem::take(&mut balanced.blocks[1].instrs);
        assert!(lir_sync_defer_stacks_agree(&balanced));
    }

    // r11c. A synchronous recover-capable scope needs an explicit LIR resume
    // target even when its normal source path returns before the scope ends.
    // Without it Cranelift sees a branch to an empty backend-only block.
    #[test]
    fn r11c_sync_recovery_records_a_lexical_continuation() {
        let src = r#"
fn f() {
    if true {
        defer match recover() {
            Some(info) => println("return:" + info.message),
            None => {}
        }
        defer { panic("cleanup"); }
        return;
    }
    println("resumed");
}
"#;
        let (f, _tables) = lir_fn_and_tables(src, "f", &["f"]);
        assert!(f.blocks.iter().any(|block| {
            block.instrs.iter().any(|inst| {
                matches!(
                    inst,
                    LirInst::EnterDeferScope {
                        resume: Some(_),
                        ..
                    }
                )
            })
        }));
    }

    // r12. Source-declared static stores are eligible since willow-0g8j.2.6.
    // Removing the registered storage models a broken declaration pass and
    // proves the diagnostic names the exact class and field.
    #[test]
    fn r12_unresolved_static_field_store_reason() {
        let src = "class Counter { pub static mut total: i64 = 0; \
                   pub static fn bump() { Counter::total = Counter::total + 1; } }";
        let (f, mut tables) = lir_fn_and_tables(src, "Counter::bump", &[]);
        tables
            .static_fields
            .remove(&("Counter".to_string(), "total".to_string()));
        let reason = tables
            .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
            .expect("missing static storage must reject the function");
        assert!(reason.contains("Counter::total"), "{reason}");
    }

    // A lowered super constructor still requires its registered class layout.
    // Corrupt backend metadata after valid source lowering to exercise that gate.
    #[test]
    fn r13_super_init_without_registered_base_layout_is_rejected() {
        let source = "open class Base { pub init(self) {} } class Child extends Base { pub init(self) { super.init(); } }";
        let (function, mut tables) = lir_fn_and_tables(source, "Child::init", &[]);
        assert!(tables.with_ctx(|ctx| lir_supported_function(&function, ctx)));
        let retained: Vec<_> = tables
            .class_layouts
            .iter()
            .filter(|(id, _)| **id != TypeId::local("Base"))
            .map(|(id, layout)| (*id, layout.clone()))
            .collect();
        tables.class_layouts.clear();
        for (id, layout) in retained {
            tables.class_layouts.insert(id, layout);
        }
        assert!(
            tables
                .with_ctx(|ctx| lir_rejection_reason(&function, ctx))
                .is_some(),
            "an unregistered constructor layout must reject"
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
        // A `ClassDowncast` binds only when the walker can represent the class
        // it binds. `Sq` stores a map whose KEY is a reference the runtime
        // cannot read (see c15b), so it cannot.
        let src = "import std::collections::Map; \
                   interface Shape { fn area(self) -> i64; } \
                   class Sq implements Shape { \
                       pub b: Map<Map<String, i64>, i64>; \
                       pub fn area(self) -> i64 { return 1; } \
                   } \
                   fn pick(s: Shape) -> i64 { return match s { Sq(q) => 1, _ => 0 }; }";
        let reason = rejected(src, "pick", &["pick"]);
        assert!(
            reason.starts_with("the `match` arm") && reason.contains("pattern outside"),
            "{reason}"
        );
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
                   fn g() -> i64 { println(0); return 1; } \
                   fn f(c: Color) -> i64 { return match c { Color::Red => g(), _ => 0 }; }";
        let reason = rejected(src, "f", &["f"]);
        assert!(reason.starts_with("the call to `g`"), "{reason}");
    }

    // r19. an arm binding is IN scope while its body is searched: blaming the
    // pattern's own variable would be the classic false positive here.
    #[test]
    fn r19_arm_binding_is_in_scope_for_the_body() {
        let src = "enum Shape { Circle(i64) } \
                   fn g() -> i64 { println(0); return 1; } \
                   fn f(s: Shape) -> i64 { return match s { Shape::Circle(r) => r + g() }; }";
        let reason = rejected(src, "f", &["f"]);
        assert!(!reason.contains("the variable `r`"), "{reason}");
        assert!(reason.starts_with("the call to `g`"), "{reason}");
    }

    // r20. with several blockers the FIRST in program order wins, so a
    // whole-corpus histogram of reasons is stable between runs.
    #[test]
    fn r20_first_blocker_in_program_order_wins() {
        let src = "fn g() -> i64 { println(0); return 1; }\n\
                   fn h() -> i64 { println(0); return 2; }\n\
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

    // r22. A declared static read is eligible. Removing its registration proves
    // the rejection still names the class and field, matching store r12.
    #[test]
    fn r22_unresolved_static_property_read_is_named() {
        let src = "class Config { pub static version: i64 = 1; } \
                   fn f() -> i64 { return Config::version; }";
        let (f, mut tables) = lir_fn_and_tables(src, "f", &["f"]);
        tables
            .static_fields
            .remove(&("Config".to_string(), "version".to_string()));
        let reason = tables
            .with_ctx(|ctx| lir_rejection_reason(&f, ctx))
            .expect("missing static storage must reject the read");
        assert!(
            reason.contains("the static property `Config::version`"),
            "{reason}"
        );
    }

    // r23. an operator is named by its source spelling, shared with the IR
    // dumper so one operator is never described two ways.
    #[test]
    fn r23_operator_named_by_source_spelling() {
        let src = "import std::collections::Map; \
                   fn f() -> i64 { let a: Map<Map<String, i64>, i64> = Map::new(); let b = 1 + 2; return b; }";
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
        // blamed first. `Dog` is outside the subset because of the
        // array-keyed map it stores (see c15b) — taking part in inheritance no
        // longer puts a class out (willow-0g8j.2.4).
        let src = "import std::collections::Map; \
                   class Dog { pub m: Map<Map<String, i64>, i64>; \
                       pub init(self) { self.m = Map::new(); } } \
                   fn show(d: Dog) { println(1); } \
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
        // fails validation independently. (`format` used to serve as the
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
    // lambda's SYMBOL comes from the backend's ID-keyed table, not from the
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
    // function-pointer value: reference parameters use a
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
                vec![Type::Named("Missing".to_string().into())],
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
        let crate::ir::lowered::LirOperand::Local(result) = call else {
            panic!("materialized indirect call result");
        };
        f.locals[result.0 as usize].ty = Type::String;
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
        let opt_i64 = Type::Generic("Option".to_string().into(), vec![Type::I64]);
        let to_string = Type::Fn(vec![Type::I64], Box::new(Type::String));
        assert_eq!(
            option_result_method(&opt_i64, "map", &[to_string]),
            Some(Type::Generic(
                "Option".to_string().into(),
                vec![Type::String]
            ))
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
        let opt_i64 = Type::Generic("Option".to_string().into(), vec![Type::I64]);
        let opt_str = Type::Generic("Option".to_string().into(), vec![Type::String]);
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

        let res = Type::Generic("Result".to_string().into(), vec![Type::I64, Type::String]);
        let unresolved_err =
            Type::Generic("Result".to_string().into(), vec![Type::I64, Type::Void]);
        let recover = Type::Fn(vec![Type::String], Box::new(unresolved_err.clone()));
        assert_eq!(
            option_result_method(&res, "or_else", &[recover]),
            Some(unresolved_err)
        );
        // the OK payload still has to match — that arm passes the receiver on
        let mismatched = Type::Fn(
            vec![Type::String],
            Box::new(Type::Generic(
                "Result".to_string().into(),
                vec![Type::String, Type::String],
            )),
        );
        assert_eq!(option_result_method(&res, "or_else", &[mismatched]), None);
    }

    // f21. `map_err` is a `Result`-only combinator, and it rebuilds the ERROR
    // side while passing the ok payload through.
    #[test]
    fn f21_map_err_is_result_only() {
        let res = Type::Generic("Result".to_string().into(), vec![Type::I64, Type::String]);
        let wrap = Type::Fn(vec![Type::String], Box::new(Type::String));
        assert_eq!(
            option_result_method(&res, "map_err", std::slice::from_ref(&wrap)),
            Some(res.clone())
        );
        let opt = Type::Generic("Option".to_string().into(), vec![Type::I64]);
        assert_eq!(option_result_method(&opt, "map_err", &[wrap]), None);
    }

    // f22. a `void` payload has no slot for a combinator to read or write, so
    // `Result<void, E>` is excluded from all of them — the same rule the
    // unwrap family already followed.
    #[test]
    fn f22_void_payloads_are_excluded_from_combinators() {
        let res_void = Type::Generic("Result".to_string().into(), vec![Type::Void, Type::String]);
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
            Type::Named("Widget".to_string().into()),
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
    // fails validation — the reason names the type, not the callee, because the
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
                callee: "panic".to_string().into(),
                args: Vec::new(),
            },
            ty: Type::Never,
            span,
        };
        let tables = empty_tables();
        let local = Type::Fn(Vec::new(), Box::new(Type::Never));
        tables.with_ctx(|ctx| {
            assert!(supported_panic(&call, ctx, &HashMap::new()));
            let names: HashMap<&str, Cow<'_, Type>> =
                HashMap::from([("panic", Cow::Borrowed(&local))]);
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

    // d19. (updated by willow-0g8j.2.13) a `let` may too. The emitter brackets
    // the arm — `vars` and the GC root depth are snapshotted before the body and
    // restored after — so the binding is scoped to the arm rather than leaked
    // into the flat map.
    #[test]
    fn d19_a_let_in_a_diverging_arm_is_admitted() {
        let src = "fn f(n: i64) -> i64 {
                       match n {
                           0 => { let t = 1; return t; }
                           _ => return 2,
                       }
                   }";
        assert!(eligible_checked(src, "f", &["f"]));
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
        let f = p
            .functions
            .iter()
            .find(|f| f.name.to_string() == "f")
            .expect("lowered");
        tables.with_ctx(|ctx| {
            let ctx = &LirTypeCtx {
                return_type: &f.return_type,
                ..*ctx
            };
            assert!(lir_supported_function(f, ctx));
            assert!(
                f.blocks
                    .iter()
                    .flat_map(|block| &block.instrs)
                    .any(|inst| matches!(inst, LirInst::MatchTest { .. }))
            );
            assert!(
                !f.blocks
                    .iter()
                    .flat_map(|block| &block.instrs)
                    .any(|inst| matches!(inst, LirInst::Unsupported { .. }))
            );
            // The expression validator still rejects a divergent match as an
            // operand; lowering only moves its statement control flow to CFG.
            let inst = returned_hir_expr(
                "fn f(n: i64) -> i64 { return match n { 0 => return 1, _ => return 2 }; }",
            );
            let i64_ty = Type::I64;
            let names = HashMap::from([("n", Cow::Borrowed(&i64_ty))]);
            assert_eq!(inst.ty, Type::Never);
            assert!(supported_divergent_expr(&inst, ctx, &names));
            assert!(!supported_expr(&inst, ctx, &names));
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
                   fn unknown() -> i64 { println(0); return 9; }
                   fn f(n: i64) -> i64 {
                       match n {
                           0 => return unknown(),
                           _ => return Config::version,
                       }
                   }";
        let reason = rejected(src, "f", &["f"]);
        assert!(
            reason.contains("the call to `unknown`") && !reason.contains("version"),
            "the reason must name only the unsupported sibling, got: {reason}"
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
            let names: HashMap<&str, Cow<'_, Type>> =
                HashMap::from([("n", Cow::Borrowed(&i64_ty))]);
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
            let names: HashMap<&str, Cow<'_, Type>> =
                HashMap::from([("n", Cow::Borrowed(&i64_ty))]);
            assert!(!supported_expr(&as_value, ctx, &names));
        });
    }

    // d26. `arm_diverges` reads the arm's TAIL statement. A `return` in the
    // middle of an arm is not a tail, and an arm ending in an ordinary value
    // does not leave.
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
                callee: "panic".to_string().into(),
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

    // ── control flow inside an HIR island (willow-0g8j.2.16) ─────────────────

    // d28. the bead's own repro: a guard that ends one path with a `panic`
    // inside an arm body. Only the LIR block graph carries a function-level
    // `if`, so an arm's has to be admitted and emitted on its own.
    #[test]
    fn d28_a_guard_inside_an_arm_body_is_admitted() {
        let src = "enum Shape { Square(i64), Circle(i64) }
                   fn f(s: Shape) -> i64 {
                       match s {
                           Shape::Square(side) => {
                               if side <= 0 { panic(\"bad\"); }
                               return side * side;
                           }
                           Shape::Circle(r) => { return r; }
                       }
                   }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // d29. the same `if`, minus the arm: a function-level guard was always in
    // the subset, so d28 must not be passing because the rule got looser for
    // everything.
    #[test]
    fn d29_a_guard_outside_a_match_was_already_admitted() {
        let src = "fn f(side: i64) -> i64 {
                       if side <= 0 { panic(\"bad\"); }
                       return side * side;
                   }";
        assert!(eligible_checked(src, "f", &["f"]));
    }

    // d30. an `if` in an arm is refused for what it CONTAINS, not for being an
    // `if`, and the reason finder names the offending node inside the branch
    // rather than blaming the guard around it.
    #[test]
    fn d30_an_arm_guard_is_still_checked_through() {
        let src = "class Config { pub static version: i64 = 7; }
                   fn unknown() -> i64 { println(0); return 9; }
                   fn f(n: i64) -> i64 {
                       match n {
                           0 => { if n > 0 { return unknown(); } return 1; }
                           _ => { return Config::version; }
                       }
                   }";
        let reason = rejected(src, "f", &["f"]);
        assert!(
            reason.contains("the call to `unknown`"),
            "the reason must name the unsupported call inside the guard, got: {reason}"
        );
    }

    // d31. an `if` both of whose branches leave ends the arm, so the arm hands
    // nothing to the merge block. `body_diverges` has to see that through the
    // `if`, which neither a `return` tail nor a `!` type would show.
    #[test]
    fn d31_an_if_ends_an_arm_only_when_both_branches_leave() {
        let span = crate::diagnostics::Span::dummy();
        let value = HirExpr {
            kind: HirExprKind::Int(1),
            ty: Type::I64,
            span,
        };
        let cond = HirExpr {
            kind: HirExprKind::Bool(true),
            ty: Type::Bool,
            span,
        };
        let returns = || {
            vec![HirStmt::Return {
                value: Some(value.clone()),
                span,
            }]
        };
        let guard = |else_branch: Option<Vec<HirStmt>>| {
            vec![HirStmt::If {
                cond: cond.clone(),
                then_branch: returns(),
                else_branch,
                span,
            }]
        };
        assert!(body_diverges(&guard(Some(returns()))));
        // no `else`: the falling-through path is still a path
        assert!(!body_diverges(&guard(None)));
        // an `else` that does not leave is one too
        assert!(!body_diverges(&guard(Some(vec![HirStmt::Expr(
            value.clone()
        )]))));
    }

    // d32. a `match` every arm of which leaves is admissible where nothing
    // follows it, and refused as an OPERAND — where something after it would
    // read the value no path can produce. The checker's own type for the
    // `match` is not what decides this: it types the same shape `!` in one
    // position and `void` in another.
    #[test]
    fn d32_an_all_leaving_match_is_admitted_only_where_nothing_follows() {
        let src = "enum Shape { Square(i64), Circle(i64) }
                   fn f(s: Shape) -> i64 {
                       match s {
                           Shape::Square(side) => { return side * side; }
                           Shape::Circle(r) => { panic(\"no circles\"); }
                       }
                   }";
        assert!(eligible_checked(src, "f", &["f"]));

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
        assert!(match_diverges(&arms));
        let as_operand = HirExpr {
            kind: HirExprKind::Match {
                scrutinee: Box::new(HirExpr {
                    kind: HirExprKind::Var("n".to_string()),
                    ty: Type::I64,
                    span,
                }),
                arms,
            },
            ty: Type::I64,
            span,
        };
        let mut tables = empty_tables();
        tables.ret = Type::I64;
        let i64_ty = Type::I64;
        tables.with_ctx(|ctx| {
            let names: HashMap<&str, Cow<'_, Type>> =
                HashMap::from([("n", Cow::Borrowed(&i64_ty))]);
            assert!(!supported_expr(&as_operand, ctx, &names));
            assert!(supported_divergent_expr(&as_operand, ctx, &names));
        });
    }

    // ── preemption safepoints on loop back edges (willow-0g8j.2.11) ───────────
    //
    // A cooperative poll fn parks at a safepoint, and a local the async liveness
    // pass left in an SSA value cannot survive that park. The pass models
    // suspension at loop BACK EDGES and at statements that execute a call, so
    // `lir_back_edges` has to name exactly the loop-closing edges — no more.

    fn back_edges_of(src: &str, name: &str, fns: &[&str]) -> Vec<(usize, usize)> {
        let (f, _tables) = lir_fn_and_tables(src, name, fns);
        let mut edges: Vec<(usize, usize)> = lir_back_edges(&f).into_iter().collect();
        edges.sort_unstable();
        edges
    }

    fn terminator_safepoints(src: &str, name: &str, fns: &[&str]) -> Vec<usize> {
        let (f, _tables) = lir_fn_and_tables(src, name, fns);
        let back = lir_back_edges(&f);
        f.blocks
            .iter()
            .filter(|b| lir_terminator_needs_preempt_safepoint(b, &back))
            .map(|b| b.id.0)
            .collect()
    }

    // s1. straight-line code closes no loop.
    #[test]
    fn s1_straight_line_has_no_back_edge() {
        let src = "fn f(a: i64) -> i64 { let b = a + 1; return b; }";
        assert!(back_edges_of(src, "f", &["f"]).is_empty());
    }

    // s2. the regression itself: lowering numbers an `if`'s JOIN block before
    // the `else` arm, so the else arm's jump to the join runs backwards by id.
    // It closes no loop, so it must not be a back edge.
    #[test]
    fn s2_if_else_join_is_not_a_back_edge() {
        let src = "fn f(c: bool) -> i64 {
                       let mut v = 0;
                       if c { v = 1; } else { v = 2; }
                       return v;
                   }";
        assert!(
            back_edges_of(src, "f", &["f"]).is_empty(),
            "an `if` join closes no loop"
        );
    }

    // s3. ... and no terminator in that function asks for a safepoint either.
    #[test]
    fn s3_if_else_join_asks_for_no_safepoint() {
        let src = "fn f(c: bool) -> i64 {
                       let mut v = 0;
                       if c { v = 1; } else { v = 2; }
                       return v;
                   }";
        assert!(terminator_safepoints(src, "f", &["f"]).is_empty());
    }

    // s4. a `while` loop does close one, and the safepoint goes with it.
    #[test]
    fn s4_while_loop_has_one_back_edge() {
        let src = "fn f(n: i64) -> i64 {
                       let mut i = 0;
                       while i < n { i = i + 1; }
                       return i;
                   }";
        let edges = back_edges_of(src, "f", &["f"]);
        assert_eq!(edges.len(), 1, "{edges:?}");
        let (from, to) = edges[0];
        assert!(to < from, "a back edge runs backwards: {edges:?}");
        assert!(terminator_safepoints(src, "f", &["f"]).contains(&from));
    }

    // s5. a loop nested in a loop closes two, one per level.
    #[test]
    fn s5_nested_loops_close_one_edge_each() {
        let src = "fn f(n: i64) -> i64 {
                       let mut total = 0;
                       let mut i = 0;
                       while i < n {
                           let mut j = 0;
                           while j < n { total = total + 1; j = j + 1; }
                           i = i + 1;
                       }
                       return total;
                   }";
        assert_eq!(back_edges_of(src, "f", &["f"]).len(), 2);
    }

    // s6. a loop whose body branches still closes exactly one edge: the `if`
    // join inside it is a forward merge, not a second loop.
    #[test]
    fn s6_branch_inside_a_loop_adds_no_back_edge() {
        let src = "fn f(n: i64) -> i64 {
                       let mut total = 0;
                       let mut i = 0;
                       while i < n {
                           if i % 2 == 0 { total = total + i; } else { total = total + 1; }
                           i = i + 1;
                       }
                       return total;
                   }";
        assert_eq!(back_edges_of(src, "f", &["f"]).len(), 1);
    }

    // s7. `continue` jumps to the loop head, which is a back edge of its own.
    #[test]
    fn s7_continue_closes_the_loop_too() {
        let src = "fn f(n: i64) -> i64 {
                       let mut i = 0;
                       let mut total = 0;
                       while i < n {
                           i = i + 1;
                           if i % 2 == 0 { continue; }
                           total = total + i;
                       }
                       return total;
                   }";
        let edges = back_edges_of(src, "f", &["f"]);
        assert!(
            edges.len() >= 2,
            "continue adds an edge to the head: {edges:?}"
        );
        for (from, to) in &edges {
            assert!(to < from, "{edges:?}");
        }
    }

    // s8. `break` leaves the loop forwards, so it is never a back edge.
    #[test]
    fn s8_break_is_a_forward_edge() {
        let src = "fn f(n: i64) -> i64 {
                       let mut i = 0;
                       while i < n {
                           if i == 3 { break; }
                           i = i + 1;
                       }
                       return i;
                   }";
        let edges = back_edges_of(src, "f", &["f"]);
        assert_eq!(edges.len(), 1, "only the loop itself closes: {edges:?}");
    }

    // Calls move out of return expressions into flat instructions. Async
    // functions still have the explicit preemption edge inserted before them.
    #[test]
    fn s9_a_call_in_return_still_asks_for_a_safepoint() {
        for asynchronous in [false, true] {
            let source = format!(
                "fn g(x: i64) -> i64 {{ return 1 / x; }} {}fn f() -> i64 {{ return g(1); }}",
                if asynchronous { "async " } else { "" }
            );
            let (program, _) = checked_lowering(&source, &["f", "g"]);
            let function = program
                .functions
                .iter()
                .find(|f| f.name.is_free_named("f"))
                .unwrap();
            assert!(
                function
                    .blocks
                    .iter()
                    .flat_map(|block| &block.instrs)
                    .any(|inst| matches!(
                        inst,
                        LirInst::Compute {
                            value: crate::ir::lowered::LirRvalue::DirectCall { .. },
                            ..
                        }
                    ))
            );
            if asynchronous {
                assert!(function.blocks.iter().any(|block| matches!(
                    block.terminator,
                    Terminator::Suspend {
                        operation: SuspendOp::Preempt,
                        ..
                    }
                )));
            }
        }
    }

    // s10. a plain `return` of a value that calls nothing asks for none.
    #[test]
    fn s10_a_valueless_return_asks_for_no_safepoint() {
        let src = "fn f(a: i64) -> i64 { return a + 1; }";
        assert!(terminator_safepoints(src, "f", &["f"]).is_empty());
    }

    // -- splitting an await out of value position (willow-0g8j.2.11) ---------
    //
    // The walker emits a value-position await BEFORE the rest of its statement,
    // because a Cranelift value computed ahead of the park does not survive the
    // poll return. That reorder is legal only when everything the statement
    // would have evaluated first can be evaluated again afterwards, and when
    // the await is reached unconditionally. `lir_hoistable_around` is the
    // predicate that decides both.

    /// The value returned by `name`, which must be `return <expr>;`.
    fn returned_expr_of(src: &str, name: &str) -> HirExpr {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        assert!(diags.is_empty(), "{diags:?}");
        let function = hir
            .functions
            .iter()
            .find(|f| f.name.to_string() == name)
            .expect("function present");
        match function.body.last().expect("return statement") {
            HirStmt::Return {
                value: Some(value), ..
            } => value.clone(),
            other => panic!("expected value return, got {other:?}"),
        }
    }

    /// Can the single suspension inside `main`'s returned expression be lifted
    /// in front of it? Panics unless there is exactly one.
    fn hoistable_in_main(src: &str) -> bool {
        let value = returned_expr_of(src, "main");
        let mut found = Vec::new();
        lir_collect_suspensions(&value, &mut found);
        assert_eq!(found.len(), 1, "fixture must hold exactly one suspension");
        let mut seen = false;
        lir_hoistable_around(&value, found[0], &mut seen)
    }

    const LEAF: &str = "async fn g() -> i64 { return 1; }\n";

    // v1. only values the emitter can produce twice with the same result are
    // rematerializable, so they can be re-read after a suspension.
    #[test]
    fn v1_literals_and_variables_are_rematerializable() {
        let src = "fn f(a: i64) -> i64 { return a; }
                   fn lit() -> i64 { return 7; }
                   fn call(a: i64) -> i64 { return f(a); }";
        assert!(lir_rematerializable(&returned_expr_of(src, "f")));
        assert!(lir_rematerializable(&returned_expr_of(src, "lit")));
        assert!(!lir_rematerializable(&returned_expr_of(src, "call")));
    }

    // v2. the collector finds the await nested in an operand.
    #[test]
    fn v2_a_nested_await_is_found() {
        let src = format!("{LEAF}async fn main() -> i64 {{ return 1 + await g(); }}");
        let value = returned_expr_of(&src, "main");
        let mut found = Vec::new();
        lir_collect_suspensions(&value, &mut found);
        assert_eq!(found.len(), 1);
        assert!(matches!(found[0].kind, HirExprKind::Await { .. }));
    }

    // v3. a lambda body is a separate function, so its awaits are not this
    // statement's suspensions and must not be counted.
    #[test]
    fn v3_a_lambda_body_is_not_searched() {
        let src = "fn main() -> i64 { let f = |x: i64| -> i64 { return x; }; return f(1); }";
        let value = returned_expr_of(src, "main");
        let mut found = Vec::new();
        lir_collect_suspensions(&value, &mut found);
        assert!(found.is_empty());
    }

    // v4. a variable read before the await can simply be read again.
    #[test]
    fn v4_a_variable_operand_before_the_await_is_hoistable() {
        let src = format!("{LEAF}async fn main(n: i64) -> i64 {{ return n + await g(); }}");
        assert!(hoistable_in_main(&src));
    }

    // v5. a synchronous call before the await may have side effects, so
    // re-running it after the park would change the program.
    #[test]
    fn v5_a_call_before_the_await_is_not_hoistable() {
        let src = format!(
            "{LEAF}fn side() -> i64 {{ return 2; }}
                   async fn main() -> i64 {{ return side() + await g(); }}"
        );
        assert!(!hoistable_in_main(&src));
    }

    // v6. everything AFTER the await already runs on the resume path, so it
    // places no restriction at all.
    #[test]
    fn v6_a_call_after_the_await_is_hoistable() {
        let src = format!(
            "{LEAF}fn side() -> i64 {{ return 2; }}
                   async fn main() -> i64 {{ return await g() + side(); }}"
        );
        assert!(hoistable_in_main(&src));
    }

    // v7. `&&` may never evaluate its right operand, so lifting an await out of
    // it would suspend on a path that does not run.
    #[test]
    fn v7_a_short_circuit_operand_is_not_hoistable() {
        let src = "async fn g() -> bool { return true; }
                   async fn main(c: bool) -> bool { return c && await g(); }";
        assert!(!hoistable_in_main(src));
    }

    // v8. same for a ternary arm - and for its condition, which this rejects
    // conservatively rather than reasoning about position.
    #[test]
    fn v8_a_ternary_operand_is_not_hoistable() {
        let arm = format!("{LEAF}async fn main(c: bool) -> i64 {{ return c ? await g() : 0; }}");
        assert!(!hoistable_in_main(&arm));
        let cond = "async fn g() -> bool { return true; }
                    async fn main() -> i64 { return await g() ? 1 : 0; }";
        assert!(!hoistable_in_main(cond));
    }

    // v9. a `match` picks one arm, so nothing inside it is reached
    // unconditionally.
    #[test]
    fn v9_a_match_operand_is_not_hoistable() {
        let src = format!(
            "{LEAF}async fn main(n: i64) -> i64 {{
                 return match n {{ 0 => await g(), _ => 1, }};
             }}"
        );
        assert!(!hoistable_in_main(&src));
    }

    // v10. the await itself is the boundary: an operand to its RIGHT in the
    // same expression is fine, one to its left is judged on rematerializability
    // alone, so a literal passes where a call does not.
    #[test]
    fn v10_the_boundary_is_the_await_not_the_expression() {
        let ok = format!("{LEAF}async fn main() -> i64 {{ return 1 + await g(); }}");
        assert!(hoistable_in_main(&ok));
        let no = format!(
            "{LEAF}fn side() -> i64 {{ return 2; }}
                   async fn main() -> i64 {{ return 1 + side() + await g(); }}"
        );
        assert!(!hoistable_in_main(&no));
    }
}
