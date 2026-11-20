//! Structural generic constraints, with bound parameters separate from named types.
//!
//! Signatures are flat preorder streams. Construction and matching visit each
//! type node once and use explicit work stacks, including for deep type trees.
use std::collections::HashMap;

use crate::parser::ast::Type;

/// Compiler-only unresolved constructor slot; source syntax cannot spell it.
/// Unlike `Void`, this carries no concrete type constraint.
pub(crate) const ENUM_HOLE: &str = "\0enum-inference-hole";

pub(crate) fn enum_hole() -> Type {
    Type::Named(ENUM_HOLE.into())
}

pub(crate) fn is_enum_hole(ty: &Type) -> bool {
    matches!(ty, Type::Named(name) if name == ENUM_HOLE)
}

pub(crate) fn has_enum_hole(ty: &Type) -> bool {
    let mut pending = vec![ty];
    while let Some(ty) = pending.pop() {
        if is_enum_hole(ty) {
            return true;
        }
        push_children(ty, &mut pending);
    }
    false
}

/// Merge only explicitly marked missing constraints. Concrete void is exact.
/// Visit each pair once; the result owns the type nodes it records.
pub(crate) fn merge_enum_holes(left: &Type, right: &Type) -> Option<Type> {
    let mut result = left.clone();
    let mut pending = vec![(&mut result, right)];
    while let Some((left, right)) = pending.pop() {
        if is_enum_hole(left) {
            *left = right.clone();
            continue;
        }
        if is_enum_hole(right) {
            continue;
        }
        if shape(left) != shape(right) {
            return None;
        }
        match (left, right) {
            (Type::Generic(_, aa), Type::Generic(_, bb)) => {
                pending.extend(aa.iter_mut().zip(bb));
            }
            (Type::Array(a), Type::Array(b)) => pending.push((a, b)),
            (Type::FixedArray(a, n), Type::FixedArray(b, m)) if n == m => pending.push((a, b)),
            (Type::Fn(aa, ar), Type::Fn(ba, br))
            | (Type::Closure(aa, ar), Type::Closure(ba, br)) => {
                pending.push((ar, br));
                pending.extend(aa.iter_mut().zip(ba));
            }
            _ => {}
        }
    }
    Some(result)
}

#[derive(Debug, PartialEq, Eq)]
enum Shape<'a> {
    Param(usize),
    I64,
    I8,
    I16,
    I32,
    U16,
    U32,
    U64,
    Char,
    U8,
    F64,
    Bool,
    String,
    Void,
    Never,
    Named(&'a str),
    Array,
    FixedArray(u64),
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
        Type::I8 => Shape::I8,
        Type::I16 => Shape::I16,
        Type::I32 => Shape::I32,
        Type::U16 => Shape::U16,
        Type::U32 => Shape::U32,
        Type::U64 => Shape::U64,
        Type::Char => Shape::Char,
        Type::U8 => Shape::U8,
        Type::I64 => Shape::I64,
        Type::F64 => Shape::F64,
        Type::Bool => Shape::Bool,
        Type::String => Shape::String,
        Type::Void => Shape::Void,
        Type::Never => Shape::Never,
        Type::Named(name) => Shape::Named(name),
        Type::Array(_) => Shape::Array,
        Type::FixedArray(_, n) => Shape::FixedArray(*n),
        Type::Generic(name, args) => Shape::Generic(name, args.len()),
        Type::Fn(args, _) => Shape::Function(args.len()),
        Type::Closure(args, _) => Shape::Closure(args.len()),
    }
}

fn push_children<'a>(ty: &'a Type, pending: &mut Vec<&'a Type>) {
    match ty {
        Type::Array(element) | Type::FixedArray(element, _) => pending.push(element),
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
/// constructors or callable signatures. Concrete nodes must match exactly,
/// except callable kinds, which follow subtyping at each node's variance: a
/// shareable `fn` value serves where a task-local one is expected, and a
/// parameter seen only at covariant (contravariant) positions binds the
/// join (meet) of the callable kinds it sees.
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
    let mut inference = Inference::new(type_params, explicit_type_args)?;
    for (formal, actual) in parameter_types.iter().zip(actual_types) {
        inference.constrain(formal, actual)?;
    }
    inference.finish(allow_unresolved)
}

/// Incremental form of [`infer`]: constraints arrive one value argument at a
/// time, so a caller can read what earlier arguments bound before it types a
/// later one (a lambda whose parameter types come from `T`). Feeding every
/// pair in order and then calling `finish(false)` is exactly `infer`; each
/// formal and actual node is still visited once, so the total stays
/// O(S + A + B) however the arguments are split.
pub(crate) struct Inference<'a> {
    type_params: &'a [String],
    parameters: HashMap<&'a str, usize>,
    bindings: Vec<Option<Type>>,
    /// The variance every use of a bound parameter shared so far. Only a
    /// parameter used solely at covariant (or solely at contravariant)
    /// positions may move to a wider (narrower) callable kind; an explicit
    /// or mixed one is fixed.
    uses: Vec<Variance>,
}

/// Where a node sits relative to the value passed: callable parameters flip
/// it, containers and generic arguments fix it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Variance {
    Co,
    Contra,
    Inv,
}

impl Variance {
    fn flip(self) -> Self {
        match self {
            Self::Co => Self::Contra,
            Self::Contra => Self::Co,
            Self::Inv => Self::Inv,
        }
    }

    fn join(self, other: Self) -> Self {
        if self == other { self } else { Self::Inv }
    }

    /// Whether a value of callable kind `actual` serves where `formal` is
    /// written: the shareable kind is a subtype of the task-local one.
    fn kind_fits(self, formal_closure: bool, actual_closure: bool) -> bool {
        match self {
            Self::Co => formal_closure || !actual_closure,
            Self::Contra => !formal_closure || actual_closure,
            Self::Inv => formal_closure == actual_closure,
        }
    }

    /// The kind that both `left` and `right` fit at this position.
    fn bound_kind(self, left_closure: bool, right_closure: bool) -> Option<bool> {
        match self {
            Self::Co => Some(left_closure || right_closure),
            Self::Contra => Some(left_closure && right_closure),
            Self::Inv => (left_closure == right_closure).then_some(left_closure),
        }
    }
}

/// Children of `ty` with their variance, in the order `push_children` uses.
fn push_children_with<'a>(
    ty: &'a Type,
    variance: Variance,
    pending: &mut Vec<(&'a Type, Variance)>,
) {
    match ty {
        Type::Array(element) | Type::FixedArray(element, _) => {
            pending.push((element, Variance::Inv))
        }
        Type::Generic(_, args) => pending.extend(args.iter().rev().map(|arg| (arg, Variance::Inv))),
        Type::Fn(args, result) | Type::Closure(args, result) => {
            pending.push((result, variance));
            pending.extend(args.iter().rev().map(|arg| (arg, variance.flip())));
        }
        _ => {}
    }
}

/// The callable kind of a shape, when it is one, with its arity.
fn callable_shape(shape: &Shape<'_>) -> Option<(usize, bool)> {
    match shape {
        Shape::Function(n) => Some((*n, false)),
        Shape::Closure(n) => Some((*n, true)),
        _ => None,
    }
}

/// Reconcile a parameter's binding with another occurrence's type that may
/// differ only in callable kinds. With `widen`, each kind becomes the one both
/// sides fit at its position (the join at a covariant use, the meet at a
/// contravariant one); otherwise `actual` must fit the binding unchanged.
/// Visits each node pair once.
fn reconcile_callable_kinds(
    binding: &Type,
    actual: &Type,
    variance: Variance,
    widen: bool,
) -> Option<Type> {
    let mut result = binding.clone();
    let mut pending = vec![(&mut result, actual, variance)];
    while let Some((left, right, variance)) = pending.pop() {
        let (left_shape, right_shape) = (shape(left), shape(right));
        if let (Some((ln, lc)), Some((rn, rc))) =
            (callable_shape(&left_shape), callable_shape(&right_shape))
        {
            if ln != rn {
                return None;
            }
            let kind = if widen {
                variance.bound_kind(lc, rc)?
            } else if variance.kind_fits(lc, rc) {
                lc
            } else {
                return None;
            };
            if kind != lc {
                let (Type::Fn(args, result) | Type::Closure(args, result)) = left else {
                    unreachable!("callable shape")
                };
                let (args, result) = (
                    std::mem::take(args),
                    std::mem::replace(result, Box::new(Type::Void)),
                );
                *left = if kind {
                    Type::Closure(args, result)
                } else {
                    Type::Fn(args, result)
                };
            }
        } else if left_shape != right_shape {
            return None;
        }
        match (left, right) {
            (Type::Generic(_, aa), Type::Generic(_, bb)) => {
                pending.extend(aa.iter_mut().zip(bb).map(|(a, b)| (a, b, Variance::Inv)));
            }
            (Type::Array(a), Type::Array(b)) | (Type::FixedArray(a, _), Type::FixedArray(b, _)) => {
                pending.push((a, b, Variance::Inv))
            }
            (
                Type::Fn(aa, ar) | Type::Closure(aa, ar),
                Type::Fn(ba, br) | Type::Closure(ba, br),
            ) => {
                pending.push((ar, br, variance));
                pending.extend(aa.iter_mut().zip(ba).map(|(a, b)| (a, b, variance.flip())));
            }
            _ => {}
        }
    }
    Some(result)
}

impl<'a> Inference<'a> {
    pub(crate) fn new(
        type_params: &'a [String],
        explicit_type_args: &[Type],
    ) -> Result<Self, String> {
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
        Ok(Self {
            type_params,
            parameters,
            bindings,
            uses: vec![Variance::Inv; type_params.len()],
        })
    }

    /// The type bound to `name` so far, if any.
    pub(crate) fn binding(&self, name: &str) -> Option<&Type> {
        self.parameters
            .get(name)
            .and_then(|&index| self.bindings[index].as_ref())
    }

    /// Match one formal parameter type against one actual argument type.
    pub(crate) fn constrain(&mut self, formal: &Type, actual: &Type) -> Result<(), String> {
        self.constrain_inner(formal, actual, false)
    }

    /// An inferred enum constructor may leave absent payload slots as holes.
    /// Only compiler-marked slots are holes; concrete void always constrains.
    pub(crate) fn constrain_partial(&mut self, formal: &Type, actual: &Type) -> Result<(), String> {
        self.constrain_inner(formal, actual, true)
    }

    fn constrain_inner(
        &mut self,
        formal: &Type,
        actual: &Type,
        partial: bool,
    ) -> Result<(), String> {
        let signature = Signature::new(std::slice::from_ref(formal), &self.parameters);
        let mut pending = vec![(actual, Variance::Co)];
        let mut nodes = signature.nodes.into_iter();
        while let Some(node) = nodes.next() {
            let (ty, variance) = pending
                .pop()
                .expect("matching shapes preserve child counts");
            let hole = partial && is_enum_hole(ty);
            if hole {
                let children = |node: &Shape<'_>| match node {
                    Shape::Array => 1,
                    Shape::Generic(_, n) => *n,
                    Shape::Function(n) | Shape::Closure(n) => n + 1,
                    _ => 0,
                };
                let mut remaining = children(&node);
                while remaining != 0 {
                    remaining = remaining - 1 + children(&nodes.next().expect("signature subtree"));
                }
                continue;
            }
            if let Shape::Param(index) = node {
                validate_argument(&self.type_params[index], ty)?;
                if let Some(previous) = &self.bindings[index] {
                    let uses = self.uses[index];
                    self.uses[index] = uses.join(variance);
                    if previous == ty {
                        continue;
                    }
                    let widen = uses == variance && variance != Variance::Inv;
                    if partial && let Some(merged) = merge_enum_holes(previous, ty) {
                        self.bindings[index] = Some(merged);
                    } else if let Some(merged) =
                        reconcile_callable_kinds(previous, ty, variance, widen)
                    {
                        self.bindings[index] = Some(merged);
                    } else {
                        return Err(format!(
                            "conflicting types for type parameter `{}`",
                            self.type_params[index]
                        ));
                    }
                } else {
                    self.bindings[index] = Some(ty.clone());
                    self.uses[index] = variance;
                }
            } else {
                let found = shape(ty);
                let fits = match (callable_shape(&node), callable_shape(&found)) {
                    (Some((formal, formal_closure)), Some((actual, actual_closure))) => {
                        formal == actual && variance.kind_fits(formal_closure, actual_closure)
                    }
                    _ => node == found,
                };
                if !fits {
                    return Err(mismatch_message(&node, &found, variance));
                }
                push_children_with(ty, variance, &mut pending);
            }
        }
        debug_assert!(pending.is_empty());
        Ok(())
    }

    /// Every parameter's binding. With `allow_unresolved`, one that no
    /// constraint reached becomes a `Void` hole (see [`infer_partial`]).
    pub(crate) fn finish(self, allow_unresolved: bool) -> Result<HashMap<String, Type>, String> {
        self.type_params
            .iter()
            .zip(self.bindings)
            .map(|(name, ty)| {
                ty.filter(|ty| allow_unresolved || !has_enum_hole(ty))
                    .or_else(|| allow_unresolved.then_some(Type::Void))
                    .map(|ty| (name.clone(), ty))
                    .ok_or_else(|| {
                        format!("cannot infer type parameter `{name}`; supply type arguments")
                    })
            })
            .collect()
    }
}

/// A callable mismatch names what differs; anything else keeps the generic
/// wording, because the concrete shapes alone do not say which side is wrong.
fn mismatch_message(expected: &Shape<'_>, found: &Shape<'_>, variance: Variance) -> String {
    match (expected, found) {
        (Shape::Function(n), Shape::Closure(m)) if n == m && variance != Variance::Contra => {
            "a function value capturing task-local state \
             cannot be passed where a shareable `fn` value is expected"
                .into()
        }
        (Shape::Closure(n), Shape::Function(m)) if n == m && variance == Variance::Contra => {
            "a callable that only accepts shareable `fn` values cannot receive the \
             task-local function values this parameter passes it"
                .into()
        }
        (
            Shape::Function(expected) | Shape::Closure(expected),
            Shape::Function(found) | Shape::Closure(found),
        ) if expected != found => {
            format!("expected a callable taking {expected} argument(s), found one taking {found}")
        }
        _ => "argument type does not match generic parameter signature".into(),
    }
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

    fn shareable(params: Vec<Type>, result: Type) -> Type {
        Type::Fn(params, Box::new(result))
    }
    fn local(params: Vec<Type>, result: Type) -> Type {
        Type::Closure(params, Box::new(result))
    }

    #[test]
    fn callable_kinds_follow_subtyping_per_position() {
        let t = named("T");
        let i = Type::I64;
        // Covariant: a task-local formal takes either kind; a shareable one
        // rejects a task-local value.
        for actual in [
            shareable(vec![i.clone()], i.clone()),
            local(vec![i.clone()], i.clone()),
        ] {
            run(
                &["T"],
                &[local(vec![t.clone()], t.clone())],
                std::slice::from_ref(&actual),
                &[],
            )
            .unwrap();
        }
        let error = run(
            &["T"],
            &[shareable(vec![t.clone()], t.clone())],
            &[local(vec![i.clone()], i.clone())],
            &[],
        )
        .unwrap_err();
        assert!(error.contains("task-local"), "{error}");
        // Contravariant: a parameter of the callable flips the direction.
        let takes_local = local(vec![local(vec![i.clone()], i.clone())], i.clone());
        let takes_shared = local(vec![shareable(vec![i.clone()], i.clone())], i.clone());
        let formal = local(vec![local(vec![t.clone()], t.clone())], t.clone());
        run(&["T"], std::slice::from_ref(&formal), &[takes_local], &[]).unwrap();
        let error = run(&["T"], &[formal], &[takes_shared], &[]).unwrap_err();
        assert!(error.contains("only accepts shareable"), "{error}");
    }

    #[test]
    fn repeated_bindings_join_covariantly_and_meet_contravariantly() {
        let i = Type::I64;
        let named_fn = shareable(vec![i.clone()], i.clone());
        let lambda = local(vec![i.clone()], i.clone());
        // `either<T>(a: T, b: T)`: both uses covariant, so T is the join.
        for order in [
            [named_fn.clone(), lambda.clone()],
            [lambda.clone(), named_fn.clone()],
        ] {
            let bound = run(&["T"], &[named("T"), named("T")], &order, &[]).unwrap();
            assert_eq!(bound["T"], lambda);
        }
        // `both<T>(f: fn(T) -> i64, g: fn(T) -> i64)`: T is only read by the
        // callables, so it is the meet of what they accept.
        let formal = local(vec![named("T")], i.clone());
        let bound = run(
            &["T"],
            &[formal.clone(), formal],
            &[
                local(vec![lambda.clone()], i.clone()),
                local(vec![named_fn.clone()], i.clone()),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(bound["T"], named_fn);
        // Mixed uses fix the binding. A later use that fits it is accepted
        // (a callable accepting any value may take shareable ones) ...
        let bound = run(
            &["T"],
            &[named("T"), local(vec![named("T")], i.clone())],
            &[named_fn.clone(), local(vec![lambda.clone()], i.clone())],
            &[],
        )
        .unwrap();
        assert_eq!(bound["T"], named_fn);
        // ... and one that would need it widened is not.
        run(
            &["T"],
            &[local(vec![named("T")], i.clone()), named("T")],
            &[local(vec![lambda.clone()], i.clone()), named_fn.clone()],
            &[],
        )
        .unwrap();
        assert!(
            run(
                &["T"],
                &[local(vec![named("T")], i.clone()), named("T")],
                &[local(vec![named_fn.clone()], i.clone()), lambda.clone()],
                &[],
            )
            .is_err()
        );
        // An explicit argument is never widened by a use.
        let error = run(&["T"], &[named("T")], &[lambda], &[named_fn]).unwrap_err();
        assert!(error.contains("conflicting"), "{error}");
    }

    #[test]
    fn partial_constraints_preserve_concrete_void() {
        let params = vec!["T".into()];
        let mut inference = Inference::new(&params, &[]).unwrap();
        assert!(
            inference
                .constrain_partial(
                    &generic("Result", vec![Type::I64, named("T")]),
                    &generic("Result", vec![Type::Void, Type::String]),
                )
                .is_err()
        );
        let mut inference = Inference::new(&params, &[]).unwrap();
        inference
            .constrain_partial(
                &generic("Result", vec![Type::Void, named("T")]),
                &generic("Result", vec![Type::Void, Type::String]),
            )
            .unwrap();
        assert_eq!(inference.finish(false).unwrap()["T"], Type::String);
    }

    #[test]
    fn partial_binding_merges_only_marked_holes() {
        let params = vec!["T".into()];
        let mut inference = Inference::new(&params, &[]).unwrap();
        inference
            .constrain_partial(&named("T"), &generic("Option", vec![enum_hole()]))
            .unwrap();
        inference
            .constrain_partial(&named("T"), &generic("Option", vec![Type::I64]))
            .unwrap();
        assert_eq!(
            inference.finish(false).unwrap()["T"],
            generic("Option", vec![Type::I64])
        );
        assert!(
            merge_enum_holes(
                &generic("Result", vec![Type::Void, Type::String]),
                &generic("Result", vec![Type::I64, Type::String])
            )
            .is_none()
        );
    }

    #[test]
    fn partial_enum_holes_skip_whole_formal_subtrees() {
        let params = vec!["T".into()];
        let mut inference = Inference::new(&params, &[]).unwrap();
        inference
            .constrain_partial(
                &generic(
                    "Result",
                    vec![generic("Option", vec![named("T")]), Type::String],
                ),
                &generic("Result", vec![enum_hole(), Type::String]),
            )
            .unwrap();
        inference.constrain(&named("T"), &Type::I64).unwrap();
        assert_eq!(inference.finish(false).unwrap()["T"], Type::I64);
    }

    #[test]
    fn partial_enum_holes_do_not_hide_other_mismatches() {
        let params = vec!["T".into()];
        let mut inference = Inference::new(&params, &[]).unwrap();
        assert!(
            inference
                .constrain_partial(
                    &generic(
                        "Result",
                        vec![generic("Option", vec![named("T")]), Type::String]
                    ),
                    &generic("Result", vec![enum_hole(), Type::Bool]),
                )
                .is_err()
        );
    }

    #[test]
    fn partial_enum_does_not_erase_callable_void_return() {
        let mut inference = Inference::new(&[], &[]).unwrap();
        assert!(
            inference
                .constrain_partial(
                    &generic("Option", vec![Type::Fn(vec![], Box::new(Type::I64))]),
                    &generic("Option", vec![Type::Fn(vec![], Box::new(Type::Void))]),
                )
                .is_err()
        );
    }

    #[test]
    fn partial_inference_rejects_void_value_arguments() {
        let params = vec!["T".into()];
        let mut inference = Inference::new(&params, &[]).unwrap();
        assert!(
            inference
                .constrain_partial(&named("T"), &Type::Void)
                .is_err()
        );
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
