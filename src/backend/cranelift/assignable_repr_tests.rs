//! Exhaustive representation and catch-all reachability regression matrix.
use super::super::type_helpers::{storage_is_gc_managed, value_slot_kind};
use super::super::type_index::TypeMap;
use super::*;
use crate::semantic::symbols::{EnumInfo, EnumVariantInfo};
use willow_abi::SlotKind;

fn builtin_case(id: B) -> (Vec<Type>, SlotKind) {
    // No wildcard: a new builtin requires a representation expectation.
    match id {
        B::Option => (vec![Type::I64], SlotKind::ScalarPair),
        B::Result => (vec![Type::I64, Type::Bool], SlotKind::ScalarPair),
        B::Future => (vec![Type::Void], SlotKind::Word),
        B::Task
        | B::TaskResult
        | B::JoinHandle
        | B::Array
        | B::FrozenArray
        | B::Channel
        | B::BlockingCell
        | B::BlockingRwCell => (vec![Type::String], SlotKind::GcRef),
        B::Map | B::FrozenMap => (vec![Type::String, Type::I64], SlotKind::GcRef),
        B::AtomicI64
        | B::AtomicBool
        | B::CancellationToken
        | B::TaskScope
        | B::Cancelled
        | B::IoError => (vec![], SlotKind::GcRef),
    }
}

#[test]
fn representation_matrix_all_types_and_builtins() {
    let mut enums = TypeMap::new();
    for (name, payload) in [("Tag", vec![]), ("Payload", vec![Type::String])] {
        enums.insert(
            name,
            EnumInfo {
                name: name.into(),
                public: true,
                type_params: vec![],
                declaration_span: Span::dummy(),
                variants: vec![EnumVariantInfo {
                    name: "V".into(),
                    payload_types: payload,
                    tag: 0,
                    declaration_span: Span::dummy(),
                }],
            },
        );
    }
    let mut cases = vec![
        (Type::I64, types::I64, SlotKind::Word),
        (Type::F64, types::F64, SlotKind::Word),
        (Type::Bool, types::I8, SlotKind::Word),
        (Type::Void, types::I8, SlotKind::Word),
        (Type::Never, types::I64, SlotKind::Word),
        (Type::String, types::I64, SlotKind::GcRef),
        (
            Type::Array(Box::new(Type::String)),
            types::I64,
            SlotKind::GcRef,
        ),
        (
            Type::Fn(vec![], Box::new(Type::Void)),
            types::I64,
            SlotKind::Word,
        ),
        (
            Type::Closure(vec![], Box::new(Type::Void)),
            types::I64,
            SlotKind::GcRef,
        ),
        (Type::Named("Class".into()), types::I64, SlotKind::GcRef),
        (Type::Named("Tag".into()), types::I64, SlotKind::Word),
        (Type::Named("Payload".into()), types::I64, SlotKind::GcRef),
        (
            Type::Generic("User".into(), vec![Type::I64]),
            types::I64,
            SlotKind::GcRef,
        ),
    ];
    for id in [
        B::Option,
        B::Result,
        B::Task,
        B::TaskResult,
        B::Future,
        B::JoinHandle,
        B::Array,
        B::FrozenArray,
        B::Map,
        B::FrozenMap,
        B::Channel,
        B::BlockingCell,
        B::BlockingRwCell,
        B::AtomicI64,
        B::AtomicBool,
        B::CancellationToken,
        B::TaskScope,
        B::Cancelled,
        B::IoError,
    ] {
        let (args, slot) = builtin_case(id);
        let clif = if slot == SlotKind::ScalarPair {
            types::I128
        } else {
            types::I64
        };
        cases.push((id.apply(args), clif, slot));
    }
    for inner in [
        Type::String,
        Type::Named("Tag".into()),
        B::Future.apply(vec![Type::Void]),
        B::Option.apply(vec![Type::String]),
    ] {
        cases.push((B::Option.apply(vec![inner]), types::I64, SlotKind::GcRef));
    }
    assert_eq!(cases.len(), 36);
    for (ty, clif, slot) in &cases {
        assert_eq!(clif_type(types::I64, ty), *clif, "{ty:?}");
        assert_eq!(value_slot_kind(ty, &enums), *slot, "{ty:?}");
        assert_eq!(
            storage_is_gc_managed(ty, &enums),
            *slot != SlotKind::Word,
            "{ty:?}"
        );
        assert_eq!(
            is_gc_managed(ty, &enums),
            *slot == SlotKind::GcRef,
            "{ty:?}"
        );
        assert!(assignable_repr(ty, ty), "{ty:?}");
    }
    // Every nominal/generic ordered pair visits the real caller. ALL generics
    // (including Future and its Options) must bypass the panic arm.
    for (a, _, _) in &cases {
        for (b, _, _) in &cases {
            if matches!(a, Type::Named(_) | Type::Generic(..))
                || matches!(b, Type::Named(_) | Type::Generic(..))
            {
                assert_eq!(assignable_repr(a, b), a == b, "{a:?} / {b:?}");
            }
        }
    }
}

#[test]
fn callable_and_heap_representations_are_not_interchangeable() {
    let function = Type::Fn(vec![Type::I64], Box::new(Type::String));
    let closure = Type::Closure(vec![Type::I64], Box::new(Type::String));
    for callable in [&function, &closure] {
        for other in [
            Type::I64,
            Type::String,
            Type::Array(Box::new(Type::String)),
            Type::Never,
        ] {
            assert!(!assignable_repr(callable, &other));
            assert!(!assignable_repr(&other, callable));
        }
    }
    assert!(!assignable_repr(&function, &closure));
    assert!(!assignable_repr(&closure, &function));
    assert!(!assignable_repr(
        &function,
        &Type::Fn(vec![Type::Bool], Box::new(Type::String))
    ));
    assert!(!assignable_repr(
        &closure,
        &Type::Closure(vec![Type::I64], Box::new(Type::Bool))
    ));
}

#[test]
fn callable_equality_scaling_reuses_stack_independent_type_equality() {
    use crate::parser::ast::Type as T;
    use std::cell::Cell;
    struct CountedName<'a>(&'a Cell<usize>);
    impl PartialEq for CountedName<'_> {
        fn eq(&self, _: &Self) -> bool {
            self.0.set(self.0.get() + 1);
            true
        }
    }
    for n in [1, 16, 64, 256] {
        let comparisons = Cell::new(0);
        // Count the actual shared Type::eq work, with deep and wide shapes.
        let mut deep_a = T::I64;
        let mut deep_b = T::I64;
        for _ in 0..n {
            deep_a = T::Generic(CountedName(&comparisons), vec![deep_a]);
            deep_b = T::Generic(CountedName(&comparisons), vec![deep_b]);
        }
        let a = T::Closure(vec![deep_a], Box::new(T::Void));
        let b = T::Closure(vec![deep_b], Box::new(T::Void));
        for _ in 0..8 {
            assert!(a == b);
        }
        assert_eq!(comparisons.get(), 8 * n);
        comparisons.set(0);
        let a = T::Fn(
            (0..n)
                .map(|_| T::Named(CountedName(&comparisons)))
                .collect(),
            Box::new(T::Void),
        );
        let b = T::Fn(
            (0..n)
                .map(|_| T::Named(CountedName(&comparisons)))
                .collect(),
            Box::new(T::Void),
        );
        for _ in 0..8 {
            assert!(a == b);
        }
        assert_eq!(comparisons.get(), 8 * n);
        eprintln!(
            "callable equality n={n} repeated=8 deep_names={} wide_names={}",
            8 * n,
            comparisons.get()
        );
    }
}
