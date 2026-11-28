use super::*;

#[willow_continuations::checker]
impl TypeChecker {
    pub(in crate::semantic::type_checker) fn register_builtin_panic_surface(&mut self) {
        let mut fields = HashMap::new();
        for (name, ty) in crate::semantic::builtin_types::panic_info_fields() {
            fields.insert(
                name.to_string(),
                FieldInfo {
                    ty: ty.clone(),
                    public: true,
                    protected: false,
                    declaration_span: Span::dummy(),
                    // `PanicInfo` fields are read-only (E1701).
                    reassigned: false,
                },
            );
        }
        self.symbols.define_class(
            "PanicInfo".to_string(),
            ClassInfo {
                name: "PanicInfo".to_string(),
                public: true,
                is_open: false,
                base_class: None,
                implements: Vec::new(),
                declaration_span: Span::dummy(),
                fields,
                methods: HashMap::new(),
                static_props: HashMap::new(),
                instance_field_order: crate::semantic::builtin_types::panic_info_fields()
                    .into_iter()
                    .map(|(name, ty)| (name.to_string(), ty))
                    .collect(),
                // Runtime-only construction is enforced explicitly by
                // `check_new`; `None` must not be interpreted as a public
                // memberwise constructor for this compiler-known type.
                constructor: None,
            },
        );

        self.symbols.define_func(
            "recover".to_string(),
            FuncInfo {
                pure: false,
                native: None,
                foreign: false,
                sending: false,
                params: Vec::new(),
                param_infos: Vec::new(),
                return_type: Builtin::Option.apply(vec![Type::Named("PanicInfo".to_string())]),
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
                constant: None,
            },
        );
    }

    pub(in crate::semantic::type_checker) fn register_builtin_functions(&mut self) {
        for name in ["pow", "powf"] {
            let params = vec![Type::F64, Type::F64];
            self.symbols.define_func(
                name.to_string(),
                FuncInfo {
                    pure: false,
                    native: None,
                    foreign: false,
                    sending: false,
                    param_infos: value_param_infos(&params),
                    params,
                    return_type: Type::F64,
                    public: true,
                    is_async: false,
                    declaration_span: Span::dummy(),
                    module_path: None,
                    constant: None,
                },
            );
        }
        self.symbols.define_func(
            "gc_collect".to_string(),
            FuncInfo {
                pure: false,
                native: None,
                foreign: false,
                sending: false,
                param_infos: vec![],
                params: vec![],
                return_type: Type::Void,
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
                constant: None,
            },
        );
        for name in ["gc_minor_collect", "gc_task_frame_diagnostics"] {
            self.symbols.define_func(
                name.to_string(),
                FuncInfo {
                    pure: false,
                    native: None,
                    foreign: false,
                    sending: false,
                    param_infos: vec![],
                    params: vec![],
                    return_type: Type::Void,
                    public: true,
                    is_async: false,
                    declaration_span: Span::dummy(),
                    module_path: None,
                    constant: None,
                },
            );
        }
        for name in [
            "sched_local_pop_hits",
            "sched_global_pop_hits",
            "sched_global_pop_attempts",
            "sched_steal_attempts",
            "sched_steal_successes",
            "sched_steal_failures",
            "sched_victim_locks",
            "sched_global_pushes",
            "sched_local_pushes",
            "gc_allocated_bytes",
            "gc_total_allocated_bytes",
            "gc_tlab_fast_allocations",
            "gc_tlab_slow_allocations",
            "gc_tlab_refills",
            "gc_tlab_large_allocations",
            "gc_tlab_reserved_bytes",
            "gc_minor_collections",
            "gc_promoted_objects",
            "gc_moved_objects",
            "gc_pause_count",
            "gc_pause_total_ns",
            "gc_pause_max_ns",
            "gc_survivor_copies",
            "gc_survivor_bytes",
            "gc_tenured_objects",
            "gc_tenured_bytes",
            "gc_pinned_promotions",
            "gc_survivor_space_reserved",
            "gc_survivor_space_live",
            "gc_remembered_set_size",
            "gc_dirty_card_count",
            "gc_write_barrier_hits",
            "gc_old_region_count",
            "gc_old_region_reserved_bytes",
            "gc_old_region_live_bytes",
            "gc_old_region_fragmentation_bytes",
            "gc_large_object_region_count",
            "gc_pinned_region_count",
            "gc_old_region_allocations",
            "gc_old_region_reuses",
            "gc_old_regions_released",
            "gc_major_collections",
        ] {
            self.symbols.define_func(
                name.to_string(),
                FuncInfo {
                    pure: false,
                    native: None,
                    foreign: false,
                    sending: false,
                    param_infos: vec![],
                    params: vec![],
                    return_type: Type::I64,
                    public: true,
                    is_async: false,
                    declaration_span: Span::dummy(),
                    module_path: None,
                    constant: None,
                },
            );
        }
        // panic(message: String) — noreturn; type-checker returns Never
        let panic_params = vec![Type::String];
        self.symbols.define_func(
            "panic".to_string(),
            FuncInfo {
                pure: false,
                native: None,
                foreign: false,
                sending: false,
                param_infos: value_param_infos(&panic_params),
                params: panic_params,
                return_type: Type::Never,
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
                constant: None,
            },
        );
        let sleep_params = vec![Type::I64];
        self.symbols.define_func(
            "sleep".to_string(),
            FuncInfo {
                pure: false,
                native: None,
                foreign: false,
                sending: false,
                param_infos: value_param_infos(&sleep_params),
                params: sleep_params,
                return_type: Type::Generic("Future".to_string(), vec![Type::Void]),
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
                constant: None,
            },
        );
        self.symbols.define_func(
            "yield".to_string(),
            FuncInfo {
                pure: false,
                native: None,
                foreign: false,
                sending: false,
                param_infos: vec![],
                params: vec![],
                return_type: Type::Generic("Future".to_string(), vec![Type::Void]),
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
                constant: None,
            },
        );
    }

    pub(in crate::semantic::type_checker) fn register_builtin_modules(&mut self) {
        for &module_name in crate::stdlib_schema::FUNCTION_MODULES {
            self.register_schema_module(module_name);
        }
    }

    pub(super) fn register_schema_module(&mut self, module_name: &str) {
        self.register_schema_module_as(module_name, module_name);
    }

    pub(super) fn register_schema_module_as(&mut self, module_name: &str, local_name: &str) {
        let functions = crate::stdlib_schema::module(module_name)
            .expect("stdlib schema must define the builtin module")
            .items
            .iter()
            .filter_map(|item| {
                let crate::stdlib_schema::StdItemKind::Function {
                    params,
                    return_type,
                } = item.kind
                else {
                    return None;
                };
                let params = params
                    .iter()
                    .copied()
                    .map(std_schema_type)
                    .collect::<Vec<_>>();
                Some((
                    item.name.to_string(),
                    FuncInfo {
                        pure: false,
                        native: None,
                        foreign: false,
                        sending: false,
                        param_infos: value_param_infos(&params),
                        params,
                        return_type: std_schema_type(return_type),
                        public: true,
                        is_async: false,
                        declaration_span: Span::dummy(),
                        module_path: None,
                        constant: None,
                    },
                ))
            })
            .collect();
        self.symbols
            .define_module(local_name.to_string(), ModuleInfo { functions });
    }

    /// Register an imported module's items so cross-module calls can report
    /// missing and private-item diagnostics accurately.
    /// Register a prelude enum so it is available in all user programs.
    pub fn register_prelude_enum(&mut self, decl: &crate::parser::ast::EnumDecl) {
        self.register_enum(decl);
    }

    pub fn register_prelude_interface(&mut self, decl: &crate::parser::ast::InterfaceDecl) {
        self.register_interface(decl, None);
    }

    pub(crate) fn set_registration_import_paths(
        &mut self,
        paths: std::rc::Rc<HashMap<String, String>>,
    ) {
        self.registration_import_paths = paths;
    }

    pub(super) fn registration_module_path(&self, path: &str) -> Option<&String> {
        if super::precheck_journal::is_active() {
            super::precheck_journal::read(
                super::precheck_journal::Key::new("registration_path", Some(path.into())),
                serde_json::to_value(self.registration_import_paths.get(path)).unwrap(),
            );
        }
        self.registration_import_paths
            .get(path)
            .or_else(|| self.module_access_names.get(path))
    }

    pub fn register_module(&mut self, name: &str, canonical: &str, path: &str, program: &Program) {
        self.register_module_impl(None, name, canonical, path, program, ModuleScope::Imported);
    }

    pub fn register_module_with_id(
        &mut self,
        id: crate::module::ModuleId,
        name: &str,
        canonical: &str,
        path: &str,
        program: &Program,
    ) {
        self.register_module_impl(
            Some(id),
            name,
            canonical,
            path,
            program,
            ModuleScope::Imported,
        );
    }

    /// Register only the TYPES of a module this unit did not import, under the
    /// module's canonical path.
    ///
    /// A unit sees exactly the modules it imports -- except that an imported
    /// module's public signature may name a type from further down: `mid`
    /// exports `Crate extends Parcel`, and `Parcel` belongs to `base`. Without
    /// `base::Parcel` in the table the subclass has a base class that resolves
    /// to nothing, so it exports none of what it inherits. The
    /// module itself is deliberately NOT defined: nothing here lets the
    /// importing unit call into `base` or collide with its name.
    pub fn register_module_type_signatures(
        &mut self,
        canonical: &str,
        path: &str,
        program: &Program,
    ) {
        self.register_module_impl(
            None,
            canonical,
            canonical,
            path,
            program,
            ModuleScope::Types,
        );
    }

    /// Preserve the source spelling of a metadata-only module for scope errors.
    /// Canonical declaration keys cannot be used to recognize source tokens.
    pub(crate) fn register_signature_only_spelling(&mut self, spelling: &str) {
        self.signature_only_modules.insert(spelling.to_string());
    }

    /// Let `spelling` name a module already registered as `registered`.
    ///
    /// Declarations are registered once under their canonical namespace.
    /// Consumer spellings bind to that registration, retaining the same IDs and
    /// metadata rather than declaring a second set of classes/interfaces.
    pub fn alias_module_spelling(
        &mut self,
        id: crate::module::ModuleId,
        spelling: &str,
        registered: &str,
        program: &Program,
    ) {
        self.alias_module_spellings(&[(id, spelling, registered, program)]);
    }

    /// Freeze every target before any source spelling shadows a declaration.
    pub(crate) fn alias_module_spellings(
        &mut self,
        bindings: &[(crate::module::ModuleId, &str, &str, &Program)],
    ) {
        enum TypeTarget {
            Class(ClassInfo),
            Enum(EnumInfo),
            Interface(InterfaceInfo),
        }
        struct Targets<'a> {
            id: crate::module::ModuleId,
            spelling: &'a str,
            registered: &'a str,
            types: Vec<(String, TypeTarget)>,
            functions: Vec<(String, std::rc::Rc<FunctionDecl>)>,
            classes: Vec<(String, std::rc::Rc<ClassDecl>)>,
            module: Option<ModuleInfo>,
        }
        let targets: Vec<_> = bindings
            .iter()
            .filter(|(_, spelling, registered, _)| spelling != registered)
            .map(|&(id, spelling, registered, program)| {
                let mut types = Vec::new();
                let mut functions = Vec::new();
                let mut classes = Vec::new();
                for item in &program.items {
                    let declared = match item {
                        Item::Class(c) => &c.name,
                        Item::Interface(i) => &i.name,
                        Item::Enum(e) => &e.name,
                        Item::Function(f) if !f.type_params.is_empty() => &f.name,
                        Item::Function(_) => continue,
                    };
                    let from = QualifiedName::join(registered, declared);
                    let to = QualifiedName::join(spelling, declared);
                    match item {
                        Item::Function(f) if !f.type_params.is_empty() => {
                            if let Some(template) = self.generics.functions.get(&from) {
                                functions.push((to, std::rc::Rc::clone(template)));
                            }
                        }
                        Item::Function(_) => {}
                        _ => {
                            if let Some(info) = self.symbols.lookup_class(&from) {
                                types.push((to.clone(), TypeTarget::Class(info.clone())));
                            } else if let Some(info) = self.symbols.lookup_interface(&from) {
                                types.push((to.clone(), TypeTarget::Interface(info.clone())));
                            } else if let Some(info) = self.symbols.lookup_enum(&from) {
                                types.push((to.clone(), TypeTarget::Enum(info.clone())));
                            }
                            if let Item::Class(c) = item
                                && !c.type_params.is_empty()
                                && let Some(template) = self.generics.classes.get(&from)
                            {
                                classes.push((to, std::rc::Rc::clone(template)));
                            }
                        }
                    }
                }
                Targets {
                    id,
                    spelling,
                    registered,
                    types,
                    functions,
                    classes,
                    module: self.symbols.lookup_module(registered).cloned(),
                }
            })
            .collect();
        for target in targets {
            for (name, info) in target.types {
                match info {
                    TypeTarget::Class(info) => self.symbols.define_class(name, info),
                    TypeTarget::Enum(info) => self.symbols.define_enum(name, info),
                    TypeTarget::Interface(info) => self.symbols.define_interface(name, info),
                }
            }
            for (name, template) in target.functions {
                self.generics.functions.insert(name.clone(), template);
                self.generics
                    .function_paths
                    .insert(name, target.registered.to_string());
            }
            for (name, template) in target.classes {
                self.generics.classes.insert(name.clone(), template);
                self.generics
                    .class_paths
                    .insert(name, target.registered.to_string());
            }
            if let Some(module) = target.module {
                self.symbols
                    .define_module_with_id(target.spelling.to_string(), target.id, module);
            }
            self.imported_names
                .insert(target.spelling.to_string(), None);
        }
    }

    /// Hide another unit's access name without rebuilding its type identities.
    pub fn hide_module_spelling(&mut self, name: &str) {
        self.symbols.hide_module_spelling(name);
        self.imported_names.remove(name);
        self.signature_only_modules.insert(name.to_string());
    }

    /// `name` is what THIS unit calls the module -- an alias, or the last
    /// segment of its path -- and is the spelling its items are registered
    /// under. `canonical` is the module's one identity in the build, which is
    /// what a type declared in it is NAMED after, so two modules
    /// that both declare `Level` stay distinct wherever both are in scope.
    pub(super) fn register_module_impl(
        &mut self,
        id: Option<crate::module::ModuleId>,
        name: &str,
        canonical: &str,
        path: &str,
        program: &Program,
        scope: ModuleScope,
    ) {
        self.register_instantiation_types(&program.instantiation_types);
        match scope {
            ModuleScope::Types => {
                self.signature_only_modules.insert(canonical.to_string());
            }
            ModuleScope::Imported => {
                self.signature_only_modules.remove(canonical);
            }
        }
        // The spelling this checker reaches the module by, for the modules
        // registered AFTER it: dependency order means an importer is registered
        // once its dependencies already answer to a name here.
        // The entry file registers some modules a second time under their
        // canonical path, which must not displace the access spelling the rest
        // of the build's types were qualified with.
        self.module_access_names
            .entry(canonical.to_string())
            .or_insert_with(|| name.to_string());

        // Type names declared in this module. A module function signature that
        // names one of them by bare name is qualified to `module::Name` so the
        // importing file resolves it -- an interface argument against the right
        // vtable, and an ENUM against the declaration it actually
        // came from: two modules that each declare `pub enum
        // Level` both presented a bare `Level` to an importer, which then read
        // one's tag as the other's variant. Classes need the same distinction
        // at a call site: two imported modules may both return a class called
        // `Point`, and leaving both return types bare loses which layout the
        // value uses. A directly-imported alias still matches because the local
        // binding carries the same registered class identity. Builtin generics
        // are left untouched.
        let local_interfaces: HashSet<String> = program
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Interface(i) => Some(i.name.clone()),
                _ => None,
            })
            .collect();
        let local_enums: HashSet<String> = program
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Enum(e) => Some(e.name.clone()),
                _ => None,
            })
            .collect();
        let local_classes: HashSet<String> = program
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Class(c) => Some(c.name.clone()),
                _ => None,
            })
            .collect();
        // An interface is qualified by the spelling THIS unit reaches the module
        // by, because that is the key its vtable is registered under here; an
        // enum by the module's canonical identity, because that is the one name
        // it has build-wide. A class uses the access name because that is the
        // key its layout and methods are registered under in this checker.
        // An enum this module reached through an item import of its own is
        // spelled by whatever local name that import bound — `Grade`, under
        // `import signal::Level as Grade;` — and that name means nothing in the
        // importing file. Modules are registered in dependency order, so the
        // declaring module's enum is already in the table and can answer with
        // the identity both units share.
        // A CLASS or INTERFACE reached the same way is spelled the same way,
        // and `mid::Parcel` — what blindly prefixing the declaring module's own
        // name used to produce for `import base::Parcel;` — is a class no unit
        // declares, so a subclass exported nothing it inherited and a signature
        // mentioning the type resolved against nothing. Unlike an
        // enum, a class answers here to the spelling THIS checker registered its
        // module under, not to its canonical path, so the import's canonical
        // module path is translated through `module_access_names` first.
        let imported_types: HashMap<String, String> = program
            .imports
            .iter()
            .filter(|import| {
                self.registration_module_path(import.path.as_str())
                    .is_none()
            })
            .filter_map(|import| {
                let (module_path, item) = QualifiedName::new(&import.path).split_last()?;
                let local = import.alias.clone().unwrap_or_else(|| item.to_string());
                if let Some(info) = self.symbols.lookup_enum(&import.path) {
                    return Some((local, info.name.clone()));
                }
                let access = self
                    .registration_module_path(module_path)
                    .map(String::as_str)
                    .unwrap_or(module_path);
                let registered = QualifiedName::join(access, item);
                if let Some(info) = self.symbols.lookup_enum(&registered) {
                    return Some((local, info.name.clone()));
                }
                (self.symbols.lookup_class(&registered).is_some()
                    || self.symbols.lookup_interface(&registered).is_some())
                .then_some((local, registered))
            })
            .collect();
        // A path this module writes for a module IT imports -- `biz::Amount`
        // under `import sales as biz;` -- is spelled for this module alone. The
        // checker keys that module's types by its canonical namespace, so the
        // exported signature is translated into it; otherwise one class under
        // two spellings is two types and nothing an importer passes ever
        // matches. Dependency order guarantees the map is
        // complete: a module is registered only once its own imports are.
        let own_module_prefixes: HashMap<String, String> = program
            .imports
            .iter()
            .filter_map(|import| {
                let registered = self.registration_module_path(import.path.as_str())?;
                let access = import
                    .alias
                    .clone()
                    .unwrap_or_else(|| QualifiedName::new(&import.path).last_segment().to_string());
                (access != *registered).then(|| (access, registered.clone()))
            })
            .collect();
        let storage_templates: HashMap<_, _> = self
            .generics
            .classes
            .iter()
            .filter(|(_, template)| template.abi_storage.is_some())
            .map(|(name, template)| (name.clone(), template.clone()))
            .collect();
        self.provider_template_files
            .extend(program.items.iter().filter_map(|item| match item {
                Item::Function(f) if !f.type_params.is_empty() => Some(f.span.file_id),
                Item::Class(c) if !c.type_params.is_empty() => Some(c.span.file_id),
                _ => None,
            }));
        let mut functions = crate::semantic::ids::FunctionMap::default();
        for item in &program.items {
            let qualify = |ty: &Type| {
                // Rewrite only paths the source wrote. Names introduced below are
                // already identities in this checker, even if their prefix also
                // happens to be one of the source module's import aliases.
                let ty = rename_module_prefix(ty, &own_module_prefixes);
                let ty = qualify_local_type(&ty, name, &local_interfaces);
                let ty = qualify_local_type(&ty, name, &local_classes);
                let ty = qualify_local_type(&ty, canonical, &local_enums);
                let ty = rename_imported_type(&ty, &imported_types);
                let ty = ty.map_names(|name| {
                    #[cfg(test)]
                    ENUM_IDENTITY_LOOKUPS.with(|count| count.set(count.get() + 1));
                    // A carried nominal alias remains resolvable in the provider;
                    // its caller-local enum name need not be in that scope.
                    if self.generics.nominal_aliases.contains_key(name) {
                        name.clone()
                    } else {
                        self.symbols
                            .lookup_enum(name)
                            .map_or_else(|| name.clone(), |info| info.name.clone())
                    }
                });
                super::bundled_storage_spelling(&storage_templates, &ty)
            };

            match item {
                Item::Function(f) => {
                    if !f.type_params.is_empty() {
                        let key = QualifiedName::join(name, &f.name);
                        let template = self.generics.registry.borrow_mut().intern_function(f);
                        self.generics.functions.insert(key.clone(), template);
                        self.generics.function_paths.insert(key, name.to_string());
                    }
                    let params = f.params.iter().map(|p| qualify(&p.ty)).collect::<Vec<_>>();
                    let mut param_infos = param_infos_from_decl(&f.params, None);
                    for pi in &mut param_infos {
                        pi.ty = qualify(&pi.ty);
                    }
                    let identity = FunctionId::free(&f.name).in_namespace(canonical);
                    // Function values retain the declaring namespace even when
                    // the module has no interned SymbolModule identity yet.
                    // The backend declares this same canonical callable.
                    functions.scope().declare(&f.name, identity);
                    functions.insert(
                        f.name.clone(),
                        FuncInfo {
                            pure: f
                                .effect_contracts
                                .iter()
                                .any(|a| a.kind == EffectContractKind::Pure),
                            native: f
                                .rust_bridge
                                .as_deref()
                                .and_then(NativeFunction::from_bridge),
                            foreign: f
                                .rust_bridge
                                .as_ref()
                                .is_some_and(|bridge| !bridge.assumes_no_block()),
                            sending: super::signature::sending_function(f),
                            param_infos,
                            params,
                            return_type: qualify(&f.return_type),
                            public: f.public(),
                            is_async: f.is_async(),
                            declaration_span: f.span,
                            module_path: Some(path.to_string()),
                            constant: f.constant.clone(),
                        },
                    );
                }
                Item::Class(c) => {
                    if !c.type_params.is_empty() {
                        let key = QualifiedName::join(name, &c.name);
                        let template = self.generics.registry.borrow_mut().intern_class(c);
                        self.generics.classes.insert(key.clone(), template);
                        self.generics.class_paths.insert(key, name.to_string());
                    }
                    let class_name = QualifiedName::join(name, &c.name);
                    let storage = c.abi_storage.as_ref().map(&qualify);
                    let info = imported_class_info_from_decl(c, &class_name, &qualify, &|field| {
                        self.field_reassigned(field)
                    });
                    if let Some(storage) = storage {
                        let key = self.declaration_type_key(&storage);
                        self.array_classes.insert(key, class_name.clone());
                        self.array_abi_files.insert(c.span.file_id);
                        self.normalized_types
                            .insert(Type::Named(class_name.clone()), storage);
                    }
                    self.symbols.define_class(class_name, info);
                }
                Item::Enum(e) => {
                    let mut info = imported_enum_info(e, canonical, &qualify);
                    // Payloads are qualified name by name; the bundled storage
                    // spelling is a whole-type rewrite.
                    for variant in &mut info.variants {
                        for ty in &mut variant.payload_types {
                            *ty = super::bundled_storage_spelling(&storage_templates, ty);
                        }
                    }
                    let access = QualifiedName::join(name, &e.name);
                    if access != info.name {
                        self.symbols.define_enum(info.name.clone(), info.clone());
                    }
                    self.symbols.define_enum(access, info);
                }
                Item::Interface(i) => {
                    // Register imported interfaces under `module::Interface` so
                    // `animals::Animal` resolves as a type and in `implements`.
                    let mut declaration = i.clone();
                    let qualify_bound = |ty: &Type| {
                        let ty = ty.map_names(|n| {
                            if n == "Self" || i.type_params.contains(n) {
                                return n.clone();
                            }
                            let Type::Named(ref result) = qualify(&Type::Named(n.clone())) else {
                                unreachable!()
                            };
                            result.clone()
                        });
                        super::bundled_storage_spelling(&storage_templates, &ty)
                    };
                    for method in &mut declaration.methods {
                        method.return_type = qualify_bound(&method.return_type);
                        for param in &mut method.params {
                            param.ty = qualify_bound(&param.ty);
                        }
                    }
                    declaration.extends = i
                        .extends
                        .iter()
                        .map(|base| {
                            let Type::Named(ref result) = qualify(&Type::Named(base.clone()))
                            else {
                                unreachable!()
                            };
                            result.clone()
                        })
                        .collect();
                    self.register_interface(&declaration, Some(name));
                }
            }
        }
        if matches!(scope, ModuleScope::Types) {
            return;
        }
        let info = ModuleInfo { functions };
        if let Some(id) = id {
            self.symbols
                .define_module_with_id(name.to_string(), id, info);
        } else {
            self.symbols.define_module(name.to_string(), info);
        }
        self.imported_names.insert(name.to_string(), None);
    }

    /// Bind a single-item import (`import math::add;`) into the current scope:
    /// `local` resolves to the public function `item` of module `module`.
    pub fn register_item_import(&mut self, local: &str, module: &str, item: &str, span: Span) {
        self.imported_names.insert(local.to_string(), Some(span));
        let key = QualifiedName::join(module, item);
        if let Some(template) = self.generics.functions.get(&key).cloned() {
            let origin = self
                .generics
                .function_paths
                .get(&key)
                .cloned()
                .unwrap_or_else(|| module.to_string());
            self.generics.functions.insert(local.to_string(), template);
            self.generics
                .function_paths
                .insert(local.to_string(), origin);
        }
        if let Some(template) = self.generics.classes.get(&key).cloned() {
            let origin = self
                .generics
                .class_paths
                .get(&key)
                .cloned()
                .unwrap_or_else(|| module.to_string());
            self.generics.classes.insert(local.to_string(), template);
            self.generics.class_paths.insert(local.to_string(), origin);
        }

        // Functions are registered per-module in the ModuleInfo table.
        let func = self.symbols.lookup_module_func(module, item).cloned();
        if let Some(info) = func {
            if info.public {
                self.symbols.define_func(local.to_string(), info);
            } else {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E2006,
                        format!("item `{item}` is private in module `{module}`"),
                    )
                    .with_label(Label::primary(span, "private item"))
                    .with_help(format!("mark `{item}` as `pub` in module `{module}`")),
                );
            }
            return;
        }

        // Types (class / interface / enum) are registered under `module::Item`.
        // Bind them under the unqualified local name too, with a
        // visibility check (E0419 for a private type).
        let qualified = QualifiedName::join(module, item);
        let private_type = |this: &mut Self| {
            this.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0419,
                    format!("type `{item}` is private to module `{module}`"),
                )
                .with_label(Label::primary(span, "private type"))
                .with_help(format!("mark `{item}` as `pub` in module `{module}`")),
            );
        };
        if let Some(info) = self.symbols.lookup_class(&qualified).cloned() {
            if info.public {
                self.symbols.define_class(local.to_string(), info);
            } else {
                private_type(self);
            }
        } else if let Some(info) = self.symbols.lookup_interface(&qualified).cloned() {
            if info.public {
                self.symbols.define_interface(local.to_string(), info);
            } else {
                private_type(self);
            }
        } else if let Some(info) = self.symbols.lookup_enum(&qualified).cloned() {
            if info.public {
                self.symbols.define_enum(local.to_string(), info);
            } else {
                private_type(self);
            }
        } else {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2006,
                    format!("no item `{item}` in module `{module}`"),
                )
                .with_label(Label::primary(span, "unknown module item")),
            );
        }
    }

    pub(in crate::semantic::type_checker) fn register_std_imports(
        &mut self,
        imports: &[ImportDecl],
    ) {
        for import in imports {
            if !std_registry::is_builtin_std_path(&import.path) {
                continue;
            }
            if std_registry::resolve_std_import(&import.path, import.span).is_err() {
                continue;
            }
            let segs = std_registry::import_segments(&import.path);
            match segs.as_slice() {
                ["std", "collections"] => {
                    let local = import.alias.as_deref().unwrap_or("collections");
                    self.imported_names
                        .insert(local.to_string(), Some(import.span));
                    if import.alias.is_none() {
                        self.imported_std_modules.insert(
                            local.to_string(),
                            ImportedStdModule {
                                module: "collections".to_string(),
                                span: import.span,
                            },
                        );
                        self.imported_collection_types
                            .insert(Builtin::Array.source_name().to_string());
                        self.imported_collection_types
                            .insert(Builtin::Map.source_name().to_string());
                    }
                }
                ["std", "collections", item]
                    if matches!(
                        Builtin::from_name(item),
                        Some(
                            Builtin::Array
                                | Builtin::Map
                                | Builtin::FrozenArray
                                | Builtin::FrozenMap
                        )
                    ) =>
                {
                    let local = import.alias.as_deref().unwrap_or(item);
                    self.imported_names
                        .insert(local.to_string(), Some(import.span));
                    self.imported_collection_aliases
                        .insert(local.to_string(), (*item).to_string());
                    self.imported_collection_targets.insert((*item).to_string());
                    if import.alias.is_none() {
                        self.imported_collection_types.insert((*item).to_string());
                    }
                }
                ["std", module, item] => {
                    let local = import.alias.as_deref().unwrap_or(item);
                    self.imported_names
                        .insert(local.to_string(), Some(import.span));
                    if *module == "fs" {
                        let operation = match *item {
                            "read_to_string" => Some("fs::read_to_string"),
                            "write_string" => Some("fs::write_string"),
                            "exists" => Some("fs::exists"),
                            "remove_file" => Some("fs::remove_file"),
                            _ => None,
                        };
                        if let Some(operation) = operation {
                            self.imported_blocking_std_functions
                                .insert(local.to_string(), operation);
                        }
                    }
                }
                ["std", module] => {
                    let local = import.alias.as_deref().unwrap_or(module);
                    self.imported_names
                        .insert(local.to_string(), Some(import.span));
                    if import.alias.is_some() && crate::stdlib_schema::is_function_module(module) {
                        self.register_schema_module_as(module, local);
                    }
                    // Type-bearing modules need their alias too: res::Result
                    // must resolve identically with and without expected type.
                    self.imported_std_modules.insert(
                        local.to_string(),
                        ImportedStdModule {
                            module: (*module).to_string(),
                            span: import.span,
                        },
                    );
                }
                _ => {}
            }
        }
    }

    pub(in crate::semantic::type_checker) fn resolve_imported_std_module_item(
        &mut self,
        qualified_name: &str,
        span: Span,
    ) -> Option<(String, String)> {
        let (module_local, item) = QualifiedName::new(&qualified_name).split_first()?;
        let imported = self.imported_std_modules.get(module_local).cloned()?;
        let path = QualifiedName::from_segments(["std", &imported.module, item]);
        match std_registry::resolve_std_import(&path, span) {
            Ok(std_registry::StdImport::Item { module, item }) => Some((module, item)),
            Ok(std_registry::StdImport::Module { .. }) => None,
            Err(diag) => {
                self.push(diag.with_label(Label::secondary(imported.span, "module imported here")));
                None
            }
        }
    }

    pub(in crate::semantic::type_checker) fn resolve_fully_qualified_std_item(
        &mut self,
        qualified_name: &str,
        span: Span,
    ) -> Option<(String, String)> {
        if !std_registry::is_builtin_std_path(qualified_name) {
            return None;
        }
        match std_registry::resolve_std_import(qualified_name, span) {
            Ok(std_registry::StdImport::Item { module, item }) => Some((module, item)),
            Ok(std_registry::StdImport::Module { .. }) => None,
            Err(diag) => {
                self.push(diag);
                None
            }
        }
    }

    /// Register an enum declared in the program this checker is checking.
    ///
    /// The KEY is the bare name, because that is how the declaring body spells
    /// it. The IDENTITY -- `EnumInfo::name` -- is `module::Enum` whenever the
    /// program is a module's: two modules that each declare `pub
    /// enum Level` are two different types, and the back-end's enum table is one
    /// namespace for the whole build, so a bare `Level` there could only ever
    /// answer for one of them. The canonical spelling is registered as a key of
    /// its own as well, since `normalize_type` rewrites every reference to it.
    pub(in crate::semantic::type_checker) fn register_enum(&mut self, decl: &EnumDecl) {
        let tuple = crate::parser::tuples::is_tuple(&decl.name);
        let outer = (!tuple)
            .then(|| self.enter_type_parameters(decl.type_params.iter().cloned().collect()));
        let mut variant_infos = Vec::new();
        for (tag, variant) in decl.variants.iter().enumerate() {
            variant_infos.push(EnumVariantInfo {
                name: variant.name.clone(),
                payload_types: variant
                    .payload
                    .iter()
                    .map(|ty| {
                        if tuple {
                            ty.clone()
                        } else {
                            self.normalize_type(ty, variant.span)
                        }
                    })
                    .collect(),
                tag: tag as i64,
                declaration_span: variant.span,
            });
        }
        if let Some(outer) = outer {
            self.local.declared_type_params = outer;
        }
        self.define_declared_enum(decl, variant_infos);
    }

    /// Register an enum's IDENTITY ahead of every declaration's types. `normalize_type`
    /// rewrites a bare enum name to its
    /// canonical `module::Enum` spelling by looking the name up, so a class
    /// field, interface signature or payload written before the enum's
    /// declaration used to keep the bare spelling and then mismatch every
    /// `module::Enum` value. [`Self::register_enum`] replaces this entry with
    /// the full declaration, payloads included.
    pub(in crate::semantic::type_checker) fn predeclare_enum(&mut self, decl: &EnumDecl) {
        self.define_declared_enum(decl, Vec::new());
    }

    pub(super) fn define_declared_enum(&mut self, decl: &EnumDecl, variants: Vec<EnumVariantInfo>) {
        let canonical = match self.module_path.as_deref() {
            Some(module) if !crate::parser::tuples::is_tuple(&decl.name) => {
                QualifiedName::join(module, &decl.name)
            }
            _ => decl.name.clone(),
        };
        let info = EnumInfo {
            name: canonical.clone(),
            public: decl.public,
            type_params: decl.type_params.clone(),
            variants,
            declaration_span: decl.span,
        };
        if canonical != decl.name {
            self.symbols.define_enum(canonical, info.clone());
        }
        self.symbols.define_enum(decl.name.clone(), info);
    }

    pub(in crate::semantic::type_checker) fn register_class(
        &mut self,
        c: &ClassDecl,
        isolation: Option<&precheck::ClassIsolation>,
    ) {
        let identity = if QualifiedName::new(&c.name).is_qualified() {
            c.name.clone()
        } else {
            self.class_module_path
                .as_ref()
                .or(self.module_path.as_ref())
                .map_or_else(
                    || c.name.clone(),
                    |module| QualifiedName::join(module, &c.name),
                )
        };
        if let Some(storage) = &c.abi_storage {
            self.normalized_types
                .insert(Type::Named(c.name.clone()), storage.clone());
            self.normalized_types
                .insert(Type::Named(identity.clone()), storage.clone());
        }
        let info = self.class_info_from_decl(c, &identity, None, isolation);
        if identity != c.name {
            self.symbols.define_class(identity, info.clone());
        }
        self.symbols.define_class(c.name.clone(), info);
    }

    pub(in crate::semantic::type_checker) fn register_interface(
        &mut self,
        decl: &InterfaceDecl,
        module_path: Option<&str>,
    ) {
        let outer_params = module_path.is_none().then(|| {
            self.enter_type_parameters(
                decl.type_params
                    .iter()
                    .cloned()
                    .chain(std::iter::once("Self".into()))
                    .collect(),
            )
        });
        let registered_name = match module_path {
            Some(module) => QualifiedName::join(module, &decl.name),
            None => decl.name.clone(),
        };
        let mut methods = HashMap::new();
        let mut method_order = Vec::new();
        for m in &decl.methods {
            let outer_metadata = self.signature_metadata;
            self.signature_metadata |= m.span.file_id != decl.span.file_id;
            // Signature types are normalized here and VALIDATED in
            // `check_interface`, once every declaration is registered: a
            // method may name a type declared later in the file, and a
            // generic interface's `T` is only known to be a parameter once
            // the whole declaration is in view. An imported
            // module's interface keeps its own spellings; the module's
            // checker has already judged them.
            let return_type = if module_path.is_none() {
                self.normalize_type(&m.return_type, m.span)
            } else {
                m.return_type.clone()
            };
            let params: Vec<Type> = if module_path.is_none() {
                m.params
                    .iter()
                    .map(|param| self.normalize_type(&param.ty, param.type_span))
                    .collect()
            } else {
                m.params.iter().map(|p| p.ty.clone()).collect()
            };
            self.signature_metadata = outer_metadata;
            if methods.contains_key(&m.name) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0502,
                        format!(
                            "method `{}` is declared more than once in interface `{}`",
                            m.name, decl.name
                        ),
                    )
                    .with_label(Label::primary(m.span, "duplicate interface method")),
                );
                continue;
            }
            method_order.push(m.name.clone());
            // Keep the declared `&`/`&mut` mode beside each normalized type: a
            // reference parameter is passed as a pointer, so dropping the mode
            // here would let a value-parameter implementation satisfy a
            // reference-parameter requirement and let a call site pass a value
            // where the callee dereferences.
            let param_infos = m
                .params
                .iter()
                .zip(params.iter())
                .map(|(param, ty)| ParamInfo {
                    ty: ty.clone(),
                    mode: param.mode.clone(),
                    sending: param.sending,
                    span: param.span,
                    type_span: param.type_span,
                })
                .collect();
            methods.insert(
                m.name.clone(),
                InterfaceMethodInfo {
                    effect_contracts: m.effect_contracts.clone(),
                    name: m.name.clone(),
                    params,
                    param_infos,
                    is_static: m.is_static,
                    return_type,
                    declaration_span: m.span,
                },
            );
        }
        if let Some(outer) = outer_params {
            self.local.declared_type_params = outer;
        }
        self.symbols.define_interface(
            registered_name.clone(),
            InterfaceInfo {
                name: registered_name,
                public: decl.public,
                methods,
                method_order: method_order.into(),
                type_params: decl.type_params.clone(),
                // Imported bounds were already qualified in register_module_impl.
                // In particular, compiler markers Send/Sync remain global.
                extends: decl.extends.clone(),
                declaration_span: decl.span,
                module_path: module_path.map(|s| s.to_string()),
            },
        );
    }
}
