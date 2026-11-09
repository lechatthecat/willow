//! Class and interface specialization and deferred method requests.
use super::*;

struct ClassInstantiation<'a> {
    template: &'a ClassDecl,
    template_access: String,
    local_name: String,
    name: String,
    key: InstantiationKey,
    bindings: HashMap<String, Type>,
    span: Span,
}

#[willow_continuations::checker]
impl TypeChecker {
    /// Queue one concrete body. The class signature is already available, so
    /// recursion and calls discovered in other units share the same request.
    pub(in crate::semantic::type_checker) fn request_class_method(
        &mut self,
        owner: &str,
        method: &str,
    ) {
        if self.generics.abstract_check {
            return;
        }
        let Some(info) = self.symbols.lookup_class(owner) else {
            return;
        };
        if !info.methods.contains_key(method) {
            return;
        }
        let local = QualifiedName::new(&info.name).last_segment().to_string();
        let file = info.declaration_span.file_id;
        let deferred = self
            .generics
            .registry
            .borrow()
            .deferred_classes
            .get(&(file, local.clone()))
            .cloned();
        let Some(deferred) = deferred else {
            return;
        };
        let key = (file, local.clone(), method.to_string());
        {
            let mut registry = self.generics.registry.borrow_mut();
            if registry.requested_methods.contains_key(&key) {
                return;
            }
            registry.requested_methods.insert(key, ());
        }
        let template = self
            .generics
            .registry
            .borrow()
            .method_templates
            .get(&(file, deferred.template.clone(), method.to_string()))
            .cloned();
        let Some(template) = template else {
            return;
        };
        let mut class = deferred.shell.clone();
        let mut method = (*template).clone();
        method.constraint = None;
        class.methods.push(method);
        let mut item = Item::Class(class);
        let mut ids = FreshIds::default();
        visit_item(
            &mut item,
            &mut |ty| crate::semantic::generic_inference::substitute(ty, &deferred.bindings),
            &mut |_| {},
            true,
            true,
            Some((&deferred.template, &local)),
            &mut ids,
        );
        ids.record_bodies(&mut self.generics.registry.borrow_mut().origins);
        self.generics.pending.push_back(Instantiation {
            item,
            // The signature request precedes every method request and installs
            // nominal aliases once in the declaring checker.
            classes: Vec::new(),
            method_only: true,
        });
    }

    /// Only the matching nominal class supplies omitted constructor arguments.
    /// Inspect the outer logical type; do not traverse or clone the expression tree.
    pub(in crate::semantic::type_checker) fn contextual_new_type(
        &mut self,
        n: &NewExpr,
        expected: &Type,
    ) -> Option<Type> {
        if !n.type_args.is_empty() || !self.generics.classes.contains_key(&n.class_name) {
            return None;
        }
        let logical = match expected {
            Type::Named(name) => self
                .generics
                .logical_types
                .get(name)
                .or_else(|| {
                    let class = self.symbols.lookup_class(name)?;
                    self.generics.logical_types.get(&class.name)
                })
                .unwrap_or(expected),
            _ => expected,
        };
        let Type::Generic(name, args) = logical else {
            return None;
        };
        if self.generic_type_identity(name) != self.generic_type_identity(&n.class_name) {
            return None;
        }
        #[cfg(test)]
        CONTEXTUAL_NEW_CHECKS.with(|count| count.set(count.get() + 1));
        // An annotated/parameter type already owns a checked specialization.
        // Reuse it without re-expanding nested logical arguments at every `new`.
        if let Type::Named(name) = expected {
            let template = &self.generics.classes[&n.class_name];
            if !self.signature_metadata
                && !template.public
                && template.span.file_id != n.span.file_id
            {
                self.generic_error(
                    n.span,
                    format!("generic class `{}` is private", n.class_name),
                );
                return Some(Self::error_type());
            }
            // Alias names describe a storage type, not an emitted constructor.
            // Effects and lowering must call the specialization's real owner.
            let class = self.symbols.lookup_class(name)?;
            return Some(Type::Named(class.name.clone()));
        }
        let args = args.clone();
        self.instantiate_class(&n.class_name, &args, n.span)
    }

    pub(in crate::semantic::type_checker) fn instantiate_class(
        &mut self,
        name: &str,
        args: &[Type],
        span: Span,
    ) -> Option<Type> {
        let template = self.generics.classes.get(name)?.clone();
        #[cfg(test)]
        {
            self.generics.registry.borrow_mut().class_requests += 1;
        }
        if !self.signature_metadata && !template.public && template.span.file_id != span.file_id {
            self.generic_error(span, format!("generic class `{name}` is private"));
            return Some(Self::error_type());
        }
        if args.len() != template.type_params.len() {
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E0201,
                format!(
                    "generic class `{name}` expects {} type argument(s), got {}",
                    template.type_params.len(),
                    args.len()
                ),
            )
            .with_label(Label::primary(span, "generic instantiation"));
            if args.is_empty() {
                diagnostic = diagnostic.with_help(format!(
                    "specify type arguments explicitly, for example `new {name}<{}>()`, or provide a matching expected class type",
                    template.type_params.join(", ")
                ));
            }
            self.push(diagnostic);
            return Some(Self::error_type());
        }
        let args: Vec<_> = args.iter().map(|t| self.normalize_type(t, span)).collect();
        // ABI source receivers are inferred from already checked values. Import
        // access is checked at written type sites, not at literal method calls.
        if template.abi_storage.is_none() {
            for ty in &args {
                self.validate_type(ty, span);
            }
        }
        if let Err(message) =
            crate::semantic::generic_inference::infer(&template.type_params, &[], &[], &args)
        {
            self.generic_error(span, message);
            return Some(Self::error_type());
        }
        if !self.check_instantiation_size(&args, span) {
            return Some(Self::error_type());
        }
        let identity_args: Vec<_> = args
            .iter()
            .map(|ty| ty.map_names(|n| self.generic_type_identity(n)))
            .collect();
        let key = (
            1,
            QualifiedName::join(template.span.file_id.0, &template.name),
            identity_args.clone(),
        );
        if let Some(name) = self.generics.instances.get(&key) {
            return Some(Type::Named(name.clone()));
        }
        // Repeated concrete keys above terminate ordinary recursive layouts.
        // Bound changing recursion by resources rather than rejecting every
        // re-entry of a template: Link<String> -> Link<i64> can be finite.
        if self.generics.active_class_depth >= 64 {
            self.generic_error(span, "generic class instantiation depth exceeded (possible non-finite recursive expansion)");
            return Some(Self::error_type());
        }
        self.generics.active_class_depth += 1;
        let logical_name = self
            .symbols
            .lookup_class(name)
            .map_or_else(|| name.to_string(), |info| info.name.clone());
        let bindings: HashMap<String, Type> = template
            .type_params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        let template_access = name.to_string();
        let local_name = symbol(&template.name, &identity_args);
        self.record_runtime_instance_name(&local_name, &template.name, &args);
        let name = self
            .generics
            .class_paths
            .get(name)
            .map_or(local_name.clone(), |path| {
                QualifiedName::join(path, &local_name)
            });
        self.generics.instances.insert(key.clone(), name.clone());
        self.generics.logical_types.insert(
            name.clone(),
            Type::Generic(
                logical_name,
                args.iter()
                    .map(|arg| {
                        arg.substitute_names(|n| self.generics.logical_types.get(n).cloned())
                    })
                    .collect(),
            ),
        );
        let cached = self
            .generics
            .registry
            .borrow()
            .instantiated_classes
            .get(&key)
            .cloned();
        if let Some(cached) = cached {
            self.register_shared_instantiation_types(&cached.types);
            let mut info = cached.info.clone();
            info.name = name.clone();
            self.symbols.define_class(name.clone(), info);
            self.generics.active_class_depth -= 1;
            return Some(Type::Named(name));
        }
        if self.reserve_instantiation(&key, span) != Some(true) {
            self.generics.active_class_depth -= 1;
            return Some(Self::error_type());
        }
        self.build_class_instantiation(ClassInstantiation {
            template: &template,
            template_access,
            local_name,
            name,
            key,
            bindings,
            span,
        })
    }

    fn build_class_instantiation(&mut self, instance: ClassInstantiation<'_>) -> Option<Type> {
        let ClassInstantiation {
            template,
            template_access,
            local_name,
            name,
            key,
            bindings,
            span,
        } = instance;
        #[cfg(test)]
        {
            self.generics.registry.borrow_mut().class_specializations += 1;
        }
        let mut class = class_signature(template);
        if !self.generics.abstract_check {
            class.fields = template.fields.clone();
            class.constructors = template.constructors.clone();
        }
        class.methods.retain(|m| {
            m.constraint.as_ref().is_none_or(|bound| {
                bindings
                    .get(&bound.parameter)
                    .is_some_and(|arg| bound.alternatives.contains(arg))
            })
        });
        for method in &mut class.methods {
            method.constraint = None;
        }
        class.name = name.clone();
        class.type_params.clear();
        let (bindings, classes) = self.instantiation_bindings(span, &bindings);
        self.register_shared_instantiation_types(&classes);
        let mut item = Item::Class(class);
        let mut ids = FreshIds::default();
        visit_item(
            &mut item,
            &mut |ty| crate::semantic::generic_inference::substitute(ty, &bindings),
            &mut |_| {},
            true,
            true,
            Some((&template.name, &local_name)),
            &mut ids,
        );
        {
            let mut registry = self.generics.registry.borrow_mut();
            ids.record_bodies(&mut registry.origins);
            for field in &template.fields {
                if let Some(initializer) = &field.initializer
                    && let Some(&instance) = ids.expressions.get(&initializer.id())
                {
                    registry
                        .origins
                        .initializers
                        .insert(instance, initializer.id());
                }
            }
        }
        let Item::Class(mut class) = item else {
            unreachable!()
        };
        let generation = self.error_generation;
        if template_access == template.name {
            self.register_class(&class);
        } else {
            // Registration happens in the consumer, while the queued body is
            // checked in the provider. Only the consumer's signature copy may
            // use its import path for the template's own receiver type.
            let mut signature = class_signature(template);
            signature.methods.retain(|m| {
                m.constraint.as_ref().is_none_or(|bound| {
                    bindings
                        .get(&bound.parameter)
                        .is_some_and(|arg| bound.alternatives.contains(arg))
                })
            });
            for method in &mut signature.methods {
                method.constraint = None;
            }
            signature.name = name.clone();
            signature.type_params.clear();
            let mut signature = Item::Class(signature);
            visit_item(
                &mut signature,
                &mut |ty| {
                    let qualified = ty.map_names(|n| {
                        if n == &template.name {
                            template_access.clone()
                        } else {
                            n.clone()
                        }
                    });
                    crate::semantic::generic_inference::substitute(&qualified, &bindings)
                },
                &mut |_| {},
                false,
                true,
                None,
                &mut FreshIds::default(),
            );
            let Item::Class(signature) = signature else {
                unreachable!()
            };
            self.register_class(&signature);
        }
        self.generics.active_class_depth -= 1;
        if self.error_generation != generation {
            return Some(Self::error_type());
        }
        let info = self
            .symbols
            .lookup_class(&name)
            .expect("registered generic class")
            .clone();
        self.generics
            .registry
            .borrow_mut()
            .instantiated_classes
            .insert(
                key,
                Rc::new(InstantiatedClass {
                    info,
                    types: classes.clone(),
                }),
            );
        if !self.generics.abstract_check {
            class.name = local_name.clone();
            // Method checking only needs the receiver and owner identity.
            // Do not copy fields, constructor bodies, interfaces or the entire
            // method-signature table into each queued method fragment.
            let shell = ClassDecl {
                abi_storage: class.abi_storage.clone(),
                type_params: Vec::new(),
                name: class.name.clone(),
                public: class.public,
                is_open: class.is_open,
                base_class: None,
                implements: Vec::new(),
                source_implements_len: 0,
                fields: Vec::new(),
                methods: Vec::new(),
                constructors: Vec::new(),
                span: class.span,
            };
            self.generics.registry.borrow_mut().deferred_classes.insert(
                (class.span.file_id, local_name),
                Rc::new(DeferredClass {
                    template: template.name.clone(),
                    shell,
                    bindings,
                }),
            );
            self.generics.pending.push_back(Instantiation {
                item: Item::Class(class),
                classes,
                method_only: false,
            });
        }
        Some(Type::Named(name))
    }

    /// Request, from the unit that declares each generic interface `c`
    /// implements, the concrete instance whose defaults `c`'s copies reuse. A copy of another
    /// unit's generic default must name that
    /// unit's items, so it is checked there, once per distinct interface
    /// instantiation, as a non-generic interface; the class's copy is then
    /// rebound to that body when generics are expanded and lowered in that
    /// unit's scope like any other foreign default. Returns the
    /// copies handled this way, which `c` must not check itself.
    pub(in crate::semantic::type_checker) fn instantiate_foreign_defaults<'c>(
        &mut self,
        c: &'c ClassDecl,
    ) -> HashSet<&'c str> {
        let copies: Vec<&MethodDecl> = c
            .methods
            .iter()
            .filter(|m| {
                m.is_interface_default
                    && !m.is_default_injected
                    && m.body.span.file_id != c.span.file_id
            })
            .collect();
        let mut handled = HashSet::new();
        if copies.is_empty() {
            return handled;
        }
        // A template's copies still name its own parameters; each concrete
        // instance of the class requests its own interface instance.
        if self.generics.abstract_check {
            handled.extend(copies.iter().map(|m| m.name.as_str()));
            return handled;
        }
        let copy_names: HashSet<&'c str> = copies.iter().map(|m| m.name.as_str()).collect();
        // Each default method name to the first implemented interface that
        // provides it, in the order desugaring chose the copies in.
        let mut providers: HashMap<&str, (Rc<InterfaceDecl>, &'c [Type], usize)> = HashMap::new();
        for ty in &c.implements {
            let Type::Generic(name, args) = ty else {
                continue;
            };
            let Some(info) = self.symbols.lookup_interface(name) else {
                continue;
            };
            let local = QualifiedName::new(&info.name).last_segment();
            let key = (info.declaration_span.file_id, local.to_string());
            let Some(template) = self
                .generics
                .registry
                .borrow()
                .interfaces
                .get(&key)
                .cloned()
            else {
                continue;
            };
            for (index, m) in template.methods.iter().enumerate() {
                if m.default_body.is_none() {
                    continue;
                }
                if let Some(&name) = copy_names.get(m.name.as_str()) {
                    providers
                        .entry(name)
                        .or_insert_with(|| (Rc::clone(&template), args.as_slice(), index));
                }
            }
        }
        let mut instances: HashMap<*const InterfaceDecl, Rc<InterfaceDecl>> = HashMap::new();
        for m in copies {
            let Some((template, args, index)) = providers.get(m.name.as_str()) else {
                continue;
            };
            // Spans survive class instantiation; body ids are renumbered.
            let declared = &template.methods[*index];
            let provides = declared
                .default_body
                .as_ref()
                .is_some_and(|b| b.span == m.body.span);
            if !provides {
                continue;
            }
            let instance = match instances.get(&Rc::as_ptr(template)) {
                Some(instance) => Rc::clone(instance),
                None => {
                    let Some(instance) =
                        self.instantiate_interface_template(template, args, c.span)
                    else {
                        continue;
                    };
                    instances.insert(Rc::as_ptr(template), Rc::clone(&instance));
                    instance
                }
            };
            self.generics
                .default_copies
                .entry(c.name.clone())
                .or_default()
                .insert(m.name.clone(), (instance, *index));
            handled.insert(m.name.as_str());
        }
        handled
    }

    /// The non-generic instance of generic interface `template` at `args`,
    /// queued once per build for the declaring unit's checker.
    pub(super) fn instantiate_interface_template(
        &mut self,
        template: &Rc<InterfaceDecl>,
        args: &[Type],
        span: Span,
    ) -> Option<Rc<InterfaceDecl>> {
        if args.len() != template.type_params.len() {
            return None;
        }
        let args: Vec<_> = args.iter().map(|t| self.normalize_type(t, span)).collect();
        if args.iter().any(|ty| mentions(ty, Self::is_error_name))
            || !self.check_instantiation_size(&args, span)
        {
            return None;
        }
        let identity_args: Vec<_> = args
            .iter()
            .map(|ty| ty.map_names(|n| self.generic_type_identity(n)))
            .collect();
        let key = (
            2,
            QualifiedName::join(template.span.file_id.0, &template.name),
            identity_args,
        );
        let cached = self
            .generics
            .registry
            .borrow()
            .interface_instances
            .get(&key)
            .cloned();
        if let Some(instance) = cached {
            return Some(instance);
        }
        if self.reserve_instantiation(&key, span) != Some(true) {
            return None;
        }
        #[cfg(test)]
        {
            self.generics
                .registry
                .borrow_mut()
                .interface_specializations += 1;
        }
        let bindings = template.type_params.iter().cloned().zip(args).collect();
        let (bindings, classes) = self.instantiation_bindings(span, &bindings);
        // Source import summaries belong to the template, not each concrete
        // instance. Clone only the methods that substitution already needs.
        let instance = InterfaceDecl {
            default_import_uses: Vec::new(),
            name: symbol(&template.name, &key.2),
            public: template.public,
            type_params: Vec::new(),
            extends: Vec::new(), // inherited methods are already composed
            methods: template.methods.clone(),
            // The opaque receiver keeps the written instantiation.
            instance_of: Some(Type::Generic(
                template.name.clone(),
                template
                    .type_params
                    .iter()
                    .map(|p| Type::Named(p.clone()))
                    .collect(),
            )),
            template_checked: false,
            span: template.span,
        };
        let mut item = Item::Interface(instance);
        substitute_item(&mut item, &bindings)
            .record_bodies(&mut self.generics.registry.borrow_mut().origins);
        let Item::Interface(instance) = item else {
            unreachable!()
        };
        let instance = Rc::new(instance);
        self.generics
            .registry
            .borrow_mut()
            .interface_instances
            .insert(key, Rc::clone(&instance));
        self.generics.pending.push_back(Instantiation {
            method_only: false,
            item: Item::Interface((*instance).clone()),
            classes,
        });
        Some(instance)
    }

    pub(in crate::semantic::type_checker) fn check_constrained_method(
        &mut self,
        method: &MethodDecl,
        class: &ClassDecl,
    ) {
        let bound = method.constraint.as_ref().expect("constrained method");
        if !class.type_params.contains(&bound.parameter) {
            self.generic_error(
                method.span,
                "method constraint must name an enclosing type parameter",
            );
            return;
        }
        // Each alternative is checked at declaration time, including alternatives
        // that no program ever instantiates. No dynamic dispatch is introduced.
        let mut seen = HashSet::new();
        for alternative in &bound.alternatives {
            if !matches!(
                alternative,
                Type::I64 | Type::F64 | Type::Bool | Type::String
            ) {
                self.generic_error(
                    method.span,
                    "method constraint alternatives must be scalar types (i64, f64, bool, String)",
                );
                continue;
            }
            if !seen.insert(alternative.clone()) {
                self.generic_error(method.span, "duplicate method constraint alternative");
                continue;
            }
            let bindings = [(bound.parameter.clone(), alternative.clone())]
                .into_iter()
                .collect();
            let receiver =
                crate::semantic::generic_inference::substitute(&class.receiver_type(), &bindings);
            let receiver = self.normalize_type(&receiver, method.span);
            let copy = ClassDecl {
                abi_storage: None,
                type_params: Vec::new(),
                name: class.name.clone(),
                public: class.public,
                is_open: false,
                base_class: None,
                implements: Vec::new(),
                source_implements_len: 0,
                fields: Vec::new(),
                methods: vec![method.clone()],
                constructors: Vec::new(),
                span: class.span,
            };
            let mut item = Item::Class(copy);
            visit_item(
                &mut item,
                &mut |ty| crate::semantic::generic_inference::substitute(ty, &bindings),
                &mut |_| {
                    #[cfg(test)]
                    CONSTRAINED_BODY_VISITS.with(|count| count.set(count.get() + 1));
                },
                true,
                false,
                None,
                &mut FreshIds::default(),
            );
            let Item::Class(copy) = item else {
                unreachable!()
            };
            self.check_method(&copy.methods[0], &copy, &receiver);
        }
    }
}
