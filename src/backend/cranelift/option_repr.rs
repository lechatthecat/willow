//! One representation decision for every compiler path that constructs or
//! inspects a Willow `Option<T>`.
//!
//! A one-word GC reference already has an invalid bit-pattern available:
//! zero.  For payloads whose valid values are guaranteed non-null we therefore
//! encode `None` as zero and `Some(value)` as `value` itself.  Other payloads
//! keep the ordinary tagged heap-enum layout.  In particular, an outer
//! `Option<Option<T>>` is always boxed so `None` and `Some(None)` remain
//! distinguishable even when the inner option uses the nullable-pointer niche.

use std::collections::HashMap;

use crate::parser::ast::Type;
use crate::semantic::builtin_types::{self, BuiltinTypeId};
use crate::semantic::symbols::EnumInfo;

use super::type_helpers::is_gc_managed;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OptionRepr {
    NullableGcPointer,
    BoxedTaggedEnum,
}

pub(crate) fn option_inner(ty: &Type) -> Option<&Type> {
    builtin_types::unary_arg(ty, BuiltinTypeId::Option)
}

/// Return the representation of an instantiated `Option<T>`.
///
/// A nested Option is excluded because zero can already be a valid inner
/// representation. The remaining GC-managed types are represented by non-null
/// heap pointers when valid, so zero is available for `None`.
pub(crate) fn option_repr(ty: &Type, enum_infos: &HashMap<String, EnumInfo>) -> Option<OptionRepr> {
    let inner = option_inner(ty)?;
    let niche = option_inner(inner).is_none() && is_gc_managed(inner, enum_infos);
    Some(if niche {
        OptionRepr::NullableGcPointer
    } else {
        OptionRepr::BoxedTaggedEnum
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(inner: Type) -> Type {
        Type::Generic("Option".to_string(), vec![inner])
    }

    #[test]
    fn gc_references_use_the_nullable_pointer_niche() {
        let enums = HashMap::new();
        assert_eq!(
            option_repr(&option(Type::String), &enums),
            Some(OptionRepr::NullableGcPointer)
        );
        assert_eq!(
            option_repr(&option(Type::Array(Box::new(Type::I64))), &enums),
            Some(OptionRepr::NullableGcPointer)
        );
        assert_eq!(
            option_repr(&option(Type::Named("User".to_string())), &enums),
            Some(OptionRepr::NullableGcPointer)
        );
    }

    #[test]
    fn scalars_and_nested_options_stay_boxed() {
        let enums = HashMap::new();
        assert_eq!(
            option_repr(&option(Type::I64), &enums),
            Some(OptionRepr::BoxedTaggedEnum)
        );
        assert_eq!(
            option_repr(&option(option(Type::String)), &enums),
            Some(OptionRepr::BoxedTaggedEnum)
        );
    }

    #[test]
    fn non_option_types_have_no_option_representation() {
        assert_eq!(option_repr(&Type::String, &HashMap::new()), None);
    }
}
