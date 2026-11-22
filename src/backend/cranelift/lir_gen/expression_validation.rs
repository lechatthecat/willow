//! Production LIR eligibility and builtin representation helpers.
use willow_abi::runtime_symbols::names as rt;

use super::*;

/// Whether the walker can both TEST `pattern` against a `scrutinee_ty` value
/// and bind whatever it destructures. On success the pattern's
/// bindings are added to `names`, which is why this takes the map by `&mut`:
/// the arm body is checked against the extended scope, exactly as the emitter
/// will run it. On failure `names` may have been partly extended, so callers
/// pass a throwaway clone rather than their own map.
///
/// `ClassDowncast` tests the boxed object's runtime `type_id`, so it is
/// admitted only for a class the walker has a `type_id` and a layout for
///.
pub(super) fn supported_lir_pattern<'n>(
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
        // actually load.
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
        crate::ir::lowered::LirPattern::LiteralInt(n) => {
            if *scrutinee_ty == Type::Char {
                u32::try_from(*n).ok().and_then(char::from_u32).is_some()
            } else {
                scrutinee_ty
                    .integer_layout()
                    .is_some_and(|layout| layout.contains(layout.value(*n)))
            }
        }
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
            // by exactly that. The emitter zips bindings
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
            // word 0 does not read them.
            let (Type::Named(iface) | Type::Generic(iface, _)) = scrutinee_ty else {
                return false;
            };
            if !(ctx.is_interface)(iface)
                || !matches!(binding_ty, Type::Named(n) if n == class_name)
                || !ctx.supported_class(&class_name.to_string())
                || ctx.classes.class_type_id(class_name).is_none()
            {
                return false;
            }
            names.insert(binding.as_str(), Cow::Borrowed(binding_ty));
            true
        }
    }
}

/// One of the two atomic cells, and the width every operand
/// and result of its methods has.
///
/// `AtomicI64` and `AtomicBool` share one runtime shape — a GC-allocated word
/// the runtime reads and writes atomically — and differ only in that width and
/// in the arithmetic `AtomicBool` does not have. Carrying the distinction as a
/// value keeps the emitter from re-deriving it from the class spelling.
#[derive(Clone, Copy)]
pub(super) struct AtomicCell {
    pub(super) is_i64: bool,
}

impl AtomicCell {
    /// The type of the cell's word: the type of every operand it takes and of
    /// every non-`void` result it produces.
    pub(super) fn word(self) -> Type {
        if self.is_i64 { Type::I64 } else { Type::Bool }
    }

    /// The constructor export for this cell.
    pub(super) fn constructor(self) -> &'static str {
        if self.is_i64 {
            rt::willow_atomic_i64_new
        } else {
            rt::willow_atomic_bool_new
        }
    }

    /// The builtin identity this cell is constructed through.
    pub(super) fn type_id(self) -> B {
        if self.is_i64 {
            B::AtomicI64
        } else {
            B::AtomicBool
        }
    }
}

/// Whether `ty` is an atomic cell, resolved through [`builtin_types`] rather
/// than by name: those two names are owned by the prelude, so a user class
/// cannot take them, and going through the resolver is what makes that true
/// here as well as in `intrinsics::resolve`.
pub(super) fn atomic_cell(ty: &Type) -> Option<AtomicCell> {
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

/// One of the two cancellation handles: a `CancellationToken`
/// or a `TaskScope`.
///
/// The two expose the same four operations over different runtime objects, so
/// the constructor export depends on the handle kind. A scope
/// additionally has `finish`.
#[derive(Clone, Copy)]
pub(super) struct CancelHandle {
    is_token: bool,
}

impl CancelHandle {
    /// The constructor export for this handle.
    pub(super) fn constructor(self) -> &'static str {
        if self.is_token {
            rt::willow_cancellation_token_new
        } else {
            rt::willow_task_scope_new
        }
    }

    /// The builtin identity this handle is constructed through.
    pub(super) fn type_id(self) -> B {
        if self.is_token {
            B::CancellationToken
        } else {
            B::TaskScope
        }
    }
}

/// Whether `ty` is a cancellation handle, resolved through [`builtin_types`]
/// rather than by name, for the same reason [`atomic_cell`] is: the prelude owns
/// both names, and going through the resolver is what makes that true here as
/// well as in `intrinsics::resolve`.
pub(super) fn cancellation_handle(ty: &Type) -> Option<CancelHandle> {
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

/// One of the two native-blocking cells: a `BlockingCell<T>`
/// or a `BlockingRwCell<T>`.
///
/// Unlike `Mutex<T>` and `RwLock<T>`, these two are single-operation cells with
/// no critical section: each accessor is one runtime call that blocks the OS
/// thread on contention rather than parking the task, which is why they are
/// callable from a synchronous function. What differs between them is the
/// constructor export and the method names, and the method names come from
/// the resolver rather than from here.
#[derive(Clone, Copy)]
pub(super) struct BlockingCellKind {
    is_rw: bool,
}

impl BlockingCellKind {
    /// The constructor export for this handle.
    pub(super) fn constructor(self) -> &'static str {
        if self.is_rw {
            rt::willow_blocking_rw_cell_new
        } else {
            rt::willow_blocking_cell_new
        }
    }

    /// The builtin identity this cell is constructed through.
    pub(super) fn type_id(self) -> B {
        if self.is_rw {
            B::BlockingRwCell
        } else {
            B::BlockingCell
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
pub(super) fn blocking_cell(ty: &Type) -> Option<(BlockingCellKind, &Type)> {
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
///.
///
/// The resolved head keeps the existing terminal-name matching policy.
///
/// The `&'static str` is the constructor export for the resolved lock kind.
pub(super) fn scheduler_lock(ty: &Type) -> Option<(&'static str, &Type)> {
    let Type::Generic(name, args) = ty else {
        return None;
    };
    let [protected] = args.as_slice() else {
        return None;
    };
    match B::from_name(name.name().as_ref()) {
        Some(B::Mutex) => Some((rt::willow_async_mutex_new, protected)),
        Some(B::RwLock) => Some((rt::willow_async_rwlock_new, protected)),
        _ => None,
    }
}

/// The type produced by an enum method. For map operations, the arguments
/// of the constructed generic come from the receiver and the callback.
#[derive(Debug, PartialEq)]
pub(super) enum EnumMethodResult<'a> {
    Value(&'a Type),
    Option(&'a Type),
    Result(&'a Type, &'a Type),
}

impl EnumMethodResult<'_> {
    pub(super) fn matches(self, actual: &Type, ctx: &LirTypeCtx<'_>) -> bool {
        match self {
            Self::Value(ty) => ctx.same_repr(ty, actual),
            Self::Option(t) => ctx.same_generic_repr(
                &TypeId::from_source_name(B::Option.source_name()),
                [t].into_iter(),
                actual,
                &mut HashSet::new(),
            ),
            Self::Result(t, e) => ctx.same_generic_repr(
                &TypeId::from_source_name(B::Result.source_name()),
                [t, e].into_iter(),
                actual,
                &mut HashSet::new(),
            ),
        }
    }
}

/// The result type of a builtin `Option`/`Result` method the walker emits, or
/// `None` when the receiver/method pair is outside the subset.
///
/// Payload reads and callable combinators require a stored (non-void)
/// payload. Predicates inspect only the tag. The result borrows the types
/// used by the emitter so validation can compare representations directly.
pub(super) fn option_result_method<'a>(
    recv: &'a Type,
    method: AdtMethod,
    args: &'a [Type],
) -> Option<EnumMethodResult<'a>> {
    use EnumMethodResult::Value;
    let resolved = builtin_types::resolve(recv)?;
    let non_void = |ty: &&Type| !matches!(ty, Type::Void);
    let payload = |i: usize| resolved.args.get(i).filter(non_void);
    let no_args = |t: &'a Type| args.is_empty().then_some(Value(t));
    let one_string = |t: &'a Type| matches!(args, [Type::String]).then_some(Value(t));
    let one_of = |t: &'a Type| matches!(args, [a] if assignable_repr(t, a)).then_some(Value(t));
    let one_fn = |params: &[Type]| match args {
        [Type::Fn(ps, ret)] if ps.as_slice() == params => Some(ret.as_ref()),
        _ => None,
    };
    let unary_fn = |i| one_fn(std::slice::from_ref(payload(i)?));
    static BOOL: Type = Type::Bool;
    match resolved.id {
        B::Option => match method {
            AdtMethod::IsSome | AdtMethod::IsNone => no_args(&BOOL),
            AdtMethod::Unwrap => no_args(payload(0)?),
            AdtMethod::Expect => one_string(payload(0)?),
            AdtMethod::UnwrapOr => one_of(payload(0)?),
            AdtMethod::Map => Some(EnumMethodResult::Option(unary_fn(0).filter(non_void)?)),
            AdtMethod::AndThen => {
                let produced = unary_fn(0)?;
                builtin_types::unary_arg(produced, B::Option).filter(non_void)?;
                Some(Value(produced))
            }
            AdtMethod::OrElse => {
                let produced = one_fn(&[])?;
                (builtin_types::unary_arg(produced, B::Option) == Some(payload(0)?))
                    .then_some(Value(produced))
            }
            _ => None,
        },
        B::Result => match method {
            AdtMethod::IsOk | AdtMethod::IsErr => no_args(&BOOL),
            AdtMethod::Unwrap => no_args(payload(0)?),
            AdtMethod::UnwrapErr => no_args(payload(1)?),
            AdtMethod::Expect => one_string(payload(0)?),
            AdtMethod::UnwrapOr => one_of(payload(0)?),
            AdtMethod::Map => Some(EnumMethodResult::Result(
                unary_fn(0).filter(non_void)?,
                payload(1)?,
            )),
            AdtMethod::MapErr => Some(EnumMethodResult::Result(
                payload(0)?,
                unary_fn(1).filter(non_void)?,
            )),
            // An inferred Result::Ok may leave E as Void. The pass-through
            // error uses the same boxed representation on either side.
            AdtMethod::AndThen => {
                let produced = unary_fn(0)?;
                let (ok, _) = builtin_types::binary_args(produced, B::Result)?;
                (!matches!(ok, Type::Void)).then_some(Value(produced))
            }
            AdtMethod::OrElse => {
                let produced = unary_fn(1)?;
                let (ok, _) = builtin_types::binary_args(produced, B::Result)?;
                (ok == payload(0)?).then_some(Value(produced))
            }
            _ => None,
        },
        _ => None,
    }
}
