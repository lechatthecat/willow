use super::*;

/// One unit's imports in the shape the back end wants them, from the module
/// resolver's own classification (the original change, the original change).
///
/// Only the resolver knows whether `import a::b;` named a module file or an
/// item of `a`, and the difference decides both halves: an item import leaves
/// `a` unnameable in this file, and binds a local name that another file may
/// bind to a different module's function.
pub(crate) fn backend_unit_imports(
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    dependencies: &ModuleDependencies,
) -> backend::cranelift::UnitImports {
    let by_path = dependencies.paths_for(program);
    let classified = module::resolver::classify_unit_imports_with(program, |path| {
        by_path.get(path).map(|&index| &modules[index])
    });
    let mut visible_modules = std::collections::HashSet::new();
    let mut module_spellings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Bind source spellings to canonical declaration identities for this unit.
    // Only the driver holds the imported program and can enumerate its types.
    let mut bind_spelling = |access: &str, dependency: &module::ResolvedModule| {
        let graph_name = dependency.registration_name();
        if access == graph_name {
            return;
        }
        module_spellings.push(backend::cranelift::ModuleSpelling {
            access: access.to_string(),
            graph_name: graph_name.to_string(),
            canonical_path: dependency.identity_path().to_string(),
            types: dependency
                .program
                .items
                .iter()
                .filter_map(|item| match item {
                    parser::ast::Item::Class(c) => Some(c.name.clone()),
                    parser::ast::Item::Enum(e) => Some(e.name.clone()),
                    parser::ast::Item::Interface(i) => Some(i.name.clone()),
                    parser::ast::Item::Function(_) => None,
                })
                .collect(),
        });
    };
    for binding in &classified.modules {
        if !seen.insert((binding.unit, binding.access.as_str())) {
            continue;
        }
        let dependency = &modules[dependencies
            .index_for_unit(binding.unit)
            .expect("resolved module")];
        // Source visibility is separate from the canonical registration name.
        visible_modules.insert(binding.access.clone());
        visible_modules.insert(dependency.registration_name().to_string());
        bind_spelling(&binding.access, dependency);
    }
    // An item import keeps its module prefix writable for qualified calls
    // (`import a::Op;` then `a::twice()`, the original change). In project mode the
    // module is registered under its package-qualified name, so that prefix
    // needs the same binding. It is NOT made visible: the
    // module's bare names stay unbound in this file.
    let item_modules: Vec<usize> = classified
        .items
        .iter()
        .map(|item| {
            dependencies
                .index_for_module(item.package, &item.canonical_module)
                .expect("resolved item module")
        })
        .collect();
    for (item, &index) in classified.items.iter().zip(&item_modules) {
        let dependency = &modules[index];
        if seen.insert((dependency.id, item.access.as_str())) {
            bind_spelling(&item.access, dependency);
        }
    }
    backend::cranelift::UnitImports {
        visible_modules,
        item_imports: classified
            .items
            .iter()
            .zip(item_modules)
            .map(|(item, index)| backend::cranelift::ItemBinding {
                local: item.local.clone(),
                module: modules[index].identity_path().to_string(),
                item: item.item.clone(),
            })
            .collect(),
        module_spellings,
    }
}

/// Report, under `WILLOW_LIR_LOG=1`, the functions HIR lowering could not build
/// at all.
///
/// A gap here is invisible from the walker's side: the function simply has no
/// entry in the LIR table, so the compile error can only say "has no lowered
/// IR" without saying WHY. These diagnostics carry the why. They are not
/// errors themselves — the error is raised where the body is wanted, by
/// `compile_function_named`.
fn log_hir_gaps(gaps: &[diagnostics::Diagnostic]) {
    if gaps.is_empty() || !*crate::diagnostic_settings::LIR_LOG {
        return;
    }
    for gap in gaps {
        eprintln!("[lir] hir gap: {}", gap.message);
    }
}

/// The interface defaults one unit's classes received from other units'
/// interfaces. Each is checked once, with its interface, in the
/// scope of the module that declares the interface; it is lowered with that
/// module's tables and emitted under that module's alias scope, so its private
/// helpers and types resolve as they do inside the interface. The receiving
/// class supplies only the receiver identity, recorded in a [`ForeignBatch`].
struct ForeignDefaults<'p> {
    /// Per declaring unit, in a deterministic order: each receiving class and
    /// the defaults it took from that unit.
    providers: std::collections::BTreeMap<
        module::UnitId,
        Vec<(&'p parser::ast::ClassDecl, Vec<&'p parser::ast::MethodDecl>)>,
    >,
    /// Each copied default body, to the unit whose scope it is emitted in.
    roots: std::collections::HashMap<parser::ast::BodyId, module::UnitId>,
}

impl<'p> ForeignDefaults<'p> {
    fn collect(
        index: &compiler_db::ids::BodyIndex,
        unit: module::UnitId,
        program: &'p parser::ast::Program,
    ) -> Self {
        let mut providers: std::collections::BTreeMap<_, Vec<_>> = Default::default();
        let mut roots = std::collections::HashMap::new();
        for item in &program.items {
            let parser::ast::Item::Class(class) = item else {
                continue;
            };
            let mut by_provider: std::collections::BTreeMap<_, Vec<_>> = Default::default();
            for method in &class.methods {
                if let Some(provider) = index.foreign_default(unit, method) {
                    roots.insert(method.body.id, provider);
                    by_provider.entry(provider).or_default().push(method);
                }
            }
            for (provider, methods) in by_provider {
                providers
                    .entry(provider)
                    .or_default()
                    .push((class, methods));
            }
        }
        Self { providers, roots }
    }

    /// Record these defaults in `batches`, by declaring unit, and remove them
    /// and the lambdas nested in them from this unit's emission `plan`.
    /// `program` and `tables` are this unit's, for the receivers' identities
    /// and class chains; `class_name` maps a class this unit declares to the
    /// name the back end compiles its other methods under.
    fn defer(
        &self,
        index: &compiler_db::ids::BodyIndex,
        plan: Option<&mut Vec<backend::cranelift::UnitBody<'_>>>,
        program: &parser::ast::Program,
        tables: &ir::lower::SemanticFacts<'_>,
        class_name: &dyn Fn(&parser::ast::ClassDecl) -> String,
        batches: &mut ForeignBatches,
    ) {
        use parser::ast::Type;
        if self.providers.is_empty() {
            return;
        }
        if let Some(plan) = plan {
            plan.retain(|target| {
                if let Some(method) = target.method() {
                    return !self.roots.contains_key(&method.body.id);
                }
                let Some((body, name)) = target.lambda_target() else {
                    return true;
                };
                let Some(&provider) = self.roots.get(&index.root_body(body)) else {
                    return true;
                };
                let batch = batches.entry(provider).or_default();
                batch.lambdas.insert(body, name.to_owned());
                false
            });
        }
        let resolution = ir::lower::lower_resolution(program, tables);
        // This unit's bare type identities (the entry file's), which a
        // declaring unit's own items could shadow. Qualified
        // module identities cannot collide.
        let normalized = |name: &str| match tables.normalize(&Type::Named(name.to_owned())) {
            Type::Named(ref name) => name.clone(),
            _ => name.to_owned(),
        };
        let bare: std::collections::HashSet<_> = program
            .items
            .iter()
            .filter_map(|item| match item {
                parser::ast::Item::Class(c) => Some(&c.name),
                parser::ast::Item::Enum(e) => Some(&e.name),
                parser::ast::Item::Interface(i) => Some(&i.name),
                _ => None,
            })
            .map(|name| normalized(name))
            .filter(|name| !QualifiedName::new(&name).is_qualified())
            .map(|name| semantic::ids::TypeId::from_source_name(&name))
            .collect();
        for (&provider, classes) in &self.providers {
            let batch = batches.entry(provider).or_default();
            // The bare identities the chains name, for [`ForeignBatch::respell`].
            let used = std::cell::RefCell::new(std::mem::take(&mut batch.bare));
            let record = |id: &semantic::ids::TypeId| {
                if bare.contains(id) {
                    used.borrow_mut().insert(*id);
                }
                *id
            };
            for (class, methods) in classes {
                let identity = semantic::ids::TypeId::from_source_name(&normalized(&class.name));
                record(&identity);
                // Stop at an ancestor an earlier receiver already recorded, so
                // shared chains are copied once.
                let mut current = Some(identity);
                while let Some(id) = current {
                    let Some(info) = resolution.classes.get(&id) else {
                        break;
                    };
                    let std::collections::hash_map::Entry::Vacant(entry) = batch.classes.entry(id)
                    else {
                        break;
                    };
                    entry.insert(ForeignBatch::scoped_class(info, &record));
                    current = info.base;
                }
                batch.receivers.push(ForeignReceiver {
                    key: identity.to_string(),
                    span: class.span,
                    name: class_name(class),
                    copies: methods.iter().map(|method| method.body.id).collect(),
                });
            }
            batch.bare = used.into_inner();
        }
    }
}

/// A class that copied defaults from one declaring unit, as its own unit
/// identifies it.
struct ForeignReceiver {
    /// The class identity in lowered IR.
    key: String,
    span: diagnostics::Span,
    /// The name the back end compiles the class's methods under.
    name: String,
    copies: Vec<parser::ast::BodyId>,
}

/// Every default copied from one declaring unit's interfaces, gathered from
/// all receiving units so the declaring unit's program and tables are loaded,
/// and its scope's signatures built, once for all of them.
#[derive(Default)]
struct ForeignBatch {
    receivers: Vec<ForeignReceiver>,
    /// The receivers' class chains, from their own units' resolutions, under
    /// the identities the declaring unit's scope spells them by.
    classes: std::collections::HashMap<semantic::ids::TypeId, ir::typed_ast::HirClassInfo>,
    /// The receiving units' bare type identities those chains mention; after
    /// [`Self::respell`], only those the declaring unit's own names shadow,
    /// which the chains and keys spell under
    /// [`semantic::ids::FOREIGN_RECEIVER_PREFIX`].
    bare: std::collections::BTreeSet<semantic::ids::TypeId>,
    /// The lambdas nested in the copies, under the names their receiving
    /// units declared them by.
    lambdas: std::collections::HashMap<parser::ast::BodyId, String>,
}

type ForeignBatches = std::collections::BTreeMap<module::UnitId, ForeignBatch>;

impl ForeignBatch {
    /// Respell the receivers' bare identities that `scope` declares or imports
    /// a name for, so the declaring unit's own item keeps that name in the
    /// copies, and keep only those in [`Self::bare`]. The
    /// chains name `scope`'s own interfaces by their identities under
    /// `identity`, the declaring unit's namespace, while `scope` spells them
    /// bare, so those are respelled bare: otherwise `self`
    /// does not convert to the interface it implements. Runs
    /// once per batch, over `scope`'s top-level items and the recorded chains.
    fn respell(&mut self, scope: &parser::ast::Program, identity: &str) {
        let interfaces: std::collections::HashSet<&str> = (scope.items.iter())
            .filter_map(|item| match item {
                parser::ast::Item::Interface(i) => Some(i.name.as_str()),
                _ => None,
            })
            .collect();
        if !self.bare.is_empty() {
            let names: std::collections::HashSet<&str> = (scope.items.iter())
                .map(|item| match item {
                    parser::ast::Item::Function(f) => f.name.as_str(),
                    parser::ast::Item::Class(c) => c.name.as_str(),
                    parser::ast::Item::Enum(e) => e.name.as_str(),
                    parser::ast::Item::Interface(i) => i.name.as_str(),
                })
                .chain(scope.imports.iter().map(|import| {
                    (import.alias.as_deref())
                        .unwrap_or_else(|| QualifiedName::new(&import.path).last_segment())
                }))
                .collect();
            self.bare
                .retain(|id| names.contains(id.to_string().as_str()));
        }
        if self.bare.is_empty() && interfaces.is_empty() {
            return;
        }
        let scoped = |id: &semantic::ids::TypeId| {
            let name = id.to_string();
            if self.bare.contains(id) {
                return semantic::ids::TypeId::from_source_name(
                    &semantic::ids::foreign_receiver_name(&name),
                );
            }
            match QualifiedName::new(&name).strip_qualifier(identity) {
                Some(local) if interfaces.contains(local) => {
                    semantic::ids::TypeId::from_source_name(local)
                }
                _ => *id,
            }
        };
        let classes = (self.classes.iter())
            .map(|(id, info)| (scoped(id), Self::scoped_class(info, &scoped)))
            .collect();
        for receiver in &mut self.receivers {
            receiver.key =
                scoped(&semantic::ids::TypeId::from_source_name(&receiver.key)).to_string();
        }
        self.classes = classes;
    }

    /// The entry classes' methods in the recorded chains, which the copies
    /// call with signatures that spell [`Self::bare`] identities bare; empty
    /// when nothing was respelled. Module classes are qualified
    /// and declared with qualified signatures.
    fn receiver_methods(&self) -> Vec<(semantic::ids::TypeId, &str)> {
        if self.bare.is_empty() {
            return Vec::new();
        }
        (self.classes.iter())
            .filter(|(id, _)| {
                id.foreign_receiver_target().is_some()
                    || !QualifiedName::new(&id.to_string()).is_qualified()
            })
            .flat_map(|(id, info)| info.methods.keys().map(|name| (*id, name.as_str())))
            .collect()
    }

    /// `info` with every type identity it names respelled by `scoped`.
    fn scoped_class(
        info: &ir::typed_ast::HirClassInfo,
        scoped: &dyn Fn(&semantic::ids::TypeId) -> semantic::ids::TypeId,
    ) -> ir::typed_ast::HirClassInfo {
        let ty = |ty: &semantic::ids::SemanticType| ty.map_names(scoped);
        let signature = |signature: &ir::typed_ast::HirSignature| ir::typed_ast::HirSignature {
            params: signature.params.iter().map(ty).collect(),
            return_type: ty(&signature.return_type),
            ..signature.clone()
        };
        ir::typed_ast::HirClassInfo {
            base: info.base.as_ref().map(scoped),
            implements: info.implements.iter().map(ty).collect(),
            fields: (info.fields.iter())
                .map(|(name, field)| (name.clone(), ty(field)))
                .collect(),
            static_fields: (info.static_fields.iter())
                .map(|(name, field)| (name.clone(), ty(field)))
                .collect(),
            constructor: info.constructor.as_ref().map(signature),
            methods: (info.methods.iter())
                .map(|(name, method)| (name.clone(), signature(method)))
                .collect(),
        }
    }

    /// The method each copy instantiates, keyed by the interface default's own
    /// body: built once from `scope`'s interface declaration, which the copies
    /// of a non-generic default share but for their body identity.
    fn defaults(
        &self,
        index: &compiler_db::ids::BodyIndex,
        scope: &parser::ast::Program,
    ) -> std::collections::HashMap<parser::ast::BodyId, parser::ast::MethodDecl> {
        let mut wanted: std::collections::HashSet<_> = self
            .receivers
            .iter()
            .flat_map(|receiver| &receiver.copies)
            .map(|&copy| index.source_body(copy))
            .collect();
        let mut defaults = std::collections::HashMap::with_capacity(wanted.len());
        for item in &scope.items {
            let parser::ast::Item::Interface(interface) = item else {
                continue;
            };
            for method in &interface.methods {
                let Some(body) = method
                    .default_body
                    .as_ref()
                    .filter(|body| wanted.remove(&body.id))
                else {
                    continue;
                };
                defaults.insert(
                    body.id,
                    parser::ast::MethodDecl {
                        effect_contracts: method.effect_contracts.clone(),
                        array_body_optimization: crate::parser::ast::ArrayBodyOptimization::None,
                        constraint: None,
                        name: method.name.clone(),
                        public: true,
                        protected: false,
                        is_async: false,
                        is_open: false,
                        is_override: false,
                        is_static: false,
                        params: method.params.clone(),
                        return_type: method.return_type.clone(),
                        body: body.clone(),
                        span: method.span,
                        is_default_injected: true,
                        is_interface_default: true,
                    },
                );
            }
        }
        defaults
    }

    /// Each receiver with its copies and the declarations they instantiate.
    fn classes<'b>(
        &'b self,
        index: &compiler_db::ids::BodyIndex,
        defaults: &'b std::collections::HashMap<parser::ast::BodyId, parser::ast::MethodDecl>,
    ) -> Result<Vec<ir::lower::ForeignClass<'b>>> {
        self.receivers
            .iter()
            .map(|receiver| {
                Ok(ir::lower::ForeignClass {
                    key: &receiver.key,
                    span: receiver.span,
                    methods: receiver
                        .copies
                        .iter()
                        .map(|&copy| {
                            let source = index.source_body(copy);
                            let method = defaults.get(&source).with_context(|| {
                                format!("copied default {copy:?} has no declaration {source:?}")
                            })?;
                            Ok((copy, method))
                        })
                        .collect::<Result<_>>()?,
                })
            })
            .collect()
    }

    /// Lower every copy in this batch with the declaring unit's `scope`.
    fn lower(
        &self,
        db: &compiler_db::CompilerDb,
        scope: &parser::ast::Program,
        scope_tables: &ir::lower::SemanticFacts<'_>,
        defaults: &std::collections::HashMap<parser::ast::BodyId, parser::ast::MethodDecl>,
    ) -> Result<compiler_db::lir::ForeignLowering> {
        let receivers = self.classes(db.bodies(), defaults)?;
        db.lir
            .lower_foreign(scope, scope_tables, &receivers, &self.classes, db.bodies())
    }

    /// Emit every copy and its lambdas; the caller installs the declaring
    /// unit's alias scope. `current` names the receiver being emitted, for
    /// failure reports.
    fn emit<'b>(
        &'b self,
        index: &compiler_db::ids::BodyIndex,
        defaults: &std::collections::HashMap<parser::ast::BodyId, parser::ast::MethodDecl>,
        backend: &mut backend::cranelift::UnitCodegenContext<'_>,
        current: &mut &'b str,
    ) -> Result<()> {
        for (receiver, class) in self.receivers.iter().zip(self.classes(index, defaults)?) {
            *current = &receiver.name;
            // A respelled receiver is compiled under its key: its bare name
            // could mean the declaring unit's own type here, `self` included
            //. The symbols are the same either way.
            let name = match QualifiedName::new(&receiver.key)
                .has_qualifier(semantic::ids::FOREIGN_RECEIVER_NAMESPACE)
            {
                true => &receiver.key,
                false => &receiver.name,
            };
            for (copy, method) in class.methods {
                for (body, lambda) in compiler_db::lir::copied_lambdas(index, copy, &method.body)? {
                    let declared = self.lambdas.get(&body).with_context(|| {
                        format!("lambda {body:?} in copied default {copy:?} was not declared")
                    })?;
                    backend.compile_body(&backend::cranelift::UnitBody::lambda(
                        body, declared, lambda,
                    ))?;
                }
                backend.compile_foreign_method(name, method, copy)?;
            }
        }
        Ok(())
    }
}

/// Back-end phases: drive Cranelift codegen over the modules and entry program,
/// emit the object file, resolve the runtime library, link the native
/// executable, and write debug/source-map artifacts.
pub(super) fn run_backend(
    frontend: Frontend,
    (src, source): (&str, String),
    out: &str,
    opts: &CompilerOptions,
    map: &diagnostics::SourceMap,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
    rust_bridge: Option<(
        &toolchain::rust_bridge_build::BridgeBuild,
        &crate::rust_bridge_ir::RustBridgeSymbols,
    )>,
) -> Result<()> {
    // The entry file's item imports are not carried here: the back end gets
    // them from `backend_unit_imports` below, the same way every module's are.
    let _symbols = frontend.db.enter_symbols();
    let native_shared = frontend
        .db
        .package_graph()
        .and_then(|graph| graph.native.as_ref())
        .map(|plan| plan.shared.clone())
        .unwrap_or_default();
    let native_args = frontend
        .db
        .package_graph()
        .and_then(|graph| graph.native.as_ref())
        .map(|plan| plan.args.clone())
        .unwrap_or_default();
    let Frontend {
        program,
        mut module_graph,
        mut db,
    } = frontend;
    let module_init_plan = ir::module_init::ModuleInitPlan::from_graph(&module_graph);
    let artifacts = module_graph.artifacts.take().expect("spooled frontend");
    let diagnostic_packages = DiagnosticModuleIndex::package_names(&module_graph);
    let modules = module_graph.files;
    // Debug metadata is read from the same hydrated trees the declare pass
    // uses, so a debug build hydrates each unit no more often than a release
    // build. Entry first, then modules in graph order.
    let emit_debug_metadata = opts.target.emit_debug_info || opts.target.emit_source_map;
    let mut codegen =
        backend::Codegen::new(opts, std::rc::Rc::clone(&db.layouts)).map_err(|error| {
            emit_codegen_error(
                errors::CodegenError::new(errors::CodegenStage::Initialize, error),
                map,
                emitter,
                &diagnostic_packages,
            )
        })?;
    if let Some((_, symbols)) = rust_bridge {
        codegen.set_rust_bridge_symbols(symbols);
    }
    codegen.configure_queries(
        std::rc::Rc::clone(&db.typed_bodies),
        std::rc::Rc::clone(&db.lir),
        db.effects.static_initializers.order(),
    );
    codegen.set_runtime_names(std::mem::take(&mut db.runtime_names));
    codegen.set_module_init_plan(module_init_plan);
    let DeclaredUnits {
        declared_modules,
        entry_artifact,
        debug_metadata,
    } = declare_units(
        &mut codegen,
        &db,
        &artifacts,
        &modules,
        &program,
        &mut CodegenDiagnostics {
            src,
            map,
            emitter,
            packages: &diagnostic_packages,
        },
        emit_debug_metadata,
    )?;
    emit_bodies(
        &mut codegen,
        &db,
        &artifacts,
        &modules,
        declared_modules,
        entry_artifact,
        &mut CodegenDiagnostics {
            src,
            map,
            emitter,
            packages: &diagnostic_packages,
        },
    )?;
    emit_frame_warnings(
        &mut codegen,
        &artifacts,
        &modules,
        &source,
        &mut CodegenDiagnostics {
            src,
            map,
            emitter,
            packages: &diagnostic_packages,
        },
    )?;

    if opts.target.emit_debug_info {
        codegen
            .embed_runtime_metadata(debug_metadata.as_deref().unwrap_or(""))
            .map_err(|error| {
                emit_codegen_error(
                    errors::CodegenError::new(errors::CodegenStage::Metadata, error),
                    map,
                    emitter,
                    &diagnostic_packages,
                )
            })?;
    }

    let obj_bytes = codegen.finish().map_err(|error| {
        emit_codegen_error(
            errors::CodegenError::new(errors::CodegenStage::Finish, error),
            map,
            emitter,
            &diagnostic_packages,
        )
    })?;

    link_output(
        &obj_bytes,
        debug_metadata.as_deref(),
        opts,
        NativeOutput {
            out,
            rust_bridge: rust_bridge.map(|(bridge, _)| bridge),
            shared: &native_shared,
            args: &native_args,
        },
        map,
        emitter,
    )
}

fn emit_codegen_error(
    mut error: errors::CodegenError,
    map: &diagnostics::SourceMap,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
    packages: &std::collections::HashMap<String, String>,
) -> anyhow::Error {
    error.message = diagnostics::diagnostic::source_names(error.message, packages);
    if let errors::CodegenStage::Module(name) = &mut error.stage {
        *name = diagnostics::diagnostic::source_names(std::mem::take(name), packages);
    }
    if let Err(error) = emitter.emit(&error.diagnostic(), map) {
        return error.into();
    }
    anyhow::Error::new(error)
}

#[cfg(test)]
#[test]
fn buildgraph_codegen_failure_projects_emitted_and_returned_names() {
    struct Capture(Vec<diagnostics::Diagnostic>);
    impl diagnostics::DiagnosticEmitter for Capture {
        fn emit(
            &mut self,
            d: &diagnostics::Diagnostic,
            _: &dyn diagnostics::source_map::SourceLookup,
        ) -> std::io::Result<()> {
            self.0.push(d.clone());
            Ok(())
        }
    }
    let package = "$pkg0123456789abcdef0123456789abcdef";
    let packages = [(package.into(), "dep::".into())].into();
    let mut emitter = Capture(Vec::new());
    let map = diagnostics::SourceMap::new("main.wi", "");
    let error = emit_codegen_error(
        errors::CodegenError::new(
            errors::CodegenStage::Module(format!("{package}::graph")),
            format!("function `{package}.graph.empty` has invalid lowered IR"),
        ),
        &map,
        &mut emitter,
        &packages,
    );
    assert!(!error.to_string().contains("$pkg"), "{error}");
    assert!(error.to_string().contains("dep::graph.empty"), "{error}");
    assert!(!emitter.0[0].message.contains("$pkg"));
    assert!(emitter.0[0].message.contains("dep::graph"));
}

/// Render a codegen failure, preferring the symbol conflicts the backend
/// recorded over the generic internal-error message (the original change, item 8).
///
/// A symbol conflict is a user error with a source location, so it gets a real
/// diagnostic instead of `internal compiler error`. The backend stops at the
/// first one, because continuing would leave its function table pointing at the
/// wrong function and abort inside Cranelift before this could be printed.
fn report_backend_failure(
    codegen: &mut backend::Codegen,
    fallback: errors::CodegenError,
    map: &diagnostics::SourceMap,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
    artifacts: &UnitArtifacts,
    packages: &std::collections::HashMap<String, String>,
) -> anyhow::Error {
    let conflicts = codegen.take_symbol_conflicts();
    if conflicts.is_empty() {
        return emit_codegen_error(fallback, map, emitter, packages);
    }
    let mut sources = diagnostics::SourceMaps::default();
    for conflict in &conflicts {
        let owner = &conflict.owner;
        if sources.get(owner.span.file_id).is_none() {
            let source = match artifacts.source(owner.span.file_id) {
                Ok(source) => source,
                Err(error) => return error,
            };
            sources.insert(diagnostics::SourceMap::with_file_id(
                owner.span.file_id,
                &owner.source_file,
                source,
            ));
        }
        if let Err(error) = emit_symbol_conflict(conflict, &sources, emitter) {
            return error.into();
        }
    }
    anyhow::anyhow!("aborting due to {} error(s)", conflicts.len())
}

fn emit_symbol_conflict(
    conflict: &backend::SymbolConflict,
    sources: &diagnostics::SourceMaps,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> std::io::Result<()> {
    use diagnostics::{Diagnostic, ErrorCode, Label, Severity};

    let symbol = &conflict.symbol;
    let owner = &conflict.owner;
    let diagnostic = match &conflict.kind {
        backend::SymbolConflictKind::Reserved => Diagnostic::new(
            Severity::Error,
            ErrorCode::E0705,
            format!("{} would define the reserved symbol `{symbol}`", owner.item),
        )
        .with_label(Label::primary(owner.span, "reserved name"))
        .with_help(
            "`willow_*` belongs to the Willow runtime and `__willow_*` to the compiler; \
             defining one replaces the runtime's version for the whole program",
        ),
        backend::SymbolConflictKind::Duplicate { previous } => Diagnostic::new(
            Severity::Error,
            ErrorCode::E0706,
            format!(
                "{} and {} both map to the linker symbol `{symbol}`",
                previous.item, owner.item
            ),
        )
        .with_label(Label::primary(owner.span, "second declaration"))
        .with_help(format!(
            "two declarations cannot share one linker symbol; rename one of them \
             (the first is {} in {})",
            previous.item, previous.source_file
        )),
    };

    emitter.emit(&diagnostic, sources)
}
/// Lower a source file to typed HIR and render it as text (the `--emit-hir`
/// build flag). Runs the normal front-end (lex → parse → import → desugar →
/// type-check) so the HIR reflects the checked, desugared program; lowering
/// covers the constructs implemented so far and lists the rest as
/// trailing comments rather than failing.
pub fn emit_hir_text(src: &str) -> Result<String> {
    let symbols = semantic::ids::SymbolInterner::new();
    let _symbols = symbols.enter();
    let _query_stats = query_stats::Session::enter();
    let _node_ids = parser::ast::NodeIdSession::enter();
    let src_path = PathBuf::from(src);
    let source = std::fs::read_to_string(&src_path)
        .with_context(|| format!("cannot read {}", src_path.display()))?;
    let root = src_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let map = diagnostics::SourceMap::new(src, &source);
    let options = CompilerOptions::debug().resolve_environment();
    let frontend = run_frontend(&source, &root, &map, &options)?;

    let body = frontend
        .module_graph
        .artifacts
        .as_ref()
        .expect("spooled frontend")
        .hydrate(&frontend.program, diagnostics::FileId::ENTRY)?;
    let checked = frontend.db.checked_unit(
        module::UnitId::ENTRY,
        frontend.module_graph.artifacts.as_ref().unwrap(),
    )?;
    let tables = checked.tables();
    let bodies = frontend.db.bodies();
    let (mut hir, mut lowering_diagnostics) =
        ir::lower::lower_program_except(&body, &tables, &|m| {
            bodies.foreign_default(module::UnitId::ENTRY, m).is_some()
        });
    // Defaults copied from imported interfaces are lowered in the scope of the
    // module that declares each interface, then shown with the
    // receiving class's own methods.
    let mut foreign = ForeignBatches::new();
    ForeignDefaults::collect(bodies, module::UnitId::ENTRY, &body).defer(
        bodies,
        None,
        &body,
        &tables,
        &|class| class.name.clone(),
        &mut foreign,
    );
    if !foreign.is_empty() {
        let artifacts = frontend.module_graph.artifacts.as_ref().unwrap();
        let files: std::collections::HashMap<_, _> = frontend
            .module_graph
            .files
            .iter()
            .map(|module| (module.id, (&module.program, module.identity_path())))
            .collect();
        let mut receivers: std::collections::HashMap<_, _> = hir
            .classes
            .iter()
            .enumerate()
            .map(|(at, class)| (class.name, at))
            .collect();
        for (&provider, batch) in &mut foreign {
            let &(program, identity) = files
                .get(&provider)
                .with_context(|| format!("interface unit {provider:?} is not in the graph"))?;
            let scope = artifacts.hydrate(program, provider.file_id())?;
            let checked = frontend.db.checked_unit(provider, artifacts)?;
            batch.respell(&scope, identity);
            let defaults = batch.defaults(bodies, &scope);
            let classes = batch.classes(bodies, &defaults)?;
            let (lowered, gaps) = ir::lower::lower_foreign_methods(
                &scope,
                &checked.tables(),
                &classes,
                &batch.classes,
            );
            lowering_diagnostics.extend(gaps);
            for mut class in lowered.classes {
                // A respelled receiver is shown with its class.
                if let Some(bare) = QualifiedName::new(&class.name.to_string())
                    .strip_qualifier(semantic::ids::FOREIGN_RECEIVER_NAMESPACE)
                {
                    class.name = semantic::ids::TypeId::from_source_name(bare);
                }
                match receivers.get(&class.name) {
                    Some(&at) => hir.classes[at].methods.extend(class.methods),
                    None => {
                        receivers.insert(class.name, hir.classes.len());
                        hir.classes.push(class);
                    }
                }
            }
        }
    }
    let mut text = ir::dump::format_program(&hir);
    if !lowering_diagnostics.is_empty() {
        text.push_str("\n// constructs not yet lowered to HIR:\n");
        for diagnostic in &lowering_diagnostics {
            text.push_str(&format!("//   {}\n", diagnostic.message));
        }
    }
    Ok(text)
}

/// Lower a source file to the basic-block LIR and render it as text (the
/// `--emit-lir` build flag). Runs the normal front-end, lowers to typed HIR,
/// then makes control flow explicit as blocks.
pub fn emit_lir_text(src: &str) -> Result<String> {
    let symbols = semantic::ids::SymbolInterner::new();
    let _symbols = symbols.enter();
    let _query_stats = query_stats::Session::enter();
    let _node_ids = parser::ast::NodeIdSession::enter();
    let src_path = PathBuf::from(src);
    let source = std::fs::read_to_string(&src_path)
        .with_context(|| format!("cannot read {}", src_path.display()))?;
    let root = src_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let map = diagnostics::SourceMap::new(src, &source);
    let options = CompilerOptions::debug().resolve_environment();
    let frontend = run_frontend(&source, &root, &map, &options)?;

    let body = frontend
        .module_graph
        .artifacts
        .as_ref()
        .expect("spooled frontend")
        .hydrate(&frontend.program, diagnostics::FileId::ENTRY)?;
    let checked = frontend.db.checked_unit(
        module::UnitId::ENTRY,
        frontend.module_graph.artifacts.as_ref().unwrap(),
    )?;
    let tables = checked.tables();
    // The source dump historically retains import spellings. Its fresh db
    // session is separate from native emission, which uses the declared tree.
    let gaps =
        frontend
            .db
            .lir
            .lower_unit(module::UnitId::ENTRY, &body, frontend.db.bodies(), &tables)?;
    let mut lir = frontend.db.lir.unit_program(module::UnitId::ENTRY)?;
    // Defaults copied from imported interfaces are lowered in the scope of the
    // module that declares each interface.
    let mut foreign = ForeignBatches::new();
    ForeignDefaults::collect(frontend.db.bodies(), module::UnitId::ENTRY, &body).defer(
        frontend.db.bodies(),
        None,
        &body,
        &tables,
        &|class| class.name.clone(),
        &mut foreign,
    );
    let mut gaps = gaps.to_vec();
    if !foreign.is_empty() {
        let artifacts = frontend.module_graph.artifacts.as_ref().unwrap();
        let files: std::collections::HashMap<_, _> = frontend
            .module_graph
            .files
            .iter()
            .map(|module| (module.id, (&module.program, module.identity_path())))
            .collect();
        for (&provider, batch) in &mut foreign {
            let &(program, identity) = files
                .get(&provider)
                .with_context(|| format!("interface unit {provider:?} is not in the graph"))?;
            let scope = artifacts.hydrate(program, provider.file_id())?;
            let checked = frontend.db.checked_unit(provider, artifacts)?;
            batch.respell(&scope, identity);
            let defaults = batch.defaults(frontend.db.bodies(), &scope);
            let (lowered_gaps, lifted) =
                batch.lower(&frontend.db, &scope, &checked.tables(), &defaults)?;
            gaps.extend(lowered_gaps);
            for receiver in &batch.receivers {
                for &copy in &receiver.copies {
                    lir.functions.push(frontend.db.lir.body(copy)?);
                }
            }
            for (id, span, body) in lifted {
                lir.lambdas.push(ir::lowered::LirLambda {
                    id,
                    span,
                    function: frontend.db.lir.body(body)?,
                });
            }
        }
    }
    let mut text = ir::lowered::format_program(&lir);
    if !gaps.is_empty() {
        text.push_str("\n// constructs not yet lowered to HIR:\n");
        for diagnostic in gaps.iter() {
            text.push_str(&format!("//   {}\n", diagnostic.message));
        }
    }
    Ok(text)
}
struct CodegenDiagnostics<'a> {
    src: &'a str,
    map: &'a diagnostics::SourceMap,
    emitter: &'a mut dyn diagnostics::DiagnosticEmitter,
    packages: &'a std::collections::HashMap<String, String>,
}

struct DeclaredUnits {
    declared_modules: Vec<usize>,
    entry_artifact: usize,
    debug_metadata: Option<String>,
}
fn declare_units(
    codegen: &mut backend::Codegen,
    db: &compiler_db::CompilerDb,
    artifacts: &UnitArtifacts,
    modules: &[module::ResolvedModule],
    program: &parser::ast::Program,
    context: &mut CodegenDiagnostics<'_>,
    emit_debug_metadata: bool,
) -> Result<DeclaredUnits> {
    let src = context.src;
    let map = context.map;
    let emitter = &mut *context.emitter;
    let diagnostic_packages = context.packages;
    let mut module_debug_metadata = Vec::new();
    // Shared declaration metadata comes from the checked entry artifact.
    {
        let checked = db.unit_declarations(module::UnitId::ENTRY, artifacts)?;
        let tables = checked.tables();
        let declared = tables.declared_enum_identities(program);
        for info in checked.symbols.enums.values() {
            codegen.register_enum_info(tables.codegen_enum_info(info, &declared));
        }
        // An identity registers once (first wins). A `$type$mono$` alias key
        // can carry another unit's type spelled like one declared here (the
        // entry's `Get` beside `util::Get`), so declarations under their own
        // key register before alias entries; hash order used to pick.
        let (own, aliased): (Vec<_>, Vec<_>) = (checked.symbols.interfaces.iter())
            .map(|(key, info)| {
                (
                    key,
                    semantic::ids::TypeId::from_source_name(&info.name),
                    info,
                )
            })
            .partition(|(key, identity, _)| *key == identity);
        for (_, identity, info) in own.into_iter().chain(aliased) {
            codegen.register_interface_info(identity, || info.to_semantic())?;
        }
    }
    // Declare every unit before emitting any body: later overrides must be
    // visible to devirtualization in earlier modules. Each declaration artifact
    // is written and dropped immediately, preserving its lambda symbols/IDs.
    let mut declared_modules = Vec::with_capacity(modules.len());
    for module in modules {
        let body = artifacts.hydrate(&module.program, module.id.file_id())?;
        if emit_debug_metadata {
            let source = artifacts.source(module.id.file_id())?;
            let source_map =
                diagnostics::SourceMap::new(module.path.to_string_lossy().into_owned(), source);
            module_debug_metadata.push(
                diagnostics::DebugSourceMap::from_program(
                    &source_map.path,
                    source_map.total_lines(),
                    &body,
                )
                .to_text(),
            );
        }
        let checker = db.checked_unit(module.id, artifacts)?;
        let tables = checker.tables();
        let declared = tables.declared_enum_identities(&module.program);
        for info in checker.symbols.enums.values() {
            let canonical = tables.codegen_enum_info(info, &declared);
            if declared.contains(&info.name) {
                codegen.register_enum_info(canonical);
            } else {
                codegen.register_referenced_enum_info(canonical);
            }
        }
        let scope = db.unit_scope(module.id, &body, modules, &checker.symbols)?;
        let declared = codegen.declare_module(
            module.registration_name(),
            module.identity_path(),
            &body,
            &module.path.to_string_lossy(),
            backend::cranelift::CheckedUnitInput {
                checked: &checker,
                scope,
                effects: std::rc::Rc::clone(&db.effects),
                unit: module.id,
            },
        );
        let unit = declared.map_err(|error| {
            report_backend_failure(
                codegen,
                errors::CodegenError::new(errors::CodegenStage::Module(module.name.clone()), error),
                map,
                emitter,
                artifacts,
                diagnostic_packages,
            )
        })?;
        let unit = artifacts.track(UnitKind::Declared, unit);
        declared_modules.push(artifacts.write(&*unit)?);
    }
    let mut debug_metadata = None;
    let entry_artifact = {
        let body = artifacts.hydrate(program, diagnostics::FileId::ENTRY)?;
        if emit_debug_metadata {
            let mut text = diagnostics::DebugSourceMap::from_program(
                map.shown_path(),
                map.total_lines(),
                &body,
            )
            .to_text();
            for module in module_debug_metadata.drain(..) {
                text.push_str("\n---\n");
                text.push_str(&module);
            }
            debug_metadata = Some(text);
        }
        let checked = db.checked_unit(module::UnitId::ENTRY, artifacts)?;
        let scope = db.unit_scope(module::UnitId::ENTRY, &body, modules, &checked.symbols)?;
        let declared = codegen.declare_program(
            &body,
            src,
            backend::cranelift::CheckedUnitInput {
                checked: &checked,
                scope,
                effects: std::rc::Rc::clone(&db.effects),
                unit: module::UnitId::ENTRY,
            },
        );
        let unit = declared.map_err(|error| {
            report_backend_failure(
                codegen,
                errors::CodegenError::new(errors::CodegenStage::Entry, error),
                map,
                emitter,
                artifacts,
                diagnostic_packages,
            )
        })?;
        let unit = artifacts.track(UnitKind::Declared, unit);
        artifacts.write(&*unit)?
    };
    Ok(DeclaredUnits {
        declared_modules,
        entry_artifact,
        debug_metadata,
    })
}
fn emit_module_bodies(
    codegen: &mut backend::Codegen,
    db: &compiler_db::CompilerDb,
    artifacts: &UnitArtifacts,
    modules: &[module::ResolvedModule],
    declared_modules: Vec<usize>,
    foreign: &mut ForeignBatches,
    context: &mut CodegenDiagnostics<'_>,
) -> Result<()> {
    let map = context.map;
    let emitter = &mut *context.emitter;
    let diagnostic_packages = context.packages;
    for (module, artifact) in modules.iter().zip(declared_modules) {
        let unit: backend::cranelift::DeclaredModule = artifacts.read(artifact)?;
        let unit = artifacts.track(UnitKind::Declared, unit);
        let _lir = artifacts.live(UnitKind::Lir);
        let checker = db.checked_unit(module.id, artifacts)?;
        {
            // ANF declarations may hoist a lambda before an await and create
            // fresh temporary IDs. Preserve source resolutions/captures, but
            // lower the exact declared tree with its extended payload types.
            let mut tables = checker.tables();
            tables.expr_types = unit.normalized_expr_types();
            log_hir_gaps(&db.lir.lower_unit(
                module.id,
                unit.normalized_program(),
                db.bodies(),
                &tables,
            )?);
        }
        // Emission is addressed one body at a time: each target names the
        // semantic body whose lowered IR the artifact store holds, so no
        // whole-unit IR is materialized here.
        let mut plan = codegen.module_body_plan(&unit);
        {
            let mut tables = checker.tables();
            tables.expr_types = unit.normalized_expr_types();
            let defaults =
                ForeignDefaults::collect(db.bodies(), module.id, unit.normalized_program());
            let names = if defaults.providers.is_empty() {
                Default::default()
            } else {
                unit.module_class_names()
            };
            defaults.defer(
                db.bodies(),
                Some(&mut plan),
                unit.normalized_program(),
                &tables,
                &|class| {
                    names
                        .get(class.name.as_str())
                        .map_or_else(|| class.name.clone(), |&name| name.to_owned())
                },
                foreign,
            );
        }
        let compiled = codegen.with_module_bodies(&unit, &checker, |backend| {
            for target in &plan {
                backend.compile_body(target)?;
            }
            Ok(())
        });
        compiled.map_err(|error| {
            report_backend_failure(
                codegen,
                errors::CodegenError::new(errors::CodegenStage::Module(module.name.clone()), error),
                map,
                emitter,
                artifacts,
                diagnostic_packages,
            )
        })?;
    }
    Ok(())
}
fn emit_frame_warnings(
    codegen: &mut backend::Codegen,
    artifacts: &UnitArtifacts,
    modules: &[module::ResolvedModule],
    source: &str,
    context: &mut CodegenDiagnostics<'_>,
) -> Result<()> {
    use diagnostics::{Diagnostic, ErrorCode, Severity};
    let src = context.src;
    let emitter = &mut *context.emitter;
    let warnings = codegen.take_async_frame_size_warnings();
    // Index once and load each warned file once, even with many large frames.
    let module_paths: std::collections::HashMap<_, _> = if warnings.is_empty() {
        Default::default()
    } else {
        modules
            .iter()
            .map(|module| (module.path.to_string_lossy(), module.id))
            .collect()
    };
    let mut warning_maps = std::collections::HashMap::new();
    for warning in &warnings {
        let warning_map = match warning_maps.entry(warning.source_file.as_str()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let warning_source = if warning.source_file == src {
                    source.to_owned()
                } else {
                    module_paths
                        .get(warning.source_file.as_str())
                        .map(|id| artifacts.source(id.file_id()))
                        .transpose()?
                        .unwrap_or_default()
                };
                entry.insert(diagnostics::SourceMap::with_file_id(
                    warning.span.file_id,
                    &warning.source_file,
                    warning_source,
                ))
            }
        };
        let point_span = diagnostics::Span::in_file(
            warning.span.file_id,
            warning.span.start,
            warning.span.start.saturating_add(1),
            warning.span.line,
            warning.span.col,
        );
        let diagnostic = Diagnostic::new(
            Severity::Warning,
            ErrorCode::W0801,
            format!(
                "async frame for `{}` is large: {} bytes",
                warning.function_name, warning.size_bytes
            ),
        )
        .with_label(diagnostics::Label::primary(
            point_span,
            "large async frame allocated here",
        ))
        .with_help("avoid keeping large arrays or objects live across await points");
        emitter.emit(&diagnostic, warning_map)?;
    }

    Ok(())
}

fn emit_bodies(
    codegen: &mut backend::Codegen,
    db: &compiler_db::CompilerDb,
    artifacts: &UnitArtifacts,
    modules: &[module::ResolvedModule],
    declared_modules: Vec<usize>,
    entry_artifact: usize,
    context: &mut CodegenDiagnostics<'_>,
) -> Result<()> {
    let src = context.src;
    let map = context.map;
    let emitter = &mut *context.emitter;
    let diagnostic_packages = context.packages;
    let declared_units: std::collections::HashMap<_, _> = modules
        .iter()
        .zip(declared_modules.iter().copied())
        .map(|(module, artifact)| {
            let names = (module.name.as_str(), module.identity_path());
            (module.id, (artifact, names))
        })
        .collect();
    let mut foreign = ForeignBatches::new();
    emit_module_bodies(
        codegen,
        db,
        artifacts,
        modules,
        declared_modules,
        &mut foreign,
        &mut CodegenDiagnostics {
            src,
            map,
            emitter,
            packages: diagnostic_packages,
        },
    )?;
    let entry_unit: backend::cranelift::DeclaredProgram = artifacts.read(entry_artifact)?;
    let entry_unit = artifacts.track(UnitKind::Declared, entry_unit);
    let _entry_lir = artifacts.live(UnitKind::Lir);
    let checked = db.checked_unit(module::UnitId::ENTRY, artifacts)?;
    {
        let mut tables = checked.tables();
        tables.expr_types = entry_unit.normalized_expr_types();
        log_hir_gaps(&db.lir.lower_unit(
            module::UnitId::ENTRY,
            entry_unit.normalized_program(),
            db.bodies(),
            &tables,
        )?);
    }
    let mut plan = codegen.program_body_plan(&entry_unit);
    {
        let mut tables = checked.tables();
        tables.expr_types = entry_unit.normalized_expr_types();
        ForeignDefaults::collect(
            db.bodies(),
            module::UnitId::ENTRY,
            entry_unit.normalized_program(),
        )
        .defer(
            db.bodies(),
            Some(&mut plan),
            entry_unit.normalized_program(),
            &tables,
            &|class| class.name.clone(),
            &mut foreign,
        );
    }
    let compiled = codegen.with_program_bodies(&entry_unit, &checked, |backend| {
        for target in &plan {
            backend.compile_body(target)?;
        }
        Ok(())
    });
    compiled.map_err(|error| {
        report_backend_failure(
            codegen,
            errors::CodegenError::new(errors::CodegenStage::Entry, error),
            map,
            emitter,
            artifacts,
            diagnostic_packages,
        )
    })?;
    drop(checked);
    drop(entry_unit);
    // Defaults copied from other units' interfaces, once per declaring unit:
    // its program, tables and scope signatures serve every receiver
    //.
    for (provider, batch) in &mut foreign {
        query_stats::query_call("foreign_default_unit");
        let &(artifact, (name, identity)) = declared_units
            .get(provider)
            .with_context(|| format!("interface unit {provider:?} was not declared"))?;
        let scope: backend::cranelift::DeclaredModule = artifacts.read(artifact)?;
        let scope = artifacts.track(UnitKind::Declared, scope);
        let _lir = artifacts.live(UnitKind::Lir);
        let checked = db.checked_unit(*provider, artifacts)?;
        batch.respell(scope.normalized_program(), identity);
        let batch = &*batch;
        let defaults = batch.defaults(db.bodies(), scope.normalized_program());
        {
            let mut tables = checked.tables();
            tables.expr_types = scope.normalized_expr_types();
            let (gaps, _) = batch.lower(db, scope.normalized_program(), &tables, &defaults)?;
            log_hir_gaps(&gaps);
        }
        let mut receiver = "";
        let methods = batch.receiver_methods();
        let compiled =
            codegen.with_foreign_bodies(&scope, &checked, &batch.bare, &methods, |backend| {
                batch.emit(db.bodies(), &defaults, backend, &mut receiver)
            });
        compiled.map_err(|error| {
            let stage = errors::CodegenStage::ForeignDefault {
                provider: name.to_owned(),
                receiver: receiver.to_owned(),
            };
            report_backend_failure(
                codegen,
                errors::CodegenError::new(stage, error),
                map,
                emitter,
                artifacts,
                diagnostic_packages,
            )
        })?;
    }

    Ok(())
}

struct NativeOutput<'a> {
    out: &'a str,
    rust_bridge: Option<&'a toolchain::rust_bridge_build::BridgeBuild>,
    shared: &'a [toolchain::native::distribution::SharedArtifact],
    args: &'a [String],
}

fn link_output(
    obj_bytes: &[u8],
    debug_metadata: Option<&str>,
    opts: &CompilerOptions,
    output: NativeOutput<'_>,
    map: &diagnostics::SourceMap,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> Result<()> {
    use diagnostics::{Diagnostic, ErrorCode, Severity};
    use toolchain::{HostToolchain, Toolchain};
    let NativeOutput {
        out,
        rust_bridge,
        shared,
        args,
    } = output;
    let toolchain = HostToolchain::new(&opts.target);
    let obj_path = toolchain.write_object(out, obj_bytes)?;
    // The object is an intermediate file, deleted as soon as it has been
    // linked. `WILLOW_KEEP_OBJECT=1` keeps it, which is how a test can assert
    // on what the backend actually emitted — the imported-symbol list of the
    // object is the only place a runtime call is visible, since the linked
    // binary also contains everything the runtime staticlib defines.
    let keep_object = *crate::diagnostic_settings::KEEP_OBJECT;
    let discard_object = |path: &Path| {
        if !keep_object {
            let _ = std::fs::remove_file(path);
        }
    };

    let runtime_lib = toolchain.resolve_runtime_library().map_err(|err| {
        discard_object(&obj_path);
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0700,
            format!("runtime library unavailable: {err}"),
        )
        .with_help(toolchain.runtime_library_help());
        match emitter.emit(&d, map) {
            Ok(()) => anyhow::anyhow!("runtime library unavailable"),
            Err(error) => error.into(),
        }
    })?;

    let distribution = toolchain::native::distribution::prepare(Path::new(out), shared)?;
    let link_result = toolchain.link_native(&obj_path, &runtime_lib, out, rust_bridge, args);
    discard_object(&obj_path);
    let status = link_result?;

    if !status.success() {
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0700,
            "linking failed: the linker exited with a non-zero status",
        )
        .with_help(format!(
            "check that {} exports the required Willow runtime ABI symbols",
            toolchain.runtime_library_origin(&runtime_lib).display()
        ));
        emitter.emit(&d, map)?;
        anyhow::bail!(if rust_bridge.is_some() {
            "rust_bridge_link_error: linking failed"
        } else {
            "linking failed"
        });
    }

    distribution.publish()?;
    toolchain.update_source_map(
        out,
        opts.target
            .emit_source_map
            .then_some(debug_metadata.unwrap_or("")),
    )?;

    let mode = if opts.target.build_mode == BuildMode::Release {
        "release"
    } else {
        "debug"
    };
    eprintln!("compiled [{}]: {}", mode, out);
    Ok(())
}
#[cfg(test)]
mod emit_hir_tests {
    use super::*;

    // End-to-end: a real source file goes through the full front-end and is
    // rendered as typed HIR, with each expression carrying its resolved type.
    #[test]
    fn emit_hir_renders_typed_program() {
        let path = std::env::temp_dir().join("willow_emit_hir_e2e_test.wi");
        std::fs::write(
            &path,
            "fn add(a: i64, b: i64) -> i64 { return a + b; }\n\
             fn main() { print(add(1, 2)); }\n",
        )
        .expect("write temp source");
        let text = emit_hir_text(path.to_str().unwrap()).expect("emit hir");
        let _ = std::fs::remove_file(&path);

        assert!(text.contains("fn add(a: i64, b: i64) -> i64 {"), "{text}");
        assert!(text.contains("return (a: i64 + b: i64): i64;"), "{text}");
        assert!(
            text.contains("print(add(1: i64, 2: i64): i64): void;"),
            "{text}"
        );
    }
}
