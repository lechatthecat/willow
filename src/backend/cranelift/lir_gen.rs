//! LIR-walking code generation — willow-0g8j.
//!
//! First stage of migrating the emit layer off the raw AST: a function whose
//! lowered IR uses only the supported scalar subset is compiled by walking its
//! [`LirFunction`] basic blocks directly (typed [`HirExpr`] trees inside), so
//! the backend never touches the AST body for it. Everything else falls back to
//! the existing AST-walking path, chosen per function in
//! `compile_function_named`. `WILLOW_LIR_BACKEND=0` disables the LIR path.
//!
//! Supported subset (v1): `i64`/`f64`/`bool` values; literals, variables,
//! arithmetic/comparison, unary ops; direct calls to known non-async
//! functions; `print`/`println` of scalars; `let`/assign; the full block
//! control flow (jump/branch/return). GC-managed values, classes, async,
//! lambdas, and short-circuit operators stay on the AST path for now.

use std::collections::HashSet;

use cranelift_codegen::ir::{InstBuilder, condcodes::FloatCC, condcodes::IntCC, types};
use cranelift_module::Module;

use crate::ir::lowered::{LirBlock, LirFunction, LirInst, Terminator};
use crate::ir::typed_ast::{HirExpr, HirExprKind};
use crate::parser::ast::{BinOp, Type, UnaryOp};

use super::type_helpers::clif_type;
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

/// Conservative eligibility: every type, instruction, and expression must be in
/// the supported subset, every callee must be a known symbol, and `let` names
/// must be unique across the function (LIR flattens block scopes, so shadowing
/// across sibling scopes would alias one variable).
pub(super) fn lir_supported_function(f: &LirFunction, known_fn: &dyn Fn(&str) -> bool) -> bool {
    if !(scalar(&f.return_type) || f.return_type == Type::Void) {
        return false;
    }
    // Reference parameters (`&`/`&mut`) are pointers at the ABI level.
    if !f.params.iter().all(|p| scalar(&p.ty) && !p.by_reference) {
        return false;
    }
    let mut let_names: HashSet<&str> = HashSet::new();
    for block in &f.blocks {
        for inst in &block.instrs {
            match inst {
                LirInst::Let { name, value, .. } => {
                    if !let_names.insert(name.as_str()) {
                        return false; // shadowing across flattened scopes
                    }
                    if !supported_expr(value, known_fn) {
                        return false;
                    }
                }
                LirInst::Assign { value, .. } => {
                    if !supported_expr(value, known_fn) {
                        return false;
                    }
                }
                LirInst::Expr(e) => {
                    if !supported_expr(e, known_fn) {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        match &block.terminator {
            Terminator::Branch { cond, .. } => {
                if !supported_expr(cond, known_fn) {
                    return false;
                }
            }
            Terminator::Return(Some(v)) => {
                if !supported_expr(v, known_fn) {
                    return false;
                }
            }
            Terminator::Jump(_) | Terminator::Return(None) => {}
        }
    }
    true
}

fn supported_expr(e: &HirExpr, known_fn: &dyn Fn(&str) -> bool) -> bool {
    if !(scalar(&e.ty) || e.ty == Type::Void) {
        return false;
    }
    match &e.kind {
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) => true,
        HirExprKind::Var(_) => true,
        HirExprKind::Binary { lhs, rhs, .. } => {
            supported_expr(lhs, known_fn) && supported_expr(rhs, known_fn)
        }
        HirExprKind::Ternary {
            condition,
            then_expr,
            else_expr,
        } => {
            supported_expr(condition, known_fn)
                && supported_expr(then_expr, known_fn)
                && supported_expr(else_expr, known_fn)
        }
        HirExprKind::Unary { operand, .. } => supported_expr(operand, known_fn),
        HirExprKind::Call { callee, args } => {
            known_fn(callee.as_str()) && args.iter().all(|a| supported_expr(a, known_fn))
        }
        HirExprKind::Print { value, newline: _ } => {
            scalar(&value.ty) && supported_expr(value, known_fn)
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
                    if let Some(VarStorage::Value { var, .. }) = self.vars.get(name.as_str()) {
                        let var = *var;
                        self.builder.def_var(var, val);
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
                let val = self.emit_lir_expr(v);
                self.builder.ins().return_(&[val]);
            }
            Terminator::Return(None) => {
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
            HirExprKind::Var(name) => match self.vars.get(name.as_str()) {
                Some(VarStorage::Value { var, .. }) => {
                    let var = *var;
                    self.builder.use_var(var)
                }
                _ => self.builder.ins().iconst(clif_type(&e.ty), 0),
            },
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
                let vals: Vec<_> = args.iter().map(|a| self.emit_lir_expr(a)).collect();
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
                    _ => unreachable!("non-scalar print passed eligibility"),
                };
                let fid = self.func_ids[fn_name];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[val]);
                self.builder.ins().iconst(types::I8, 0)
            }
            _ => unreachable!("unsupported LIR expression reached emission"),
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

    // 4. string values are not eligible
    #[test]
    fn e04_string_ineligible() {
        assert!(!eligible("fn s() { println(\"hi\"); }", "s", &["s"]));
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

    // 14. a ternary with a non-scalar branch stays ineligible
    #[test]
    fn e14_string_ternary_ineligible() {
        let src = "fn a() -> String { return \"a\"; } fn b() -> String { return \"b\"; } \
                   fn f(c: bool) -> String { return c ? a() : b(); }";
        assert!(!eligible(src, "f", &["a", "b", "f"]));
    }
}
