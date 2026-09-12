//! Flat value operations. Operands refer to locals or immediate constants;
//! evaluation order is represented by the surrounding instruction sequence.
use super::{LirLocal, LirLocalId, SourceBlock, SourceInst, SourceTerminator};
use crate::diagnostics::Span;
use crate::ir::typed_ast::{HirExpr, HirExprKind};
use crate::parser::ast::{BinOp, ExprId, UnaryOp};
use crate::semantic::ids::{FunctionId, SemanticType as Type, TypeId};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum LirOperand {
    Local(LirLocalId),
    Int(i64),
    Float(f64),
    Bool(bool),
    Reference {
        place: LirPlace,
        span: Span,
        display: String,
    },
}

/// Captured storage identity, independent of subsequent array resizing.
#[derive(Debug, Clone, PartialEq)]
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
    pub fn locals(&self) -> Vec<LirLocalId> {
        match self {
            Self::Local(local) => vec![*local],
            Self::Field { object, .. } => vec![*object],
            Self::ArrayElement { owner, index, .. } => vec![*owner, *index],
        }
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
    pub fn locals(&self) -> Vec<LirLocalId> {
        match self {
            Self::Local(local) => vec![*local],
            Self::Reference { place, .. } => place.locals(),
            _ => Vec::new(),
        }
    }
    pub fn ty(&self, locals: &[LirLocal]) -> Option<Type> {
        match self {
            Self::Local(id) => locals
                .get(id.0 as usize)
                .filter(|local| !local.is_gc_owner())
                .map(|local| local.ty.clone()),
            Self::Int(_) => Some(Type::I64),
            Self::Float(_) => Some(Type::F64),
            Self::Bool(_) => Some(Type::Bool),
            Self::Reference { place, .. } => place.ty(locals),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
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
        method: String,
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
        args: Vec<LirOperand>,
        arg_types: Vec<Type>,
        result: Type,
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
        method: String,
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
}

impl LirRvalue {
    pub fn is_well_typed(&self, locals: &[LirLocal], destination: LirLocalId) -> bool {
        let Some(destination) = locals.get(destination.0 as usize) else {
            return false;
        };
        if let Self::CaptureArrayOwner { array, index } = self {
            return destination.is_gc_owner()
                && matches!(array.ty(locals), Some(Type::Array(_)))
                && index.ty(locals) == Some(Type::I64);
        }
        if destination.is_gc_owner() {
            return false;
        }
        if self
            .operands()
            .iter()
            .any(|operand| matches!(operand, LirOperand::Reference { .. }))
            && !matches!(
                self,
                Self::DirectCall { .. }
                    | Self::MethodCall { .. }
                    | Self::StaticCall { .. }
                    | Self::ConstructorCall { .. }
                    | Self::ReferenceDebug { .. }
            )
        {
            return false;
        }
        let ty = |operand: &LirOperand| operand.ty(locals);
        match self {
            Self::BeginReferenceCall => destination.ty == Type::Void,
            Self::CaptureArrayOwner { .. } => false,
            Self::ReferenceDebug { argument, .. } => destination.ty == Type::Void && matches!(argument, LirOperand::Reference { .. }) && ty(argument).is_some(),
            Self::Use(operand) => ty(operand).as_ref() == Some(&destination.ty),
            Self::StartTask { args, params, output, .. } => destination.ty == Type::Generic(TypeId::local("Task"), vec![output.clone()]) && args.len() == params.len() && args.iter().zip(params).all(|(arg, param)| ty(arg).as_ref() == Some(param)),
            Self::AwaitFuture { future, result } => destination.ty == *result && ty(future) == Some(Type::Generic(TypeId::local("Future"), vec![result.clone()])),
            Self::SelectIdleWait { deadlines } => destination.ty == Type::Void && deadlines.iter().all(|operand| ty(operand) == Some(Type::I64)),
            Self::Coerce { value, source, target } => ty(value).as_ref() == Some(source) && destination.ty == *target,
            Self::ArrayAlloc { element, .. } => destination.ty == Type::Array(Box::new(element.clone())),
            Self::ArrayStore { array, index, value, element } => ty(array) == Some(Type::Array(Box::new(element.clone()))) && ty(index) == Some(Type::I64) && ty(value).is_some() && destination.ty == Type::Void,
            Self::Index { array, index, element } => ty(index) == Some(Type::I64) && destination.ty == *element && ty(array).is_some_and(|array| match &array { Type::Array(inner) => **inner == *element, Type::Generic(name, args) => *name == TypeId::local("FrozenArray") && args == std::slice::from_ref(element), _ => false }),
            Self::ObjectAlloc { class } => destination.ty == Type::Named(*class),
            Self::FieldLoad { object, object_ty, result, .. } => ty(object).as_ref() == Some(object_ty) && destination.ty == *result,
            Self::FieldStore { object, object_ty, value, .. } => ty(object).as_ref() == Some(object_ty) && ty(value).is_some() && destination.ty == Type::Void,
            Self::StaticField { result, .. } => destination.ty == *result,
            Self::StaticStore { value, .. } => ty(value).is_some() && destination.ty == Type::Void,
            Self::ConstructorCall { object, class, args, arg_types } => ty(object) == Some(Type::Named(*class)) && destination.ty == Type::Void && args.len() == arg_types.len() && args.iter().zip(arg_types).all(|(arg, expected)| ty(arg).as_ref() == Some(expected)),
            Self::Range { start, end } => ty(start) == Some(Type::I64) && ty(end) == Some(Type::I64) && destination.ty == Type::Generic(TypeId::local("Range"), vec![Type::I64]),
            Self::EnumMethod { receiver, receiver_ty, args, arg_types, result, .. } => ty(receiver).as_ref() == Some(receiver_ty) && destination.ty == *result && args.len() == arg_types.len() && args.iter().zip(arg_types).all(|(arg, expected)| ty(arg).as_ref() == Some(expected)),
            Self::BuiltinCall { args, params, result, .. } => destination.ty == *result && args.len() == params.len() && args.iter().zip(params).all(|(arg, expected)| ty(arg).as_ref() == Some(expected)),
            Self::StaticCall { args, arg_types, result, .. } => destination.ty == *result && args.len() == arg_types.len() && args.iter().zip(arg_types).all(|(arg, expected)| ty(arg).as_ref() == Some(expected)),
            Self::EnumAlloc { enum_ty, .. } => destination.ty == *enum_ty,
            Self::EnumPayloadStore { object, value, source, enum_ty, .. } => ty(object).as_ref() == Some(enum_ty) && ty(value).as_ref() == Some(source) && destination.ty == *enum_ty,
            Self::FormatScalar { value, ty: expected, format } => ty(value).as_ref() == Some(expected) && destination.ty == Type::String && if format.is_some() { *expected == Type::F64 } else { matches!(expected, Type::I64 | Type::F64 | Type::Bool | Type::String) },
            Self::Panic { message } => ty(message) == Some(Type::String) && destination.ty == Type::Never,
            Self::Recover => destination.ty == Type::Generic(TypeId::local("Option"), vec![Type::Named(TypeId::local("PanicInfo"))]),
            Self::RebindResultError { value, source, target } | Self::IntoError { value, source, target } => ty(value).as_ref() == Some(source) && destination.ty == *target,
            Self::PrepareMethod { receiver, receiver_ty, .. } => ty(receiver).as_ref() == Some(receiver_ty) && destination.ty == *receiver_ty,
            Self::MethodCall { receiver, receiver_ty, args, arg_types, result, .. } => ty(receiver).as_ref() == Some(receiver_ty) && destination.ty == *result && args.len() == arg_types.len() && args.iter().zip(arg_types).all(|(arg, expected)| ty(arg).as_ref() == Some(expected)),
            Self::IntrinsicCall { intrinsic, method, receiver, receiver_ty, args, arg_types, result } => {
                ty(receiver).as_ref() == Some(receiver_ty) && destination.ty == *result
                    && args.len() == arg_types.len() && args.iter().zip(arg_types).all(|(arg, expected)| ty(arg).as_ref() == Some(expected))
                    && crate::semantic::intrinsics::resolve(receiver_ty, method, args.len()).is_some_and(|resolved|
                        resolved.intrinsic == *intrinsic && resolved.return_type(|i| arg_types.get(i).cloned()) == *result)
            }
            Self::StringLiteral(_) => destination.ty == Type::String,
            Self::FunctionRef { ty, .. } => destination.ty == *ty && matches!(ty, Type::Fn(..)),
            Self::Closure { captures, ty: result, .. } => destination.ty == *result
                && matches!(result, Type::Fn(..) | Type::Closure(..)) && captures.iter().all(|capture| ty(capture).is_some()),
            Self::Print { value, ty: expected, .. } => destination.ty == Type::Void
                && ty(value).as_ref() == Some(expected) && matches!(expected, Type::I64 | Type::F64 | Type::Bool | Type::String),
            Self::DirectCall { args, params, result, .. } => destination.ty == *result
                && args.len() == params.len() && args.iter().zip(params).all(|(arg, param)| ty(arg).as_ref() == Some(param)),
            Self::IndirectCall { callee, args, params, result, .. } => destination.ty == *result
                && ty(callee).is_some_and(|callee| matches!(&callee, Type::Fn(expected, output) | Type::Closure(expected, output) if expected == params && **output == *result))
                && args.len() == params.len() && args.iter().zip(params).all(|(arg, param)| ty(arg).as_ref() == Some(param)),
            Self::Unary {
                op,
                operand,
                ty: expected,
            } => {
                ty(operand).as_ref() == Some(expected)
                    && destination.ty == *expected
                    && match op {
                        UnaryOp::Neg => matches!(expected, Type::I64 | Type::F64),
                        UnaryOp::Not => *expected == Type::Bool,
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
                    Type::Bool
                } else {
                    operand_ty.clone()
                };
                ty(lhs).as_ref() == Some(operand_ty)
                    && ty(rhs).as_ref() == Some(operand_ty)
                    && destination.ty == result
                    && !matches!(op, BinOp::And | BinOp::Or)
                    && match operand_ty {
                        Type::I64 | Type::F64 => true,
                        Type::Bool => matches!(op, BinOp::Eq | BinOp::Ne),
                        Type::String => matches!(op, BinOp::Add | BinOp::Eq | BinOp::Ne),
                        Type::Named(_) | Type::Generic(_, _) => matches!(op, BinOp::Eq | BinOp::Ne),
                        _ => false,
                    }
            }
        }
    }

    pub fn operands(&self) -> Vec<&LirOperand> {
        match self {
            Self::BeginReferenceCall => vec![],
            Self::CaptureArrayOwner { array, index } => vec![array, index],
            Self::ReferenceDebug { .. } => vec![],
            Self::StartTask { args, .. } => args.iter().collect(),
            Self::AwaitFuture { future, .. } => vec![future],
            Self::SelectIdleWait { deadlines } => deadlines.iter().collect(),
            Self::Use(operand) => vec![operand],
            Self::Coerce { value, .. }
            | Self::StaticStore { value, .. }
            | Self::FormatScalar { value, .. } => vec![value],
            Self::ArrayAlloc { .. }
            | Self::ObjectAlloc { .. }
            | Self::StaticField { .. }
            | Self::Recover => vec![],
            Self::ArrayStore {
                array,
                index,
                value,
                ..
            } => vec![array, index, value],
            Self::Index { array, index, .. } => vec![array, index],
            Self::FieldLoad { object, .. } => vec![object],
            Self::FieldStore { object, value, .. } => vec![object, value],
            Self::ConstructorCall { object, args, .. } => {
                std::iter::once(object).chain(args).collect()
            }
            Self::Range { start, end } => vec![start, end],
            Self::EnumMethod { receiver, args, .. } => {
                std::iter::once(receiver).chain(args).collect()
            }
            Self::BuiltinCall { args, .. } => args.iter().collect(),
            Self::StaticCall { args, .. } => args.iter().collect(),
            Self::EnumAlloc { .. } => vec![],
            Self::EnumPayloadStore { object, value, .. } => vec![object, value],
            Self::Panic { message } => vec![message],
            Self::RebindResultError { value, .. } | Self::IntoError { value, .. } => vec![value],
            Self::PrepareMethod { receiver, .. } => vec![receiver],
            Self::MethodCall { receiver, args, .. } => {
                std::iter::once(receiver).chain(args).collect()
            }
            Self::IntrinsicCall { receiver, args, .. } => {
                std::iter::once(receiver).chain(args).collect()
            }
            Self::StringLiteral(_) | Self::FunctionRef { .. } => vec![],
            Self::Closure { captures, .. } => captures.iter().collect(),
            Self::Print { value, .. } => vec![value],
            Self::DirectCall { args, .. } => args.iter().collect(),
            Self::IndirectCall { callee, args, .. } => {
                std::iter::once(callee).chain(args).collect()
            }
            Self::Unary { operand, .. } => vec![operand],
            Self::Binary { lhs, rhs, .. } => vec![lhs, rhs],
        }
    }
}

mod lower;
pub(super) use lower::{lower_blocks, lower_calls};

#[cfg(test)]
mod tests {
    use super::*;

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
