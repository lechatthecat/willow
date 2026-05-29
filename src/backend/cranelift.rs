use anyhow::Result;
use cranelift_codegen::ir::{
    AbiParam, InstBuilder, MemFlags, StackSlotData, StackSlotKind, TrapCode, UserFuncName,
    condcodes::{FloatCC, IntCC},
    types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::collections::{HashMap, HashSet};

use crate::parser::ast::*;
use crate::semantic::symbols::EnumInfo;
use crate::{BuildMode, CodegenOptions};

const USER_MAIN_SYMBOL: &str = "willow_user_main";

pub struct Codegen {
    module: ObjectModule,
    func_ids: HashMap<String, FuncId>,
    func_return_types: HashMap<String, Type>,
    /// Full `Type::Fn(params, ret)` for each declared function — used to type function values.
    fn_types: HashMap<String, Type>,
    /// Parameter passing modes for declared Willow functions, keyed like `func_ids`.
    func_param_modes: HashMap<String, Vec<ParamMode>>,
    /// Names of imported modules, used to distinguish `mod::fn` from `Class::method`.
    known_modules: HashSet<String>,
    /// Maps each lambda's source span to its generated private function name.
    lambda_names: HashMap<crate::diagnostics::Span, String>,
    /// Counter for generating unique lambda names.
    lambda_counter: usize,
    /// Maps each spawn expression's source span to its generated trampoline name.
    spawn_tramp_names: HashMap<crate::diagnostics::Span, String>,
    string_literals: HashMap<String, DataId>,
    string_counter: usize,
    runtime_declared: bool,
    /// Per-class ordered field list: class_name -> [(field_name, type)].
    class_layouts: HashMap<String, Vec<(String, Type)>>,
    /// Build mode: controls whether debug nil checks are emitted.
    build_mode: BuildMode,
    /// Source file path of the current compilation unit, used in nil-check diagnostics.
    source_file: String,
    /// Enum info for enum variant construction in generated code.
    enum_infos: HashMap<String, EnumInfo>,
}

impl Codegen {
    pub fn new(opts: &CodegenOptions) -> Result<Self> {
        let isa_builder = cranelift_native::builder().map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut flag_builder = settings::builder();
        match opts.build_mode {
            BuildMode::Debug => flag_builder.set("opt_level", "none")?,
            BuildMode::Release => flag_builder.set("opt_level", "speed")?,
        }
        let flags = settings::Flags::new(flag_builder);
        let isa = isa_builder.finish(flags)?;
        let obj_builder =
            ObjectBuilder::new(isa, "willow", cranelift_module::default_libcall_names())?;
        let module = ObjectModule::new(obj_builder);
        Ok(Self {
            module,
            func_ids: HashMap::new(),
            func_return_types: HashMap::new(),
            fn_types: HashMap::new(),
            func_param_modes: HashMap::new(),
            known_modules: HashSet::new(),
            lambda_names: HashMap::new(),
            lambda_counter: 0,
            spawn_tramp_names: HashMap::new(),
            string_literals: HashMap::new(),
            string_counter: 0,
            runtime_declared: false,
            class_layouts: HashMap::new(),
            build_mode: opts.build_mode,
            source_file: String::new(),
            enum_infos: HashMap::new(),
        })
    }

    /// Register enum info so the backend can lower enum variant construction.
    pub fn register_enum_info(&mut self, name: String, info: EnumInfo) {
        self.enum_infos.insert(name, info);
    }

    /// Compile an imported module. Functions are given the mangled name `{mod_name}__{fn}`.
    /// Must be called before `compile_program` so the entry module can call them.
    pub fn compile_module(
        &mut self,
        mod_name: &str,
        program: &Program,
        source_file: &str,
    ) -> Result<()> {
        self.source_file = source_file.to_string();
        self.known_modules.insert(mod_name.to_string());
        self.declare_runtime()?;
        self.declare_string_literals(program)?;
        if self.build_mode == BuildMode::Debug {
            self.declare_string_literal(source_file)?;
            for name in collect_nil_check_names(program) {
                self.declare_string_literal(&name)?;
            }
        }

        // Forward-declare all functions in this module.
        for item in &program.items {
            match item {
                Item::Function(f) => {
                    let mangled = format!("{}__{}", mod_name, f.name);
                    self.declare_function_named(&mangled, f)?;
                }
                Item::Enum(_) | Item::Class(_) => {}
            }
        }

        // Collect spawn sites and declare/compile trampolines.
        let spawns = collect_spawns_in_program(program);
        for (span, tramp_name, _callee) in &spawns {
            self.spawn_tramp_names.insert(*span, tramp_name.clone());
            self.declare_spawn_trampoline(tramp_name)?;
        }
        for (_span, tramp_name, callee) in &spawns {
            let mangled_callee = format!("{}__{}", mod_name, callee);
            self.compile_spawn_trampoline(tramp_name, &mangled_callee)?;
        }

        // Compile bodies.
        for item in &program.items {
            match item {
                Item::Function(f) => {
                    let mangled = format!("{}__{}", mod_name, f.name);
                    self.compile_function_named(&mangled, f)?;
                }
                Item::Enum(_) | Item::Class(_) => {}
            }
        }
        Ok(())
    }

    pub fn compile_program(&mut self, program: &Program, source_file: &str) -> Result<()> {
        self.source_file = source_file.to_string();
        self.declare_runtime()?;
        self.declare_string_literals(program)?;
        if self.build_mode == BuildMode::Debug {
            self.declare_string_literal(source_file)?;
            for name in collect_nil_check_names(program) {
                self.declare_string_literal(&name)?;
            }
        }

        // Register class layouts, enum info, and forward-declare class methods.
        for item in &program.items {
            match item {
                Item::Class(c) => {
                    self.register_class_layout(c);
                    self.declare_class_methods(c)?;
                }
                Item::Enum(_) => {} // enum infos are registered via register_enum_info before compile
                _ => {}
            }
        }

        // Forward-declare all user functions first
        for item in &program.items {
            match item {
                Item::Function(f) => self.declare_user_function(f)?,
                Item::Class(_) | Item::Enum(_) => {}
            }
        }

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

        // Collect spawn sites and declare/compile trampolines.
        // Must happen after all function declarations so fn_types is populated.
        let spawns = collect_spawns_in_program(program);
        for (span, tramp_name, _callee) in &spawns {
            self.spawn_tramp_names.insert(*span, tramp_name.clone());
            self.declare_spawn_trampoline(tramp_name)?;
        }
        for (_span, tramp_name, callee) in &spawns {
            self.compile_spawn_trampoline(tramp_name, callee)?;
        }

        // Compile user function bodies and class methods
        for item in &program.items {
            match item {
                Item::Function(f) => self.compile_function(f)?,
                Item::Class(c) => self.compile_class_methods(c)?,
                Item::Enum(_) => {} // no codegen needed for enum declarations
            }
        }
        Ok(())
    }

    /// Declare the signature for a lambda private function.
    fn declare_lambda(&mut self, name: &str, l: &LambdaExpr) -> Result<()> {
        let mut sig = self.module.make_signature();
        for p in &l.params {
            let ty = p.ty.as_ref().map(clif_type).unwrap_or(types::I64);
            sig.params.push(AbiParam::new(ty));
        }
        let ret = l.return_type.as_ref().map(clif_type).unwrap_or(types::I64);
        sig.returns.push(AbiParam::new(ret));
        let id = self.module.declare_function(name, Linkage::Local, &sig)?;
        self.func_ids.insert(name.to_string(), id);
        let ast_ret = l.return_type.clone().unwrap_or(Type::I64);
        self.func_return_types
            .insert(name.to_string(), ast_ret.clone());
        self.func_param_modes.insert(
            name.to_string(),
            l.params.iter().map(|_| ParamMode::Value).collect(),
        );
        let param_types: Vec<Type> = l.params.iter().filter_map(|p| p.ty.clone()).collect();
        self.fn_types
            .insert(name.to_string(), Type::Fn(param_types, Box::new(ast_ret)));
        Ok(())
    }

    /// Compile a lambda as a private function.
    fn compile_lambda(&mut self, name: &str, l: &LambdaExpr) -> Result<()> {
        let params: Vec<Param> = l
            .params
            .iter()
            .filter_map(|p| {
                p.ty.as_ref().map(|ty| Param {
                    name: p.name.clone(),
                    ty: ty.clone(),
                    mode: ParamMode::Value,
                    span: p.span,
                    type_span: p.span,
                })
            })
            .collect();
        let return_type = l.return_type.clone().unwrap_or(Type::I64);
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

    /// Declare the signature for a spawn trampoline: fn(data_ptr: I64) -> void.
    fn declare_spawn_trampoline(&mut self, name: &str) -> Result<()> {
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        let id = self.module.declare_function(name, Linkage::Local, &sig)?;
        self.func_ids.insert(name.to_string(), id);
        Ok(())
    }

    /// Compile a spawn trampoline for the given callee.
    ///
    /// Layout of the data area (pointed to by data_ptr):
    ///   offset  0      : result slot (8 bytes, or 0 bytes for void)
    ///   offset  result_slot_size       : arg0 (8 bytes)
    ///   offset  result_slot_size + 8   : arg1 (8 bytes)
    ///   ...
    fn compile_spawn_trampoline(&mut self, tramp_name: &str, callee: &str) -> Result<()> {
        let func_id = self.func_ids[tramp_name];

        let Some(fn_ty) = self.fn_types.get(callee).cloned() else {
            return Ok(());
        };
        let (param_types, ret_type) = match &fn_ty {
            Type::Fn(p, r) => (p.clone(), *r.clone()),
            _ => return Ok(()),
        };

        let result_slot_size: i32 = if ret_type == Type::Void { 0 } else { 8 };

        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        let callee_fid = self.func_ids[callee];
        let mut callee_sig = self.module.make_signature();
        for pt in &param_types {
            callee_sig.params.push(AbiParam::new(clif_type(pt)));
        }
        if ret_type != Type::Void {
            callee_sig.returns.push(AbiParam::new(clif_type(&ret_type)));
        }

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());

        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);

        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let data_ptr = builder.block_params(entry)[0];

        // Load each arg from data_ptr + result_slot_size + i*8
        let mut call_args = Vec::new();
        for (i, pt) in param_types.iter().enumerate() {
            let offset = result_slot_size + (i as i32) * 8;
            let val = builder
                .ins()
                .load(clif_type(pt), MemFlags::trusted(), data_ptr, offset);
            call_args.push(val);
        }

        // Call the target function
        let callee_fref = self.module.declare_func_in_func(callee_fid, builder.func);
        let call = builder.ins().call(callee_fref, &call_args);
        let results = builder.inst_results(call);

        // Store result (if non-void) at data_ptr + 0
        if ret_type != Type::Void {
            let result_val = results[0];
            builder
                .ins()
                .store(MemFlags::trusted(), result_val, data_ptr, 0i32);
        }

        // Call willow_task_complete(data_ptr)
        let complete_fid = self.func_ids["willow_task_complete"];
        let complete_fref = self.module.declare_func_in_func(complete_fid, builder.func);
        builder.ins().call(complete_fref, &[data_ptr]);

        builder.ins().return_(&[]);
        builder.finalize();

        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    fn declare_runtime(&mut self) -> Result<()> {
        if self.runtime_declared {
            return Ok(());
        }

        for name in &["willow_print_i64", "willow_println_i64"] {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            let id = self.module.declare_function(name, Linkage::Import, &sig)?;
            self.func_ids.insert(name.to_string(), id);
        }
        for name in &["willow_print_bool", "willow_println_bool"] {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I8));
            let id = self.module.declare_function(name, Linkage::Import, &sig)?;
            self.func_ids.insert(name.to_string(), id);
        }
        for name in &["willow_print_f64", "willow_println_f64"] {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::F64));
            let id = self.module.declare_function(name, Linkage::Import, &sig)?;
            self.func_ids.insert(name.to_string(), id);
        }
        {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::F64));
            sig.params.push(AbiParam::new(types::F64));
            sig.returns.push(AbiParam::new(types::F64));
            let id = self
                .module
                .declare_function("willow_pow_f64", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_pow_f64".to_string(), id);
        }
        {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::F64));
            sig.returns.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_f64_to_string", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_f64_to_string".to_string(), id);
        }
        for name in &[
            "willow_format_f64_17g",
            "willow_format_f64_16f",
            "willow_format_f64_6f",
        ] {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::F64));
            sig.returns.push(AbiParam::new(types::I64));
            let id = self.module.declare_function(name, Linkage::Import, &sig)?;
            self.func_ids.insert(name.to_string(), id);
        }
        for name in &["willow_print_string", "willow_println_string"] {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            let id = self.module.declare_function(name, Linkage::Import, &sig)?;
            self.func_ids.insert(name.to_string(), id);
        }
        {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            sig.params.push(AbiParam::new(types::I64));
            sig.returns.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_string_concat", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_string_concat".to_string(), id);
        }
        {
            let mut sig = self.module.make_signature();
            sig.returns.push(AbiParam::new(types::I64));
            let id =
                self.module
                    .declare_function("willow_runtime_args_len", Linkage::Import, &sig)?;
            self.func_ids
                .insert("willow_runtime_args_len".to_string(), id);
        }
        {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            sig.returns.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_runtime_arg", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_runtime_arg".to_string(), id);
        }
        {
            let mut sig = self.module.make_signature();
            sig.returns.push(AbiParam::new(types::I64));
            let id = self.module.declare_function(
                "willow_runtime_program_name",
                Linkage::Import,
                &sig,
            )?;
            self.func_ids
                .insert("willow_runtime_program_name".to_string(), id);
        }
        {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            sig.returns.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_alloc", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_alloc".to_string(), id);
        }
        {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            sig.params.push(AbiParam::new(types::I64));
            sig.returns.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_alloc_typed", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_alloc_typed".to_string(), id);
        }
        {
            // willow_gc_collect() -> void
            let sig = self.module.make_signature();
            let id = self
                .module
                .declare_function("willow_gc_collect", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_gc_collect".to_string(), id);
        }
        {
            // willow_gc_allocated_bytes() -> i64
            let mut sig = self.module.make_signature();
            sig.returns.push(AbiParam::new(types::I64));
            let id =
                self.module
                    .declare_function("willow_gc_allocated_bytes", Linkage::Import, &sig)?;
            self.func_ids
                .insert("willow_gc_allocated_bytes".to_string(), id);
        }
        {
            // willow_runtime_sleep(ms: i64) -> Future<void>
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            sig.returns.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_runtime_sleep", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_runtime_sleep".to_string(), id);
        }
        {
            let mut sig = self.module.make_signature();
            sig.returns.push(AbiParam::new(types::I64));
            let id =
                self.module
                    .declare_function("willow_future_ready_void", Linkage::Import, &sig)?;
            self.func_ids
                .insert("willow_future_ready_void".to_string(), id);
        }
        for (name, arg_ty) in &[
            ("willow_future_ready_i64", types::I64),
            ("willow_future_ready_bool", types::I8),
            ("willow_future_ready_f64", types::F64),
            ("willow_future_ready_ptr", types::I64),
        ] {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(*arg_ty));
            sig.returns.push(AbiParam::new(types::I64));
            let id = self.module.declare_function(name, Linkage::Import, &sig)?;
            self.func_ids.insert((*name).to_string(), id);
        }
        for (name, ret_ty) in &[
            ("willow_future_await_void", types::I8),
            ("willow_future_await_i64", types::I64),
            ("willow_future_await_bool", types::I8),
            ("willow_future_await_f64", types::F64),
            ("willow_future_await_ptr", types::I64),
        ] {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            sig.returns.push(AbiParam::new(*ret_ty));
            let id = self.module.declare_function(name, Linkage::Import, &sig)?;
            self.func_ids.insert((*name).to_string(), id);
        }
        {
            let mut sig = self.module.make_signature();
            sig.returns.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_channel_new", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_channel_new".to_string(), id);
        }
        for (name, arg_ty) in &[
            ("willow_channel_send_i64", types::I64),
            ("willow_channel_send_bool", types::I8),
            ("willow_channel_send_f64", types::F64),
            ("willow_channel_send_ptr", types::I64),
        ] {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            sig.params.push(AbiParam::new(*arg_ty));
            let id = self.module.declare_function(name, Linkage::Import, &sig)?;
            self.func_ids.insert((*name).to_string(), id);
        }
        for (name, ret_ty) in &[
            ("willow_channel_recv_i64", types::I64),
            ("willow_channel_recv_bool", types::I8),
            ("willow_channel_recv_f64", types::F64),
            ("willow_channel_recv_ptr", types::I64),
        ] {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            sig.returns.push(AbiParam::new(*ret_ty));
            let id = self.module.declare_function(name, Linkage::Import, &sig)?;
            self.func_ids.insert((*name).to_string(), id);
        }
        {
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_channel_close", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_channel_close".to_string(), id);
        }
        {
            // willow_push_root(slot_addr: I64) -> void
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_push_root", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_push_root".to_string(), id);
        }
        {
            // willow_pop_roots(n: I32) -> void
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I32));
            let id = self
                .module
                .declare_function("willow_pop_roots", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_pop_roots".to_string(), id);
        }
        {
            // willow_nil_deref(file: I64, line: I32, col: I32, context: I64) -> void (noreturn)
            let ptr_ty = self.module.target_config().pointer_type();
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(ptr_ty)); // file
            sig.params.push(AbiParam::new(types::I32)); // line
            sig.params.push(AbiParam::new(types::I32)); // col
            sig.params.push(AbiParam::new(ptr_ty)); // context
            let id = self
                .module
                .declare_function("willow_nil_deref", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_nil_deref".to_string(), id);
        }
        {
            // willow_task_alloc(data_size: I64) -> I64 (data_ptr)
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            sig.returns.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_task_alloc", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_task_alloc".to_string(), id);
        }
        {
            // willow_task_spawn(tramp_ptr: I64, data_ptr: I64) -> void
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            sig.params.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_task_spawn", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_task_spawn".to_string(), id);
        }
        {
            // willow_task_join(data_ptr: I64) -> void
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_task_join", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_task_join".to_string(), id);
        }
        {
            // willow_task_complete(data_ptr: I64) -> void
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function("willow_task_complete", Linkage::Import, &sig)?;
            self.func_ids.insert("willow_task_complete".to_string(), id);
        }
        {
            // willow_task_set_spawn_location(data_ptr: I64, file: ptr, line: I32, col: I32) -> void
            let ptr_ty = self.module.target_config().pointer_type();
            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(types::I64));
            sig.params.push(AbiParam::new(ptr_ty));
            sig.params.push(AbiParam::new(types::I32));
            sig.params.push(AbiParam::new(types::I32));
            let id = self.module.declare_function(
                "willow_task_set_spawn_location",
                Linkage::Import,
                &sig,
            )?;
            self.func_ids
                .insert("willow_task_set_spawn_location".to_string(), id);
        }
        self.runtime_declared = true;
        Ok(())
    }

    fn declare_string_literals(&mut self, program: &Program) -> Result<()> {
        for value in collect_string_literals_in_program(program) {
            self.declare_string_literal(&value)?;
        }
        Ok(())
    }

    fn declare_string_literal(&mut self, value: &str) -> Result<()> {
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

    fn declare_user_function(&mut self, f: &FunctionDecl) -> Result<()> {
        let symbol_name = user_function_symbol(&f.name);
        self.declare_function_symbol(&f.name, &symbol_name, f, f.name == "main")
    }

    fn declare_function_named(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        self.declare_function_symbol(name, name, f, false)
    }

    fn declare_function_symbol(
        &mut self,
        lookup_name: &str,
        symbol_name: &str,
        f: &FunctionDecl,
        export: bool,
    ) -> Result<()> {
        let mut sig = self.module.make_signature();
        let ptr_ty = self.module.target_config().pointer_type();
        for param in &f.params {
            sig.params
                .push(AbiParam::new(param_abi_type(param, ptr_ty)));
        }
        let call_return_type = function_call_return_type(f);
        if call_return_type != Type::Void {
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
        // Store full function type for use when the function is passed as a value.
        let param_types = f.params.iter().map(|p| p.ty.clone()).collect();
        self.fn_types.insert(
            lookup_name.to_string(),
            Type::Fn(param_types, Box::new(call_return_type)),
        );
        Ok(())
    }

    fn compile_function(&mut self, f: &FunctionDecl) -> Result<()> {
        self.compile_function_named(&f.name.clone(), f)
    }

    fn compile_function_named(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        let func_id = self.func_ids[name];

        let mut sig = self.module.make_signature();
        let ptr_ty = self.module.target_config().pointer_type();
        for param in &f.params {
            sig.params
                .push(AbiParam::new(param_abi_type(param, ptr_ty)));
        }
        let call_return_type = function_call_return_type(f);
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
            known_modules: &self.known_modules,
            lambda_names: &self.lambda_names,
            spawn_tramp_names: &self.spawn_tramp_names,
            string_literals: &self.string_literals,
            class_layouts: &self.class_layouts,
            enum_infos: &self.enum_infos,
            vars: HashMap::new(),
            return_type: f.return_type.clone(),
            is_async: f.is_async,
            terminated: false,
            gc_root_count: 0,
            build_mode: self.build_mode,
            source_file: &self.source_file,
        };

        // Bind params
        for (i, param) in f.params.iter().enumerate() {
            let val = fg.builder.block_params(entry_block)[i];
            fg.bind_param(&param.name, &param.ty, &param.mode, val);
        }

        fg.emit_block(&f.body);

        // Implicit return at end of function body.
        if !fg.terminated {
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

    // ── Class helpers ─────────────────────────────────────────────────────────

    fn register_class_layout(&mut self, c: &ClassDecl) {
        let fields = c
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.ty.clone()))
            .collect();
        self.class_layouts.insert(c.name.clone(), fields);
    }

    fn declare_class_methods(&mut self, c: &ClassDecl) -> Result<()> {
        for m in &c.methods {
            let mangled = format!("{}__{}", c.name, m.name);
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
            let mut param_types = vec![Type::Named(c.name.clone())]; // self
            param_types.extend(m.params.iter().map(|p| p.ty.clone()));
            self.fn_types
                .insert(mangled, Type::Fn(param_types, Box::new(call_return_type)));
        }
        Ok(())
    }

    fn compile_class_methods(&mut self, c: &ClassDecl) -> Result<()> {
        for m in &c.methods {
            self.compile_class_method(c, m)?;
        }
        Ok(())
    }

    fn compile_class_method(&mut self, c: &ClassDecl, m: &MethodDecl) -> Result<()> {
        let mangled = format!("{}__{}", c.name, m.name);
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
            known_modules: &self.known_modules,
            lambda_names: &self.lambda_names,
            spawn_tramp_names: &self.spawn_tramp_names,
            string_literals: &self.string_literals,
            class_layouts: &self.class_layouts,
            enum_infos: &self.enum_infos,
            vars: HashMap::new(),
            return_type: m.return_type.clone(),
            is_async: m.is_async,
            terminated: false,
            gc_root_count: 0,
            build_mode: self.build_mode,
            source_file: &self.source_file,
        };

        // Bind self as first param
        let self_val = fg.builder.block_params(entry_block)[0];
        let self_var = fg.builder.declare_var(types::I64);
        fg.builder.def_var(self_var, self_val);
        fg.vars.insert(
            "self".to_string(),
            VarStorage::Value {
                var: self_var,
                ty: Type::Named(c.name.clone()),
            },
        );

        // Bind remaining method params
        for (i, p) in m.params.iter().enumerate() {
            let val = fg.builder.block_params(entry_block)[i + 1];
            fg.bind_param(&p.name, &p.ty, &p.mode, val);
        }

        fg.emit_block(&m.body);

        if !fg.terminated {
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

    pub fn embed_runtime_metadata(&mut self, metadata: &str) -> Result<()> {
        let data_id = self.module.declare_data(
            "willow_runtime_metadata_v1",
            Linkage::Export,
            false,
            false,
        )?;
        let mut data = DataDescription::new();
        let mut bytes = b"willow_runtime_metadata_v1\n".to_vec();
        bytes.extend_from_slice(metadata.as_bytes());
        bytes.push(0);
        data.define(bytes.into_boxed_slice());
        self.module.define_data(data_id, &data)?;
        Ok(())
    }

    pub fn finish(self) -> Result<Vec<u8>> {
        let obj = self.module.finish();
        Ok(obj.emit()?)
    }
}

struct FuncGen<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    module: &'a mut ObjectModule,
    func_ids: &'a HashMap<String, FuncId>,
    func_return_types: &'a HashMap<String, Type>,
    fn_types: &'a HashMap<String, Type>,
    func_param_modes: &'a HashMap<String, Vec<ParamMode>>,
    known_modules: &'a HashSet<String>,
    lambda_names: &'a HashMap<crate::diagnostics::Span, String>,
    spawn_tramp_names: &'a HashMap<crate::diagnostics::Span, String>,
    string_literals: &'a HashMap<String, DataId>,
    class_layouts: &'a HashMap<String, Vec<(String, Type)>>,
    enum_infos: &'a HashMap<String, EnumInfo>,
    vars: HashMap<String, VarStorage>,
    return_type: Type,
    is_async: bool,
    terminated: bool,
    /// Number of GC roots currently on the root stack for this function invocation.
    gc_root_count: usize,
    /// Build mode: controls whether debug nil checks are emitted.
    build_mode: BuildMode,
    /// Source file path used in nil-check runtime diagnostics.
    source_file: &'a str,
}

#[derive(Clone)]
enum VarStorage {
    Value {
        var: Variable,
        ty: Type,
    },
    Stack {
        slot: cranelift_codegen::ir::StackSlot,
        ty: Type,
    },
    ReferencePtr {
        var: Variable,
        ty: Type,
    },
}

impl VarStorage {
    fn ty(&self) -> &Type {
        match self {
            VarStorage::Value { ty, .. }
            | VarStorage::Stack { ty, .. }
            | VarStorage::ReferencePtr { ty, .. } => ty,
        }
    }
}

impl<'a, 'b> FuncGen<'a, 'b> {
    fn bind_param(
        &mut self,
        name: &str,
        ty: &Type,
        mode: &ParamMode,
        val: cranelift_codegen::ir::Value,
    ) {
        match mode {
            ParamMode::Value => {
                let var = self.builder.declare_var(clif_type(ty));
                self.builder.def_var(var, val);
                self.vars.insert(
                    name.to_string(),
                    VarStorage::Value {
                        var,
                        ty: ty.clone(),
                    },
                );
            }
            ParamMode::Reference { .. } => {
                let ptr_ty = self.module.target_config().pointer_type();
                let var = self.builder.declare_var(ptr_ty);
                self.builder.def_var(var, val);
                self.vars.insert(
                    name.to_string(),
                    VarStorage::ReferencePtr {
                        var,
                        ty: ty.clone(),
                    },
                );
            }
        }
    }

    fn create_local_stack_slot(
        &mut self,
        ty: &Type,
        val: cranelift_codegen::ir::Value,
    ) -> VarStorage {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        self.builder.ins().stack_store(val, slot, 0);
        VarStorage::Stack {
            slot,
            ty: ty.clone(),
        }
    }

    fn load_var(&mut self, storage: &VarStorage) -> cranelift_codegen::ir::Value {
        match storage {
            VarStorage::Value { var, .. } => self.builder.use_var(*var),
            VarStorage::Stack { slot, ty } => {
                self.builder.ins().stack_load(clif_type(ty), *slot, 0)
            }
            VarStorage::ReferencePtr { var, ty } => {
                let ptr = self.builder.use_var(*var);
                self.builder
                    .ins()
                    .load(clif_type(ty), MemFlags::new(), ptr, 0)
            }
        }
    }

    fn store_var(&mut self, storage: &VarStorage, val: cranelift_codegen::ir::Value) {
        match storage {
            VarStorage::Value { var, .. } => self.builder.def_var(*var, val),
            VarStorage::Stack { slot, .. } => {
                self.builder.ins().stack_store(val, *slot, 0);
            }
            VarStorage::ReferencePtr { var, .. } => {
                let ptr = self.builder.use_var(*var);
                self.builder.ins().store(MemFlags::new(), val, ptr, 0);
            }
        }
    }

    fn address_of_var(&mut self, storage: &VarStorage) -> cranelift_codegen::ir::Value {
        match storage {
            VarStorage::Stack { slot, .. } => {
                let ptr_ty = self.module.target_config().pointer_type();
                self.builder.ins().stack_addr(ptr_ty, *slot, 0)
            }
            VarStorage::ReferencePtr { var, .. } => self.builder.use_var(*var),
            VarStorage::Value { var, .. } => {
                let ptr_ty = self.module.target_config().pointer_type();
                let val = self.builder.use_var(*var);
                let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    0,
                ));
                self.builder.ins().stack_store(val, slot, 0);
                self.builder.ins().stack_addr(ptr_ty, slot, 0)
            }
        }
    }

    /// Push a GC root for a pointer value. Creates a stack slot to hold the pointer so
    /// the GC can find and mark the object via `willow_push_root`.
    fn emit_push_root(&mut self, val: cranelift_codegen::ir::Value) {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        self.builder.ins().stack_store(val, slot, 0);
        let ptr_ty = self.module.target_config().pointer_type();
        let addr = self.builder.ins().stack_addr(ptr_ty, slot, 0);
        let push_id = self.func_ids["willow_push_root"];
        let push_ref = self.module.declare_func_in_func(push_id, self.builder.func);
        self.builder.ins().call(push_ref, &[addr]);
        self.gc_root_count += 1;
    }

    /// Pop `n` GC roots by calling `willow_pop_roots(n)`.
    fn emit_pop_roots_n(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let pop_id = self.func_ids["willow_pop_roots"];
        let pop_ref = self.module.declare_func_in_func(pop_id, self.builder.func);
        let n_val = self.builder.ins().iconst(types::I32, n as i64);
        self.builder.ins().call(pop_ref, &[n_val]);
    }

    fn emit_ready_future_void(&mut self) -> cranelift_codegen::ir::Value {
        let fid = self.func_ids["willow_future_ready_void"];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[]);
        self.builder.inst_results(call)[0]
    }

    fn emit_ready_future(
        &mut self,
        ty: &Type,
        value: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let runtime_name = future_ready_runtime_name(ty);
        let fid = self.func_ids[runtime_name];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[value]);
        self.builder.inst_results(call)[0]
    }

    fn emit_await(&mut self, await_expr: &AwaitExpr) -> cranelift_codegen::ir::Value {
        let future_ty = self.ast_type_of(&await_expr.expr);
        let output_ty = future_output_type(&future_ty).unwrap_or(Type::Void);
        let future = self.emit_expr(&await_expr.expr);
        let runtime_name = future_await_runtime_name(&output_ty);
        let fid = self.func_ids[runtime_name];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[future]);
        self.builder.inst_results(call)[0]
    }

    fn emit_block(&mut self, block: &Block) {
        let saved_vars = self.vars.clone();
        let gc_roots_before = self.gc_root_count;

        for stmt in &block.stmts {
            if self.terminated {
                break;
            }
            self.emit_stmt(stmt);
        }

        // Pop any GC roots introduced by this block before the vars go out of scope.
        if !self.terminated {
            let block_roots = self.gc_root_count - gc_roots_before;
            if block_roots > 0 {
                self.emit_pop_roots_n(block_roots);
            }
        }
        // Restore scope: gc_root_count goes back to what it was before the block
        // (in the terminated path the return handler already popped all roots).
        self.gc_root_count = gc_roots_before;
        self.vars = saved_vars;
    }

    fn emit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                let val = self.emit_expr(&s.init);
                let ast_ty =
                    s.ty.clone()
                        .unwrap_or_else(|| self.ast_type_of_init(&s.init));
                let ty = clif_type(&ast_ty);
                // Always use Cranelift SSA variables (register-allocatable) for both
                // mutable and immutable locals.  Stack slots are only allocated lazily
                // when a variable's address is actually taken (i.e. passed as &mut).
                // See emit_reference_arg_address for the lazy-promotion path.
                let storage = {
                    let var = self.builder.declare_var(ty);
                    self.builder.def_var(var, val);
                    VarStorage::Value {
                        var,
                        ty: ast_ty.clone(),
                    }
                };
                if is_gc_managed(&ast_ty) {
                    self.emit_push_root(val);
                }
                self.vars.insert(s.name.clone(), storage);
            }
            Stmt::Assign(s) => {
                if let Some(storage) = self.vars.get(&s.name).cloned() {
                    let val = self.emit_expr(&s.value);
                    self.store_var(&storage, val);
                }
            }
            Stmt::If(s) => self.emit_if(s),
            Stmt::While(s) => self.emit_while(s),
            Stmt::Return(s) => {
                if self.is_async {
                    let future = if let Some(val_expr) = &s.value {
                        if self.return_type == Type::Void {
                            self.emit_expr(val_expr);
                            self.emit_ready_future_void()
                        } else {
                            let return_type = self.return_type.clone();
                            let val = self.emit_expr(val_expr);
                            self.emit_ready_future(&return_type, val)
                        }
                    } else {
                        self.emit_ready_future_void()
                    };
                    if self.gc_root_count > 0 {
                        self.emit_pop_roots_n(self.gc_root_count);
                    }
                    self.builder.ins().return_(&[future]);
                } else {
                    if let Some(val_expr) = &s.value {
                        // Evaluate the return value BEFORE popping roots (it may load from GC objects).
                        let val = self.emit_expr(val_expr);
                        if self.gc_root_count > 0 {
                            self.emit_pop_roots_n(self.gc_root_count);
                        }
                        self.builder.ins().return_(&[val]);
                    } else {
                        if self.gc_root_count > 0 {
                            self.emit_pop_roots_n(self.gc_root_count);
                        }
                        self.builder.ins().return_(&[]);
                    }
                }
                self.terminated = true;
            }
            Stmt::Expr(s) => {
                self.emit_expr(&s.expr);
            }
        }
    }

    fn emit_if(&mut self, s: &IfStmt) {
        let cond = self.emit_expr(&s.cond);

        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(cond, then_block, &[], else_block, &[]);

        // then branch
        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        let outer_terminated = self.terminated;
        self.terminated = false;
        self.emit_block(&s.then_block);
        let then_terminated = self.terminated;
        if !self.terminated {
            self.builder.ins().jump(merge_block, &[]);
        }

        // else branch
        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        self.terminated = false;
        if let Some(else_b) = &s.else_block {
            self.emit_block(else_b);
        }
        let else_terminated = self.terminated;
        if !self.terminated {
            self.builder.ins().jump(merge_block, &[]);
        }

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.terminated = outer_terminated || (then_terminated && else_terminated);
    }

    fn emit_while(&mut self, s: &WhileStmt) {
        let header = self.builder.create_block();
        let body_block = self.builder.create_block();
        let exit_block = self.builder.create_block();

        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let cond = self.emit_expr(&s.cond);
        self.builder
            .ins()
            .brif(cond, body_block, &[], exit_block, &[]);

        self.builder.switch_to_block(body_block);
        self.builder.seal_block(body_block);
        self.terminated = false;
        self.emit_block(&s.body);
        if !self.terminated {
            self.builder.ins().jump(header, &[]);
        }

        self.builder.seal_block(header);
        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(exit_block);
        self.terminated = false;
    }

    /// Determine the AST type of a `let` initialiser, including full `Type::Fn` for
    /// named-function and lambda values so indirect calls later work correctly.
    /// Resolve the Willow AST type of an expression, handling FieldAccess and
    /// MethodCall by looking up class layouts and func_return_types.
    fn ast_type_of(&self, expr: &Expr) -> Type {
        match expr {
            Expr::FieldAccess(obj, field_name, _) => {
                if let Some(class_name) = class_name_for_object_type(&self.ast_type_of(obj)) {
                    if let Some(layout) = self.class_layouts.get(&class_name) {
                        if let Some((_, ty)) = layout.iter().find(|(n, _)| n == field_name) {
                            return ty.clone();
                        }
                    }
                }
                Type::I64
            }
            Expr::MethodCall(m) => {
                let obj_ty = self.ast_type_of(&m.object);
                if m.method == "join" {
                    if let Some(result_ty) = join_handle_result_type(&obj_ty) {
                        return result_ty;
                    }
                }
                if m.method == "recv" {
                    if let Some(element_ty) = channel_element_type(&obj_ty) {
                        return element_ty;
                    }
                }
                if let Some(class_name) = class_name_for_object_type(&obj_ty) {
                    let mangled = format!("{}__{}", class_name, m.method);
                    if let Some(ty) = self.func_return_types.get(&mangled) {
                        return ty.clone();
                    }
                }
                Type::I64
            }
            _ => ast_type_of_expr(expr, &self.vars, self.func_return_types),
        }
    }

    fn ast_type_of_init(&self, expr: &Expr) -> Type {
        match expr {
            // Named function used as a value → look up its full fn type.
            Expr::Var(name, _) => {
                if let Some(ty) = self.fn_types.get(name.as_str()) {
                    return ty.clone();
                }
                self.ast_type_of(expr)
            }
            // Lambda expression → build the fn type from its params and return type.
            Expr::Lambda(l) => {
                let params = l.params.iter().filter_map(|p| p.ty.clone()).collect();
                let ret = l.return_type.clone().unwrap_or(Type::I64);
                Type::Fn(params, Box::new(ret))
            }
            _ => self.ast_type_of(expr),
        }
    }

    fn emit_expr(&mut self, expr: &Expr) -> cranelift_codegen::ir::Value {
        match expr {
            Expr::Integer(n, _) => self.builder.ins().iconst(types::I64, *n),
            Expr::Float(f, _) => self.builder.ins().f64const(*f),
            Expr::Bool(b, _) => self.builder.ins().iconst(types::I8, if *b { 1 } else { 0 }),
            Expr::Nil(_) => self.builder.ins().iconst(types::I64, 0),
            Expr::String(value, _) => self.emit_string_literal(value),
            Expr::Var(name, _) => {
                // Local variable or function value?
                if let Some(storage) = self.vars.get(name.as_str()).cloned() {
                    return self.load_var(&storage);
                }
                // Named function used as a first-class value — emit its address.
                if let Some(&fid) = self.func_ids.get(name.as_str()) {
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    return self.builder.ins().func_addr(types::I64, fref);
                }
                // Should not reach here after type checking, but produce a safe zero.
                self.builder.ins().iconst(types::I64, 0)
            }
            Expr::Binary(b) => self.emit_binary(b),
            Expr::Unary(u) => self.emit_unary(u),
            Expr::Call(c) => self.emit_call(c),
            Expr::Print(arg, newline, _) => {
                let val = self.emit_expr(arg);
                let arg_ty = self.ast_type_of(arg);
                let fn_name = match (arg_ty, newline) {
                    (Type::I64, false) => "willow_print_i64",
                    (Type::I64, true) => "willow_println_i64",
                    (Type::F64, false) => "willow_print_f64",
                    (Type::F64, true) => "willow_println_f64",
                    (Type::Bool, false) => "willow_print_bool",
                    (Type::Bool, true) => "willow_println_bool",
                    (Type::String, false) => "willow_print_string",
                    (Type::String, true) => "willow_println_string",
                    _ => "",
                };
                if !fn_name.is_empty() {
                    let fid = self.func_ids[fn_name];
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    self.builder.ins().call(fref, &[val]);
                }
                self.builder.ins().iconst(types::I8, 0)
            }
            Expr::Ternary(t) => self.emit_ternary(t),
            // Lambda: emit the address of the pre-compiled private function.
            Expr::Lambda(l) => {
                if let Some(name) = self.lambda_names.get(&l.span) {
                    if let Some(&fid) = self.func_ids.get(name.as_str()) {
                        let fref = self.module.declare_func_in_func(fid, self.builder.func);
                        return self.builder.ins().func_addr(types::I64, fref);
                    }
                }
                self.builder.ins().iconst(types::I64, 0)
            }
            // Field/method access: codegen deferred to willow-jbf
            Expr::FieldAccess(obj, field_name, _) => self.emit_field_access(obj, field_name),
            Expr::MethodCall(m) => self.emit_method_call(m),
            Expr::ObjectLiteral(o) => self.emit_object_literal(o),
            Expr::StaticCall(s) => self.emit_static_call(s),
            Expr::Spawn(s) => self.emit_spawn(s),
            Expr::Await(a) => self.emit_await(a),
            Expr::Select(_) => self.builder.ins().iconst(types::I64, 0),
            Expr::Match(m) => self.emit_match(m),
        }
    }

    fn emit_string_literal(&mut self, value: &str) -> cranelift_codegen::ir::Value {
        if let Some(data_id) = self.string_literals.get(value) {
            let gv = self
                .module
                .declare_data_in_func(*data_id, self.builder.func);
            let ptr_ty = self.module.target_config().pointer_type();
            return self.builder.ins().global_value(ptr_ty, gv);
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    fn emit_binary(&mut self, b: &BinaryExpr) -> cranelift_codegen::ir::Value {
        let lhs = self.emit_expr(&b.lhs);
        let lty = ast_type_of_expr(&b.lhs, &self.vars, self.func_return_types);
        let is_float = lty == Type::F64;
        match &b.op {
            BinOp::And => self.emit_short_circuit_and(lhs, &b.rhs),
            BinOp::Or => self.emit_short_circuit_or(lhs, &b.rhs),
            BinOp::Add => {
                let rhs = self.emit_expr(&b.rhs);
                if lty == Type::String {
                    let fid = self.func_ids["willow_string_concat"];
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    let call = self.builder.ins().call(fref, &[lhs, rhs]);
                    self.builder.inst_results(call)[0]
                } else if is_float {
                    self.builder.ins().fadd(lhs, rhs)
                } else {
                    self.builder.ins().iadd(lhs, rhs)
                }
            }
            BinOp::Sub => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    self.builder.ins().fsub(lhs, rhs)
                } else {
                    self.builder.ins().isub(lhs, rhs)
                }
            }
            BinOp::Mul => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    self.builder.ins().fmul(lhs, rhs)
                } else {
                    self.builder.ins().imul(lhs, rhs)
                }
            }
            BinOp::Div => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    self.builder.ins().fdiv(lhs, rhs)
                } else {
                    self.builder.ins().sdiv(lhs, rhs)
                }
            }
            BinOp::Rem => {
                let rhs = self.emit_expr(&b.rhs);
                self.builder.ins().srem(lhs, rhs)
            }
            BinOp::Lt => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::LessThan, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedLessThan, lhs, rhs)
                }
            }
            BinOp::Le => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::LessThanOrEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedLessThanOrEqual, lhs, rhs)
                }
            }
            BinOp::Gt => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::GreaterThan, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedGreaterThan, lhs, rhs)
                }
            }
            BinOp::Ge => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::GreaterThanOrEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedGreaterThanOrEqual, lhs, rhs)
                }
            }
            BinOp::Eq => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::Equal, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::Equal, lhs, rhs)
                }
            }
            BinOp::Ne => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::NotEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::NotEqual, lhs, rhs)
                }
            }
        }
    }

    fn emit_short_circuit_and(
        &mut self,
        lhs: cranelift_codegen::ir::Value,
        rhs_expr: &Expr,
    ) -> cranelift_codegen::ir::Value {
        let result_var = self.builder.declare_var(types::I8);
        let rhs_block = self.builder.create_block();
        let false_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(lhs, rhs_block, &[], false_block, &[]);

        self.builder.switch_to_block(rhs_block);
        self.builder.seal_block(rhs_block);
        let rhs = self.emit_expr(rhs_expr);
        self.builder.def_var(result_var, rhs);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(false_block);
        self.builder.seal_block(false_block);
        let false_value = self.builder.ins().iconst(types::I8, 0);
        self.builder.def_var(result_var, false_value);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    fn emit_short_circuit_or(
        &mut self,
        lhs: cranelift_codegen::ir::Value,
        rhs_expr: &Expr,
    ) -> cranelift_codegen::ir::Value {
        let result_var = self.builder.declare_var(types::I8);
        let true_block = self.builder.create_block();
        let rhs_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(lhs, true_block, &[], rhs_block, &[]);

        self.builder.switch_to_block(true_block);
        self.builder.seal_block(true_block);
        let true_value = self.builder.ins().iconst(types::I8, 1);
        self.builder.def_var(result_var, true_value);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(rhs_block);
        self.builder.seal_block(rhs_block);
        let rhs = self.emit_expr(rhs_expr);
        self.builder.def_var(result_var, rhs);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    fn emit_unary(&mut self, u: &UnaryExpr) -> cranelift_codegen::ir::Value {
        let val = self.emit_expr(&u.expr);
        let ty = ast_type_of_expr(&u.expr, &self.vars, self.func_return_types);
        match &u.op {
            UnaryOp::Neg => {
                if ty == Type::F64 {
                    self.builder.ins().fneg(val)
                } else {
                    self.builder.ins().ineg(val)
                }
            }
            UnaryOp::Not => {
                let one = self.builder.ins().iconst(types::I8, 1);
                self.builder.ins().bxor(val, one)
            }
        }
    }

    fn emit_call(&mut self, c: &CallExpr) -> cranelift_codegen::ir::Value {
        if c.callee == "format" {
            return self.emit_format_call(c);
        }

        // Direct call to a known function.
        if let Some(&fid) = self.func_ids.get(&c.callee) {
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let modes = self.func_param_modes.get(&c.callee).cloned();
            let args = self.emit_call_args(modes.as_deref(), &c.args);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            return if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
        }

        if let Some(runtime_name) = builtin_call_runtime_name(&c.callee) {
            let fid = self.func_ids[runtime_name];
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let args: Vec<_> = c.args.iter().map(|a| self.emit_expr(&a.expr)).collect();
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            return if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
        }

        // Indirect call through a function-value local variable.
        if let Some(storage) = self.vars.get(&c.callee).cloned() {
            if let Type::Fn(param_types, ret_type) = storage.ty().clone() {
                let callee_val = self.load_var(&storage);
                let args: Vec<_> = c.args.iter().map(|a| self.emit_expr(&a.expr)).collect();

                // Build the Cranelift signature matching the function type.
                let mut sig = self.module.make_signature();
                for pt in &param_types {
                    sig.params.push(AbiParam::new(clif_type(pt)));
                }
                let ret_clif = clif_type(&ret_type);
                let has_return = *ret_type != Type::Void;
                if has_return {
                    sig.returns.push(AbiParam::new(ret_clif));
                }
                let sig_ref = self.builder.import_signature(sig);
                let call = self.builder.ins().call_indirect(sig_ref, callee_val, &args);
                let results = self.builder.inst_results(call);
                return if results.is_empty() {
                    self.builder.ins().iconst(types::I8, 0)
                } else {
                    results[0]
                };
            }
        }

        // Should not reach here after type checking.
        self.builder.ins().iconst(types::I64, 0)
    }

    fn emit_call_args(
        &mut self,
        modes: Option<&[ParamMode]>,
        args: &[CallArg],
    ) -> Vec<cranelift_codegen::ir::Value> {
        args.iter()
            .enumerate()
            .map(|(idx, arg)| match modes.and_then(|modes| modes.get(idx)) {
                Some(ParamMode::Reference { .. }) => self.emit_reference_arg_address(arg),
                _ => self.emit_expr(&arg.expr),
            })
            .collect()
    }

    fn emit_reference_arg_address(&mut self, arg: &CallArg) -> cranelift_codegen::ir::Value {
        if let Expr::Var(name, _) = &arg.expr {
            let storage = self.vars.get(name.as_str()).cloned();
            match storage {
                Some(VarStorage::Stack { slot, .. }) => {
                    let ptr_ty = self.module.target_config().pointer_type();
                    return self.builder.ins().stack_addr(ptr_ty, slot, 0);
                }
                Some(VarStorage::Value { var, ty }) => {
                    // Lazy promotion: this variable is being passed by &mut for the first
                    // time.  Promote it from a Cranelift SSA variable to a stack slot so
                    // the callee can write through the pointer and future reads see the
                    // updated value.
                    let ptr_ty = self.module.target_config().pointer_type();
                    let val = self.builder.use_var(var);
                    let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot, 8, 0,
                    ));
                    self.builder.ins().stack_store(val, slot, 0);
                    let ty_clone = ty.clone();
                    self.vars.insert(name.clone(), VarStorage::Stack { slot, ty: ty_clone });
                    return self.builder.ins().stack_addr(ptr_ty, slot, 0);
                }
                Some(VarStorage::ReferencePtr { var, .. }) => {
                    return self.builder.use_var(var);
                }
                None => {}
            }
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    fn emit_format_call(&mut self, c: &CallExpr) -> cranelift_codegen::ir::Value {
        let runtime_name = match c.args.first().map(|arg| &arg.expr) {
            Some(Expr::String(spec, _)) => match spec.as_str() {
                "{:.17g}" => "willow_format_f64_17g",
                "{:.16f}" => "willow_format_f64_16f",
                "{:.6f}" => "willow_format_f64_6f",
                _ => "",
            },
            _ => "",
        };
        if runtime_name.is_empty() || c.args.len() < 2 {
            return self.builder.ins().iconst(types::I64, 0);
        }

        let value = self.emit_expr(&c.args[1].expr);
        let fid = self.func_ids[runtime_name];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[value]);
        self.builder.inst_results(call)[0]
    }

    fn emit_object_literal(&mut self, o: &ObjectLiteralExpr) -> cranelift_codegen::ir::Value {
        let layout = match self.class_layouts.get(&o.class).cloned() {
            Some(l) => l,
            None => return self.builder.ins().iconst(types::I64, 0),
        };
        let size = layout.len() as i64 * 8;
        let size_val = self.builder.ins().iconst(types::I64, size);
        let ref_mask = gc_ref_mask_for_layout(&layout);
        let ref_mask_val = self.builder.ins().iconst(types::I64, ref_mask as i64);
        let alloc_id = self.func_ids["willow_alloc_typed"];
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self
            .builder
            .ins()
            .call(alloc_ref, &[size_val, ref_mask_val]);
        let ptr = self.builder.inst_results(call)[0];

        for field in &o.fields {
            if let Some(idx) = layout.iter().position(|(n, _)| n == &field.name) {
                let offset = idx as i32 * 8;
                let val = self.emit_expr(&field.value);
                self.builder.ins().store(MemFlags::new(), val, ptr, offset);
            }
        }
        ptr
    }

    /// Emit a nil pointer check in debug builds.
    /// If `ptr` is null at runtime, calls `willow_nil_deref` with source location and
    /// `context` (field or method name) then traps. Otherwise execution continues.
    fn emit_nil_check(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        span: crate::diagnostics::Span,
        context: &str,
    ) {
        let zero = self.builder.ins().iconst(types::I64, 0);
        let is_nil = self.builder.ins().icmp(IntCC::Equal, ptr, zero);

        let nil_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_nil, nil_block, &[], ok_block, &[]);

        // ── nil branch: report and abort ──────────────────────────────────────
        self.builder.switch_to_block(nil_block);
        self.builder.seal_block(nil_block);

        let source_file = self.source_file.to_string();
        let context_owned = context.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let ctx_ptr = self.emit_string_literal(&context_owned);
        let line_val = self.builder.ins().iconst(types::I32, span.line as i64);
        let col_val = self.builder.ins().iconst(types::I32, span.col as i64);

        let nil_deref_id = self.func_ids["willow_nil_deref"];
        let nil_deref_ref = self
            .module
            .declare_func_in_func(nil_deref_id, self.builder.func);
        self.builder
            .ins()
            .call(nil_deref_ref, &[file_ptr, line_val, col_val, ctx_ptr]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        // ── ok branch: continue ───────────────────────────────────────────────
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
    }

    fn emit_field_access(&mut self, obj: &Expr, field_name: &str) -> cranelift_codegen::ir::Value {
        let ptr = self.emit_expr(obj);

        // Debug build: guard against nil dereference with a source-aware runtime error.
        if self.build_mode == BuildMode::Debug {
            let span = obj.span();
            self.emit_nil_check(ptr, span, field_name);
        }

        let obj_type = self.ast_type_of(obj);
        if let Some(class_name) = class_name_for_object_type(&obj_type) {
            if let Some(layout) = self.class_layouts.get(&class_name).cloned() {
                if let Some(idx) = layout.iter().position(|(n, _)| n == field_name) {
                    let offset = idx as i32 * 8;
                    let (_, field_ty) = &layout[idx];
                    let load_ty = clif_type(field_ty);
                    return self
                        .builder
                        .ins()
                        .load(load_ty, MemFlags::new(), ptr, offset);
                }
            }
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    fn emit_method_call(&mut self, m: &MethodCallExpr) -> cranelift_codegen::ir::Value {
        let self_ptr = self.emit_expr(&m.object);
        let obj_type = self.ast_type_of(&m.object);

        if m.method == "join" {
            if let Some(result_ty) = join_handle_result_type(&obj_type) {
                // Wait for the spawned task to complete
                let join_fid = self.func_ids["willow_task_join"];
                let join_fref = self
                    .module
                    .declare_func_in_func(join_fid, self.builder.func);
                self.builder.ins().call(join_fref, &[self_ptr]);

                // Load the result from data_ptr + 0 (if non-void)
                if result_ty == Type::Void {
                    return self.builder.ins().iconst(types::I8, 0);
                }
                let clif_ret_ty = clif_type(&result_ty);
                return self
                    .builder
                    .ins()
                    .load(clif_ret_ty, MemFlags::trusted(), self_ptr, 0i32);
            }
        }

        if let Some(element_ty) = channel_element_type(&obj_type) {
            return self.emit_channel_method_call(self_ptr, &element_ty, m);
        }

        // Debug build: guard against nil dereference with a source-aware runtime error.
        if self.build_mode == BuildMode::Debug {
            let span = m.object.span();
            self.emit_nil_check(self_ptr, span, &m.method.clone());
        }

        if let Some(class_name) = class_name_for_object_type(&obj_type) {
            let mangled = format!("{}__{}", class_name, m.method);
            if let Some(&func_id) = self.func_ids.get(&mangled) {
                let func_ref = self.module.declare_func_in_func(func_id, self.builder.func);
                let mut call_args = vec![self_ptr];
                let modes = self.func_param_modes.get(&mangled).cloned();
                call_args.extend(self.emit_call_args(modes.as_deref(), &m.args));
                let call = self.builder.ins().call(func_ref, &call_args);
                let ret_type = self
                    .func_return_types
                    .get(&mangled)
                    .cloned()
                    .unwrap_or(Type::Void);
                if ret_type != Type::Void {
                    return self.builder.inst_results(call)[0];
                } else {
                    return self.builder.ins().iconst(types::I64, 0);
                }
            }
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    fn emit_channel_method_call(
        &mut self,
        channel_ptr: cranelift_codegen::ir::Value,
        element_ty: &Type,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        match m.method.as_str() {
            "send" => {
                if let Some(arg) = m.args.first() {
                    let runtime_name =
                        format!("willow_channel_send_{}", channel_runtime_suffix(element_ty));
                    let fid = self.func_ids[&runtime_name];
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    let value = self.emit_expr(&arg.expr);
                    self.builder.ins().call(fref, &[channel_ptr, value]);
                }
                self.builder.ins().iconst(types::I8, 0)
            }
            "recv" => {
                let runtime_name =
                    format!("willow_channel_recv_{}", channel_runtime_suffix(element_ty));
                let fid = self.func_ids[&runtime_name];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                let call = self.builder.ins().call(fref, &[channel_ptr]);
                self.builder.inst_results(call)[0]
            }
            "close" => {
                let fid = self.func_ids["willow_channel_close"];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[channel_ptr]);
                self.builder.ins().iconst(types::I8, 0)
            }
            _ => self.builder.ins().iconst(types::I64, 0),
        }
    }

    fn emit_spawn(&mut self, s: &SpawnExpr) -> cranelift_codegen::ir::Value {
        // Prefer runtime-based spawn for named functions with a pre-compiled trampoline.
        if let Some(tramp_name) = self.spawn_tramp_names.get(&s.span).cloned() {
            if let Some(&tramp_fid) = self.func_ids.get(&tramp_name) {
                let ret_ty = self
                    .func_return_types
                    .get(&s.callee)
                    .cloned()
                    .unwrap_or(Type::Void);
                let result_slot_size: i64 = if ret_ty == Type::Void { 0 } else { 8 };
                let args_size: i64 = s.args.len() as i64 * 8;
                let data_size = result_slot_size + args_size;

                // Allocate task data area: willow_task_alloc(data_size) -> data_ptr
                let alloc_fid = self.func_ids["willow_task_alloc"];
                let alloc_fref = self
                    .module
                    .declare_func_in_func(alloc_fid, self.builder.func);
                let size_val = self.builder.ins().iconst(types::I64, data_size);
                let alloc_call = self.builder.ins().call(alloc_fref, &[size_val]);
                let data_ptr = self.builder.inst_results(alloc_call)[0];

                // Store each argument into the data area at result_slot_size + i*8.
                // Each slot is 8 bytes; we store the native type (I8/F64/I64).
                let modes = self.func_param_modes.get(&s.callee).cloned();
                let arg_vals = self.emit_call_args(modes.as_deref(), &s.args);
                for (i, val) in arg_vals.into_iter().enumerate() {
                    let offset = result_slot_size as i32 + i as i32 * 8;
                    self.builder
                        .ins()
                        .store(MemFlags::trusted(), val, data_ptr, offset);
                }

                // Get trampoline function address
                let tramp_fref = self
                    .module
                    .declare_func_in_func(tramp_fid, self.builder.func);
                let tramp_ptr = self.builder.ins().func_addr(types::I64, tramp_fref);

                // Debug builds: record the spawn source location for task-aware panic messages.
                if self.build_mode == BuildMode::Debug {
                    let set_loc_fid = self.func_ids["willow_task_set_spawn_location"];
                    let set_loc_fref = self
                        .module
                        .declare_func_in_func(set_loc_fid, self.builder.func);
                    let source_file = self.source_file.to_string();
                    let file_ptr = self.emit_string_literal(&source_file);
                    let line_val = self.builder.ins().iconst(types::I32, s.span.line as i64);
                    let col_val = self.builder.ins().iconst(types::I32, s.span.col as i64);
                    self.builder
                        .ins()
                        .call(set_loc_fref, &[data_ptr, file_ptr, line_val, col_val]);
                }

                // willow_task_spawn(tramp_ptr, data_ptr)
                let spawn_fid = self.func_ids["willow_task_spawn"];
                let spawn_fref = self
                    .module
                    .declare_func_in_func(spawn_fid, self.builder.func);
                self.builder.ins().call(spawn_fref, &[tramp_ptr, data_ptr]);

                return data_ptr;
            }
        }

        // Fallback for function-pointer spawn (not yet runtime-lowered).
        if let Some(storage) = self.vars.get(&s.callee).cloned() {
            if let Type::Fn(param_types, ret_type) = storage.ty().clone() {
                let callee_val = self.load_var(&storage);
                let args: Vec<_> = s.args.iter().map(|arg| self.emit_expr(&arg.expr)).collect();
                let mut sig = self.module.make_signature();
                for pt in &param_types {
                    sig.params.push(AbiParam::new(clif_type(pt)));
                }
                if *ret_type != Type::Void {
                    sig.returns.push(AbiParam::new(clif_type(&ret_type)));
                }
                let sig_ref = self.builder.import_signature(sig);
                let call = self.builder.ins().call_indirect(sig_ref, callee_val, &args);
                let results = self.builder.inst_results(call);
                return if results.is_empty() {
                    self.builder.ins().iconst(types::I8, 0)
                } else {
                    results[0]
                };
            }
        }

        self.builder.ins().iconst(types::I64, 0)
    }

    fn emit_ternary(&mut self, t: &TernaryExpr) -> cranelift_codegen::ir::Value {
        let result_ty = clif_type(&ast_type_of_ternary(t, &self.vars, self.func_return_types));
        let result_var = self.builder.declare_var(result_ty);

        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        let cond = self.emit_expr(&t.condition);
        self.builder
            .ins()
            .brif(cond, then_block, &[], else_block, &[]);

        // then branch — only this runs when condition is true (lazy)
        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        let then_val = self.emit_expr(&t.then_expr);
        self.builder.def_var(result_var, then_val);
        self.builder.ins().jump(merge_block, &[]);

        // else branch — only this runs when condition is false (lazy)
        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        let else_val = self.emit_expr(&t.else_expr);
        self.builder.def_var(result_var, else_val);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    fn emit_match(&mut self, m: &MatchExpr) -> cranelift_codegen::ir::Value {
        let scrutinee = self.emit_expr(&m.scrutinee);

        // Determine the result type from match arms, accounting for pattern bindings.
        let result_ast_type = {
            let mut scratch = self.vars.clone();
            let mut found = Type::I64;
            'outer: for arm in &m.arms {
                match &arm.pattern {
                    Pattern::Binding { name, .. } => {
                        let sty = self.ast_type_of(&m.scrutinee);
                        scratch.insert(name.clone(), VarStorage::Value {
                            var: self.builder.declare_var(clif_type(&sty)), ty: sty,
                        });
                    }
                    Pattern::EnumVariantTuple { enum_name, variant, bindings, .. } => {
                        if let Some(pts) = self.enum_infos.get(enum_name.as_str())
                            .and_then(|e| e.variants.iter().find(|v| v.name == *variant))
                            .map(|v| v.payload_types.clone())
                        {
                            for (name, ty) in bindings.iter().zip(pts.iter()) {
                                scratch.insert(name.clone(), VarStorage::Value {
                                    var: self.builder.declare_var(clif_type(ty)), ty: ty.clone(),
                                });
                            }
                        }
                    }
                    _ => {}
                }
                let ty = match &arm.body {
                    MatchBody::Expr(e) => ast_type_of_expr(e, &scratch, self.func_return_types),
                    MatchBody::Block(_) => Type::Void,
                };
                if ty != Type::Void && ty != Type::Never {
                    found = ty;
                    break 'outer;
                }
            }
            found
        };
        let result_clif_type = clif_type(&result_ast_type);
        let result_var = self.builder.declare_var(result_clif_type);
        let zero = if result_clif_type == types::F64 {
            let bits = self.builder.ins().iconst(types::I64, 0);
            self.builder.ins().bitcast(types::F64, MemFlags::new(), bits)
        } else if result_clif_type == types::I8 {
            self.builder.ins().iconst(types::I8, 0)
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };
        self.builder.def_var(result_var, zero);

        let merge_block = self.builder.create_block();

        let mut remaining = m.arms.as_slice();

        while !remaining.is_empty() {
            let arm = &remaining[0];
            remaining = &remaining[1..];

            let is_last = remaining.is_empty();

            let always_matches = matches!(arm.pattern, Pattern::Wildcard(_) | Pattern::Binding { .. });

            let arm_block = self.builder.create_block();
            let next_block = if always_matches || is_last {
                None
            } else {
                Some(self.builder.create_block())
            };

            if always_matches {
                self.builder.ins().jump(arm_block, &[]);
            } else {
                let cond = self.emit_pattern_check(scrutinee, &arm.pattern);
                let fallthrough = next_block.unwrap_or(merge_block);
                self.builder.ins().brif(cond, arm_block, &[], fallthrough, &[]);
            }

            self.builder.switch_to_block(arm_block);
            self.builder.seal_block(arm_block);

            // For binding patterns, define the variable
            let saved_vars = match &arm.pattern {
                Pattern::Binding { name, .. } => {
                    let var = self.builder.declare_var(types::I64);
                    self.builder.def_var(var, scrutinee);
                    let saved = self.vars.clone();
                    self.vars.insert(name.clone(), VarStorage::Value { var, ty: result_ast_type.clone() });
                    Some(saved)
                }
                Pattern::EnumVariantTuple { enum_name, variant, bindings, .. } => {
                    let saved = self.vars.clone();
                    let payload_types = self.enum_infos.get(enum_name.as_str())
                        .and_then(|e| e.variants.iter().find(|v| v.name == *variant))
                        .map(|v| v.payload_types.clone())
                        .unwrap_or_default();
                    for (i, (binding, payload_ty)) in bindings.iter().zip(payload_types.iter()).enumerate() {
                        let offset = (1 + i) as i32 * 8;
                        let clif_ty = clif_type(payload_ty);
                        let raw = self.builder.ins().load(types::I64, MemFlags::new(), scrutinee, offset);
                        let val = if clif_ty == types::F64 {
                            self.builder.ins().bitcast(types::F64, MemFlags::new(), raw)
                        } else if clif_ty == types::I8 {
                            self.builder.ins().ireduce(types::I8, raw)
                        } else {
                            raw
                        };
                        let var = self.builder.declare_var(clif_ty);
                        self.builder.def_var(var, val);
                        self.vars.insert(binding.clone(), VarStorage::Value { var, ty: payload_ty.clone() });
                    }
                    Some(saved)
                }
                _ => None,
            };

            let outer_terminated = self.terminated;
            self.terminated = false;
            let arm_val = self.emit_match_body(&arm.body);

            if !self.terminated {
                self.builder.def_var(result_var, arm_val);
                self.builder.ins().jump(merge_block, &[]);
            }
            self.terminated = outer_terminated;

            if let Some(saved) = saved_vars {
                self.vars = saved;
            }

            if let Some(next) = next_block {
                self.builder.switch_to_block(next);
                self.builder.seal_block(next);
            }

            if always_matches {
                break;
            }
        }

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    fn emit_match_body(&mut self, body: &MatchBody) -> cranelift_codegen::ir::Value {
        match body {
            MatchBody::Expr(expr) => self.emit_expr(expr),
            MatchBody::Block(block) => {
                self.emit_block(block);
                self.builder.ins().iconst(types::I64, 0)
            }
        }
    }

    fn emit_pattern_check(
        &mut self,
        scrutinee: cranelift_codegen::ir::Value,
        pattern: &Pattern,
    ) -> cranelift_codegen::ir::Value {
        match pattern {
            Pattern::Wildcard(_) | Pattern::Binding { .. } => {
                self.builder.ins().iconst(types::I8, 1)
            }
            Pattern::LiteralBool(b, _) => {
                let expected = self.builder.ins().iconst(types::I8, if *b { 1 } else { 0 });
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            Pattern::LiteralInt(n, _) => {
                let expected = self.builder.ins().iconst(types::I64, *n);
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            Pattern::EnumVariant { enum_name, variant, .. } => {
                let tag = self.enum_variant_tag(enum_name, variant);
                let expected = self.builder.ins().iconst(types::I64, tag);
                if self.enum_is_gc_object_type(enum_name) {
                    let actual_tag = self.emit_load_enum_tag(scrutinee);
                    self.builder.ins().icmp(IntCC::Equal, actual_tag, expected)
                } else {
                    self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
                }
            }
            Pattern::EnumVariantTuple { enum_name, variant, .. } => {
                let tag = self.enum_variant_tag(enum_name, variant);
                let expected = self.builder.ins().iconst(types::I64, tag);
                let actual_tag = self.emit_load_enum_tag(scrutinee);
                self.builder.ins().icmp(IntCC::Equal, actual_tag, expected)
            }
        }
    }

    fn enum_variant_tag(&self, enum_name: &str, variant: &str) -> i64 {
        self.enum_infos
            .get(enum_name)
            .and_then(|e| e.variants.iter().find(|v| v.name == variant))
            .map(|v| v.tag)
            .unwrap_or(0)
    }

    fn enum_is_gc_object_type(&self, enum_name: &str) -> bool {
        self.enum_infos
            .get(enum_name)
            .map(|e| e.variants.iter().any(|v| !v.payload_types.is_empty()))
            .unwrap_or(false)
    }

    fn emit_load_enum_tag(&mut self, ptr: cranelift_codegen::ir::Value) -> cranelift_codegen::ir::Value {
        self.builder.ins().load(types::I64, MemFlags::new(), ptr, 0i32)
    }

    fn emit_enum_variant_alloc(&mut self, tag: i64, args: &[crate::parser::ast::CallArg]) -> cranelift_codegen::ir::Value {
        let field_count = args.len();
        let total_words = 1 + field_count;
        let size = self.builder.ins().iconst(types::I64, (total_words * 8) as i64);
        let mask = self.builder.ins().iconst(types::I64, 0i64);
        let alloc_id = self.func_ids["willow_alloc_typed"];
        let alloc_ref = self.module.declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
        let tag_val = self.builder.ins().iconst(types::I64, tag);
        self.builder.ins().store(MemFlags::new(), tag_val, ptr, 0i32);
        for (i, arg) in args.iter().enumerate() {
            let offset = (1 + i) as i32 * 8;
            let val = self.emit_expr(&arg.expr);
            let val_i64 = if matches!(self.ast_type_of(&arg.expr), Type::F64) {
                self.builder.ins().bitcast(types::I64, MemFlags::new(), val)
            } else {
                val
            };
            self.builder.ins().store(MemFlags::new(), val_i64, ptr, offset);
        }
        ptr
    }

    fn emit_static_call(&mut self, s: &StaticCallExpr) -> cranelift_codegen::ir::Value {
        // Check if class is an enum — handle variant construction
        if let Some(enum_info) = self.enum_infos.get(&s.class).cloned() {
            if let Some(variant) = enum_info.variants.iter().find(|v| v.name == s.method).cloned() {
                if variant.payload_types.is_empty() && !self.enum_is_gc_object_type(&s.class) {
                    return self.builder.ins().iconst(types::I64, variant.tag);
                }
                if variant.payload_types.is_empty() {
                    return self.emit_enum_variant_alloc(variant.tag, &[]);
                }
                return self.emit_enum_variant_alloc(variant.tag, &s.args);
            }
        }

        if s.class == "Channel" && s.method == "new" {
            let fid = self.func_ids["willow_channel_new"];
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[]);
            return self.builder.inst_results(call)[0];
        }

        if s.class == "f64" && s.method == "to_string" {
            let fid = self.func_ids["willow_f64_to_string"];
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let args = self.emit_call_args(None, &s.args);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            return results[0];
        }

        if s.class == "env" {
            let runtime_name = match s.method.as_str() {
                "args_len" => "willow_runtime_args_len",
                "arg" => "willow_runtime_arg",
                "program_name" => "willow_runtime_program_name",
                _ => "",
            };
            if !runtime_name.is_empty() {
                let fid = self.func_ids[runtime_name];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                let args = self.emit_call_args(None, &s.args);
                let call = self.builder.ins().call(fref, &args);
                let results = self.builder.inst_results(call);
                return if results.is_empty() {
                    self.builder.ins().iconst(types::I8, 0)
                } else {
                    results[0]
                };
            }
        }

        // Module call: `math::add(args)` → mangled name `math__add`
        if self.known_modules.contains(&s.class) {
            let mangled = format!("{}__{}", s.class, s.method);
            let fid = match self.func_ids.get(&mangled) {
                Some(&id) => id,
                None => panic!("undefined module function: {}", mangled),
            };
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let modes = self.func_param_modes.get(&mangled).cloned();
            let args = self.emit_call_args(modes.as_deref(), &s.args);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            return if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
        }
        // Class static call: not yet implemented (willow-jbf)
        self.builder.ins().iconst(types::I64, 0)
    }
}

fn fcmp_to_i8(
    builder: &mut FunctionBuilder<'_>,
    cc: FloatCC,
    lhs: cranelift_codegen::ir::Value,
    rhs: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    builder.ins().fcmp(cc, lhs, rhs)
}

fn icmp_to_i8(
    builder: &mut FunctionBuilder<'_>,
    cc: IntCC,
    lhs: cranelift_codegen::ir::Value,
    rhs: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    // icmp returns I8 in cranelift 0.132
    builder.ins().icmp(cc, lhs, rhs)
}

fn clif_type(ty: &Type) -> cranelift_codegen::ir::Type {
    match ty {
        Type::I64 => types::I64,
        Type::F64 => types::F64,
        Type::Bool => types::I8,
        Type::String => types::I64,
        Type::Nil => types::I64,
        Type::Never => types::I64, // bottom type — treated as I64 for codegen purposes
        Type::Array(_) => types::I64,
        // JoinHandle<T> is a pointer to a task data area — always I64.
        Type::Generic(name, _) if name == "JoinHandle" => types::I64,
        // Future<T> is an opaque runtime future pointer.
        Type::Generic(name, args) if name == "Future" && args.len() == 1 => types::I64,
        Type::Generic(_, _) => types::I64,
        Type::Nullable(_) => types::I64,
        Type::Fn(_, _) => types::I64, // function pointer (pointer-sized)
        Type::Named(_) => types::I64,
        Type::Void => types::I8,
    }
}

fn join_handle_result_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "JoinHandle" && args.len() == 1 => {
            Some(args[0].clone())
        }
        _ => None,
    }
}

fn future_output_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "Future" && args.len() == 1 => Some(args[0].clone()),
        _ => None,
    }
}

fn function_call_return_type(f: &FunctionDecl) -> Type {
    if f.is_async {
        Type::Generic("Future".to_string(), vec![f.return_type.clone()])
    } else {
        f.return_type.clone()
    }
}

fn method_call_return_type(m: &MethodDecl) -> Type {
    if m.is_async {
        Type::Generic("Future".to_string(), vec![m.return_type.clone()])
    } else {
        m.return_type.clone()
    }
}

fn future_ready_runtime_name(ty: &Type) -> &'static str {
    match ty {
        Type::Void => "willow_future_ready_void",
        Type::I64 => "willow_future_ready_i64",
        Type::Bool => "willow_future_ready_bool",
        Type::F64 => "willow_future_ready_f64",
        _ => "willow_future_ready_ptr",
    }
}

fn future_await_runtime_name(ty: &Type) -> &'static str {
    match ty {
        Type::Void => "willow_future_await_void",
        Type::I64 => "willow_future_await_i64",
        Type::Bool => "willow_future_await_bool",
        Type::F64 => "willow_future_await_f64",
        _ => "willow_future_await_ptr",
    }
}

fn channel_element_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "Channel" && args.len() == 1 => Some(args[0].clone()),
        _ => None,
    }
}

fn channel_runtime_suffix(ty: &Type) -> &'static str {
    match ty {
        Type::I64 => "i64",
        Type::Bool => "bool",
        Type::F64 => "f64",
        _ => "ptr",
    }
}

fn param_abi_type(
    param: &Param,
    pointer_type: cranelift_codegen::ir::Type,
) -> cranelift_codegen::ir::Type {
    match &param.mode {
        ParamMode::Reference { .. } => pointer_type,
        ParamMode::Value => clif_type(&param.ty),
    }
}

fn is_gc_managed(ty: &Type) -> bool {
    match ty {
        Type::Named(_) => true,
        Type::Nullable(inner) => is_gc_managed(inner),
        _ => false,
    }
}

fn gc_ref_mask_for_layout(layout: &[(String, Type)]) -> u64 {
    layout
        .iter()
        .take(64)
        .enumerate()
        .fold(0u64, |mask, (idx, (_, ty))| {
            if is_gc_managed(ty) {
                mask | (1u64 << idx)
            } else {
                mask
            }
        })
}

fn class_name_for_object_type(ty: &Type) -> Option<String> {
    match ty {
        Type::Named(name) => Some(name.clone()),
        Type::Nullable(inner) => class_name_for_object_type(inner),
        _ => None,
    }
}

fn user_function_symbol(name: &str) -> String {
    if name == "main" {
        USER_MAIN_SYMBOL.to_string()
    } else {
        name.to_string()
    }
}

fn clif_type_of_expr(
    expr: &Expr,
    vars: &HashMap<String, VarStorage>,
    frt: &HashMap<String, Type>,
) -> cranelift_codegen::ir::Type {
    clif_type(&ast_type_of_expr(expr, vars, frt))
}

fn ast_type_of_expr(
    expr: &Expr,
    vars: &HashMap<String, VarStorage>,
    frt: &HashMap<String, Type>,
) -> Type {
    match expr {
        Expr::Integer(_, _) => Type::I64,
        Expr::Float(_, _) => Type::F64,
        Expr::Bool(_, _) => Type::Bool,
        Expr::Nil(_) => Type::Nil,
        Expr::String(_, _) => Type::String,
        Expr::Var(name, _) => vars
            .get(name.as_str())
            .map(|storage| storage.ty().clone())
            .unwrap_or(Type::I64),
        Expr::Binary(b) => match &b.op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                ast_type_of_expr(&b.lhs, vars, frt)
            }
            _ => Type::Bool,
        },
        Expr::Unary(u) => match &u.op {
            UnaryOp::Neg => ast_type_of_expr(&u.expr, vars, frt),
            UnaryOp::Not => Type::Bool,
        },
        Expr::Call(c) => frt
            .get(&c.callee)
            .cloned()
            .or_else(|| builtin_call_return_type(&c.callee))
            .unwrap_or(Type::I64),
        Expr::Print(_, _, _) => Type::Void,
        Expr::Ternary(t) => ast_type_of_ternary(t, vars, frt),
        Expr::Lambda(l) => {
            let params = l
                .params
                .iter()
                .filter_map(|p| p.ty.clone())
                .collect::<Vec<_>>();
            let ret = l.return_type.clone().unwrap_or(Type::I64);
            Type::Fn(params, Box::new(ret))
        }
        Expr::FieldAccess(_, _, _) => Type::Void,
        Expr::MethodCall(m) => {
            let obj_ty = ast_type_of_expr(&m.object, vars, frt);
            if m.method == "join" {
                if let Some(result_ty) = join_handle_result_type(&obj_ty) {
                    return result_ty;
                }
            }
            if m.method == "recv" {
                if let Some(element_ty) = channel_element_type(&obj_ty) {
                    return element_ty;
                }
            }
            Type::Void
        }
        Expr::ObjectLiteral(o) => Type::Named(o.class.clone()),
        Expr::Spawn(s) => {
            let ret_ty = frt.get(&s.callee).cloned().unwrap_or_else(|| {
                vars.get(s.callee.as_str())
                    .and_then(|storage| match storage.ty() {
                        Type::Fn(_, ret) => Some((**ret).clone()),
                        _ => None,
                    })
                    .unwrap_or(Type::Void)
            });
            Type::Generic("JoinHandle".to_string(), vec![ret_ty])
        }
        Expr::Await(a) => future_output_type(&ast_type_of_expr(&a.expr, vars, frt))
            .unwrap_or_else(|| ast_type_of_expr(&a.expr, vars, frt)),
        Expr::Select(_) => Type::Void,
        Expr::StaticCall(s) => {
            if let Some(ty) = builtin_static_return_type(&s.class, &s.type_args, &s.method) {
                return ty;
            }
            // Look up mangled name for module calls.
            let mangled = format!("{}__{}", s.class, s.method);
            frt.get(&mangled)
                .or_else(|| frt.get(&s.method))
                .cloned()
                .unwrap_or(Type::I64)
        }
        Expr::Match(m) => {
            // Return type of first non-void arm body
            for arm in &m.arms {
                let ty = match &arm.body {
                    MatchBody::Expr(e) => ast_type_of_expr(e, vars, frt),
                    MatchBody::Block(_) => Type::Void,
                };
                if ty != Type::Void && ty != Type::Never {
                    return ty;
                }
            }
            Type::I64
        }
    }
}

fn ast_type_of_ternary(
    t: &TernaryExpr,
    vars: &HashMap<String, VarStorage>,
    frt: &HashMap<String, Type>,
) -> Type {
    let then_ty = ast_type_of_expr(&t.then_expr, vars, frt);
    let else_ty = ast_type_of_expr(&t.else_expr, vars, frt);

    if then_ty == else_ty {
        return then_ty;
    }

    match (&then_ty, &else_ty) {
        (Type::Nil, Type::Nil) => Type::Nil,
        (Type::Nullable(_), Type::Nil) => then_ty.clone(),
        (Type::Nil, Type::Nullable(_)) => else_ty.clone(),
        (Type::Nil, other) => Type::Nullable(Box::new(other.clone())),
        (other, Type::Nil) => Type::Nullable(Box::new(other.clone())),
        (Type::Nullable(inner), other) if inner.as_ref() == other => then_ty.clone(),
        (other, Type::Nullable(inner)) if inner.as_ref() == other => else_ty.clone(),
        _ => then_ty.clone(),
    }
}

fn builtin_static_return_type(class: &str, type_args: &[Type], method: &str) -> Option<Type> {
    match (class, method) {
        ("Channel", "new") => Some(Type::Generic(
            "Channel".to_string(),
            vec![type_args.first().cloned().unwrap_or(Type::Void)],
        )),
        ("env", "args_len") => Some(Type::I64),
        ("env", "arg") => Some(Type::String),
        ("env", "program_name") => Some(Type::String),
        ("f64", "to_string") => Some(Type::String),
        _ => None,
    }
}

fn builtin_call_return_type(callee: &str) -> Option<Type> {
    match callee {
        "pow" | "powf" => Some(Type::F64),
        "format" => Some(Type::String),
        "gc_allocated_bytes" => Some(Type::I64),
        "gc_collect" => Some(Type::Void),
        "sleep" => Some(Type::Generic("Future".to_string(), vec![Type::Void])),
        _ => None,
    }
}

fn builtin_call_runtime_name(callee: &str) -> Option<&'static str> {
    match callee {
        "pow" | "powf" => Some("willow_pow_f64"),
        "gc_collect" => Some("willow_gc_collect"),
        "gc_allocated_bytes" => Some("willow_gc_allocated_bytes"),
        "sleep" => Some("willow_runtime_sleep"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_async_codegen_01_sleep_builtin_returns_future_void() {
        assert_eq!(
            builtin_call_return_type("sleep"),
            Some(Type::Generic("Future".to_string(), vec![Type::Void]))
        );
    }

    #[test]
    fn unit_async_codegen_02_sleep_builtin_lowers_to_runtime_sleep() {
        assert_eq!(
            builtin_call_runtime_name("sleep"),
            Some("willow_runtime_sleep")
        );
    }

    #[test]
    fn unit_async_codegen_03_channel_new_returns_channel_void_placeholder() {
        assert_eq!(
            builtin_static_return_type("Channel", &[], "new"),
            Some(Type::Generic("Channel".to_string(), vec![Type::Void]))
        );
    }

    #[test]
    fn unit_async_codegen_06_channel_new_with_type_arg_returns_typed_channel() {
        assert_eq!(
            builtin_static_return_type("Channel", &[Type::I64], "new"),
            Some(Type::Generic("Channel".to_string(), vec![Type::I64]))
        );
    }

    #[test]
    fn unit_async_codegen_04_channel_element_type_extracts_generic_argument() {
        assert_eq!(
            channel_element_type(&Type::Generic("Channel".to_string(), vec![Type::I64])),
            Some(Type::I64)
        );
        assert_eq!(channel_element_type(&Type::I64), None);
    }

    #[test]
    fn unit_async_codegen_05_channel_runtime_suffix_selects_primitive_or_pointer_abi() {
        assert_eq!(channel_runtime_suffix(&Type::I64), "i64");
        assert_eq!(channel_runtime_suffix(&Type::Bool), "bool");
        assert_eq!(channel_runtime_suffix(&Type::F64), "f64");
        assert_eq!(channel_runtime_suffix(&Type::String), "ptr");
        assert_eq!(
            channel_runtime_suffix(&Type::Named("Node".to_string())),
            "ptr"
        );
    }

    #[test]
    fn unit_async_codegen_07_future_uses_runtime_pointer_abi() {
        assert_eq!(
            clif_type(&Type::Generic("Future".to_string(), vec![Type::I64])),
            types::I64
        );
        assert_eq!(
            clif_type(&Type::Generic("Future".to_string(), vec![Type::Void])),
            types::I64
        );
    }

    #[test]
    fn unit_async_codegen_08_async_function_call_returns_future_type() {
        let function = FunctionDecl {
            name: "work".to_string(),
            public: false,
            is_async: true,
            params: Vec::new(),
            return_type: Type::I64,
            body: Block {
                stmts: Vec::new(),
                span: crate::diagnostics::Span::dummy(),
            },
            span: crate::diagnostics::Span::dummy(),
        };

        assert_eq!(
            function_call_return_type(&function),
            Type::Generic("Future".to_string(), vec![Type::I64])
        );
    }

    #[test]
    fn unit_async_codegen_09_future_ready_runtime_selects_by_value_type() {
        assert_eq!(
            future_ready_runtime_name(&Type::Void),
            "willow_future_ready_void"
        );
        assert_eq!(
            future_ready_runtime_name(&Type::I64),
            "willow_future_ready_i64"
        );
        assert_eq!(
            future_ready_runtime_name(&Type::Bool),
            "willow_future_ready_bool"
        );
        assert_eq!(
            future_ready_runtime_name(&Type::F64),
            "willow_future_ready_f64"
        );
        assert_eq!(
            future_ready_runtime_name(&Type::String),
            "willow_future_ready_ptr"
        );
    }

    #[test]
    fn unit_async_codegen_10_future_await_runtime_selects_by_output_type() {
        assert_eq!(
            future_await_runtime_name(&Type::Void),
            "willow_future_await_void"
        );
        assert_eq!(
            future_await_runtime_name(&Type::I64),
            "willow_future_await_i64"
        );
        assert_eq!(
            future_await_runtime_name(&Type::Bool),
            "willow_future_await_bool"
        );
        assert_eq!(
            future_await_runtime_name(&Type::F64),
            "willow_future_await_f64"
        );
        assert_eq!(
            future_await_runtime_name(&Type::Named("Node".to_string())),
            "willow_future_await_ptr"
        );
    }
}

// ── String literal collection helpers ─────────────────────────────────────────

fn collect_string_literals_in_program(program: &Program) -> Vec<String> {
    let mut out = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) => collect_string_literals_in_block(&f.body, &mut out),
            Item::Class(c) => {
                for method in &c.methods {
                    collect_string_literals_in_block(&method.body, &mut out);
                }
            }
            Item::Enum(_) => {}
        }
    }
    out
}

fn collect_string_literals_in_block(block: &Block, out: &mut Vec<String>) {
    for stmt in &block.stmts {
        collect_string_literals_in_stmt(stmt, out);
    }
}

fn collect_string_literals_in_stmt(stmt: &Stmt, out: &mut Vec<String>) {
    match stmt {
        Stmt::Let(s) => collect_string_literals_in_expr(&s.init, out),
        Stmt::Assign(s) => collect_string_literals_in_expr(&s.value, out),
        Stmt::If(s) => {
            collect_string_literals_in_expr(&s.cond, out);
            collect_string_literals_in_block(&s.then_block, out);
            if let Some(else_block) = &s.else_block {
                collect_string_literals_in_block(else_block, out);
            }
        }
        Stmt::While(s) => {
            collect_string_literals_in_expr(&s.cond, out);
            collect_string_literals_in_block(&s.body, out);
        }
        Stmt::Return(s) => {
            if let Some(value) = &s.value {
                collect_string_literals_in_expr(value, out);
            }
        }
        Stmt::Expr(s) => collect_string_literals_in_expr(&s.expr, out),
    }
}

fn collect_string_literals_in_expr(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::String(value, _) => out.push(value.clone()),
        Expr::Binary(b) => {
            collect_string_literals_in_expr(&b.lhs, out);
            collect_string_literals_in_expr(&b.rhs, out);
        }
        Expr::Unary(u) => collect_string_literals_in_expr(&u.expr, out),
        Expr::Call(c) => {
            for arg in &c.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
        }
        Expr::FieldAccess(obj, _, _) => collect_string_literals_in_expr(obj, out),
        Expr::MethodCall(m) => {
            collect_string_literals_in_expr(&m.object, out);
            for arg in &m.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
        }
        Expr::StaticCall(s) => {
            for arg in &s.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
        }
        Expr::ObjectLiteral(o) => {
            for field in &o.fields {
                collect_string_literals_in_expr(&field.value, out);
            }
        }
        Expr::Spawn(s) => {
            for arg in &s.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
        }
        Expr::Await(a) => collect_string_literals_in_expr(&a.expr, out),
        Expr::Select(_) => {}
        Expr::Print(arg, _, _) => collect_string_literals_in_expr(arg, out),
        Expr::Ternary(t) => {
            collect_string_literals_in_expr(&t.condition, out);
            collect_string_literals_in_expr(&t.then_expr, out);
            collect_string_literals_in_expr(&t.else_expr, out);
        }
        Expr::Lambda(l) => match &l.body {
            LambdaBody::Expr(e) => collect_string_literals_in_expr(e, out),
            LambdaBody::Block(b) => collect_string_literals_in_block(b, out),
        },
        Expr::Match(m) => {
            collect_string_literals_in_expr(&m.scrutinee, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_string_literals_in_expr(e, out),
                    MatchBody::Block(b) => collect_string_literals_in_block(b, out),
                }
            }
        }
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::Var(_, _) => {}
    }
}

// ── Lambda collection helpers ─────────────────────────────────────────────────

fn collect_lambdas_in_program(program: &Program) -> Vec<(String, LambdaExpr)> {
    let mut out = Vec::new();
    let mut counter = 0usize;
    for item in &program.items {
        match item {
            Item::Function(f) => collect_lambdas_in_block(&f.body, &mut counter, &mut out),
            Item::Class(c) => {
                for m in &c.methods {
                    collect_lambdas_in_block(&m.body, &mut counter, &mut out);
                }
            }
            Item::Enum(_) => {}
        }
    }
    out
}

fn collect_lambdas_in_block(
    block: &Block,
    counter: &mut usize,
    out: &mut Vec<(String, LambdaExpr)>,
) {
    for stmt in &block.stmts {
        collect_lambdas_in_stmt(stmt, counter, out);
    }
}

fn collect_lambdas_in_stmt(stmt: &Stmt, counter: &mut usize, out: &mut Vec<(String, LambdaExpr)>) {
    match stmt {
        Stmt::Let(s) => collect_lambdas_in_expr(&s.init, counter, out),
        Stmt::Assign(s) => collect_lambdas_in_expr(&s.value, counter, out),
        Stmt::If(s) => {
            collect_lambdas_in_expr(&s.cond, counter, out);
            collect_lambdas_in_block(&s.then_block, counter, out);
            if let Some(eb) = &s.else_block {
                collect_lambdas_in_block(eb, counter, out);
            }
        }
        Stmt::While(s) => {
            collect_lambdas_in_expr(&s.cond, counter, out);
            collect_lambdas_in_block(&s.body, counter, out);
        }
        Stmt::Return(s) => {
            if let Some(v) = &s.value {
                collect_lambdas_in_expr(v, counter, out);
            }
        }
        Stmt::Expr(s) => collect_lambdas_in_expr(&s.expr, counter, out),
    }
}

fn collect_lambdas_in_expr(expr: &Expr, counter: &mut usize, out: &mut Vec<(String, LambdaExpr)>) {
    match expr {
        Expr::Lambda(l) => {
            // Recurse into the lambda body first so nested lambdas get lower IDs.
            match &l.body {
                LambdaBody::Block(b) => collect_lambdas_in_block(b, counter, out),
                LambdaBody::Expr(e) => collect_lambdas_in_expr(e, counter, out),
            }
            let name = format!("__lambda_{}", *counter);
            *counter += 1;
            out.push((name, *l.clone()));
        }
        Expr::Call(c) => {
            for arg in &c.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::Binary(b) => {
            collect_lambdas_in_expr(&b.lhs, counter, out);
            collect_lambdas_in_expr(&b.rhs, counter, out);
        }
        Expr::Unary(u) => collect_lambdas_in_expr(&u.expr, counter, out),
        Expr::Ternary(t) => {
            collect_lambdas_in_expr(&t.condition, counter, out);
            collect_lambdas_in_expr(&t.then_expr, counter, out);
            collect_lambdas_in_expr(&t.else_expr, counter, out);
        }
        Expr::Print(e, _, _) => collect_lambdas_in_expr(e, counter, out),
        Expr::StaticCall(s) => {
            for arg in &s.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::ObjectLiteral(o) => {
            for field in &o.fields {
                collect_lambdas_in_expr(&field.value, counter, out);
            }
        }
        Expr::Spawn(s) => {
            for arg in &s.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::Await(a) => collect_lambdas_in_expr(&a.expr, counter, out),
        Expr::Select(_) => {}
        Expr::MethodCall(m) => {
            collect_lambdas_in_expr(&m.object, counter, out);
            for arg in &m.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::FieldAccess(e, _, _) => collect_lambdas_in_expr(e, counter, out),
        Expr::Match(m) => {
            collect_lambdas_in_expr(&m.scrutinee, counter, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_lambdas_in_expr(e, counter, out),
                    MatchBody::Block(b) => collect_lambdas_in_block(b, counter, out),
                }
            }
        }
        _ => {}
    }
}

// ── Spawn-site collection helpers ────────────────────────────────────────────
// Returns (span, tramp_name, callee_name) for every Expr::Spawn in the program.

fn collect_spawns_in_program(program: &Program) -> Vec<(crate::diagnostics::Span, String, String)> {
    let mut out = Vec::new();
    let mut counter = 0usize;
    for item in &program.items {
        match item {
            Item::Function(f) => collect_spawns_in_block(&f.body, &mut counter, &mut out),
            Item::Class(c) => {
                for m in &c.methods {
                    collect_spawns_in_block(&m.body, &mut counter, &mut out);
                }
            }
            Item::Enum(_) => {}
        }
    }
    out
}

fn collect_spawns_in_block(
    block: &Block,
    counter: &mut usize,
    out: &mut Vec<(crate::diagnostics::Span, String, String)>,
) {
    for stmt in &block.stmts {
        collect_spawns_in_stmt(stmt, counter, out);
    }
}

fn collect_spawns_in_stmt(
    stmt: &Stmt,
    counter: &mut usize,
    out: &mut Vec<(crate::diagnostics::Span, String, String)>,
) {
    match stmt {
        Stmt::Let(s) => collect_spawns_in_expr(&s.init, counter, out),
        Stmt::Assign(s) => collect_spawns_in_expr(&s.value, counter, out),
        Stmt::If(s) => {
            collect_spawns_in_expr(&s.cond, counter, out);
            collect_spawns_in_block(&s.then_block, counter, out);
            if let Some(eb) = &s.else_block {
                collect_spawns_in_block(eb, counter, out);
            }
        }
        Stmt::While(s) => {
            collect_spawns_in_expr(&s.cond, counter, out);
            collect_spawns_in_block(&s.body, counter, out);
        }
        Stmt::Return(s) => {
            if let Some(v) = &s.value {
                collect_spawns_in_expr(v, counter, out);
            }
        }
        Stmt::Expr(s) => collect_spawns_in_expr(&s.expr, counter, out),
    }
}

fn collect_spawns_in_expr(
    expr: &Expr,
    counter: &mut usize,
    out: &mut Vec<(crate::diagnostics::Span, String, String)>,
) {
    match expr {
        Expr::Spawn(s) => {
            let tramp_name = format!("__willow_spawn_tramp_{}", *counter);
            *counter += 1;
            out.push((s.span, tramp_name, s.callee.clone()));
            for arg in &s.args {
                collect_spawns_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::Call(c) => {
            for arg in &c.args {
                collect_spawns_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::Binary(b) => {
            collect_spawns_in_expr(&b.lhs, counter, out);
            collect_spawns_in_expr(&b.rhs, counter, out);
        }
        Expr::Unary(u) => collect_spawns_in_expr(&u.expr, counter, out),
        Expr::Ternary(t) => {
            collect_spawns_in_expr(&t.condition, counter, out);
            collect_spawns_in_expr(&t.then_expr, counter, out);
            collect_spawns_in_expr(&t.else_expr, counter, out);
        }
        Expr::Print(e, _, _) => collect_spawns_in_expr(e, counter, out),
        Expr::Await(a) => collect_spawns_in_expr(&a.expr, counter, out),
        Expr::MethodCall(m) => {
            collect_spawns_in_expr(&m.object, counter, out);
            for arg in &m.args {
                collect_spawns_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::FieldAccess(e, _, _) => collect_spawns_in_expr(e, counter, out),
        Expr::StaticCall(s) => {
            for arg in &s.args {
                collect_spawns_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::ObjectLiteral(o) => {
            for field in &o.fields {
                collect_spawns_in_expr(&field.value, counter, out);
            }
        }
        Expr::Lambda(l) => match &l.body {
            LambdaBody::Block(b) => collect_spawns_in_block(b, counter, out),
            LambdaBody::Expr(e) => collect_spawns_in_expr(e, counter, out),
        },
        Expr::Match(m) => {
            collect_spawns_in_expr(&m.scrutinee, counter, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_spawns_in_expr(e, counter, out),
                    MatchBody::Block(b) => collect_spawns_in_block(b, counter, out),
                }
            }
        }
        Expr::Select(_)
        | Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::String(_, _)
        | Expr::Var(_, _) => {}
    }
}

// ── Nil-check string pre-scan ─────────────────────────────────────────────────
// Collect all field names and method names referenced in the program so their
// string literals can be pre-declared before any function is compiled.

fn collect_nil_check_names(program: &Program) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for item in &program.items {
        match item {
            Item::Function(f) => collect_nil_check_names_in_block(&f.body, &mut out),
            Item::Class(c) => {
                for m in &c.methods {
                    collect_nil_check_names_in_block(&m.body, &mut out);
                }
            }
            Item::Enum(_) => {
            }
        }
    }
    out
}

fn collect_nil_check_names_in_block(block: &Block, out: &mut std::collections::HashSet<String>) {
    for stmt in &block.stmts {
        collect_nil_check_names_in_stmt(stmt, out);
    }
}

fn collect_nil_check_names_in_stmt(stmt: &Stmt, out: &mut std::collections::HashSet<String>) {
    match stmt {
        Stmt::Let(s) => collect_nil_check_names_in_expr(&s.init, out),
        Stmt::Assign(s) => collect_nil_check_names_in_expr(&s.value, out),
        Stmt::If(s) => {
            collect_nil_check_names_in_expr(&s.cond, out);
            collect_nil_check_names_in_block(&s.then_block, out);
            if let Some(eb) = &s.else_block {
                collect_nil_check_names_in_block(eb, out);
            }
        }
        Stmt::While(s) => {
            collect_nil_check_names_in_expr(&s.cond, out);
            collect_nil_check_names_in_block(&s.body, out);
        }
        Stmt::Return(s) => {
            if let Some(v) = &s.value {
                collect_nil_check_names_in_expr(v, out);
            }
        }
        Stmt::Expr(s) => collect_nil_check_names_in_expr(&s.expr, out),
    }
}

fn collect_nil_check_names_in_expr(expr: &Expr, out: &mut std::collections::HashSet<String>) {
    match expr {
        Expr::FieldAccess(obj, name, _) => {
            out.insert(name.clone());
            collect_nil_check_names_in_expr(obj, out);
        }
        Expr::MethodCall(m) => {
            out.insert(m.method.clone());
            collect_nil_check_names_in_expr(&m.object, out);
            for arg in &m.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Expr::Binary(b) => {
            collect_nil_check_names_in_expr(&b.lhs, out);
            collect_nil_check_names_in_expr(&b.rhs, out);
        }
        Expr::Unary(u) => collect_nil_check_names_in_expr(&u.expr, out),
        Expr::Call(c) => {
            for arg in &c.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Expr::Ternary(t) => {
            collect_nil_check_names_in_expr(&t.condition, out);
            collect_nil_check_names_in_expr(&t.then_expr, out);
            collect_nil_check_names_in_expr(&t.else_expr, out);
        }
        Expr::Lambda(l) => match &l.body {
            LambdaBody::Expr(e) => collect_nil_check_names_in_expr(e, out),
            LambdaBody::Block(b) => collect_nil_check_names_in_block(b, out),
        },
        Expr::Print(e, _, _) => collect_nil_check_names_in_expr(e, out),
        Expr::Spawn(s) => {
            for arg in &s.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Expr::Await(a) => collect_nil_check_names_in_expr(&a.expr, out),
        Expr::StaticCall(s) => {
            for arg in &s.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Expr::ObjectLiteral(o) => {
            for f in &o.fields {
                collect_nil_check_names_in_expr(&f.value, out);
            }
        }
        Expr::Match(m) => {
            collect_nil_check_names_in_expr(&m.scrutinee, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_nil_check_names_in_expr(e, out),
                    MatchBody::Block(b) => collect_nil_check_names_in_block(b, out),
                }
            }
        }
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::String(_, _)
        | Expr::Var(_, _)
        | Expr::Select(_) => {}
    }
}
