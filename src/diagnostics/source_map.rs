use crate::parser::ast::{Block, FunctionDecl, Item, MethodDecl, Program, Stmt};

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
    pub functions: Vec<DebugFunction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugFunction {
    pub name: String,
    pub line: usize,
    pub col: usize,
    pub statements: Vec<DebugStatement>,
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
        let mut functions = Vec::new();
        for item in &program.items {
            match item {
                Item::Function(function) => {
                    functions.push(DebugFunction::from_function(function));
                }
                Item::Class(class) => {
                    for method in &class.methods {
                        functions.push(DebugFunction::from_method(&class.name, method));
                    }
                }
            }
        }

        DebugSourceMap {
            file: file.into(),
            total_lines,
            functions,
        }
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("willow_debug_source_map_v1\n");
        out.push_str(&format!("file={}\n", self.file));
        out.push_str(&format!("total_lines={}\n", self.total_lines));

        for function in &self.functions {
            out.push('\n');
            out.push_str(&format!(
                "function name={} line={} col={}\n",
                function.name, function.line, function.col
            ));
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

impl DebugFunction {
    fn from_function(function: &FunctionDecl) -> DebugFunction {
        DebugFunction {
            name: function.name.clone(),
            line: function.span.line,
            col: function.span.col,
            statements: collect_debug_statements(&function.body),
        }
    }

    fn from_method(class_name: &str, method: &MethodDecl) -> DebugFunction {
        DebugFunction {
            name: format!("{class_name}::{}", method.name),
            line: method.span.line,
            col: method.span.col,
            statements: collect_debug_statements(&method.body),
        }
    }
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
            Stmt::Let(_) | Stmt::Assign(_) | Stmt::Return(_) | Stmt::Expr(_) => {}
        }
    }
}

fn stmt_kind(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::Let(_) => "let",
        Stmt::Assign(_) => "assign",
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
        Stmt::If(s) => s.span,
        Stmt::While(s) => s.span,
        Stmt::Return(s) => s.span,
        Stmt::Expr(s) => s.span,
    }
}
