//! One representation decision for every compiler path that constructs or
//! inspects a Willow `Option<T>`.
//!
//! A one-word GC reference already has an invalid bit-pattern available:
//! zero.  For payloads whose valid values are guaranteed non-null we therefore
//! encode `None` as zero and `Some(value)` as `value` itself. Scalar payloads
//! use an inline tag/payload pair. Other payloads retain the heap-enum layout.
//! In particular, an outer
//! `Option<Option<T>>` is always boxed so `None` and `Some(None)` remain
//! distinguishable even when the inner option uses the nullable-pointer niche.

use super::type_index::TypeMap;

use super::EnumInfo;
use crate::semantic::builtin_types::{self, BuiltinTypeId};
use crate::semantic::ids::SemanticType as Type;

use super::type_helpers::is_gc_managed;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OptionRepr {
    NullableGcPointer,
    TaggedPair,
    BoxedTaggedEnum,
}

/// Scalar small ADTs use two machine words: tag followed by payload bits.
/// Larger or reference-bearing variants retain their traced heap layout.
/// This predicate is shared with signature selection, which also runs before
/// instantiated enum metadata is available.
pub(crate) use crate::semantic::builtin_types::is_scalar_pair;

pub(crate) fn option_inner(ty: &Type) -> Option<&Type> {
    builtin_types::unary_arg(ty, BuiltinTypeId::Option)
}

/// Return the representation of an instantiated `Option<T>`.
///
/// A nested Option is excluded because zero can already be a valid inner
/// representation. The remaining GC-managed types are represented by non-null
/// heap pointers when valid, so zero is available for `None`.
pub(crate) fn option_repr(ty: &Type, enum_infos: &TypeMap<EnumInfo>) -> Option<OptionRepr> {
    let inner = option_inner(ty)?;
    let niche = option_inner(inner).is_none() && is_gc_managed(inner, enum_infos);
    Some(if is_scalar_pair(ty) {
        OptionRepr::TaggedPair
    } else if niche {
        OptionRepr::NullableGcPointer
    } else {
        OptionRepr::BoxedTaggedEnum
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(inner: Type) -> Type {
        Type::Generic("Option".to_string().into(), vec![inner])
    }

    #[test]
    fn gc_references_use_the_nullable_pointer_niche() {
        let enums = TypeMap::new();
        assert_eq!(
            option_repr(&option(Type::String), &enums),
            Some(OptionRepr::NullableGcPointer)
        );
        assert_eq!(
            option_repr(&option(Type::Array(Box::new(Type::I64))), &enums),
            Some(OptionRepr::NullableGcPointer)
        );
        assert_eq!(
            option_repr(&option(Type::Named("User".to_string().into())), &enums),
            Some(OptionRepr::NullableGcPointer)
        );
    }

    #[test]
    fn scalar_options_use_tagged_pairs() {
        let enums = TypeMap::new();
        for scalar in [Type::I64, Type::F64, Type::Bool, Type::Void] {
            assert_eq!(
                option_repr(&option(scalar), &enums),
                Some(OptionRepr::TaggedPair)
            );
        }
    }

    #[test]
    fn nested_options_stay_boxed() {
        let enums = TypeMap::new();
        assert_eq!(
            option_repr(&option(option(Type::I64)), &enums),
            Some(OptionRepr::BoxedTaggedEnum)
        );
        assert_eq!(
            option_repr(&option(option(Type::String)), &enums),
            Some(OptionRepr::BoxedTaggedEnum)
        );
    }

    #[test]
    fn scalar_results_use_pairs_for_every_scalar_combination() {
        for ok in [Type::I64, Type::F64, Type::Bool, Type::Void] {
            for err in [Type::I64, Type::F64, Type::Bool, Type::Void] {
                let result = Type::Generic("Result".into(), vec![ok.clone(), err]);
                assert!(is_scalar_pair(&result));
                assert_eq!(option_repr(&result, &TypeMap::new()), None);
            }
        }
    }

    #[test]
    fn reference_mixed_and_nested_results_do_not_use_scalar_pairs() {
        for payload in [
            Type::String,
            Type::Array(Box::new(Type::I64)),
            option(Type::I64),
        ] {
            for args in [
                vec![payload.clone(), Type::I64],
                vec![Type::I64, payload.clone()],
                vec![payload.clone(), payload],
            ] {
                assert!(!is_scalar_pair(&Type::Generic("Result".into(), args)));
            }
        }
        let mut nested = Type::I64;
        for depth in 1..=128 {
            nested = option(nested);
            assert_eq!(is_scalar_pair(&nested), depth == 1);
        }
    }

    #[test]
    fn non_option_types_have_no_option_representation() {
        assert_eq!(option_repr(&Type::String, &TypeMap::new()), None);
    }
}
