//! Instance-field reassignment summary (willow-jz15.78).
//!
//! Willow fields have no `readonly` spelling: every instance field can be
//! assigned wherever it is visible. A class value shared by several tasks is
//! only race-free when none of its fields is written after construction, so
//! the `Sync` rule needs to know which fields are ever reassigned.
//!
//! The summary is syntactic and whole-program. One walk per body records the
//! field writes it contains; resolution then maps them to field declarations:
//!
//! * `self.f = …` (or `&self.f`) inside a class method resolves through the
//!   enclosing class's declared base chain to the declaration of `f`;
//! * the same write directly in an `init` body is construction and is ignored
//!   while the object is still private to `init`. Once `self` escapes (any use
//!   other than `self.f` — a call argument or receiver, a binding, a capture),
//!   another task may already hold it, so every later write counts, as do
//!   deferred writes and writes in a lambda created by `init`;
//! * a subclass `init`'s writes after `super.init` count when a constructor
//!   on the base chain lets `self` escape; a base missing from the class
//!   index (import alias, mangled name) is assumed to escape;
//! * parameter types, annotated locals, `new` expressions and local aliases
//!   identify the receiver's class. Lexical shadowing is tracked. Import paths
//!   resolve to module-scoped class declarations, including inherited fields;
//! * other receivers remain unknown and conservatively mark every field named
//!   `f`. Unknown/shadowed generic parameters are never treated as a class.
//!
//! A `&expr.f` call argument counts as a write because a reference parameter
//! may be `&mut`. The analysis over-approximates writes (never misses one), so
//! it can only make fewer classes `Sync`.

use std::collections::{HashMap, HashSet};

use crate::diagnostics::{FileId, Span};
use crate::parser::ast::*;
use crate::parser::visit::{self, AstVisitor};
use crate::semantic::scopes::Scopes;

/// One syntactic field write. `self_class` is the receiver's nominal type
/// spelling in `file` (also used for `self`); `None` means unresolved.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RawFieldWrite {
    pub(crate) file: FileId,
    pub(crate) self_class: Option<String>,
    pub(crate) field: String,
    pub(crate) kind: WriteKind,
}

/// How a recorded fact of a body takes part in the summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum WriteKind {
    /// A write after construction.
    Write,
    /// A constructor's `self.f` write that runs after its base constructor
    /// (`super.init`, or anywhere when that call is implicit). It is a write
    /// only when a constructor on the base chain lets `self` escape.
    AfterBaseInit,
    /// Marker (empty `field`): a constructor of `self_class` lets `self`
    /// escape, so its subclasses' later `init` writes can race.
    InitEscapes,
}

/// Where a body sits, which decides how `self.f = …` is classified.
#[derive(Clone, Copy)]
pub(crate) enum WriteContext<'a> {
    /// Free function, static initializer, or interface default body.
    Free,
    Method(&'a str),
    Constructor(&'a str),
}

/// Fields that may be written after their object has been constructed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FieldWrites {
    /// Field names written through an unresolved receiver.
    by_name: HashSet<String>,
    /// Resolved field declarations, including writes through other receivers.
    declarations: HashSet<Span>,
}

impl FieldWrites {
    /// Whether the instance field declared at `span` as `name` may be
    /// reassigned after construction.
    pub fn is_reassigned(&self, name: &str, span: Span) -> bool {
        self.by_name.contains(name) || self.declarations.contains(&span)
    }

    /// Resolve against declarations and import identities without hydrating bodies.
    pub(crate) fn resolve_programs<'a, 'w>(
        programs: impl IntoIterator<Item = (&'a str, &'a Program)>,
        dependencies: Option<&'a crate::compiler_db::dependencies::ModuleDependencies>,
        writes: impl IntoIterator<Item = &'w RawFieldWrite>,
    ) -> Self {
        let programs: Vec<_> = programs.into_iter().collect();
        let mut resolver = Resolver::new(programs.iter().flat_map(|(_, p)| classes(p)));
        resolver.dependencies = dependencies;
        for (namespace, program) in programs {
            for class in classes(program) {
                let id = resolver.local[&class.span.file_id][class.name.as_str()];
                let qualified = if namespace.is_empty() {
                    class.name.clone()
                } else {
                    format!("{namespace}::{}", class.name)
                };
                resolver.qualified.insert(qualified, id);
            }
            for import in &program.imports {
                let alias = import
                    .alias
                    .as_deref()
                    .unwrap_or_else(|| import.path.rsplit("::").next().unwrap());
                resolver
                    .imports
                    .entry(import.span.file_id)
                    .or_default()
                    .insert(alias.to_owned(), import.path.clone());
            }
        }
        resolver.resolve(writes).0
    }

    #[cfg(test)]
    pub(crate) fn resolve_counted<'c, 'w>(
        classes: impl IntoIterator<Item = &'c ClassDecl>,
        writes: impl IntoIterator<Item = &'w RawFieldWrite>,
    ) -> (Self, usize) {
        Resolver::new(classes).resolve(writes)
    }

    pub(crate) fn from_program(program: &Program) -> Self {
        let mut writes = HashSet::new();
        for_each_body(program, |body, context, params, type_params| {
            writes.extend(body.collect_with_bindings(context, params, type_params));
        });
        Self::resolve_programs(std::iter::once(("", program)), None, &writes)
    }
}

/// Declaration identity is an index into the input class inventory, never a
/// bare class or field name. Fields and lexical scopes both have indexed lookup.
struct Resolver<'a> {
    classes: Vec<&'a ClassDecl>,
    dependencies: Option<&'a crate::compiler_db::dependencies::ModuleDependencies>,
    local: HashMap<FileId, HashMap<&'a str, usize>>,
    qualified: HashMap<String, usize>,
    imports: HashMap<FileId, HashMap<String, String>>,
    fields: Vec<HashMap<&'a str, Span>>,
    bases: Vec<Option<Result<usize, ()>>>,
    escaping: HashSet<usize>,
    escapes: HashMap<usize, bool>,
}

impl<'a> Resolver<'a> {
    fn new(classes: impl IntoIterator<Item = &'a ClassDecl>) -> Self {
        let classes: Vec<_> = classes.into_iter().collect();
        let mut local: HashMap<FileId, HashMap<&str, usize>> = HashMap::new();
        for (id, class) in classes.iter().enumerate() {
            local
                .entry(class.span.file_id)
                .or_default()
                .insert(&class.name, id);
        }
        let fields = classes
            .iter()
            .map(|c| {
                c.fields
                    .iter()
                    .filter(|f| !f.is_static)
                    .map(|f| (f.name.as_str(), f.span))
                    .collect()
            })
            .collect();
        Self {
            classes,
            dependencies: None,
            local,
            qualified: HashMap::new(),
            imports: HashMap::new(),
            fields,
            bases: Vec::new(),
            escaping: HashSet::new(),
            escapes: HashMap::new(),
        }
    }

    fn class(&self, file: FileId, name: &str) -> Option<usize> {
        if let Some(id) = self.local.get(&file).and_then(|classes| classes.get(name)) {
            return Some(*id);
        }
        let (head, tail) = name
            .split_once("::")
            .map_or((name, None), |(h, t)| (h, Some(t)));
        if let Some(path) = self
            .imports
            .get(&file)
            .and_then(|imports| imports.get(head))
        {
            let qualified = tail.map_or_else(|| path.clone(), |tail| format!("{path}::{tail}"));
            return self.qualified_class(file, &qualified);
        }
        self.qualified_class(file, name)
    }

    fn qualified_class(&self, file: FileId, name: &str) -> Option<usize> {
        if let Some(id) = self.qualified.get(name) {
            return Some(*id);
        }
        let (module, class) = name.rsplit_once("::")?;
        let unit = self
            .dependencies?
            .unit_for_path(crate::module::ModuleId(file.0), module)?;
        self.local.get(&FileId(unit.0))?.get(class).copied()
    }

    fn resolve<'w>(
        mut self,
        writes: impl IntoIterator<Item = &'w RawFieldWrite>,
    ) -> (FieldWrites, usize) {
        self.bases = self
            .classes
            .iter()
            .map(|c| {
                c.base_class
                    .as_ref()
                    .map(|base| self.class(c.span.file_id, base.name()).ok_or(()))
            })
            .collect();
        let writes: Vec<_> = writes.into_iter().collect();
        for write in &writes {
            if write.kind == WriteKind::InitEscapes
                && let Some(id) = write
                    .self_class
                    .as_deref()
                    .and_then(|name| self.class(write.file, name))
            {
                self.escaping.insert(id);
            }
        }
        let mut summary = FieldWrites::default();
        let mut by_owner = vec![Vec::new(); self.classes.len()];
        for write in writes {
            if write.kind == WriteKind::InitEscapes {
                continue;
            }
            let owner = write
                .self_class
                .as_deref()
                .and_then(|name| self.class(write.file, name));
            let Some(id) = owner else {
                summary.by_name.insert(write.field.clone());
                continue;
            };
            if write.kind == WriteKind::AfterBaseInit {
                let escapes = match self.bases[id] {
                    None => false,
                    Some(Err(())) => true,
                    Some(Ok(base)) => self.chain_escapes(base),
                };
                if !escapes {
                    continue;
                }
            }
            by_owner[id].push(write);
        }

        // Resolve all fields in one traversal of the inheritance forest.
        // Indexed scope stacks provide the nearest declaring field in O(1),
        // avoiding a separate ancestor walk for every distinct written field.
        let mut children = vec![Vec::new(); self.classes.len()];
        let mut stack = Vec::new();
        for (id, base) in self.bases.iter().enumerate() {
            match base {
                Some(Ok(base)) => children[*base].push(id),
                _ => stack.push((id, false)),
            }
        }
        let mut fields = Scopes::default();
        let mut visited = vec![false; self.classes.len()];
        let mut resolutions = 0;
        while let Some((id, exit)) = stack.pop() {
            if exit {
                fields.pop();
                continue;
            }
            visited[id] = true;
            fields.push();
            for (name, span) in &self.fields[id] {
                fields.insert((*name).to_owned(), *span);
            }
            for write in &by_owner[id] {
                resolutions += 1;
                match fields.get(&write.field) {
                    Some(span) => {
                        summary.declarations.insert(*span);
                    }
                    None => {
                        summary.by_name.insert(write.field.clone());
                    }
                }
            }
            stack.push((id, true));
            stack.extend(children[id].iter().map(|&child| (child, false)));
        }
        // Cycles (diagnosed by class checking) and their descendants cannot
        // belong to the rooted forest. Never lose their writes on an error path.
        for (id, writes) in by_owner.iter().enumerate() {
            if !visited[id] {
                for write in writes {
                    summary.by_name.insert(write.field.clone());
                }
            }
        }
        (summary, resolutions)
    }

    /// Iterative path compression: each ancestor is computed once, including
    /// missing bases and cycles (both conservatively escaping).
    fn chain_escapes(&mut self, start: usize) -> bool {
        let mut path = Vec::new();
        let mut seen = HashSet::new();
        let mut id = start;
        let result = loop {
            if let Some(result) = self.escapes.get(&id) {
                break *result;
            }
            if !seen.insert(id) {
                break true;
            }
            path.push(id);
            if self.escaping.contains(&id) {
                break true;
            }
            match self.bases[id] {
                None => break false,
                Some(Err(())) => break true,
                Some(Ok(base)) => id = base,
            }
        };
        for id in path {
            self.escapes.insert(id, result);
        }
        result
    }
}

/// Every class declaration of `program`.
pub(crate) fn classes(program: &Program) -> impl Iterator<Item = &ClassDecl> {
    program.items.iter().filter_map(|item| match item {
        Item::Class(class) => Some(class),
        _ => None,
    })
}

/// A body-bearing slot of a program.
pub(crate) enum BodyRef<'a> {
    Block(&'a Block),
    /// A static-field initializer expression.
    Initializer(&'a Expr),
}

impl BodyRef<'_> {
    pub(crate) fn collect_with_bindings(
        &self,
        context: WriteContext<'_>,
        params: &[Param],
        type_params: &[String],
    ) -> HashSet<RawFieldWrite> {
        let mut collector = Collector {
            context,
            bindings: Scopes::with_frame([]),
            type_params: type_params.iter().map(String::as_str).collect(),
            lambda_depth: 0,
            self_escaped: false,
            after_base_init: false,
            writes: HashSet::new(),
        };
        for param in params {
            let ty = collector.nominal(&param.ty);
            collector.bindings.insert(param.name.clone(), ty);
        }
        match self {
            BodyRef::Block(block) if matches!(context, WriteContext::Constructor(_)) => {
                collector.visit_constructor_body(block)
            }
            BodyRef::Block(block) => collector.visit_block(block),
            BodyRef::Initializer(expr) => collector.visit_expr(expr),
        }
        collector.writes
    }
}

/// Visit every body slot of `program` with its write context.
pub(crate) fn for_each_body<'a>(
    program: &'a Program,
    mut f: impl FnMut(BodyRef<'a>, WriteContext<'a>, &'a [Param], &'a [String]),
) {
    for item in &program.items {
        match item {
            Item::Function(function) => f(
                BodyRef::Block(&function.body),
                WriteContext::Free,
                &function.params,
                &function.type_params,
            ),
            Item::Class(class) => {
                for method in &class.methods {
                    f(
                        BodyRef::Block(&method.body),
                        WriteContext::Method(&class.name),
                        &method.params,
                        &class.type_params,
                    );
                }
                for constructor in &class.constructors {
                    f(
                        BodyRef::Block(&constructor.body),
                        WriteContext::Constructor(&class.name),
                        &constructor.params,
                        &class.type_params,
                    );
                }
                for field in &class.fields {
                    if let Some(expr) = &field.initializer {
                        f(
                            BodyRef::Initializer(expr),
                            WriteContext::Free,
                            &[],
                            &class.type_params,
                        );
                    }
                }
            }
            Item::Interface(interface) => {
                for method in &interface.methods {
                    if let Some(body) = &method.default_body {
                        f(
                            BodyRef::Block(body),
                            WriteContext::Free,
                            &method.params,
                            &interface.type_params,
                        );
                    }
                }
            }
            Item::Enum(_) => {}
        }
    }
}

struct Collector<'a> {
    context: WriteContext<'a>,
    bindings: Scopes<Option<String>>,
    type_params: HashSet<&'a str>,
    lambda_depth: usize,
    /// In a constructor: `self` may already be visible outside `init`.
    self_escaped: bool,
    /// In a constructor: the base constructor has already run.
    after_base_init: bool,
    writes: HashSet<RawFieldWrite>,
}

fn is_self(expr: &Expr) -> bool {
    matches!(expr, Expr::Var(name, ..) if name == "self")
}

impl Collector<'_> {
    fn nominal(&self, ty: &Type) -> Option<String> {
        match ty {
            Type::Named(name) | Type::Generic(name, _)
                if !self.type_params.contains(name.as_str()) =>
            {
                Some(name.clone())
            }
            _ => None,
        }
    }

    fn receiver(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::New(call) if !self.type_params.contains(call.class_name.as_str()) => {
                Some(call.class_name.clone())
            }
            Expr::Var(name, ..) => self.bindings.get(name).cloned().flatten(),
            _ => None,
        }
    }
    /// Top-level statements run in order and a loop is one statement, so
    /// every write in or after the first statement where `self` escapes may
    /// run while another holder can observe it. Deferred statements run at
    /// exit, after any escape.
    ///
    /// `super.init(…)` hands `self` to the base constructor, which may let it
    /// escape; writes from that statement on are recorded as
    /// [`WriteKind::AfterBaseInit`] and resolved against the base chain.
    fn visit_constructor_body(&mut self, block: &Block) {
        let scans: Vec<EscapeFinder> = block
            .stmts
            .iter()
            .map(|stmt| {
                let mut finder = EscapeFinder::default();
                finder.visit_stmt(stmt);
                finder
            })
            .collect();
        let first_escape = scans.iter().position(|scan| scan.escaped);
        // Without an explicit call the base constructor runs first.
        let base_init = scans.iter().position(|scan| scan.super_init).unwrap_or(0);
        if first_escape.is_some()
            && let WriteContext::Constructor(class) = self.context
        {
            self.writes.insert(RawFieldWrite {
                file: block.span.file_id,
                self_class: Some(class.to_string()),
                field: String::new(),
                kind: WriteKind::InitEscapes,
            });
        }
        for (index, stmt) in block.stmts.iter().enumerate() {
            let deferred = matches!(stmt, Stmt::Defer(_));
            self.self_escaped = first_escape.is_some_and(|first| index >= first || deferred);
            self.after_base_init = index >= base_init || deferred;
            self.visit_stmt(stmt);
        }
    }

    fn record(&mut self, object: &Expr, field: &str) {
        let is_self = is_self(object);
        let mut kind = WriteKind::Write;
        let self_class = match self.context {
            WriteContext::Constructor(class)
                if is_self && self.lambda_depth == 0 && !self.self_escaped =>
            {
                if !self.after_base_init {
                    return;
                }
                kind = WriteKind::AfterBaseInit;
                Some(class.to_string())
            }
            WriteContext::Method(class) | WriteContext::Constructor(class) if is_self => {
                Some(class.to_string())
            }
            _ => self.receiver(object),
        };
        self.writes.insert(RawFieldWrite {
            file: object.span().file_id,
            self_class,
            field: field.to_string(),
            kind,
        });
    }

    fn record_reference_args(&mut self, args: &[CallArg]) {
        for arg in args {
            if matches!(arg.mode, CallArgMode::Reference { .. })
                && let Expr::FieldAccess(object, field, ..) = &arg.expr
            {
                self.record(object, field);
            }
        }
    }
}

/// Finds a use of `self` other than a direct `self.f` read or write, and a
/// `super.init(…)` call.
#[derive(Default)]
struct EscapeFinder {
    escaped: bool,
    super_init: bool,
}

impl AstVisitor for EscapeFinder {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::FieldAssign(assign) if is_self(&assign.object) => self.visit_expr(&assign.value),
            Stmt::SuperInit(_) => {
                self.super_init = true;
                visit::walk_stmt(self, stmt);
            }
            _ => visit::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::FieldAccess(object, ..) if is_self(object) => {}
            _ if is_self(expr) => self.escaped = true,
            _ => visit::walk_expr(self, expr),
        }
    }
}

impl AstVisitor for Collector<'_> {
    fn enter_scope(&mut self) {
        self.bindings.push();
    }
    fn exit_scope(&mut self) {
        self.bindings.pop();
    }
    fn bind(&mut self, name: &str) {
        self.bindings.insert(name.to_owned(), None);
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(binding) => {
                self.visit_expr(&binding.init);
                let ty = match &binding.ty {
                    Some(ty) => self.nominal(ty),
                    None => self.receiver(&binding.init),
                };
                self.bindings.insert(binding.name.clone(), ty);
                return;
            }
            Stmt::FieldAssign(assign) => self.record(&assign.object, &assign.field),
            Stmt::SuperInit(init) => self.record_reference_args(&init.args),
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Call(call) => self.record_reference_args(&call.args),
            Expr::MethodCall(call) => self.record_reference_args(&call.args),
            Expr::StaticCall(call) => self.record_reference_args(&call.args),
            Expr::New(new) => self.record_reference_args(&new.args),
            _ => {}
        }
        visit::walk_expr(self, expr);
    }

    fn visit_lambda(&mut self, lambda: &LambdaExpr) {
        self.lambda_depth += 1;
        self.enter_scope();
        for param in &lambda.params {
            let ty = param.ty.as_ref().and_then(|ty| self.nominal(ty));
            self.bindings.insert(param.name.clone(), ty);
        }
        match &lambda.body {
            LambdaBody::Expr(expr) => self.visit_expr(expr),
            LambdaBody::Block(block) => self.visit_block(block),
        }
        self.exit_scope();
        self.lambda_depth -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Program {
        let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        program
    }

    /// `class.field` → reassigned?
    fn reassigned(source: &str, class: &str, field: &str) -> bool {
        let program = parse(source);
        let summary = FieldWrites::from_program(&program);
        let decl = classes(&program)
            .find(|c| c.name == class)
            .and_then(|c| c.fields.iter().find(|f| f.name == field))
            .expect("field");
        summary.is_reassigned(&decl.name, decl.span)
    }

    #[test]
    fn method_self_write_marks_declaring_field_only() {
        let source = "class A { pub n: i64; pub fn f(self) { self.n = 1; } } class B { pub n: i64; } fn main() {}";
        assert!(reassigned(source, "A", "n"));
        assert!(!reassigned(source, "B", "n"));
    }

    #[test]
    fn constructor_self_write_is_construction() {
        let source = "class A { pub n: i64; pub init(self) { self.n = 1; } } fn main() {}";
        assert!(!reassigned(source, "A", "n"));
    }

    #[test]
    fn known_receiver_marks_only_its_declaration() {
        let source = "class A { pub n: i64; } class B { pub n: i64; pub m: i64; } fn main() { let b = new B(1, 2); b.n = 3; }";
        assert!(!reassigned(source, "A", "n"));
        assert!(reassigned(source, "B", "n"));
        assert!(!reassigned(source, "B", "m"));
    }

    #[test]
    fn reference_arguments_count_as_writes() {
        let source = "class A { pub n: i64; pub m: i64; } fn inc(x: &mut i64) { x = x + 1; } fn main() { let a = new A(1, 2); inc(&a.n); }";
        assert!(reassigned(source, "A", "n"));
        assert!(!reassigned(source, "A", "m"));
    }

    #[test]
    fn super_init_reference_argument_counts() {
        let source = "open class P { pub init(self, x: &mut i64) { x = 1; } } class A { pub n: i64; } class C extends P { pub init(self, a: A) { super.init(&a.n); } } fn main() {}";
        assert!(reassigned(source, "A", "n"));
    }

    #[test]
    fn subclass_method_resolves_to_base_declaration() {
        let source = "open class Base { pub n: i64; } class Sub extends Base { pub fn f(self) { self.n = 2; } } class Other { pub n: i64; } fn main() {}";
        assert!(reassigned(source, "Base", "n"));
        assert!(!reassigned(source, "Other", "n"));
    }

    #[test]
    fn lambda_in_constructor_is_not_construction() {
        let program = parse("class A { pub n: i64; pub init(self) { self.n = 0; } } fn main() {}");
        let class = classes(&program).next().unwrap();
        let mut collector = Collector {
            context: WriteContext::Constructor(&class.name),
            bindings: Scopes::with_frame([]),
            type_params: HashSet::new(),
            lambda_depth: 1,
            self_escaped: false,
            after_base_init: false,
            writes: HashSet::new(),
        };
        collector.visit_block(&class.constructors[0].body);
        assert_eq!(
            collector.writes,
            HashSet::from([RawFieldWrite {
                file: FileId::ENTRY,
                self_class: Some("A".into()),
                field: "n".into(),
                kind: WriteKind::Write,
            }])
        );
    }

    #[test]
    fn constructor_writes_after_self_escapes_count() {
        let escaping = "class A { pub n: i64; pub m: i64; pub init(self) { self.m = 1; self.n = 0; let t = spawn_reader(self); self.n = 5; } } async fn spawn_reader(a: A) -> i64 { return a.n; } fn main() {}";
        assert!(reassigned(escaping, "A", "n"));
        // Written only before the escape: still construction.
        assert!(!reassigned(escaping, "A", "m"));
        // A receiver call may hand `self` on; a loop is one statement.
        let looped = "class A { pub n: i64; pub init(self) { self.n = 0; while self.n < 3 { self.n = self.n + 1; self.touch(); } } pub fn touch(self) {} } fn main() {}";
        assert!(reassigned(looped, "A", "n"));
        let deferred = "class A { pub n: i64; pub init(self) { defer { self.n = 9; } self.n = 0; let me = self; } } fn main() {}";
        assert!(reassigned(deferred, "A", "n"));
        let private = "class A { pub n: i64; pub init(self) { self.n = 0; self.n = self.n + 1; defer { self.n = 2; } } } fn main() {}";
        assert!(!reassigned(private, "A", "n"));
    }

    #[test]
    fn writes_after_escaping_base_init_count() {
        let base = "open class P { pub x: i64; pub init(self) { self.x = 0; let t = reader(self); } } async fn reader(p: P) -> i64 { return p.x; }";
        let sub = format!(
            "{base} class C extends P {{ pub y: i64; pub init(self) {{ super.init(); self.x = 5; self.y = 1; }} }} fn main() {{}}"
        );
        assert!(reassigned(&sub, "P", "x"));
        assert!(reassigned(&sub, "C", "y"));
        // Implicit base construction runs first.
        let implicit = format!(
            "{base} class C extends P {{ pub y: i64; pub init(self) {{ self.y = 1; }} }} fn main() {{}}"
        );
        assert!(reassigned(&implicit, "C", "y"));
        // Through a deeper chain.
        let deep = format!(
            "{base} open class Q extends P {{ pub init(self) {{ super.init(); }} }} class C extends Q {{ pub y: i64; pub init(self) {{ super.init(); self.y = 2; }} }} fn main() {{}}"
        );
        assert!(reassigned(&deep, "C", "y"));
        // A base whose constructor keeps `self` private does not promote.
        let private = "open class P { pub x: i64; pub init(self) { self.x = 0; } } class C extends P { pub y: i64; pub init(self) { super.init(); self.y = 1; self.x = 2; } } fn main() {}";
        assert!(!reassigned(private, "C", "y"));
        assert!(!reassigned(private, "P", "x"));
    }

    #[test]
    fn unknown_base_is_assumed_to_escape() {
        // `B` is not in the class index (an import alias or a mangled name),
        // so its constructor may let `self` escape.
        let source = "class C extends B { pub y: i64; pub init(self) { super.init(); self.y = 1; self.x = 2; } } fn main() {}";
        assert!(reassigned(source, "C", "y"));
        let program = parse(source);
        let mut writes = HashSet::new();
        for_each_body(&program, |body, context, params, type_params| {
            writes.extend(body.collect_with_bindings(context, params, type_params))
        });
        let (summary, _) = FieldWrites::resolve_counted(classes(&program), &writes);
        // `x` is declared outside the index: the conservative name set.
        assert!(summary.by_name.contains("x"));
    }

    #[test]
    fn static_writes_and_reads_are_ignored() {
        let source = "class A { pub static mut total: i64 = 0; pub n: i64; } fn main() { A::total = 1; let a = new A(1); println(a.n); }";
        assert!(!reassigned(source, "A", "n"));
    }

    /// Deep chain: every class writes the root's field. The shared inheritance
    /// traversal resolves each write with an indexed scope lookup.
    #[test]
    fn deep_chain_resolution_is_linear() {
        for depth in [16usize, 64, 256, 4096] {
            let mut source =
                String::from("open class C0 { pub f: i64; pub fn w(self) { self.f = 1; } }\n");
            for i in 1..depth {
                source.push_str(&format!(
                    "open class C{i} extends C{} {{ pub fn w{i}(self) {{ self.f = {i}; }} }}\n",
                    i - 1
                ));
            }
            source.push_str("fn main() {}\n");
            let program = parse(&source);
            let mut writes = HashSet::new();
            for_each_body(&program, |body, context, params, type_params| {
                writes.extend(body.collect_with_bindings(context, params, type_params))
            });
            assert_eq!(writes.len(), depth);
            let (summary, resolutions) = FieldWrites::resolve_counted(classes(&program), &writes);
            assert_eq!(resolutions, depth, "one resolution per (class, field)");
            assert_eq!(summary.declarations.len(), 1);
            assert!(summary.by_name.is_empty());
        }
    }

    /// Repeated identical writes collapse before resolution.
    #[test]
    fn repeated_writes_resolve_once() {
        let mut source = String::from("class A { pub n: i64;");
        for i in 0..100 {
            source.push_str(&format!(" pub fn w{i}(self) {{ self.n = {i}; }}"));
        }
        source.push_str(" } fn main() {}");
        let program = parse(&source);
        let mut writes = HashSet::new();
        for_each_body(&program, |body, context, params, type_params| {
            writes.extend(body.collect_with_bindings(context, params, type_params))
        });
        let (_, resolutions) = FieldWrites::resolve_counted(classes(&program), &writes);
        assert_eq!(resolutions, 1);
    }

    #[test]
    fn cyclic_base_chain_terminates() {
        let program = parse(
            "open class A extends B { pub fn w(self) { self.x = 1; } } open class B extends A {} fn main() {}",
        );
        let writes = HashSet::from([RawFieldWrite {
            file: FileId::ENTRY,
            self_class: Some("A".into()),
            field: "x".into(),
            kind: WriteKind::Write,
        }]);
        let (summary, _) = FieldWrites::resolve_counted(classes(&program), &writes);
        // Undeclared on the chain: falls back to the conservative name set.
        assert!(summary.by_name.contains("x"));
    }
    #[test]
    fn unresolved_and_shadowed_receivers_stay_conservative() {
        for source in [
            "class A { pub n: i64; } class B { pub n: i64; } fn make() -> B { return new B(0); } fn f() { let a = new A(0); if true { let a = make(); a.n = 1; } }",
            "class A { pub n: i64; } class B { pub n: i64; } fn f() { let a = new A(0); let write = |a| { a.n = 1; }; write(new B(0)); }",
            "class A { pub n: i64; } class B { pub n: i64; } fn f() { let a = new A(0); for a in [new B(0)] { a.n = 1; } }",
            "class A { pub n: i64; } class B { pub n: i64; } fn f<B>(b: B) { b.n = 1; }",
        ] {
            let program = parse(source);
            let summary = FieldWrites::from_program(&program);
            assert!(summary.by_name.contains("n"), "{source}");
        }
    }

    #[test]
    fn lexical_receiver_lookup_is_independent_of_scope_depth() {
        for depth in [16, 64, 256] {
            let mut source = String::from("class A { pub n: i64; } fn f() { let a = new A(0);");
            for _ in 0..depth {
                source.push_str("if true { a.n = 1;");
            }
            for _ in 0..=depth {
                source.push('}');
            }
            let program = parse(&source);
            crate::semantic::scopes::LOOKUP_PROBES.with(|n| n.set(0));
            let mut writes = HashSet::new();
            for_each_body(&program, |body, context, params, type_params| {
                writes.extend(body.collect_with_bindings(context, params, type_params));
            });
            let probes = crate::semantic::scopes::LOOKUP_PROBES.with(|n| n.get());
            assert_eq!(probes, 2 * depth);
            assert_eq!(writes.len(), 1);
            let (summary, resolutions) = FieldWrites::resolve_counted(classes(&program), &writes);
            assert_eq!(resolutions, 1);
            assert_eq!(summary.declarations.len(), 1);
            println!(
                "scope_depth={depth} receiver_probes={probes} distinct_writes=1 resolutions=1"
            );
        }
    }

    #[test]
    fn field_inventory_and_resolution_scale_in_independent_dimensions() {
        for (depth, fields) in [(16, 4), (64, 4), (256, 4), (64, 16), (64, 64)] {
            let mut source = String::from("open class C0 {");
            for f in 0..fields {
                source.push_str(&format!("pub f{f}: i64;"));
            }
            source.push('}');
            for c in 1..depth {
                source.push_str(&format!(
                    "open class C{c} extends C{} {{ pub fn write(self) {{",
                    c - 1
                ));
                for f in 0..fields {
                    source.push_str(&format!("self.f{f} = 1;"));
                }
                source.push_str("} }");
            }
            let program = parse(&source);
            let mut writes = HashSet::new();
            for_each_body(&program, |body, context, params, type_params| {
                writes.extend(body.collect_with_bindings(context, params, type_params));
            });
            let resolver = Resolver::new(classes(&program));
            let indexed_fields: usize = resolver.fields.iter().map(HashMap::len).sum();
            assert_eq!(indexed_fields, fields);
            let (summary, resolutions) = resolver.resolve(&writes);
            assert_eq!(resolutions, (depth - 1) * fields);
            assert_eq!(summary.declarations.len(), fields);
            assert!(summary.by_name.is_empty());
            println!(
                "depth={depth} fields={fields} indexed_fields={indexed_fields} write_lookups={resolutions}"
            );
        }
    }
    #[test]
    fn many_fields_at_a_deep_leaf_do_not_multiply_chain_walks() {
        for depth in [16, 64, 256, 4096] {
            let mut source = String::from("open class C0 {");
            for f in 0..64 {
                source.push_str(&format!("pub f{f}: i64;"));
            }
            source.push('}');
            for c in 1..depth {
                source.push_str(&format!("open class C{c} extends C{} {{", c - 1));
                if c == depth - 1 {
                    source.push_str("pub fn write(self) {");
                    for f in 0..64 {
                        source.push_str(&format!("self.f{f} = 1;"));
                    }
                    source.push('}');
                }
                source.push('}');
            }
            let program = parse(&source);
            let mut writes = HashSet::new();
            for_each_body(&program, |body, context, params, type_params| {
                writes.extend(body.collect_with_bindings(context, params, type_params));
            });
            crate::semantic::scopes::LOOKUP_PROBES.with(|n| n.set(0));
            let (summary, resolutions) = FieldWrites::resolve_counted(classes(&program), &writes);
            let probes = crate::semantic::scopes::LOOKUP_PROBES.with(|n| n.get());
            assert_eq!(resolutions, 64);
            assert_eq!(probes, 128);
            assert_eq!(summary.declarations.len(), 64);
            println!(
                "leaf_depth={depth} fields=64 write_lookups={resolutions} field_probes={probes}"
            );
        }
    }
}
