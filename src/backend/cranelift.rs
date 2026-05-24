use anyhow::Result;
use cranelift_codegen::ir::{
    AbiParam, InstBuilder, UserFuncName,
    condcodes::{FloatCC, IntCC},
    types,
};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::collections::{HashMap, HashSet};

use crate::parser::ast::*;
use crate::{BuildMode, CodegenOptions};

pub struct Codegen {
    module: ObjectModule,
    func_ids: HashMap<String, FuncId>,
    func_return_types: HashMap<String, Type>,
    /// Full `Type::Fn(params, ret)` for each declared function — used to type function values.
    fn_types: HashMap<String, Type>,
    /// Names of imported modules, used to distinguish `mod::fn` from `Class::method`.
    known_modules: HashSet<String>,
    /// Maps each lambda's source span to its generated private function name.
    lambda_names: HashMap<crate::diagnostics::Span, String>,
    /// Counter for generating unique lambda names.
    lambda_counter: usize,
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
            known_modules: HashSet::new(),
            lambda_names: HashMap::new(),
            lambda_counter: 0,
        })
    }

    /// Compile an imported module. Functions are given the mangled name `{mod_name}__{fn}`.
    /// Must be called before `compile_program` so the entry module can call them.
    pub fn compile_module(&mut self, mod_name: &str, program: &Program) -> Result<()> {
        self.known_modules.insert(mod_name.to_string());

        // Forward-declare all functions in this module.
        for item in &program.items {
            if let Item::Function(f) = item {
                let mangled = format!("{}__{}", mod_name, f.name);
                self.declare_function_named(&mangled, f)?;
            }
        }

        // Compile bodies.
        for item in &program.items {
            if let Item::Function(f) = item {
                let mangled = format!("{}__{}", mod_name, f.name);
                self.compile_function_named(&mangled, f)?;
            }
        }
        Ok(())
    }

    pub fn compile_program(&mut self, program: &Program) -> Result<()> {
        self.declare_runtime()?;

        // Forward-declare all user functions first
        for item in &program.items {
            match item {
                Item::Function(f) => self.declare_user_function(f)?,
                Item::Class(_) => {}
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

        // Compile user function bodies
        for item in &program.items {
            match item {
                Item::Function(f) => self.compile_function(f)?,
                Item::Class(_) => {}
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
        self.func_return_types.insert(name.to_string(), ast_ret.clone());
        let param_types: Vec<Type> = l.params.iter().filter_map(|p| p.ty.clone()).collect();
        self.fn_types.insert(name.to_string(), Type::Fn(param_types, Box::new(ast_ret)));
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
                    span: p.span,
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
            params,
            return_type,
            body,
            span: l.span,
        };
        self.compile_function_named(name, &f)
    }

    fn declare_runtime(&mut self) -> Result<()> {
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
        Ok(())
    }

    fn declare_user_function(&mut self, f: &FunctionDecl) -> Result<()> {
        self.declare_function_named(&f.name.clone(), f)
    }

    fn declare_function_named(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        let mut sig = self.module.make_signature();
        for param in &f.params {
            sig.params.push(AbiParam::new(clif_type(&param.ty)));
        }
        // The C ABI `main` must return int.  Willow's `fn main()` is void in the
        // language, but we make the compiled symbol return I32 so the OS sees 0.
        if name == "main" {
            sig.returns.push(AbiParam::new(types::I32));
        } else if f.return_type != Type::Void {
            sig.returns.push(AbiParam::new(clif_type(&f.return_type)));
        }
        let linkage = if name == "main" {
            Linkage::Export
        } else {
            Linkage::Local
        };
        let id = self.module.declare_function(name, linkage, &sig)?;
        self.func_ids.insert(name.to_string(), id);
        self.func_return_types
            .insert(name.to_string(), f.return_type.clone());
        // Store full function type for use when the function is passed as a value.
        let param_types = f.params.iter().map(|p| p.ty.clone()).collect();
        self.fn_types.insert(
            name.to_string(),
            Type::Fn(param_types, Box::new(f.return_type.clone())),
        );
        Ok(())
    }

    fn compile_function(&mut self, f: &FunctionDecl) -> Result<()> {
        self.compile_function_named(&f.name.clone(), f)
    }

    fn compile_function_named(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        let func_id = self.func_ids[name];

        let mut sig = self.module.make_signature();
        for param in &f.params {
            sig.params.push(AbiParam::new(clif_type(&param.ty)));
        }
        if name == "main" {
            sig.returns.push(AbiParam::new(types::I32));
        } else if f.return_type != Type::Void {
            sig.returns.push(AbiParam::new(clif_type(&f.return_type)));
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
            known_modules: &self.known_modules,
            lambda_names: &self.lambda_names,
            vars: HashMap::new(),
            return_type: f.return_type.clone(),
            is_main: name == "main",
            terminated: false,
        };

        // Bind params
        for (i, param) in f.params.iter().enumerate() {
            let val = fg.builder.block_params(entry_block)[i];
            let var = fg.builder.declare_var(clif_type(&param.ty));
            fg.builder.def_var(var, val);
            fg.vars.insert(param.name.clone(), (var, param.ty.clone()));
        }

        fg.emit_block(&f.body);

        // Implicit return at end of function body.
        if !fg.terminated {
            if name == "main" {
                // C ABI main must return int; Willow main is void so we synthesize `return 0`.
                let zero = fg.builder.ins().iconst(types::I32, 0);
                fg.builder.ins().return_(&[zero]);
            } else {
                fg.builder.ins().return_(&[]);
            }
        }

        builder.finalize();
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
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
    known_modules: &'a HashSet<String>,
    lambda_names: &'a HashMap<crate::diagnostics::Span, String>,
    vars: HashMap<String, (Variable, Type)>,
    return_type: Type,
    is_main: bool,
    terminated: bool,
}

impl<'a, 'b> FuncGen<'a, 'b> {
    fn emit_block(&mut self, block: &Block) {
        // Save the name→variable mapping so inner `let` bindings don't
        // escape the block (shadowing: restore outer binding on exit).
        let saved_vars = self.vars.clone();
        for stmt in &block.stmts {
            if self.terminated {
                break;
            }
            self.emit_stmt(stmt);
        }
        self.vars = saved_vars;
    }

    fn emit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                let val = self.emit_expr(&s.init);
                let ty = clif_type_of_expr(&s.init, &self.vars, self.func_return_types);
                let var = self.builder.declare_var(ty);
                self.builder.def_var(var, val);
                // Determine the AST type. For named function values and lambdas, look up
                // their full fn type so indirect calls later get the right signature.
                let ast_ty = self.ast_type_of_init(&s.init);
                self.vars.insert(s.name.clone(), (var, ast_ty));
            }
            Stmt::Assign(s) => {
                if let Some((var, _)) = self.vars.get(&s.name).cloned() {
                    let val = self.emit_expr(&s.value);
                    self.builder.def_var(var, val);
                }
            }
            Stmt::If(s) => self.emit_if(s),
            Stmt::While(s) => self.emit_while(s),
            Stmt::Return(s) => {
                if let Some(val_expr) = &s.value {
                    let val = self.emit_expr(val_expr);
                    self.builder.ins().return_(&[val]);
                } else if self.is_main {
                    let zero = self.builder.ins().iconst(types::I32, 0);
                    self.builder.ins().return_(&[zero]);
                } else {
                    self.builder.ins().return_(&[]);
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
    fn ast_type_of_init(&self, expr: &Expr) -> Type {
        match expr {
            // Named function used as a value → look up its full fn type.
            Expr::Var(name, _) => {
                if let Some(ty) = self.fn_types.get(name.as_str()) {
                    return ty.clone();
                }
                ast_type_of_expr(expr, &self.vars, self.func_return_types)
            }
            // Lambda expression → build the fn type from its params and return type.
            Expr::Lambda(l) => {
                let params = l.params.iter().filter_map(|p| p.ty.clone()).collect();
                let ret = l.return_type.clone().unwrap_or(Type::I64);
                Type::Fn(params, Box::new(ret))
            }
            _ => ast_type_of_expr(expr, &self.vars, self.func_return_types),
        }
    }

    fn emit_expr(&mut self, expr: &Expr) -> cranelift_codegen::ir::Value {
        match expr {
            Expr::Integer(n, _) => self.builder.ins().iconst(types::I64, *n),
            Expr::Float(f, _) => self.builder.ins().f64const(*f),
            Expr::Bool(b, _) => self.builder.ins().iconst(types::I8, if *b { 1 } else { 0 }),
            Expr::Var(name, _) => {
                // Local variable or function value?
                if let Some(&(var, _)) = self.vars.get(name.as_str()) {
                    return self.builder.use_var(var);
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
                let arg_ty = ast_type_of_expr(arg, &self.vars, self.func_return_types);
                let fn_name = match (arg_ty, newline) {
                    (Type::I64, false) => "willow_print_i64",
                    (Type::I64, true) => "willow_println_i64",
                    (Type::F64, false) => "willow_print_f64",
                    (Type::F64, true) => "willow_println_f64",
                    (Type::Bool, false) => "willow_print_bool",
                    (Type::Bool, true) => "willow_println_bool",
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
            Expr::FieldAccess(_, _, _) | Expr::MethodCall(_) => {
                self.builder.ins().iconst(types::I64, 0)
            }
            Expr::StaticCall(s) => self.emit_static_call(s),
        }
    }

    fn emit_binary(&mut self, b: &BinaryExpr) -> cranelift_codegen::ir::Value {
        let lhs = self.emit_expr(&b.lhs);
        let rhs = self.emit_expr(&b.rhs);
        let lty = ast_type_of_expr(&b.lhs, &self.vars, self.func_return_types);
        let is_float = lty == Type::F64;
        match &b.op {
            BinOp::Add => {
                if is_float {
                    self.builder.ins().fadd(lhs, rhs)
                } else {
                    self.builder.ins().iadd(lhs, rhs)
                }
            }
            BinOp::Sub => {
                if is_float {
                    self.builder.ins().fsub(lhs, rhs)
                } else {
                    self.builder.ins().isub(lhs, rhs)
                }
            }
            BinOp::Mul => {
                if is_float {
                    self.builder.ins().fmul(lhs, rhs)
                } else {
                    self.builder.ins().imul(lhs, rhs)
                }
            }
            BinOp::Div => {
                if is_float {
                    self.builder.ins().fdiv(lhs, rhs)
                } else {
                    self.builder.ins().sdiv(lhs, rhs)
                }
            }
            BinOp::Rem => self.builder.ins().srem(lhs, rhs),
            BinOp::Lt => {
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::LessThan, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedLessThan, lhs, rhs)
                }
            }
            BinOp::Le => {
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::LessThanOrEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedLessThanOrEqual, lhs, rhs)
                }
            }
            BinOp::Gt => {
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::GreaterThan, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedGreaterThan, lhs, rhs)
                }
            }
            BinOp::Ge => {
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::GreaterThanOrEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedGreaterThanOrEqual, lhs, rhs)
                }
            }
            BinOp::Eq => {
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::Equal, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::Equal, lhs, rhs)
                }
            }
            BinOp::Ne => {
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::NotEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::NotEqual, lhs, rhs)
                }
            }
            BinOp::And => self.builder.ins().band(lhs, rhs),
            BinOp::Or => self.builder.ins().bor(lhs, rhs),
        }
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
        // Direct call to a known function.
        if let Some(&fid) = self.func_ids.get(&c.callee) {
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let args: Vec<_> = c.args.iter().map(|a| self.emit_expr(a)).collect();
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            return if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
        }

        // Indirect call through a function-value local variable.
        if let Some((var, var_ty)) = self.vars.get(&c.callee).cloned() {
            if let Type::Fn(param_types, ret_type) = var_ty {
                let callee_val = self.builder.use_var(var);
                let args: Vec<_> = c.args.iter().map(|a| self.emit_expr(a)).collect();

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

    fn emit_ternary(&mut self, t: &TernaryExpr) -> cranelift_codegen::ir::Value {
        let result_ty = clif_type_of_expr(&t.then_expr, &self.vars, self.func_return_types);
        let result_var = self.builder.declare_var(result_ty);

        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        let cond = self.emit_expr(&t.condition);
        self.builder.ins().brif(cond, then_block, &[], else_block, &[]);

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

    fn emit_static_call(&mut self, s: &StaticCallExpr) -> cranelift_codegen::ir::Value {
        // Module call: `math::add(args)` → mangled name `math__add`
        if self.known_modules.contains(&s.class) {
            let mangled = format!("{}__{}", s.class, s.method);
            let fid = match self.func_ids.get(&mangled) {
                Some(&id) => id,
                None => panic!("undefined module function: {}", mangled),
            };
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let args: Vec<_> = s.args.iter().map(|a| self.emit_expr(a)).collect();
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
        Type::Fn(_, _) => types::I64, // function pointer (pointer-sized)
        Type::Void | Type::Named(_) => types::I8,
    }
}

fn clif_type_of_expr(
    expr: &Expr,
    vars: &HashMap<String, (Variable, Type)>,
    frt: &HashMap<String, Type>,
) -> cranelift_codegen::ir::Type {
    clif_type(&ast_type_of_expr(expr, vars, frt))
}

fn ast_type_of_expr(
    expr: &Expr,
    vars: &HashMap<String, (Variable, Type)>,
    frt: &HashMap<String, Type>,
) -> Type {
    match expr {
        Expr::Integer(_, _) => Type::I64,
        Expr::Float(_, _) => Type::F64,
        Expr::Bool(_, _) => Type::Bool,
        Expr::Var(name, _) => vars
            .get(name.as_str())
            .map(|(_, t)| t.clone())
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
        Expr::Call(c) => frt.get(&c.callee).cloned().unwrap_or(Type::I64),
        Expr::Print(_, _, _) => Type::Void,
        Expr::Ternary(t) => ast_type_of_expr(&t.then_expr, vars, frt),
        Expr::Lambda(l) => {
            let params = l
                .params
                .iter()
                .filter_map(|p| p.ty.clone())
                .collect::<Vec<_>>();
            let ret = l
                .return_type
                .clone()
                .unwrap_or(Type::I64);
            Type::Fn(params, Box::new(ret))
        }
        Expr::FieldAccess(_, _, _) | Expr::MethodCall(_) => Type::Void,
        Expr::StaticCall(s) => {
            // Look up mangled name for module calls.
            let mangled = format!("{}__{}", s.class, s.method);
            frt.get(&mangled)
                .or_else(|| frt.get(&s.method))
                .cloned()
                .unwrap_or(Type::I64)
        }
    }
}

// ── Lambda collection helpers ─────────────────────────────────────────────────

fn collect_lambdas_in_program(program: &Program) -> Vec<(String, LambdaExpr)> {
    let mut out = Vec::new();
    let mut counter = 0usize;
    for item in &program.items {
        if let Item::Function(f) = item {
            collect_lambdas_in_block(&f.body, &mut counter, &mut out);
        }
    }
    out
}

fn collect_lambdas_in_block(block: &Block, counter: &mut usize, out: &mut Vec<(String, LambdaExpr)>) {
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
                collect_lambdas_in_expr(arg, counter, out);
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
                collect_lambdas_in_expr(arg, counter, out);
            }
        }
        Expr::MethodCall(m) => {
            collect_lambdas_in_expr(&m.object, counter, out);
            for arg in &m.args {
                collect_lambdas_in_expr(arg, counter, out);
            }
        }
        Expr::FieldAccess(e, _, _) => collect_lambdas_in_expr(e, counter, out),
        _ => {}
    }
}
