//! Flat value operations. Operands refer to locals or immediate constants;
//! evaluation order is represented by the surrounding instruction sequence.
use super::{LirLocal, LirLocalId, SourceBlock, SourceInst, SourceTerminator};
use crate::diagnostics::Span;
use crate::ir::typed_ast::{HirExpr, HirExprKind};
use crate::parser::ast::{BinOp, ExprId, UnaryOp};
use crate::semantic::builtin_types::{self, BuiltinTypeId as Builtin};
use crate::semantic::ids::{FunctionId, SemanticType as Type, TypeId};
use std::collections::HashMap;

// JSON numbers cannot preserve every IEEE-754 value. Store the payload bits,
// including signed zero, infinities, and distinct NaN payloads.
mod float_bits {
    use serde::{Deserialize, Serialize};
    pub fn serialize<S: serde::Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
        value.to_bits().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
        u64::deserialize(deserializer).map(f64::from_bits)
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LirOperand {
    Local(LirLocalId),
    Int(i64),
    Integer {
        value: i64,
        ty: Type,
    },
    Float(#[serde(with = "float_bits")] f64),
    Bool(bool),
    Reference {
        place: LirPlace,
        span: Span,
        display: String,
    },
}

/// Captured storage identity, independent of subsequent array resizing.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LirPlace {
    Local(LirLocalId),
    Field {
        object: LirLocalId,
        object_ty: Type,
        field: String,
        ty: Type,
    },
    ArrayElement {
        owner: LirLocalId,
        index: LirLocalId,
        element: Type,
    },
}

impl LirPlace {
    fn base_and_index(&self) -> (LirLocalId, Option<LirLocalId>) {
        match self {
            Self::Local(local) => (*local, None),
            Self::Field { object, .. } => (*object, None),
            Self::ArrayElement { owner, index, .. } => (*owner, Some(*index)),
        }
    }

    pub fn locals(&self) -> Vec<LirLocalId> {
        let (base, index) = self.base_and_index();
        std::iter::once(base).chain(index).collect()
    }
    pub fn ty(&self, locals: &[LirLocal]) -> Option<Type> {
        match self {
            Self::Local(local) => locals
                .get(local.0 as usize)
                .filter(|local| !local.is_gc_owner())
                .map(|local| local.ty.clone()),
            Self::Field {
                object,
                object_ty,
                ty,
                ..
            } => locals
                .get(object.0 as usize)
                .filter(|local| !local.is_gc_owner() && local.ty == *object_ty)
                .map(|_| ty.clone()),
            Self::ArrayElement {
                owner,
                index,
                element,
            } => (locals
                .get(owner.0 as usize)
                .is_some_and(LirLocal::is_gc_owner)
                && locals
                    .get(index.0 as usize)
                    .is_some_and(|local| !local.is_gc_owner() && local.ty == Type::I64))
            .then(|| element.clone()),
        }
    }
}

impl LirOperand {
    /// True for direct values that do not capture a storage location.
    pub(crate) fn is_plain_value(&self) -> bool {
        match self {
            Self::Local(_)
            | Self::Int(_)
            | Self::Integer { .. }
            | Self::Float(_)
            | Self::Bool(_) => true,
            Self::Reference { .. } => false,
        }
    }

    /// The local whose own slot supplies the borrowed storage, if any.
    pub(crate) fn borrowed_local(&self) -> Option<LirLocalId> {
        match self {
            Self::Reference { place, .. } => match place {
                LirPlace::Local(local) => Some(*local),
                LirPlace::Field { .. } | LirPlace::ArrayElement { .. } => None,
            },
            Self::Local(_)
            | Self::Int(_)
            | Self::Integer { .. }
            | Self::Float(_)
            | Self::Bool(_) => None,
        }
    }

    /// Read an immediate or resolve a local without lowering an expression tree.
    pub(in crate::ir) fn from_flat_expr(
        expr: &HirExpr,
        local: impl FnOnce(&str) -> Option<LirLocalId>,
    ) -> Option<Self> {
        match &expr.kind {
            HirExprKind::Var(name) => local(name).map(Self::Local),
            HirExprKind::Int(value) => Some(Self::integer(*value, &expr.ty)),
            HirExprKind::Float(value) => Some(Self::Float(*value)),
            HirExprKind::Bool(value) => Some(Self::Bool(*value)),
            ir_variants!(
                HirExprKind;
                Str, FnRef, Binary, Unary, Call, Invoke, Print, Array, Index, Ternary, New,
                FieldAccess, MethodCall, ObjectLiteral, StaticField, StaticCall, ReferenceArg,
                Range, Await, TryPropagate, Lambda, Match, Select
            ) => None,
        }
    }

    pub(crate) fn integer_value(&self) -> Option<i64> {
        match self {
            Self::Int(n) | Self::Integer { value: n, .. } => Some(*n),
            Self::Local(_) | Self::Float(_) | Self::Bool(_) | Self::Reference { .. } => None,
        }
    }
    pub(crate) fn integer(value: i64, ty: &Type) -> Self {
        if *ty == Type::I64 {
            Self::Int(value)
        } else {
            Self::Integer {
                value,
                ty: ty.clone(),
            }
        }
    }
    pub(crate) fn iter_locals(&self) -> impl Iterator<Item = LirLocalId> {
        let (first, second) = match self {
            Self::Local(local) => (Some(*local), None),
            Self::Reference { place, .. } => {
                let (base, index) = place.base_and_index();
                (Some(base), index)
            }
            Self::Int(_) | Self::Integer { .. } | Self::Float(_) | Self::Bool(_) => (None, None),
        };
        first.into_iter().chain(second)
    }

    pub fn locals(&self) -> Vec<LirLocalId> {
        self.iter_locals().collect()
    }
    fn plain_type<'a>(&'a self, locals: &'a [LirLocal]) -> Option<&'a Type> {
        static I64: Type = Type::I64;
        static F64: Type = Type::F64;
        static BOOL: Type = Type::Bool;
        match self {
            Self::Local(id) => locals
                .get(id.0 as usize)
                .filter(|local| !local.is_gc_owner())
                .map(|local| &local.ty),
            Self::Integer { value, ty } => {
                let valid = if *ty == Type::Char {
                    u32::try_from(*value)
                        .ok()
                        .and_then(char::from_u32)
                        .is_some()
                } else {
                    ty.integer_layout()
                        .is_some_and(|layout| layout.contains(layout.value(*value)))
                };
                valid.then_some(ty)
            }
            Self::Int(_) => Some(&I64),
            Self::Float(_) => Some(&F64),
            Self::Bool(_) => Some(&BOOL),
            Self::Reference { .. } => None,
        }
    }

    pub fn ty(&self, locals: &[LirLocal]) -> Option<Type> {
        match self {
            Self::Reference { place, .. } => place.ty(locals),
            Self::Int(_) => Some(Type::I64),
            Self::Float(_) => Some(Type::F64),
            Self::Bool(_) => Some(Type::Bool),
            Self::Integer { .. } | Self::Local(_) => self.plain_type(locals).cloned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LirRvalue {
    BeginReferenceCall,
    Use(LirOperand),
    AwaitFuture {
        future: LirOperand,
        result: Type,
    },
    StartTask {
        callee: FunctionId,
        args: Vec<LirOperand>,
        params: Vec<Type>,
        output: Type,
    },
    SelectIdleWait {
        deadlines: Vec<LirOperand>,
        quiescent: LirOperand,
    },
    Coerce {
        value: LirOperand,
        source: Type,
        target: Type,
    },
    ArrayAlloc {
        length: usize,
        element: Type,
    },
    CaptureArrayOwner {
        array: LirOperand,
        index: LirOperand,
    },
    ArrayStore {
        array: LirOperand,
        index: LirOperand,
        value: LirOperand,
        element: Type,
    },
    Index {
        array: LirOperand,
        index: LirOperand,
        element: Type,
    },
    ObjectAlloc {
        class: TypeId,
    },
    FieldLoad {
        object: LirOperand,
        object_ty: Type,
        field: String,
        result: Type,
    },
    FieldStore {
        object: LirOperand,
        object_ty: Type,
        field: String,
        value: LirOperand,
    },
    StaticField {
        class: TypeId,
        field: String,
        result: Type,
    },
    StaticStore {
        class: TypeId,
        field: String,
        value: LirOperand,
    },
    StaticCall {
        class: TypeId,
        method: String,
        is_async: bool,
        args: Vec<LirOperand>,
        arg_types: Vec<Type>,
        result: Type,
    },
    EnumAlloc {
        class: TypeId,
        variant: String,
        enum_ty: Type,
    },
    EnumPayloadStore {
        object: LirOperand,
        class: TypeId,
        variant: String,
        index: usize,
        value: LirOperand,
        source: Type,
        enum_ty: Type,
    },
    ConstructorCall {
        object: LirOperand,
        class: TypeId,
        args: Vec<LirOperand>,
        arg_types: Vec<Type>,
    },
    Range {
        start: LirOperand,
        end: LirOperand,
    },
    EnumMethod {
        receiver: LirOperand,
        receiver_ty: Type,
        method: crate::semantic::intrinsics::AdtMethod,
        args: Vec<LirOperand>,
        arg_types: Vec<Type>,
        result: Type,
    },
    BuiltinCall {
        callee: FunctionId,
        args: Vec<LirOperand>,
        params: Vec<Type>,
        result: Type,
    },
    FormatScalar {
        value: LirOperand,
        ty: Type,
        format: Option<crate::interpolate::F64Format>,
    },
    Panic {
        message: LirOperand,
    },
    Recover,
    ReferenceDebug {
        argument: LirOperand,
        callee: FunctionId,
        index: usize,
    },
    RebindResultError {
        value: LirOperand,
        source: Type,
        target: Type,
    },
    IntoError {
        value: LirOperand,
        source: Type,
        target: Type,
    },
    PrepareMethod {
        receiver: LirOperand,
        receiver_ty: Type,
        method: String,
    },
    MethodCall {
        receiver: LirOperand,
        receiver_ty: Type,
        method: String,
        /// Can dispatch to an async declaration (including an override).
        /// A synchronous Task-returning method alone does not set this bit.
        is_async: bool,
        args: Vec<LirOperand>,
        arg_types: Vec<Type>,
        result: Type,
        /// `Some(base)` for `super.method(...)`: call `base`'s resolved
        /// implementation directly instead of dispatching on the receiver.
        super_class: Option<TypeId>,
    },
    StringLiteral(String),
    FunctionRef {
        function: FunctionId,
        ty: Type,
    },
    Closure {
        id: ExprId,
        captures: Vec<LirOperand>,
        ty: Type,
    },
    Print {
        value: LirOperand,
        ty: Type,
        newline: bool,
    },
    IntrinsicCall {
        intrinsic: crate::semantic::intrinsics::Intrinsic,
        method: crate::parser::ast::MethodSpelling,
        receiver: LirOperand,
        receiver_ty: Type,
        args: Vec<LirOperand>,
        arg_types: Vec<Type>,
        result: Type,
    },
    DirectCall {
        callee: FunctionId,
        args: Vec<LirOperand>,
        params: Vec<Type>,
        result: Type,
    },
    IndirectCall {
        callee: LirOperand,
        name: FunctionId,
        args: Vec<LirOperand>,
        params: Vec<Type>,
        result: Type,
    },
    Unary {
        op: UnaryOp,
        operand: LirOperand,
        ty: Type,
    },
    Binary {
        op: BinOp,
        lhs: LirOperand,
        rhs: LirOperand,
        operand_ty: Type,
    },
    /// Concatenation of `parts` in order, built by one size-summed runtime
    /// allocation. `part_types[i]` is `String`, or `I64` for
    /// a value written in decimal as by `i64.to_string()`.
    StringConcat {
        parts: Vec<LirOperand>,
        part_types: Vec<Type>,
    },
}

/// Integer operators that debug builds check for signed overflow. Release builds wrap them;
/// `**` additionally keeps its
/// negative-exponent guard in both modes. A shift checks its amount against
/// `0..64`, so a literal amount in range needs no check;
/// release builds mask the amount to its low six bits.
pub fn is_overflow_checked_binary(op: &BinOp, rhs: &LirOperand, operand_ty: &Type) -> bool {
    operand_ty.integer_layout().is_some()
        && match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Pow => true,
            BinOp::Shl | BinOp::Shr => !rhs.integer_value().is_some_and(|n| {
                (0..i64::from(operand_ty.integer_layout().unwrap().bits)).contains(&n)
            }),
            _ => false,
        }
}

/// Integer negation faults only for `i64::MIN`, so negating any other constant
/// needs no check. A folded `i64::MIN` constant operand keeps the check.
pub fn is_overflow_checked_unary(op: &UnaryOp, operand: &LirOperand, ty: &Type) -> bool {
    *op == UnaryOp::Neg
        && ty.integer_layout().is_some()
        && !operand.integer_value().is_some_and(|n| {
            let layout = ty.integer_layout().unwrap();
            layout.contains(-layout.value(n))
        })
}

impl LirRvalue {
    /// Whether the resolved policy gives this operation an overflow fault path.
    pub(crate) fn has_overflow_check_with(&self, checks: crate::driver::OverflowChecks) -> bool {
        let enabled = match self {
            Self::Binary {
                op: BinOp::Shl | BinOp::Shr,
                ..
            } => checks.other,
            Self::Binary { operand_ty: ty, .. } | Self::Unary { ty, .. } => ty
                .integer_layout()
                .is_some_and(|layout| checks.arithmetic(layout)),
            _ => false,
        };
        enabled && self.has_overflow_check()
    }

    /// Whether debug overflow checks give this operation a fault path.
    pub fn has_overflow_check(&self) -> bool {
        match self {
            Self::Binary {
                op,
                rhs,
                operand_ty,
                ..
            } => is_overflow_checked_binary(op, rhs, operand_ty),
            Self::Unary { op, operand, ty } => is_overflow_checked_unary(op, operand, ty),
            ir_variants!(
                Self;
                BeginReferenceCall, Use, AwaitFuture, StartTask, SelectIdleWait, Coerce,
                ArrayAlloc, CaptureArrayOwner, ArrayStore, Index, ObjectAlloc, FieldLoad,
                FieldStore, StaticField, StaticStore, StaticCall, EnumAlloc, EnumPayloadStore,
                ConstructorCall, Range, EnumMethod, BuiltinCall, FormatScalar, Panic, Recover,
                ReferenceDebug, RebindResultError, IntoError, PrepareMethod, MethodCall,
                StringLiteral, FunctionRef, Closure, Print, IntrinsicCall, DirectCall,
                IndirectCall, StringConcat
            ) => false,
        }
    }

    pub fn is_well_typed(&self, locals: &[LirLocal], destination: LirLocalId) -> bool {
        let Some(destination) = locals.get(destination.0 as usize) else {
            return false;
        };
        // Check the operand kind during type lookup. Calls that can pass a
        // reference check its place type directly in their own arms.
        let plain_ty = |operand| LirOperand::plain_type(operand, locals);
        if let Self::CaptureArrayOwner { array, index } = self {
            return destination.is_gc_owner()
                && matches!(plain_ty(array), Some(Type::Array(_)))
                && plain_ty(index) == Some(&Type::I64);
        }
        if destination.is_gc_owner() {
            return false;
        }
        match self {
            Self::BeginReferenceCall => destination.ty == Type::Void,
            Self::CaptureArrayOwner { .. } => false,
            Self::ReferenceDebug { argument, .. } => destination.ty == Type::Void && matches!(argument, LirOperand::Reference { .. }) && argument.ty(locals).is_some(),
            Self::Use(operand) => plain_ty(operand) == Some(&destination.ty),
            Self::StartTask { args, params, output, .. } => matches!(&destination.ty, Type::Generic(name, payloads) if builtin_types::resolve_name(name) == Some(Builtin::Task) && payloads == std::slice::from_ref(output)) && args.len() == params.len() && args.iter().zip(params).all(|(arg, param)| arg.ty(locals).as_ref() == Some(param)),
            Self::AwaitFuture { future, result } => destination.ty == *result && matches!(plain_ty(future), Some(Type::Generic(name, payloads)) if builtin_types::resolve_name(name) == Some(Builtin::Future) && payloads == std::slice::from_ref(result)),
            Self::SelectIdleWait { deadlines, quiescent } => destination.ty == Type::Bool && plain_ty(quiescent) == Some(&Type::Bool) && deadlines.iter().all(|operand| plain_ty(operand) == Some(&Type::I64)),
            Self::Coerce { value, source, target } => plain_ty(value) == Some(source) && destination.ty == *target,
            Self::ArrayAlloc { element, .. } => matches!(&destination.ty, Type::Array(inner) if inner.as_ref() == element),
            Self::ArrayStore { array, index, value, element } => matches!(plain_ty(array), Some(Type::Array(inner)) if inner.as_ref() == element) && plain_ty(index) == Some(&Type::I64) && plain_ty(value).is_some() && destination.ty == Type::Void,
            Self::Index { array, index, element } => plain_ty(index) == Some(&Type::I64) && destination.ty == *element && plain_ty(array).is_some_and(|array| match array { Type::Array(inner) => **inner == *element, Type::Generic(name, args) => crate::semantic::builtin_types::resolve_name(name) == Some(crate::semantic::builtin_types::BuiltinTypeId::FrozenArray) && args == std::slice::from_ref(element), _ => false }),
            Self::ObjectAlloc { class } => destination.ty == Type::Named(*class),
            Self::FieldLoad { object, object_ty, result, .. } => plain_ty(object) == Some(object_ty) && destination.ty == *result,
            Self::FieldStore { object, object_ty, value, .. } => plain_ty(object) == Some(object_ty) && plain_ty(value).is_some() && destination.ty == Type::Void,
            Self::StaticField { result, .. } => destination.ty == *result,
            Self::StaticStore { value, .. } => plain_ty(value).is_some() && destination.ty == Type::Void,
            Self::ConstructorCall { object, class, args, arg_types } => plain_ty(object) == Some(&Type::Named(*class)) && destination.ty == Type::Void && args.len() == arg_types.len() && args.iter().zip(arg_types).all(|(arg, expected)| arg.ty(locals).as_ref() == Some(expected)),
            Self::Range { start, end } => plain_ty(start) == Some(&Type::I64) && plain_ty(end) == Some(&Type::I64) && matches!(&destination.ty, Type::Generic(name, payloads) if builtin_types::resolve_name(name) == Some(Builtin::Range) && payloads == &[Type::I64]),
            Self::EnumMethod { receiver, receiver_ty, args, arg_types, result, .. } => plain_ty(receiver) == Some(receiver_ty) && destination.ty == *result && args.len() == arg_types.len() && args.iter().zip(arg_types).all(|(arg, expected)| plain_ty(arg) == Some(expected)),
            Self::BuiltinCall { args, params, result, .. } => destination.ty == *result && args.len() == params.len() && args.iter().zip(params).all(|(arg, expected)| plain_ty(arg) == Some(expected)),
            Self::StaticCall { args, arg_types, result, .. } => destination.ty == *result && args.len() == arg_types.len() && args.iter().zip(arg_types).all(|(arg, expected)| arg.ty(locals).as_ref() == Some(expected)),
            Self::EnumAlloc { enum_ty, .. } => destination.ty == *enum_ty,
            Self::EnumPayloadStore { object, value, source, enum_ty, .. } => plain_ty(object) == Some(enum_ty) && plain_ty(value) == Some(source) && destination.ty == *enum_ty,
            Self::FormatScalar { value, ty: expected, format } => plain_ty(value) == Some(expected) && destination.ty == Type::String && if format.is_some() { *expected == Type::F64 } else { matches!(expected, Type::I8 | Type::I16 | Type::I32 | Type::U16 | Type::U32 | Type::U64 | Type::Char | Type::U8 | Type::I64 | Type::F64 | Type::Bool | Type::String) },
            Self::Panic { message } => plain_ty(message) == Some(&Type::String) && destination.ty == Type::Never,
            Self::Recover => destination.ty == Type::Generic(TypeId::local(Builtin::Option.source_name()), vec![Type::Named(TypeId::local("PanicInfo"))]),
            Self::RebindResultError { value, source, target } | Self::IntoError { value, source, target } => plain_ty(value) == Some(source) && destination.ty == *target,
            Self::PrepareMethod { receiver, receiver_ty, .. } => plain_ty(receiver) == Some(receiver_ty) && destination.ty == *receiver_ty,
            Self::MethodCall { receiver, receiver_ty, args, arg_types, result, .. } => plain_ty(receiver) == Some(receiver_ty) && destination.ty == *result && args.len() == arg_types.len() && args.iter().zip(arg_types).all(|(arg, expected)| arg.ty(locals).as_ref() == Some(expected)),
            Self::IntrinsicCall { intrinsic, method, receiver, receiver_ty, args, arg_types, result } => {
                plain_ty(receiver) == Some(receiver_ty) && destination.ty == *result
                    && args.len() == arg_types.len() && args.iter().zip(arg_types).all(|(arg, expected)| plain_ty(arg) == Some(expected))
                    && crate::semantic::intrinsics::matches_signature(receiver_ty, method.kind(), *intrinsic, arg_types, result)
            }
            Self::StringLiteral(_) => destination.ty == Type::String,
            Self::FunctionRef { ty, .. } => destination.ty == *ty && matches!(ty, Type::Fn(..) | Type::Closure(..)),
            Self::Closure { captures, ty: result, .. } => destination.ty == *result
                && matches!(result, Type::Fn(..) | Type::Closure(..)) && captures.iter().all(|capture| plain_ty(capture).is_some()),
            Self::Print { value, ty: expected, .. } => destination.ty == Type::Void
                && plain_ty(value) == Some(expected) && matches!(expected, Type::I8 | Type::I16 | Type::I32 | Type::U16 | Type::U32 | Type::U64 | Type::Char | Type::U8 | Type::I64 | Type::F64 | Type::Bool | Type::String),
            Self::DirectCall { args, params, result, .. } => destination.ty == *result
                && args.len() == params.len() && args.iter().zip(params).all(|(arg, param)| arg.ty(locals).as_ref() == Some(param)),
            Self::IndirectCall { callee, args, params, result, .. } => destination.ty == *result
                && plain_ty(callee).is_some_and(|callee| matches!(callee, Type::Fn(expected, output) | Type::Closure(expected, output) if expected == params && **output == *result))
                && args.len() == params.len() && args.iter().zip(params).all(|(arg, param)| plain_ty(arg) == Some(param)),
            Self::Unary {
                op,
                operand,
                ty: expected,
            } => {
                plain_ty(operand) == Some(expected)
                    && destination.ty == *expected
                    && match op {
                        UnaryOp::Neg => expected.integer_layout().is_some() || *expected == Type::F64,
                        UnaryOp::Not => expected.integer_layout().is_some() || *expected == Type::Bool,
                    }
            }
            Self::Binary {
                op,
                lhs,
                rhs,
                operand_ty,
            } => {
                let comparison = matches!(
                    op,
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                );
                let result = if comparison {
                    &Type::Bool
                } else {
                    operand_ty
                };
                plain_ty(lhs) == Some(operand_ty)
                    && plain_ty(rhs) == Some(operand_ty)
                    && &destination.ty == result
                    && !matches!(op, BinOp::And | BinOp::Or)
                    && match operand_ty {
                        ty if ty.integer_layout().is_some() => true,
                        Type::Char => comparison,
                        Type::F64 => !op.is_integer_only(),
                        Type::Bool => matches!(op, BinOp::Eq | BinOp::Ne),
                        Type::String => *op == BinOp::Add || comparison,
                        Type::Named(_) | Type::Generic(_, _) => matches!(op, BinOp::Eq | BinOp::Ne),
                        _ => false,
                    }
            }
            Self::StringConcat { parts, part_types } => {
                destination.ty == Type::String
                    && parts.len() == part_types.len()
                    && parts.iter().zip(part_types).all(|(part, expected)| {
                        matches!(expected, Type::String | Type::I8 | Type::I16 | Type::I32 | Type::U16 | Type::U32 | Type::U64 | Type::Char | Type::U8 | Type::I64)
                            && plain_ty(part) == Some(expected)
                    })
            }
        }
    }

    /// Fixed operands precede any variable-length argument or capture list.
    pub(crate) fn iter_operands(&self) -> impl Iterator<Item = &LirOperand> {
        let (first, second, tail): (_, _, &[LirOperand]) = match self {
            Self::BeginReferenceCall
            | Self::ReferenceDebug { .. }
            | Self::ArrayAlloc { .. }
            | Self::ObjectAlloc { .. }
            | Self::StaticField { .. }
            | Self::Recover
            | Self::EnumAlloc { .. }
            | Self::StringLiteral(_)
            | Self::FunctionRef { .. } => (None, None, &[]),
            Self::AwaitFuture { future: value, .. }
            | Self::Use(value)
            | Self::Coerce { value, .. }
            | Self::StaticStore { value, .. }
            | Self::FormatScalar { value, .. }
            | Self::FieldLoad { object: value, .. }
            | Self::Panic { message: value }
            | Self::RebindResultError { value, .. }
            | Self::IntoError { value, .. }
            | Self::PrepareMethod {
                receiver: value, ..
            }
            | Self::Print { value, .. }
            | Self::Unary { operand: value, .. } => (Some(value), None, &[]),
            Self::CaptureArrayOwner { array, index } | Self::Index { array, index, .. } => {
                (Some(array), Some(index), &[])
            }
            Self::FieldStore { object, value, .. }
            | Self::EnumPayloadStore { object, value, .. } => (Some(object), Some(value), &[]),
            Self::Range { start, end } => (Some(start), Some(end), &[]),
            Self::Binary { lhs, rhs, .. } => (Some(lhs), Some(rhs), &[]),
            Self::ArrayStore {
                array,
                index,
                value,
                ..
            } => (Some(array), Some(index), std::slice::from_ref(value)),
            Self::StartTask { args, .. }
            | Self::BuiltinCall { args, .. }
            | Self::StaticCall { args, .. }
            | Self::DirectCall { args, .. } => (None, None, args),
            Self::SelectIdleWait {
                deadlines,
                quiescent,
            } => (Some(quiescent), None, deadlines),
            Self::Closure { captures, .. } => (None, None, captures),
            Self::StringConcat { parts, .. } => (None, None, parts),
            Self::ConstructorCall { object, args, .. } => (Some(object), None, args),
            Self::EnumMethod { receiver, args, .. }
            | Self::MethodCall { receiver, args, .. }
            | Self::IntrinsicCall { receiver, args, .. } => (Some(receiver), None, args),
            Self::IndirectCall { callee, args, .. } => (Some(callee), None, args),
        };
        first.into_iter().chain(second).chain(tail)
    }

    pub fn operands(&self) -> Vec<&LirOperand> {
        self.iter_operands().collect()
    }
}

mod lower;
pub(super) use lower::{lower_blocks, lower_calls};

#[cfg(test)]
mod tests {
    #[test]
    fn scalar_width_char_artifacts_reject_arithmetic() {
        let value = LirOperand::integer(i64::from(u32::from('λ')), &Type::Char);
        let locals = [value_local(0, Type::Char), value_local(1, Type::Bool)];
        for op in [UnaryOp::Neg, UnaryOp::Not] {
            assert!(
                !LirRvalue::Unary {
                    op,
                    operand: value.clone(),
                    ty: Type::Char
                }
                .is_well_typed(&locals, LirLocalId(0))
            );
        }
        for op in [
            BinOp::Add,
            BinOp::Sub,
            BinOp::Mul,
            BinOp::Div,
            BinOp::Rem,
            BinOp::Pow,
            BinOp::BitAnd,
            BinOp::BitOr,
            BinOp::BitXor,
            BinOp::Shl,
            BinOp::Shr,
        ] {
            assert!(
                !LirRvalue::Binary {
                    op,
                    lhs: value.clone(),
                    rhs: value.clone(),
                    operand_ty: Type::Char
                }
                .is_well_typed(&locals, LirLocalId(0))
            );
        }
        for op in [
            BinOp::Eq,
            BinOp::Ne,
            BinOp::Lt,
            BinOp::Le,
            BinOp::Gt,
            BinOp::Ge,
        ] {
            assert!(
                LirRvalue::Binary {
                    op,
                    lhs: value.clone(),
                    rhs: value.clone(),
                    operand_ty: Type::Char
                }
                .is_well_typed(&locals, LirLocalId(1))
            );
        }
        for raw in [-1, 0xd800, 0xdfff, 0x110000] {
            assert_eq!(LirOperand::integer(raw, &Type::Char).ty(&[]), None);
        }
    }

    #[test]
    fn scalar_width_constant_negation_retains_unsigned_overflow() {
        use super::{LirOperand, Type, UnaryOp, is_overflow_checked_unary};
        for (ty, nonzero, min) in [
            (Type::U8, 255, 0),
            (Type::U16, 65535, 0),
            (Type::U32, i64::from(u32::MAX), 0),
            (Type::U64, -1, 0),
            (Type::I8, 127, -128),
            (Type::I16, 32767, -32768),
            (Type::I32, i64::from(i32::MAX), i64::from(i32::MIN)),
            (Type::I64, i64::MAX, i64::MIN),
        ] {
            assert!(!is_overflow_checked_unary(
                &UnaryOp::Neg,
                &LirOperand::integer(0, &ty),
                &ty
            ));
            assert_eq!(
                is_overflow_checked_unary(&UnaryOp::Neg, &LirOperand::integer(nonzero, &ty), &ty),
                min == 0
            );
            if min != 0 {
                assert!(is_overflow_checked_unary(
                    &UnaryOp::Neg,
                    &LirOperand::integer(min, &ty),
                    &ty
                ));
            }
        }
        assert!(is_overflow_checked_unary(
            &UnaryOp::Neg,
            &LirOperand::integer(i64::MIN, &Type::U64),
            &Type::U64
        ));
    }

    #[test]
    fn typed_integer_operands_reject_invalid_artifacts() {
        use super::{LirOperand, Type};
        for value in [0, 128, 255] {
            assert_eq!(
                LirOperand::integer(value, &Type::U8).ty(&[]),
                Some(Type::U8)
            );
        }
        for (value, ty) in [(-1, Type::U8), (256, Type::U8), (1, Type::Bool)] {
            assert_eq!(LirOperand::Integer { value, ty }.ty(&[]), None);
        }
    }

    #[test]
    fn float_operand_artifacts_preserve_every_payload_bit() {
        for bits in [
            0_u64,
            1_u64,
            (-0.0_f64).to_bits(),
            1.25_f64.to_bits(),
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            0x7ff8_0000_0000_0001,
            0x7ff0_0000_0000_0001,
            0xfff8_1234_5678_9abc,
        ] {
            let wire = serde_json::to_vec(&super::LirOperand::Float(f64::from_bits(bits))).unwrap();
            let restored: super::LirOperand = serde_json::from_slice(&wire).unwrap();
            let super::LirOperand::Float(value) = restored else {
                panic!("float operand");
            };
            assert_eq!(value.to_bits(), bits);
        }
    }

    use super::*;

    fn value_local(id: u32, ty: Type) -> LirLocal {
        LirLocal {
            storage_kind: super::super::LirStorageKind::Value,
            id: LirLocalId(id),
            name: format!("v{id}"),
            ty,
            source_span: None,
            synthetic: true,
            parameter: false,
        }
    }

    fn intrinsic_call(
        method: &str,
        intrinsic: crate::semantic::intrinsics::Intrinsic,
        receiver_ty: Type,
        arg_types: Vec<Type>,
        result: Type,
    ) -> (LirRvalue, Vec<LirLocal>) {
        let locals = [result.clone(), receiver_ty.clone()]
            .into_iter()
            .chain(arg_types.iter().cloned())
            .enumerate()
            .map(|(i, ty)| value_local(i as u32, ty))
            .collect();
        let args = (0..arg_types.len())
            .map(|i| LirOperand::Local(LirLocalId(2 + i as u32)))
            .collect();
        (
            LirRvalue::IntrinsicCall {
                intrinsic,
                method: method.into(),
                receiver: LirOperand::Local(LirLocalId(1)),
                receiver_ty,
                args,
                arg_types,
                result,
            },
            locals,
        )
    }

    #[test]
    fn intrinsic_lir_requires_the_id_of_the_typed_member() {
        use crate::semantic::intrinsics::Intrinsic::*;
        for (name, id, wrong_id, recv, args, result) in [
            (
                "to_upper",
                StringToUpper,
                StringToLower,
                Type::String,
                vec![],
                Type::String,
            ),
            (
                "slice",
                StringSubstring,
                StringReplace,
                Type::String,
                vec![Type::I64, Type::I64],
                Type::String,
            ),
            (
                "substring",
                StringSubstring,
                StringReplace,
                Type::String,
                vec![Type::I64, Type::I64],
                Type::String,
            ),
            (
                "__abi_debug",
                I64ToString,
                ScalarToString,
                Type::I64,
                vec![],
                Type::String,
            ),
            (
                "__abi_debug",
                ScalarToString,
                I64ToString,
                Type::Char,
                vec![],
                Type::String,
            ),
            (
                "close",
                ChannelClose,
                ChannelSend,
                Builtin::Channel.apply(vec![Type::I64]),
                vec![],
                Type::Void,
            ),
        ] {
            let (mut call, locals) = intrinsic_call(name, id, recv, args, result);
            assert!(call.is_well_typed(&locals, LirLocalId(0)), "{name}");
            let LirRvalue::IntrinsicCall { intrinsic, .. } = &mut call else {
                unreachable!()
            };
            *intrinsic = wrong_id;
            assert!(!call.is_well_typed(&locals, LirLocalId(0)), "{name}");
        }
    }

    #[test]
    fn intrinsic_lir_compares_result_types_without_copying_payloads() {
        use crate::parser::ast::REBUILT_TYPE_NODES;
        use crate::semantic::intrinsics::Intrinsic::*;

        for depth in [0, 8, 32, 128] {
            let item = (0..depth).fold(Type::I64, |ty, _| Type::Array(Box::new(ty)));
            REBUILT_TYPE_NODES.set(0);
            let key = item.clone();
            assert_eq!(REBUILT_TYPE_NODES.get(), depth + 1);
            let map = Builtin::Map.apply(vec![key.clone(), item.clone()]);
            let task = Builtin::Task.apply(vec![item.clone()]);
            let wrong = Type::Generic(TypeId::local("Other"), vec![item.clone()]);
            for (name, id, recv, args, result) in [
                (
                    "get",
                    MapGet,
                    map.clone(),
                    vec![key.clone()],
                    Builtin::Option.apply(vec![item.clone()]),
                ),
                (
                    "freeze",
                    MapFreeze,
                    map.clone(),
                    vec![],
                    Builtin::FrozenMap.apply(vec![key.clone(), item.clone()]),
                ),
                (
                    "entries",
                    MapEntries,
                    map.clone(),
                    vec![],
                    Type::Array(Box::new(Type::Generic(
                        TypeId::local("$Tuple2"),
                        vec![key.clone(), item.clone()],
                    ))),
                ),
                (
                    "attach",
                    TokenAttach,
                    Builtin::CancellationToken.apply(vec![]),
                    vec![task.clone()],
                    task.clone(),
                ),
                (
                    "add",
                    ScopeAdd,
                    Builtin::TaskScope.apply(vec![]),
                    vec![task.clone()],
                    task.clone(),
                ),
                (
                    "recv",
                    ChannelRecv,
                    Builtin::Channel.apply(vec![item.clone()]),
                    vec![],
                    item.clone(),
                ),
            ] {
                let (call, locals) = intrinsic_call(name, id, recv.clone(), args.clone(), result);
                let (invalid, bad_locals) = intrinsic_call(name, id, recv, args, wrong.clone());
                for count in [1, 8, 64] {
                    REBUILT_TYPE_NODES.set(0);
                    for _ in 0..count {
                        assert!(call.is_well_typed(&locals, LirLocalId(0)), "{name}");
                        assert!(!invalid.is_well_typed(&bad_locals, LirLocalId(0)), "{name}");
                    }
                    assert_eq!(
                        REBUILT_TYPE_NODES.get(),
                        0,
                        "{name}, depth {depth}, calls {count}"
                    );
                }
            }
        }
    }

    #[test]
    fn operand_iteration_preserves_fixed_operands_arguments_and_place_locals() {
        let local = |n| LirOperand::Local(LirLocalId(n));
        let args = vec![
            LirOperand::Int(9),
            LirOperand::Reference {
                place: LirPlace::ArrayElement {
                    owner: LirLocalId(1),
                    index: LirLocalId(2),
                    element: Type::I64,
                },
                span: Span::dummy(),
                display: "array[index]".into(),
            },
            local(1),
        ];
        let call = LirRvalue::IndirectCall {
            callee: local(0),
            name: FunctionId::free_from_source_name("callee"),
            params: vec![Type::I64; args.len()],
            args,
            result: Type::Void,
        };
        let store = LirRvalue::ArrayStore {
            array: local(2),
            index: local(1),
            value: local(0),
            element: Type::I64,
        };
        for (value, expected) in [
            (call, vec![0, 1, 2, 1]),
            (store, vec![2, 1, 0]),
            (LirRvalue::BeginReferenceCall, vec![]),
        ] {
            let ids = value.iter_operands().flat_map(LirOperand::iter_locals);
            assert!(ids.eq(expected.into_iter().map(LirLocalId)));
        }
    }

    #[test]
    fn reference_operand_is_rejected_for_use_and_accepted_for_direct_call() {
        let local = LirLocalId(0);
        let locals = [value_local(local.0, Type::I64)];
        let reference = LirOperand::Reference {
            place: LirPlace::Local(local),
            span: Span::dummy(),
            display: "v0".into(),
        };
        assert!(LirRvalue::Use(LirOperand::Local(local)).is_well_typed(&locals, local));
        assert!(!LirRvalue::Use(reference.clone()).is_well_typed(&locals, local));
        let call = LirRvalue::DirectCall {
            callee: FunctionId::free_from_source_name("read"),
            args: vec![reference],
            params: vec![Type::I64],
            result: Type::I64,
        };
        assert!(call.is_well_typed(&locals, local));
    }

    fn reference_to(id: u32) -> LirOperand {
        LirOperand::Reference {
            place: LirPlace::Local(LirLocalId(id)),
            span: Span::dummy(),
            display: format!("v{id}"),
        }
    }

    #[test]
    fn capture_array_owner_requires_plain_array_and_index() {
        let locals = [
            value_local(0, Type::Array(Box::new(Type::I64))),
            value_local(1, Type::I64),
            LirLocal {
                storage_kind: super::super::LirStorageKind::GcOwner,
                ..value_local(2, Type::Void)
            },
        ];
        let capture = |array, index| {
            LirRvalue::CaptureArrayOwner { array, index }.is_well_typed(&locals, LirLocalId(2))
        };
        let array = LirOperand::Local(LirLocalId(0));
        let index = LirOperand::Local(LirLocalId(1));
        assert!(capture(array.clone(), index.clone()));
        assert!(!capture(reference_to(0), index));
        assert!(!capture(array, reference_to(1)));
    }

    #[test]
    fn constructor_and_method_require_plain_receivers_and_allow_reference_arguments() {
        let class = TypeId::local("Cell");
        let locals = [
            value_local(0, Type::Named(class)),
            value_local(1, Type::I64),
            value_local(2, Type::Void),
        ];
        for (receiver, valid) in [
            (LirOperand::Local(LirLocalId(0)), true),
            (reference_to(0), false),
        ] {
            let constructor = LirRvalue::ConstructorCall {
                object: receiver.clone(),
                class,
                args: vec![reference_to(1)],
                arg_types: vec![Type::I64],
            };
            let method = LirRvalue::MethodCall {
                receiver,
                receiver_ty: Type::Named(class),
                method: "read".into(),
                is_async: false,
                args: vec![reference_to(1)],
                arg_types: vec![Type::I64],
                result: Type::Void,
                super_class: None,
            };
            assert_eq!(constructor.is_well_typed(&locals, LirLocalId(2)), valid);
            assert_eq!(method.is_well_typed(&locals, LirLocalId(2)), valid);
        }
    }

    fn nominal_shapes(id: Builtin) -> Vec<(Type, bool)> {
        let name = id.source_name();
        vec![
            (id.apply(vec![Type::I64]), true),
            (Type::Named(TypeId::local(name)), false),
            (id.apply(vec![]), false),
            (id.apply(vec![Type::I64, Type::I64]), false),
            (id.apply(vec![Type::Bool]), false),
            (
                Type::Generic(TypeId::from(format!("user::{name}")), vec![Type::I64]),
                false,
            ),
            (
                Type::Generic(TypeId::local(format!("{name}Like")), vec![Type::I64]),
                false,
            ),
            (Type::Array(Box::new(Type::I64)), false),
        ]
    }

    #[test]
    fn builtin_verifier_start_task_preserves_nominal_shape_and_arguments() {
        let task = LirRvalue::StartTask {
            callee: FunctionId::free_from_source_name("work"),
            args: vec![LirOperand::Int(1)],
            params: vec![Type::I64],
            output: Type::I64,
        };
        for (ty, expected) in nominal_shapes(Builtin::Task) {
            assert_eq!(
                task.is_well_typed(&[value_local(0, ty.clone())], LirLocalId(0)),
                expected,
                "{ty:?}"
            );
        }
        let locals = [value_local(0, Builtin::Task.apply(vec![Type::I64]))];
        for (args, params) in [
            (vec![], vec![Type::I64]),
            (vec![LirOperand::Int(1)], vec![]),
            (vec![LirOperand::Bool(true)], vec![Type::I64]),
            (vec![LirOperand::Local(LirLocalId(9))], vec![Type::I64]),
        ] {
            let bad = LirRvalue::StartTask {
                callee: FunctionId::free_from_source_name("work"),
                args,
                params,
                output: Type::I64,
            };
            assert!(!bad.is_well_typed(&locals, LirLocalId(0)));
        }
    }

    #[test]
    fn builtin_verifier_await_future_preserves_nominal_shape_and_result() {
        let awaiter = LirRvalue::AwaitFuture {
            future: LirOperand::Local(LirLocalId(1)),
            result: Type::I64,
        };
        for (ty, expected) in nominal_shapes(Builtin::Future) {
            let locals = [value_local(0, Type::I64), value_local(1, ty.clone())];
            assert_eq!(
                awaiter.is_well_typed(&locals, LirLocalId(0)),
                expected,
                "{ty:?}"
            );
        }
        assert!(!awaiter.is_well_typed(&[value_local(0, Type::I64)], LirLocalId(0)));
        assert!(!awaiter.is_well_typed(
            &[
                value_local(0, Type::Bool),
                value_local(1, Builtin::Future.apply(vec![Type::I64]))
            ],
            LirLocalId(0)
        ));
    }

    #[test]
    fn builtin_verifier_range_preserves_nominal_shape_and_bounds() {
        let range = LirRvalue::Range {
            start: LirOperand::Int(0),
            end: LirOperand::Int(3),
        };
        for (ty, expected) in nominal_shapes(Builtin::Range) {
            assert_eq!(
                range.is_well_typed(&[value_local(0, ty.clone())], LirLocalId(0)),
                expected,
                "{ty:?}"
            );
        }
        let locals = [value_local(0, Builtin::Range.apply(vec![Type::I64]))];
        for (start, end) in [
            (LirOperand::Bool(false), LirOperand::Int(3)),
            (LirOperand::Int(0), LirOperand::Bool(true)),
            (LirOperand::Local(LirLocalId(9)), LirOperand::Int(3)),
        ] {
            assert!(!LirRvalue::Range { start, end }.is_well_typed(&locals, LirLocalId(0)));
        }
    }

    #[test]
    fn scalar_operators_become_flat_local_operands() {
        let tokens = crate::lexer::Lexer::new("fn f(x: i64) -> i64 { return -(x + 1) * 2; }")
            .tokenize()
            .unwrap();
        let (ast, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let (hir, errors) = crate::ir::lower::lower_program(&ast);
        assert!(errors.is_empty());
        let program = super::super::lower_source_program(&hir);
        let function = &program.functions[0];
        let values: Vec<_> = function
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| match inst {
                SourceInst::Compute { local, value, .. } => Some((*local, value)),
                _ => None,
            })
            .collect();
        assert_eq!(values.len(), 3);
        for (local, value) in values {
            assert!(value.is_well_typed(&function.locals, local));
        }
        assert!(function.blocks.iter().any(|block| matches!(
            &block.terminator,
            SourceTerminator::Return(Some(HirExpr {
                kind: HirExprKind::Var(_),
                ..
            }))
        )));
    }
}

#[cfg(test)]
mod opaque_owner_tests {
    #[test]
    fn an_opaque_owner_is_not_a_language_operand() {
        let local = super::LirLocal {
            storage_kind: super::super::LirStorageKind::GcOwner,
            id: super::LirLocalId(0),
            name: "owner".into(),
            ty: super::Type::Void,
            source_span: None,
            synthetic: true,
            parameter: false,
        };
        assert!(super::LirOperand::Local(local.id).ty(&[local]).is_none());
    }
}
