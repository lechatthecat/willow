//! Construction is the lifetime boundary for unit-local codegen state.
use super::*;
impl Codegen {
    pub(crate) fn declaration_context(&mut self) -> UnitCodegenContext<'_> {
        assert!(
            !self.frozen,
            "declarations are frozen after body emission starts"
        );
        UnitCodegenContext::declaring(&mut self.db, &mut self.output)
    }
    fn emission_context(&mut self) -> UnitCodegenContext<'_> {
        self.frozen = true;
        UnitCodegenContext::emitting(&self.db, &mut self.output, &mut self.generated_func_ids)
    }
}
impl<'a> UnitCodegenContext<'a> {
    pub(super) fn assert_declaring(&self) {
        assert!(
            self.declarations_open,
            "attempt to mutate frozen codegen metadata"
        );
    }
    fn declaring(db: &'a mut BuildCodegenDb, output: &'a mut EmissionState) -> Self {
        let mut context = Self {
            static_init_order: db.static_init_order.clone(),
            type_scope: TypeScope::default().fork_unit(),
            func_ids: db.func_ids.borrowed_mut(),
            func_return_types: db.func_return_types.borrowed_mut(),
            fn_types: db.fn_types.borrowed_mut(),
            func_param_modes: db.func_param_modes.borrowed_mut(),
            func_param_debug: db.func_param_debug.borrowed_mut(),
            function_may_panic: db.function_may_panic.borrowed_mut(),
            known_modules: db.known_modules.borrowed_mut(),
            method_functions: Storage::Mutable(&mut db.method_functions),
            lambda_body_names: Storage::Mutable(&mut db.lambda_body_names),
            cooperative_leaves: Storage::Mutable(&mut db.cooperative_leaves),
            body_queries: db.body_queries.clone(),
            layout_queries: db.layout_queries.clone(),
            build_mode: db.build_mode,
            enum_infos: db.enum_infos.borrowed_mut(),
            class_descriptor_ids: db.class_descriptor_ids.borrowed_mut(),
            lir_queries: db.lir_queries.clone(),
            vtable_ids: db.vtable_ids.borrowed_mut(),
            vtable_thunk_ids: Storage::Mutable(&mut db.vtable_thunk_ids),
            interface_slot_targets: Storage::Mutable(&mut db.interface_slot_targets),
            async_methods: Storage::Mutable(&mut db.async_methods),
            static_storage: db.static_storage.borrowed_mut(),
            module_init_plan: Storage::Mutable(&mut db.module_init_plan),
            visible_modules: Default::default(),
            builtin_module_aliases: Default::default(),
            lambda_names: Default::default(),
            effect_queries: Default::default(),
            source_file: Default::default(),
            dispatch_cache: Default::default(),
            lir_functions: Default::default(),
            lir_lambdas: Default::default(),
            declarations_open: true,
            checked: None,
            output,
        };
        context.install_function_scope(context.func_ids.scope().fork_codegen_unit(false));
        context.install_type_scope(context.type_scope.clone());
        context
    }
    fn emitting(
        db: &'a BuildCodegenDb,
        output: &'a mut EmissionState,
        generated: &'a mut HashMap<FunctionId, FuncId>,
    ) -> Self {
        let mut context = Self {
            static_init_order: db.static_init_order.clone(),
            type_scope: TypeScope::default().fork_unit(),
            func_ids: db.func_ids.with_generated(generated),
            func_return_types: db.func_return_types.borrowed(),
            fn_types: db.fn_types.borrowed(),
            func_param_modes: db.func_param_modes.borrowed(),
            func_param_debug: db.func_param_debug.borrowed(),
            function_may_panic: db.function_may_panic.borrowed(),
            known_modules: db.known_modules.borrowed(),
            method_functions: Storage::Shared(&db.method_functions),
            lambda_body_names: Storage::Shared(&db.lambda_body_names),
            cooperative_leaves: Storage::Shared(&db.cooperative_leaves),
            body_queries: db.body_queries.clone(),
            layout_queries: db.layout_queries.clone(),
            build_mode: db.build_mode,
            enum_infos: db.enum_infos.borrowed(),
            class_descriptor_ids: db.class_descriptor_ids.borrowed(),
            lir_queries: db.lir_queries.clone(),
            vtable_ids: db.vtable_ids.borrowed(),
            vtable_thunk_ids: Storage::Shared(&db.vtable_thunk_ids),
            interface_slot_targets: Storage::Shared(&db.interface_slot_targets),
            async_methods: Storage::Shared(&db.async_methods),
            static_storage: db.static_storage.borrowed(),
            module_init_plan: Storage::Shared(&db.module_init_plan),
            visible_modules: Default::default(),
            builtin_module_aliases: Default::default(),
            lambda_names: Default::default(),
            effect_queries: Default::default(),
            source_file: Default::default(),
            dispatch_cache: Default::default(),
            lir_functions: Default::default(),
            lir_lambdas: Default::default(),
            declarations_open: false,
            checked: None,
            output,
        };
        context.install_function_scope(context.func_ids.scope().fork_codegen_unit(true));
        context.install_type_scope(context.type_scope.clone());
        context
    }
}

/// Standalone callers supply all transient state explicitly for one unit.
#[derive(Default)]
pub struct StandaloneUnitInput {
    pub expr_types: HashMap<ExprId, Type>,
    pub lir: Option<crate::ir::lowered::LirProgram>,
}

impl Codegen {
    pub(crate) fn configure_queries(
        &mut self,
        bodies: std::rc::Rc<crate::compiler_db::body::BodyQueries>,
        lir: std::rc::Rc<crate::compiler_db::lir::LirQueries>,
        static_init_order: Option<std::sync::Arc<[crate::compiler_db::ids::StaticId]>>,
    ) {
        assert!(!self.frozen);
        self.db.body_queries = Some(bodies);
        self.db.lir_queries = Some(lir);
        self.db.static_init_order = static_init_order;
    }

    /// Metadata registration is canonical-only. Source aliases come exclusively
    /// from CheckedUnitInput/UnitScope, never from another unit's symbol table.
    pub fn register_enum_info(&mut self, info: EnumInfo) {
        assert!(!self.frozen);
        self.db.enum_infos.insert(info.name, info);
    }

    pub fn register_interface_info(
        &mut self,
        identity: TypeId,
        info: impl FnOnce() -> InterfaceInfo,
    ) -> Result<()> {
        assert!(!self.frozen);
        self.db
            .layout_queries
            .interface_composition(identity, info)?;
        Ok(())
    }

    pub(crate) fn declare_module<'a>(
        &'a mut self,
        mod_name: &str,
        canonical_path: &str,
        program: &Program,
        source_file: &str,
        input: CheckedUnitInput<'a>,
    ) -> Result<DeclaredModule> {
        let mut context = self.declaration_context();
        context.checked = Some(input.checked);
        context.effect_queries = Some((input.effects, input.unit));
        let types = context.checked_expr_types();
        context.declare_module_with_types(
            mod_name,
            canonical_path,
            program,
            source_file,
            &types,
            input.scope,
        )
    }

    pub(crate) fn declare_program<'a>(
        &'a mut self,
        program: &Program,
        source_file: &str,
        input: CheckedUnitInput<'a>,
    ) -> Result<DeclaredProgram> {
        let mut context = self.declaration_context();
        context.checked = Some(input.checked);
        context.effect_queries = Some((input.effects, input.unit));
        let types = context.checked_expr_types();
        context.declare_program_with_types(program, source_file, &types, input.scope)
    }

    pub fn module_body_plan<'a>(&self, unit: &'a DeclaredModule) -> Vec<compile::UnitBody<'a>> {
        compile::BodyPlanner(self.db.body_queries.is_some()).module_body_plan(unit)
    }
    pub fn program_body_plan<'a>(&self, unit: &'a DeclaredProgram) -> Vec<compile::UnitBody<'a>> {
        compile::BodyPlanner(self.db.body_queries.is_some()).program_body_plan(unit)
    }
    pub fn with_module_bodies<'a>(
        &'a mut self,
        unit: &DeclaredModule,
        checked: &'a crate::compiler_db::CheckedUnit,
        emit: impl FnOnce(&mut UnitCodegenContext<'_>) -> Result<()>,
    ) -> Result<()> {
        let mut context = self.emission_context();
        context.checked = Some(checked);
        context.emit_module(unit, emit)
    }
    pub fn with_program_bodies<'a>(
        &'a mut self,
        unit: &DeclaredProgram,
        checked: &'a crate::compiler_db::CheckedUnit,
        emit: impl FnOnce(&mut UnitCodegenContext<'_>) -> Result<()>,
    ) -> Result<()> {
        let mut context = self.emission_context();
        context.checked = Some(checked);
        context.emit_program(unit, emit)
    }
    pub fn compile_program(
        &mut self,
        program: &Program,
        source: &str,
        input: StandaloneUnitInput,
    ) -> Result<()> {
        let StandaloneUnitInput { expr_types, lir } = input;
        let unit = self.declaration_context().declare_program_with_types(
            program,
            source,
            &expr_types,
            Default::default(),
        )?;
        drop(expr_types);
        let mut context = self.emission_context();
        let plan = context.program_body_plan(&unit);
        context.emit_program(&unit, |context| {
            if let Some(lir) = lir {
                context.register_lir_functions(lir);
            }
            for body in &plan {
                context.compile_body(body)?;
            }
            Ok(())
        })
    }
}

/// The declaration context borrows the immutable checked artifact of its unit.
pub(crate) struct CheckedUnitInput<'a> {
    pub checked: &'a crate::compiler_db::CheckedUnit,
    pub scope: crate::compiler_db::scope::UnitScope,
    pub effects: std::rc::Rc<crate::compiler_db::effects::EffectQueries>,
    pub unit: crate::module::UnitId,
}
impl UnitCodegenContext<'_> {
    fn checked_expr_types(&self) -> HashMap<ExprId, Type> {
        self.checked
            .expect("checked declaration input")
            .expr_types
            .iter()
            .map(|(id, ty)| (*id, ty.into()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Span;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn enum_info(name: &str) -> EnumInfo {
        EnumInfo {
            name: name.into(),
            public: true,
            type_params: vec![],
            declaration_span: Span::dummy(),
            variants: vec![],
        }
    }

    #[test]
    fn unit_context_isolation_on_success_error_and_unwind() {
        // 2 phases x 4 binding shapes x 5 exits = 40 explicit perspectives.
        // Shapes: absent base, shadow base, repeated binding, class/enum collision.
        // Exits: success, returned error, propagated error, unwind, early success.
        for emission in [false, true] {
            for shape in 0..4 {
                for exit in 0..5 {
                    let mut build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
                    build.db.enum_infos.insert("A", enum_info("A"));
                    build.db.enum_infos.insert("B", enum_info("B"));
                    build.db.func_return_types.insert("A", Type::I64);
                    build.db.func_return_types.insert("B", Type::Bool);
                    for (id, name) in [(1, "A"), (2, "B")] {
                        build
                            .db
                            .known_modules
                            .register(crate::module::ModuleId(id), name, name);
                    }
                    if shape != 0 {
                        build.db.enum_infos.insert("Local", enum_info("Local"));
                        build.db.func_return_types.insert("local", Type::I64);
                        build
                            .db
                            .known_modules
                            .register(crate::module::ModuleId(1), "A", "local");
                    }
                    let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
                        let mut unit = if emission {
                            build.emission_context()
                        } else {
                            build.declaration_context()
                        };
                        assert_eq!(unit.enum_infos.get("Local").is_some(), shape != 0);
                        for _ in 0..if shape == 2 { 8 } else { 1 } {
                            unit.bind_canonical_type_alias(
                                "Local",
                                if shape == 3 { "ClassOnly" } else { "B" },
                            );
                            unit.bind_function_alias(
                                FunctionId::free("local"),
                                FunctionId::free("B"),
                            );
                            unit.known_modules
                                .bind("local".into(), crate::module::ModuleId(2));
                        }
                        assert_eq!(
                            unit.enum_infos.get("Local").map(|info| info.name),
                            if shape == 3 {
                                None
                            } else {
                                Some(TypeId::from_source_name("B"))
                            }
                        );
                        assert_eq!(unit.func_return_types.get("local"), Some(&Type::Bool));
                        assert_eq!(
                            unit.known_modules
                                .linker_prefix("local")
                                .map(String::as_str),
                            Some("B")
                        );
                        unit.visible_modules.insert("only_this_unit".into());
                        unit.builtin_module_aliases
                            .insert("files".into(), "fs".into());
                        unit.source_file = "private-unit.wi".into();
                        unit.lambda_names
                            .insert(ExprId::fresh(), FunctionId::free("local_lambda"));
                        unit.lir_functions.insert(
                            FunctionId::free("body"),
                            crate::ir::lowered::LirFunction::empty_artifact_region(),
                        );
                        unit.lir_lambdas.insert(
                            ExprId::fresh(),
                            crate::ir::lowered::LirFunction::empty_artifact_region(),
                        );
                        match exit {
                            0 => Ok(()),
                            1 => Err(anyhow::anyhow!("returned")),
                            2 => {
                                Err(anyhow::anyhow!("propagated"))?;
                                unreachable!()
                            }
                            3 => panic!("unit unwind"),
                            _ => Ok(()),
                        }
                    }));
                    assert_eq!(result.is_err(), exit == 3);
                    if let Ok(result) = result {
                        assert_eq!(result.is_err(), exit == 1 || exit == 2);
                    }
                    let next = if emission {
                        build.emission_context()
                    } else {
                        build.declaration_context()
                    };
                    assert_eq!(
                        next.enum_infos.get("Local").map(|info| info.name),
                        (shape != 0).then(|| TypeId::from_source_name("Local"))
                    );
                    assert_eq!(
                        next.func_return_types.get("local").cloned(),
                        if shape == 0 { None } else { Some(Type::I64) }
                    );
                    assert_eq!(
                        next.known_modules
                            .linker_prefix("local")
                            .map(String::as_str),
                        if shape == 0 { None } else { Some("A") }
                    );
                    assert!(next.visible_modules.is_empty());
                    assert!(next.builtin_module_aliases.is_empty());
                    assert!(next.source_file.is_empty());
                    assert!(next.lambda_names.is_empty());
                    assert!(next.lir_functions.is_empty());
                    assert!(next.lir_lambdas.is_empty());
                }
            }
        }
    }

    fn standalone(source: &str) -> (Program, StandaloneUnitInput) {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).unwrap();
        checker.check_program(&program);
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        let types = checker
            .expr_types
            .iter()
            .map(|(id, ty)| (*id, ty.into()))
            .collect();
        let tables = crate::ir::lower::CheckerTables::from_checker(&checker);
        let (hir, gaps) = crate::ir::lower::lower_program_with(&program, &tables);
        assert!(gaps.is_empty(), "{gaps:?}");
        (
            program,
            StandaloneUnitInput {
                expr_types: types,
                lir: Some(crate::ir::lowered::lower_program(&hir)),
            },
        )
    }

    #[test]
    fn standalone_entry_module_and_failure_own_their_transients() {
        for module in [false, true] {
            let source = if module {
                "pub fn answer() -> i64 { let base = 41; let f = |x: i64| x + base; return f(1); }"
            } else {
                "fn main() { let base = 41; let f = |x: i64| x + base; println(f(1)); }"
            };
            let (program, input) = standalone(source);
            let mut build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
            if module {
                let unit = build
                    .declaration_context()
                    .declare_module_with_types(
                        "worker",
                        "worker",
                        &program,
                        "worker.wi",
                        &input.expr_types,
                        Default::default(),
                    )
                    .unwrap();
                let mut context = build.emission_context();
                let plan = context.module_body_plan(&unit);
                context
                    .emit_module(&unit, |context| {
                        context.register_lir_functions(input.lir.unwrap());
                        for body in &plan {
                            context.compile_body(body)?;
                        }
                        Ok(())
                    })
                    .unwrap();
            } else {
                build.compile_program(&program, "entry.wi", input).unwrap();
            }
            let next = build.emission_context();
            assert!(next.lir_functions.is_empty());
            assert!(next.lir_lambdas.is_empty());
            assert!(next.lambda_names.is_empty());
            drop(next);
            assert!(!build.finish().unwrap().is_empty());
        }
        let (program, mut input) = standalone("fn main() { println(42); }");
        input.lir = None;
        let mut build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
        assert!(
            build
                .compile_program(&program, "missing-lir.wi", input)
                .is_err()
        );
        let next = build.emission_context();
        assert!(next.source_file.is_empty());
        assert!(next.lir_functions.is_empty());
        assert!(next.lir_lambdas.is_empty());
    }

    #[test]
    fn frozen_metadata_rejects_writes_but_generated_symbols_persist() {
        let mut build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
        let symbol;
        {
            let mut context = build.emission_context();
            let sig = context.output.module.make_signature();
            symbol = context
                .output
                .module
                .declare_function("late_poll", Linkage::Local, &sig)
                .unwrap();
            context.func_ids.insert("late_poll", symbol);
            assert_eq!(context.func_ids.get("late_poll"), Some(&symbol));
            context.alias_function_symbol("temporary", "late_poll");
            assert_eq!(context.func_ids.get("temporary"), Some(&symbol));
            assert!(
                catch_unwind(AssertUnwindSafe(|| context
                    .fn_types
                    .insert("bad", Type::Void)))
                .is_err()
            );
            assert!(
                catch_unwind(AssertUnwindSafe(|| context
                    .func_ids
                    .scope()
                    .declare("bad", FunctionId::free("bad"))))
                .is_err()
            );
        }
        assert!(!build.db.func_ids.contains_key("late_poll"));
        assert_eq!(build.generated_func_ids.len(), 1);
        let next = build.emission_context();
        assert_eq!(next.func_ids.get("late_poll"), Some(&symbol));
        assert!(next.func_ids.get("temporary").is_none());
        drop(next);
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                build.declaration_context();
            }))
            .is_err()
        );
    }

    #[test]
    fn class_import_index_visits_only_imported_methods() {
        for declarations in [1, 16, 128] {
            let mut build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
            let mut context = build.declaration_context();
            context
                .known_modules
                .register(crate::module::ModuleId(1), "pkg", "pkg");
            for index in 0..declarations {
                let tokens =
                    crate::lexer::Lexer::new("class C { pub fn value(self) -> i64 { return 1; } }")
                        .tokenize()
                        .unwrap();
                let (program, errors) = crate::parser::Parser::new(tokens).parse();
                assert!(errors.is_empty());
                let Item::Class(mut class) = program.items.into_iter().next().unwrap() else {
                    unreachable!()
                };
                class.name = format!("pkg::C{index}");
                context.register_class_layout(&class).unwrap();
                context.declare_class_methods(&class).unwrap();
            }
            context.finalize_class_layouts().unwrap();
            drop(context);
            for queries in [1, 16, 128] {
                let context = build.emission_context();
                METHOD_ALIAS_VISITS.with(|count| count.set(0));
                for _ in 0..queries {
                    let aliases = context.item_import_method_aliases("Local", "pkg", "C0");
                    assert_eq!(aliases.len(), 1);
                }
                assert_eq!(METHOD_ALIAS_VISITS.with(|count| count.get()), queries);
                println!(
                    "class_import_index classes={declarations} queries={queries} method_visits={queries}"
                );
            }
        }
    }

    fn db_identities(db: &BuildCodegenDb) -> Vec<usize> {
        let mut identities = vec![
            db.func_ids.storage_identity(),
            db.func_return_types.storage_identity(),
            db.fn_types.storage_identity(),
            db.func_param_modes.storage_identity(),
            db.func_param_debug.storage_identity(),
            db.function_may_panic.storage_identity(),
            db.enum_infos.storage_identity(),
            db.class_descriptor_ids.storage_identity(),
            db.vtable_ids.storage_identity(),
            db.static_storage.storage_identity(),
            std::ptr::from_ref(&db.lambda_body_names) as usize,
            std::ptr::from_ref(&db.cooperative_leaves) as usize,
            std::ptr::from_ref(&db.vtable_thunk_ids) as usize,
            std::ptr::from_ref(&db.interface_slot_targets) as usize,
            std::ptr::from_ref(&db.async_methods) as usize,
            std::ptr::from_ref(&db.method_functions) as usize,
            std::ptr::from_ref(&db.module_init_plan) as usize,
        ];
        identities.extend(db.known_modules.storage_identities());
        identities
    }
    fn context_identities(context: &UnitCodegenContext<'_>) -> Vec<usize> {
        let mut identities = vec![
            context.func_ids.storage_identity(),
            context.func_return_types.storage_identity(),
            context.fn_types.storage_identity(),
            context.func_param_modes.storage_identity(),
            context.func_param_debug.storage_identity(),
            context.function_may_panic.storage_identity(),
            context.enum_infos.storage_identity(),
            context.class_descriptor_ids.storage_identity(),
            context.vtable_ids.storage_identity(),
            context.static_storage.storage_identity(),
            std::ptr::from_ref(&*context.lambda_body_names) as usize,
            std::ptr::from_ref(&*context.cooperative_leaves) as usize,
            std::ptr::from_ref(&*context.vtable_thunk_ids) as usize,
            std::ptr::from_ref(&*context.interface_slot_targets) as usize,
            std::ptr::from_ref(&*context.async_methods) as usize,
            std::ptr::from_ref(&*context.method_functions) as usize,
            std::ptr::from_ref(&*context.module_init_plan) as usize,
        ];
        identities.extend(context.known_modules.storage_identities());
        identities
    }

    #[test]
    fn unit_context_scaling_borrows_every_canonical_table() {
        // Independently grow declaration count and unit count. Identity equality
        // tests the actual storage used by each lookup, not a wall-clock proxy.
        for declarations in [1, 16, 128, 512] {
            let mut build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
            for index in 0..declarations {
                let name = format!("Declaration{index}");
                build.db.fn_types.insert(&name, Type::I64);
                build.db.enum_infos.insert(name.as_str(), enum_info(&name));
            }
            let identities = db_identities(&build.db);
            {
                let context = build.declaration_context();
                assert_eq!(identities, context_identities(&context));
            }
            for units in [1, 8, 64] {
                borrowed::CANONICAL_CLONES.with(|count| count.set(0));
                let mut views = 0;
                let mut bindings = 0;
                for _ in 0..units {
                    let mut context = build.emission_context();
                    assert_eq!(identities, context_identities(&context));
                    views += identities.len();
                    for index in 0..declarations {
                        let target = format!("Declaration{index}");
                        let alias = format!("Alias{index}");
                        context.bind_canonical_type_alias(&alias, &target);
                        context.bind_function_alias(
                            FunctionId::free(&alias),
                            FunctionId::free(&target),
                        );
                        assert_eq!(
                            context.enum_infos.get(alias.as_str()).unwrap().name,
                            TypeId::from_source_name(&target)
                        );
                        assert_eq!(context.fn_types.get(&alias), Some(&Type::I64));
                        bindings += 2;
                    }
                }
                assert_eq!(views, 20 * units);
                assert_eq!(borrowed::CANONICAL_CLONES.with(|count| count.get()), 0);
                assert_eq!(bindings, 2 * declarations * units);
                println!(
                    "codegen_context declarations={declarations} units={units} borrowed_tables={views} alias_writes={bindings} canonical_map_clones=0"
                );
            }
        }
    }
}
