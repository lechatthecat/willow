use anyhow::Result;
use cranelift_codegen::ir::{
    AbiParam, InstBuilder, UserFuncName,
    condcodes::{FloatCC, IntCC},
    types,
};
use cranelift_codegen::settings;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::collections::HashMap;

use crate::parser::ast::*;

pub struct Codegen {
    module: ObjectModule,
    func_ids: HashMap<String, FuncId>,
    func_return_types: HashMap<String, Type>,
}

impl Codegen {
    pub fn new() -> Result<Self> {
        let isa_builder = cranelift_native::builder().map_err(|e| anyhow::anyhow!("{}", e))?;
        let flag_builder = settings::builder();
        let flags = settings::Flags::new(flag_builder);
        let isa = isa_builder.finish(flags)?;
        let obj_builder =
            ObjectBuilder::new(isa, "willow", cranelift_module::default_libcall_names())?;
        let module = ObjectModule::new(obj_builder);
        Ok(Self {
            module,
            func_ids: HashMap::new(),
            func_return_types: HashMap::new(),
        })
    }

    pub fn compile_program(&mut self, program: &Program) -> Result<()> {
        self.declare_runtime()?;

        // Forward-declare all user functions first
        for item in &program.items {
            match item {
                Item::Function(f) => self.declare_user_function(f)?,
                Item::Class(_) => {} // class codegen handled in willow-jbf
            }
        }

        // Then compile bodies
        for item in &program.items {
            match item {
                Item::Function(f) => self.compile_function(f)?,
                Item::Class(_) => {} // class codegen handled in willow-jbf
            }
        }
        Ok(())
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
        let mut sig = self.module.make_signature();
        for param in &f.params {
            sig.params.push(AbiParam::new(clif_type(&param.ty)));
        }
        if f.return_type != Type::Void {
            sig.returns.push(AbiParam::new(clif_type(&f.return_type)));
        }
        let linkage = if f.name == "main" {
            Linkage::Export
        } else {
            Linkage::Local
        };
        let id = self.module.declare_function(&f.name, linkage, &sig)?;
        self.func_ids.insert(f.name.clone(), id);
        self.func_return_types
            .insert(f.name.clone(), f.return_type.clone());
        Ok(())
    }

    fn compile_function(&mut self, f: &FunctionDecl) -> Result<()> {
        let func_id = self.func_ids[&f.name];

        let mut sig = self.module.make_signature();
        for param in &f.params {
            sig.params.push(AbiParam::new(clif_type(&param.ty)));
        }
        if f.return_type != Type::Void {
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
            vars: HashMap::new(),
            return_type: f.return_type.clone(),
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

        // Implicit void return
        if !fg.terminated {
            fg.builder.ins().return_(&[]);
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
    vars: HashMap<String, (Variable, Type)>,
    return_type: Type,
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
                let ast_ty = ast_type_of_expr(&s.init, &self.vars, self.func_return_types);
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

    fn emit_expr(&mut self, expr: &Expr) -> cranelift_codegen::ir::Value {
        match expr {
            Expr::Integer(n, _) => self.builder.ins().iconst(types::I64, *n),
            Expr::Float(f, _) => self.builder.ins().f64const(*f),
            Expr::Bool(b, _) => self.builder.ins().iconst(types::I8, if *b { 1 } else { 0 }),
            Expr::Var(name, _) => {
                let (var, _) = self.vars[name.as_str()];
                self.builder.use_var(var)
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
            // Class-related expressions not yet supported in codegen (willow-jbf)
            Expr::FieldAccess(_, _, _) | Expr::MethodCall(_) | Expr::StaticCall(_) => {
                self.builder.ins().iconst(types::I64, 0)
            }
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
        let fid = match self.func_ids.get(&c.callee) {
            Some(&id) => id,
            None => panic!("undefined function: {}", c.callee),
        };
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let args: Vec<_> = c.args.iter().map(|a| self.emit_expr(a)).collect();
        let call = self.builder.ins().call(fref, &args);
        let results = self.builder.inst_results(call);
        if results.is_empty() {
            self.builder.ins().iconst(types::I8, 0)
        } else {
            results[0]
        }
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
        Expr::FieldAccess(_, _, _) | Expr::MethodCall(_) | Expr::StaticCall(_) => Type::Void,
    }
}
