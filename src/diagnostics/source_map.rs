use crate::parser::ast::{
    Block, CallArg, CallArgMode, ClassDecl, Expr, FunctionDecl, Item, LambdaBody, MatchBody,
    MethodDecl, Param, ParamMode, Program, StaticCallExpr, Stmt, Type,
};
use std::collections::HashMap;

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
    pub params: Vec<DebugParam>,
    pub await_points: Vec<DebugAwaitPoint>,
    pub reference_calls: Vec<DebugReferenceCall>,
    pub statements: Vec<DebugStatement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugParam {
    pub name: String,
    pub ty: String,
    pub mode: String,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugAwaitPoint {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugReferenceCall {
    pub callee: String,
    pub param: String,
    pub param_ty: String,
    pub mode: String,
    pub place_kind: String,
    pub place_name: String,
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
        let reference_signatures = build_reference_signature_map(program);
        let mut classes = Vec::new();
        let mut functions = Vec::new();
        for item in &program.items {
            match item {
                Item::Function(function) => {
                    functions.push(DebugFunction::from_function(
                        function,
                        &reference_signatures,
                    ));
                }
                Item::Class(class) => {
                    classes.push(DebugClass::from_class(class));
                    for method in &class.methods {
                        functions.push(DebugFunction::from_method(
                            &class.name,
                            method,
                            &reference_signatures,
                        ));
                    }
                }
                Item::Enum(_) => {}
                Item::Interface(_) => {} // no executable code; nothing to map
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
            for param in &function.params {
                out.push_str(&format!(
                    "  param name={} mode={} type={} line={} col={}\n",
                    param.name, param.mode, param.ty, param.line, param.col
                ));
            }
            for await_point in &function.await_points {
                out.push_str(&format!(
                    "  await line={} col={}\n",
                    await_point.line, await_point.col
                ));
            }
            for reference_call in &function.reference_calls {
                out.push_str(&format!(
                    "  reference_call callee={} param={} mode={} type={} place_kind={} place={} line={} col={}\n",
                    reference_call.callee,
                    reference_call.param,
                    reference_call.mode,
                    reference_call.param_ty,
                    reference_call.place_kind,
                    reference_call.place_name,
                    reference_call.line,
                    reference_call.col
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
    fn from_function(
        function: &FunctionDecl,
        reference_signatures: &ReferenceSignatureMap,
    ) -> DebugFunction {
        DebugFunction {
            name: function.name.clone(),
            is_async: function.is_async,
            line: function.span.line,
            col: function.span.col,
            params: function.params.iter().map(DebugParam::from_param).collect(),
            await_points: collect_debug_await_points(&function.body),
            reference_calls: collect_debug_reference_calls(&function.body, reference_signatures),
            statements: collect_debug_statements(&function.body),
        }
    }

    fn from_method(
        class_name: &str,
        method: &MethodDecl,
        reference_signatures: &ReferenceSignatureMap,
    ) -> DebugFunction {
        DebugFunction {
            name: format!("{class_name}::{}", method.name),
            is_async: method.is_async,
            line: method.span.line,
            col: method.span.col,
            params: method.params.iter().map(DebugParam::from_param).collect(),
            await_points: collect_debug_await_points(&method.body),
            reference_calls: collect_debug_reference_calls(&method.body, reference_signatures),
            statements: collect_debug_statements(&method.body),
        }
    }
}

impl DebugParam {
    fn from_param(param: &Param) -> DebugParam {
        DebugParam {
            name: param.name.clone(),
            ty: type_name(&param.ty),
            mode: param_mode_name(&param.mode).to_string(),
            line: param.span.line,
            col: param.span.col,
        }
    }
}

#[derive(Debug, Clone)]
struct ReferenceParamSignature {
    name: String,
    ty: Type,
    mode: ParamMode,
}

type ReferenceSignatureMap = HashMap<String, Vec<ReferenceParamSignature>>;

fn build_reference_signature_map(program: &Program) -> ReferenceSignatureMap {
    let mut signatures = HashMap::new();
    let mut unique_methods: HashMap<String, Option<Vec<ReferenceParamSignature>>> = HashMap::new();

    for item in &program.items {
        match item {
            Item::Function(function) => {
                signatures.insert(function.name.clone(), param_signatures(&function.params));
            }
            Item::Class(class) => {
                for method in &class.methods {
                    let params = param_signatures(&method.params);
                    signatures.insert(format!("{}::{}", class.name, method.name), params.clone());
                    unique_methods
                        .entry(method.name.clone())
                        .and_modify(|existing| *existing = None)
                        .or_insert_with(|| Some(params));
                }
            }
            Item::Enum(_) => {}
            Item::Interface(_) => {} // no method bodies; no signatures to record
        }
    }

    for (method_name, params) in unique_methods {
        if let Some(params) = params {
            signatures.insert(method_name, params);
        }
    }

    signatures
}

fn param_signatures(params: &[Param]) -> Vec<ReferenceParamSignature> {
    params
        .iter()
        .map(|param| ReferenceParamSignature {
            name: param.name.clone(),
            ty: param.ty.clone(),
            mode: param.mode.clone(),
        })
        .collect()
}

fn collect_debug_await_points(block: &Block) -> Vec<DebugAwaitPoint> {
    let mut await_points = Vec::new();
    collect_block_await_points(block, &mut await_points);
    await_points
}

fn collect_debug_reference_calls(
    block: &Block,
    reference_signatures: &ReferenceSignatureMap,
) -> Vec<DebugReferenceCall> {
    let mut reference_calls = Vec::new();
    collect_block_reference_calls(block, reference_signatures, &mut reference_calls);
    reference_calls
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
            Stmt::For(for_stmt) => collect_block_statements(&for_stmt.body, statements),
            Stmt::Let(_)
            | Stmt::Assign(_)
            | Stmt::FieldAssign(_)
            | Stmt::StaticFieldAssign(_)
            | Stmt::SuperInit(_)
            | Stmt::IndexAssign(_)
            | Stmt::Return(_)
            | Stmt::Expr(_) => {}
        }
    }
}

fn collect_block_await_points(block: &Block, await_points: &mut Vec<DebugAwaitPoint>) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Let(stmt) => collect_expr_await_points(&stmt.init, await_points),
            Stmt::Assign(stmt) => collect_expr_await_points(&stmt.value, await_points),
            Stmt::StaticFieldAssign(stmt) => collect_expr_await_points(&stmt.value, await_points),
            Stmt::FieldAssign(stmt) => {
                collect_expr_await_points(&stmt.object, await_points);
                collect_expr_await_points(&stmt.value, await_points);
            }
            Stmt::IndexAssign(stmt) => {
                collect_expr_await_points(&stmt.array, await_points);
                collect_expr_await_points(&stmt.index, await_points);
                collect_expr_await_points(&stmt.value, await_points);
            }
            Stmt::SuperInit(stmt) => {
                for arg in &stmt.args {
                    collect_expr_await_points(&arg.expr, await_points);
                }
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
            Stmt::For(stmt) => {
                collect_expr_await_points(&stmt.iterable, await_points);
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

fn collect_block_reference_calls(
    block: &Block,
    reference_signatures: &ReferenceSignatureMap,
    reference_calls: &mut Vec<DebugReferenceCall>,
) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Let(stmt) => {
                collect_expr_reference_calls(&stmt.init, reference_signatures, reference_calls)
            }
            Stmt::Assign(stmt) => {
                collect_expr_reference_calls(&stmt.value, reference_signatures, reference_calls)
            }
            Stmt::StaticFieldAssign(stmt) => {
                collect_expr_reference_calls(&stmt.value, reference_signatures, reference_calls)
            }
            Stmt::FieldAssign(stmt) => {
                collect_expr_reference_calls(&stmt.object, reference_signatures, reference_calls);
                collect_expr_reference_calls(&stmt.value, reference_signatures, reference_calls);
            }
            Stmt::IndexAssign(stmt) => {
                collect_expr_reference_calls(&stmt.array, reference_signatures, reference_calls);
                collect_expr_reference_calls(&stmt.index, reference_signatures, reference_calls);
                collect_expr_reference_calls(&stmt.value, reference_signatures, reference_calls);
            }
            Stmt::SuperInit(stmt) => {
                for arg in &stmt.args {
                    collect_expr_reference_calls(&arg.expr, reference_signatures, reference_calls);
                }
            }
            Stmt::If(stmt) => {
                collect_expr_reference_calls(&stmt.cond, reference_signatures, reference_calls);
                collect_block_reference_calls(
                    &stmt.then_block,
                    reference_signatures,
                    reference_calls,
                );
                if let Some(else_block) = &stmt.else_block {
                    collect_block_reference_calls(
                        else_block,
                        reference_signatures,
                        reference_calls,
                    );
                }
            }
            Stmt::While(stmt) => {
                collect_expr_reference_calls(&stmt.cond, reference_signatures, reference_calls);
                collect_block_reference_calls(&stmt.body, reference_signatures, reference_calls);
            }
            Stmt::For(stmt) => {
                collect_expr_reference_calls(&stmt.iterable, reference_signatures, reference_calls);
                collect_block_reference_calls(&stmt.body, reference_signatures, reference_calls);
            }
            Stmt::Return(stmt) => {
                if let Some(value) = &stmt.value {
                    collect_expr_reference_calls(value, reference_signatures, reference_calls);
                }
            }
            Stmt::Expr(stmt) => {
                collect_expr_reference_calls(&stmt.expr, reference_signatures, reference_calls)
            }
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
        Expr::New(n) => {
            for arg in &n.args {
                collect_expr_await_points(&arg.expr, await_points);
            }
        }
        Expr::StaticField(_) => {}
        Expr::ObjectLiteral(object) => {
            for field in &object.fields {
                collect_expr_await_points(&field.value, await_points);
            }
        }
        Expr::Print(value, _, _) => collect_expr_await_points(value, await_points),
        Expr::Ternary(ternary) => {
            collect_expr_await_points(&ternary.condition, await_points);
            collect_expr_await_points(&ternary.then_expr, await_points);
            collect_expr_await_points(&ternary.else_expr, await_points);
        }
        Expr::Range(range) => {
            collect_expr_await_points(&range.start, await_points);
            collect_expr_await_points(&range.end, await_points);
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
        Expr::ArrayLiteral(elements, _) => {
            for el in elements {
                collect_expr_await_points(el, await_points);
            }
        }
        Expr::Index(arr, index, _) => {
            collect_expr_await_points(arr, await_points);
            collect_expr_await_points(index, await_points);
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

fn collect_expr_reference_calls(
    expr: &Expr,
    reference_signatures: &ReferenceSignatureMap,
    reference_calls: &mut Vec<DebugReferenceCall>,
) {
    match expr {
        Expr::Binary(binary) => {
            collect_expr_reference_calls(&binary.lhs, reference_signatures, reference_calls);
            collect_expr_reference_calls(&binary.rhs, reference_signatures, reference_calls);
        }
        Expr::Unary(unary) => {
            collect_expr_reference_calls(&unary.expr, reference_signatures, reference_calls)
        }
        Expr::Call(call) => {
            collect_reference_call_args(
                &call.callee,
                &call.args,
                reference_signatures,
                reference_calls,
            );
            for arg in &call.args {
                collect_expr_reference_calls(&arg.expr, reference_signatures, reference_calls);
            }
        }
        Expr::FieldAccess(object, _, _) => {
            collect_expr_reference_calls(object, reference_signatures, reference_calls)
        }
        Expr::MethodCall(call) => {
            collect_expr_reference_calls(&call.object, reference_signatures, reference_calls);
            collect_reference_call_args(
                &call.method,
                &call.args,
                reference_signatures,
                reference_calls,
            );
            for arg in &call.args {
                collect_expr_reference_calls(&arg.expr, reference_signatures, reference_calls);
            }
        }
        Expr::StaticCall(call) => {
            collect_static_reference_call_args(call, reference_signatures, reference_calls);
            for arg in &call.args {
                collect_expr_reference_calls(&arg.expr, reference_signatures, reference_calls);
            }
        }
        Expr::New(n) => {
            for arg in &n.args {
                collect_expr_reference_calls(&arg.expr, reference_signatures, reference_calls);
            }
        }
        Expr::StaticField(_) => {}
        Expr::ObjectLiteral(object) => {
            for field in &object.fields {
                collect_expr_reference_calls(&field.value, reference_signatures, reference_calls);
            }
        }
        Expr::Await(await_expr) => {
            collect_expr_reference_calls(&await_expr.expr, reference_signatures, reference_calls)
        }
        Expr::Print(value, _, _) => {
            collect_expr_reference_calls(value, reference_signatures, reference_calls)
        }
        Expr::Ternary(ternary) => {
            collect_expr_reference_calls(&ternary.condition, reference_signatures, reference_calls);
            collect_expr_reference_calls(&ternary.then_expr, reference_signatures, reference_calls);
            collect_expr_reference_calls(&ternary.else_expr, reference_signatures, reference_calls);
        }
        Expr::Range(range) => {
            collect_expr_reference_calls(&range.start, reference_signatures, reference_calls);
            collect_expr_reference_calls(&range.end, reference_signatures, reference_calls);
        }
        Expr::Lambda(lambda) => match &lambda.body {
            LambdaBody::Expr(value) => {
                collect_expr_reference_calls(value, reference_signatures, reference_calls)
            }
            LambdaBody::Block(block) => {
                collect_block_reference_calls(block, reference_signatures, reference_calls)
            }
        },
        Expr::Match(m) => {
            collect_expr_reference_calls(&m.scrutinee, reference_signatures, reference_calls);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => {
                        collect_expr_reference_calls(e, reference_signatures, reference_calls)
                    }
                    MatchBody::Block(b) => {
                        collect_block_reference_calls(b, reference_signatures, reference_calls)
                    }
                }
            }
        }
        Expr::TryPropagate(inner, _) => {
            collect_expr_reference_calls(inner, reference_signatures, reference_calls)
        }
        Expr::ArrayLiteral(elements, _) => {
            for el in elements {
                collect_expr_reference_calls(el, reference_signatures, reference_calls);
            }
        }
        Expr::Index(arr, index, _) => {
            collect_expr_reference_calls(arr, reference_signatures, reference_calls);
            collect_expr_reference_calls(index, reference_signatures, reference_calls);
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

fn collect_static_reference_call_args(
    call: &StaticCallExpr,
    reference_signatures: &ReferenceSignatureMap,
    reference_calls: &mut Vec<DebugReferenceCall>,
) {
    let callee = format!("{}::{}", call.class, call.method);
    collect_reference_call_args(&callee, &call.args, reference_signatures, reference_calls);
}

fn collect_reference_call_args(
    callee: &str,
    args: &[CallArg],
    reference_signatures: &ReferenceSignatureMap,
    reference_calls: &mut Vec<DebugReferenceCall>,
) {
    let signature = reference_signatures.get(callee);
    for (idx, arg) in args.iter().enumerate() {
        let ampersand_span = match &arg.mode {
            CallArgMode::Reference { ampersand_span } => *ampersand_span,
            CallArgMode::Value => continue,
        };

        let param = signature.and_then(|params| params.get(idx));
        let mode = param
            .map(|param| param_mode_name(&param.mode))
            .unwrap_or("&");
        let param_name = param
            .map(|param| param.name.clone())
            .unwrap_or_else(|| format!("arg{idx}"));
        let param_ty = param
            .map(|param| type_name(&param.ty))
            .unwrap_or_else(|| "<unknown>".to_string());

        reference_calls.push(DebugReferenceCall {
            callee: callee.to_string(),
            param: param_name,
            param_ty,
            mode: mode.to_string(),
            place_kind: reference_place_kind(&arg.expr).to_string(),
            place_name: reference_place_name(&arg.expr),
            line: ampersand_span.line,
            col: ampersand_span.col,
        });
    }
}

fn stmt_kind(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::Let(_) => "let",
        Stmt::Assign(_) => "assign",
        Stmt::StaticFieldAssign(_) => "static_field_assign",
        Stmt::FieldAssign(_) => "field_assign",
        Stmt::SuperInit(_) => "super_init",
        Stmt::IndexAssign(_) => "index_assign",
        Stmt::If(_) => "if",
        Stmt::While(_) => "while",
        Stmt::For(_) => "for",
        Stmt::Return(_) => "return",
        Stmt::Expr(_) => "expr",
    }
}

fn stmt_span(stmt: &Stmt) -> crate::diagnostics::Span {
    match stmt {
        Stmt::Let(s) => s.span,
        Stmt::Assign(s) => s.span,
        Stmt::StaticFieldAssign(s) => s.span,
        Stmt::FieldAssign(s) => s.span,
        Stmt::SuperInit(s) => s.span,
        Stmt::IndexAssign(s) => s.span,
        Stmt::If(s) => s.span,
        Stmt::While(s) => s.span,
        Stmt::For(s) => s.span,
        Stmt::Return(s) => s.span,
        Stmt::Expr(s) => s.span,
    }
}

fn param_mode_name(mode: &ParamMode) -> &'static str {
    match mode {
        ParamMode::Value => "value",
        ParamMode::Reference { mutable: true, .. } => "&mut",
        ParamMode::Reference { mutable: false, .. } => "&",
    }
}

fn type_name(ty: &Type) -> String {
    match ty {
        Type::I64 => "i64".to_string(),
        Type::F64 => "f64".to_string(),
        Type::Bool => "bool".to_string(),
        Type::String => "String".to_string(),
        Type::Void => "void".to_string(),
        Type::Nil => "nil".to_string(),
        Type::Never => "!".to_string(),
        Type::Named(name) => name.clone(),
        Type::Array(element) => format!("Array<{}>", type_name(element)),
        Type::Generic(name, args) => {
            let args = args.iter().map(type_name).collect::<Vec<_>>().join(",");
            format!("{name}<{args}>")
        }
        Type::Nullable(inner) => format!("{}?", type_name(inner)),
        Type::Fn(params, ret) => {
            let param_str = params.iter().map(type_name).collect::<Vec<_>>().join(",");
            format!("fn({}) -> {}", param_str, type_name(ret))
        }
    }
}

fn reference_place_kind(expr: &Expr) -> &'static str {
    match expr {
        Expr::Var(_, _) => "local",
        Expr::FieldAccess(_, _, _) => "field",
        Expr::Index(_, _, _) => "array_element",
        _ => "expression",
    }
}

fn reference_place_name(expr: &Expr) -> String {
    match expr {
        Expr::Var(name, _) => name.clone(),
        Expr::FieldAccess(object, field, _) => {
            format!("{}.{}", reference_place_name(object), field)
        }
        Expr::Index(array, index, _) => {
            format!(
                "{}[{}]",
                reference_place_name(array),
                reference_index_name(index)
            )
        }
        _ => "<expression>".to_string(),
    }
}

fn reference_index_name(expr: &Expr) -> String {
    match expr {
        Expr::Integer(value, _) => value.to_string(),
        Expr::Var(name, _) => name.clone(),
        _ => "<expr>".to_string(),
    }
}
