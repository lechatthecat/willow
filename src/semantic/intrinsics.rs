//! Resolved identity for Willow's builtin methods (shared compiler infrastructure, catalog item 7).
//!
//! The checker resolves a receiver method's spelling to a signature once and
//! saves its intrinsic identity by expression ID. Checked bodies carry that fact
//! into analysis and HIR. Declared methods with the same spelling have no entry.
//! Lowering without checker facts uses the same signature table.
//!
//! The checker owns argument validation, recovery and diagnostics. Executable
//! consumers use arity-checked identities. Recovery passes can also inspect the
//! candidate for an invalid call. Option and Result methods use typed IDs and retain their
//! argument-dependent contracts.

use crate::parser::ast::Type;
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::ids::{FunctionId, TypeId};
use willow_abi::RuntimeEffects;
use willow_abi::runtime_symbols::names as rt;

/// A builtin method, resolved to an identity the backend can match on.
///
/// Fieldless on purpose: the variant names an operation, not its
/// representation. Width and element types stay in the receiver `Type`, which
/// every caller already holds, so this enum can be `Copy`/`Eq`/`Hash` and can be
/// enumerated exhaustively by [`Intrinsic::ALL`] for round-trip tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Intrinsic {
    /// `i64.to_string()` → `willow_i64_to_string`.
    I64ToString,
    IntegerConvert,
    IntegerWrappingConvert,
    IntegerCheckedConvert,
    ScalarParse,
    ScalarToString,
    /// `f64.to_string()` → `willow_f64_to_string`.
    F64ToString,
    /// `bool.to_string()` → `willow_bool_to_string`.
    BoolToString,
    /// `i64.wrapping_add(i64)`: two's-complement `+` in every build mode. Inline, like the
    /// other explicit overflow operations.
    I64WrappingAdd,
    /// `i64.wrapping_sub(i64)`.
    I64WrappingSub,
    /// `i64.wrapping_mul(i64)`.
    I64WrappingMul,
    /// `i64.wrapping_neg()`: `i64::MIN` negates to itself.
    I64WrappingNeg,
    /// `i64.checked_add(i64)` → `Option<i64>`, `None` on overflow.
    I64CheckedAdd,
    /// `i64.checked_sub(i64)` → `Option<i64>`.
    I64CheckedSub,
    /// `i64.checked_mul(i64)` → `Option<i64>`.
    I64CheckedMul,
    /// `i64.checked_neg()` → `Option<i64>`, `None` for `i64::MIN`.
    I64CheckedNeg,
    /// `String.to_string()` — the identity, and the only intrinsic that emits no
    /// code at all.
    StringToString,
    /// UTF-8 byte length, excluding the trailing NUL.
    StringLen,
    StringSubstring,
    StringByteAt,
    StringSplit,
    StringContains,
    /// `String.find(needle)` → `Option<i64>`: the first byte offset, `None`
    /// when absent. The runtime call answers `-1` for absence (an ABI detail);
    /// the lowering turns that into the inline `Option<i64>` pair.
    StringFind,
    StringEndsWith,
    StringReplace,
    StringTrimStart,
    StringTrimEnd,
    StringToLower,
    StringToUpper,
    StringTrim,
    StringRepeat,
    StringStartsWith,

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
    /// Strong, sequentially consistent compare-and-exchange; returns success.
    AtomicCompareExchange,
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
    /// `Channel<T>::recv_opt()` → `Option<T>`: `recv` that answers `None`
    /// once the channel is closed and drained.
    ChannelRecvOpt,
    /// `Channel<T>::close()`.
    ChannelClose,

    /// Raw array storage length.
    ArrayLen,
    StringConcatArray,
    /// Allocate or release logical array slots, retaining the handle.
    ArrayResize,
    /// Scalar string debug formatting (including quotes/escapes).
    StringDebug,
    /// Allocate independent immutable array storage.
    ArrayCopy,

    /// `FrozenArray<T>::len()` — the only method on a frozen array.
    FrozenArrayLen,

    /// `Map<K, V>::insert(k, v)`.
    MapInsert,
    /// `Map<K, V>::get(k)` → `Option<V>`.
    MapGet,
    MapRemove,
    MapKeys,
    MapValues,
    MapEntries,
    /// `Map<K, V>::contains(k)`.
    MapContains,
    /// `Map<K, V>::len()`.
    MapLen,
    /// `Map<K, V>::to_string()` → `"{k: v, ...}"` sorted by key.
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
pub enum IntrinsicReturn<N = String> {
    /// A type determined by the receiver.
    Fixed(Type<N>),
    /// The type of the argument at this index, passed straight through.
    SameAsArg(usize),
}

/// A checked member can still have an arity error. Retain its identity for
/// recovery diagnostics, while executable consumers see only complete calls.
#[derive(Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BuiltinCalls(
    std::collections::HashMap<crate::parser::ast::ExprId, (BuiltinMethod, bool)>,
);

impl BuiltinCalls {
    pub(crate) fn record(
        &mut self,
        id: crate::parser::ast::ExprId,
        method: BuiltinMethod,
        arity: usize,
        supplied: usize,
    ) {
        self.0.insert(id, (method, arity == supplied));
    }

    pub(crate) fn get(&self, id: &crate::parser::ast::ExprId) -> Option<&Intrinsic> {
        match self.0.get(id) {
            Some((BuiltinMethod::Intrinsic(id), true)) => Some(id),
            _ => None,
        }
    }

    pub(crate) fn candidate(&self, id: &crate::parser::ast::ExprId) -> Option<&Intrinsic> {
        match self.0.get(id) {
            Some((BuiltinMethod::Intrinsic(id), _)) => Some(id),
            _ => None,
        }
    }

    pub(crate) fn checked(&self, id: &crate::parser::ast::ExprId) -> Option<BuiltinMethod> {
        self.0
            .get(id)
            .and_then(|(method, valid)| valid.then_some(*method))
    }
    /// ADT contracts model a recognized call even during arity recovery.
    pub(crate) fn effect_method(&self, id: &crate::parser::ast::ExprId) -> Option<BuiltinMethod> {
        let &(method, valid) = self.0.get(id)?;
        (valid || matches!(method, BuiltinMethod::Adt(_))).then_some(method)
    }

    pub(crate) fn adt(&self, id: &crate::parser::ast::ExprId) -> Option<AdtMethod> {
        match self.0.get(id) {
            Some((BuiltinMethod::Adt(method), _)) => Some(*method),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.0.extend(other.0);
    }
}

/// A resolved builtin call: what it is, and what it produces.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedMethod<N = String> {
    pub intrinsic: Intrinsic,
    pub arity: usize,
    pub ret: IntrinsicReturn<N>,
}

impl<N: From<&'static str>> ResolvedMethod<N> {
    /// The result type, given a way to type the call's arguments. Callers that
    /// have no argument types available can pass a closure returning `None`; the
    /// two argument-typed intrinsics then fall back to their receiver family's
    /// declared shape (`Task<void>`), which is the same conservative answer the
    /// checker gives for a malformed `add`/`attach`.
    pub fn return_type(self, arg_type: impl Fn(usize) -> Option<Type<N>>) -> Type<N> {
        match self.ret {
            IntrinsicReturn::Fixed(ty) => ty,
            IntrinsicReturn::SameAsArg(index) => {
                arg_type(index).unwrap_or_else(|| Type::Generic("Task".into(), vec![Type::Void]))
            }
        }
    }
}

/// Storage that a resolved method can mutate, independent of allocation or GC.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntrinsicWrite {
    None,
    Receiver,
    External,
}

impl Intrinsic {
    /// Every intrinsic, for exhaustiveness tests. A new variant that is not
    /// added here fails `all_variants_are_listed`.
    pub const ALL: &'static [Intrinsic] = &[
        Intrinsic::I64ToString,
        Intrinsic::IntegerConvert,
        Intrinsic::IntegerWrappingConvert,
        Intrinsic::IntegerCheckedConvert,
        Intrinsic::ScalarParse,
        Intrinsic::ScalarToString,
        Intrinsic::F64ToString,
        Intrinsic::BoolToString,
        Intrinsic::I64WrappingAdd,
        Intrinsic::I64WrappingSub,
        Intrinsic::I64WrappingMul,
        Intrinsic::I64WrappingNeg,
        Intrinsic::I64CheckedAdd,
        Intrinsic::I64CheckedSub,
        Intrinsic::I64CheckedMul,
        Intrinsic::I64CheckedNeg,
        Intrinsic::StringToString,
        Intrinsic::StringLen,
        Intrinsic::StringSubstring,
        Intrinsic::StringByteAt,
        Intrinsic::StringSplit,
        Intrinsic::StringContains,
        Intrinsic::StringFind,
        Intrinsic::StringEndsWith,
        Intrinsic::StringReplace,
        Intrinsic::StringTrimStart,
        Intrinsic::StringTrimEnd,
        Intrinsic::StringToLower,
        Intrinsic::StringToUpper,
        Intrinsic::StringTrim,
        Intrinsic::StringRepeat,
        Intrinsic::StringStartsWith,
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
        Intrinsic::AtomicCompareExchange,
        Intrinsic::AtomicAdd,
        Intrinsic::AtomicSub,
        Intrinsic::CellGet,
        Intrinsic::CellSet,
        Intrinsic::RwCellRead,
        Intrinsic::RwCellWrite,
        Intrinsic::ChannelSend,
        Intrinsic::ChannelRecv,
        Intrinsic::ChannelRecvOpt,
        Intrinsic::ChannelClose,
        Intrinsic::ArrayLen,
        Intrinsic::StringConcatArray,
        Intrinsic::ArrayResize,
        Intrinsic::StringDebug,
        Intrinsic::ArrayCopy,
        Intrinsic::FrozenArrayLen,
        Intrinsic::MapInsert,
        Intrinsic::MapGet,
        Intrinsic::MapRemove,
        Intrinsic::MapKeys,
        Intrinsic::MapValues,
        Intrinsic::MapEntries,
        Intrinsic::MapContains,
        Intrinsic::MapLen,
        Intrinsic::MapToString,
        Intrinsic::MapFreeze,
        Intrinsic::FrozenMapGet,
        Intrinsic::FrozenMapContains,
        Intrinsic::FrozenMapLen,
    ];

    /// The source-level method names [`resolve`] maps to this intrinsic. The
    /// match is exhaustive, so a new builtin method cannot skip the naming
    /// inventory (`public_builtin_names_are_snake_case`). `__abi_*` names are
    /// private `std` plumbing, not public API.
    #[cfg(test)]
    pub fn source_names(self) -> &'static [&'static str] {
        use Intrinsic::*;
        match self {
            I64ToString | F64ToString | BoolToString | StringToString | MapToString => {
                &["to_string"]
            }
            ScalarParse => &[
                "parse_i8",
                "parse_i16",
                "parse_i32",
                "parse_i64",
                "parse_u8",
                "parse_u16",
                "parse_u32",
                "parse_u64",
                "parse_char",
            ],
            ScalarToString => &["to_string"],
            IntegerConvert => &[
                "to_i8", "to_i16", "to_i32", "to_i64", "to_u8", "to_u16", "to_u32", "to_u64",
            ],
            IntegerWrappingConvert => &[
                "wrapping_to_i8",
                "wrapping_to_i16",
                "wrapping_to_i32",
                "wrapping_to_i64",
                "wrapping_to_u8",
                "wrapping_to_u16",
                "wrapping_to_u32",
                "wrapping_to_u64",
            ],
            IntegerCheckedConvert => &[
                "checked_to_i8",
                "checked_to_i16",
                "checked_to_i32",
                "checked_to_i64",
                "checked_to_u8",
                "checked_to_u16",
                "checked_to_u32",
                "checked_to_u64",
                "checked_to_char",
            ],
            I64WrappingAdd => &["wrapping_add"],
            I64WrappingSub => &["wrapping_sub"],
            I64WrappingMul => &["wrapping_mul"],
            I64WrappingNeg => &["wrapping_neg"],
            I64CheckedAdd => &["checked_add"],
            I64CheckedSub => &["checked_sub"],
            I64CheckedMul => &["checked_mul"],
            I64CheckedNeg => &["checked_neg"],
            StringLen | FrozenArrayLen | MapLen | FrozenMapLen => &["len"],
            StringSubstring => &["substring", "slice"],
            StringByteAt => &["byte_at"],
            StringSplit => &["split"],
            StringContains | MapContains | FrozenMapContains => &["contains"],
            StringFind => &["find"],
            StringEndsWith => &["ends_with"],
            StringReplace => &["replace"],
            StringTrimStart => &["trim_start"],
            StringTrimEnd => &["trim_end"],
            StringToLower => &["to_lower"],
            StringToUpper => &["to_upper"],
            StringTrim => &["trim"],
            StringRepeat => &["repeat"],
            StringStartsWith => &["starts_with"],
            TaskCancel | TokenCancel | ScopeCancel => &["cancel"],
            TaskIsCancelled | TokenIsCancelled | ScopeIsCancelled => &["is_cancelled"],
            TaskResult => &["result"],
            TokenChild | ScopeChild => &["child"],
            TokenAttach => &["attach"],
            ScopeAdd => &["add"],
            ScopeFinish => &["finish"],
            AtomicLoad => &["load"],
            AtomicStore => &["store"],
            AtomicSwap => &["swap"],
            AtomicCompareExchange => &["compare_exchange"],
            AtomicAdd => &["add"],
            AtomicSub => &["sub"],
            CellGet | MapGet | FrozenMapGet => &["get"],
            CellSet => &["set"],
            RwCellRead => &["read"],
            RwCellWrite => &["write"],
            ChannelSend => &["send"],
            ChannelRecv => &["recv"],
            ChannelRecvOpt => &["recv_opt"],
            ChannelClose => &["close"],
            ArrayLen => &["__abi_len"],
            StringConcatArray => &["__abi_concat"],
            ArrayResize => &["__abi_resize"],
            StringDebug => &["__abi_debug"],
            ArrayCopy => &["__abi_copy"],
            MapInsert => &["insert"],
            MapRemove => &["remove"],
            MapKeys => &["keys"],
            MapValues => &["values"],
            MapEntries => &["entries"],
            MapFreeze => &["freeze"],
        }
    }

    pub(crate) fn channel_wait_name(self) -> Option<&'static str> {
        match self {
            Self::ChannelSend => Some("Channel.send"),
            Self::ChannelRecv => Some("Channel.recv"),
            Self::ChannelRecvOpt => Some("Channel.recv_opt"),
            _ => None,
        }
    }

    /// Whether lowering this intrinsic can suspend the calling task.
    ///
    /// A synchronous `send` on a full bounded channel and a `recv` on an empty
    /// one both park the task and hand the worker back to the scheduler, so the
    /// cooperative passes must treat the call as a suspension point and
    /// frame-back everything live across it. Nothing else in the table can
    /// suspend: the atomics and the blocking cells complete in place, and
    /// `TaskScope::finish` returns a task rather than waiting for one.
    pub fn is_suspension_point(self) -> bool {
        matches!(
            self,
            Intrinsic::ChannelSend | Intrinsic::ChannelRecv | Intrinsic::ChannelRecvOpt
        )
    }

    /// Writes to existing storage and whether the returned aggregate has private
    /// outer storage. Snapshot elements can still alias their source elements.
    pub(crate) fn storage_effect(self) -> (IntrinsicWrite, bool) {
        use Intrinsic::*;
        match self {
            StringSplit
            | ArrayCopy
            | MapKeys
            | MapValues
            | MapEntries
            | MapFreeze
            | MapGet
            | FrozenMapGet
            | IntegerCheckedConvert
            | ScalarParse
            | I64CheckedAdd
            | I64CheckedSub
            | I64CheckedMul
            | I64CheckedNeg
            | StringFind => (IntrinsicWrite::None, true),
            MapRemove => (IntrinsicWrite::Receiver, true),
            ChannelRecvOpt => (IntrinsicWrite::External, true),
            TaskCancel | TokenCancel | TokenChild | TokenAttach | ScopeCancel | ScopeChild
            | ScopeAdd | ScopeFinish | ChannelSend | ChannelRecv | ChannelClose => {
                (IntrinsicWrite::External, false)
            }
            AtomicStore
            | AtomicSwap
            | AtomicCompareExchange
            | AtomicAdd
            | AtomicSub
            | CellSet
            | RwCellWrite
            | ArrayResize
            | MapInsert => (IntrinsicWrite::Receiver, false),
            I64ToString
            | IntegerConvert
            | IntegerWrappingConvert
            | ScalarToString
            | F64ToString
            | BoolToString
            | I64WrappingAdd
            | I64WrappingSub
            | I64WrappingMul
            | I64WrappingNeg
            | StringToString
            | StringLen
            | StringSubstring
            | StringByteAt
            | StringContains
            | StringEndsWith
            | StringReplace
            | StringTrimStart
            | StringTrimEnd
            | StringToLower
            | StringToUpper
            | StringTrim
            | StringRepeat
            | StringStartsWith
            | TaskIsCancelled
            | TaskResult
            | TokenIsCancelled
            | ScopeIsCancelled
            | AtomicLoad
            | CellGet
            | RwCellRead
            | ArrayLen
            | StringConcatArray
            | StringDebug
            | FrozenArrayLen
            | MapContains
            | MapLen
            | MapToString
            | FrozenMapContains
            | FrozenMapLen => (IntrinsicWrite::None, false),
        }
    }

    /// Every runtime symbol the lowering of this intrinsic can call, over all
    /// receiver representations (a channel's element suffix, an atomic's
    /// width). Inline fast paths are covered by their runtime fallback: an
    /// array length reads the header inline and calls `willow_array_len` only
    /// for a null receiver or a malformed negative length. Keep in step with
    /// `emit_flat_intrinsic_inner`.
    pub fn runtime_symbols(self) -> &'static [&'static str] {
        use Intrinsic::*;
        match self {
            I64ToString => &[rt::willow_i64_to_string],
            ScalarParse => &[rt::willow_scalar_parse],
            ScalarToString => &[rt::willow_scalar_to_string],
            F64ToString => &[rt::willow_f64_to_string],
            BoolToString => &[rt::willow_bool_to_string],
            IntegerConvert
            | IntegerWrappingConvert
            | IntegerCheckedConvert
            | StringToString
            | TaskResult => &[],
            I64WrappingAdd | I64WrappingSub | I64WrappingMul | I64WrappingNeg | I64CheckedAdd
            | I64CheckedSub | I64CheckedMul | I64CheckedNeg => &[],
            StringLen => &[],
            StringByteAt => &[rt::willow_string_byte_at],
            StringSubstring => &[rt::willow_string_substring],
            StringSplit => &[rt::willow_string_split],
            StringContains => &[rt::willow_string_contains],
            StringFind => &[rt::willow_string_find],
            StringEndsWith => &[rt::willow_string_ends_with],
            StringReplace => &[rt::willow_string_replace],
            StringTrimStart => &[rt::willow_string_trim_start],
            StringTrimEnd => &[rt::willow_string_trim_end],
            StringToLower => &[rt::willow_string_to_lower],
            StringToUpper => &[rt::willow_string_to_upper],
            StringTrim => &[rt::willow_string_trim],
            StringRepeat => &[rt::willow_string_repeat],
            StringStartsWith => &[rt::willow_string_starts_with],

            TaskCancel => &[rt::willow_sched_cancel],
            TaskIsCancelled => &[rt::willow_frame_is_cancelled],
            TokenIsCancelled => &[rt::willow_cancellation_token_is_cancelled],
            TokenCancel => &[rt::willow_cancellation_token_cancel],
            TokenChild => &[rt::willow_cancellation_token_child],
            TokenAttach => &[rt::willow_cancellation_token_attach],
            ScopeIsCancelled => &[rt::willow_task_scope_is_cancelled],
            ScopeCancel => &[rt::willow_task_scope_cancel],
            ScopeChild => &[rt::willow_task_scope_child],
            ScopeAdd => &[rt::willow_task_scope_add],
            ScopeFinish => &[rt::willow_task_scope_finish],
            AtomicLoad => &[rt::willow_atomic_i64_load, rt::willow_atomic_bool_load],
            AtomicStore => &[rt::willow_atomic_i64_store, rt::willow_atomic_bool_store],
            AtomicSwap => &[rt::willow_atomic_i64_swap, rt::willow_atomic_bool_swap],
            AtomicCompareExchange => &[
                rt::willow_atomic_i64_compare_exchange,
                rt::willow_atomic_bool_compare_exchange,
            ],
            AtomicAdd => &[rt::willow_atomic_i64_add],
            AtomicSub => &[rt::willow_atomic_i64_sub],
            CellGet => &[rt::willow_blocking_cell_get],
            CellSet => &[rt::willow_blocking_cell_set],
            RwCellRead => &[rt::willow_blocking_rw_cell_read],
            RwCellWrite => &[rt::willow_blocking_rw_cell_write],
            ChannelSend => &[
                rt::willow_channel_send_i64,
                rt::willow_channel_send_bool,
                rt::willow_channel_send_f64,
                rt::willow_channel_send_ptr,
            ],
            ChannelRecv => &[
                rt::willow_channel_recv_i64,
                rt::willow_channel_recv_bool,
                rt::willow_channel_recv_f64,
                rt::willow_channel_recv_ptr,
            ],
            ChannelRecvOpt => &[rt::willow_channel_recv_opt_into],
            ChannelClose => &[rt::willow_channel_close],
            StringConcatArray => &[rt::willow_string_concat_array],
            ArrayLen | FrozenArrayLen => &[rt::willow_array_len],
            ArrayResize => &[rt::willow_array_resize],
            StringDebug => &[rt::willow_string_debug],
            ArrayCopy => &[rt::willow_array_copy, rt::willow_array_copy_scalar_pairs],
            MapRemove => &[rt::willow_map_remove_into],
            MapKeys | MapValues | MapEntries => &[rt::willow_map_snapshot],
            MapInsert => &[rt::willow_map_insert],
            MapGet | FrozenMapGet => &[rt::willow_map_get, rt::willow_map_get_into],
            MapContains | FrozenMapContains => &[rt::willow_map_contains],
            MapLen | FrozenMapLen => &[rt::willow_map_len],
            MapToString => &[rt::willow_map_to_string],
            MapFreeze => &[rt::willow_map_copy],
        }
    }

    /// Runtime effects of one call: the union of its runtime symbols' ABI
    /// effects, plus `MAY_SUSPEND` at a suspension point. A symbol missing from
    /// the ABI table fails closed to every effect.
    pub fn runtime_effects(self) -> RuntimeEffects {
        let mut effects = if self.is_suspension_point() {
            RuntimeEffects::MAY_SUSPEND
        } else {
            RuntimeEffects::NONE
        };
        for name in self.runtime_symbols() {
            effects = effects.union(
                willow_abi::runtime_symbol(name).map_or(RuntimeEffects::ALL, |s| s.effects()),
            );
        }
        effects
    }

    /// The call-graph target of a resolved builtin method call. Intrinsics
    /// have no Willow body, so this id never names a graph node; consumers
    /// classify it with [`builtin_target_effects`] instead of treating the
    /// call site as unresolved.
    pub fn function_id(self) -> FunctionId {
        FunctionId::method(TypeId::local(INTRINSIC_OWNER), format!("{self:?}"))
    }

    /// Inverse of [`Intrinsic::function_id`], by one hash lookup.
    pub fn from_function_id(id: &FunctionId) -> Option<Self> {
        static BY_NAME: std::sync::OnceLock<std::collections::HashMap<String, Intrinsic>> =
            std::sync::OnceLock::new();
        if id.owner().as_deref() != Some(INTRINSIC_OWNER) || id.namespace().is_some() {
            return None;
        }
        BY_NAME
            .get_or_init(|| Self::ALL.iter().map(|&i| (format!("{i:?}"), i)).collect())
            .get(id.name().as_ref())
            .copied()
    }
}

/// Owner of synthetic intrinsic call targets. `$` cannot start a source
/// identifier, so no user type can collide with it.
const INTRINSIC_OWNER: &str = "$intrinsic";

/// The builtin free functions code generation defines as ordinary
/// Willow-ABI functions, so they can be used as `fn` values. Every other
/// builtin is lowered inline at each call site and has no address.
///
pub(crate) const ADDRESSABLE_BUILTINS: [&str; 2] = ["pow", "powf"];

/// Full runtime effects of a call target that has no Willow body: a builtin
/// method ([`Intrinsic::function_id`]) or a builtin free function. `None`
/// means the target is not a known builtin, and the caller must fail closed.
/// A user declaration of the same name is a graph node and is looked up
/// before this is consulted.
pub(crate) fn builtin_target_effects(target: &FunctionId) -> Option<RuntimeEffects> {
    if target.is_method_of("$output") && matches!(target.name().as_ref(), "print" | "println") {
        // All scalar output overloads participate in the same source operation.
        // Union the ABI rows so future overload-specific effects remain sound.
        return Some(
            [
                rt::willow_print_i64,
                rt::willow_println_i64,
                rt::willow_print_bool,
                rt::willow_println_bool,
                rt::willow_print_f64,
                rt::willow_println_f64,
                rt::willow_print_string,
                rt::willow_println_string,
            ]
            .into_iter()
            .fold(RuntimeEffects::NONE, |effects, symbol| {
                effects.union(symbol_effects(symbol))
            }),
        );
    }
    if let Some(intrinsic) = Intrinsic::from_function_id(target) {
        return Some(intrinsic.runtime_effects());
    }
    if let Some(method) = target.adt_method() {
        return (!method.callback()).then(|| {
            if method.can_panic() {
                RuntimeEffects::MAY_PANIC.union(RuntimeEffects::MAY_ALLOCATE)
            } else {
                RuntimeEffects::NONE
            }
        });
    }
    if target.owner().is_some() || target.namespace().is_some() {
        return None;
    }
    match target.name().as_ref() {
        "panic" => Some(RuntimeEffects::MAY_PANIC),
        // String formatting allocates the result and can panic on a failed
        // conversion; argument evaluation is attributed at its own sites.
        "format" => Some(RuntimeEffects::MAY_PANIC.union(RuntimeEffects::MAY_ALLOCATE)),
        "recover" | "pow" | "powf" => Some(RuntimeEffects::NONE),
        name => builtin_call_runtime_name(name).map(symbol_effects),
    }
}

fn symbol_effects(name: &str) -> RuntimeEffects {
    willow_abi::runtime_symbol(name).map_or(RuntimeEffects::ALL, |s| s.effects())
}

/// Runtime entry points whose only managed allocation formats a language
/// fault's message before raising it. The ABI row keeps `MAY_ALLOCATE` for GC
/// and summary consumers; `no_alloc` excludes fault reporting, so contracts
/// drop that bit. Keep in step with each symbol's success path.
/// Map lookups stay conservative: key encoding may box at the call site.
const FAULT_ONLY_ALLOCATION: [&str; 9] = [
    rt::willow_array_len,
    rt::willow_array_get,
    rt::willow_array_set,
    rt::willow_array_element_addr,
    rt::willow_array_reference_owner,
    rt::willow_i64_abs,
    rt::willow_i64_clamp,
    rt::willow_f64_clamp,
    rt::willow_string_byte_at,
];

/// [`builtin_target_effects`]/[`builtin_static_effects`] as seen by effect
/// contracts: allocation that happens only while reporting a fault is not a
/// `no_alloc` violation. Every symbol an intrinsic may call must be
/// fault-only for its allocation bit to be dropped.
pub(crate) fn builtin_contract_effects(
    target: &FunctionId,
    module_aliases: &std::collections::HashMap<String, String>,
) -> Option<RuntimeEffects> {
    let effects = builtin_target_effects(target)
        .or_else(|| builtin_static_effects(target, module_aliases))?;
    let fault_only = if let Some(intrinsic) = Intrinsic::from_function_id(target) {
        let symbols = intrinsic.runtime_symbols();
        !symbols.is_empty() && symbols.iter().all(|s| FAULT_ONLY_ALLOCATION.contains(s))
    } else {
        // Unwrapping only allocates the panic payload on the failing branch.
        target.adt_method().is_some_and(AdtMethod::can_panic)
    };
    Some(if fault_only {
        effects.difference(RuntimeEffects::MAY_ALLOCATE)
    } else {
        effects
    })
}

/// The runtime constructor a builtin `Class::method(...)` static call lowers to
/// (`emit_lir_static_call_values_inner`). The caller rules out user classes of
/// the same name first: a declared static method always wins.
pub(crate) fn builtin_static_constructor(class: &str, method: &str) -> Option<&'static str> {
    Some(match (B::from_name(class), method) {
        (Some(B::Map), "new") => rt::willow_map_new,
        (Some(B::Channel), "new") => rt::willow_channel_new,
        (Some(B::Channel), "with_capacity") => rt::willow_channel_new_bounded,
        (Some(B::CancellationToken), "new") => rt::willow_cancellation_token_new,
        (Some(B::TaskScope), "new") => rt::willow_task_scope_new,
        (Some(B::AtomicI64), "new") => rt::willow_atomic_i64_new,
        (Some(B::AtomicBool), "new") => rt::willow_atomic_bool_new,
        (Some(B::BlockingCell), "new") => rt::willow_blocking_cell_new,
        (Some(B::BlockingRwCell), "new") => rt::willow_blocking_rw_cell_new,
        (Some(B::Mutex), "new") => rt::willow_async_mutex_new,
        (Some(B::RwLock), "new") => rt::willow_async_rwlock_new,
        _ => return None,
    })
}

/// Effects of a static call whose target has no Willow body: a builtin
/// constructor, a builtin-enum variant, or a namespace builtin (`fs::read`).
/// `module_aliases` maps a caller's `import std::fs as files` spellings back to
/// the builtin namespace. `None` means the target is not a known builtin and
/// stays unresolved.
pub(crate) fn builtin_static_effects(
    target: &FunctionId,
    module_aliases: &std::collections::HashMap<String, String>,
) -> Option<RuntimeEffects> {
    let owner = target.owner().filter(|_| target.namespace().is_none())?;
    let owner = owner.as_ref();
    let method = target.name();
    let method = method.as_ref();
    // Array construction has its own typed lowering, but the same runtime
    // preemption/effect contract as its allocation ABI entry.
    if (B::from_name(owner), method) == (Some(B::Array), "new") {
        return Some(symbol_effects(rt::willow_array_new_literal));
    }
    if let Some(runtime) = builtin_static_constructor(owner, method) {
        return Some(symbol_effects(runtime));
    }
    // Variant construction boxes the payload on the GC heap.
    if matches!(
        (B::from_name(owner), method),
        (Some(B::Option), "Some" | "None") | (Some(B::Result), "Ok" | "Err")
    ) {
        return Some(RuntimeEffects::MAY_ALLOCATE);
    }
    let namespace = module_aliases.get(owner).map_or(owner, String::as_str);
    namespace_builtin(namespace, method).map(|entry| symbol_effects(entry.runtime))
}

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
pub fn resolve<N: builtin_types::TypeName + Clone + From<&'static str>>(
    recv: &Type<N>,
    method: &str,
    arity: usize,
) -> Option<ResolvedMethod<N>> {
    match lookup(recv, method) {
        MethodLookup::Intrinsic(sig) if sig.arity == arity => Some(sig),
        _ => None,
    }
}

pub(crate) const LENGTH: &str = "len";
pub(crate) const ARRAY_LENGTH: &str = "__abi_len";
pub(crate) const ARRAY_RESIZE: &str = "__abi_resize";
pub(crate) const MAP_ENTRIES: &str = "entries";

/// Select case syntax uses the same channel operation spellings before typing.
pub(crate) mod channel_names {
    pub const SEND: &str = "send";
    pub const RECV: &str = "recv";
    pub const RECV_OPT: &str = "recv_opt";
}

/// The two builtin dispatch paths share one checked fact per method expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BuiltinMethod {
    Intrinsic(Intrinsic),
    Adt(AdtMethod),
}

impl BuiltinMethod {
    pub(crate) fn is_suspension_point(self) -> bool {
        matches!(self, Self::Intrinsic(id) if id.is_suspension_point())
    }
}

macro_rules! method_names {
    (adt {
        option { $($opt:ident => $opt_name:literal $(+ $also:ident)?),+ $(,)? }
        result_only { $($res:ident => $res_name:literal),+ $(,)? }
    } other $other:tt) => {
        method_names!(@all { $($opt => $opt_name,)+ $($res => $res_name),+ } other $other);
        impl AdtMethod {
            pub(crate) const OPTION_NAMES: &'static [&'static str] = &[$($opt_name),+];

            fn accepts(self, family: B) -> bool {
                match self {
                    $(Self::$opt => matches!(family, B::Option $(| B::$also)?),)+
                    $(Self::$res)|+ => family == B::Result,
                }
            }
        }
    };
    (@all { $($adt:ident => $spelling:literal),+ } other { $($other:ident => $name:tt $(:: $part:ident)*),+ $(,)? }) => {
        #[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        pub enum AdtMethod { $(#[serde(rename = $spelling)] $adt),+ }
        impl AdtMethod {
            fn name(self) -> &'static str { match self { $(Self::$adt => $spelling),+ } }
            pub(crate) fn from_graph_name(name: &str) -> Option<Self> {
                match name { $($spelling => Some(Self::$adt),)+ _ => None }
            }
        }
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub(crate) enum MethodName { Adt(AdtMethod), $($other,)+ Parse(ScalarTarget), CheckedConvert(ScalarTarget), WrappingConvert(ScalarTarget), Convert(ScalarTarget), Unknown }
        impl MethodName {
            pub(crate) fn fixed_spelling(self) -> Option<&'static str> {
                match self {
                    Self::Adt(method) => Some(method.name()),
                    $(Self::$other => Some($name $(:: $part)*),)+
                    _ => None,
                }
            }
            pub(crate) fn classify(method: &str) -> Self {
                #[cfg(test)]
                METHOD_LOOKUPS.with(|count| count.set(count.get() + 1));
                match method {
                    $($spelling => Self::Adt(AdtMethod::$adt),)+
                    $($name $(:: $part)* => Self::$other,)+
                    _ => {
                        if let Some(s) = method.strip_prefix("parse_") { ScalarTarget::from_suffix(s).map(Self::Parse) }
                        else if let Some(s) = method.strip_prefix("checked_to_") { ScalarTarget::from_suffix(s).map(Self::CheckedConvert) }
                        else if let Some(s) = method.strip_prefix("wrapping_to_") { ScalarTarget::from_suffix(s).map(Self::WrappingConvert) }
                        else if let Some(s) = method.strip_prefix("to_") { ScalarTarget::from_suffix(s).map(Self::Convert) }
                        else { None }.unwrap_or(Self::Unknown)
                    }
                }
            }
        }
    };
}
method_names! {
    adt {
        // Declaration order breaks ties in Option's typo suggestions.
        option {
            Unwrap => "unwrap" + Result,
            Expect => "expect" + Result,
            UnwrapOr => "unwrap_or" + Result,
            IsSome => "is_some",
            IsNone => "is_none",
            Map => "map" + Result,
            AndThen => "and_then" + Result,
            OrElse => "or_else" + Result,
        }
        result_only {
            IsOk => "is_ok",
            IsErr => "is_err",
            UnwrapErr => "unwrap_err",
            MapErr => "map_err",
        }
    }
    other {
        AbiConcat => "__abi_concat",
        AbiCopy => "__abi_copy",
        AbiDebug => "__abi_debug",
        AbiLen => ARRAY_LENGTH,
        AbiResize => ARRAY_RESIZE,
        Add => "add",
        Attach => "attach",
        ByteAt => "byte_at",
        Cancel => "cancel",
        CheckedAdd => "checked_add",
        CheckedMul => "checked_mul",
        CheckedNeg => "checked_neg",
        CheckedSub => "checked_sub",
        Child => "child",
        Close => "close",
        CompareExchange => "compare_exchange",
        Contains => "contains",
        EndsWith => "ends_with",
        Entries => MAP_ENTRIES,
        Find => "find",
        Finish => "finish",
        Freeze => "freeze",
        Get => "get",
        Insert => "insert",
        IsCancelled => "is_cancelled",
        Join => "join",
        Keys => "keys",
        Len => LENGTH,
        Load => "load",
        Pop => "pop",
        Push => "push",
        Read => "read",
        Recv => channel_names::RECV,
        RecvOpt => channel_names::RECV_OPT,
        Remove => "remove",
        Repeat => "repeat",
        Replace => "replace",
        Result => "result",
        Send => channel_names::SEND,
        Set => "set",
        Slice => "slice",
        Split => "split",
        StartsWith => "starts_with",
        Store => "store",
        Sub => "sub",
        Substring => "substring",
        Swap => "swap",
        RetiredToString => "toString",
        ToLower => "to_lower",
        ToString => "to_string",
        ToUpper => "to_upper",
        Trim => "trim",
        TryJoin => "try_join",
        TrimEnd => "trim_end",
        TrimStart => "trim_start",
        Values => "values",
        WrappingAdd => "wrapping_add",
        WrappingMul => "wrapping_mul",
        WrappingNeg => "wrapping_neg",
        WrappingSub => "wrapping_sub",
        Write => "write",
    }
}

// Keep the established LIR debug dump (quoted source spelling).
impl std::fmt::Debug for AdtMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.name(), f)
    }
}
impl std::fmt::Display for AdtMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}
impl AdtMethod {
    pub(crate) fn callback(self) -> bool {
        matches!(
            self,
            Self::Map | Self::MapErr | Self::AndThen | Self::OrElse
        )
    }
    fn can_panic(self) -> bool {
        matches!(self, Self::Expect | Self::Unwrap | Self::UnwrapErr)
    }
    pub(crate) fn target(self) -> Option<FunctionId> {
        (!self.callback()).then(|| FunctionId::method(TypeId::local("$adt"), self.name()))
    }
    pub(crate) fn arity(self) -> usize {
        usize::from(self.callback() || matches!(self, Self::Expect | Self::UnwrapOr))
    }
}
impl MethodName {
    pub(crate) fn is_retired_wait(self) -> bool {
        matches!(self, Self::Join | Self::TryJoin)
    }

    /// Private storage operations the compiler lowers itself.
    pub(crate) fn is_abi_storage(self) -> bool {
        matches!(
            self,
            Self::AbiConcat | Self::AbiCopy | Self::AbiDebug | Self::AbiLen | Self::AbiResize
        )
    }

    fn scalar_operation(self) -> Option<(Intrinsic, ScalarTarget)> {
        Some(match self {
            Self::Parse(target) => (Intrinsic::ScalarParse, target),
            Self::CheckedConvert(target) => (Intrinsic::IntegerCheckedConvert, target),
            Self::WrappingConvert(target) => (Intrinsic::IntegerWrappingConvert, target),
            Self::Convert(target) => (Intrinsic::IntegerConvert, target),
            _ => return None,
        })
    }

    pub(crate) fn replacement(self) -> Option<(&'static str, Self)> {
        let current = match self {
            Self::RetiredToString => Self::ToString,
            _ => return None,
        };
        Some((current.fixed_spelling()?, current))
    }

    pub(crate) fn channel(self) -> Option<Intrinsic> {
        Some(match self {
            Self::Send => Intrinsic::ChannelSend,
            Self::Recv => Intrinsic::ChannelRecv,
            Self::RecvOpt => Intrinsic::ChannelRecvOpt,
            Self::Close => Intrinsic::ChannelClose,
            _ => return None,
        })
    }
}

pub(crate) enum Lookup<S> {
    Intrinsic(S),
    Adt(AdtMethod),
    Missing(MethodName),
}
pub(crate) type MethodLookup<N> = Lookup<ResolvedMethod<N>>;

impl<N> MethodLookup<N> {
    pub(crate) fn is_retired_wait(&self) -> bool {
        matches!(self, Self::Missing(name) if name.is_retired_wait())
    }

    pub(crate) fn into_signature(self) -> Option<ResolvedMethod<N>> {
        if let Self::Intrinsic(sig) = self {
            Some(sig)
        } else {
            None
        }
    }
    pub(crate) fn identity(&self) -> Option<(BuiltinMethod, usize)> {
        match self {
            Self::Intrinsic(sig) => Some((BuiltinMethod::Intrinsic(sig.intrinsic), sig.arity)),
            Self::Adt(method) => Some((BuiltinMethod::Adt(*method), method.arity())),
            Self::Missing(_) => None,
        }
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static SCALAR_TARGET_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static RECEIVER_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static METHOD_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn lookup<N: builtin_types::TypeName + Clone + From<&'static str>>(
    recv: &Type<N>,
    method: &str,
) -> MethodLookup<N> {
    MethodName::classify(method).lookup(recv)
}

impl MethodName {
    pub(crate) fn lookup<N: builtin_types::TypeName + Clone + From<&'static str>>(
        self,
        recv: &Type<N>,
    ) -> MethodLookup<N> {
        self.lookup_with(recv, |intrinsic, arity, rule| ResolvedMethod {
            intrinsic,
            arity,
            ret: match rule {
                SignatureReturn::Fixed(pattern) => IntrinsicReturn::Fixed(pattern.materialize()),
                SignatureReturn::SameAsArg(i) => IntrinsicReturn::SameAsArg(i),
            },
        })
    }

    pub(crate) fn has_method_on<N: builtin_types::TypeName>(self, recv: &Type<N>) -> bool {
        !matches!(self.lookup_with(recv, |_, _, _| ()), Lookup::Missing(_))
    }

    fn lookup_with<N: builtin_types::TypeName, S>(
        self,
        recv: &Type<N>,
        select: impl FnOnce(Intrinsic, usize, SignatureReturn<'_, N>) -> S,
    ) -> Lookup<S> {
        #[cfg(test)]
        RECEIVER_LOOKUPS.with(|count| count.set(count.get() + 1));
        if let Self::Adt(adt) = self {
            if builtin_types::resolve(recv).is_some_and(|family| adt.accepts(family.id)) {
                return Lookup::Adt(adt);
            }
        } else if let Some(signature) = intrinsic_contract(recv, self, select) {
            return Lookup::Intrinsic(signature);
        }
        Lookup::Missing(self)
    }
}

enum SignatureReturn<'a, N> {
    Fixed(ResultPattern<'a, N>),
    SameAsArg(usize),
}

/// A return type pattern borrows receiver payloads. Composite nodes refer to
/// their children only while the selected signature is being consumed.
enum ResultPattern<'a, N> {
    Borrowed(&'a Type<N>),
    Scalar(ScalarTarget),
    Bool,
    Void,
    String,
    Array(&'a Self),
    Named(B),
    Generic(&'static str, &'a [Self]),
}

impl<N> ResultPattern<'_, N> {
    fn materialize(&self) -> Type<N>
    where
        N: Clone + From<&'static str>,
    {
        match self {
            Self::Borrowed(ty) => (*ty).clone(),
            Self::Scalar(target) => target.ty(),
            Self::Bool => Type::Bool,
            Self::Void => Type::Void,
            Self::String => Type::String,
            Self::Array(item) => Type::Array(Box::new(item.materialize())),
            Self::Named(head) => Type::Named(head.source_name().into()),
            Self::Generic(name, args) => {
                Type::Generic((*name).into(), args.iter().map(Self::materialize).collect())
            }
        }
    }

    fn matches(&self, ty: &Type<N>) -> bool
    where
        N: From<&'static str> + PartialEq,
    {
        match (self, ty) {
            (Self::Borrowed(expected), actual) => *expected == actual,
            (Self::Scalar(target), actual) => {
                std::mem::discriminant(&target.ty::<N>()) == std::mem::discriminant(actual)
            }
            (Self::Bool, Type::Bool) | (Self::Void, Type::Void) | (Self::String, Type::String) => {
                true
            }
            (Self::Array(item), Type::Array(actual)) => item.matches(actual),
            (Self::Named(head), Type::Named(actual)) => *actual == head.source_name().into(),
            (Self::Generic(name, args), Type::Generic(actual, items)) => {
                *actual == (*name).into()
                    && args.len() == items.len()
                    && args.iter().zip(items).all(|(arg, item)| arg.matches(item))
            }
            _ => false,
        }
    }
}

/// Verify the recorded operation and result against the same typed mapping
/// used by the checker. No source spelling or owned result type is needed.
pub(crate) fn matches_signature<N: builtin_types::TypeName + PartialEq + From<&'static str>>(
    recv: &Type<N>,
    name: MethodName,
    id: Intrinsic,
    args: &[Type<N>],
    result: &Type<N>,
) -> bool {
    intrinsic_contract(recv, name, |canonical, arity, rule| {
        id == canonical
            && args.len() == arity
            && match rule {
                SignatureReturn::Fixed(pattern) => pattern.matches(result),
                SignatureReturn::SameAsArg(i) => args.get(i) == Some(result),
            }
    })
    .unwrap_or(false)
}

fn intrinsic_contract<N: builtin_types::TypeName, S>(
    recv: &Type<N>,
    name: MethodName,
    select: impl FnOnce(Intrinsic, usize, SignatureReturn<'_, N>) -> S,
) -> Option<S> {
    use Intrinsic::*;
    use ResultPattern as P;
    macro_rules! methods {
        ($($name:pat => $id:ident $(if $guard:tt)? : ($arity:literal, $ty:expr)),+ $(,)?) => {{
            match name {
                $($name $(if $guard)? => Some(select($id, $arity, SignatureReturn::Fixed($ty))),)+
                _ => None,
            }
        }};
    }

    if let Some((op, target)) = name.scalar_operation() {
        let destination = target.ty::<N>();
        let scalar = P::Scalar(target);
        let option_args = [P::Scalar(target)];
        let result = match op {
            ScalarParse if matches!(recv, Type::String) => {
                P::Generic(B::Option.source_name(), &option_args)
            }
            IntegerCheckedConvert if recv.scalar_layout().is_some() => {
                P::Generic(B::Option.source_name(), &option_args)
            }
            IntegerWrappingConvert
                if recv.integer_layout().is_some() && destination.integer_layout().is_some() =>
            {
                scalar
            }
            IntegerConvert => {
                let source = recv.scalar_layout()?;
                let dest = destination.integer_layout()?;
                let max = if matches!(recv, Type::Char) {
                    0x10ffff
                } else {
                    source.max()
                };
                if dest.min() > source.min() || dest.max() < max {
                    return None;
                }
                scalar
            }
            _ => return None,
        };
        return Some(select(op, 0, SignatureReturn::Fixed(result)));
    }
    if matches!(recv, Type::String) {
        return methods!(
            MethodName::ToString => StringToString: (0, P::String),
            MethodName::AbiDebug => StringDebug: (0, P::String),
            MethodName::Len => StringLen: (0, P::Scalar(ScalarTarget::I64)),
            MethodName::ByteAt => StringByteAt: (1, P::Scalar(ScalarTarget::I64)),
            MethodName::Substring | MethodName::Slice => StringSubstring: (2, P::String),
            MethodName::Split => StringSplit: (1, P::Array(&P::String)),
            MethodName::Contains => StringContains: (1, P::Bool),
            MethodName::Find => StringFind: (1, P::Generic(B::Option.source_name(), &[P::Scalar(ScalarTarget::I64)])),
            MethodName::EndsWith => StringEndsWith: (1, P::Bool),
            MethodName::Replace => StringReplace: (2, P::String),
            MethodName::TrimStart => StringTrimStart: (0, P::String),
            MethodName::TrimEnd => StringTrimEnd: (0, P::String),
            MethodName::ToLower => StringToLower: (0, P::String),
            MethodName::ToUpper => StringToUpper: (0, P::String),
            MethodName::Trim => StringTrim: (0, P::String),
            MethodName::Repeat => StringRepeat: (1, P::String),
            MethodName::StartsWith => StringStartsWith: (1, P::Bool),
        );
    }
    if matches!(recv, Type::Char | Type::U64)
        && matches!(name, MethodName::ToString | MethodName::AbiDebug)
    {
        return Some(select(ScalarToString, 0, SignatureReturn::Fixed(P::String)));
    }
    if recv.integer_layout().is_some() {
        return methods!(
            MethodName::ToString | MethodName::AbiDebug => I64ToString: (0, P::String),
            MethodName::WrappingAdd => I64WrappingAdd: (1, P::Borrowed(recv)),
            MethodName::WrappingSub => I64WrappingSub: (1, P::Borrowed(recv)),
            MethodName::WrappingMul => I64WrappingMul: (1, P::Borrowed(recv)),
            MethodName::WrappingNeg => I64WrappingNeg: (0, P::Borrowed(recv)),
            MethodName::CheckedAdd => I64CheckedAdd: (1, P::Generic(B::Option.source_name(), &[P::Borrowed(recv)])),
            MethodName::CheckedSub => I64CheckedSub: (1, P::Generic(B::Option.source_name(), &[P::Borrowed(recv)])),
            MethodName::CheckedMul => I64CheckedMul: (1, P::Generic(B::Option.source_name(), &[P::Borrowed(recv)])),
            MethodName::CheckedNeg => I64CheckedNeg: (0, P::Generic(B::Option.source_name(), &[P::Borrowed(recv)])),
        );
    }
    match recv {
        Type::Bool => {
            return methods!(MethodName::ToString | MethodName::AbiDebug => BoolToString: (0, P::String));
        }
        Type::F64 => {
            return methods!(MethodName::ToString | MethodName::AbiDebug => F64ToString: (0, P::String));
        }
        Type::Array(elem) | Type::FixedArray(elem, _) => {
            return methods!(
                MethodName::AbiConcat => StringConcatArray if (matches!(**elem, Type::String)): (0, P::String),
                MethodName::AbiLen => ArrayLen: (0, P::Scalar(ScalarTarget::I64)),
                MethodName::AbiResize => ArrayResize if (matches!(recv, Type::Array(_))): (1, P::Void),
                MethodName::AbiCopy => ArrayCopy: (0, P::Generic(B::FrozenArray.source_name(), &[P::Borrowed(elem)])),
            );
        }
        _ => {}
    }

    let family = builtin_types::resolve(recv)?;
    match (family.id, family.args) {
        (B::AtomicI64 | B::AtomicBool, []) => {
            let word = if family.id == B::AtomicI64 {
                P::Scalar(ScalarTarget::I64)
            } else {
                P::Bool
            };
            methods!(
                MethodName::Load => AtomicLoad: (0, word),
                MethodName::Store => AtomicStore: (1, P::Void),
                MethodName::Swap => AtomicSwap: (1, word),
                MethodName::CompareExchange => AtomicCompareExchange: (2, P::Bool),
                MethodName::Add => AtomicAdd if (family.id == B::AtomicI64): (1, word),
                MethodName::Sub => AtomicSub if (family.id == B::AtomicI64): (1, word),
            )
        }
        (B::CancellationToken, []) => {
            if name == MethodName::Attach {
                return Some(select(TokenAttach, 1, SignatureReturn::SameAsArg(0)));
            }
            methods!(
                MethodName::Cancel => TokenCancel: (0, P::Void),
                MethodName::IsCancelled => TokenIsCancelled: (0, P::Bool),
                MethodName::Child => TokenChild: (0, P::Named(B::CancellationToken)),
            )
        }
        (B::TaskScope, []) => {
            if name == MethodName::Add {
                return Some(select(ScopeAdd, 1, SignatureReturn::SameAsArg(0)));
            }
            methods!(
                MethodName::Cancel => ScopeCancel: (0, P::Void),
                MethodName::IsCancelled => ScopeIsCancelled: (0, P::Bool),
                MethodName::Child => ScopeChild: (0, P::Named(B::TaskScope)),
                MethodName::Finish => ScopeFinish: (0, P::Generic(B::Task.source_name(), &[P::Generic(B::Result.source_name(), &[P::Void, P::Named(B::Cancelled)])])),
            )
        }
        (B::Task | B::JoinHandle, [item]) => methods!(
            MethodName::Cancel => TaskCancel: (0, P::Void),
            MethodName::IsCancelled => TaskIsCancelled: (0, P::Bool),
            MethodName::Result => TaskResult: (0, P::Generic(B::TaskResult.source_name(), &[P::Borrowed(item)])),
        ),
        (B::BlockingCell, [item]) => methods!(
            MethodName::Get => CellGet: (0, P::Borrowed(item)),
            MethodName::Set => CellSet: (1, P::Void),
        ),
        (B::BlockingRwCell, [item]) => methods!(
            MethodName::Read => RwCellRead: (0, P::Borrowed(item)),
            MethodName::Write => RwCellWrite: (1, P::Void),
        ),
        (B::Channel, [item]) => {
            let id = name.channel()?;
            let optional_item = [P::Borrowed(item)];
            let (arity, result) = match id {
                ChannelSend => (1, P::Void),
                ChannelRecv => (0, P::Borrowed(item)),
                ChannelRecvOpt => (0, P::Generic(B::Option.source_name(), &optional_item)),
                ChannelClose => (0, P::Void),
                _ => return None,
            };
            Some(select(id, arity, SignatureReturn::Fixed(result)))
        }
        (B::FrozenArray, [_]) => {
            methods!(MethodName::Len => FrozenArrayLen: (0, P::Scalar(ScalarTarget::I64)))
        }
        (B::Map, [k, v]) => methods!(
            MethodName::Insert => MapInsert: (2, P::Void),
            MethodName::Remove => MapRemove: (1, P::Generic(B::Option.source_name(), &[P::Borrowed(v)])),
            MethodName::Keys => MapKeys: (0, P::Array(&P::Borrowed(k))),
            MethodName::Values => MapValues: (0, P::Array(&P::Borrowed(v))),
            MethodName::Entries => MapEntries: (0, P::Array(&P::Generic("$Tuple2", &[P::Borrowed(k), P::Borrowed(v)]))),
            MethodName::Get => MapGet: (1, P::Generic(B::Option.source_name(), &[P::Borrowed(v)])),
            MethodName::Contains => MapContains: (1, P::Bool),
            MethodName::Len => MapLen: (0, P::Scalar(ScalarTarget::I64)),
            MethodName::ToString => MapToString: (0, P::String),
            MethodName::Freeze => MapFreeze: (0, P::Generic(B::FrozenMap.source_name(), &[P::Borrowed(k), P::Borrowed(v)])),
        ),
        (B::FrozenMap, [_, v]) => methods!(
            MethodName::Get => FrozenMapGet: (1, P::Generic(B::Option.source_name(), &[P::Borrowed(v)])),
            MethodName::Contains => FrozenMapContains: (1, P::Bool),
            MethodName::Len => FrozenMapLen: (0, P::Scalar(ScalarTarget::I64)),
        ),
        _ => None,
    }
}

/// Convenience wrapper for callers that only want the intrinsic identity.
pub fn resolve_intrinsic<N: builtin_types::TypeName>(
    recv: &Type<N>,
    method: &str,
    arity: usize,
) -> Option<Intrinsic> {
    match MethodName::classify(method)
        .lookup_with(recv, |id, expected, _| (expected == arity).then_some(id))
    {
        Lookup::Intrinsic(id) => id,
        _ => None,
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod checker_agreement_tests;

/// The zero-argument runtime statistic builtins: `() -> i64` reads of a runtime
/// counter. Their ABI entries are all `NONE; ([] -> Some(I64))`, so LIR
/// emission needs only the runtime call.
///
/// Derived from `builtin_call_runtime_name` rather than repeating its list:
/// Collection and task-frame diagnostic calls return void, and
/// `sleep` / `yield` are futures, not reads.
pub(crate) fn runtime_stat_builtin_runtime_name(callee: &str) -> Option<&'static str> {
    if matches!(
        callee,
        "gc_collect" | "gc_minor_collect" | "gc_task_frame_diagnostics"
    ) {
        return None;
    }
    callee
        .strip_prefix("gc_")
        .or_else(|| callee.strip_prefix("sched_"))?;
    builtin_call_runtime_name(callee)
}

pub(crate) fn builtin_call_runtime_name(callee: &str) -> Option<&'static str> {
    match callee {
        "gc_collect" => Some(rt::willow_gc_collect),
        "gc_minor_collect" => Some(rt::willow_gc_minor_collect),
        "sched_local_pop_hits" => Some(rt::willow_sched_local_pop_hits),
        "sched_global_pop_hits" => Some(rt::willow_sched_global_pop_hits),
        "sched_global_pop_attempts" => Some(rt::willow_sched_global_pop_attempts),
        "sched_steal_attempts" => Some(rt::willow_sched_steal_attempts),
        "sched_steal_successes" => Some(rt::willow_sched_steal_successes),
        "sched_steal_failures" => Some(rt::willow_sched_steal_failures),
        "sched_victim_locks" => Some(rt::willow_sched_victim_locks),
        "sched_global_pushes" => Some(rt::willow_sched_global_pushes),
        "sched_local_pushes" => Some(rt::willow_sched_local_pushes),
        "gc_task_frame_diagnostics" => Some(rt::willow_gc_task_frame_diagnostics),
        "gc_allocated_bytes" => Some(rt::willow_gc_allocated_bytes),
        "gc_total_allocated_bytes" => Some(rt::willow_gc_total_allocated_bytes),
        "gc_tlab_fast_allocations" => Some(rt::willow_gc_tlab_fast_allocations),
        "gc_tlab_slow_allocations" => Some(rt::willow_gc_tlab_slow_allocations),
        "gc_tlab_refills" => Some(rt::willow_gc_tlab_refills),
        "gc_tlab_large_allocations" => Some(rt::willow_gc_tlab_large_allocations),
        "gc_tlab_reserved_bytes" => Some(rt::willow_gc_tlab_reserved_bytes),
        "gc_minor_collections" => Some(rt::willow_gc_minor_collections),
        "gc_promoted_objects" => Some(rt::willow_gc_promoted_objects),
        "gc_moved_objects" => Some(rt::willow_gc_moved_objects),
        "gc_pause_count" => Some(rt::willow_gc_pause_count),
        "gc_pause_total_ns" => Some(rt::willow_gc_pause_total_ns),
        "gc_pause_max_ns" => Some(rt::willow_gc_pause_max_ns),
        "gc_survivor_copies" => Some(rt::willow_gc_survivor_copies),
        "gc_survivor_bytes" => Some(rt::willow_gc_survivor_bytes),
        "gc_tenured_objects" => Some(rt::willow_gc_tenured_objects),
        "gc_tenured_bytes" => Some(rt::willow_gc_tenured_bytes),
        "gc_pinned_promotions" => Some(rt::willow_gc_pinned_promotions),
        "gc_survivor_space_reserved" => Some(rt::willow_gc_survivor_space_reserved),
        "gc_survivor_space_live" => Some(rt::willow_gc_survivor_space_live),
        "gc_remembered_set_size" => Some(rt::willow_gc_remembered_set_size),
        "gc_dirty_card_count" => Some(rt::willow_gc_dirty_card_count),
        "gc_write_barrier_hits" => Some(rt::willow_gc_write_barrier_hits),
        "gc_old_region_count" => Some(rt::willow_gc_old_region_count),
        "gc_old_region_reserved_bytes" => Some(rt::willow_gc_old_region_reserved_bytes),
        "gc_old_region_live_bytes" => Some(rt::willow_gc_old_region_live_bytes),
        "gc_old_region_fragmentation_bytes" => Some(rt::willow_gc_old_region_fragmentation_bytes),
        "gc_large_object_region_count" => Some(rt::willow_gc_large_object_region_count),
        "gc_pinned_region_count" => Some(rt::willow_gc_pinned_region_count),
        "gc_old_region_allocations" => Some(rt::willow_gc_old_region_allocations),
        "gc_old_region_reuses" => Some(rt::willow_gc_old_region_reuses),
        "gc_old_regions_released" => Some(rt::willow_gc_old_regions_released),
        "gc_major_collections" => Some(rt::willow_gc_major_collections),
        "sleep" => Some(rt::willow_timer_value_sleep),
        "yield" => Some(rt::willow_timer_value_yield),
        _ => None,
    }
}

pub(crate) struct NamespaceBuiltin {
    pub runtime: &'static str,
    pub params: Vec<crate::semantic::ids::SemanticType>,
    pub ret: crate::semantic::ids::SemanticType,
    /// `fs::exists` is the one entry whose runtime returns a full word for a
    /// `bool` result; emission narrows that word to a boolean.
    pub narrow_to_bool: bool,
}

pub(crate) fn math_namespace_builtin(class: &str, method: &str) -> Option<NamespaceBuiltin> {
    use crate::semantic::ids::SemanticType as Type;
    let math = match (class, method) {
        ("i64", "abs") => Some((rt::willow_i64_abs, Type::I64, 1)),
        ("i64", "min") => Some((rt::willow_i64_min, Type::I64, 2)),
        ("i64", "max") => Some((rt::willow_i64_max, Type::I64, 2)),
        ("i64", "clamp") => Some((rt::willow_i64_clamp, Type::I64, 3)),
        ("f64", "abs") => Some((rt::willow_f64_abs, Type::F64, 1)),
        ("f64", "min") => Some((rt::willow_f64_min, Type::F64, 2)),
        ("f64", "max") => Some((rt::willow_f64_max, Type::F64, 2)),
        ("f64", "clamp") => Some((rt::willow_f64_clamp, Type::F64, 3)),
        ("f64", "sqrt") => Some((rt::willow_f64_sqrt, Type::F64, 1)),
        ("f64", "floor") => Some((rt::willow_f64_floor, Type::F64, 1)),
        ("f64", "ceil") => Some((rt::willow_f64_ceil, Type::F64, 1)),
        ("f64", "round") => Some((rt::willow_f64_round, Type::F64, 1)),
        _ => None,
    };
    if let Some((runtime, ty, arity)) = math {
        return Some(NamespaceBuiltin {
            runtime,
            params: vec![ty.clone(); arity],
            ret: ty,
            narrow_to_bool: false,
        });
    }
    None
}

pub(crate) fn namespace_builtin(class: &str, method: &str) -> Option<NamespaceBuiltin> {
    use crate::semantic::ids::SemanticType as Type;
    use crate::stdlib_schema::{self, StdItemKind};
    if let Some(entry) = math_namespace_builtin(class, method) {
        return Some(entry);
    }
    if class == "i64" && method == "parse" {
        return Some(NamespaceBuiltin {
            runtime: rt::willow_i64_parse,
            params: vec![Type::String],
            ret: Type::Generic(
                "Result".to_string().into(),
                vec![Type::I64, Type::Named("ParseIntError".to_string().into())],
            ),
            narrow_to_bool: false,
        });
    }
    if class == "f64" {
        return match method {
            "from_i64" => Some(NamespaceBuiltin {
                runtime: rt::willow_f64_from_i64,
                params: vec![Type::I64],
                ret: Type::F64,
                narrow_to_bool: false,
            }),
            "trunc_to_i64" => Some(NamespaceBuiltin {
                runtime: rt::willow_f64_trunc_to_i64,
                params: vec![Type::F64],
                ret: Type::I64,
                narrow_to_bool: false,
            }),
            "to_string" => Some(NamespaceBuiltin {
                runtime: rt::willow_f64_to_string,
                params: vec![Type::F64],
                ret: Type::String,
                narrow_to_bool: false,
            }),
            "parse" => Some(NamespaceBuiltin {
                runtime: rt::willow_f64_parse,
                params: vec![Type::String],
                ret: Type::Generic(
                    "Result".to_string().into(),
                    vec![Type::F64, Type::Named("ParseFloatError".to_string().into())],
                ),
                narrow_to_bool: false,
            }),
            _ => None,
        };
    }
    let runtime = match (class, method) {
        ("text", "from_scalar") => rt::willow_string_from_scalar,
        ("time", "now_ns") => rt::willow_time_now_ns,
        ("env", "args_len") => rt::willow_runtime_args_len,
        ("env", "args") => rt::willow_runtime_args_array,
        ("env", "program_name") => rt::willow_runtime_program_name,
        ("env", "arg") => rt::willow_runtime_arg,
        ("env", "var") => rt::willow_env_var,

        ("path", "join") => rt::willow_path_join,
        ("path", "parent") => rt::willow_path_parent,
        ("path", "file_name") => rt::willow_path_file_name,
        ("path", "extension") => rt::willow_path_extension,
        ("path", "file_stem") => rt::willow_path_file_stem,
        ("path", "is_absolute") => rt::willow_path_is_absolute,

        ("fs", "temp_path") => rt::willow_fs_temp_path,
        ("fs", "read_to_string") => rt::willow_fs_read_to_string,
        ("fs", "write_string") => rt::willow_fs_write_string,
        ("fs", "exists") => rt::willow_fs_exists,
        ("fs", "remove_file") => rt::willow_fs_remove_file,
        ("fs", "read_to_string_async") => rt::willow_fs_read_to_string_async,
        ("fs", "write_string_async") => rt::willow_fs_write_string_async,
        ("fs", "exists_async") => rt::willow_fs_exists_async,
        ("fs", "remove_file_async") => rt::willow_fs_remove_file_async,

        // `parallel::map(frozen, f)`. Its two arguments are
        // the only ones in this table that are not scalars or strings: a
        // `FrozenArray<i64>`, which is rooted like any other heap handle, and a
        // FUNCTION VALUE, which is a bare code address and so is deliberately
        // not. The runtime owns the chunking, so from here it is one call.
        ("parallel", "map") => rt::willow_parallel_map_i64,

        ("net", "bind") => rt::willow_net_bind,
        ("net", "local_addr") => rt::willow_net_local_addr,
        ("net", "peer_addr") => rt::willow_net_peer_addr,
        ("net", "shutdown") => rt::willow_net_shutdown,
        ("net", "connect_async") => rt::willow_net_connect_async,
        ("net", "accept_async") => rt::willow_net_accept_async,
        ("net", "read_bytes_async") => rt::willow_net_read_bytes_async,
        ("net", "write_bytes_async") => rt::willow_net_write_bytes_async,
        ("net", "read_async") => rt::willow_net_read_async,
        ("net", "write_async") => rt::willow_net_write_async,
        _ => return None,
    };
    let StdItemKind::Function {
        params,
        return_type,
    } = stdlib_schema::item(class, method)?.kind
    else {
        return None;
    };
    Some(NamespaceBuiltin {
        runtime,
        params: params
            .iter()
            .map(|ty| ty.to_ast_type().map(Into::into))
            .collect::<Option<Vec<_>>>()?,
        ret: return_type.to_ast_type()?.into(),
        narrow_to_bool: matches!((class, method), ("fs", "exists") | ("path", "is_absolute")),
    })
}

/// Destination encoded by a scalar conversion or parse member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScalarTarget {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    Char,
}

impl ScalarTarget {
    fn from_suffix(suffix: &str) -> Option<Self> {
        #[cfg(test)]
        SCALAR_TARGET_PARSES.with(|count| count.set(count.get() + 1));
        Some(match suffix {
            "i8" => Self::I8,
            "i16" => Self::I16,
            "i32" => Self::I32,
            "i64" => Self::I64,
            "u8" => Self::U8,
            "u16" => Self::U16,
            "u32" => Self::U32,
            "u64" => Self::U64,
            "char" => Self::Char,
            _ => return None,
        })
    }

    fn ty<N>(self) -> Type<N> {
        match self {
            Self::I8 => Type::I8,
            Self::I16 => Type::I16,
            Self::I32 => Type::I32,
            Self::I64 => Type::I64,
            Self::U8 => Type::U8,
            Self::U16 => Type::U16,
            Self::U32 => Type::U32,
            Self::U64 => Type::U64,
            Self::Char => Type::Char,
        }
    }
}
