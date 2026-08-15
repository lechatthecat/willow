//! Resolved identity for Willow's builtin methods (willow-uqzx, catalog item 7).
//!
//! A call like `ch.recv()` or `arr.toString()` is not a user method call: it is
//! a compiler intrinsic that lowers to a specific runtime symbol. Deciding
//! *which* intrinsic a call denotes used to be done by comparing the method
//! name and the receiver's type name against string literals, and that decision
//! was re-made independently in at least four places:
//!
//! * the type checker, for validity and the call's result type,
//! * `Codegen::ast_type_of_structural`, for the backend's own type walk,
//! * `FuncGen::emit_method_call`, to pick the lowering,
//! * the LIR walker's eligibility predicate and the cooperative-scheduling
//!   passes, to recognise suspension points.
//!
//! Four string waterfalls answering the same question is four chances to
//! disagree, and they did: the backend's type walk knew `Array::freeze` but not
//! `Array::toString`, typed `Map::toString` as `void`, and did not know
//! `Task::result` at all. Those gaps were masked only because the checker's
//! span→type side-map takes priority over the structural walk for expressions
//! that appear in source.
//!
//! This module is the single answer. [`resolve`] maps a receiver type, a method
//! name and an argument count to an [`Intrinsic`] — a plain enum — plus the
//! result type. Callers match on the enum instead of on strings, so a new
//! builtin is a new variant that every `match` must handle, and a receiver the
//! table does not know resolves to `None` exactly once.
//!
//! Option/Result are deliberately absent: their combinators already resolve
//! through a single shared function (`option_result_method_return_type`) used by
//! both the checker and the backend, which is the shape this module generalises,
//! and their result types depend on a closure argument's type rather than on the
//! receiver alone.
//!
//! # Relationship to the type checker
//!
//! The checker remains the authority on *validity*: it produces the diagnostic
//! for a misspelled method, a wrong argument count, or a non-`Send` channel
//! item. This table is deliberately no wider than what the checker accepts, so
//! anything it returns `None` for is either a user method or already a compile
//! error. What it guarantees is that everything downstream of the checker agrees
//! on what an accepted call *means* and what type it produces.

use crate::parser::ast::Type;
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};

/// A builtin method, resolved to an identity the backend can match on.
///
/// Fieldless on purpose: the variant names an operation, not its
/// representation. Width and element types stay in the receiver `Type`, which
/// every caller already holds, so this enum can be `Copy`/`Eq`/`Hash` and can be
/// enumerated exhaustively by [`Intrinsic::ALL`] for round-trip tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intrinsic {
    /// `i64.toString()` → `willow_i64_to_string`.
    I64ToString,
    /// `f64.toString()` → `willow_f64_to_string`.
    F64ToString,
    /// `bool.toString()` → `willow_bool_to_string`.
    BoolToString,
    /// `String.toString()` — the identity, and the only intrinsic that emits no
    /// code at all.
    StringToString,

    /// `Task<T>::cancel()` / `JoinHandle<T>::cancel()`.
    TaskCancel,
    /// `Task<T>::is_cancelled()` — answered from the frame header, not the
    /// scheduler's task table.
    TaskIsCancelled,
    /// `Task<T>::result()` → `TaskResult<T>`, a cancellation-aware view of the
    /// same frame. Represented by the same pointer, so it lowers to the
    /// identity.
    TaskResult,

    /// `CancellationToken::is_cancelled()`.
    TokenIsCancelled,
    /// `CancellationToken::cancel()`.
    TokenCancel,
    /// `CancellationToken::child()` → a new `CancellationToken`.
    TokenChild,
    /// `CancellationToken::attach(task)` → the same task handle back.
    TokenAttach,

    /// `TaskScope::is_cancelled()`.
    ScopeIsCancelled,
    /// `TaskScope::cancel()`.
    ScopeCancel,
    /// `TaskScope::child()` → a new `TaskScope`.
    ScopeChild,
    /// `TaskScope::add(task)` → the same task handle back.
    ScopeAdd,
    /// `TaskScope::finish()` → `Task<Result<void, Cancelled>>`.
    ScopeFinish,

    /// `AtomicI64::load()` / `AtomicBool::load()`.
    AtomicLoad,
    /// `AtomicI64::store(v)` / `AtomicBool::store(v)`.
    AtomicStore,
    /// `AtomicI64::swap(v)` / `AtomicBool::swap(v)`.
    AtomicSwap,
    /// `AtomicI64::add(v)` — `AtomicBool` has no arithmetic.
    AtomicAdd,
    /// `AtomicI64::sub(v)` — `AtomicBool` has no arithmetic.
    AtomicSub,

    /// `BlockingCell<T>::get()`.
    CellGet,
    /// `BlockingCell<T>::set(v)`.
    CellSet,
    /// `BlockingRwCell<T>::read()`.
    RwCellRead,
    /// `BlockingRwCell<T>::write(v)`.
    RwCellWrite,

    /// `Channel<T>::send(v)` — a suspension point on a full bounded channel.
    ChannelSend,
    /// `Channel<T>::recv()` — a suspension point on an empty channel.
    ChannelRecv,
    /// `Channel<T>::close()`.
    ChannelClose,

    /// `Array<T>::len()`.
    ArrayLen,
    /// `Array<T>::push(v)`.
    ArrayPush,
    /// `Array<T>::pop()`.
    ArrayPop,
    /// `Array<T>::toString()` → `"[1, 2, 3]"`.
    ArrayToString,
    /// `Array<T>::freeze()` → `FrozenArray<T>`.
    ArrayFreeze,

    /// `FrozenArray<T>::len()` — the only method on a frozen array.
    FrozenArrayLen,

    /// `Map<K, V>::insert(k, v)`.
    MapInsert,
    /// `Map<K, V>::get(k)` → `Option<V>`.
    MapGet,
    /// `Map<K, V>::contains(k)`.
    MapContains,
    /// `Map<K, V>::len()`.
    MapLen,
    /// `Map<K, V>::toString()` → `"{k: v, ...}"` sorted by key.
    MapToString,
    /// `Map<K, V>::freeze()` → `FrozenMap<K, V>`.
    MapFreeze,

    /// `FrozenMap<K, V>::get(k)` → `Option<V>`.
    FrozenMapGet,
    /// `FrozenMap<K, V>::contains(k)`.
    FrozenMapContains,
    /// `FrozenMap<K, V>::len()`.
    FrozenMapLen,
}

/// Where an intrinsic's result type comes from.
///
/// Most intrinsics answer from the receiver alone. Two do not: `TaskScope::add`
/// and `CancellationToken::attach` hand back the very `Task<T>` they were given,
/// so their result type is their argument's. Encoding that here keeps the fact
/// in the table instead of leaving each caller to rediscover it — the backend's
/// structural type walk previously did not know these methods existed and typed
/// them as `i64`.
#[derive(Debug, Clone, PartialEq)]
pub enum IntrinsicReturn {
    /// A type determined by the receiver.
    Fixed(Type),
    /// The type of the argument at this index, passed straight through.
    SameAsArg(usize),
}

/// A resolved builtin call: what it is, and what it produces.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedMethod {
    pub intrinsic: Intrinsic,
    pub ret: IntrinsicReturn,
}

impl ResolvedMethod {
    fn fixed(intrinsic: Intrinsic, ret: Type) -> Self {
        Self {
            intrinsic,
            ret: IntrinsicReturn::Fixed(ret),
        }
    }

    fn same_as_arg(intrinsic: Intrinsic, index: usize) -> Self {
        Self {
            intrinsic,
            ret: IntrinsicReturn::SameAsArg(index),
        }
    }

    /// The result type, given a way to type the call's arguments. Callers that
    /// have no argument types available can pass a closure returning `None`; the
    /// two argument-typed intrinsics then fall back to their receiver family's
    /// declared shape (`Task<void>`), which is the same conservative answer the
    /// checker gives for a malformed `add`/`attach`.
    pub fn return_type(&self, arg_type: impl Fn(usize) -> Option<Type>) -> Type {
        match &self.ret {
            IntrinsicReturn::Fixed(ty) => ty.clone(),
            IntrinsicReturn::SameAsArg(index) => arg_type(*index)
                .unwrap_or_else(|| Type::Generic("Task".to_string(), vec![Type::Void])),
        }
    }
}

impl Intrinsic {
    /// Every intrinsic, for exhaustiveness tests. A new variant that is not
    /// added here fails `all_variants_are_listed`.
    pub const ALL: &'static [Intrinsic] = &[
        Intrinsic::I64ToString,
        Intrinsic::F64ToString,
        Intrinsic::BoolToString,
        Intrinsic::StringToString,
        Intrinsic::TaskCancel,
        Intrinsic::TaskIsCancelled,
        Intrinsic::TaskResult,
        Intrinsic::TokenIsCancelled,
        Intrinsic::TokenCancel,
        Intrinsic::TokenChild,
        Intrinsic::TokenAttach,
        Intrinsic::ScopeIsCancelled,
        Intrinsic::ScopeCancel,
        Intrinsic::ScopeChild,
        Intrinsic::ScopeAdd,
        Intrinsic::ScopeFinish,
        Intrinsic::AtomicLoad,
        Intrinsic::AtomicStore,
        Intrinsic::AtomicSwap,
        Intrinsic::AtomicAdd,
        Intrinsic::AtomicSub,
        Intrinsic::CellGet,
        Intrinsic::CellSet,
        Intrinsic::RwCellRead,
        Intrinsic::RwCellWrite,
        Intrinsic::ChannelSend,
        Intrinsic::ChannelRecv,
        Intrinsic::ChannelClose,
        Intrinsic::ArrayLen,
        Intrinsic::ArrayPush,
        Intrinsic::ArrayPop,
        Intrinsic::ArrayToString,
        Intrinsic::ArrayFreeze,
        Intrinsic::FrozenArrayLen,
        Intrinsic::MapInsert,
        Intrinsic::MapGet,
        Intrinsic::MapContains,
        Intrinsic::MapLen,
        Intrinsic::MapToString,
        Intrinsic::MapFreeze,
        Intrinsic::FrozenMapGet,
        Intrinsic::FrozenMapContains,
        Intrinsic::FrozenMapLen,
    ];

    /// Whether lowering this intrinsic can suspend the calling task.
    ///
    /// A synchronous `send` on a full bounded channel and a `recv` on an empty
    /// one both park the task and hand the worker back to the scheduler, so the
    /// cooperative passes must treat the call as a suspension point and
    /// frame-back everything live across it. Nothing else in the table can
    /// suspend: the atomics and the blocking cells complete in place, and
    /// `TaskScope::finish` returns a task rather than waiting for one.
    pub fn is_suspension_point(self) -> bool {
        matches!(self, Intrinsic::ChannelSend | Intrinsic::ChannelRecv)
    }
}

/// The name a receiver type resolves under, for the families this table keys on
/// by name. Returns `None` for receivers no builtin family claims.
/// Resolve a builtin method call.
///
/// `recv` is the receiver's type, `method` the source-level method name, and
/// `arity` the number of arguments written at the call site. Arity is part of
/// the key so that a call the checker would reject never resolves to an
/// intrinsic whose lowering assumes arguments that are not there.
///
/// Returns `None` when the call is not a builtin — a user class method, an
/// interface method, an Option/Result combinator, or an error the checker has
/// already reported.
pub fn resolve(recv: &Type, method: &str, arity: usize) -> Option<ResolvedMethod> {
    // Scalar `toString` (willow-fvfc). Checked first because it is the only
    // intrinsic on a primitive receiver, and because `String` is a receiver no
    // other family claims.
    if method == "toString" && arity == 0 {
        match recv {
            Type::I64 => return Some(ResolvedMethod::fixed(Intrinsic::I64ToString, Type::String)),
            Type::F64 => return Some(ResolvedMethod::fixed(Intrinsic::F64ToString, Type::String)),
            Type::Bool => {
                return Some(ResolvedMethod::fixed(Intrinsic::BoolToString, Type::String));
            }
            Type::String => {
                return Some(ResolvedMethod::fixed(
                    Intrinsic::StringToString,
                    Type::String,
                ));
            }
            _ => {}
        }
    }

    // `Array<T>` has its own `Type` constructor rather than a generic name.
    if let Type::Array(elem) = recv {
        return match (method, arity) {
            ("len", 0) => Some(ResolvedMethod::fixed(Intrinsic::ArrayLen, Type::I64)),
            ("push", 1) => Some(ResolvedMethod::fixed(Intrinsic::ArrayPush, Type::Void)),
            ("pop", 0) => Some(ResolvedMethod::fixed(Intrinsic::ArrayPop, (**elem).clone())),
            ("toString", 0) => Some(ResolvedMethod::fixed(
                Intrinsic::ArrayToString,
                Type::String,
            )),
            ("freeze", 0) => Some(ResolvedMethod::fixed(
                Intrinsic::ArrayFreeze,
                Type::Generic("FrozenArray".to_string(), vec![(**elem).clone()]),
            )),
            _ => None,
        };
    }

    // Nominal, non-generic receivers: the atomics and the two cancellation
    // handles. A user class named `AtomicI64` cannot reach here because these
    // names are owned by the prelude.
    if let Some(builtin) = builtin_types::resolve(recv)
        && builtin.args.is_empty()
    {
        return match (builtin.id, method, arity) {
            (B::AtomicI64, "load", 0) => {
                Some(ResolvedMethod::fixed(Intrinsic::AtomicLoad, Type::I64))
            }
            (B::AtomicI64, "store", 1) => {
                Some(ResolvedMethod::fixed(Intrinsic::AtomicStore, Type::Void))
            }
            (B::AtomicI64, "swap", 1) => {
                Some(ResolvedMethod::fixed(Intrinsic::AtomicSwap, Type::I64))
            }
            (B::AtomicI64, "add", 1) => {
                Some(ResolvedMethod::fixed(Intrinsic::AtomicAdd, Type::I64))
            }
            (B::AtomicI64, "sub", 1) => {
                Some(ResolvedMethod::fixed(Intrinsic::AtomicSub, Type::I64))
            }
            (B::AtomicBool, "load", 0) => {
                Some(ResolvedMethod::fixed(Intrinsic::AtomicLoad, Type::Bool))
            }
            (B::AtomicBool, "store", 1) => {
                Some(ResolvedMethod::fixed(Intrinsic::AtomicStore, Type::Void))
            }
            (B::AtomicBool, "swap", 1) => {
                Some(ResolvedMethod::fixed(Intrinsic::AtomicSwap, Type::Bool))
            }

            (B::CancellationToken, "is_cancelled", 0) => Some(ResolvedMethod::fixed(
                Intrinsic::TokenIsCancelled,
                Type::Bool,
            )),
            (B::CancellationToken, "cancel", 0) => {
                Some(ResolvedMethod::fixed(Intrinsic::TokenCancel, Type::Void))
            }
            (B::CancellationToken, "child", 0) => Some(ResolvedMethod::fixed(
                Intrinsic::TokenChild,
                B::CancellationToken.apply(vec![]),
            )),
            (B::CancellationToken, "attach", 1) => {
                Some(ResolvedMethod::same_as_arg(Intrinsic::TokenAttach, 0))
            }

            (B::TaskScope, "is_cancelled", 0) => Some(ResolvedMethod::fixed(
                Intrinsic::ScopeIsCancelled,
                Type::Bool,
            )),
            (B::TaskScope, "cancel", 0) => {
                Some(ResolvedMethod::fixed(Intrinsic::ScopeCancel, Type::Void))
            }
            (B::TaskScope, "child", 0) => Some(ResolvedMethod::fixed(
                Intrinsic::ScopeChild,
                B::TaskScope.apply(vec![]),
            )),
            (B::TaskScope, "add", 1) => Some(ResolvedMethod::same_as_arg(Intrinsic::ScopeAdd, 0)),
            (B::TaskScope, "finish", 0) => Some(ResolvedMethod::fixed(
                Intrinsic::ScopeFinish,
                B::Task.apply(vec![
                    B::Result.apply(vec![Type::Void, B::Cancelled.apply(vec![])]),
                ]),
            )),
            _ => None,
        };
    }

    let family = builtin_types::resolve(recv)?;
    let args = family.args;
    match (family.id, args.len()) {
        // `Task<T>` and `JoinHandle<T>` share a representation: the async frame.
        (B::Task | B::JoinHandle, 1) => match (method, arity) {
            ("cancel", 0) => Some(ResolvedMethod::fixed(Intrinsic::TaskCancel, Type::Void)),
            ("is_cancelled", 0) => Some(ResolvedMethod::fixed(
                Intrinsic::TaskIsCancelled,
                Type::Bool,
            )),
            ("result", 0) => Some(ResolvedMethod::fixed(
                Intrinsic::TaskResult,
                B::TaskResult.apply(vec![args[0].clone()]),
            )),
            _ => None,
        },
        (B::BlockingCell, 1) => match (method, arity) {
            ("get", 0) => Some(ResolvedMethod::fixed(Intrinsic::CellGet, args[0].clone())),
            ("set", 1) => Some(ResolvedMethod::fixed(Intrinsic::CellSet, Type::Void)),
            _ => None,
        },
        (B::BlockingRwCell, 1) => match (method, arity) {
            ("read", 0) => Some(ResolvedMethod::fixed(
                Intrinsic::RwCellRead,
                args[0].clone(),
            )),
            ("write", 1) => Some(ResolvedMethod::fixed(Intrinsic::RwCellWrite, Type::Void)),
            _ => None,
        },
        (B::Channel, 1) => match (method, arity) {
            ("send", 1) => Some(ResolvedMethod::fixed(Intrinsic::ChannelSend, Type::Void)),
            ("recv", 0) => Some(ResolvedMethod::fixed(
                Intrinsic::ChannelRecv,
                args[0].clone(),
            )),
            ("close", 0) => Some(ResolvedMethod::fixed(Intrinsic::ChannelClose, Type::Void)),
            _ => None,
        },
        (B::FrozenArray, 1) => match (method, arity) {
            ("len", 0) => Some(ResolvedMethod::fixed(Intrinsic::FrozenArrayLen, Type::I64)),
            _ => None,
        },
        (B::Map, 2) => match (method, arity) {
            ("insert", 2) => Some(ResolvedMethod::fixed(Intrinsic::MapInsert, Type::Void)),
            ("get", 1) => Some(ResolvedMethod::fixed(
                Intrinsic::MapGet,
                B::Option.apply(vec![args[1].clone()]),
            )),
            ("contains", 1) => Some(ResolvedMethod::fixed(Intrinsic::MapContains, Type::Bool)),
            ("len", 0) => Some(ResolvedMethod::fixed(Intrinsic::MapLen, Type::I64)),
            ("toString", 0) => Some(ResolvedMethod::fixed(Intrinsic::MapToString, Type::String)),
            ("freeze", 0) => Some(ResolvedMethod::fixed(
                Intrinsic::MapFreeze,
                B::FrozenMap.apply(args.to_vec()),
            )),
            _ => None,
        },
        // A frozen map is the same runtime object, so reads lower identically —
        // but `insert`/`freeze` are absent rather than aliased, because the
        // checker rejects them and an intrinsic the checker rejects must not
        // have a lowering waiting for it.
        (B::FrozenMap, 2) => match (method, arity) {
            ("get", 1) => Some(ResolvedMethod::fixed(
                Intrinsic::FrozenMapGet,
                B::Option.apply(vec![args[1].clone()]),
            )),
            ("contains", 1) => Some(ResolvedMethod::fixed(
                Intrinsic::FrozenMapContains,
                Type::Bool,
            )),
            ("len", 0) => Some(ResolvedMethod::fixed(Intrinsic::FrozenMapLen, Type::I64)),
            _ => None,
        },
        _ => None,
    }
}

/// Convenience wrapper for callers that only want the intrinsic identity.
pub fn resolve_intrinsic(recv: &Type, method: &str, arity: usize) -> Option<Intrinsic> {
    resolve(recv, method, arity).map(|r| r.intrinsic)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn array(elem: Type) -> Type {
        Type::Array(Box::new(elem))
    }

    fn generic(name: &str, args: Vec<Type>) -> Type {
        Type::Generic(name.to_string(), args)
    }

    fn named(name: &str) -> Type {
        Type::Named(name.to_string())
    }

    fn ret_of(recv: &Type, method: &str, arity: usize) -> Option<Type> {
        resolve(recv, method, arity).map(|r| r.return_type(|_| None))
    }

    /// Perspective 1: every scalar `toString` resolves to its own intrinsic, and
    /// each produces a String. These four are the whole reason the backend had a
    /// `m.method == "toString"` test ahead of everything else.
    #[test]
    fn scalar_to_string_resolves_per_receiver() {
        assert_eq!(
            resolve_intrinsic(&Type::I64, "toString", 0),
            Some(Intrinsic::I64ToString)
        );
        assert_eq!(
            resolve_intrinsic(&Type::F64, "toString", 0),
            Some(Intrinsic::F64ToString)
        );
        assert_eq!(
            resolve_intrinsic(&Type::Bool, "toString", 0),
            Some(Intrinsic::BoolToString)
        );
        assert_eq!(
            resolve_intrinsic(&Type::String, "toString", 0),
            Some(Intrinsic::StringToString)
        );
        for recv in [Type::I64, Type::F64, Type::Bool, Type::String] {
            assert_eq!(ret_of(&recv, "toString", 0), Some(Type::String));
        }
    }

    /// Perspective 2: `toString` with an argument is not the intrinsic. The
    /// backend used to guard this with `m.args.is_empty()`; arity is now part of
    /// the key, so the guard cannot be forgotten at one call site and kept at
    /// another.
    #[test]
    fn scalar_to_string_with_arguments_does_not_resolve() {
        assert_eq!(resolve(&Type::I64, "toString", 1), None);
        assert_eq!(resolve(&Type::String, "toString", 2), None);
    }

    /// Perspective 3: `Array<T>::toString` is an intrinsic that the backend's
    /// structural type walk did not know before this table existed — it fell
    /// through to the `i64` default. Its result is String regardless of element
    /// type; whether the element is renderable is the checker's business.
    #[test]
    fn array_to_string_resolves_and_returns_string() {
        assert_eq!(
            resolve_intrinsic(&array(Type::I64), "toString", 0),
            Some(Intrinsic::ArrayToString)
        );
        assert_eq!(ret_of(&array(Type::I64), "toString", 0), Some(Type::String));
        assert_eq!(
            ret_of(&array(named("Point")), "toString", 0),
            Some(Type::String)
        );
    }

    /// Perspective 4: `Array<T>::pop` produces the element type, so the result
    /// type has to be read out of the receiver rather than fixed in the table.
    #[test]
    fn array_pop_returns_the_element_type() {
        assert_eq!(ret_of(&array(Type::String), "pop", 0), Some(Type::String));
        assert_eq!(
            ret_of(&array(named("Point")), "pop", 0),
            Some(named("Point"))
        );
    }

    /// Perspective 5: `Array<T>::freeze` produces `FrozenArray<T>` — the one
    /// intrinsic whose result is a *different* container of the same element.
    #[test]
    fn array_freeze_returns_a_frozen_array_of_the_same_element() {
        assert_eq!(
            ret_of(&array(Type::I64), "freeze", 0),
            Some(generic("FrozenArray", vec![Type::I64]))
        );
    }

    /// Perspective 6: array arity. `push` needs exactly one argument and `len`
    /// exactly none; the mismatched forms are the checker's errors and must not
    /// reach a lowering.
    #[test]
    fn array_methods_are_keyed_by_arity() {
        assert_eq!(resolve(&array(Type::I64), "push", 0), None);
        assert_eq!(resolve(&array(Type::I64), "push", 2), None);
        assert_eq!(resolve(&array(Type::I64), "len", 1), None);
        assert_eq!(resolve(&array(Type::I64), "pop", 1), None);
    }

    /// Perspective 7: a `FrozenArray<T>` answers `len` only. `push` and `pop`
    /// resolve to nothing, so no mutation lowering exists for a frozen array
    /// even if a front-end check were weakened.
    #[test]
    fn frozen_array_exposes_only_len() {
        let fa = generic("FrozenArray", vec![Type::I64]);
        assert_eq!(
            resolve_intrinsic(&fa, "len", 0),
            Some(Intrinsic::FrozenArrayLen)
        );
        assert_eq!(resolve(&fa, "push", 1), None);
        assert_eq!(resolve(&fa, "pop", 0), None);
        assert_eq!(resolve(&fa, "freeze", 0), None);
        assert_eq!(resolve(&fa, "toString", 0), None);
    }

    /// Perspective 8: `Map<K, V>::get` produces `Option<V>`, picking the *second*
    /// type argument. Swapping the two would type `Map<String, i64>::get` as
    /// `Option<String>` and silently coerce a boxed integer.
    #[test]
    fn map_get_returns_option_of_the_value_type() {
        let map = generic("Map", vec![Type::String, Type::I64]);
        assert_eq!(resolve_intrinsic(&map, "get", 1), Some(Intrinsic::MapGet));
        assert_eq!(
            ret_of(&map, "get", 1),
            Some(generic("Option", vec![Type::I64]))
        );
    }

    /// Perspective 9: `Map::toString` produces a String. The backend's
    /// structural walk used to answer `void` here, because its `Map` arm ended
    /// in a catch-all that swallowed every unlisted method.
    #[test]
    fn map_to_string_returns_string_not_void() {
        let map = generic("Map", vec![Type::String, Type::I64]);
        assert_eq!(ret_of(&map, "toString", 0), Some(Type::String));
    }

    /// Perspective 10: `Map::freeze` preserves both type arguments in order.
    #[test]
    fn map_freeze_preserves_key_and_value_types() {
        let map = generic("Map", vec![Type::String, Type::Bool]);
        assert_eq!(
            ret_of(&map, "freeze", 0),
            Some(generic("FrozenMap", vec![Type::String, Type::Bool]))
        );
    }

    /// Perspective 11: a `FrozenMap` is read-only. It shares a runtime
    /// representation with `Map`, and the old backend routed both to the same
    /// lowering function — which would have emitted `willow_map_insert` on a
    /// frozen map had a front-end check ever let one through.
    #[test]
    fn frozen_map_rejects_mutation_and_refreezing() {
        let fm = generic("FrozenMap", vec![Type::String, Type::I64]);
        assert_eq!(
            resolve_intrinsic(&fm, "get", 1),
            Some(Intrinsic::FrozenMapGet)
        );
        assert_eq!(
            resolve_intrinsic(&fm, "contains", 1),
            Some(Intrinsic::FrozenMapContains)
        );
        assert_eq!(
            resolve_intrinsic(&fm, "len", 0),
            Some(Intrinsic::FrozenMapLen)
        );
        assert_eq!(resolve(&fm, "insert", 2), None);
        assert_eq!(resolve(&fm, "remove", 1), None);
        assert_eq!(resolve(&fm, "freeze", 0), None);
        assert_eq!(resolve(&fm, "toString", 0), None);
    }

    /// Perspective 12: `Map` and `FrozenMap` resolve to *distinct* intrinsics
    /// even for the reads they share, so a lowering can never accidentally
    /// depend on the two being the same value.
    #[test]
    fn map_and_frozen_map_reads_are_distinct_intrinsics() {
        let map = generic("Map", vec![Type::String, Type::I64]);
        let fm = generic("FrozenMap", vec![Type::String, Type::I64]);
        assert_ne!(
            resolve_intrinsic(&map, "get", 1),
            resolve_intrinsic(&fm, "get", 1)
        );
        assert_ne!(
            resolve_intrinsic(&map, "len", 0),
            resolve_intrinsic(&fm, "len", 0)
        );
    }

    /// Perspective 13: the collection families are keyed by type-argument count
    /// as well as name. `Map<K>` with one argument is not a map.
    #[test]
    fn collection_families_require_the_right_arity_of_type_arguments() {
        assert_eq!(resolve(&generic("Map", vec![Type::I64]), "len", 0), None);
        assert_eq!(
            resolve(
                &generic("Map", vec![Type::I64, Type::I64, Type::I64]),
                "len",
                0
            ),
            None
        );
        assert_eq!(resolve(&generic("FrozenArray", vec![]), "len", 0), None);
        assert_eq!(
            resolve(&generic("Channel", vec![Type::I64, Type::I64]), "recv", 0),
            None
        );
    }

    /// Perspective 14: `Channel<T>::recv` produces the element type and `send`
    /// produces void. Getting this backwards would make `let v = ch.recv();`
    /// bind a void.
    #[test]
    fn channel_send_and_recv_return_types() {
        let ch = generic("Channel", vec![Type::String]);
        assert_eq!(ret_of(&ch, "recv", 0), Some(Type::String));
        assert_eq!(ret_of(&ch, "send", 1), Some(Type::Void));
        assert_eq!(ret_of(&ch, "close", 0), Some(Type::Void));
    }

    /// Perspective 15: only the two channel operations that can park the task
    /// are suspension points. The cooperative passes key off this instead of
    /// re-testing the method name.
    #[test]
    fn only_channel_send_and_recv_suspend() {
        let suspending: Vec<Intrinsic> = Intrinsic::ALL
            .iter()
            .copied()
            .filter(|i| i.is_suspension_point())
            .collect();
        assert_eq!(
            suspending,
            vec![Intrinsic::ChannelSend, Intrinsic::ChannelRecv]
        );
    }

    /// Perspective 16: `AtomicI64` and `AtomicBool` share intrinsic identities
    /// but not result types — `load` gives `i64` on one and `bool` on the other.
    #[test]
    fn atomic_load_and_swap_follow_the_receiver_width() {
        assert_eq!(ret_of(&named("AtomicI64"), "load", 0), Some(Type::I64));
        assert_eq!(ret_of(&named("AtomicBool"), "load", 0), Some(Type::Bool));
        assert_eq!(ret_of(&named("AtomicI64"), "swap", 1), Some(Type::I64));
        assert_eq!(ret_of(&named("AtomicBool"), "swap", 1), Some(Type::Bool));
        assert_eq!(ret_of(&named("AtomicI64"), "store", 1), Some(Type::Void));
        assert_eq!(ret_of(&named("AtomicBool"), "store", 1), Some(Type::Void));
    }

    /// Perspective 17: `AtomicBool` has no arithmetic. The lowering builds the
    /// runtime symbol name by interpolating the method, so resolving
    /// `AtomicBool::add` would have produced a call to the non-existent
    /// `willow_atomic_bool_add`.
    #[test]
    fn atomic_bool_has_no_arithmetic() {
        assert_eq!(
            resolve_intrinsic(&named("AtomicI64"), "add", 1),
            Some(Intrinsic::AtomicAdd)
        );
        assert_eq!(
            resolve_intrinsic(&named("AtomicI64"), "sub", 1),
            Some(Intrinsic::AtomicSub)
        );
        assert_eq!(resolve(&named("AtomicBool"), "add", 1), None);
        assert_eq!(resolve(&named("AtomicBool"), "sub", 1), None);
    }

    /// Perspective 18: the blocking cells use different method names for the
    /// same shape, and neither answers the other's.
    #[test]
    fn blocking_cells_do_not_share_method_names() {
        let cell = generic("BlockingCell", vec![Type::I64]);
        let rw = generic("BlockingRwCell", vec![Type::I64]);
        assert_eq!(resolve_intrinsic(&cell, "get", 0), Some(Intrinsic::CellGet));
        assert_eq!(resolve_intrinsic(&cell, "set", 1), Some(Intrinsic::CellSet));
        assert_eq!(resolve(&cell, "read", 0), None);
        assert_eq!(resolve(&cell, "write", 1), None);
        assert_eq!(
            resolve_intrinsic(&rw, "read", 0),
            Some(Intrinsic::RwCellRead)
        );
        assert_eq!(
            resolve_intrinsic(&rw, "write", 1),
            Some(Intrinsic::RwCellWrite)
        );
        assert_eq!(resolve(&rw, "get", 0), None);
        assert_eq!(resolve(&rw, "set", 1), None);
    }

    /// Perspective 19: `Mutex<T>` and `RwLock<T>` have no accessor methods at
    /// all — the scheduler-aware locks go through `lock ... as ...`. Resolving
    /// one here would bypass that.
    #[test]
    fn scheduler_locks_expose_no_intrinsic_accessors() {
        for name in ["Mutex", "RwLock"] {
            let lock = generic(name, vec![Type::I64]);
            for method in ["get", "set", "read", "write", "load", "store"] {
                assert_eq!(resolve(&lock, method, 0), None, "{name}::{method}");
                assert_eq!(resolve(&lock, method, 1), None, "{name}::{method}");
            }
        }
    }

    /// Perspective 20: `Task<T>` and `JoinHandle<T>` are the same frame, so they
    /// resolve identically. `result()` yields `TaskResult<T>` — a type the
    /// backend's structural walk did not know at all and typed as `i64`.
    #[test]
    fn task_and_join_handle_resolve_identically() {
        for name in ["Task", "JoinHandle"] {
            let t = generic(name, vec![Type::I64]);
            assert_eq!(
                resolve_intrinsic(&t, "cancel", 0),
                Some(Intrinsic::TaskCancel)
            );
            assert_eq!(
                resolve_intrinsic(&t, "is_cancelled", 0),
                Some(Intrinsic::TaskIsCancelled)
            );
            assert_eq!(
                ret_of(&t, "result", 0),
                Some(generic("TaskResult", vec![Type::I64]))
            );
            assert_eq!(ret_of(&t, "cancel", 0), Some(Type::Void));
            assert_eq!(ret_of(&t, "is_cancelled", 0), Some(Type::Bool));
        }
    }

    /// Perspective 21: `Future<T>` is awaitable but is not a cancellable handle,
    /// so it must not pick up `Task`'s intrinsics.
    #[test]
    fn future_is_not_a_cancellable_task_handle() {
        let f = generic("Future", vec![Type::I64]);
        assert_eq!(resolve(&f, "cancel", 0), None);
        assert_eq!(resolve(&f, "is_cancelled", 0), None);
        assert_eq!(resolve(&f, "result", 0), None);
    }

    /// Perspective 22: `TaskResult<T>` is the *output* of `result()`, not
    /// another receiver — calling `result()` on it again resolves to nothing
    /// rather than nesting.
    #[test]
    fn task_result_is_not_itself_a_task_handle() {
        let tr = generic("TaskResult", vec![Type::I64]);
        assert_eq!(resolve(&tr, "result", 0), None);
        assert_eq!(resolve(&tr, "cancel", 0), None);
    }

    /// Perspective 23: the two cancellation handles overlap on three method
    /// names and differ on two. `finish` belongs to the scope and `attach` to
    /// the token; crossing them is a checker error today and resolves to nothing
    /// here.
    #[test]
    fn token_and_scope_share_names_but_not_intrinsics() {
        let token = named("CancellationToken");
        let scope = named("TaskScope");
        assert_eq!(
            resolve_intrinsic(&token, "cancel", 0),
            Some(Intrinsic::TokenCancel)
        );
        assert_eq!(
            resolve_intrinsic(&scope, "cancel", 0),
            Some(Intrinsic::ScopeCancel)
        );
        assert_ne!(
            resolve_intrinsic(&token, "is_cancelled", 0),
            resolve_intrinsic(&scope, "is_cancelled", 0)
        );
        assert_eq!(
            resolve_intrinsic(&scope, "finish", 0),
            Some(Intrinsic::ScopeFinish)
        );
        assert_eq!(resolve(&token, "finish", 0), None);
        assert_eq!(
            resolve_intrinsic(&token, "attach", 1),
            Some(Intrinsic::TokenAttach)
        );
        assert_eq!(resolve(&scope, "attach", 1), None);
        assert_eq!(
            resolve_intrinsic(&scope, "add", 1),
            Some(Intrinsic::ScopeAdd)
        );
        assert_eq!(resolve(&token, "add", 1), None);
    }

    /// Perspective 24: `child()` returns the receiver's own handle type, so a
    /// token's child is a token and a scope's child is a scope.
    #[test]
    fn child_returns_the_same_handle_kind() {
        assert_eq!(
            ret_of(&named("CancellationToken"), "child", 0),
            Some(named("CancellationToken"))
        );
        assert_eq!(
            ret_of(&named("TaskScope"), "child", 0),
            Some(named("TaskScope"))
        );
    }

    /// Perspective 25: `TaskScope::finish` returns the fully spelled
    /// `Task<Result<void, Cancelled>>`. This is the checker's answer, restated
    /// once so the backend can stop guessing `i64`.
    #[test]
    fn scope_finish_returns_a_cancellable_unit_task() {
        assert_eq!(
            ret_of(&named("TaskScope"), "finish", 0),
            Some(generic(
                "Task",
                vec![generic("Result", vec![Type::Void, named("Cancelled")])]
            ))
        );
    }

    /// Perspective 26: `attach`/`add` hand back their argument, so their result
    /// type is not knowable from the receiver. The table says so explicitly
    /// instead of returning a plausible-looking wrong type.
    #[test]
    fn attach_and_add_return_their_argument() {
        let attach = resolve(&named("CancellationToken"), "attach", 1).unwrap();
        let add = resolve(&named("TaskScope"), "add", 1).unwrap();
        assert_eq!(attach.ret, IntrinsicReturn::SameAsArg(0));
        assert_eq!(add.ret, IntrinsicReturn::SameAsArg(0));

        let task = generic("Task", vec![Type::String]);
        let seen = attach.return_type(|i| (i == 0).then(|| task.clone()));
        assert_eq!(seen, task);
        assert_eq!(add.return_type(|i| (i == 0).then(|| task.clone())), task);
    }

    /// Perspective 27: with no argument type available, the argument-typed
    /// intrinsics fall back to `Task<void>` rather than to a scalar. A caller
    /// that cannot type its arguments still gets a task-shaped answer, so a
    /// downstream `await` sees a handle instead of an integer.
    #[test]
    fn attach_and_add_fall_back_to_a_task_shape() {
        let add = resolve(&named("TaskScope"), "add", 1).unwrap();
        assert_eq!(add.return_type(|_| None), generic("Task", vec![Type::Void]));
    }

    /// Perspective 28: an ordinary user class receiver resolves to nothing, even
    /// when it spells a builtin method name. Class dispatch must stay reachable.
    #[test]
    fn user_class_receivers_never_resolve() {
        let user = named("Point");
        for method in [
            "len", "push", "pop", "get", "set", "send", "recv", "close", "cancel", "toString",
            "freeze", "child", "finish",
        ] {
            for arity in 0..3 {
                assert_eq!(resolve(&user, method, arity), None, "Point::{method}");
            }
        }
    }

    /// Perspective 29: a user *generic* class receiver resolves to nothing too,
    /// as long as it is not one of the reserved family names.
    #[test]
    fn user_generic_receivers_never_resolve() {
        let user = generic("Stack", vec![Type::I64]);
        for method in ["len", "push", "pop", "recv", "toString"] {
            for arity in 0..3 {
                assert_eq!(resolve(&user, method, arity), None, "Stack::{method}");
            }
        }
    }

    /// Perspective 30: unknown method names on a known family resolve to
    /// nothing, so a misspelling reaches the checker's diagnostic rather than a
    /// lowering for a neighbouring method.
    #[test]
    fn unknown_methods_on_known_families_do_not_resolve() {
        assert_eq!(resolve(&array(Type::I64), "lenght", 0), None);
        assert_eq!(
            resolve(&generic("Map", vec![Type::String, Type::I64]), "put", 2),
            None
        );
        assert_eq!(
            resolve(&generic("Channel", vec![Type::I64]), "receive", 0),
            None
        );
        assert_eq!(resolve(&named("AtomicI64"), "fetch_add", 1), None);
    }

    /// Perspective 31: `void` and `!` receivers resolve to nothing. Both appear
    /// in compiler-synthesized expressions, and neither has methods.
    #[test]
    fn uninhabited_and_unit_receivers_do_not_resolve() {
        for recv in [Type::Void, Type::Never] {
            for method in ["toString", "len", "recv"] {
                assert_eq!(resolve(&recv, method, 0), None);
            }
        }
    }

    /// Perspective 32: an empty method name resolves to nothing rather than
    /// panicking or matching a prefix.
    #[test]
    fn empty_method_name_does_not_resolve() {
        assert_eq!(resolve(&array(Type::I64), "", 0), None);
        assert_eq!(resolve(&Type::I64, "", 0), None);
    }

    /// Perspective 33: nested receivers resolve on their outermost family, so
    /// `Array<Array<i64>>::pop` produces the inner array rather than `i64`.
    #[test]
    fn nested_receivers_resolve_on_the_outer_family() {
        let nested = array(array(Type::I64));
        assert_eq!(ret_of(&nested, "pop", 0), Some(array(Type::I64)));
        assert_eq!(ret_of(&nested, "len", 0), Some(Type::I64));

        let map_of_arrays = generic("Map", vec![Type::String, array(Type::I64)]);
        assert_eq!(
            ret_of(&map_of_arrays, "get", 1),
            Some(generic("Option", vec![array(Type::I64)]))
        );
    }

    /// Perspective 34: every variant in [`Intrinsic::ALL`] is actually
    /// reachable from `resolve`. A variant nobody can produce is either dead or
    /// a lowering waiting for a resolution that never happens.
    #[test]
    fn every_intrinsic_is_reachable_from_resolve() {
        let receivers: Vec<Type> = vec![
            Type::I64,
            Type::F64,
            Type::Bool,
            Type::String,
            array(Type::I64),
            generic("FrozenArray", vec![Type::I64]),
            generic("Map", vec![Type::String, Type::I64]),
            generic("FrozenMap", vec![Type::String, Type::I64]),
            generic("Channel", vec![Type::I64]),
            generic("BlockingCell", vec![Type::I64]),
            generic("BlockingRwCell", vec![Type::I64]),
            generic("Task", vec![Type::I64]),
            generic("JoinHandle", vec![Type::I64]),
            named("AtomicI64"),
            named("AtomicBool"),
            named("CancellationToken"),
            named("TaskScope"),
        ];
        let methods = [
            "toString",
            "len",
            "push",
            "pop",
            "freeze",
            "insert",
            "get",
            "set",
            "contains",
            "read",
            "write",
            "send",
            "recv",
            "close",
            "load",
            "store",
            "swap",
            "add",
            "sub",
            "cancel",
            "is_cancelled",
            "result",
            "child",
            "attach",
            "finish",
        ];
        let mut seen = std::collections::HashSet::new();
        for recv in &receivers {
            for method in methods {
                for arity in 0..3 {
                    if let Some(r) = resolve(recv, method, arity) {
                        seen.insert(r.intrinsic);
                    }
                }
            }
        }
        let missing: Vec<Intrinsic> = Intrinsic::ALL
            .iter()
            .copied()
            .filter(|i| !seen.contains(i))
            .collect();
        assert!(missing.is_empty(), "unreachable intrinsics: {missing:?}");
    }

    /// Perspective 35: [`Intrinsic::ALL`] really is every variant. Counting is
    /// the only check Rust cannot do for us here, so it is spelled out — a new
    /// variant left out of `ALL` would silently escape the exhaustiveness tests
    /// that iterate it.
    #[test]
    fn all_variants_are_listed() {
        let unique: std::collections::HashSet<Intrinsic> = Intrinsic::ALL.iter().copied().collect();
        assert_eq!(unique.len(), Intrinsic::ALL.len(), "duplicate in ALL");
        assert_eq!(
            Intrinsic::ALL.len(),
            43,
            "Intrinsic::ALL must list every variant; update the count when adding one"
        );
    }

    /// Perspective 36: resolution is deterministic and free of ordering effects
    /// — the same key always gives the same answer, and no receiver resolves two
    /// different intrinsics for one method/arity pair.
    #[test]
    fn resolution_is_deterministic() {
        let map = generic("Map", vec![Type::String, Type::I64]);
        for _ in 0..4 {
            assert_eq!(resolve_intrinsic(&map, "get", 1), Some(Intrinsic::MapGet));
        }
        assert_eq!(resolve(&map, "get", 1), resolve(&map, "get", 1));
    }
}

#[cfg(test)]
mod checker_agreement_tests {
    //! Perspectives 37-52: the resolver and the type checker must answer the
    //! same question the same way.
    //!
    //! The unit tests above pin what [`resolve`] returns in isolation. They
    //! cannot catch the failure this table exists to prevent: [`resolve`]
    //! staying self-consistent while drifting away from the type checker. That
    //! is exactly what happened to the four hand-written string waterfalls the
    //! table replaced — each was internally tidy and none agreed with the
    //! others.
    //!
    //! So these tests type-check real source and then assert, for every builtin
    //! method call in it, that the resolver's result type equals the type the
    //! checker recorded in `expr_types`: the type the rest of the compiler
    //! actually believes. Adding a method to one side and not the other fails
    //! here rather than at some later miscompile.

    use super::*;
    use crate::parser::ast::{Block, Expr, Item, MethodCallExpr, Stmt};

    /// Every builtin call the snippets reach. Only the expression and statement
    /// forms the snippets below actually use are descended into; a snippet that
    /// grows a new form collects fewer calls than its test asserts, which fails
    /// rather than passing vacuously.
    fn walk_expr<'a>(expr: &'a Expr, out: &mut Vec<&'a MethodCallExpr>) {
        match expr {
            Expr::MethodCall(m) => {
                walk_expr(&m.object, out);
                for arg in &m.args {
                    walk_expr(&arg.expr, out);
                }
                out.push(m);
            }
            Expr::Await(a) => walk_expr(&a.expr, out),
            Expr::Print(inner, _, _) => walk_expr(inner, out),
            Expr::Binary(b) => {
                walk_expr(&b.lhs, out);
                walk_expr(&b.rhs, out);
            }
            Expr::Unary(u) => walk_expr(&u.expr, out),
            Expr::Call(c) => {
                for arg in &c.args {
                    walk_expr(&arg.expr, out);
                }
            }
            Expr::StaticCall(c) => {
                for arg in &c.args {
                    walk_expr(&arg.expr, out);
                }
            }
            Expr::Index(base, index, _) => {
                walk_expr(base, out);
                walk_expr(index, out);
            }
            Expr::ArrayLiteral(items, _) => {
                for item in items {
                    walk_expr(item, out);
                }
            }
            _ => {}
        }
    }

    fn walk_block<'a>(block: &'a Block, out: &mut Vec<&'a MethodCallExpr>) {
        for stmt in &block.stmts {
            match stmt {
                Stmt::Let(s) => walk_expr(&s.init, out),
                Stmt::Expr(s) => walk_expr(&s.expr, out),
                Stmt::Return(s) => {
                    if let Some(value) = &s.value {
                        walk_expr(value, out);
                    }
                }
                _ => {}
            }
        }
    }

    /// Type-check `src` with the prelude registered, then check every builtin
    /// method call in it against [`resolve`]. Returns the intrinsics reached, so
    /// the coverage test at the end can prove no family lost its snippet.
    ///
    /// `expected` is the number of calls the snippet is meant to contain: an
    /// assertion that passes because the walker found nothing is worse than one
    /// that fails.
    fn assert_agrees(src: &str, expected: usize) -> Vec<Intrinsic> {
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
        let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
        assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).expect("prelude registers");
        checker.check_program(&program);
        assert!(
            checker.errors.is_empty(),
            "snippet must type-check cleanly, got {:?}",
            checker.errors
        );

        let mut calls = Vec::new();
        for item in &program.items {
            if let Item::Function(f) = item {
                walk_block(&f.body, &mut calls);
            }
        }

        let mut reached = Vec::new();
        let mut builtin_calls = 0;
        for call in &calls {
            let recv = checker
                .expr_types
                .get(&call.object.span())
                .unwrap_or_else(|| {
                    panic!("checker recorded no receiver type for `{}`", call.method)
                })
                .clone();
            let Some(resolved) = resolve(&recv, &call.method, call.args.len()) else {
                // Not a builtin: a user method, which class dispatch owns.
                continue;
            };
            builtin_calls += 1;
            let checker_ret = checker
                .expr_types
                .get(&call.span)
                .unwrap_or_else(|| panic!("checker recorded no result type for `{}`", call.method))
                .clone();
            let resolved_ret = resolved.return_type(|i| {
                call.args
                    .get(i)
                    .and_then(|arg| checker.expr_types.get(&arg.expr.span()))
                    .cloned()
            });
            assert_eq!(
                resolved_ret,
                checker_ret,
                "`{}::{}` — resolver says {resolved_ret:?}, type checker says {checker_ret:?}",
                type_name_for_message(&recv),
                call.method
            );
            reached.push(resolved.intrinsic);
        }
        assert_eq!(
            builtin_calls, expected,
            "snippet should contain {expected} builtin calls, found {builtin_calls}"
        );
        reached
    }

    fn type_name_for_message(ty: &Type) -> String {
        format!("{ty:?}")
    }

    const SCALARS: &str = r#"
fn f() {
    let n = 42;
    let r = 1.5;
    let b = true;
    let s = "willow";
    let a = n.toString();
    let c = r.toString();
    let d = b.toString();
    let e = s.toString();
}
"#;

    const ARRAYS: &str = r#"
import std::collections::Array;

fn f() {
    let xs: Array<i64> = [1, 2, 3];
    xs.push(4);
    let last = xs.pop();
    let size = xs.len();
    let text = xs.toString();
    let frozen = xs.freeze();
    let frozen_size = frozen.len();
}
"#;

    const MAPS: &str = r#"
import std::collections::Map;

fn f() {
    let m: Map<String, i64> = Map::new();
    m.insert("ann", 87);
    let hit = m.get("ann");
    let has = m.contains("ann");
    let size = m.len();
    let text = m.toString();
    let frozen = m.freeze();
    let frozen_hit = frozen.get("ann");
    let frozen_has = frozen.contains("ann");
    let frozen_size = frozen.len();
}
"#;

    const ATOMICS: &str = r#"
fn f() {
    let counter = AtomicI64::new(0);
    counter.store(5);
    let before_add = counter.add(3);
    let before_sub = counter.sub(2);
    let before_swap = counter.swap(100);
    let now = counter.load();
    let flag = AtomicBool::new(false);
    flag.store(true);
    let was = flag.swap(false);
    let set = flag.load();
}
"#;

    const CELLS: &str = r#"
fn f() {
    let mode = BlockingRwCell::new("dev");
    mode.write("prod");
    let current = mode.read();
    let ready = BlockingCell::new(false);
    ready.set(true);
    let done = ready.get();
}
"#;

    const CHANNELS: &str = r#"
async fn f() {
    let ch = Channel<i64>::new();
    ch.send(7);
    let value = ch.recv();
    ch.close();
}
"#;

    const TASKS: &str = r#"
async fn work() -> i64 {
    await sleep(1);
    return 10;
}

async fn f() {
    let running = work();
    let stopped = running.is_cancelled();
    let outcome = await running.result();
    let other = work();
    other.cancel();
}
"#;

    const TOKENS: &str = r#"
async fn work() -> i64 {
    await sleep(1);
    return 10;
}

async fn f() {
    let token = CancellationToken::new();
    let idle = token.is_cancelled();
    let nested = token.child();
    let attached = nested.attach(work());
    token.cancel();
}
"#;

    const SCOPES: &str = r#"
async fn work() -> i64 {
    await sleep(1);
    return 10;
}

async fn f() {
    let scope = TaskScope::new();
    let idle = scope.is_cancelled();
    let nested = scope.child();
    let owned = scope.add(work());
    let done = await scope.finish();
    scope.cancel();
}
"#;

    /// Perspective 37: `toString` on i64, f64, bool and String. The checker and
    /// the resolver must agree that all four produce a String even though each
    /// lowers to a different runtime symbol.
    #[test]
    fn p37_scalar_to_string_agrees() {
        assert_agrees(SCALARS, 4);
    }

    /// Perspective 38: the whole `Array<T>` surface, plus the `FrozenArray<T>`
    /// that `freeze` produces. `pop` returns the element type and `freeze`
    /// returns a differently-named family, so both are places a hand-written
    /// waterfall would get wrong.
    #[test]
    fn p38_array_and_frozen_array_agree() {
        assert_agrees(ARRAYS, 6);
    }

    /// Perspective 39: the `Map<K,V>` surface plus the `FrozenMap<K,V>` reads.
    /// `get` returns `Option<V>` — the value type, not the key type — which the
    /// backend's structural walk had no answer for at all before this table.
    #[test]
    fn p39_map_and_frozen_map_agree() {
        assert_agrees(MAPS, 9);
    }

    /// Perspective 40: `AtomicI64` and `AtomicBool` share method names but not
    /// result types: `swap` gives an i64 on one and a bool on the other.
    #[test]
    fn p40_atomics_agree() {
        assert_agrees(ATOMICS, 8);
    }

    /// Perspective 41: `BlockingCell<T>` and `BlockingRwCell<T>` return their
    /// element type from a read and `void` from a write.
    #[test]
    fn p41_cells_agree() {
        assert_agrees(CELLS, 4);
    }

    /// Perspective 42: `Channel<T>`. `recv` yields the element type and is a
    /// suspension point; the other two are void.
    #[test]
    fn p42_channels_agree() {
        assert_agrees(CHANNELS, 3);
    }

    /// Perspective 43: `Task<T>`. `result` yields `TaskResult<T>`, which the
    /// structural walk previously defaulted to `i64` — an `await` on it would
    /// have been typed as an integer.
    #[test]
    fn p43_tasks_agree() {
        assert_agrees(TASKS, 3);
    }

    /// Perspective 44: `CancellationToken`, including `attach`, whose result
    /// type follows its argument rather than its receiver.
    #[test]
    fn p44_tokens_agree() {
        assert_agrees(TOKENS, 4);
    }

    /// Perspective 45: `TaskScope`, including `add` (argument-typed) and
    /// `finish` (a fixed `Task<Result<void, Cancelled>>`).
    #[test]
    fn p45_scopes_agree() {
        assert_agrees(SCOPES, 5);
    }

    /// Perspective 46: a user class method that spells a builtin name is not
    /// stolen by the table — it resolves to nothing and stays with class
    /// dispatch, and the snippet therefore contains zero builtin calls.
    #[test]
    fn p46_user_methods_are_not_intrinsics() {
        assert_agrees(
            r#"
class Bag {
    pub count: i64;
    pub fn len(self) -> i64 { return self.count; }
    pub fn toString(self) -> String { return "bag"; }
}

fn f() {
    let b = new Bag(3);
    let n = b.len();
    let s = b.toString();
}
"#,
            0,
        );
    }

    /// Perspective 47: chained calls. The receiver of the second call is the
    /// *result* of the first, so agreement on `freeze` is what makes agreement
    /// on the following `len` possible at all.
    #[test]
    fn p47_chained_receivers_agree() {
        assert_agrees(
            r#"
import std::collections::Array;

fn f() {
    let xs: Array<i64> = [1, 2, 3];
    let size = xs.freeze().len();
}
"#,
            2,
        );
    }

    /// Perspective 48: a builtin call nested inside another expression is typed
    /// the same way as one standing alone. The backend hoists suspension points
    /// out of larger expressions, so nesting must not change the answer.
    #[test]
    fn p48_nested_positions_agree() {
        assert_agrees(
            r#"
import std::collections::Array;

fn f() {
    let xs: Array<i64> = [1, 2, 3];
    let total = xs.len() + xs.freeze().len();
    println(xs.toString());
}
"#,
            4,
        );
    }

    /// Perspective 49: element types travel. A `Map<String, String>` and a
    /// `Map<String, i64>` differ only in their value type, and `get` must follow
    /// it on both.
    #[test]
    fn p49_element_types_travel() {
        assert_agrees(
            r#"
import std::collections::Map;

fn f() {
    let words: Map<String, String> = Map::new();
    words.insert("a", "b");
    let word = words.get("a");
    let counts: Map<String, i64> = Map::new();
    counts.insert("a", 1);
    let count = counts.get("a");
}
"#,
            4,
        );
    }

    /// Perspective 50: `Array<String>` — `pop` gives a String, not the `i64`
    /// that a family-only answer would produce.
    #[test]
    fn p50_string_arrays_agree() {
        assert_agrees(
            r#"
import std::collections::Array;

fn f() {
    let names: Array<String> = ["ann", "ben"];
    let last = names.pop();
    let text = names.toString();
}
"#,
            2,
        );
    }

    /// Perspective 51: the snippets in this module reach every intrinsic. A
    /// family that loses its snippet stops being cross-checked, which is the
    /// quiet way this protection would rot.
    #[test]
    fn p51_every_intrinsic_is_cross_checked() {
        let mut reached: std::collections::HashSet<Intrinsic> = std::collections::HashSet::new();
        for (src, expected) in [
            (SCALARS, 4),
            (ARRAYS, 6),
            (MAPS, 9),
            (ATOMICS, 8),
            (CELLS, 4),
            (CHANNELS, 3),
            (TASKS, 3),
            (TOKENS, 4),
            (SCOPES, 5),
        ] {
            reached.extend(assert_agrees(src, expected));
        }
        let missing: Vec<Intrinsic> = Intrinsic::ALL
            .iter()
            .copied()
            .filter(|i| !reached.contains(i))
            .collect();
        assert!(
            missing.is_empty(),
            "intrinsics with no checker cross-check: {missing:?}"
        );
    }

    /// Perspective 52: the two suspension-point intrinsics are exactly the two
    /// the checker treats as async-only. Getting this set wrong would either
    /// drop a frame-narrowing hoist or add a spurious one.
    #[test]
    fn p52_suspension_points_are_the_channel_pair() {
        let suspending: Vec<Intrinsic> = Intrinsic::ALL
            .iter()
            .copied()
            .filter(|i| i.is_suspension_point())
            .collect();
        assert_eq!(
            suspending,
            vec![Intrinsic::ChannelSend, Intrinsic::ChannelRecv]
        );
    }
}
