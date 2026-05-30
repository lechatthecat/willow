use crate::parser::ast::{
    Block, ClassDecl, Expr, FunctionDecl, Item, LambdaBody, MatchBody, MethodDecl, Program, Stmt,
};

/// Holds the source text for a single file, enabling line/column lookups.
pub struct SourceMap {
    pub path: String,
    pub source: String,
    line_offsets: Vec<usize>,
}

impl SourceMap {
    pub fn new(path: impl Into<String>, source: impl Into<String>) -> Self {
        let source = source.into();
        let mut offsets = vec![0usize];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                offsets.push(i + 1);
            }
        }
        Self {
            path: path.into(),
            source,
            line_offsets: offsets,
        }
    }

    /// Returns the text of line `line` (1-indexed). Empty string if out of range.
    pub fn line_text(&self, line: usize) -> &str {
        if line == 0 {
            return "";
        }
        let idx = line - 1;
        let start = match self.line_offsets.get(idx) {
            Some(&s) => s,
            None => return "",
        };
        let end = self
            .line_offsets
            .get(idx + 1)
            .map(|&e| e.saturating_sub(1))
            .unwrap_or(self.source.len());
        self.source
            .get(start..end)
            .unwrap_or("")
            .trim_end_matches('\r')
    }

    /// Returns the byte offset of the start of line `line` (1-indexed).
    pub fn line_start(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        self.line_offsets
            .get(line - 1)
            .copied()
            .unwrap_or(self.source.len())
    }

    pub fn total_lines(&self) -> usize {
        self.line_offsets.len()
    }
}

/// Debug-build metadata that preserves the source positions needed by later
/// debugging/runtime reporting stages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugSourceMap {
    pub file: String,
    pub total_lines: usize,
    pub classes: Vec<DebugClass>,
    pub functions: Vec<DebugFunction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugClass {
    pub name: String,
    pub line: usize,
    pub col: usize,
    pub fields: Vec<DebugField>,
    pub methods: Vec<DebugMethod>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugField {
    pub name: String,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugMethod {
    pub name: String,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugFunction {
    pub name: String,
    pub is_async: bool,
    pub line: usize,
    pub col: usize,
    pub await_points: Vec<DebugAwaitPoint>,
    pub statements: Vec<DebugStatement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugAwaitPoint {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugStatement {
    pub kind: String,
    pub line: usize,
    pub col: usize,
}

impl DebugSourceMap {
    pub fn from_program(
        file: impl Into<String>,
        total_lines: usize,
        program: &Program,
    ) -> DebugSourceMap {
        let mut classes = Vec::new();
        let mut functions = Vec::new();
        for item in &program.items {
            match item {
                Item::Function(function) => {
                    functions.push(DebugFunction::from_function(function));
                }
                Item::Class(class) => {
                    classes.push(DebugClass::from_class(class));
                    for method in &class.methods {
                        functions.push(DebugFunction::from_method(&class.name, method));
                    }
                }
                Item::Enum(_) => {}
            }
        }

        DebugSourceMap {
            file: file.into(),
            total_lines,
            classes,
            functions,
        }
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("willow_debug_source_map_v1\n");
        out.push_str(&format!("file={}\n", self.file));
        out.push_str(&format!("total_lines={}\n", self.total_lines));

        for class in &self.classes {
            out.push('\n');
            out.push_str(&format!(
                "class name={} line={} col={}\n",
                class.name, class.line, class.col
            ));
            out.push_str(&format!("  gc_type name={}\n", class.name));
            for field in &class.fields {
                out.push_str(&format!(
                    "  field name={} line={} col={}\n",
                    field.name, field.line, field.col
                ));
            }
            for method in &class.methods {
                out.push_str(&format!(
                    "  method name={} line={} col={}\n",
                    method.name, method.line, method.col
                ));
            }
        }

        for function in &self.functions {
            out.push('\n');
            out.push_str(&format!(
                "function name={} line={} col={}\n",
                function.name, function.line, function.col
            ));
            if function.is_async {
                out.push_str("  async=true\n");
                out.push_str(&format!("  async_stack_frame name={}\n", function.name));
            }
            for await_point in &function.await_points {
                out.push_str(&format!(
                    "  await line={} col={}\n",
                    await_point.line, await_point.col
                ));
            }
            for statement in &function.statements {
                out.push_str(&format!(
                    "  statement kind={} line={} col={}\n",
                    statement.kind, statement.line, statement.col
                ));
            }
        }

        out
    }
}

impl DebugClass {
    fn from_class(class: &ClassDecl) -> DebugClass {
        DebugClass {
            name: class.name.clone(),
            line: class.span.line,
            col: class.span.col,
            fields: class
                .fields
                .iter()
                .map(|field| DebugField {
                    name: field.name.clone(),
                    line: field.span.line,
                    col: field.span.col,
                })
                .collect(),
            methods: class
                .methods
                .iter()
                .map(|method| DebugMethod {
                    name: method.name.clone(),
                    line: method.span.line,
                    col: method.span.col,
                })
                .collect(),
        }
    }
}

impl DebugFunction {
    fn from_function(function: &FunctionDecl) -> DebugFunction {
        DebugFunction {
            name: function.name.clone(),
            is_async: function.is_async,
            line: function.span.line,
            col: function.span.col,
            await_points: collect_debug_await_points(&function.body),
            statements: collect_debug_statements(&function.body),
        }
    }

    fn from_method(class_name: &str, method: &MethodDecl) -> DebugFunction {
        DebugFunction {
            name: format!("{class_name}::{}", method.name),
            is_async: method.is_async,
            line: method.span.line,
            col: method.span.col,
            await_points: collect_debug_await_points(&method.body),
            statements: collect_debug_statements(&method.body),
        }
    }
}

fn collect_debug_await_points(block: &Block) -> Vec<DebugAwaitPoint> {
    let mut await_points = Vec::new();
    collect_block_await_points(block, &mut await_points);
    await_points
}

fn collect_debug_statements(block: &Block) -> Vec<DebugStatement> {
    let mut statements = Vec::new();
    collect_block_statements(block, &mut statements);
    statements
}

fn collect_block_statements(block: &Block, statements: &mut Vec<DebugStatement>) {
    for stmt in &block.stmts {
        let span = stmt_span(stmt);
        statements.push(DebugStatement {
            kind: stmt_kind(stmt).to_string(),
            line: span.line,
            col: span.col,
        });

        match stmt {
            Stmt::If(if_stmt) => {
                collect_block_statements(&if_stmt.then_block, statements);
                if let Some(else_block) = &if_stmt.else_block {
                    collect_block_statements(else_block, statements);
                }
            }
            Stmt::While(while_stmt) => collect_block_statements(&while_stmt.body, statements),
            Stmt::Let(_) | Stmt::Assign(_) | Stmt::FieldAssign(_) | Stmt::Return(_) | Stmt::Expr(_) => {}
        }
    }
}

fn collect_block_await_points(block: &Block, await_points: &mut Vec<DebugAwaitPoint>) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Let(stmt) => collect_expr_await_points(&stmt.init, await_points),
            Stmt::Assign(stmt) => collect_expr_await_points(&stmt.value, await_points),
            Stmt::FieldAssign(stmt) => {
                collect_expr_await_points(&stmt.object, await_points);
                collect_expr_await_points(&stmt.value, await_points);
            }
            Stmt::If(stmt) => {
                collect_expr_await_points(&stmt.cond, await_points);
                collect_block_await_points(&stmt.then_block, await_points);
                if let Some(else_block) = &stmt.else_block {
                    collect_block_await_points(else_block, await_points);
                }
            }
            Stmt::While(stmt) => {
                collect_expr_await_points(&stmt.cond, await_points);
                collect_block_await_points(&stmt.body, await_points);
            }
            Stmt::Return(stmt) => {
                if let Some(value) = &stmt.value {
                    collect_expr_await_points(value, await_points);
                }
            }
            Stmt::Expr(stmt) => collect_expr_await_points(&stmt.expr, await_points),
        }
    }
}

fn collect_expr_await_points(expr: &Expr, await_points: &mut Vec<DebugAwaitPoint>) {
    match expr {
        Expr::Await(await_expr) => {
            await_points.push(DebugAwaitPoint {
                line: await_expr.span.line,
                col: await_expr.span.col,
            });
            collect_expr_await_points(&await_expr.expr, await_points);
        }
        Expr::Binary(binary) => {
            collect_expr_await_points(&binary.lhs, await_points);
            collect_expr_await_points(&binary.rhs, await_points);
        }
        Expr::Unary(unary) => collect_expr_await_points(&unary.expr, await_points),
        Expr::Call(call) => {
            for arg in &call.args {
                collect_expr_await_points(&arg.expr, await_points);
            }
        }
        Expr::FieldAccess(object, _, _) => collect_expr_await_points(object, await_points),
        Expr::MethodCall(call) => {
            collect_expr_await_points(&call.object, await_points);
            for arg in &call.args {
                collect_expr_await_points(&arg.expr, await_points);
            }
        }
        Expr::StaticCall(call) => {
            for arg in &call.args {
                collect_expr_await_points(&arg.expr, await_points);
            }
        }
        Expr::ObjectLiteral(object) => {
            for field in &object.fields {
                collect_expr_await_points(&field.value, await_points);
            }
        }
        Expr::Spawn(spawn) => {
            for arg in &spawn.args {
                collect_expr_await_points(&arg.expr, await_points);
            }
        }
        Expr::Print(value, _, _) => collect_expr_await_points(value, await_points),
        Expr::Ternary(ternary) => {
            collect_expr_await_points(&ternary.condition, await_points);
            collect_expr_await_points(&ternary.then_expr, await_points);
            collect_expr_await_points(&ternary.else_expr, await_points);
        }
        Expr::Lambda(lambda) => match &lambda.body {
            LambdaBody::Expr(value) => collect_expr_await_points(value, await_points),
            LambdaBody::Block(block) => collect_block_await_points(block, await_points),
        },
        Expr::Match(m) => {
            collect_expr_await_points(&m.scrutinee, await_points);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_expr_await_points(e, await_points),
                    MatchBody::Block(b) => collect_block_await_points(b, await_points),
                }
            }
        }
        Expr::TryPropagate(inner, _) => collect_expr_await_points(inner, await_points),
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::String(_, _)
        | Expr::Var(_, _)
        | Expr::Select(_) => {}
    }
}

fn stmt_kind(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::Let(_) => "let",
        Stmt::Assign(_) => "assign",
        Stmt::FieldAssign(_) => "field_assign",
        Stmt::If(_) => "if",
        Stmt::While(_) => "while",
        Stmt::Return(_) => "return",
        Stmt::Expr(_) => "expr",
    }
}

fn stmt_span(stmt: &Stmt) -> crate::diagnostics::Span {
    match stmt {
        Stmt::Let(s) => s.span,
        Stmt::Assign(s) => s.span,
        Stmt::FieldAssign(s) => s.span,
        Stmt::If(s) => s.span,
        Stmt::While(s) => s.span,
        Stmt::Return(s) => s.span,
        Stmt::Expr(s) => s.span,
    }
}
