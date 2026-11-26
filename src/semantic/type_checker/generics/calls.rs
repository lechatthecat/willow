//! Generic function argument checking, inference and specialization.
use super::*;

struct GenericCallArguments {
    params: Vec<Type>,
    imported: Option<FuncInfo>,
    explicit: Vec<Type>,
    contextual_params: Vec<Option<Type>>,
    deferred: Vec<bool>,
    reference_places: Vec<Option<ReferencePlaceInfo>>,
    actual: Vec<Option<Type>>,
}

struct GenericCallSite<'a> {
    callee: &'a str,
    type_args: &'a [Type],
    call_args: &'a [CallArg],
    span: Span,
    context: CallContext<'a>,
}

struct CheckedGenericArguments {
    params: Vec<Type>,
    imported: Option<FuncInfo>,
    bindings: HashMap<String, Type>,
    reference_places: Vec<Option<ReferencePlaceInfo>>,
    actual: Vec<Type>,
}

struct LambdaInferenceContext<'a, 't> {
    parameter_names: &'a HashSet<&'t str>,
    inference: &'a crate::semantic::generic_inference::Inference<'t>,
    spellings: &'a HashMap<String, String>,
    poisoned: bool,
    span: Span,
}

#[willow_continuations::checker]
impl TypeChecker {
    /// The deferred lambdas of a call rejected before inference starts still
    /// need their bodies checked and typed.
    fn check_deferred_lambdas_plainly(&mut self, call_args: &[CallArg], deferred: &[bool]) {
        for (arg, _) in call_args.iter().zip(deferred).filter(|(_, d)| **d) {
            if let Expr::Lambda(lambda) = &arg.expr {
                let ty = self.check_lambda(lambda);
                self.expr_types.insert(arg.expr.id(), ty);
            }
        }
    }

    /// An argument of a generic call inside an interface default, as
    /// inference sees it. The receiver is unknown there, so no
    /// instantiation may be keyed by `Self`: a whole `self` is inferred as
    /// the interface it converts to, and `Self` inside another type, which no
    /// conversion reaches, is reported (`None`).
    fn generic_self_argument(&mut self, ty: Type, callee: &str, span: Span) -> Option<Type> {
        let Some(interface) = self.local.self_interface.clone() else {
            return Some(ty);
        };
        if matches!(&ty, Type::Named(name) if name == "Self") {
            return Some(interface);
        }
        if !mentions(&ty, |name| name == "Self") {
            return Some(ty);
        }
        let interface = self.diagnostic_type_name(&interface);
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0201,
                format!(
                    "`{}` cannot be passed to generic function `{callee}`: its `Self` is not a whole value that converts to `{interface}`",
                    self.diagnostic_type_name(&ty)
                ),
            )
            .with_label(Label::primary(span, "the implementing class is unknown here"))
            .with_help(format!(
                "convert `self` first (`let it: {interface} = self;`) and build the argument from `it`"
            )),
        );
        None
    }

    pub(in crate::semantic::type_checker) fn generic_error(
        &mut self,
        span: Span,
        message: impl Into<String>,
    ) {
        self.push(
            Diagnostic::new(Severity::Error, ErrorCode::E0201, message)
                .with_label(Label::primary(span, "generic instantiation")),
        );
    }

    /// Preserve the enum head as context while its payload parameters are still
    /// unknown. Each payload is checked once; absent constraints remain holes
    /// until all call arguments have contributed to inference.
    pub(in crate::semantic::type_checker) fn check_generic_enum_argument(
        &mut self,
        expr: &Expr,
        expected: &Type,
        parameters: &HashSet<&str>,
        checked: bool,
    ) -> Type {
        #[cfg(test)]
        GENERIC_ENUM_ARGUMENT_CHECKS.with(|count| count.set(count.get() + 1));
        let checked = checked && self.expr_types.contains_key(&expr.id());
        if !uses_parameters(expected, parameters) {
            return self.check_argument_expecting(expr, expected, checked);
        }
        if let (Expr::ArrayLiteral(elements, _, _), Type::FixedArray(element, _)) = (expr, expected)
        {
            let mut actual = crate::semantic::generic_inference::enum_hole();
            for item in elements {
                let ty = self.check_generic_enum_argument(item, element, parameters, checked);
                if crate::semantic::generic_inference::is_enum_hole(&actual) {
                    actual = ty;
                } else if crate::semantic::generic_inference::has_enum_hole(&actual)
                    || crate::semantic::generic_inference::has_enum_hole(&ty)
                {
                    if let Some(merged) =
                        crate::semantic::generic_inference::merge_enum_holes(&actual, &ty)
                    {
                        actual = merged;
                    } else {
                        self.generic_error(
                            item.span(),
                            "fixed array elements must have the same type",
                        );
                    }
                } else if !self.types_compatible(&actual, &ty) {
                    self.generic_error(item.span(), "fixed array elements must have the same type");
                }
            }
            let ty = Type::FixedArray(Box::new(actual), elements.len() as u64);
            self.expr_types.insert(expr.id(), ty.clone());
            return ty;
        }
        if matches!(expected, Type::Generic(..) | Type::FixedArray(..))
            && matches!(expr, Expr::Ternary(_) | Expr::Match(_))
        {
            let context = expected.substitute_names(|name| {
                parameters
                    .contains(name.as_str())
                    .then(crate::semantic::generic_inference::enum_hole)
            });
            let ty = match expr {
                Expr::Ternary(t) => self.check_ternary_expr(t, Some(&context), checked),
                Expr::Match(m) => self.check_match_expr(m, Some(&context), checked),
                _ => unreachable!(),
            };
            self.expr_types.insert(expr.id(), ty.clone());
            return ty;
        }
        let Type::Generic(enum_name, expected_args) = expected else {
            return self.check_or_reuse_expr(expr, checked);
        };
        // Only enum constructors produce inference holes. Ordinary values keep
        // their own type, even when their formal parameter is a generic enum.
        let (variant, args) = match expr {
            Expr::Call(call)
                if self.prelude_variant_name_is_unshadowed(&call.callee)
                    && self.enum_nameable_bare(enum_name) =>
            {
                (call.callee.as_str(), call.args.as_slice())
            }
            Expr::Var(name, _, _)
                if self.prelude_variant_name_is_unshadowed(name)
                    && self.enum_nameable_bare(enum_name) =>
            {
                (name.as_str(), &[][..])
            }
            Expr::StaticCall(call)
                if call.type_args.is_empty()
                    && self.static_call_enum_key(&call.class).as_deref()
                        == Some(enum_name.as_str()) =>
            {
                (call.method.as_str(), call.args.as_slice())
            }
            _ => return self.check_or_reuse_expr(expr, checked),
        };
        let Some(info) = self.symbols.lookup_enum(enum_name) else {
            return self.check_or_reuse_expr(expr, checked);
        };
        let Some(definition) = info.variants.iter().find(|v| v.name == variant) else {
            return self.check_or_reuse_expr(expr, checked);
        };
        let type_params = info.type_params.clone();
        let payload_templates = definition.payload_types.clone();
        let bindings: HashMap<_, _> = type_params.iter().zip(expected_args).collect();
        let payloads: Vec<_> = payload_templates
            .iter()
            .map(|ty| ty.substitute_names(|name| bindings.get(name).map(|ty| (*ty).clone())))
            .collect();
        let omitted_void_payload = args.is_empty() && matches!(payloads.as_slice(), [Type::Void]);
        if payloads.len() != args.len() && !omitted_void_payload {
            self.generic_error(
                expr.span(),
                format!(
                    "enum variant `{enum_name}::{variant}` takes {} argument(s), got {}",
                    payloads.len(),
                    args.len()
                ),
            );
            for arg in args {
                self.check_or_reuse_expr(&arg.expr, checked);
            }
            return Self::error_type();
        }
        let actual: Vec<_> = args
            .iter()
            .zip(&payloads)
            .map(|(arg, ty)| self.check_generic_enum_argument(&arg.expr, ty, parameters, checked))
            .collect();
        let mut payload_inference =
            crate::semantic::generic_inference::Inference::new(&type_params, &[])
                .expect("registered enum parameters");
        let constraints = payload_templates
            .iter()
            .zip(&actual)
            .try_for_each(|(formal, actual)| payload_inference.constrain_partial(formal, actual));
        let inferred = match constraints.and_then(|()| payload_inference.finish(true)) {
            Ok(inferred) => inferred,
            Err(message) => {
                self.generic_error(expr.span(), message);
                return Self::error_type();
            }
        };
        let result = Type::Generic(
            enum_name.clone(),
            type_params
                .iter()
                .zip(expected_args)
                .map(|(name, expected)| {
                    let actual = &inferred[name];
                    if *actual == Type::Void {
                        expected.substitute_names(|name| {
                            parameters
                                .contains(name.as_str())
                                .then(crate::semantic::generic_inference::enum_hole)
                        })
                    } else {
                        actual.clone()
                    }
                })
                .collect(),
        );
        self.record_variant_use(enum_name, variant, expr.span());
        if matches!(expr, Expr::Call(_) | Expr::Var(..)) {
            self.enum_variant_resolutions
                .insert(expr.id(), enum_name.clone());
        }
        if let Expr::StaticCall(call) = expr {
            self.record_type_use(&call.class, &result, call.span);
            self.check_source_type_name(&call.class, call.span);
            self.static_call_classes.insert(
                call.id,
                crate::semantic::ids::TypeId::from_source_name(enum_name),
            );
        }
        self.expr_types.insert(expr.id(), result.clone());
        result
    }

    pub(super) fn is_inferred_enum_constructor(&self, expr: &Expr) -> bool {
        self.enum_variant_resolutions.contains_key(&expr.id())
            || matches!(expr, Expr::StaticCall(call)
                if call.type_args.is_empty() && self.static_call_enum_key(&call.class).is_some())
    }

    /// Complete constructor metadata after successful structural inference,
    /// without checking payloads again. Each nested constructor owns a type
    /// entry, including holes below more than one generic enum head.
    pub(super) fn resolve_generic_argument(&mut self, expr: &Expr, expected: &Type) {
        let mut pending = vec![(expr, expected.clone())];
        while let Some((expr, expected)) = pending.pop() {
            match expr {
                Expr::Ternary(t) => {
                    self.expr_types.insert(expr.id(), expected.clone());
                    pending.push((&t.then_expr, expected.clone()));
                    pending.push((&t.else_expr, expected));
                    continue;
                }
                Expr::Match(m) => {
                    self.expr_types.insert(expr.id(), expected.clone());
                    pending.extend(m.arms.iter().filter_map(|arm| match &arm.body {
                        MatchBody::Expr(expr) => Some((expr.as_ref(), expected.clone())),
                        _ => None,
                    }));
                    continue;
                }
                _ if !self.is_inferred_enum_constructor(expr) => continue,
                _ => {}
            }
            self.expr_types.insert(expr.id(), expected.clone());
            let (variant, args) = match expr {
                Expr::Call(call) => (call.callee.as_str(), call.args.as_slice()),
                Expr::StaticCall(call) => (call.method.as_str(), call.args.as_slice()),
                _ => continue,
            };
            if let Some((_, payloads, _)) = self.expected_qualified_variant(variant, &expected) {
                pending.extend(args.iter().zip(payloads).map(|(arg, ty)| (&arg.expr, ty)));
            }
        }
    }

    /// Per value parameter, the formal with its type parameters taken from the
    /// expected result (`expected` matched against `declared`, the return type),
    /// or `None` when the formal does not mention a type parameter or the match
    /// leaves one of them unbound. Explicit type arguments win over the hint.
    /// One walk over `declared` and `expected`, then one substitution per
    /// formal: O(|declared| + |expected| + sum |formal|).
    fn expected_result_param_hints(
        &mut self,
        type_params: &[String],
        declared: &Type,
        params: &[Type],
        explicit: &HashMap<String, Type>,
        expected: &Type,
        span: Span,
    ) -> Vec<Option<Type>> {
        #[cfg(test)]
        EXPECTED_RESULT_HINTS.with(|count| count.set(count.get() + 1));
        if Self::contains_error_type(expected) {
            return Vec::new();
        }
        let Ok(mut inference) =
            crate::semantic::generic_inference::Inference::new(type_params, &[])
        else {
            return Vec::new();
        };
        // An expected generic class arrives as its instance; match its logical
        // application, as argument types are.
        let expected =
            expected.substitute_names(|name| self.generics.logical_types.get(name).cloned());
        let mut spellings = HashMap::new();
        let formal = self.canonical_generic_type(declared, &mut HashMap::new());
        let expected = self.canonical_generic_type(&expected, &mut spellings);
        if inference.constrain(&formal, &expected).is_err() {
            return Vec::new();
        }
        let parameter_names: HashSet<_> = type_params.iter().map(String::as_str).collect();
        let mut hints = Vec::with_capacity(params.len());
        for formal in params {
            hints.push(if !uses_parameters(formal, &parameter_names) {
                None
            } else {
                let mut unbound = false;
                let hint = formal.substitute_names(|name| {
                    if !parameter_names.contains(name.as_str()) {
                        return None;
                    }
                    let bound = explicit.get(name).cloned().or_else(|| {
                        inference.binding(name).map(|bound| {
                            bound.map_names(|identity| {
                                spellings
                                    .get(identity)
                                    .cloned()
                                    .unwrap_or_else(|| identity.clone())
                            })
                        })
                    });
                    unbound |= bound.is_none();
                    bound
                });
                (!unbound).then(|| self.normalize_type(&hint, span))
            });
        }
        hints
    }

    pub(in crate::semantic::type_checker) fn check_generic_call(
        &mut self,
        c: &CallExpr,
        context: CallContext<'_>,
    ) -> Option<Type> {
        self.check_generic_call_parts(&c.callee, &c.type_args, &c.args, c.span, c.id, context)
    }

    pub(in crate::semantic::type_checker) fn check_generic_call_parts(
        &mut self,
        callee: &str,
        type_args: &[Type],
        call_args: &[CallArg],
        span: Span,
        id: ExprId,
        context: CallContext<'_>,
    ) -> Option<Type> {
        let template = self.generics.functions.get(callee)?.clone();
        if context.checked
            && let Some(expected) = context.expected
            && self.checked_as(id, expected)
        {
            // All type parameters are already bound, including any inferred
            // from a callback body. The identical result adds no constraints.
            return Some(expected.clone());
        }
        #[cfg(test)]
        GENERIC_CALL_CHECKS.with(|count| count.set(count.get() + 1));
        if !template.public() && template.span.file_id != span.file_id {
            self.generic_error(span, format!("generic function `{}` is private", callee));
            return Some(Self::error_type());
        }
        let parameter_names: HashSet<_> = template.type_params.iter().map(String::as_str).collect();
        let arguments = self.prepare_generic_call_arguments(
            &template,
            &parameter_names,
            GenericCallSite {
                callee,
                type_args,
                call_args,
                span,
                context,
            },
        );
        let Some(CheckedGenericArguments {
            params,
            imported,
            bindings,
            reference_places,
            actual,
        }) = self.infer_generic_call_arguments(
            &template,
            call_args,
            span,
            &parameter_names,
            arguments,
            None,
        )
        else {
            return Some(Self::error_type());
        };
        let actual: Vec<_> = actual
            .into_iter()
            .zip(call_args)
            .zip(&params)
            .map(|((actual, arg), formal)| {
                let expected = formal.substitute_names(|name| bindings.get(name).cloned());
                let expected = self.normalize_type(&expected, arg.span);
                if self.is_inferred_enum_constructor(&arg.expr)
                    || matches!(arg.expr, Expr::Ternary(_) | Expr::Match(_))
                {
                    self.resolve_generic_argument(&arg.expr, &expected);
                    expected
                } else if is_empty_array_literal(&arg.expr) {
                    self.check_argument_expecting(&arg.expr, &expected, false)
                } else {
                    actual
                }
            })
            .collect();
        let param_infos: Vec<_> = template
            .params
            .iter()
            .zip(&actual)
            .map(|(param, ty)| ParamInfo {
                ty: self.normalize_type(ty, param.type_span),
                mode: param.mode.clone(),
                sending: param.sending,
                span: param.span,
                type_span: param.type_span,
            })
            .collect();
        for (((param, arg), actual), place) in param_infos
            .iter()
            .zip(call_args)
            .zip(&actual)
            .zip(reference_places)
        {
            match (&param.mode, &arg.mode) {
                (ParamMode::Value, CallArgMode::Reference { .. }) => {
                    self.push_unexpected_reference_arg(&param.ty, actual, arg.span)
                }
                (ParamMode::Reference { .. }, CallArgMode::Value) => {
                    self.push_missing_reference_arg(arg)
                }
                (ParamMode::Reference { mutable, .. }, CallArgMode::Reference { .. }) => {
                    if let Some(place) = place {
                        self.check_reference_argument_place(param, arg, *mutable, place);
                    }
                }
                (ParamMode::Value, CallArgMode::Value) => {}
            }
        }
        self.check_mut_reference_aliases(&param_infos, call_args);
        self.check_async_call_captures(&param_infos, call_args, template.is_async());
        let result = imported
            .as_ref()
            .map_or(&template.return_type, |info| &info.return_type)
            .substitute_names(|n| bindings.get(n).cloned());
        let result = self.normalize_type(&result, span);
        if self.generics.abstract_check {
            if self.capture_call_sites {
                self.analysis_symbols.reference(
                    span,
                    callee,
                    crate::semantic::analysis_symbols::Declaration::new(
                        &template.name,
                        "function",
                        template.span,
                        None,
                    ),
                    "call",
                    false,
                );
            }
            return Some(if template.is_async() {
                Type::Generic("Task".into(), vec![result])
            } else {
                result
            });
        }
        let args: Vec<_> = template
            .type_params
            .iter()
            .map(|p| bindings[p].clone())
            .collect();
        if !self.check_instantiation_size(&args, span) {
            return Some(Self::error_type());
        }
        let identity_args: Vec<_> = args
            .iter()
            .map(|ty| ty.map_names(|n| self.generic_type_identity(n)))
            .collect();
        let key = (
            0,
            QualifiedName::join(template.span.file_id.0, &template.name),
            identity_args.clone(),
        );
        let name = if let Some(name) = self.generics.instances.get(&key) {
            name.clone()
        } else {
            let name = symbol(&template.name, &identity_args);
            self.record_runtime_instance_name(&name, &template.name, &args);
            self.generics.instances.insert(key.clone(), name.clone());
            let Some(fresh) = self.reserve_instantiation(&key, span) else {
                return Some(Self::error_type());
            };
            if fresh {
                #[cfg(test)]
                {
                    self.generics.registry.borrow_mut().function_specializations += 1;
                }
                let mut f = (*template).clone();
                f.name = name.clone();
                f.type_params.clear();
                let (bindings, classes) = self.instantiation_bindings(span, &bindings);
                let mut item = Item::Function(f);
                substitute_item(&mut item, &bindings)
                    .record_bodies(&mut self.generics.registry.borrow_mut().origins);
                self.generics.pending.push_back(Instantiation {
                    item,
                    classes,
                    method_only: false,
                });
            }
            name
        };
        // Template origin is canonical, but the generated source call still
        // resolves in the consumer's scope. Preserve its written prefix so
        // source-use diagnostics do not lose a used module import.
        let path = self.generics.function_paths.get(callee).map(|origin| {
            QualifiedName::new(callee)
                .qualifier()
                .unwrap_or(origin.as_str())
        });
        let target = path.map_or(name.clone(), |path| QualifiedName::join(path, &name));
        self.generics.calls.insert(id, target);
        Some(if template.is_async() {
            Type::Generic("Task".into(), vec![result])
        } else {
            result
        })
    }

    fn prepare_generic_call_arguments(
        &mut self,
        template: &FunctionDecl,
        parameter_names: &HashSet<&str>,
        site: GenericCallSite<'_>,
    ) -> GenericCallArguments {
        let GenericCallSite {
            callee,
            type_args,
            call_args,
            span,
            context,
        } = site;
        let explicit: Vec<_> = type_args
            .iter()
            .map(|t| {
                let ty = self.normalize_type(t, span);
                ty.substitute_names(|n| self.generics.logical_types.get(n).cloned())
            })
            .collect();
        for ty in &explicit {
            self.validate_type(ty, span);
        }
        let imported = self
            .generics
            .function_paths
            .get(callee)
            .and_then(|path| self.symbols.lookup_module_func(path, &template.name))
            .cloned();
        let params: Vec<_> = imported.as_ref().map_or_else(
            || template.params.iter().map(|p| p.ty.clone()).collect(),
            |info| info.params.clone(),
        );
        let explicit_bindings: HashMap<_, _> = template
            .type_params
            .iter()
            .cloned()
            .zip(explicit.iter().cloned())
            .collect();
        let fully_explicit = !explicit.is_empty() && explicit.len() == template.type_params.len();
        // A fixed value parameter may require an ordinary upcast or interface
        // box. Explicit arguments also fix parameters that contain T, so use
        // their substituted expected types for both context and inference.
        let contextual_params: Vec<_> = params
            .iter()
            .map(|ty| {
                let substituted = fully_explicit
                    .then(|| ty.substitute_names(|name| explicit_bindings.get(name).cloned()));
                // The test runs on the formal: an explicit argument may spell
                // the caller's own parameter under a callee parameter's name.
                //
                let expected = substituted.as_ref().unwrap_or(ty);
                (fully_explicit || !uses_parameters(ty, parameter_names))
                    .then(|| self.normalize_type(expected, span))
            })
            .collect();
        // The expected result type, matched against the declared return type,
        // gives a parameter-mentioning formal a context for contextual forms
        // (`let h: Box<Array<i64>> = wrap([])`, contextual generic arguments). It is only a
        // checking context: inference still binds from the checked arguments,
        // so an ordinary value keeps its own type.
        let hinted_params = match context.expected {
            // An async call yields `Task<R>` and has no generic form to match.
            Some(expected) if !fully_explicit && !template.is_async() => {
                let declared = imported
                    .as_ref()
                    .map_or(&template.return_type, |info| &info.return_type);
                self.expected_result_param_hints(
                    &template.type_params,
                    declared,
                    &params,
                    &explicit_bindings,
                    expected,
                    span,
                )
            }
            _ => Vec::new(),
        };
        // A lambda whose formal still mentions a type parameter is typed after
        // the other arguments: its parameter types come from what they bind
        // (`map(xs, |x| x * 2)`), and its result then binds the rest.
        let deferred: Vec<bool> = call_args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                matches!(a.expr, Expr::Lambda(_))
                    && a.mode == CallArgMode::Value
                    && template
                        .params
                        .get(i)
                        .is_some_and(|p| p.mode == ParamMode::Value)
                    && matches!(params.get(i), Some(Type::Fn(..) | Type::Closure(..)))
                    && matches!(contextual_params.get(i), Some(None))
            })
            .collect();
        let mut reference_places = Vec::with_capacity(call_args.len());
        let actual: Vec<Option<Type>> = call_args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                if deferred[i] {
                    reference_places.push(None);
                    return None;
                }
                let is_reference = matches!(
                    template.params.get(i).map(|p| &p.mode),
                    Some(ParamMode::Reference { .. })
                ) && matches!(a.mode, CallArgMode::Reference { .. });
                let checked = context.checked && self.expr_types.contains_key(&a.expr.id());
                let place = if is_reference {
                    self.reference_place_info(&a.expr, a.span, checked)
                } else {
                    None
                };
                let result = if is_reference {
                    place
                        .as_ref()
                        .map_or_else(Self::error_type, |place| place.ty.clone())
                } else if let Some(Some(expected)) = contextual_params.get(i) {
                    self.check_argument_expecting(&a.expr, expected, checked)
                } else if let Some(Some(hint)) = hinted_params.get(i) {
                    self.check_argument_expecting(&a.expr, hint, checked)
                } else if let Some(expected) = params.get(i) {
                    self.check_generic_enum_argument(&a.expr, expected, parameter_names, checked)
                } else {
                    self.check_or_reuse_expr(&a.expr, checked)
                };
                reference_places.push(place);
                Some(result.substitute_names(|n| self.generics.logical_types.get(n).cloned()))
            })
            .collect();
        GenericCallArguments {
            params,
            imported,
            explicit,
            contextual_params,
            deferred,
            reference_places,
            actual,
        }
    }

    fn infer_generic_call_arguments(
        &mut self,
        template: &FunctionDecl,
        call_args: &[CallArg],
        span: Span,
        parameter_names: &HashSet<&str>,
        arguments: GenericCallArguments,
        constructed: Option<&str>,
    ) -> Option<CheckedGenericArguments> {
        let GenericCallArguments {
            params,
            imported,
            explicit,
            contextual_params,
            deferred,
            reference_places,
            mut actual,
        } = arguments;
        let mut spellings = HashMap::new();
        let inferred_explicit: Vec<_> = explicit
            .iter()
            .map(|ty| self.canonical_generic_type(ty, &mut spellings))
            .collect();
        // Bindings are subtrees of the explicit and actual types only, so only
        // their spellings are reported back; the template's own spellings of
        // the same declarations must not displace the caller's.
        let inferred_params: Vec<_> = params
            .iter()
            .map(|ty| self.canonical_generic_type(ty, &mut HashMap::new()))
            .collect();
        let mut inference = match crate::semantic::generic_inference::Inference::new(
            &template.type_params,
            &inferred_explicit,
        ) {
            Ok(inference) => inference,
            Err(message) => {
                self.check_deferred_lambdas_plainly(call_args, &deferred);
                self.generic_error(span, message);
                return None;
            }
        };
        // A wrong argument count is reported first, but the arguments that do
        // have a formal still bind, so a lambda among them is typed normally.
        let mut failure = (params.len() != call_args.len()).then(|| {
            format!(
                "expected {} value arguments, got {}",
                params.len(),
                call_args.len()
            )
        });
        let mut mismatched = false;
        let mut failure_argument = None;
        // An argument whose type already carries the recovery marker has been
        // reported; matching it would only add a derived mismatch, and every
        // later call fed this call's result would add another.
        let mut poisoned = false;
        for (i, ty) in actual.iter().enumerate() {
            let Some(ty) = ty else { continue };
            // `[]` says nothing about its element type: the other arguments
            // bind it, and the literal is typed against the result afterwards.
            if is_empty_array_literal(&call_args[i].expr)
                && matches!(contextual_params.get(i), Some(None))
                && untyped_array(ty)
            {
                continue;
            }
            let fixed = contextual_params
                .get(i)
                .and_then(Option::as_ref)
                .filter(|_| template.params[i].mode == ParamMode::Value);
            let ty = if let Some(expected) = fixed
                && let normalized = self.normalize_type(ty, span)
                && self.types_compatible(expected, &normalized)
            {
                expected.substitute_names(|name| self.generics.logical_types.get(name).cloned())
            } else {
                ty.clone()
            };
            let Some(ty) = self.generic_self_argument(ty, &template.name, call_args[i].span) else {
                poisoned = true;
                continue;
            };
            let ty = self.canonical_generic_type(&ty, &mut spellings);
            if mentions(&ty, Self::is_error_name) {
                poisoned = true;
            } else if !mismatched
                && let Some(formal) = inferred_params.get(i)
                && let Err(message) = inference.constrain_partial(formal, &ty)
            {
                mismatched = true;
                if failure.is_none() {
                    failure_argument = Some(i);
                }
                failure.get_or_insert_with(|| {
                    format!(
                        "argument {} (`{}`): expected `{}`, found `{}`; {message}",
                        i + 1,
                        template.params[i].name,
                        self.diagnostic_type_name(
                            &params[i].substitute_names(|name| inference.binding(name).cloned())
                        ),
                        self.diagnostic_type_name(&ty)
                    )
                });
            }
        }
        for i in (0..call_args.len()).filter(|&i| deferred[i]) {
            let Expr::Lambda(lambda) = &call_args[i].expr else {
                unreachable!("deferred arguments are lambdas")
            };
            // A constructor reports the type parameters left open as one
            // diagnostic naming them; an untyped closure parameter that only
            // they would type is checked as recovery instead of adding E1001.
            let open = constructed.is_some()
                && !poisoned
                && open_lambda_parameter(lambda, &params[i], |name| {
                    parameter_names.contains(name) && inference.binding(name).is_none()
                });
            let lambda_ty = self.check_inferred_generic_lambda(
                lambda,
                &params[i],
                LambdaInferenceContext {
                    parameter_names,
                    inference: &inference,
                    spellings: &spellings,
                    poisoned: poisoned || open,
                    span,
                },
            );
            self.expr_types
                .insert(call_args[i].expr.id(), lambda_ty.clone());
            if open {
                actual[i] = Some(lambda_ty);
                continue;
            }
            let lambda_ty =
                lambda_ty.substitute_names(|n| self.generics.logical_types.get(n).cloned());
            let canonical = match self.generic_self_argument(
                lambda_ty.clone(),
                &template.name,
                call_args[i].span,
            ) {
                Some(ty) => self.canonical_generic_type(&ty, &mut spellings),
                None => Self::error_type(),
            };
            if mentions(&canonical, Self::is_error_name) {
                poisoned = true;
            } else if !mismatched
                && let Err(message) = inference.constrain(&inferred_params[i], &canonical)
            {
                mismatched = true;
                if failure.is_none() {
                    failure_argument = Some(i);
                }
                failure.get_or_insert_with(|| {
                    format!(
                        "argument {} (`{}`): expected `{}`, found `{}`; {message}",
                        i + 1,
                        template.params[i].name,
                        self.diagnostic_type_name(
                            &params[i].substitute_names(|name| inference.binding(name).cloned())
                        ),
                        self.diagnostic_type_name(&lambda_ty)
                    )
                });
            }
            actual[i] = Some(lambda_ty);
        }
        let actual: Vec<Type> = actual.into_iter().map(Option::unwrap).collect();
        if let Some(message) = failure {
            if let Some(i) = failure_argument {
                let diagnostic =
                    Diagnostic::new(Severity::Error, ErrorCode::E0201, message).with_label(
                        Label::primary(call_args[i].expr.span(), "argument type mismatch"),
                    );
                self.push(self.with_callable_kind_help(diagnostic, &params[i], &actual[i]));
            } else {
                self.generic_error(span, message);
            }
            return None;
        }
        if poisoned {
            return None;
        }
        let unbound = |name: &str| {
            inference
                .binding(name)
                .is_none_or(crate::semantic::generic_inference::has_enum_hole)
        };
        if let Some(class) = constructed
            && template.type_params.iter().any(|name| unbound(name))
        {
            let shown: Vec<_> = template
                .type_params
                .iter()
                .map(|name| {
                    inference
                        .binding(name)
                        .filter(|_| !unbound(name))
                        .map_or_else(
                            || name.clone(),
                            |bound| {
                                self.diagnostic_type_name(&bound.map_names(|identity| {
                                    spellings
                                        .get(identity)
                                        .cloned()
                                        .unwrap_or_else(|| identity.clone())
                                }))
                            },
                        )
                })
                .collect();
            self.push_uninferred_constructor(class, template, &shown, call_args, span, unbound);
            return None;
        }
        let bindings = match inference.finish(false) {
            Ok(bindings) => bindings
                .into_iter()
                .map(|(name, ty)| {
                    (
                        name,
                        ty.map_names(|identity| {
                            spellings
                                .get(identity)
                                .cloned()
                                .unwrap_or_else(|| identity.clone())
                        }),
                    )
                })
                .collect::<HashMap<_, _>>(),
            Err(message) => {
                self.generic_error(span, message);
                return None;
            }
        };
        Some(CheckedGenericArguments {
            params,
            imported,
            bindings,
            reference_places,
            actual,
        })
    }

    /// `new Box(5)` of a generic class with no written type arguments and no
    /// expected class type. The constructor is a generic call: its value
    /// parameters are the `init` parameters (or the memberwise fields) and its
    /// type parameters are the class's, so the arguments bind them exactly as
    /// they bind a generic function's. The class is then instantiated with the
    /// bindings and the constructor checked concretely; that check reuses the
    /// argument types computed here instead of walking the arguments again.
    pub(in crate::semantic::type_checker) fn check_inferred_generic_new(
        &mut self,
        n: &NewExpr,
    ) -> Option<Type> {
        // No argument can fix a type parameter; the explicit-arguments
        // diagnostic says what to write.
        if n.args.is_empty() {
            return None;
        }
        let template = self.generics.classes.get(&n.class_name)?.clone();
        if !self.signature_metadata && !template.public() && template.span.file_id != n.span.file_id
        {
            return None;
        }
        let params = self.generic_constructor_params(&n.class_name, &template)?;
        #[cfg(test)]
        GENERIC_NEW_INFERENCES.with(|count| count.set(count.get() + 1));
        if params.len() != n.args.len() {
            for arg in &n.args {
                self.check_expr(&arg.expr);
            }
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0845,
                    format!(
                        "constructor `{}::init` expects {} argument(s) but got {}",
                        n.class_name,
                        params.len(),
                        n.args.len()
                    ),
                )
                .with_label(Label::primary(n.span, "wrong number of arguments")),
            );
            return Some(Self::error_type());
        }
        let signature = FunctionDecl {
            requires: Vec::new(),
            ensures: Vec::new(),
            effect_contracts: Vec::new(),
            type_params: template.type_params.clone(),
            rust_bridge: None,
            name: n.class_name.clone(),
            modifiers: Modifiers::PUBLIC,
            params,
            return_type: Type::Generic(
                n.class_name.clone(),
                template
                    .type_params
                    .iter()
                    .cloned()
                    .map(Type::Named)
                    .collect(),
            ),
            return_type_span: None,
            body: Block {
                id: BodyId::fresh(),
                stmts: Vec::new(),
                span: template.span,
            },
            span: template.span,
            constant: None,
        };
        let parameter_names: HashSet<_> =
            signature.type_params.iter().map(String::as_str).collect();
        // No generic function is registered under a class name, so the
        // signature is never mistaken for an imported function's.
        let arguments = self.prepare_generic_call_arguments(
            &signature,
            &parameter_names,
            GenericCallSite {
                callee: "",
                type_args: &[],
                call_args: &n.args,
                span: n.span,
                context: CallContext {
                    expected: None,
                    checked: false,
                },
            },
        );
        let Some(checked) = self.infer_generic_call_arguments(
            &signature,
            &n.args,
            n.span,
            &parameter_names,
            arguments,
            Some(&n.class_name),
        ) else {
            return Some(Self::error_type());
        };
        let args: Vec<_> = signature
            .type_params
            .iter()
            .map(|name| checked.bindings[name].clone())
            .collect();
        let ty = self.instantiate_class(&n.class_name, &args, n.span)?;
        let Type::Named(name) = &ty else {
            return Some(ty);
        };
        self.generics.objects.insert(n.id, name.clone());
        Some(self.check_new_with_effects(n, name, true))
    }

    /// The constructor parameters of a generic class, spelled in this module:
    /// an imported template's signature comes from its qualified class info,
    /// a local one from its declaration. `None` when the class has no
    /// constructor signature to infer from (its base declares `init`).
    fn generic_constructor_params(&self, name: &str, template: &ClassDecl) -> Option<Vec<Param>> {
        let field_param = |field_name: &str, ty: Type, span: Span| Param {
            name: field_name.to_string(),
            ty,
            mode: ParamMode::Value,
            sending: false,
            span,
            type_span: span,
        };
        let inherited = |base: &str| {
            self.implicit_constructor_field_infos(base).map(|fields| {
                fields
                    .into_iter()
                    .map(|(_, field_name, field)| {
                        field_param(&field_name, field.ty, field.declaration_span)
                    })
                    .collect::<Vec<_>>()
            })
        };
        if let Some(path) = self.generics.class_paths.get(name) {
            let info = self.symbols.lookup_class(name).or_else(|| {
                self.symbols
                    .lookup_class(&QualifiedName::join(path, &template.name))
            })?;
            return match (&info.constructor, template.constructors.first()) {
                (Some(ctor), Some(decl)) => Some(
                    decl.params
                        .iter()
                        .zip(&ctor.param_infos)
                        .map(|(param, info)| Param {
                            ty: info.ty.clone(),
                            ..param.clone()
                        })
                        .collect(),
                ),
                (Some(_), None) => None,
                (None, _) => inherited(&info.name.clone()),
            };
        }
        if let Some(ctor) = template.constructors.first() {
            return Some(ctor.params.clone());
        }
        let mut params = match &template.base_class {
            Some(base) => inherited(&base.spelling())?,
            None => Vec::new(),
        };
        params.extend(
            template
                .fields
                .iter()
                .filter(|field| !field.is_static())
                .map(|field| field_param(&field.name, field.ty.clone(), field.span)),
        );
        Some(params)
    }

    /// Inference left a class type parameter unbound. Name it, say which
    /// parameters mention it, and show the explicit form with the bound
    /// parameters filled in, keeping the call's arguments.
    fn push_uninferred_constructor(
        &mut self,
        class: &str,
        signature: &FunctionDecl,
        shown: &[String],
        call_args: &[CallArg],
        span: Span,
        unbound: impl Fn(&str) -> bool,
    ) {
        let missing: Vec<_> = signature
            .type_params
            .iter()
            .filter(|name| unbound(name))
            .collect();
        let names = missing
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let plural = if missing.len() == 1 { "" } else { "s" };
        let mut diagnostic = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0201,
            format!(
                "cannot infer type parameter{plural} {names} of generic class `{class}` from the constructor arguments"
            ),
        )
        .with_label(Label::primary(span, "type argument not determined"));
        for name in &missing {
            let users: Vec<_> = signature
                .params
                .iter()
                .zip(call_args)
                .filter(|(param, _)| mentions(&param.ty, |n| n == name.as_str()))
                .map(|(param, _)| format!("`{}`", param.name))
                .collect();
            diagnostic = diagnostic.with_note(if users.is_empty() {
                format!("no constructor parameter mentions `{name}`")
            } else {
                format!(
                    "`{name}` appears only in {}, whose argument does not fix it (an empty array literal or an untyped closure parameter leaves it open)",
                    users.join(", ")
                )
            });
        }
        let arguments = if call_args.is_empty() { "" } else { "..." };
        self.push(diagnostic.with_help(format!(
            "write the type argument{plural} and keep the arguments as they are: `new {class}<{}>({arguments})`, replacing {names} with a concrete type; or annotate the expected type, as in `let value: {class}<{}> = new {class}({arguments});`",
            shown.join(", "),
            shown.join(", ")
        )));
    }

    fn check_inferred_generic_lambda(
        &mut self,
        lambda: &LambdaExpr,
        formal: &Type,
        context: LambdaInferenceContext<'_, '_>,
    ) -> Type {
        let LambdaInferenceContext {
            parameter_names,
            inference,
            spellings,
            poisoned,
            span,
        } = context;
        let (Type::Fn(formal_params, formal_ret) | Type::Closure(formal_params, formal_ret)) =
            formal
        else {
            unreachable!("deferred formals are callable")
        };
        // What the arguments so far bound, in this unit's spellings. An
        // unbound parameter leaves its type unknown, unless a poisoned
        // argument means it never will be bound: it is then the recovery
        // marker, so the body is checked without a derived E1001.
        let unbound = poisoned.then(Self::error_type);
        // Knownness is decided on the FORMAL: a binding may itself spell
        // the caller's own type parameter, whose name can coincide with
        // one of the callee's (`fold` called from `count_where<T>` binds
        // the callee's `T` to the caller's `T`), so the substituted type
        // says nothing about which callee parameters are still unbound.
        //
        let known = |ty: &Type| {
            if mentions(ty, |name| {
                parameter_names.contains(name)
                    && unbound.is_none()
                    && inference.binding(name).is_none()
            }) {
                return None;
            }
            Some(ty.substitute_names(|name| {
                if !parameter_names.contains(name.as_str()) {
                    return None;
                }
                inference
                    .binding(name)
                    .map(|bound| {
                        bound.map_names(|identity| {
                            spellings
                                .get(identity)
                                .cloned()
                                .unwrap_or_else(|| identity.clone())
                        })
                    })
                    .or_else(|| unbound.clone())
            }))
        };
        if formal_params.len() == lambda.params.len() {
            let expected_params: Option<Vec<_>> = formal_params.iter().map(known).collect();
            let expected_ret = known(formal_ret);
            let expected_params = expected_params.map(|types| {
                types
                    .iter()
                    .map(|ty| self.normalize_type(ty, span))
                    .collect::<Vec<_>>()
            });
            let expected_ret = expected_ret.map(|ty| self.normalize_type(&ty, span));
            // The help for a rejected capture names the declared
            // signature, which is what the user would change.
            self.check_lambda_for_callable(
                lambda,
                formal,
                expected_params.as_deref(),
                expected_ret.as_ref(),
            )
            .0
        } else {
            self.check_lambda(lambda)
        }
    }
}

/// The type of `[]` checked without an element context.
fn untyped_array(ty: &Type) -> bool {
    match ty {
        Type::Array(element) => **element == Type::Void,
        Type::Generic(_, args) => matches!(args.as_slice(), [Type::Void]),
        _ => false,
    }
}

fn is_empty_array_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::ArrayLiteral(elements, _, _) if elements.is_empty())
}

/// Whether a closure parameter without an annotation sits where the formal
/// names a type parameter that is still unbound.
fn open_lambda_parameter(
    lambda: &LambdaExpr,
    formal: &Type,
    unbound: impl Fn(&str) -> bool,
) -> bool {
    let (Type::Fn(formal_params, _) | Type::Closure(formal_params, _)) = formal else {
        return false;
    };
    lambda
        .params
        .iter()
        .zip(formal_params)
        .any(|(param, formal)| param.ty.is_none() && mentions(formal, &unbound))
}
