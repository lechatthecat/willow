//! LIR-walking code generation — willow-0g8j.
//!
//! First stage of migrating the emit layer off the raw AST: a function whose
//! lowered IR uses only the supported scalar subset is compiled by walking its
//! [`LirFunction`] basic blocks directly (typed [`HirExpr`] trees inside), so
//! the backend never touches the AST body for it. Everything else falls back to
//! the existing AST-walking path, chosen per function in
//! `compile_function_named`. `WILLOW_LIR_BACKEND=0` disables the LIR path.
//!
//! Supported subset (v2): `i64`/`f64`/`bool`/`String` values; literals,
//! variables, arithmetic/comparison, unary ops, string concatenation and
//! content comparison; direct calls to known non-async functions;
//! `print`/`println` of a scalar or a string; `let`/assign; the full block
//! control flow (jump/branch/return). Arrays, class objects, enums,
//! interfaces, async, and lambdas stay on the AST path for now.
//!
//! GC rooting (willow-0g8j.1): the LIR has no block scopes — it is a flat
//! basic-block graph — so a per-`let` push/pop pairing like the AST path's
//! would grow the shadow root stack once per loop iteration. Instead every
//! GC-managed local gets ONE stack slot allocated and rooted at function
//! entry, null-initialized so a collection before the `let` runs sees an empty
//! slot; the slot is both the variable's storage and its root (the AST
//! invariant that keeps a reassignment from leaving a stale root), and all
//! roots are popped at each `return`. Expression temporaries that must survive
//! an allocating call are rooted exactly as the AST path roots them.

use std::collections::HashSet;

use cranelift_codegen::ir::{
    InstBuilder, StackSlotData, StackSlotKind, condcodes::FloatCC, condcodes::IntCC, types,
};
use cranelift_module::Module;

use crate::ir::lowered::{LirBlock, LirFunction, LirInst, Terminator};
use crate::ir::typed_ast::{HirExpr, HirExprKind};
use crate::parser::ast::{BinOp, Type, UnaryOp};

use super::type_helpers::{clif_type, is_gc_managed};
use super::{FuncGen, VarStorage};

/// True when the environment does not disable the LIR backend.
pub(super) fn lir_backend_enabled() -> bool {
    std::env::var("WILLOW_LIR_BACKEND")
        .map(|v| v != "0")
        .unwrap_or(true)
}

fn scalar(ty: &Type) -> bool {
    matches!(ty, Type::I64 | Type::F64 | Type::Bool)
}

/// Types the LIR walker can hold in a value position. `String` is the first
/// GC-managed type it handles; arrays, class objects, enums, and interfaces
/// need expression forms (allocation, field access, interface boxing) the
/// walker does not emit yet, so they keep falling back to the AST path.
fn supported_type(ty: &Type) -> bool {
    scalar(ty) || matches!(ty, Type::Void | Type::String)
}

/// Conservative eligibility: every type, instruction, and expression must be in
/// the supported subset, every callee must be a known symbol, every variable
/// must be a parameter or a `let` of this function, and binding names must be
/// unique across it (LIR flattens block scopes, so shadowing across sibling
/// scopes — or over a parameter — would alias one variable).
pub(super) fn lir_supported_function(f: &LirFunction, known_fn: &dyn Fn(&str) -> bool) -> bool {
    if !supported_type(&f.return_type) {
        return false;
    }
    // Reference parameters (`&`/`&mut`) are pointers at the ABI level.
    if !f
        .params
        .iter()
        .all(|p| supported_type(&p.ty) && !p.by_reference)
    {
        return false;
    }
    // Names the walker can resolve. Any other `Var` is something the HIR spells
    // like a variable but codegen must special-case — a bare enum variant, a
    // function used as a value — so the function falls back (willow-0g8j.1).
    let mut names: HashSet<&str> = HashSet::new();
    for p in &f.params {
        if !names.insert(p.name.as_str()) {
            return false;
        }
    }
    for block in &f.blocks {
        for inst in &block.instrs {
            if let LirInst::Let { name, .. } = inst
                && !names.insert(name.as_str())
            {
                return false; // shadows a parameter or another `let`
            }
        }
    }

    for block in &f.blocks {
        for inst in &block.instrs {
            match inst {
                LirInst::Let { value, .. } | LirInst::Assign { value, .. } => {
                    if !supported_expr(value, known_fn, &names) {
                        return false;
                    }
                }
                LirInst::Expr(e) => {
                    if !supported_expr(e, known_fn, &names) {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        match &block.terminator {
            Terminator::Branch { cond, .. } => {
                if !supported_expr(cond, known_fn, &names) {
                    return false;
                }
            }
            Terminator::Return(Some(v)) => {
                if !supported_expr(v, known_fn, &names) {
                    return false;
                }
            }
            Terminator::Jump(_) | Terminator::Return(None) => {}
        }
    }
    true
}

fn supported_expr(e: &HirExpr, known_fn: &dyn Fn(&str) -> bool, names: &HashSet<&str>) -> bool {
    if !supported_type(&e.ty) {
        return false;
    }
    match &e.kind {
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) => true,
        HirExprKind::Str(_) => true,
        HirExprKind::Var(name) => names.contains(name.as_str()),
        HirExprKind::Binary { op, lhs, rhs } => {
            // On strings only `+` (concat) and content comparison are emitted.
            if lhs.ty == Type::String && !matches!(op, BinOp::Add | BinOp::Eq | BinOp::Ne) {
                return false;
            }
            supported_expr(lhs, known_fn, names) && supported_expr(rhs, known_fn, names)
        }
        HirExprKind::Ternary {
            condition,
            then_expr,
            else_expr,
        } => {
            supported_expr(condition, known_fn, names)
                && supported_expr(then_expr, known_fn, names)
                && supported_expr(else_expr, known_fn, names)
        }
        HirExprKind::Unary { operand, .. } => supported_expr(operand, known_fn, names),
        HirExprKind::Call { callee, args } => {
            known_fn(callee.as_str()) && args.iter().all(|a| supported_expr(a, known_fn, names))
        }
        HirExprKind::Print { value, newline: _ } => {
            (scalar(&value.ty) || value.ty == Type::String)
                && supported_expr(value, known_fn, names)
        }
        _ => false,
    }
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit a whole function body by walking its LIR block graph. The entry
    /// block's instructions land in the already-created Cranelift entry block
    /// (parameters are bound there); every other LIR block gets its own.
    /// All paths are terminated by the LIR, so the caller must skip its
    /// implicit-return epilogue.
    pub(super) fn emit_lir_function(&mut self, f: &LirFunction) {
        let entry = self.builder.current_block().expect("entry block active");
        self.bind_lir_gc_locals(f);
        let mut blocks = vec![entry];
        for _ in 1..f.blocks.len() {
            blocks.push(self.builder.create_block());
        }

        for (i, block) in f.blocks.iter().enumerate() {
            if i > 0 {
                self.builder.switch_to_block(blocks[i]);
            }
            self.emit_lir_block(block, &blocks, &f.return_type);
        }
        self.builder.seal_all_blocks();
        self.terminated = true;
    }

    /// Give every GC-managed `let` of this function one entry-allocated, rooted
    /// stack slot (see the module docs). The slot is null-initialized so a
    /// collection that happens before the `let` executes reads an empty root
    /// rather than uninitialized stack memory. GC-managed *parameters* already
    /// got the same treatment from `bind_param`, so they are skipped here.
    fn bind_lir_gc_locals(&mut self, f: &LirFunction) {
        let ptr_ty = self.module.target_config().pointer_type();
        let mut null = None;
        for block in &f.blocks {
            for inst in &block.instrs {
                let LirInst::Let { name, value, .. } = inst else {
                    continue;
                };
                if !is_gc_managed(&value.ty, self.enum_infos) {
                    continue;
                }
                let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    0,
                ));
                let zero = *null.get_or_insert_with(|| self.builder.ins().iconst(ptr_ty, 0));
                self.builder.ins().stack_store(zero, slot, 0);
                self.emit_push_root_slot(slot);
                self.vars.insert(
                    name.clone(),
                    VarStorage::Stack {
                        slot,
                        ty: value.ty.clone(),
                    },
                );
            }
        }
    }

    fn emit_lir_block(
        &mut self,
        block: &LirBlock,
        blocks: &[cranelift_codegen::ir::Block],
        return_type: &Type,
    ) {
        for inst in &block.instrs {
            match inst {
                LirInst::Let { name, value, .. } => {
                    let val = self.emit_lir_expr(value);
                    // A GC-managed local already has its rooted slot from
                    // `bind_lir_gc_locals`; storing into it is the whole binding.
                    if let Some(storage @ VarStorage::Stack { .. }) =
                        self.vars.get(name.as_str()).cloned()
                    {
                        self.store_var(&storage, val);
                        continue;
                    }
                    let var = self.builder.declare_var(clif_type(&value.ty));
                    self.builder.def_var(var, val);
                    self.vars.insert(
                        name.clone(),
                        VarStorage::Value {
                            var,
                            ty: value.ty.clone(),
                        },
                    );
                }
                LirInst::Assign { name, value } => {
                    let val = self.emit_lir_expr(value);
                    if let Some(storage) = self.vars.get(name.as_str()).cloned() {
                        self.store_var(&storage, val);
                    }
                }
                LirInst::Expr(e) => {
                    self.emit_lir_expr(e);
                }
                // Filtered out by eligibility.
                _ => unreachable!("unsupported LIR instruction reached emission"),
            }
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                self.builder.ins().jump(blocks[b.0], &[]);
            }
            Terminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                let c = self.emit_lir_expr(cond);
                self.builder
                    .ins()
                    .brif(c, blocks[then_block.0], &[], blocks[else_block.0], &[]);
            }
            Terminator::Return(Some(v)) => {
                // Evaluate first: the value may read through a rooted local.
                let val = self.emit_lir_expr(v);
                self.emit_pop_roots_n(self.gc_root_count);
                self.builder.ins().return_(&[val]);
            }
            Terminator::Return(None) => {
                self.emit_pop_roots_n(self.gc_root_count);
                if *return_type == Type::Void {
                    self.builder.ins().return_(&[]);
                } else {
                    // Unreachable fall-through in a value function (the checker
                    // guarantees returns); satisfy the signature with a zero.
                    let zero = match clif_type(return_type) {
                        types::F64 => self.builder.ins().f64const(0.0),
                        ty => self.builder.ins().iconst(ty, 0),
                    };
                    self.builder.ins().return_(&[zero]);
                }
            }
        }
    }

    fn emit_lir_expr(&mut self, e: &HirExpr) -> cranelift_codegen::ir::Value {
        match &e.kind {
            HirExprKind::Int(n) => self.builder.ins().iconst(types::I64, *n),
            HirExprKind::Float(x) => self.builder.ins().f64const(*x),
            HirExprKind::Bool(b) => self.builder.ins().iconst(types::I8, i64::from(*b)),
            HirExprKind::Str(s) => self.emit_string_literal(s),
            HirExprKind::Var(name) => match self.vars.get(name.as_str()).cloned() {
                Some(storage) => self.load_var(&storage),
                // Same loud failure as the AST path (willow-thqe class): a
                // variable the eligibility check admitted must be bound.
                None => {
                    panic!("internal compiler error: variable `{name}` reached LIR codegen unbound")
                }
            },
            HirExprKind::Binary { op, lhs, rhs } if lhs.ty == Type::String => {
                self.emit_lir_string_binop(op, lhs, rhs)
            }
            HirExprKind::Binary { op, lhs, rhs } => match op {
                // Short-circuit: the rhs must not evaluate when the lhs decides.
                BinOp::And | BinOp::Or => {
                    let l = self.emit_lir_expr(lhs);
                    let result_var = self.builder.declare_var(types::I8);
                    let rhs_block = self.builder.create_block();
                    let short_block = self.builder.create_block();
                    let merge_block = self.builder.create_block();
                    if matches!(op, BinOp::And) {
                        self.builder.ins().brif(l, rhs_block, &[], short_block, &[]);
                    } else {
                        self.builder.ins().brif(l, short_block, &[], rhs_block, &[]);
                    }

                    self.builder.switch_to_block(rhs_block);
                    self.builder.seal_block(rhs_block);
                    let r = self.emit_lir_expr(rhs);
                    self.builder.def_var(result_var, r);
                    self.builder.ins().jump(merge_block, &[]);

                    self.builder.switch_to_block(short_block);
                    self.builder.seal_block(short_block);
                    let short_val = self
                        .builder
                        .ins()
                        .iconst(types::I8, i64::from(matches!(op, BinOp::Or)));
                    self.builder.def_var(result_var, short_val);
                    self.builder.ins().jump(merge_block, &[]);

                    self.builder.switch_to_block(merge_block);
                    self.builder.seal_block(merge_block);
                    self.builder.use_var(result_var)
                }
                _ => {
                    let float = lhs.ty == Type::F64;
                    let l = self.emit_lir_expr(lhs);
                    let r = self.emit_lir_expr(rhs);
                    if !float && matches!(op, BinOp::Div | BinOp::Rem) {
                        self.emit_int_div_guard(l, r, matches!(op, BinOp::Rem), e.span);
                    }
                    self.emit_lir_binop(op, l, r, float)
                }
            },
            HirExprKind::Ternary {
                condition,
                then_expr,
                else_expr,
            } => {
                let result_var = self.builder.declare_var(clif_type(&e.ty));
                let then_block = self.builder.create_block();
                let else_block = self.builder.create_block();
                let merge_block = self.builder.create_block();

                let cond = self.emit_lir_expr(condition);
                self.builder
                    .ins()
                    .brif(cond, then_block, &[], else_block, &[]);

                self.builder.switch_to_block(then_block);
                self.builder.seal_block(then_block);
                let t = self.emit_lir_expr(then_expr);
                self.builder.def_var(result_var, t);
                self.builder.ins().jump(merge_block, &[]);

                self.builder.switch_to_block(else_block);
                self.builder.seal_block(else_block);
                let f = self.emit_lir_expr(else_expr);
                self.builder.def_var(result_var, f);
                self.builder.ins().jump(merge_block, &[]);

                self.builder.switch_to_block(merge_block);
                self.builder.seal_block(merge_block);
                self.builder.use_var(result_var)
            }
            HirExprKind::Unary { op, operand } => {
                let val = self.emit_lir_expr(operand);
                match op {
                    UnaryOp::Neg if operand.ty == Type::F64 => self.builder.ins().fneg(val),
                    UnaryOp::Neg => self.builder.ins().ineg(val),
                    UnaryOp::Not => {
                        let one = self.builder.ins().iconst(types::I8, 1);
                        self.builder.ins().bxor(val, one)
                    }
                }
            }
            HirExprKind::Call { callee, args } => {
                // Root each GC-managed argument as it is produced: a later
                // argument (or the callee itself) can allocate and collect,
                // and an already-evaluated argument is otherwise only held in
                // an SSA register the GC cannot see (same rule as the AST path).
                let mut vals = Vec::with_capacity(args.len());
                let mut temp_roots = 0usize;
                for a in args {
                    let val = self.emit_lir_expr(a);
                    if is_gc_managed(&a.ty, self.enum_infos) {
                        self.emit_push_root(val);
                        temp_roots += 1;
                    }
                    vals.push(val);
                }
                let fid = self.func_ids[callee.as_str()];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                // Debug builds record the call on the panic call-chain stack,
                // exactly like the AST path (willow-992h).
                let pushed = self.emit_callstack_push(callee, e.span);
                let call = self.builder.ins().call(fref, &vals);
                let results = self.builder.inst_results(call);
                let result = results
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0));
                if pushed {
                    self.emit_callstack_pop();
                }
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
                result
            }
            HirExprKind::Print { value, newline } => {
                let val = self.emit_lir_expr(value);
                let fn_name = match (&value.ty, newline) {
                    (Type::I64, false) => "willow_print_i64",
                    (Type::I64, true) => "willow_println_i64",
                    (Type::F64, false) => "willow_print_f64",
                    (Type::F64, true) => "willow_println_f64",
                    (Type::Bool, false) => "willow_print_bool",
                    (Type::Bool, true) => "willow_println_bool",
                    (Type::String, false) => "willow_print_string",
                    (Type::String, true) => "willow_println_string",
                    _ => unreachable!("unsupported print type passed eligibility"),
                };
                let fid = self.func_ids[fn_name];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[val]);
                self.builder.ins().iconst(types::I8, 0)
            }
            _ => unreachable!("unsupported LIR expression reached emission"),
        }
    }

    /// `String` `+` / `==` / `!=`. Both runtime calls allocate (concat builds a
    /// new string; comparison can run a collection through the allocator), so
    /// the left operand is rooted across the right operand's evaluation and the
    /// call itself, mirroring the AST path.
    fn emit_lir_string_binop(
        &mut self,
        op: &BinOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> cranelift_codegen::ir::Value {
        let l = self.emit_lir_expr(lhs);
        self.emit_push_root(l);
        let r = self.emit_lir_expr(rhs);
        let (rt, roots) = match op {
            BinOp::Add => {
                // The concat call allocates, so the right operand must be a
                // root too — it may itself be a fresh temporary.
                self.emit_push_root(r);
                ("willow_string_concat", 2)
            }
            BinOp::Eq | BinOp::Ne => ("willow_string_eq", 1),
            _ => unreachable!("non-concat/compare string operator passed eligibility"),
        };
        let fid = self.func_id(rt);
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[l, r]);
        let raw = self.builder.inst_results(call)[0];
        self.emit_pop_roots_n(roots);
        self.gc_root_count -= roots;
        match op {
            BinOp::Add => raw,
            // `willow_string_eq` answers in a word; the language's bool is i8.
            BinOp::Eq => self.builder.ins().ireduce(types::I8, raw),
            _ => {
                let inv = self.builder.ins().bxor_imm(raw, 1);
                self.builder.ins().ireduce(types::I8, inv)
            }
        }
    }

    fn emit_lir_binop(
        &mut self,
        op: &BinOp,
        l: cranelift_codegen::ir::Value,
        r: cranelift_codegen::ir::Value,
        float: bool,
    ) -> cranelift_codegen::ir::Value {
        let ins = self.builder.ins();
        if float {
            return match op {
                BinOp::Add => ins.fadd(l, r),
                BinOp::Sub => ins.fsub(l, r),
                BinOp::Mul => ins.fmul(l, r),
                BinOp::Div => ins.fdiv(l, r),
                BinOp::Rem => unreachable!("f64 % is rejected by the checker"),
                BinOp::Eq => ins.fcmp(FloatCC::Equal, l, r),
                BinOp::Ne => ins.fcmp(FloatCC::NotEqual, l, r),
                BinOp::Lt => ins.fcmp(FloatCC::LessThan, l, r),
                BinOp::Le => ins.fcmp(FloatCC::LessThanOrEqual, l, r),
                BinOp::Gt => ins.fcmp(FloatCC::GreaterThan, l, r),
                BinOp::Ge => ins.fcmp(FloatCC::GreaterThanOrEqual, l, r),
                BinOp::And | BinOp::Or => unreachable!("short-circuit ops rejected"),
            };
        }
        match op {
            BinOp::Add => ins.iadd(l, r),
            BinOp::Sub => ins.isub(l, r),
            BinOp::Mul => ins.imul(l, r),
            BinOp::Div => ins.sdiv(l, r),
            BinOp::Rem => ins.srem(l, r),
            BinOp::Eq => ins.icmp(IntCC::Equal, l, r),
            BinOp::Ne => ins.icmp(IntCC::NotEqual, l, r),
            BinOp::Lt => ins.icmp(IntCC::SignedLessThan, l, r),
            BinOp::Le => ins.icmp(IntCC::SignedLessThanOrEqual, l, r),
            BinOp::Gt => ins.icmp(IntCC::SignedGreaterThan, l, r),
            BinOp::Ge => ins.icmp(IntCC::SignedGreaterThanOrEqual, l, r),
            BinOp::And | BinOp::Or => unreachable!("short-circuit ops rejected"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn lir_program(src: &str) -> crate::ir::lowered::LirProgram {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        assert!(diags.is_empty(), "{diags:?}");
        crate::ir::lowered::lower_program(&hir)
    }

    fn eligible(src: &str, name: &str, fns: &[&str]) -> bool {
        let p = lir_program(src);
        let known: HashSet<&str> = fns.iter().copied().collect();
        let f = p.functions.iter().find(|f| f.name == name).unwrap();
        lir_supported_function(f, &|n| known.contains(n))
    }

    /// Like [`eligible`], but for forms the HIR may refuse to lower at all: a
    /// function with no lowered IR is by definition not claimed by the LIR path.
    fn eligible_lenient(src: &str, name: &str, fns: &[&str]) -> bool {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        if !diags.is_empty() {
            return false;
        }
        let p = crate::ir::lowered::lower_program(&hir);
        let known: HashSet<&str> = fns.iter().copied().collect();
        match p.functions.iter().find(|f| f.name == name) {
            Some(f) => lir_supported_function(f, &|n| known.contains(n)),
            None => false,
        }
    }

    // 1. a scalar arithmetic function is eligible
    #[test]
    fn e01_scalar_fn_eligible() {
        assert!(eligible(
            "fn add(a: i64, b: i64) -> i64 { return a + b; }",
            "add",
            &["add"]
        ));
    }

    // 2. recursive control flow (fib) is eligible
    #[test]
    fn e02_fib_eligible() {
        let src = "fn fib(n: i64) -> i64 { if n <= 1 { return n; } return fib(n-1) + fib(n-2); }";
        assert!(eligible(src, "fib", &["fib"]));
    }

    // 3. print of a scalar is eligible
    #[test]
    fn e03_scalar_print_eligible() {
        assert!(eligible(
            "fn show(n: i64) { println(n * 2); }",
            "show",
            &["show"]
        ));
    }

    // 4. (updated by willow-0g8j.1) string values became eligible with GC
    // rooting; kept as a positive check so a regression here is loud.
    #[test]
    fn e04_string_now_eligible() {
        assert!(eligible("fn s() { println(\"hi\"); }", "s", &["s"]));
    }

    // 5. (updated) short-circuit operators became eligible with lazy block
    // emission; kept as a positive check so a regression here is loud.
    #[test]
    fn e05_short_circuit_now_eligible() {
        assert!(eligible(
            "fn f(a: bool, b: bool) -> bool { return a && b; }",
            "f",
            &["f"]
        ));
    }

    // 6. unknown callees are not eligible
    #[test]
    fn e06_unknown_callee_ineligible() {
        assert!(!eligible(
            "fn g() -> i64 { return 1; } fn f() -> i64 { return g(); }",
            "f",
            &[] // g not in the known set
        ));
    }

    // 7. shadowing a let across sibling scopes is not eligible (flattened LIR)
    #[test]
    fn e07_shadowing_ineligible() {
        let src = "fn f(c: bool) -> i64 { let x = 1; if c { let x = 2; print(x); } return x; }";
        assert!(!eligible(src, "f", &["f"]));
    }

    // 8. while/for loops stay eligible (control flow is blocks, not exprs)
    #[test]
    fn e08_loops_eligible() {
        let src =
            "fn sum_to(n: i64) -> i64 { let mut t = 0; for i in 0..n { t = t + i; } return t; }";
        assert!(eligible(src, "sum_to", &["sum_to"]));
    }

    // 9. array-typed values are not eligible
    #[test]
    fn e09_arrays_ineligible() {
        let src = "fn f() -> i64 { let xs = [1, 2]; return xs.len(); }";
        assert!(!eligible(src, "f", &["f"]));
    }

    // 10. f64 arithmetic + comparison is eligible
    #[test]
    fn e10_f64_eligible() {
        let src = "fn half(x: f64) -> bool { return x / 2.0 > 1.0; }";
        assert!(eligible(src, "half", &["half"]));
    }

    // 11. reference-mode parameters are rejected by the HIR itself — the
    // eligibility check never consults the AST declaration (willow-0g8j fix).
    #[test]
    fn e11_reference_params_ineligible_via_hir() {
        let src = "fn bump(n: &mut i64) { n = n + 1; }";
        assert!(!eligible(src, "bump", &["bump"]));
        let src2 = "fn read(n: &i64) -> i64 { return n; }";
        assert!(!eligible(src2, "read", &["read"]));
    }

    // 12. short-circuit && / || are now eligible (lazy block emission)
    #[test]
    fn e12_short_circuit_eligible() {
        assert!(eligible(
            "fn f(a: bool, b: bool) -> bool { return a && b || !a; }",
            "f",
            &["f"]
        ));
    }

    // 13. scalar ternaries are eligible
    #[test]
    fn e13_ternary_eligible() {
        assert!(eligible(
            "fn f(c: bool) -> i64 { return c ? 1 : 2; }",
            "f",
            &["f"]
        ));
    }

    // 14. (updated by willow-0g8j.1) a String ternary is now eligible
    #[test]
    fn e14_string_ternary_now_eligible() {
        let src = "fn f(c: bool) -> String { let s = c ? \"a\" : \"b\"; return s; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // ---------------------------------------------------------------------
    // willow-0g8j.1 — GC-managed values and rooting in the LIR walker.
    //
    // Perspectives 1-12 below are the *eligibility* half (which functions the
    // LIR path claims); perspectives 13-32 live in
    // `tests/integration/lir_backend.rs` as differential and GC-stress runs,
    // because they are about emitted code, not about the predicate.
    //
    //  1. a String parameter/return function is eligible
    //  2. String concatenation is eligible
    //  3. String equality/inequality is eligible
    //  4. `println` of a String is eligible
    //  5. a String ternary is eligible
    //  6. mixed scalar + String locals in one function are eligible
    //  7. a String `let` that is reassigned in a loop is eligible
    //  8. calling a String-returning function is eligible
    //  9. a `let` shadowing a PARAMETER is rejected (flattened scopes)
    // 10. a bare enum variant `Var` is rejected (needs the AST special case)
    // 11. an unsupported String operator (`<`) is rejected
    // 12. arrays / class objects / interfaces still fall back
    // ---------------------------------------------------------------------

    // 15. String parameters and returns are eligible
    #[test]
    fn e15_string_param_and_return_eligible() {
        let src = "fn id(s: String) -> String { return s; }";
        assert!(eligible(src, "id", &["id"]));
    }

    // 16. concatenation of strings is eligible
    #[test]
    fn e16_string_concat_eligible() {
        let src = "fn join(a: String, b: String) -> String { return a + b; }";
        assert!(eligible(src, "join", &["join"]));
    }

    // 17. string equality and inequality are eligible
    #[test]
    fn e17_string_compare_eligible() {
        let eq = "fn f(a: String, b: String) -> bool { return a == b; }";
        assert!(eligible(eq, "f", &["f"]));
        let ne = "fn f(a: String, b: String) -> bool { return a != b; }";
        assert!(eligible(ne, "f", &["f"]));
    }

    // 18. a String local reassigned inside a loop is eligible — the case the
    // entry-rooted slot design exists for (a per-`let` root would grow the
    // shadow stack once per iteration).
    #[test]
    fn e18_string_loop_accumulator_eligible() {
        let src = "fn rep(n: i64) -> String { let mut s = \"\"; let mut i = 0; \
                   while i < n { s = s + \"x\"; i = i + 1; } return s; }";
        assert!(eligible(src, "rep", &["rep"]));
    }

    // 19. mixed scalar and String locals in one function are eligible
    #[test]
    fn e19_mixed_scalar_and_gc_eligible() {
        let src = "fn f(n: i64) -> String { let tag = \"n=\"; let doubled = n * 2; \
                   let ok = doubled > 0; return ok ? tag : \"\"; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 20. a call that both takes and returns a String is eligible
    #[test]
    fn e20_string_call_eligible() {
        let src = "fn wrap(s: String) -> String { return \"[\" + s + \"]\"; } \
                   fn f() -> String { return wrap(\"a\"); }";
        assert!(eligible(src, "f", &["f", "wrap"]));
    }

    // 21. a `let` shadowing a PARAMETER is rejected: LIR has no block scopes,
    // so one name cannot have two storages (willow-0g8j.1).
    #[test]
    fn e21_let_shadowing_param_ineligible() {
        let src = "fn f(s: String) -> String { let s = \"other\"; return s; }";
        assert!(!eligible(src, "f", &["f"]));
    }

    // 22. an enum value never reaches the LIR walker: the bare variant form
    // does not survive HIR lowering at all, and the qualified form is not a
    // supported expression. Both are checked so that a future lowering change
    // cannot quietly hand the walker a `Var` it would resolve to a local (the
    // `names` guard in `lir_supported_function` is the backstop).
    #[test]
    fn e22_enum_variant_never_reaches_walker() {
        let bare = "enum Status { Open, Closed } fn f() -> Status { return Closed; }";
        let tokens = Lexer::new(bare).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (_, diags) = crate::ir::lower::lower_program(&program);
        assert!(!diags.is_empty(), "bare variant unexpectedly lowered");

        let qualified = "enum Status { Open, Closed } fn f() -> Status { return Status::Closed; }";
        assert!(!eligible_lenient(qualified, "f", &["f"]));
    }

    // 23. an ordering operator on strings is not emitted, so it is rejected
    // even though both operand types are supported.
    #[test]
    fn e23_string_ordering_ineligible() {
        let src = "fn f(a: String, b: String) -> bool { return a < b; }";
        // The checker may reject this outright; if it lowers, we must not claim it.
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 24. arrays of strings still fall back (no array expression support yet)
    #[test]
    fn e24_string_array_still_ineligible() {
        let src = "fn f() -> i64 { let xs = [\"a\", \"b\"]; return xs.len(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 25. class objects still fall back (no allocation/field access yet)
    #[test]
    fn e25_class_object_still_ineligible() {
        let src = "class Item { name: String; pub static fn make(n: String) -> Item \
                   { return new Item(n); } } \
                   fn f() -> Item { return Item::make(\"a\"); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }
}
