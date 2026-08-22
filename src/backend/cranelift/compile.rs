//! Top-level compilation and symbol-declaration methods for the Cranelift
//! backend (`compile_*` / `declare_*`, extracted from `mod.rs`). `compile_module`
//! / `compile_program` stay `pub` (the entry points); the rest are `pub(super)`.

use anyhow::Result;
use cranelift_codegen::ir::{AbiParam, InstBuilder, UserFuncName, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module};

use super::coop_anf::normalize_coop_suspensions;
use super::*;

impl Codegen {
    /// Compile an imported module. Functions are given the mangled name
    /// `{canonical_module_path}__{fn}` with `::` normalized to `__`.
    /// Must be called before `compile_program` so the entry module can call them.
    pub fn compile_module(
        &mut self,
        mod_name: &str,
        canonical_path: &str,
        program: &Program,
        source_file: &str,
    ) -> Result<()> {
        let normalized_program = normalize_std_collection_program(program);
        let normalized_program = normalize_coop_suspensions(&normalized_program, &self.expr_types);
        let program = &normalized_program;
        self.source_file = source_file.to_string();
        let module_prefix = module_symbol_prefix(canonical_path);
        self.known_modules
            .insert(mod_name.to_string(), module_prefix.clone());
        self.declare_runtime()?;
        self.declare_string_literals(program)?;
        // The source path backs `PanicInfo.file` for every panic, so it is a
        // release-mode literal too: V1 records message AND source location
        // (willow-s9ej.7).
        self.declare_string_literal(source_file)?;
        if self.build_mode == BuildMode::Debug {
            for name in collect_nil_check_names(program) {
                self.declare_string_literal(&name)?;
            }
            self.declare_reference_debug_strings(program)?;
        }

        // INTERFACE names declared in this module, so a module-local (possibly
        // generic) interface named in an `implements` / signature by its bare name
        // is qualified to `module::Iface` (qualify_module_type alone does not
        // qualify a generic head name). Only interfaces are qualified so enum/class
        // value params keep matching a directly-imported bare alias (willow-1js.5).
        let local_type_names: std::collections::HashSet<String> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Interface(i) => Some(i.name.clone()),
                _ => None,
            })
            .collect();

        // CLASS names declared in this module, used to qualify a module-local
        // `extends Base` so the subclass's class_base / layout / inherited-method
        // resolution all key off `module::Base` (willow-2egr).
        let local_class_names: std::collections::HashSet<String> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(c) => Some(c.name.clone()),
                _ => None,
            })
            .collect();

        let module_classes: Vec<(String, ClassDecl)> = program
            .items
            .iter()
            .filter_map(|item| {
                let Item::Class(c) = item else {
                    return None;
                };
                let local_name = c.name.clone();
                let mut qualified = qualify_module_class_decl(c, mod_name);
                // Qualify a module-local generic interface in `implements`
                // (`implements Box<i64>` -> `boxmod2::Box<i64>`) so its vtable is
                // declared and keyed by the same name the entry boxes against.
                qualified.implements = qualified
                    .implements
                    .iter()
                    .map(|t| qualify_module_local_type(t, mod_name, &local_type_names))
                    .collect();
                // Qualify a module-local base class so `name()` yields the
                // module-qualified base (TypePath::name() returns only the last
                // segment, so the qualified name must live in a single Local
                // string) (willow-2egr).
                let module_local_base = match &qualified.base_class {
                    Some(TypePath::Local(name)) if local_class_names.contains(name) => {
                        Some(name.clone())
                    }
                    _ => None,
                };
                if let Some(base) = module_local_base {
                    qualified.base_class = Some(TypePath::Local(format!("{mod_name}::{base}")));
                }
                Some((local_name, qualified))
            })
            .collect();

        // Register imported module class layouts and methods under their
        // module-qualified names so entry code can call `geom::Point::new(...)`.
        //
        // Registration only records each class's OWN fields; the inherited ones
        // are prepended afterwards by `finalize_class_layouts`, which walks the
        // `extends` chain root-down. A cross-module hierarchy can arrive
        // subclass-first too, and no declaration order may change a layout
        // (willow-59gx).
        for (_, c) in &module_classes {
            self.register_class_layout(c);
        }
        self.finalize_class_layouts();
        for (_, c) in &module_classes {
            self.declare_class_methods(c)?;
            // Static-property storage for imported modules (replayed by
            // `__willow_static_init`, compiled in the entry's compile_program).
            self.declare_static_storage_for_class(&c.name, c)?;
        }
        self.validate_gc_ref_mask_layouts()?;

        // Forward-declare all functions in this module. The declaration records
        // the SIGNATURE-qualified type metadata (fn_types / param debug); the body
        // is compiled later from the original `f` under local-name aliases.
        for item in &program.items {
            match item {
                Item::Function(f) => {
                    let mangled = module_item_symbol(&module_prefix, &f.name);
                    let qualified = qualify_module_fn_signature(f, mod_name, &local_type_names);
                    self.declare_function_named(&mangled, &qualified)?;
                }
                Item::Enum(_) | Item::Class(_) | Item::Interface(_) => {}
            }
        }

        // Emit (class, interface) vtables for module classes that implement an
        // interface (their methods are declared above; implements paths were
        // module-qualified by `qualify_module_class_decl`).
        let qualified_classes: Vec<ClassDecl> =
            module_classes.iter().map(|(_, c)| c.clone()).collect();
        self.declare_vtables_for_classes(&qualified_classes)?;

        // Emit one descriptor per module class: word 0 of every object of that
        // class, holding its `type_id` and its virtual method slots
        // (willow-fm7t). Must follow the method declarations above, since every
        // slot is filled by function address.
        self.finalize_class_vslots();
        self.declare_class_descriptors_for(&qualified_classes)?;

        // Analyze under canonical backend names before installing the module's
        // temporary local aliases. Imported/unknown callees remain conservative
        // unless an earlier module already published an explicit summary.
        self.analyze_and_register_panic_effects(
            program,
            super::panic_effect::UnitNaming {
                module_prefix: Some(&module_prefix),
            },
            &[],
        );

        let mut aliases = ModuleAliasSnapshot::default();
        // Bind the module's own enums/interfaces under their unqualified names so
        // the module body resolves its own types internally (willow-64gs.1).
        self.alias_module_local_types(program, mod_name, &mut aliases);
        for item in &program.items {
            if let Item::Function(f) = item {
                let mangled = module_item_symbol(&module_prefix, &f.name);
                self.alias_function_symbol(&f.name, &mangled, &mut aliases);
            }
        }
        for (local_name, qualified) in &module_classes {
            self.alias_class_symbol(local_name, &qualified.name, &mut aliases);
            for method in &qualified.methods {
                let local_mangled = class_member_symbol(local_name, &method.name);
                let qualified_mangled = self.class_method_symbol(&qualified.name, &method.name);
                self.alias_function_symbol(&local_mangled, &qualified_mangled, &mut aliases);
            }
        }

        let result = (|| -> Result<()> {
            // Compile bodies.
            for item in &program.items {
                match item {
                    Item::Function(f) => {
                        let mangled = module_item_symbol(&module_prefix, &f.name);
                        self.compile_function_named(&mangled, f)?;
                    }
                    Item::Class(_) | Item::Enum(_) | Item::Interface(_) => {}
                }
            }
            for (_, c) in &module_classes {
                self.compile_class_methods(c)?;
            }
            Ok(())
        })();

        self.restore_module_aliases(aliases);
        result
    }

    pub fn compile_program(&mut self, program: &Program, source_file: &str) -> Result<()> {
        let normalized_program = normalize_std_collection_program(program);
        let normalized_program = normalize_coop_suspensions(&normalized_program, &self.expr_types);
        let program = &normalized_program;
        self.source_file = source_file.to_string();
        self.declare_runtime()?;
        self.declare_string_literals(program)?;
        // The source path backs `PanicInfo.file` for every panic, so it is a
        // release-mode literal too: V1 records message AND source location
        // (willow-s9ej.7).
        self.declare_string_literal(source_file)?;
        if self.build_mode == BuildMode::Debug {
            for name in collect_nil_check_names(program) {
                self.declare_string_literal(&name)?;
            }
            self.declare_reference_debug_strings(program)?;
        }

        // Pass 1: record every class's OWN fields, base and type_id. No
        // inherited field is resolved here, because a subclass may be declared
        // before its base and reading a base mid-walk sees whatever has been
        // registered so far (willow-59gx).
        for item in &program.items {
            if let Item::Class(c) = item {
                self.register_class_layout(c);
            }
        }
        // Pass 2: prepend inherited fields by walking each `extends` chain
        // root-down, which makes the result independent of declaration order.
        self.finalize_class_layouts();
        // Pass 3: forward-declare methods and static storage, now that every
        // layout is final -- a constructor's parameter order IS the layout.
        for item in &program.items {
            match item {
                Item::Class(c) => {
                    self.declare_class_methods(c)?;
                    self.declare_static_storage_for_class(&c.name, c)?;
                }
                Item::Enum(_) => {} // enum infos are registered via register_enum_info before compile
                _ => {}
            }
        }
        self.validate_gc_ref_mask_layouts()?;

        // Async calls are eager tasks (willow-h2vf): every non-main async fn is
        // exposed as a constructor that schedules its poll fn and returns the
        // async frame (`Task<T>`).
        self.cooperative_leaves.clear();
        for item in &program.items {
            if let Item::Function(f) = item
                && f.is_async
                && f.name != "main"
            {
                self.cooperative_leaves
                    .insert(crate::semantic::ids::FunctionId::free(f.name.as_str()));
            }
        }

        // Forward-declare all user functions first
        for item in &program.items {
            match item {
                Item::Function(f) => self.declare_user_function(f)?,
                Item::Class(_) | Item::Enum(_) | Item::Interface(_) => {}
            }
        }

        // Emit one static vtable per (class, implemented-interface) pair. All
        // class method symbols are declared by now, so the vtable can reference
        // them by function address.
        self.declare_interface_vtables(program)?;

        // Emit one descriptor per class (willow-fm7t). Unlike interface
        // vtables this covers EVERY class, since word 0 of every object points
        // at its descriptor whether or not the class implements an interface.
        self.finalize_class_vslots();
        self.declare_class_descriptors(program)?;

        // Collect and declare all lambdas (they may call user functions already declared above).
        let lambdas = collect_lambdas_in_program(program);
        for (name, lambda) in &lambdas {
            self.declare_lambda(name, lambda)?;
            self.lambda_names.insert(lambda.span, name.clone());
            // The lowered body was lifted under a span-derived placeholder
            // because only this loop knows the symbol (willow-0g8j.2.2). Moving
            // it into `lir_functions` under that symbol is what lets a lambda be
            // compiled by the walker like any other function.
            if let Some(mut lf) = self.lir_lambdas.remove(&lambda.span) {
                lf.name = name.clone();
                self.lir_functions.insert(name.clone(), lf);
            }
        }

        self.analyze_and_register_panic_effects(
            program,
            super::panic_effect::UnitNaming {
                module_prefix: None,
            },
            &lambdas,
        );

        // Compile lambdas first (user functions are already declared, so calls inside work).
        for (name, lambda) in &lambdas {
            self.compile_lambda(name, lambda)?;
        }

        // Always declare `__willow_static_init` (willow-qsqf §13.5). The runtime
        // calls it after `gc_init` and before `willow_user_main`; it is a no-op
        // when the program has no static properties. Declaring it unconditionally
        // keeps the runtime call path uniform regardless of the `main` lowering.
        self.declare_static_init()?;

        // Compile user function bodies and class methods
        for item in &program.items {
            match item {
                Item::Function(f) => self.compile_function(f)?,
                Item::Class(c) => self.compile_class_methods(c)?,
                Item::Enum(_) => {} // no codegen needed for enum declarations
                Item::Interface(_) => {} // interfaces emit no code in Stage 1 (vtables: Stage 3)
            }
        }

        // Compile the static-init function body after all symbols are defined.
        self.compile_static_init()?;
        Ok(())
    }

    /// Declare the `__willow_static_init` symbol (no params, no returns). Exported
    /// so the runtime entry can call it before `main` (willow-qsqf §13.5).
    pub(super) fn declare_static_init(&mut self) -> Result<()> {
        if self.func_ids.contains_key(STATIC_INIT_SYMBOL) {
            return Ok(());
        }
        let sig = self.module.make_signature();
        let id = self
            .module
            .declare_function(STATIC_INIT_SYMBOL, Linkage::Export, &sig)?;
        self.func_ids.insert(STATIC_INIT_SYMBOL, id);
        Ok(())
    }

    /// Declare the signature for a lambda private function.
    pub(super) fn declare_lambda(&mut self, name: &str, l: &LambdaExpr) -> Result<()> {
        let (param_types, ast_ret) =
            if let Some(Type::Fn(params, ret)) = self.lambda_fn_types.get(&l.span) {
                (params.clone(), *ret.clone())
            } else {
                let params = l
                    .params
                    .iter()
                    .map(|p| p.ty.clone().unwrap_or(Type::I64))
                    .collect();
                let ret = l
                    .return_type
                    .clone()
                    .or_else(|| self.lambda_return_types.get(&l.span).cloned())
                    .unwrap_or(Type::I64);
                (params, ret)
            };
        let mut sig = self.module.make_signature();
        for ty in &param_types {
            sig.params.push(AbiParam::new(clif_type(ty)));
        }
        sig.returns.push(AbiParam::new(clif_type(&ast_ret)));
        let id = self.module.declare_function(name, Linkage::Local, &sig)?;
        self.func_ids.insert(name, id);
        self.func_return_types.insert(name, ast_ret.clone());
        self.func_param_modes
            .insert(name, l.params.iter().map(|_| ParamMode::Value).collect());
        self.func_param_debug.insert(
            name,
            l.params
                .iter()
                .zip(param_types.iter())
                .map(|p| ParamDebug {
                    name: p.0.name.clone(),
                    ty: p.1.clone(),
                    mode: ParamMode::Value,
                })
                .collect(),
        );
        self.fn_types
            .insert(name, Type::Fn(param_types, Box::new(ast_ret)));
        Ok(())
    }

    /// Compile a lambda as a private function.
    pub(super) fn compile_lambda(&mut self, name: &str, l: &LambdaExpr) -> Result<()> {
        let (param_types, return_type) =
            if let Some(Type::Fn(params, ret)) = self.lambda_fn_types.get(&l.span) {
                (params.clone(), *ret.clone())
            } else {
                let params = l
                    .params
                    .iter()
                    .map(|p| p.ty.clone().unwrap_or(Type::I64))
                    .collect();
                let ret = l
                    .return_type
                    .clone()
                    .or_else(|| self.lambda_return_types.get(&l.span).cloned())
                    .unwrap_or(Type::I64);
                (params, ret)
            };
        let params: Vec<Param> = l
            .params
            .iter()
            .zip(param_types.iter())
            .map(|(p, ty)| Param {
                name: p.name.clone(),
                ty: ty.clone(),
                mode: ParamMode::Value,
                span: p.span,
                type_span: p.span,
            })
            .collect();
        let body = match &l.body {
            LambdaBody::Block(b) => b.clone(),
            // A `void` body is a STATEMENT, not a returned value: synthesising
            // `return println(x);` would emit a `return` with an operand
            // against a signature that has no result slot (willow-0g8j.2.2).
            LambdaBody::Expr(e) if return_type == Type::Void => Block {
                stmts: vec![Stmt::Expr(ExprStmt {
                    expr: *e.clone(),
                    span: e.span(),
                })],
                span: l.span,
            },
            LambdaBody::Expr(e) => Block {
                stmts: vec![Stmt::Return(ReturnStmt {
                    value: Some(*e.clone()),
                    span: e.span(),
                })],
                span: l.span,
            },
        };
        let f = FunctionDecl {
            name: name.to_string(),
            public: false,
            is_async: false,
            params,
            return_type,
            body,
            span: l.span,
        };
        self.compile_function_named(name, &f)
    }

    pub(super) fn declare_runtime(&mut self) -> Result<()> {
        if self.runtime_declared {
            return Ok(());
        }

        // The runtime ABI surface is declared from a single source of truth in
        // `crate::backend::abi`. Adding or changing a runtime symbol means
        // editing `RUNTIME_SYMBOLS`, not this loop.
        let ptr_ty = self.module.target_config().pointer_type();
        for symbol in abi::RUNTIME_SYMBOLS {
            let mut sig = self.module.make_signature();
            symbol.fill_signature(&mut sig, ptr_ty);
            let id = self
                .module
                .declare_function(symbol.name, Linkage::Import, &sig)?;
            self.func_ids.insert(symbol.name, id);
        }
        self.runtime_declared = true;
        Ok(())
    }

    pub(super) fn declare_string_literals(&mut self, program: &Program) -> Result<()> {
        for value in collect_string_literals_in_program(program) {
            self.declare_string_literal(&value)?;
        }
        // Pre-declare builtin panic messages used by Option/Result helper methods.
        for msg in [
            "called `Option::unwrap()` on a `None` value",
            "called `Result::unwrap()` on an `Err` value",
            "called `Result::unwrap_err()` on an `Ok` value",
            "interface downcast box",
            "interface downcast object",
        ] {
            self.declare_string_literal(msg)?;
        }
        Ok(())
    }

    pub(super) fn declare_reference_debug_strings(&mut self, program: &Program) -> Result<()> {
        for value in collect_reference_debug_strings_in_program(program) {
            self.declare_string_literal(&value)?;
        }
        Ok(())
    }

    pub(super) fn declare_string_literal(&mut self, value: &str) -> Result<()> {
        if self.string_literals.contains_key(value) {
            return Ok(());
        }

        let name = format!("__willow_str_{}", self.string_counter);
        self.string_counter += 1;
        let data_id = self
            .module
            .declare_data(&name, Linkage::Local, false, false)?;
        let mut data = DataDescription::new();
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        data.define(bytes.into_boxed_slice());
        self.module.define_data(data_id, &data)?;
        self.string_literals.insert(value.to_string(), data_id);
        Ok(())
    }

    pub(super) fn declare_user_function(&mut self, f: &FunctionDecl) -> Result<()> {
        let symbol_name = user_function_symbol(&f.name);
        self.declare_function_symbol(&f.name, &symbol_name, f, f.name == "main")
    }

    pub(super) fn declare_function_named(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        self.declare_function_symbol(name, name, f, false)
    }

    pub(super) fn declare_function_symbol(
        &mut self,
        lookup_name: &str,
        symbol_name: &str,
        f: &FunctionDecl,
        export: bool,
    ) -> Result<()> {
        let mut sig = self.module.make_signature();
        let ptr_ty = self.module.target_config().pointer_type();
        // `willow_user_main` is parameterless even when `fn main(args:
        // Array<String>)` is declared (see compile_function_named).
        if symbol_name != USER_MAIN_SYMBOL {
            for param in &f.params {
                sig.params
                    .push(AbiParam::new(param_abi_type(param, ptr_ty)));
            }
        }
        let call_return_type = function_call_return_type(f);
        // A `Result<void, E>` main lowers to a VOID `willow_user_main` (it
        // inspects its result and exits in the body; willow-exg). Keep this in
        // sync with compile_function_named.
        let force_void_main = symbol_name == USER_MAIN_SYMBOL && main_result_err_type(f).is_some();
        if call_return_type != Type::Void && !force_void_main {
            sig.returns
                .push(AbiParam::new(clif_type(&call_return_type)));
        }
        let linkage = if export {
            Linkage::Export
        } else {
            Linkage::Local
        };
        self.claim_symbol(symbol_name, format!("function `{}`", f.name), f.span)?;
        let id = self.module.declare_function(symbol_name, linkage, &sig)?;
        self.func_ids.insert(lookup_name, id);
        self.func_return_types
            .insert(lookup_name, call_return_type.clone());
        self.func_param_modes.insert(
            lookup_name,
            f.params.iter().map(|p| p.mode.clone()).collect(),
        );
        self.func_param_debug
            .insert(lookup_name, param_debug_from_params(&f.params));
        // Store full function type for use when the function is passed as a value.
        let param_types = f.params.iter().map(|p| p.ty.clone()).collect();
        self.fn_types.insert(
            lookup_name,
            Type::Fn(param_types, Box::new(call_return_type)),
        );
        Ok(())
    }

    pub(super) fn compile_function(&mut self, f: &FunctionDecl) -> Result<()> {
        self.compile_function_named(&f.name.clone(), f)
    }

    pub(super) fn compile_function_named(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        let func_id = self.func_ids[name];
        // `willow_user_main` is always parameterless (the runtime calls it with
        // no arguments). A declared `fn main(args: Array<String>)` parameter is
        // bound from the runtime inside the body instead of via a call argument.
        // `name` here is the lookup name (`main`), so map it to the symbol.
        let is_main = user_function_symbol(name) == USER_MAIN_SYMBOL;

        let mut sig = self.module.make_signature();
        let ptr_ty = self.module.target_config().pointer_type();
        if !is_main {
            for param in &f.params {
                sig.params
                    .push(AbiParam::new(param_abi_type(param, ptr_ty)));
            }
        }
        // LIR-walking path (willow-0g8j): a non-main function in the supported
        // scalar subset compiles from its lowered IR; everything else uses the
        // AST walk below. Decided here (before `self` is mutably borrowed).
        // `main` is eligible in its `void` forms, with or without the declared
        // `args: Array<String>` (willow-0g8j.2.10): `willow_user_main` takes no
        // arguments either way, and a declared `args` is bound from the process
        // arguments below, BEFORE the body is emitted, so the walker sees an
        // ordinary local. A `Result` main stays AST-only — its returns carry
        // the exit/report path (willow-exg).
        let simple_main = is_main && f.return_type == Type::Void;
        // Why the walker turned this function down, for the `WILLOW_LIR_REQUIRE`
        // error below. Filled in only on the path that actually asked.
        let mut lir_reject: Option<String> = None;
        let lir_fn = if (!is_main || simple_main) && super::lir_gen::lir_backend_enabled() {
            let ctx = super::lir_gen::LirTypeCtx {
                debug_build: self.build_mode == BuildMode::Debug,
                known_fn: &|n| self.func_ids.contains_key(n),
                class_layouts: &self.class_layouts,
                class_base: &self.class_base,
                class_type_ids: &self.class_type_ids,
                is_interface: &|n| self.interface_infos.contains_key(n),
                can_box: &|class, iface| {
                    super::emit::resolve_vtable_id(
                        &self.vtable_ids,
                        &self.interface_infos,
                        class,
                        iface,
                    )
                    .is_some()
                },
                // The same `enum_infos` table `enum_variant_tag` and
                // `enum_is_gc_object_type` answer from, so the tags and the
                // representation eligibility vets are the ones emission uses
                // (willow-0g8j.8).
                enum_def: &|n| {
                    let info = self.enum_infos.get(n)?;
                    Some(super::lir_gen::LirEnumDef {
                        type_params: info.type_params.clone(),
                        variants: info
                            .variants
                            .iter()
                            .map(|v| super::lir_gen::LirEnumVariant {
                                name: v.name.clone(),
                                payloads: v.payload_types.clone(),
                            })
                            .collect(),
                    })
                },
                // Straight from the same table `emit_interface_dispatch` reads,
                // so the slot the walker vets is the slot it will index.
                iface_method: &|iface_ty, method| {
                    let (iface, args): (&str, &[Type]) = match iface_ty {
                        Type::Named(name) => (name, &[]),
                        Type::Generic(name, args) => (name, args),
                        _ => return None,
                    };
                    let info = self.interface_infos.get(iface)?;
                    if info.type_params.len() != args.len() {
                        return None;
                    }
                    info.method_order.iter().position(|n| n == method)?;
                    let sig = info.methods.get(method)?;
                    let mut substitutions: HashMap<String, Type> = info
                        .type_params
                        .iter()
                        .cloned()
                        .zip(args.iter().cloned())
                        .collect();
                    substitutions.insert("Self".to_string(), iface_ty.clone());
                    Some(super::lir_gen::IfaceMethodSig {
                        params: sig
                            .params
                            .iter()
                            .map(|ty| crate::semantic::symbols::substitute_type(ty, &substitutions))
                            .collect(),
                        modes: sig.param_infos.iter().map(|p| p.mode.clone()).collect(),
                        ret: crate::semantic::symbols::substitute_type(
                            &sig.return_type,
                            &substitutions,
                        ),
                    })
                },
                // Resolved through the same hierarchy walk
                // `emit_static_field_read` uses, so an inherited static is
                // admitted iff the emitter can find its data slot.
                static_field: &|class, field| {
                    super::lookup_static_storage_in(
                        &self.static_storage,
                        &self.class_base,
                        class,
                        field,
                    )
                    .map(|info| info.ty)
                },
                // The composed order the vtable is BUILT from
                // (`emit_interface_vtables` walks `method_order`), so a prefix
                // here is a prefix of the emitted slots.
                iface_slot_prefix: &|target, source| {
                    let (Some(t), Some(s)) = (
                        self.interface_infos.get(target),
                        self.interface_infos.get(source),
                    ) else {
                        return false;
                    };
                    t.method_order.len() <= s.method_order.len()
                        && t.method_order
                            .iter()
                            .zip(&s.method_order)
                            .all(|(a, b)| a == b)
                },
                fn_types: &self.fn_types,
                func_param_modes: &self.func_param_modes,
                known_modules: &self.known_modules,
                return_type: &f.return_type,
                // The same table `emit_expr` reads for a lambda's address, so
                // the symbol eligibility vets is the symbol emission takes the
                // address of (willow-0g8j.2.2).
                lambda_symbol: &|span| self.lambda_names.get(&span).cloned(),
                cooperative_leaves: &self.cooperative_leaves,
            };
            match self.lir_functions.get(name) {
                Some(lf) => match super::lir_gen::lir_rejection_reason(lf, &ctx).or_else(|| {
                    f.is_async
                        .then(|| {
                            super::lir_gen::lir_async_rejection_reason(lf, &self.cooperative_leaves)
                        })
                        .flatten()
                }) {
                    None => Some(lf.clone()),
                    Some(why) => {
                        lir_reject = Some(why);
                        None
                    }
                },
                None => None,
            }
        } else {
            None
        };
        // `WILLOW_LIR_REQUIRE=1`: a fallback is an error instead of a silent
        // downgrade, so a test can prove a function really compiled from the
        // lowered IR.
        if lir_fn.is_none()
            && super::lir_gen::lir_backend_enabled()
            && super::lir_gen::lir_required()
            // Stage 4k is landing vertically: supported async bodies opt into
            // LIR, but an unsupported cooperative body must retain its mature
            // state-machine emitter until the whole async surface has moved.
            // WILLOW_LIR_LOG is the temporary proof for the opted-in slice.
            && !f.is_async
        {
            let reason = if is_main && !simple_main {
                "`main` is not in the supported `void` form".to_string()
            } else if !self.lir_functions.contains_key(name) {
                "it has no lowered IR".to_string()
            } else {
                // Always `Some` here: this branch means the function had lowered
                // IR and the walker rejected it, which is exactly when the reason
                // was recorded. The fallback keeps a future restructure honest.
                lir_reject.unwrap_or_else(|| {
                    "it is outside the LIR walker's supported subset".to_string()
                })
            };
            anyhow::bail!(
                "WILLOW_LIR_REQUIRE is set, but function `{name}` fell back to the AST backend: {reason}"
            );
        }

        // Async functions still use the cooperative constructor/poll ABI, but
        // an eligible body is now handed to that poll emitter as LIR. Keeping
        // the decision beside the synchronous one makes WILLOW_LIR_REQUIRE
        // police async fallback too (willow-0g8j.2.11).
        if f.is_async {
            if std::env::var("WILLOW_LIR_LOG").is_ok() {
                match &lir_fn {
                    Some(_) => eprintln!("[lir] compiling async `{name}` from lowered IR"),
                    None => {
                        // The reason the cooperative AST emitter still owns this
                        // body. `WILLOW_LIR_REQUIRE` cannot report it while
                        // Stage 4k is landing vertically, so this log is how the
                        // remaining async surface is measured.
                        let reason = lir_reject.as_deref().unwrap_or(
                            if self.lir_functions.contains_key(name) {
                                "it is outside the LIR walker's supported subset"
                            } else {
                                "it has no lowered IR"
                            },
                        );
                        eprintln!("[lir] async `{name}` stays on the AST backend: {reason}");
                    }
                }
            }
            return if is_main {
                self.compile_cooperative_main(name, f, lir_fn)
            } else {
                self.compile_cooperative_leaf(name, f, lir_fn)
            };
        }

        let call_return_type = function_call_return_type(f);
        // For a `Result<void, E>` main, the error payload type `E` drives the
        // exit/report path emitted at each return.
        let main_result_err_ty: Option<Type> = if is_main {
            main_result_err_type(f)
        } else {
            None
        };
        // A `Result<void, E>` main lowers to a VOID `willow_user_main` — it
        // inspects its result inside the body and exits accordingly (willow-exg),
        // so the runtime keeps calling `willow_user_main()` uniformly. Other
        // mains (incl. async, whose body returns a Future) keep their signature.
        let force_void_main = main_result_err_ty.is_some();
        if call_return_type != Type::Void && !force_void_main {
            sig.returns
                .push(AbiParam::new(clif_type(&call_return_type)));
        }

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());

        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);

        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);
        let panic_return_block = (!is_main && !f.is_async && self.user_function_may_panic(name))
            .then(|| builder.create_block());

        let mut fg = FuncGen {
            builder: &mut builder,
            loop_stack: Vec::new(),
            defer_stack: Vec::new(),
            defer_counter: 0,
            sync_defer_flags: HashMap::new(),
            panic_scopes: Vec::new(),
            unavailable_defer_ids: HashSet::new(),
            panic_defer_codegen_depth: 0,
            recover_eligible_depth: 0,
            panic_recovery_targets: HashSet::new(),
            panic_return_block,
            panic_function_root_depth: None,
            callstack_frame_depth: 0,
            fault_site_span: None,
            collected_defer_sites: Vec::new(),
            lock_scopes: Vec::new(),
            collected_lock_sites: Vec::new(),
            collected_cleanup_order: 0,
            module: &mut self.module,
            gc_tlab_state: self.gc_tlab_state,
            func_ids: &self.func_ids,
            func_return_types: &self.func_return_types,
            fn_types: &self.fn_types,
            func_param_modes: &self.func_param_modes,
            func_param_debug: &self.func_param_debug,
            function_may_panic: &self.function_may_panic,
            known_modules: &self.known_modules,
            lambda_names: &self.lambda_names,
            cooperative_leaves: &self.cooperative_leaves,
            string_literals: &self.string_literals,
            class_layouts: &self.class_layouts,
            static_storage: &self.static_storage,
            enum_infos: &self.enum_infos,
            class_base: &self.class_base,
            class_type_ids: &self.class_type_ids,
            class_descriptor_ids: &self.class_descriptor_ids,
            class_vslots: &self.class_vslots,
            lambda_return_types: &self.lambda_return_types,
            lambda_fn_types: &self.lambda_fn_types,
            interface_infos: &self.interface_infos,
            vtable_ids: &self.vtable_ids,
            async_local_types: &self.async_local_types,
            expr_types: &self.expr_types,
            coop_frame: None,
            coop_result_offset: None,
            enum_variant_resolutions: &self.enum_variant_resolutions,
            pattern_resolutions: &self.pattern_resolutions,
            async_frame: None,
            async_frame_offsets: HashMap::new(),
            lir_hoisted_await: None,
            main_result_err_ty,
            vars: HashMap::new(),
            return_type: f.return_type.clone(),
            current_class: None,
            is_async: f.is_async,
            terminated: false,
            gc_root_count: 0,
            coop_shadow_roots: None,
            build_mode: self.build_mode,
            source_file: &self.source_file,
        };
        if panic_return_block.is_some() {
            fg.panic_function_root_depth =
                Some(fg.emit_value_runtime_call("willow_root_depth", &[]));
        }

        // Async fns (except `main`, which has special arg binding) allocate a
        // heap frame and store GC-managed params/locals into it so they survive
        // `await` (willow-lpn.5a/5b). Eager execution is unchanged. After this,
        // `fg.async_frame_offsets` maps each frame-backed name to its offset.
        if f.is_async && !is_main {
            fg.setup_async_frame(&f.params, &f.body)?;
        }

        // Bind params
        if is_main {
            // Bind a declared `args: Array<String>` parameter from the process
            // arguments. `willow_user_main` itself takes no parameters.
            if let Some(param) = f.params.first() {
                let arr_id = fg.func_id("willow_runtime_args_array");
                let arr_ref = fg.module.declare_func_in_func(arr_id, fg.builder.func);
                let call = fg.builder.ins().call(arr_ref, &[]);
                let arr = fg.builder.inst_results(call)[0];
                fg.bind_param(&param.name, &param.ty, &param.mode, arr);
            }
        } else {
            for (i, param) in f.params.iter().enumerate() {
                let val = fg.builder.block_params(entry_block)[i];
                // Frame-back a GC-managed value param (its name is in the map).
                let framed = matches!(param.mode, ParamMode::Value)
                    .then(|| fg.async_frame_offsets.get(&param.span).copied())
                    .flatten();
                if let Some(offset) = framed {
                    fg.bind_param_framed(&param.name, &param.ty, val, offset);
                    continue;
                }
                fg.bind_param(&param.name, &param.ty, &param.mode, val);
            }
        }

        if let Some(lir_fn) = &lir_fn {
            if std::env::var("WILLOW_LIR_LOG").is_ok() {
                eprintln!("[lir] compiling `{name}` from lowered IR");
            }
            fg.emit_lir_function(lir_fn);
        } else {
            fg.emit_block(&f.body);
        }

        // Implicit return at end of function body.
        if !fg.terminated {
            // Pop any GC roots that were pushed for parameters.
            if fg.gc_root_count > 0 {
                fg.emit_pop_roots_n(fg.gc_root_count);
            }
            if fg.is_async {
                let future = fg.emit_ready_future_void();
                fg.builder.ins().return_(&[future]);
            } else if call_return_type != Type::Void && !force_void_main {
                // A value-returning fn can END with a statement whose arms all
                // return (e.g. a statement-position match, willow-zvkv); this
                // fall-through is then unreachable but must still satisfy the
                // signature.
                let zero = match clif_type(&call_return_type) {
                    types::F64 => fg.builder.ins().f64const(0.0),
                    ty => fg.builder.ins().iconst(ty, 0),
                };
                fg.builder.ins().return_(&[zero]);
            } else {
                fg.builder.ins().return_(&[]);
            }
        }
        fg.emit_panic_return(&call_return_type, force_void_main);
        fg.builder.seal_all_blocks();

        builder.finalize(self.module.target_config());
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| {
                if std::env::var("WILLOW_VERIFY_DEBUG").is_ok() {
                    eprintln!("[verify] {e:?}");
                }
                e
            })?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    pub(super) fn declare_class_methods(&mut self, c: &ClassDecl) -> Result<()> {
        // Constructors lower to an ordinary `init` method (self receiver, void
        // return) so they reuse the method machinery (willow-scq2).
        let mut all_methods: Vec<MethodDecl> = c.methods.clone();
        for ctor in &c.constructors {
            all_methods.push(constructor_to_method(ctor));
        }
        for m in &all_methods {
            let mangled = self.class_method_symbol(&c.name, &m.name);
            self.claim_symbol(&mangled, format!("method `{}::{}`", c.name, m.name), m.span)?;
            let mut sig = self.module.make_signature();
            let ptr_ty = self.module.target_config().pointer_type();
            sig.params.push(AbiParam::new(types::I64)); // self pointer
            for p in &m.params {
                sig.params.push(AbiParam::new(param_abi_type(p, ptr_ty)));
            }
            let call_return_type = method_call_return_type(m);
            if call_return_type != Type::Void {
                sig.returns
                    .push(AbiParam::new(clif_type(&call_return_type)));
            }
            let id = self
                .module
                .declare_function(&mangled, Linkage::Local, &sig)?;
            self.func_ids.insert(mangled.clone(), id);
            self.func_return_types
                .insert(mangled.clone(), call_return_type.clone());
            self.func_param_modes.insert(
                mangled.clone(),
                m.params.iter().map(|p| p.mode.clone()).collect(),
            );
            self.func_param_debug
                .insert(mangled.clone(), param_debug_from_params(&m.params));
            let mut param_types = vec![Type::Named(c.name.clone())]; // self
            param_types.extend(m.params.iter().map(|p| p.ty.clone()));
            self.fn_types
                .insert(mangled, Type::Fn(param_types, Box::new(call_return_type)));
        }
        Ok(())
    }

    /// Declare global storage for each `static [mut] name: T = expr` property and
    /// record its initializer for `__willow_static_init` (willow-qsqf §13.3/§11).
    /// `class_key` is the registered (possibly module-qualified) class name.
    pub(super) fn declare_static_storage_for_class(
        &mut self,
        class_key: &str,
        c: &ClassDecl,
    ) -> Result<()> {
        for field in &c.fields {
            if !field.is_static {
                continue;
            }
            let Some(init) = &field.initializer else {
                continue;
            };
            let key = (class_key.to_string(), field.name.clone());
            if self.static_storage.contains_key(&key) {
                continue;
            }
            let sym = static_property_symbol(class_key, &field.name);
            self.claim_symbol(
                &sym,
                format!("static property `{class_key}::{}`", field.name),
                field.span,
            )?;
            let data_id = self
                .module
                .declare_data(&sym, Linkage::Local, true, false)?;
            let mut data = DataDescription::new();
            // Zero-initialized: GC-managed slots start null so a collection during
            // static init sees a safe (null) slot (willow-qsqf §12.3). The slot
            // holds a pointer and is registered as a GC root, so it must be
            // 8-aligned — the collector dereferences the root slot.
            data.define_zeroinit(8);
            data.set_align(8);
            self.module.define_data(data_id, &data)?;
            self.static_storage.insert(
                key,
                StaticStorageInfo {
                    data_id,
                    ty: field.ty.clone(),
                },
            );
            self.static_init_order.push(StaticInitItem {
                class_key: class_key.to_string(),
                field: field.name.clone(),
                init: init.clone(),
                ty: field.ty.clone(),
            });
        }
        Ok(())
    }

    /// Compile `__willow_static_init`: evaluate every static-property initializer
    /// in declaration order, store it into global storage, and register
    /// GC-managed slots as permanent roots (willow-qsqf §11/§12). Called once at
    /// the start of `willow_user_main`.
    pub(super) fn compile_static_init(&mut self) -> Result<()> {
        let items = self.static_init_order.clone();
        let func_id = self.func_ids[STATIC_INIT_SYMBOL];

        let mut sig = self.module.make_signature();
        let _ = &mut sig; // no params, no returns
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());

        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let mut fg = FuncGen {
            builder: &mut builder,
            loop_stack: Vec::new(),
            defer_stack: Vec::new(),
            defer_counter: 0,
            sync_defer_flags: HashMap::new(),
            panic_scopes: Vec::new(),
            unavailable_defer_ids: HashSet::new(),
            panic_defer_codegen_depth: 0,
            recover_eligible_depth: 0,
            panic_recovery_targets: HashSet::new(),
            panic_return_block: None,
            panic_function_root_depth: None,
            callstack_frame_depth: 0,
            fault_site_span: None,
            collected_defer_sites: Vec::new(),
            lock_scopes: Vec::new(),
            collected_lock_sites: Vec::new(),
            collected_cleanup_order: 0,
            module: &mut self.module,
            gc_tlab_state: self.gc_tlab_state,
            func_ids: &self.func_ids,
            func_return_types: &self.func_return_types,
            fn_types: &self.fn_types,
            func_param_modes: &self.func_param_modes,
            func_param_debug: &self.func_param_debug,
            function_may_panic: &self.function_may_panic,
            known_modules: &self.known_modules,
            lambda_names: &self.lambda_names,
            cooperative_leaves: &self.cooperative_leaves,
            string_literals: &self.string_literals,
            class_layouts: &self.class_layouts,
            static_storage: &self.static_storage,
            enum_infos: &self.enum_infos,
            class_base: &self.class_base,
            class_type_ids: &self.class_type_ids,
            class_descriptor_ids: &self.class_descriptor_ids,
            class_vslots: &self.class_vslots,
            lambda_return_types: &self.lambda_return_types,
            lambda_fn_types: &self.lambda_fn_types,
            interface_infos: &self.interface_infos,
            vtable_ids: &self.vtable_ids,
            async_local_types: &self.async_local_types,
            expr_types: &self.expr_types,
            coop_frame: None,
            coop_result_offset: None,
            enum_variant_resolutions: &self.enum_variant_resolutions,
            pattern_resolutions: &self.pattern_resolutions,
            async_frame: None,
            async_frame_offsets: HashMap::new(),
            lir_hoisted_await: None,
            main_result_err_ty: None,
            vars: HashMap::new(),
            return_type: Type::Void,
            current_class: None,
            is_async: false,
            terminated: false,
            gc_root_count: 0,
            coop_shadow_roots: None,
            build_mode: self.build_mode,
            source_file: &self.source_file,
        };

        let ptr_ty = fg.module.target_config().pointer_type();
        for item in &items {
            // Initializers reference other statics by explicit class name
            // (`C::a`); `Self::` is not resolved here in the MVP.
            let val = fg.emit_expr_coerced(&item.init, &item.ty);
            let info = &fg.static_storage[&(item.class_key.clone(), item.field.clone())];
            let gv = fg
                .module
                .declare_data_in_func(info.data_id, fg.builder.func);
            let addr = fg.builder.ins().symbol_value(ptr_ty, gv);
            fg.emit_gc_heap_store(addr, 0, val, &item.ty, GcStoreDestination::GlobalStatic);
            // GC-managed statics: root the slot permanently so the collector
            // traces the current value (also correct for `static mut`).
            if is_gc_managed(&item.ty, fg.enum_infos) {
                let push_id = fg.func_id("willow_push_root");
                let push_ref = fg.module.declare_func_in_func(push_id, fg.builder.func);
                fg.builder.ins().call(push_ref, &[addr]);
            }
        }
        fg.builder.ins().return_(&[]);
        builder.finalize(self.module.target_config());
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| {
                if std::env::var("WILLOW_VERIFY_DEBUG").is_ok() {
                    eprintln!("[verify] {e:?}");
                }
                e
            })?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// Emit a static vtable per `(class, implemented-interface)` pair. Each
    /// vtable is `slot_count` function pointers in the interface's declaration
    /// (method) order; slot K points at the concrete method the class provides
    /// (found in the class itself or an ancestor). See spec §8.2 / §9.5.
    pub(super) fn declare_interface_vtables(&mut self, program: &Program) -> Result<()> {
        let classes: Vec<ClassDecl> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        self.declare_vtables_for_classes(&classes)
    }

    /// Emit `(class, interface)` vtables for the given (already module-qualified)
    /// class declarations. Used for both the entry program and imported modules.
    pub(super) fn declare_vtables_for_classes(&mut self, classes: &[ClassDecl]) -> Result<()> {
        for c in classes {
            for iface_ty in &c.implements {
                // The vtable layout (method slots) is keyed by the interface name;
                // generic type arguments do not change the class's method func ids.
                // A class may implement several instantiations of one generic
                // interface (`Container<i64>`, `Container<String>`): every slot
                // points to a monomorphic class method (default-method bodies are
                // injected once as class methods too), so all instantiations yield
                // a byte-identical vtable and correctly share this single name-keyed
                // entry — `declare_one_vtable` dedups them (willow-1js.6).
                let iface_name = match iface_ty {
                    Type::Named(n) | Type::Generic(n, _) => n.clone(),
                    _ => continue,
                };
                let Some(iface) = self.interface_infos.get(&iface_name).cloned() else {
                    continue; // unknown interface already reported by the type checker
                };
                self.declare_one_vtable(&c.name, &iface, c.span)?;
            }
        }
        Ok(())
    }

    pub(super) fn declare_one_vtable(
        &mut self,
        class_name: &str,
        iface: &InterfaceInfo,
        span: crate::diagnostics::Span,
    ) -> Result<()> {
        let key = (class_name.to_string(), iface.name.clone());
        if self.vtable_ids.contains_key(&key) {
            return Ok(());
        }
        let slot_count = iface.method_order.len().max(1);
        let symbol = vtable_symbol(class_name, &iface.name);
        // A vtable is data, not a function, but it shares the one linker
        // namespace with every other symbol the backend hands out, so it is
        // claimed like the rest (willow-uqzx, catalog item 8).
        self.claim_symbol(
            &symbol,
            format!("interface implementation `{class_name}: {}`", iface.name),
            span,
        )?;
        let data_id = self
            .module
            .declare_data(&symbol, Linkage::Local, false, false)?;
        let mut data = DataDescription::new();
        // Explicit zeroed bytes (not `define_zeroinit`, which is BSS and cannot
        // carry the function-address relocations written below).
        data.define(vec![0u8; slot_count * 8].into_boxed_slice());
        for (slot, method_name) in iface.method_order.iter().enumerate() {
            if let Some(func_id) = self.resolve_class_method_func_id(class_name, method_name) {
                let func_ref = self.module.declare_func_in_data(func_id, &mut data);
                data.write_function_addr((slot * 8) as u32, func_ref);
            }
        }
        self.module.define_data(data_id, &data)?;
        self.vtable_ids.insert(key, data_id);
        Ok(())
    }

    /// Emit one descriptor per class named in `classes` (already
    /// module-qualified), plus one for every ancestor they name.
    ///
    /// Must run AFTER `declare_class_methods`, because every slot is filled by
    /// `resolve_class_method_func_id`, which reads `func_ids`.
    /// Emit a descriptor for every class in `program` (willow-fm7t).
    pub(super) fn declare_class_descriptors(&mut self, program: &Program) -> Result<()> {
        let classes: Vec<ClassDecl> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        self.declare_class_descriptors_for(&classes)
    }

    pub(super) fn declare_class_descriptors_for(&mut self, classes: &[ClassDecl]) -> Result<()> {
        for c in classes {
            self.declare_one_class_descriptor(&c.name, c.span)?;
        }
        Ok(())
    }

    /// Emit `class_name`'s descriptor: `type_id` at offset 0, then one function
    /// pointer per entry of `class_vslots[class_name]`, in slot order.
    ///
    /// Every class gets one, INCLUDING a class with no virtual methods at all —
    /// otherwise word 0 of an object would sometimes be a descriptor address
    /// and sometimes a bare type_id, and no reader could tell which
    /// (willow-fm7t.1).
    pub(super) fn declare_one_class_descriptor(
        &mut self,
        class_name: &str,
        span: crate::diagnostics::Span,
    ) -> Result<()> {
        if self.class_descriptor_ids.contains_key(class_name) {
            return Ok(());
        }
        let Some(&type_id) = self.class_type_ids.get(class_name) else {
            return Ok(()); // not a registered class; nothing to describe
        };
        let slots = self
            .class_vslots
            .get(class_name)
            .cloned()
            .unwrap_or_default();
        let symbol = class_descriptor_symbol(class_name);
        // Shares the one linker namespace with every other symbol the backend
        // hands out, so it is claimed like the rest (willow-uqzx, item 8).
        self.claim_symbol(&symbol, format!("class `{class_name}`"), span)?;
        let data_id = self
            .module
            .declare_data(&symbol, Linkage::Local, false, false)?;
        let mut data = DataDescription::new();
        // Explicit zeroed bytes (not `define_zeroinit`, which is BSS and cannot
        // carry the function-address relocations written below) — the same
        // constraint `declare_one_vtable` works under.
        let mut bytes = vec![0u8; (slots.len() + 1) * 8];
        // The type_id is a plain constant, not a relocation, so it is written
        // into the bytes directly — in the TARGET's byte order, which is what
        // the generated `load` will read it back in.
        let type_id_bytes = match self.module.isa().endianness() {
            cranelift_codegen::ir::Endianness::Little => type_id.to_le_bytes(),
            cranelift_codegen::ir::Endianness::Big => type_id.to_be_bytes(),
        };
        bytes[..8].copy_from_slice(&type_id_bytes);
        data.define(bytes.into_boxed_slice());
        for (slot, method_name) in slots.iter().enumerate() {
            // Resolves to an ancestor's body when this class did not redeclare
            // the method, which is exactly what an inherited slot must hold.
            if let Some(func_id) = self.resolve_class_method_func_id(class_name, method_name) {
                let func_ref = self.module.declare_func_in_data(func_id, &mut data);
                data.write_function_addr(CLASS_DESCRIPTOR_HEADER_BYTES + slot as u32 * 8, func_ref);
            }
        }
        self.module.define_data(data_id, &data)?;
        self.class_descriptor_ids
            .insert(class_name.to_string(), data_id);
        Ok(())
    }

    pub(super) fn compile_class_methods(&mut self, c: &ClassDecl) -> Result<()> {
        for m in &c.methods {
            self.compile_class_method(c, m)?;
        }
        // Compile each constructor as its synthesized `init` method (willow-scq2).
        for ctor in &c.constructors {
            let m = constructor_to_method(ctor);
            self.compile_class_method(c, &m)?;
        }
        Ok(())
    }

    pub(super) fn compile_class_method(&mut self, c: &ClassDecl, m: &MethodDecl) -> Result<()> {
        if m.is_default_injected {
            // A default-interface-method body is INJECTED into every
            // implementing class with the interface's original spans, so the
            // checker's span-keyed expression types are recorded at most once
            // (and for a different `self` context). Compile injected copies
            // with the structural derivation only (willow-mb5).
            let saved = std::mem::take(&mut self.expr_types);
            let result = self.compile_class_method_inner(c, m);
            self.expr_types = saved;
            return result;
        }
        self.compile_class_method_inner(c, m)
    }

    fn compile_class_method_inner(&mut self, c: &ClassDecl, m: &MethodDecl) -> Result<()> {
        let mangled = self.class_method_symbol(&c.name, &m.name);
        if m.is_async {
            return self.compile_cooperative_method(&c.name, &mangled, m, None);
        }
        let func_id = self.func_ids[&mangled];

        let mut sig = self.module.make_signature();
        let ptr_ty = self.module.target_config().pointer_type();
        sig.params.push(AbiParam::new(types::I64)); // self pointer
        for p in &m.params {
            sig.params.push(AbiParam::new(param_abi_type(p, ptr_ty)));
        }
        let call_return_type = method_call_return_type(m);
        if call_return_type != Type::Void {
            sig.returns
                .push(AbiParam::new(clif_type(&call_return_type)));
        }

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());

        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);

        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);
        let panic_return_block =
            (!m.is_async && self.user_function_may_panic(&mangled)).then(|| builder.create_block());

        let mut fg = FuncGen {
            builder: &mut builder,
            loop_stack: Vec::new(),
            defer_stack: Vec::new(),
            defer_counter: 0,
            sync_defer_flags: HashMap::new(),
            panic_scopes: Vec::new(),
            unavailable_defer_ids: HashSet::new(),
            panic_defer_codegen_depth: 0,
            recover_eligible_depth: 0,
            panic_recovery_targets: HashSet::new(),
            panic_return_block,
            panic_function_root_depth: None,
            callstack_frame_depth: 0,
            fault_site_span: None,
            collected_defer_sites: Vec::new(),
            lock_scopes: Vec::new(),
            collected_lock_sites: Vec::new(),
            collected_cleanup_order: 0,
            module: &mut self.module,
            gc_tlab_state: self.gc_tlab_state,
            func_ids: &self.func_ids,
            func_return_types: &self.func_return_types,
            fn_types: &self.fn_types,
            func_param_modes: &self.func_param_modes,
            func_param_debug: &self.func_param_debug,
            function_may_panic: &self.function_may_panic,
            known_modules: &self.known_modules,
            lambda_names: &self.lambda_names,
            cooperative_leaves: &self.cooperative_leaves,
            string_literals: &self.string_literals,
            class_layouts: &self.class_layouts,
            static_storage: &self.static_storage,
            enum_infos: &self.enum_infos,
            class_base: &self.class_base,
            class_type_ids: &self.class_type_ids,
            class_descriptor_ids: &self.class_descriptor_ids,
            class_vslots: &self.class_vslots,
            lambda_return_types: &self.lambda_return_types,
            lambda_fn_types: &self.lambda_fn_types,
            interface_infos: &self.interface_infos,
            vtable_ids: &self.vtable_ids,
            async_local_types: &self.async_local_types,
            expr_types: &self.expr_types,
            coop_frame: None,
            coop_result_offset: None,
            enum_variant_resolutions: &self.enum_variant_resolutions,
            pattern_resolutions: &self.pattern_resolutions,
            async_frame: None,
            async_frame_offsets: HashMap::new(),
            lir_hoisted_await: None,
            main_result_err_ty: None,
            vars: HashMap::new(),
            return_type: m.return_type.clone(),
            current_class: Some(c.name.as_str()),
            is_async: m.is_async,
            terminated: false,
            gc_root_count: 0,
            coop_shadow_roots: None,
            build_mode: self.build_mode,
            source_file: &self.source_file,
        };
        if panic_return_block.is_some() {
            fg.panic_function_root_depth =
                Some(fg.emit_value_runtime_call("willow_root_depth", &[]));
        }

        // Bind `self` as the first parameter for INSTANCE methods only.
        // The uniform method ABI keeps a hidden first param slot even for static
        // methods (static `::` calls pass a dummy null there), so user params
        // always start at block_params[1]. A static method simply does not bind
        // `self`: there is no receiver, and the body cannot reference it
        // (rejected by the type checker, willow-qsqf §9.2).
        //
        // The receiver is a GC-managed class object; it must be stored in a
        // stack slot and rooted so that allocations inside the method body
        // cannot cause the receiver to be collected.
        if !m.is_static {
            let self_val = fg.builder.block_params(entry_block)[0];
            let self_slot = fg.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                0,
            ));
            fg.stack_store(self_val, self_slot);
            {
                let ptr_ty = fg.module.target_config().pointer_type();
                let addr = fg.builder.ins().stack_addr(ptr_ty, self_slot, 0);
                let push_id = fg.func_id("willow_push_root");
                let push_ref = fg.module.declare_func_in_func(push_id, fg.builder.func);
                fg.builder.ins().call(push_ref, &[addr]);
                fg.gc_root_count += 1;
            }
            let receiver_ty = Type::Named(c.name.clone());
            let receiver_storage = VarStorage::Stack {
                slot: self_slot,
                ty: receiver_ty,
            };
            fg.vars.insert("self".to_string(), receiver_storage);
        }

        // Bind remaining method params
        for (i, p) in m.params.iter().enumerate() {
            let val = fg.builder.block_params(entry_block)[i + 1];
            fg.bind_param(&p.name, &p.ty, &p.mode, val);
        }

        fg.emit_block(&m.body);

        if !fg.terminated {
            // Pop any GC roots (self, params) before the implicit void return.
            if fg.gc_root_count > 0 {
                fg.emit_pop_roots_n(fg.gc_root_count);
            }
            if fg.is_async {
                let future = fg.emit_ready_future_void();
                fg.builder.ins().return_(&[future]);
            } else if call_return_type != Type::Void {
                // Unreachable fall-through after a body that ends with an
                // all-returning statement match (willow-zvkv): satisfy the
                // signature with a typed zero.
                let zero = match clif_type(&call_return_type) {
                    types::F64 => fg.builder.ins().f64const(0.0),
                    ty => fg.builder.ins().iconst(ty, 0),
                };
                fg.builder.ins().return_(&[zero]);
            } else {
                fg.builder.ins().return_(&[]);
            }
        }
        fg.emit_panic_return(&call_return_type, false);

        builder.finalize(self.module.target_config());
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| {
                if std::env::var("WILLOW_VERIFY_DEBUG").is_ok() {
                    eprintln!("[verify] {e:?}");
                }
                e
            })?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }
}
