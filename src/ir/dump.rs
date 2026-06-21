//! Human-readable dump of the typed HIR — willow-mb5.
//!
//! Every expression is rendered with a trailing `: <type>` so the dump makes
//! the whole point of the HIR visible: types are attached to nodes, not
//! re-derived. Used by the `--emit-hir` build flag.

use crate::parser::ast::{BinOp, Type, UnaryOp};

use super::typed_ast::{HirExpr, HirExprKind, HirProgram, HirStmt};

/// Render a whole HIR program as indented pseudo-code with inline types.
pub fn format_program(program: &HirProgram) -> String {
    let mut out = String::new();
    for f in &program.functions {
        let params = f
            .params
            .iter()
            .map(|p| format!("{}: {}", p.name, type_str(&p.ty)))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "fn {}({}) -> {} {{\n",
            f.name,
            params,
            type_str(&f.return_type)
        ));
        for stmt in &f.body {
            format_stmt(stmt, 1, &mut out);
        }
        out.push_str("}\n");
    }
    out
}

fn indent(level: usize, out: &mut String) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn format_stmt(stmt: &HirStmt, level: usize, out: &mut String) {
    indent(level, out);
    match stmt {
        HirStmt::Let {
            name,
            mutable,
            value,
            ..
        } => {
            let kw = if *mutable { "let mut" } else { "let" };
            out.push_str(&format!("{kw} {name} = {};\n", format_expr(value)));
        }
        HirStmt::Assign { name, value, .. } => {
            out.push_str(&format!("{name} = {};\n", format_expr(value)));
        }
        HirStmt::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            out.push_str(&format!("if {} {{\n", format_expr(cond)));
            for s in then_branch {
                format_stmt(s, level + 1, out);
            }
            indent(level, out);
            out.push('}');
            if let Some(else_branch) = else_branch {
                out.push_str(" else {\n");
                for s in else_branch {
                    format_stmt(s, level + 1, out);
                }
                indent(level, out);
                out.push('}');
            }
            out.push('\n');
        }
        HirStmt::While { cond, body, .. } => {
            out.push_str(&format!("while {} {{\n", format_expr(cond)));
            for s in body {
                format_stmt(s, level + 1, out);
            }
            indent(level, out);
            out.push_str("}\n");
        }
        HirStmt::Return { value, .. } => match value {
            Some(v) => out.push_str(&format!("return {};\n", format_expr(v))),
            None => out.push_str("return;\n"),
        },
        HirStmt::Expr(e) => out.push_str(&format!("{};\n", format_expr(e))),
    }
}

/// Render an expression with its resolved type as a `<expr>: <type>` suffix.
fn format_expr(e: &HirExpr) -> String {
    let inner = match &e.kind {
        HirExprKind::Int(n) => n.to_string(),
        HirExprKind::Float(f) => format!("{f:?}"),
        HirExprKind::Bool(b) => b.to_string(),
        HirExprKind::Str(s) => format!("{s:?}"),
        HirExprKind::Var(name) => name.clone(),
        HirExprKind::Binary { op, lhs, rhs } => {
            format!(
                "({} {} {})",
                format_expr(lhs),
                binop_str(op),
                format_expr(rhs)
            )
        }
        HirExprKind::Unary { op, operand } => {
            format!("{}{}", unaryop_str(op), format_expr(operand))
        }
        HirExprKind::Call { callee, args } => {
            let args = args.iter().map(format_expr).collect::<Vec<_>>().join(", ");
            format!("{callee}({args})")
        }
        HirExprKind::Print { value, newline } => {
            let name = if *newline { "println" } else { "print" };
            format!("{name}({})", format_expr(value))
        }
    };
    format!("{inner}: {}", type_str(&e.ty))
}

fn binop_str(op: &BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
    }
}

fn unaryop_str(op: &UnaryOp) -> &'static str {
    match op {
        UnaryOp::Neg => "-",
        UnaryOp::Not => "!",
    }
}

fn type_str(ty: &Type) -> String {
    match ty {
        Type::I64 => "i64".to_string(),
        Type::F64 => "f64".to_string(),
        Type::Bool => "bool".to_string(),
        Type::String => "String".to_string(),
        Type::Void => "void".to_string(),
        Type::Nil => "nil".to_string(),
        Type::Never => "never".to_string(),
        Type::Named(name) => name.clone(),
        Type::Array(inner) => format!("Array<{}>", type_str(inner)),
        Type::Generic(name, args) => {
            let args = args.iter().map(type_str).collect::<Vec<_>>().join(", ");
            format!("{name}<{args}>")
        }
        Type::Nullable(inner) => format!("{}?", type_str(inner)),
        Type::Fn(params, ret) => {
            let params = params.iter().map(type_str).collect::<Vec<_>>().join(", ");
            format!("fn({params}) -> {}", type_str(ret))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::lower::lower_program;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn dump(src: &str) -> String {
        let tokens = Lexer::new(src).tokenize().expect("lexing failed");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "parse errors: {errs:?}");
        let (hir, diags) = lower_program(&program);
        assert!(diags.is_empty(), "lowering diagnostics: {diags:?}");
        format_program(&hir)
    }

    // 1. function signature renders params + return type
    #[test]
    fn dump_01_signature() {
        let text = dump("fn add(a: i64, b: i64) -> i64 { return a + b; }");
        assert!(text.contains("fn add(a: i64, b: i64) -> i64 {"), "{text}");
    }

    // 2. every expression carries its type as a `: ty` suffix
    #[test]
    fn dump_02_types_on_expressions() {
        let text = dump("fn add(a: i64, b: i64) -> i64 { return a + b; }");
        assert!(text.contains("return (a: i64 + b: i64): i64;"), "{text}");
    }

    // 3. comparison renders as bool
    #[test]
    fn dump_03_comparison_is_bool() {
        let text = dump("fn f(a: i64) -> bool { return a < 10; }");
        assert!(text.contains("(a: i64 < 10: i64): bool"), "{text}");
    }

    // 4. if/return nest with indentation and a typed condition
    #[test]
    fn dump_04_if_block() {
        let text = dump("fn f(n: i64) -> i64 { if n <= 1 { return n; } return 0; }");
        assert!(text.contains("if (n: i64 <= 1: i64): bool {"), "{text}");
        assert!(text.contains("    return n: i64;"), "{text}");
    }

    // 5. calls render with typed arguments and the callee return type
    #[test]
    fn dump_05_call() {
        let text = dump("fn g(x: i64) -> i64 { return x; } fn f() -> i64 { return g(2); }");
        assert!(text.contains("return g(2: i64): i64;"), "{text}");
    }

    // 6. print renders with void type
    #[test]
    fn dump_06_print_void() {
        let text = dump("fn f() { print(1); }");
        assert!(text.contains("print(1: i64): void;"), "{text}");
    }
}
