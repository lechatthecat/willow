//! Stable semantic identities for compiler-known nominal types.
//!
//! The parser still preserves the source spelling in [`Type`], but semantic
//! consumers classify that spelling once through [`resolve`] and compare a
//! closed [`BuiltinTypeId`] instead of independently matching strings. This is
//! the migration boundary toward storing the ID directly in typed HIR.

use crate::parser::ast::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BuiltinTypeId {
    Option,
    Result,
    Task,
    TaskResult,
    Future,
    JoinHandle,
    Array,
    FrozenArray,
    Map,
    FrozenMap,
    Channel,
    BlockingCell,
    BlockingRwCell,
    AtomicI64,
    AtomicBool,
    CancellationToken,
    TaskScope,
    Cancelled,
    IoError,
}

impl BuiltinTypeId {
    pub const fn source_name(self) -> &'static str {
        match self {
            Self::Option => "Option",
            Self::Result => "Result",
            Self::Task => "Task",
            Self::TaskResult => "TaskResult",
            Self::Future => "Future",
            Self::JoinHandle => "JoinHandle",
            Self::Array => "Array",
            Self::FrozenArray => "FrozenArray",
            Self::Map => "Map",
            Self::FrozenMap => "FrozenMap",
            Self::Channel => "Channel",
            Self::BlockingCell => "BlockingCell",
            Self::BlockingRwCell => "BlockingRwCell",
            Self::AtomicI64 => "AtomicI64",
            Self::AtomicBool => "AtomicBool",
            Self::CancellationToken => "CancellationToken",
            Self::TaskScope => "TaskScope",
            Self::Cancelled => "Cancelled",
            Self::IoError => "IoError",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "Option" => Self::Option,
            "Result" => Self::Result,
            "Task" => Self::Task,
            "TaskResult" => Self::TaskResult,
            "Future" => Self::Future,
            "JoinHandle" => Self::JoinHandle,
            "Array" => Self::Array,
            "FrozenArray" => Self::FrozenArray,
            "Map" => Self::Map,
            "FrozenMap" => Self::FrozenMap,
            "Channel" => Self::Channel,
            "BlockingCell" => Self::BlockingCell,
            "BlockingRwCell" => Self::BlockingRwCell,
            "AtomicI64" => Self::AtomicI64,
            "AtomicBool" => Self::AtomicBool,
            "CancellationToken" => Self::CancellationToken,
            "TaskScope" => Self::TaskScope,
            "Cancelled" => Self::Cancelled,
            "IoError" => Self::IoError,
            _ => return None,
        })
    }

    pub fn apply(self, args: Vec<Type>) -> Type {
        if args.is_empty()
            && matches!(
                self,
                Self::AtomicI64
                    | Self::AtomicBool
                    | Self::CancellationToken
                    | Self::TaskScope
                    | Self::Cancelled
                    | Self::IoError
            )
        {
            Type::Named(self.source_name().to_string())
        } else {
            Type::Generic(self.source_name().to_string(), args)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BuiltinTypeRef<'a> {
    pub id: BuiltinTypeId,
    pub args: &'a [Type],
}

pub fn resolve(ty: &Type) -> Option<BuiltinTypeRef<'_>> {
    match ty {
        Type::Array(element) => Some(BuiltinTypeRef {
            id: BuiltinTypeId::Array,
            args: std::slice::from_ref(element.as_ref()),
        }),
        Type::Generic(name, args) => Some(BuiltinTypeRef {
            id: BuiltinTypeId::from_name(name)?,
            args,
        }),
        Type::Named(name) => Some(BuiltinTypeRef {
            id: BuiltinTypeId::from_name(name)?,
            args: &[],
        }),
        _ => None,
    }
}

pub fn is(ty: &Type, id: BuiltinTypeId) -> bool {
    resolve(ty).is_some_and(|resolved| resolved.id == id)
}

pub fn unary_arg(ty: &Type, id: BuiltinTypeId) -> Option<&Type> {
    let resolved = resolve(ty)?;
    (resolved.id == id && resolved.args.len() == 1).then(|| &resolved.args[0])
}

pub fn binary_args(ty: &Type, id: BuiltinTypeId) -> Option<(&Type, &Type)> {
    let resolved = resolve(ty)?;
    (resolved.id == id && resolved.args.len() == 2).then(|| (&resolved.args[0], &resolved.args[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_identity_is_independent_of_type_arguments() {
        let ty = Type::Generic("Result".into(), vec![Type::String, Type::I64]);
        let resolved = resolve(&ty).unwrap();
        assert_eq!(resolved.id, BuiltinTypeId::Result);
        assert_eq!(resolved.args, &[Type::String, Type::I64]);
    }

    #[test]
    fn user_nominal_types_do_not_acquire_builtin_identity() {
        assert_eq!(resolve(&Type::Named("ResultLike".into())), None);
        assert_eq!(
            resolve(&Type::Generic("TaskBox".into(), vec![Type::I64])),
            None
        );
    }

    #[test]
    fn array_uses_the_same_semantic_identity_as_other_builtin_families() {
        let ty = Type::Array(Box::new(Type::String));
        let resolved = resolve(&ty).unwrap();
        assert_eq!(resolved.id, BuiltinTypeId::Array);
        assert_eq!(resolved.args, &[Type::String]);
    }

    #[test]
    fn construction_round_trips_ids() {
        for (id, args) in [
            (BuiltinTypeId::Option, vec![Type::I64]),
            (BuiltinTypeId::Result, vec![Type::String, Type::I64]),
            (BuiltinTypeId::Task, vec![Type::Bool]),
            (BuiltinTypeId::CancellationToken, vec![]),
        ] {
            assert_eq!(resolve(&id.apply(args)).unwrap().id, id);
        }
    }
}
