//! Top-level compilation and symbol-declaration methods for the Cranelift
//! backend (`compile_*` / `declare_*`, extracted from `mod.rs`). `compile_module`
//! / `compile_program` stay `pub` (the entry points); the rest are `pub(super)`.

use anyhow::Result;
use cranelift_codegen::ir::{AbiParam, InstBuilder, UserFuncName, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module};

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
        let program = &normalized_program;
        self.source_file = source_file.to_string();
        let module_prefix = module_symbol_prefix(canonical_path);
        self.known_modules
            .insert(mod_name.to_string(), module_prefix.clone());
        self.declare_runtime()?;
        self.declare_string_literals(program)?;
        if self.build_mode == BuildMode::Debug {
            self.declare_string_literal(source_file)?;
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
        for (_, c) in &module_classes {
            let fields: Vec<(String, Type)> = c
                .fields
                .iter()
                .filter(|f| !f.is_static)
                .map(|f| (f.name.clone(), f.ty.clone()))
                .collect();
            self.class_layouts.insert(c.name.clone(), fields);
        }
        for (_, c) in &module_classes {
            self.register_class_layout(c);
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
                    let mangled = format!("{}__{}", module_prefix, f.name);
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

        let mut aliases = ModuleAliasSnapshot::default();
        // Bind the module's own enums/interfaces under their unqualified names so
        // the module body resolves its own types internally (willow-64gs.1).
        self.alias_module_local_types(program, mod_name, &mut aliases);
        for item in &program.items {
            if let Item::Function(f) = item {
                let mangled = format!("{}__{}", module_prefix, f.name);
                self.alias_function_symbol(&f.name, &mangled, &mut aliases);
            }
        }
        for (local_name, qualified) in &module_classes {
            self.alias_class_symbol(local_name, &qualified.name, &mut aliases);
            for method in &qualified.methods {
                let local_mangled = format!("{}__{}", local_name, method.name);
                let qualified_mangled = self.class_method_symbol(&qualified.name, &method.name);
                self.alias_function_symbol(&local_mangled, &qualified_mangled, &mut aliases);
            }
        }

        let result = (|| -> Result<()> {
            // Compile bodies.
            for item in &program.items {
                match item {
                    Item::Function(f) => {
                        let mangled = format!("{}__{}", module_prefix, f.name);
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
        let program = &normalized_program;
        self.source_file = source_file.to_string();
        self.declare_runtime()?;
        self.declare_string_literals(program)?;
        if self.build_mode == BuildMode::Debug {
            self.declare_string_literal(source_file)?;
            for name in collect_nil_check_names(program) {
                self.declare_string_literal(&name)?;
            }
            self.declare_reference_debug_strings(program)?;
        }

        // Register class layouts in two passes so that base-class layouts are available
        // when derived-class layouts are built (handles any declaration order).
        // Pass 1: register layouts without inherited fields (direct fields only).
        for item in &program.items {
            if let Item::Class(c) = item {
                let fields: Vec<(String, Type)> = c
                    .fields
                    .iter()
                    .filter(|f| !f.is_static)
                    .map(|f| (f.name.clone(), f.ty.clone()))
                    .collect();
                self.class_layouts.insert(c.name.clone(), fields);
            }
        }
        // Pass 2: rebuild layouts to prepend inherited fields, then forward-declare methods.
        for item in &program.items {
            match item {
                Item::Class(c) => {
                    self.register_class_layout(c);
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
            if let Item::Function(f) = item {
                if f.is_async && f.name != "main" {
                    self.cooperative_leaves.insert(f.name.clone());
                }
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

        // Collect and declare all lambdas (they may call user functions already declared above).
        let lambdas = collect_lambdas_in_program(program);
        for (name, lambda) in &lambdas {
            self.declare_lambda(name, lambda)?;
            self.lambda_names.insert(lambda.span, name.clone());
        }

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
        self.func_ids.insert(STATIC_INIT_SYMBOL.to_string(), id);
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
        self.func_ids.insert(name.to_string(), id);
        self.func_return_types
            .insert(name.to_string(), ast_ret.clone());
        self.func_param_modes.insert(
            name.to_string(),
            l.params.iter().map(|_| ParamMode::Value).collect(),
        );
        self.func_param_debug.insert(
            name.to_string(),
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
            .insert(name.to_string(), Type::Fn(param_types, Box::new(ast_ret)));
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
            self.func_ids.insert(symbol.name.to_string(), id);
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
        let id = self.module.declare_function(symbol_name, linkage, &sig)?;
        self.func_ids.insert(lookup_name.to_string(), id);
        self.func_return_types
            .insert(lookup_name.to_string(), call_return_type.clone());
        self.func_param_modes.insert(
            lookup_name.to_string(),
            f.params.iter().map(|p| p.mode.clone()).collect(),
        );
        self.func_param_debug
            .insert(lookup_name.to_string(), param_debug_from_params(&f.params));
        // Store full function type for use when the function is passed as a value.
        let param_types = f.params.iter().map(|p| p.ty.clone()).collect();
        self.fn_types.insert(
            lookup_name.to_string(),
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

        // Async functions lower to eager scheduler tasks (willow-h2vf).
        if is_main && f.is_async {
            return self.compile_cooperative_main(name, f);
        }
        if !is_main && f.is_async {
            return self.compile_cooperative_leaf(name, f);
        }

        let mut sig = self.module.make_signature();
        let ptr_ty = self.module.target_config().pointer_type();
        if !is_main {
            for param in &f.params {
                sig.params
                    .push(AbiParam::new(param_abi_type(param, ptr_ty)));
            }
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

        let mut fg = FuncGen {
            builder: &mut builder,
            module: &mut self.module,
            func_ids: &self.func_ids,
            func_return_types: &self.func_return_types,
            fn_types: &self.fn_types,
            func_param_modes: &self.func_param_modes,
            func_param_debug: &self.func_param_debug,
            known_modules: &self.known_modules,
            lambda_names: &self.lambda_names,
            cooperative_leaves: &self.cooperative_leaves,
            string_literals: &self.string_literals,
            class_layouts: &self.class_layouts,
            static_storage: &self.static_storage,
            enum_infos: &self.enum_infos,
            class_base: &self.class_base,
            class_type_ids: &self.class_type_ids,
            lambda_return_types: &self.lambda_return_types,
            lambda_fn_types: &self.lambda_fn_types,
            interface_infos: &self.interface_infos,
            vtable_ids: &self.vtable_ids,
            async_local_types: &self.async_local_types,
                enum_variant_resolutions: &self.enum_variant_resolutions,
            async_frame: None,
            async_frame_offsets: HashMap::new(),
            main_result_err_ty,
            vars: HashMap::new(),
            return_type: f.return_type.clone(),
            current_class: None,
            is_async: f.is_async,
            terminated: false,
            gc_root_count: 0,
            build_mode: self.build_mode,
            source_file: &self.source_file,
        };

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
                let arr_id = fg.func_ids["willow_runtime_args_array"];
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

        fg.emit_block(&f.body);

        // Implicit return at end of function body.
        if !fg.terminated {
            // Pop any GC roots that were pushed for parameters.
            if fg.gc_root_count > 0 {
                fg.emit_pop_roots_n(fg.gc_root_count);
            }
            if fg.is_async {
                let future = fg.emit_ready_future_void();
                fg.builder.ins().return_(&[future]);
            } else {
                fg.builder.ins().return_(&[]);
            }
        }

        builder.finalize();
        self.module.define_function(func_id, &mut ctx)?;
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
            let sym = format!("{}__static__{}", class_key.replace("::", "__"), field.name);
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
            module: &mut self.module,
            func_ids: &self.func_ids,
            func_return_types: &self.func_return_types,
            fn_types: &self.fn_types,
            func_param_modes: &self.func_param_modes,
            func_param_debug: &self.func_param_debug,
            known_modules: &self.known_modules,
            lambda_names: &self.lambda_names,
            cooperative_leaves: &self.cooperative_leaves,
            string_literals: &self.string_literals,
            class_layouts: &self.class_layouts,
            static_storage: &self.static_storage,
            enum_infos: &self.enum_infos,
            class_base: &self.class_base,
            class_type_ids: &self.class_type_ids,
            lambda_return_types: &self.lambda_return_types,
            lambda_fn_types: &self.lambda_fn_types,
            interface_infos: &self.interface_infos,
            vtable_ids: &self.vtable_ids,
            async_local_types: &self.async_local_types,
                enum_variant_resolutions: &self.enum_variant_resolutions,
            async_frame: None,
            async_frame_offsets: HashMap::new(),
            main_result_err_ty: None,
            vars: HashMap::new(),
            return_type: Type::Void,
            current_class: None,
            is_async: false,
            terminated: false,
            gc_root_count: 0,
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
            let addr = fg.builder.ins().global_value(ptr_ty, gv);
            fg.builder.ins().store(MemFlags::new(), val, addr, 0);
            // GC-managed statics: root the slot permanently so the collector
            // traces the current value (also correct for `static mut`).
            if is_gc_managed(&item.ty, fg.enum_infos) {
                let push_id = fg.func_ids["willow_push_root"];
                let push_ref = fg.module.declare_func_in_func(push_id, fg.builder.func);
                fg.builder.ins().call(push_ref, &[addr]);
            }
        }
        fg.builder.ins().return_(&[]);
        builder.finalize();
        self.module.define_function(func_id, &mut ctx)?;
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
                self.declare_one_vtable(&c.name, &iface)?;
            }
        }
        Ok(())
    }

    pub(super) fn declare_one_vtable(
        &mut self,
        class_name: &str,
        iface: &InterfaceInfo,
    ) -> Result<()> {
        let key = (class_name.to_string(), iface.name.clone());
        if self.vtable_ids.contains_key(&key) {
            return Ok(());
        }
        let slot_count = iface.method_order.len().max(1);
        let symbol = format!(
            "{}__as__{}__vtable",
            backend_symbol_component(class_name),
            backend_symbol_component(&iface.name)
        );
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
        let mangled = self.class_method_symbol(&c.name, &m.name);
        if m.is_async {
            return self.compile_cooperative_method(&c.name, &mangled, m);
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

        let mut fg = FuncGen {
            builder: &mut builder,
            module: &mut self.module,
            func_ids: &self.func_ids,
            func_return_types: &self.func_return_types,
            fn_types: &self.fn_types,
            func_param_modes: &self.func_param_modes,
            func_param_debug: &self.func_param_debug,
            known_modules: &self.known_modules,
            lambda_names: &self.lambda_names,
            cooperative_leaves: &self.cooperative_leaves,
            string_literals: &self.string_literals,
            class_layouts: &self.class_layouts,
            static_storage: &self.static_storage,
            enum_infos: &self.enum_infos,
            class_base: &self.class_base,
            class_type_ids: &self.class_type_ids,
            lambda_return_types: &self.lambda_return_types,
            lambda_fn_types: &self.lambda_fn_types,
            interface_infos: &self.interface_infos,
            vtable_ids: &self.vtable_ids,
            async_local_types: &self.async_local_types,
                enum_variant_resolutions: &self.enum_variant_resolutions,
            async_frame: None,
            async_frame_offsets: HashMap::new(),
            main_result_err_ty: None,
            vars: HashMap::new(),
            return_type: m.return_type.clone(),
            current_class: Some(c.name.as_str()),
            is_async: m.is_async,
            terminated: false,
            gc_root_count: 0,
            build_mode: self.build_mode,
            source_file: &self.source_file,
        };

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
            fg.builder.ins().stack_store(self_val, self_slot, 0);
            {
                let ptr_ty = fg.module.target_config().pointer_type();
                let addr = fg.builder.ins().stack_addr(ptr_ty, self_slot, 0);
                let push_id = fg.func_ids["willow_push_root"];
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
            } else {
                fg.builder.ins().return_(&[]);
            }
        }

        builder.finalize();
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }
}
