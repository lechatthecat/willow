//! Human-readable dump of the typed HIR — willow-mb5.
//!
//! Every expression is rendered with a trailing `: <type>` so the dump makes
//! the whole point of the HIR visible: types are attached to nodes, not
//! re-derived. Used by the `--emit-hir` build flag.

use crate::parser::ast::{BinOp, Type, UnaryOp};

use super::typed_ast::{HirExpr, HirExprKind, HirFunction, HirPattern, HirProgram, HirStmt};

/// Render a whole HIR program as indented pseudo-code with inline types.
pub fn format_program(program: &HirProgram) -> String {
    let mut out = String::new();
    for f in &program.functions {
        format_function(f, 0, &mut out);
    }
    for c in &program.classes {
        out.push_str(&format!("class {} {{\n", c.name));
        for method in &c.methods {
            format_function(method, 1, &mut out);
        }
        out.push_str("}\n");
    }
    out
}

fn format_function(f: &HirFunction, level: usize, out: &mut String) {
    let params = f
        .params
        .iter()
        .map(|p| {
            let amp = if p.by_reference { "&" } else { "" };
            format!("{}: {amp}{}", p.name, type_str(&p.ty))
        })
        .collect::<Vec<_>>()
        .join(", ");
    indent(level, out);
    out.push_str(&format!(
        "fn {}({}) -> {} {{\n",
        f.name,
        params,
        type_str(&f.return_type)
    ));
    for stmt in &f.body {
        format_stmt(stmt, level + 1, out);
    }
    indent(level, out);
    out.push_str("}\n");
}

fn indent(level: usize, out: &mut String) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn format_stmt(stmt: &HirStmt, level: usize, out: &mut String) {
    indent(level, out);
    match stmt {
        HirStmt::Break { .. } => out.push_str("break;\n"),
        HirStmt::Continue { .. } => out.push_str("continue;\n"),
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
        HirStmt::For {
            name,
            iterable,
            body,
            ..
        } => {
            out.push_str(&format!("for {name} in {} {{\n", format_expr(iterable)));
            for s in body {
                format_stmt(s, level + 1, out);
            }
            indent(level, out);
            out.push_str("}\n");
        }
        HirStmt::FieldAssign {
            object,
            field,
            value,
            ..
        } => {
            out.push_str(&format!(
                "{}.{field} = {};\n",
                format_expr(object),
                format_expr(value)
            ));
        }
        HirStmt::IndexAssign {
            array,
            index,
            value,
            ..
        } => {
            out.push_str(&format!(
                "{}[{}] = {};\n",
                format_expr(array),
                format_expr(index),
                format_expr(value)
            ));
        }
        HirStmt::StaticFieldAssign {
            class,
            field,
            value,
            ..
        } => {
            out.push_str(&format!("{class}::{field} = {};\n", format_expr(value)));
        }
        HirStmt::SuperInit { args, .. } => {
            let args = args.iter().map(format_expr).collect::<Vec<_>>().join(", ");
            out.push_str(&format!("super.init({args});\n"));
        }
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
        HirExprKind::Array { elements } => {
            let items = elements
                .iter()
                .map(format_expr)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{items}]")
        }
        HirExprKind::Index { array, index } => {
            format!("{}[{}]", format_expr(array), format_expr(index))
        }
        HirExprKind::Ternary {
            condition,
            then_expr,
            else_expr,
        } => format!(
            "({} ? {} : {})",
            format_expr(condition),
            format_expr(then_expr),
            format_expr(else_expr)
        ),
        HirExprKind::New { class, args } => {
            let args = args.iter().map(format_expr).collect::<Vec<_>>().join(", ");
            format!("new {class}({args})")
        }
        HirExprKind::FieldAccess { object, field } => {
            format!("{}.{field}", format_expr(object))
        }
        HirExprKind::MethodCall {
            object,
            method,
            args,
        } => {
            let args = args.iter().map(format_expr).collect::<Vec<_>>().join(", ");
            format!("{}.{method}({args})", format_expr(object))
        }
        HirExprKind::ObjectLiteral { class, fields } => {
            let items = fields
                .iter()
                .map(|(name, value)| format!("{name}: {}", format_expr(value)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{class} {{ {items} }}")
        }
        HirExprKind::Nil => "nil".to_string(),
        HirExprKind::StaticField { class, field } => format!("{class}::{field}"),
        HirExprKind::StaticCall {
            class,
            method,
            args,
        } => {
            let args = args.iter().map(format_expr).collect::<Vec<_>>().join(", ");
            format!("{class}::{method}({args})")
        }
        HirExprKind::Range { start, end } => {
            format!("({}..{})", format_expr(start), format_expr(end))
        }
        HirExprKind::Await { inner } => format!("await {}", format_expr(inner)),
        HirExprKind::TryPropagate { inner } => format!("{}?", format_expr(inner)),
        HirExprKind::Lambda { params, body } => {
            let params = params
                .iter()
                .map(|p| format!("{}: {}", p.name, type_str(&p.ty)))
                .collect::<Vec<_>>()
                .join(", ");
            let mut body_text = String::new();
            for s in body {
                format_stmt(s, 0, &mut body_text);
            }
            format!(
                "|{params}| {{ {} }}",
                body_text.trim_end().replace('\n', " ")
            )
        }
        HirExprKind::Match { scrutinee, arms } => {
            let arms = arms
                .iter()
                .map(|arm| {
                    let mut body_text = String::new();
                    for s in &arm.body {
                        format_stmt(s, 0, &mut body_text);
                    }
                    format!(
                        "{} => {{ {} }}",
                        format_pattern(&arm.pattern),
                        body_text.trim_end().replace('\n', " ")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("match {} {{ {arms} }}", format_expr(scrutinee))
        }
    };
    format!("{inner}: {}", type_str(&e.ty))
}

/// Expression rendering shared with the LIR dump (`lowered.rs`).
pub(crate) fn expr_text(e: &HirExpr) -> String {
    format_expr(e)
}

/// Type rendering shared with the LIR dump (`lowered.rs`).
pub(crate) fn type_text(ty: &Type) -> String {
    type_str(ty)
}

fn format_pattern(p: &HirPattern) -> String {
    match p {
        HirPattern::Wildcard => "_".to_string(),
        HirPattern::Binding { name, ty } => format!("{name}: {}", type_str(ty)),
        HirPattern::LiteralBool(b) => b.to_string(),
        HirPattern::LiteralInt(n) => n.to_string(),
        HirPattern::EnumVariant { enum_name, variant } => format!("{enum_name}::{variant}"),
        HirPattern::EnumVariantTuple {
            enum_name,
            variant,
            bindings,
        } => {
            let bindings = bindings
                .iter()
                .map(|(name, ty)| format!("{name}: {}", type_str(ty)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{enum_name}::{variant}({bindings})")
        }
        HirPattern::ClassDowncast {
            class_name,
            binding,
        } => format!("{class_name}({binding})"),
    }
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

    // 7. array literal renders with its element types and Array<_> type
    #[test]
    fn dump_07_array_literal() {
        let text = dump("fn f() { let xs = [1, 2]; }");
        assert!(
            text.contains("let xs = [1: i64, 2: i64]: Array<i64>;"),
            "{text}"
        );
    }

    // 8. index renders array and index with the element type
    #[test]
    fn dump_08_index() {
        let text = dump("fn f() -> i64 { let xs = [7]; return xs[0]; }");
        assert!(
            text.contains("return xs: Array<i64>[0: i64]: i64;"),
            "{text}"
        );
    }

    // 9. ternary renders cond/then/else with the branch type
    #[test]
    fn dump_09_ternary() {
        let text = dump("fn f(c: bool) -> i64 { return c ? 1 : 2; }");
        assert!(
            text.contains("return (c: bool ? 1: i64 : 2: i64): i64;"),
            "{text}"
        );
    }

    // 10. `new` renders with typed args and the class type
    #[test]
    fn dump_10_new() {
        let text = dump("class Box { pub v: i64; } fn f() { let b = new Box(7); }");
        assert!(text.contains("let b = new Box(7: i64): Box;"), "{text}");
    }

    // 11. field access renders object.field with the field type
    #[test]
    fn dump_11_field_access() {
        let text =
            dump("class Box { pub v: i64; } fn f() -> i64 { let b = new Box(7); return b.v; }");
        assert!(text.contains("return b: Box.v: i64;"), "{text}");
    }

    // 12. method call renders object.method(args) with the return type
    #[test]
    fn dump_12_method_call() {
        let text = dump(
            "class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } } \
             fn f() -> i64 { let b = new Box(7); return b.get(); }",
        );
        assert!(text.contains("return b: Box.get(): i64;"), "{text}");
    }

    // 13. a class method body renders nested under the class with a typed `self`
    #[test]
    fn dump_13_class_method_body() {
        let text = dump("class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } }");
        assert!(text.contains("class Box {"), "{text}");
        assert!(text.contains("  fn get(self: Box) -> i64 {"), "{text}");
        assert!(text.contains("    return self: Box.v: i64;"), "{text}");
    }

    // 14. a `for` loop renders with a typed iterable and body
    #[test]
    fn dump_14_for_loop() {
        let text = dump("fn f() { let xs = [1]; for v in xs { print(v); } }");
        assert!(text.contains("for v in xs: Array<i64> {"), "{text}");
        assert!(text.contains("    print(v: i64): void;"), "{text}");
    }

    // 15. an object literal renders class + typed field values
    #[test]
    fn dump_15_object_literal() {
        let text = dump("class P { x: i64; } fn f() { let p = P { x: 1 }; }");
        assert!(text.contains("let p = P { x: 1: i64 }: P;"), "{text}");
    }

    // 16. static field read and static call render with `::`
    #[test]
    fn dump_16_static_members() {
        let read = dump("class C { static v: i64 = 0; } fn f() -> i64 { return C::v; }");
        assert!(read.contains("return C::v: i64;"), "{read}");
        let call = dump(
            "class C { static fn make() -> i64 { return 1; } } fn f() -> i64 { return C::make(); }",
        );
        assert!(call.contains("return C::make(): i64;"), "{call}");
    }

    // 17. assignment statements render field/index/static targets
    #[test]
    fn dump_17_assignments() {
        let text = dump(
            "class C { x: i64; static mut t: i64 = 0; } \
             fn f() { let p = new C(1); p.x = 2; let xs = [1]; xs[0] = 9; C::t = 5; }",
        );
        assert!(text.contains("p: C.x = 2: i64;"), "{text}");
        assert!(text.contains("xs: Array<i64>[0: i64] = 9: i64;"), "{text}");
        assert!(text.contains("C::t = 5: i64;"), "{text}");
    }

    // 18. range and await render with their types
    #[test]
    fn dump_18_range_and_await() {
        let text = dump(
            "async fn g() -> i64 { return 1; } \
             async fn f() -> i64 { for i in 0..2 { print(i); } return await g(); }",
        );
        assert!(
            text.contains("for i in (0: i64..2: i64): Range<i64> {"),
            "{text}"
        );
        assert!(text.contains("return await g(): Task<i64>: i64;"), "{text}");
    }

    // 19. `?` renders with the unwrapped success type
    #[test]
    fn dump_19_try_propagate() {
        let text = dump("fn f(r: Result<i64, String>) -> i64 { return r?; }");
        assert!(
            text.contains("return r: Result<i64, String>?: i64;"),
            "{text}"
        );
    }

    // 20. a lambda renders params, body, and its fn type
    #[test]
    fn dump_20_lambda() {
        let text = dump("fn f() { let d = |x: i64| x * 2; }");
        assert!(
            text.contains("let d = |x: i64| { return (x: i64 * 2: i64): i64; }: fn(i64) -> i64;"),
            "{text}"
        );
    }

    // 21. super.init renders inside a lowered constructor
    #[test]
    fn dump_21_super_init() {
        let text = dump(
            "open class A { v: i64; init(self, v: i64) { self.v = v; } } \
             class B extends A { init(self, v: i64) { super.init(v); } }",
        );
        assert!(text.contains("super.init(v: i64);"), "{text}");
        assert!(
            text.contains("fn init(self: B, v: i64) -> void {"),
            "{text}"
        );
    }

    // 22. match renders arms with typed pattern bindings and the arm type
    #[test]
    fn dump_22_match() {
        let text =
            dump("fn f(o: Option<i64>) -> i64 { return match o { Some(v) => v, None => -1, }; }");
        assert!(text.contains("match o: Option<i64> {"), "{text}");
        assert!(
            text.contains("Option::Some(v: i64) => { v: i64; }"),
            "{text}"
        );
        assert!(text.contains("Option::None => {"), "{text}");
    }

    // 23. builtin collection methods render with their types
    #[test]
    fn dump_23_builtin_methods() {
        let text = dump("fn f() -> i64 { let xs = [1, 2]; return xs.len(); }");
        assert!(text.contains("return xs: Array<i64>.len(): i64;"), "{text}");
    }

    // 24. enum variant construction renders with the enum type
    #[test]
    fn dump_24_enum_construction() {
        let text = dump("enum Color { Red, Rgb(i64), } fn f() -> Color { return Color::Rgb(7); }");
        assert!(text.contains("return Color::Rgb(7: i64): Color;"), "{text}");
    }
}
