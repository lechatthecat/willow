//! Structural generic constraints, with bound parameters separate from named types.
//!
//! Signatures are flat preorder streams. Construction and matching visit each
//! type node once and use explicit work stacks, including for deep type trees.
use std::collections::HashMap;

use crate::parser::ast::Type;

#[derive(Debug, PartialEq, Eq)]
enum Shape<'a> {
    Param(usize),
    I64,
    F64,
    Bool,
    String,
    Void,
    Never,
    Named(&'a str),
    Array,
    Generic(&'a str, usize),
    Function(usize),
    Closure(usize),
}

/// `Param` is only produced for a bound named leaf, never a nominal type head.
struct Signature<'a> {
    nodes: Vec<Shape<'a>>,
}

fn shape(ty: &Type) -> Shape<'_> {
    match ty {
        Type::I64 => Shape::I64,
        Type::F64 => Shape::F64,
        Type::Bool => Shape::Bool,
        Type::String => Shape::String,
        Type::Void => Shape::Void,
        Type::Never => Shape::Never,
        Type::Named(name) => Shape::Named(name),
        Type::Array(_) => Shape::Array,
        Type::Generic(name, args) => Shape::Generic(name, args.len()),
        Type::Fn(args, _) => Shape::Function(args.len()),
        Type::Closure(args, _) => Shape::Closure(args.len()),
    }
}

fn push_children<'a>(ty: &'a Type, pending: &mut Vec<&'a Type>) {
    match ty {
        Type::Array(element) => pending.push(element),
        Type::Generic(_, args) => pending.extend(args.iter().rev()),
        Type::Fn(args, result) | Type::Closure(args, result) => {
            pending.push(result);
            pending.extend(args.iter().rev());
        }
        _ => {}
    }
}

impl<'a> Signature<'a> {
    fn new(types: &'a [Type], parameters: &HashMap<&str, usize>) -> Self {
        let mut pending: Vec<_> = types.iter().rev().collect();
        let mut nodes = Vec::new();
        while let Some(ty) = pending.pop() {
            if let Type::Named(name) = ty
                && let Some(&index) = parameters.get(name.as_str())
            {
                nodes.push(Shape::Param(index));
            } else {
                nodes.push(shape(ty));
                push_children(ty, &mut pending);
            }
        }
        Self { nodes }
    }
}

fn validate_argument(name: &str, ty: &Type) -> Result<(), String> {
    if matches!(ty, Type::Void | Type::Never) {
        return Err(format!("type argument for `{name}` must be a value type"));
    }
    Ok(())
}

/// Infer all declared parameters from positional argument types. A nonempty
/// explicit list fixes every parameter and must have exactly the declared arity.
/// Repeated occurrences impose equality, including occurrences nested in type
/// constructors or callable signatures. Concrete nodes must match exactly.
///
/// With S signature nodes, A visited actual nodes and B total stored binding
/// size, work is O(S + A + B), plus equality checks for repeated bindings (each
/// checks its corresponding actual subtree); space is O(S + A + B).
pub(crate) fn infer(
    type_params: &[String],
    parameter_types: &[Type],
    actual_types: &[Type],
    explicit_type_args: &[Type],
) -> Result<HashMap<String, Type>, String> {
    infer_inner(
        type_params,
        parameter_types,
        actual_types,
        explicit_type_args,
        false,
    )
}

/// Infer an enum variant's available constraints, leaving parameters absent
/// from its payload as `Void` holes for the existing contextual enum resolver.
/// An explicit or inferred `Void` argument remains invalid; only an absent
/// constraint creates a hole. Conflicting constraints are never discarded.
pub(crate) fn infer_partial(
    type_params: &[String],
    parameter_types: &[Type],
    actual_types: &[Type],
    explicit_type_args: &[Type],
) -> Result<HashMap<String, Type>, String> {
    infer_inner(
        type_params,
        parameter_types,
        actual_types,
        explicit_type_args,
        true,
    )
}

fn infer_inner(
    type_params: &[String],
    parameter_types: &[Type],
    actual_types: &[Type],
    explicit_type_args: &[Type],
    allow_unresolved: bool,
) -> Result<HashMap<String, Type>, String> {
    if parameter_types.len() != actual_types.len() {
        return Err(format!(
            "expected {} value arguments, got {}",
            parameter_types.len(),
            actual_types.len()
        ));
    }
    let mut parameters = HashMap::with_capacity(type_params.len());
    for (index, name) in type_params.iter().enumerate() {
        if parameters.insert(name.as_str(), index).is_some() {
            return Err(format!("duplicate type parameter `{name}`"));
        }
    }
    if !explicit_type_args.is_empty() && explicit_type_args.len() != type_params.len() {
        return Err(format!(
            "expected {} type arguments, got {}",
            type_params.len(),
            explicit_type_args.len()
        ));
    }
    let mut bindings: Vec<Option<Type>> = vec![None; type_params.len()];
    for (index, ty) in explicit_type_args.iter().enumerate() {
        validate_argument(&type_params[index], ty)?;
        bindings[index] = Some(ty.clone());
    }
    let signature = Signature::new(parameter_types, &parameters);
    let mut actual: Vec<_> = actual_types.iter().rev().collect();
    for node in signature.nodes {
        let ty = actual.pop().expect("matching shapes preserve child counts");
        if let Shape::Param(index) = node {
            validate_argument(&type_params[index], ty)?;
            if let Some(previous) = &bindings[index] {
                if previous != ty {
                    return Err(format!(
                        "conflicting types for type parameter `{}`",
                        type_params[index]
                    ));
                }
            } else {
                bindings[index] = Some(ty.clone());
            }
        } else {
            if node != shape(ty) {
                return Err("argument type does not match generic parameter signature".into());
            }
            push_children(ty, &mut actual);
        }
    }
    debug_assert!(actual.is_empty());
    type_params
        .iter()
        .cloned()
        .zip(bindings)
        .map(|(name, ty)| {
            ty.or_else(|| allow_unresolved.then_some(Type::Void))
                .map(|ty| (name.clone(), ty))
                .ok_or_else(|| {
                    format!("cannot infer type parameter `{name}`; supply type arguments")
                })
        })
        .collect()
}

/// Substitute bound leaves only. `Type::substitute_names` uses an explicit
/// traversal stack and does not replace nominal generic constructor names.
pub(crate) fn substitute(ty: &Type, bindings: &HashMap<String, Type>) -> Type {
    ty.substitute_names(|name| bindings.get(name).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> Type {
        Type::Named(name.into())
    }
    fn generic(name: &str, args: Vec<Type>) -> Type {
        Type::Generic(name.into(), args)
    }
    fn run(
        params: &[&str],
        formals: &[Type],
        actual: &[Type],
        explicit: &[Type],
    ) -> Result<HashMap<String, Type>, String> {
        infer(
            &params.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
            formals,
            actual,
            explicit,
        )
    }

    #[test]
    fn p01_scalar_parameter() {
        assert_eq!(
            run(&["T"], &[named("T")], &[Type::I64], &[]).unwrap()["T"],
            Type::I64
        );
    }
    #[test]
    fn p02_multiple_parameters() {
        let b = run(
            &["T", "U"],
            &[named("T"), named("U")],
            &[Type::I64, Type::String],
            &[],
        )
        .unwrap();
        assert_eq!(b["T"], Type::I64);
        assert_eq!(b["U"], Type::String);
    }
    #[test]
    fn p03_repeated_consistent_parameter() {
        assert!(
            run(
                &["T"],
                &[named("T"), named("T")],
                &[Type::Bool, Type::Bool],
                &[]
            )
            .is_ok()
        );
    }
    #[test]
    fn p04_repeated_conflicting_parameter() {
        assert!(
            run(
                &["T"],
                &[named("T"), named("T")],
                &[Type::Bool, Type::I64],
                &[]
            )
            .is_err()
        );
    }
    #[test]
    fn p05_nested_array_parameter() {
        let b = run(
            &["T"],
            &[Type::Array(Box::new(named("T")))],
            &[Type::Array(Box::new(Type::F64))],
            &[],
        )
        .unwrap();
        assert_eq!(b["T"], Type::F64);
    }
    #[test]
    fn p06_nested_generic_parameter() {
        let b = run(
            &["T"],
            &[generic("Box", vec![generic("Array", vec![named("T")])])],
            &[generic("Box", vec![generic("Array", vec![Type::String])])],
            &[],
        )
        .unwrap();
        assert_eq!(b["T"], Type::String);
    }
    #[test]
    fn p07_nested_repeated_conflict() {
        assert!(
            run(
                &["T"],
                &[generic("Pair", vec![named("T"), named("T")])],
                &[generic("Pair", vec![Type::I64, Type::String])],
                &[]
            )
            .is_err()
        );
    }
    #[test]
    fn p08_function_argument_and_result() {
        let b = run(
            &["T", "U"],
            &[Type::Fn(vec![named("T")], Box::new(named("U")))],
            &[Type::Fn(vec![Type::Bool], Box::new(Type::String))],
            &[],
        )
        .unwrap();
        assert_eq!(b["T"], Type::Bool);
        assert_eq!(b["U"], Type::String);
    }
    #[test]
    fn p09_closure_argument_and_result() {
        assert!(
            run(
                &["T"],
                &[Type::Closure(vec![named("T")], Box::new(named("T")))],
                &[Type::Closure(vec![Type::I64], Box::new(Type::I64))],
                &[]
            )
            .is_ok()
        );
    }
    #[test]
    fn p10_function_closure_distinct() {
        assert!(
            run(
                &["T"],
                &[Type::Fn(vec![named("T")], Box::new(Type::Void))],
                &[Type::Closure(vec![Type::I64], Box::new(Type::Void))],
                &[]
            )
            .is_err()
        );
    }
    #[test]
    fn p11_explicit_matching() {
        assert!(run(&["T"], &[named("T")], &[Type::String], &[Type::String]).is_ok());
    }
    #[test]
    fn p12_explicit_conflict() {
        assert!(run(&["T"], &[named("T")], &[Type::I64], &[Type::String]).is_err());
    }
    #[test]
    fn p13_explicit_wrong_arity() {
        assert!(run(&["T", "U"], &[], &[], &[Type::I64]).is_err());
        assert!(run(&["T"], &[], &[], &[Type::I64, Type::I64]).is_err());
    }
    #[test]
    fn p14_unresolved_parameter() {
        assert!(
            run(&["T", "U"], &[named("T")], &[Type::I64], &[])
                .unwrap_err()
                .contains("`U`")
        );
    }
    #[test]
    fn p15_explicit_resolves_return_only_parameter() {
        assert_eq!(
            run(&["T"], &[], &[], &[Type::Bool]).unwrap()["T"],
            Type::Bool
        );
    }
    #[test]
    fn p16_void_type_argument_rejected() {
        assert!(run(&["T"], &[], &[], &[Type::Void]).is_err());
        assert!(run(&["T"], &[named("T")], &[Type::Void], &[]).is_err());
    }
    #[test]
    fn p17_never_type_argument_rejected() {
        assert!(run(&["T"], &[], &[], &[Type::Never]).is_err());
        assert!(run(&["T"], &[named("T")], &[Type::Never], &[]).is_err());
    }
    #[test]
    fn p18_concrete_nominal_identity() {
        assert!(run(&[], &[named("a::X")], &[named("b::X")], &[]).is_err());
        assert!(run(&[], &[named("a::X")], &[named("a::X")], &[]).is_ok());
    }
    #[test]
    fn p19_concrete_primitive_mismatch() {
        assert!(run(&[], &[Type::I64], &[Type::F64], &[]).is_err());
    }
    #[test]
    fn p20_generic_head_and_arity_mismatch() {
        assert!(
            run(
                &["T"],
                &[generic("Box", vec![named("T")])],
                &[generic("Other", vec![Type::I64])],
                &[]
            )
            .is_err()
        );
        assert!(
            run(
                &["T"],
                &[generic("Box", vec![named("T")])],
                &[generic("Box", vec![])],
                &[]
            )
            .is_err()
        );
    }
    #[test]
    fn p21_callable_arity_mismatch() {
        assert!(
            run(
                &["T"],
                &[Type::Fn(vec![named("T")], Box::new(Type::Void))],
                &[Type::Fn(vec![], Box::new(Type::Void))],
                &[]
            )
            .is_err()
        );
    }
    #[test]
    fn p22_value_argument_arity_mismatch() {
        assert!(run(&["T"], &[named("T")], &[], &[]).is_err());
    }
    #[test]
    fn p23_duplicate_bound_names_rejected() {
        assert!(run(&["T", "T"], &[], &[], &[Type::I64, Type::I64]).is_err());
    }
    #[test]
    fn p24_generic_heads_are_not_parameters() {
        let b = run(
            &["T"],
            &[generic("T", vec![named("T")])],
            &[generic("T", vec![Type::I64])],
            &[],
        )
        .unwrap();
        assert_eq!(
            substitute(&generic("T", vec![named("T")]), &b),
            generic("T", vec![Type::I64])
        );
    }
    #[test]
    fn p25_substitution_preserves_unbound_names() {
        let b = HashMap::from([("T".into(), Type::Bool)]);
        assert_eq!(
            substitute(&Type::Fn(vec![named("T")], Box::new(named("Concrete"))), &b),
            Type::Fn(vec![Type::Bool], Box::new(named("Concrete")))
        );
    }
    #[test]
    fn p26_deep_shapes_are_stack_independent() {
        let mut formal = named("T");
        let mut actual = Type::I64;
        for _ in 0..20_000 {
            formal = Type::Array(Box::new(formal));
            actual = Type::Array(Box::new(actual));
        }
        let b = run(
            &["T"],
            std::slice::from_ref(&formal),
            std::slice::from_ref(&actual),
            &[],
        )
        .unwrap();
        assert_eq!(substitute(&formal, &b), actual);
    }
    #[test]
    fn p27_concrete_void_callable_return_is_valid() {
        assert!(
            run(
                &["T"],
                &[Type::Fn(vec![named("T")], Box::new(Type::Void))],
                &[Type::Fn(vec![Type::String], Box::new(Type::Void))],
                &[]
            )
            .is_ok()
        );
    }
    #[test]
    fn p28_flat_signature_has_one_node_per_input_node() {
        for size in [1, 16, 256, 4096] {
            let formals = vec![generic("Pair", vec![named("T"), named("T")]); size];
            let signature = Signature::new(&formals, &HashMap::from([("T", 0)]));
            assert_eq!(signature.nodes.len(), size * 3);
            let actual = vec![generic("Pair", vec![Type::I64, Type::I64]); size];
            assert_eq!(run(&["T"], &formals, &actual, &[]).unwrap()["T"], Type::I64);
        }
    }

    #[test]
    fn p29_partial_result_leaves_only_missing_parameter_unresolved() {
        let params = vec!["T".into(), "E".into()];
        let bindings = infer_partial(&params, &[named("T")], &[Type::I64], &[]).unwrap();
        assert_eq!(bindings["T"], Type::I64);
        assert_eq!(bindings["E"], Type::Void);
        assert!(infer(&params, &[named("T")], &[Type::I64], &[]).is_err());
    }

    #[test]
    fn p30_partial_fieldless_variant_retains_contextual_hole() {
        let bindings = infer_partial(&["T".into()], &[], &[], &[]).unwrap();
        assert_eq!(bindings["T"], Type::Void);
    }

    #[test]
    fn p31_partial_inference_does_not_hide_conflicts() {
        assert!(
            infer_partial(
                &["T".into(), "E".into()],
                &[named("T"), named("T")],
                &[Type::I64, Type::Bool],
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn p32_partial_explicit_arity_and_value_validation_remain_strict() {
        assert!(infer_partial(&["T".into(), "E".into()], &[], &[], &[Type::I64]).is_err());
        assert!(infer_partial(&["T".into()], &[], &[], &[Type::Void]).is_err());
        assert!(infer_partial(&["T".into()], &[named("T")], &[Type::Void], &[]).is_err());
        assert_eq!(
            infer_partial(&["T".into()], &[], &[], &[Type::String]).unwrap()["T"],
            Type::String
        );
    }
}
