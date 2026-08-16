use crate::parser::ast::{
    Block, CallArg, CallArgMode, ClassDecl, Expr, FunctionDecl, Item, MethodDecl, Param, ParamMode,
    Program, StaticCallExpr, Stmt, Type,
};
use crate::parser::visit::{AstVisitor, walk_expr, walk_stmt};
use std::collections::HashMap;

use super::FileId;

/// Holds the source text for a single file, enabling line/column lookups.
#[derive(Clone)]
pub struct SourceMap {
    pub file_id: FileId,
    pub path: String,
    pub source: String,
    line_offsets: Vec<usize>,
}

impl SourceMap {
    pub fn new(path: impl Into<String>, source: impl Into<String>) -> Self {
        Self::with_file_id(FileId::ENTRY, path, source)
    }

    pub fn with_file_id(
        file_id: FileId,
        path: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        let source = source.into();
        let mut offsets = vec![0usize];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                offsets.push(i + 1);
            }
        }
        Self {
            file_id,
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

/// All source maps participating in one compilation.
#[derive(Default)]
pub struct SourceMaps {
    maps: HashMap<FileId, SourceMap>,
}

impl SourceMaps {
    pub fn new(entry: SourceMap) -> Self {
        let mut maps = HashMap::new();
        maps.insert(entry.file_id, entry);
        Self { maps }
    }

    pub fn insert(&mut self, map: SourceMap) {
        self.maps.insert(map.file_id, map);
    }

    pub fn get(&self, file_id: FileId) -> Option<&SourceMap> {
        self.maps.get(&file_id)
    }

    pub fn entry(&self) -> Option<&SourceMap> {
        self.get(FileId::ENTRY)
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
    let mut collector = AwaitPointCollector::default();
    collector.visit_block(block);
    collector.await_points
}

fn collect_debug_reference_calls(
    block: &Block,
    reference_signatures: &ReferenceSignatureMap,
) -> Vec<DebugReferenceCall> {
    let mut collector = ReferenceCallCollector {
        signatures: reference_signatures,
        reference_calls: Vec::new(),
    };
    collector.visit_block(block);
    collector.reference_calls
}

/// Records every `await` in a function body, in source order, on the shared
/// structural walk (willow-uqzx.1.1).
///
/// Two skips are carried over from the hand-written traversal this replaced:
/// a `defer` body belongs to the deferred callable, and a `select` case's
/// suspension is scheduled by the `select` itself rather than by an await
/// point in the enclosing body.
#[derive(Default)]
struct AwaitPointCollector {
    await_points: Vec<DebugAwaitPoint>,
}

impl AstVisitor for AwaitPointCollector {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if matches!(stmt, Stmt::Defer(_)) {
            return;
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Await(await_expr) => self.await_points.push(DebugAwaitPoint {
                line: await_expr.span.line,
                col: await_expr.span.col,
            }),
            Expr::Select(_) => return,
            _ => {}
        }
        walk_expr(self, expr);
    }
}

/// Records every by-reference argument in a function body on the shared
/// structural walk, with the same `defer` and `select` skips as
/// [`AwaitPointCollector`].
///
/// Calls are recorded before their own arguments are walked, so a nested call
/// in an argument or a receiver follows the call that encloses it.
struct ReferenceCallCollector<'a> {
    signatures: &'a ReferenceSignatureMap,
    reference_calls: Vec<DebugReferenceCall>,
}

impl AstVisitor for ReferenceCallCollector<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if matches!(stmt, Stmt::Defer(_)) {
            return;
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => collect_reference_call_args(
                &call.callee,
                &call.args,
                self.signatures,
                &mut self.reference_calls,
            ),
            Expr::MethodCall(call) => collect_reference_call_args(
                &call.method,
                &call.args,
                self.signatures,
                &mut self.reference_calls,
            ),
            Expr::StaticCall(call) => {
                collect_static_reference_call_args(call, self.signatures, &mut self.reference_calls)
            }
            Expr::Select(_) => return,
            _ => {}
        }
        walk_expr(self, expr);
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
            Stmt::Defer(_) => {}
            Stmt::Break(_) | Stmt::Continue(_) => {}
            Stmt::If(if_stmt) => {
                collect_block_statements(&if_stmt.then_block, statements);
                if let Some(else_block) = &if_stmt.else_block {
                    collect_block_statements(else_block, statements);
                }
            }
            Stmt::While(while_stmt) => collect_block_statements(&while_stmt.body, statements),
            Stmt::For(for_stmt) => collect_block_statements(&for_stmt.body, statements),
            Stmt::Lock(lock_stmt) => collect_block_statements(&lock_stmt.body, statements),
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
        Stmt::Lock(_) => "lock",
        Stmt::Return(_) => "return",
        Stmt::Expr(_) => "expr",
        Stmt::Break(_) => "break",
        Stmt::Defer(_) => "defer",
        Stmt::Continue(_) => "continue",
    }
}

fn stmt_span(stmt: &Stmt) -> crate::diagnostics::Span {
    match stmt {
        Stmt::Defer(d) => d.span,
        Stmt::Break(span) | Stmt::Continue(span) => *span,
        Stmt::Let(s) => s.span,
        Stmt::Assign(s) => s.span,
        Stmt::StaticFieldAssign(s) => s.span,
        Stmt::FieldAssign(s) => s.span,
        Stmt::SuperInit(s) => s.span,
        Stmt::IndexAssign(s) => s.span,
        Stmt::If(s) => s.span,
        Stmt::While(s) => s.span,
        Stmt::For(s) => s.span,
        Stmt::Lock(s) => s.span,
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
        Type::Never => "!".to_string(),
        Type::Named(name) => name.clone(),
        Type::Array(element) => format!("Array<{}>", type_name(element)),
        Type::Generic(name, args) => {
            let args = args.iter().map(type_name).collect::<Vec<_>>().join(",");
            format!("{name}<{args}>")
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn debug_map(source: &str) -> DebugSourceMap {
        let tokens = Lexer::new(source).tokenize().unwrap();
        let (program, errors) = Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        DebugSourceMap::from_program("test.wi", 0, &program)
    }

    fn function<'a>(map: &'a DebugSourceMap, name: &str) -> &'a DebugFunction {
        map.functions
            .iter()
            .find(|function| function.name == name)
            .unwrap_or_else(|| panic!("no debug function `{name}`"))
    }

    fn await_lines(source: &str, name: &str) -> Vec<usize> {
        function(&debug_map(source), name)
            .await_points
            .iter()
            .map(|point| point.line)
            .collect()
    }

    fn reference_callees(source: &str, name: &str) -> Vec<String> {
        function(&debug_map(source), name)
            .reference_calls
            .iter()
            .map(|call| call.callee.clone())
            .collect()
    }

    // --- Shared structural AST walk (willow-uqzx.1.1) ---
    //
    // Await points and by-reference call sites are collected on the shared
    // walk in `parser::visit`. A slot the walk misses drops a suspension point
    // or a reference call site from the debug map, which the debugger then
    // cannot show, so each container gets its own program.

    /// An `await` nested inside a call argument, three expression levels from
    /// the statement, is still a suspension point of this function.
    #[test]
    fn await_point_in_a_nested_call_argument_is_recorded() {
        let source = r#"
async fn work() -> i64 { return 1; }
fn twice(n: i64) -> i64 { return n + n; }

async fn run(f: Future<i64>) -> i64 {
    return twice(await f);
}
"#;
        assert_eq!(await_lines(source, "run"), vec![6]);
    }

    #[test]
    fn await_points_in_ternary_branches_are_recorded_in_source_order() {
        let source = r#"
async fn run(flag: bool, a: Future<i64>, b: Future<i64>) -> i64 {
    return flag
        ? await a
        : await b;
}
"#;
        assert_eq!(await_lines(source, "run"), vec![4, 5]);
    }

    #[test]
    fn await_point_in_a_match_arm_body_is_recorded() {
        let source = r#"
async fn run(n: i64, f: Future<i64>) -> i64 {
    return match n {
        1 => await f,
        _ => 0
    };
}
"#;
        assert_eq!(await_lines(source, "run"), vec![4]);
    }

    #[test]
    fn await_point_in_a_loop_body_is_recorded() {
        let source = r#"
async fn run(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        await sleep(1);
        i = i + 1;
    }
    return i;
}
"#;
        assert_eq!(await_lines(source, "run"), vec![5]);
    }

    /// A `defer` body runs at scope exit and is attributed to the deferred
    /// callable. The hand-written traversal this replaced skipped it, and the
    /// skip is preserved deliberately.
    #[test]
    fn await_point_in_a_defer_body_is_not_attributed_to_the_enclosing_function() {
        let source = r#"
async fn run(f: Future<i64>) -> i64 {
    defer {
        println(1);
    }
    return await f;
}
"#;
        assert_eq!(await_lines(source, "run"), vec![6]);
    }

    /// A select case's suspension is scheduled by the `select` itself, so it
    /// is not an await point of the enclosing body — also preserved.
    #[test]
    fn await_point_in_a_select_case_is_not_recorded() {
        let source = r#"
async fn work() -> i64 { return 1; }

async fn run(f: Future<i64>) -> i64 {
    let t = work();
    select {
        let a = await t => { println(a); }
        default => { println(0); }
    }
    return await f;
}
"#;
        // Only the `return await f;` on line 10 — the case await on line 7 is
        // the select's own.
        assert_eq!(await_lines(source, "run"), vec![10]);
    }

    #[test]
    fn reference_call_in_a_nested_argument_is_recorded() {
        let source = r#"
fn read(x: & i64) -> i64 { return x; }
fn twice(n: i64) -> i64 { return n + n; }

fn run() -> i64 {
    let n = 1;
    return twice(read(&n));
}
"#;
        assert_eq!(reference_callees(source, "run"), vec!["read".to_string()]);
    }

    #[test]
    fn reference_call_in_a_loop_body_is_recorded() {
        let source = r#"
fn bump(x: &mut i64) { x = x + 1; }

fn run(limit: i64) {
    let mut n = 0;
    let mut i = 0;
    while i < limit {
        bump(&n);
        i = i + 1;
    }
}
"#;
        assert_eq!(reference_callees(source, "run"), vec!["bump".to_string()]);
    }

    #[test]
    fn reference_calls_in_both_ternary_branches_are_recorded() {
        let source = r#"
fn read(x: & i64) -> i64 { return x; }
fn other(x: & i64) -> i64 { return x + 1; }

fn run(flag: bool) -> i64 {
    let n = 1;
    return flag ? read(&n) : other(&n);
}
"#;
        assert_eq!(
            reference_callees(source, "run"),
            vec!["read".to_string(), "other".to_string()]
        );
    }

    /// The place a reference argument names is recorded with the call, so the
    /// debugger can show which local a `&mut` borrowed.
    #[test]
    fn reference_call_records_its_place_and_mode() {
        let source = r#"
fn bump(x: &mut i64) { x = x + 1; }

fn run() {
    let mut n = 1;
    bump(&n);
}
"#;
        let map = debug_map(source);
        let call = &function(&map, "run").reference_calls[0];
        assert_eq!(call.callee, "bump");
        assert_eq!(call.param, "x");
        assert_eq!(call.mode, "&mut");
        assert_eq!(call.place_kind, "local");
        assert_eq!(call.place_name, "n");
    }
}
