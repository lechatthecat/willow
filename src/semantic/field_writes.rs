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
//! * any other receiver's `.f` is unresolved without types and conservatively
//!   marks every instance field named `f`.
//!
//! A `&expr.f` call argument counts as a write because a reference parameter
//! may be `&mut`. The analysis over-approximates writes (never misses one), so
//! it can only make fewer classes `Sync`.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::diagnostics::Span;
use crate::parser::ast::*;
use crate::parser::visit::{self, AstVisitor};

/// One syntactic field write. `self_class` is the enclosing class when the
/// receiver is `self`; `None` means the receiver is unresolved.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RawFieldWrite {
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
    /// Declaration spans of fields written through `self`.
    declarations: HashSet<Span>,
}

impl FieldWrites {
    /// Whether the instance field declared at `span` as `name` may be
    /// reassigned after construction.
    pub fn is_reassigned(&self, name: &str, span: Span) -> bool {
        self.by_name.contains(name) || self.declarations.contains(&span)
    }

    /// Resolve raw writes against every class declaration of the program.
    pub(crate) fn resolve<'c, 'w>(
        classes: impl IntoIterator<Item = &'c ClassDecl>,
        writes: impl IntoIterator<Item = &'w RawFieldWrite>,
    ) -> Self {
        Self::resolve_counted(classes, writes).0
    }

    /// [`Self::resolve`] plus the number of `(class, field)` resolutions
    /// computed. Each pair is resolved once and shared by every subclass
    /// that reaches it, so the count is bounded by the distinct pairs on
    /// the written chains, not by writes × chain depth.
    pub(crate) fn resolve_counted<'c, 'w>(
        classes: impl IntoIterator<Item = &'c ClassDecl>,
        writes: impl IntoIterator<Item = &'w RawFieldWrite>,
    ) -> (Self, usize) {
        // Names are unqualified, so same-named classes from different modules
        // are merged: every candidate is followed (conservative).
        let mut index: HashMap<&str, Vec<&ClassDecl>> = HashMap::new();
        for class in classes {
            index.entry(class.name.as_str()).or_default().push(class);
        }
        let writes: Vec<&RawFieldWrite> = writes.into_iter().collect();
        let escaping: HashSet<&str> = writes
            .iter()
            .filter(|write| write.kind == WriteKind::InitEscapes)
            .filter_map(|write| write.self_class.as_deref())
            .collect();
        let mut resolver = Resolver {
            index,
            memo: HashMap::new(),
            escaping,
            escapes: HashMap::new(),
        };
        let mut summary = Self::default();
        for write in writes {
            match write.kind {
                WriteKind::Write => {}
                WriteKind::InitEscapes => continue,
                WriteKind::AfterBaseInit => {
                    let class = write.self_class.as_deref().unwrap_or_default();
                    if !resolver.base_init_escapes(class) {
                        continue;
                    }
                }
            }
            let Some(class) = write.self_class.as_deref() else {
                summary.by_name.insert(write.field.clone());
                continue;
            };
            let declarations = resolver.declarations(class, &write.field);
            if declarations.is_empty() {
                summary.by_name.insert(write.field.clone());
            }
            summary.declarations.extend(declarations.iter().copied());
        }
        (summary, resolver.memo.len())
    }

    /// Summary of one fully-bodied program (no offloaded bodies).
    pub(crate) fn from_program(program: &Program) -> Self {
        let mut writes = HashSet::new();
        for_each_body(program, |body, context| {
            writes.extend(body.collect(context));
        });
        Self::resolve(classes(program), &writes)
    }
}

/// Memoized "nearest declaration of `field` reachable from `class`" and
/// "some constructor on the chain from `class` lets `self` escape".
struct Resolver<'c, 'w> {
    index: HashMap<&'c str, Vec<&'c ClassDecl>>,
    memo: HashMap<(&'w str, &'w str), Rc<[Span]>>,
    /// Classes with a constructor that lets `self` escape.
    escaping: HashSet<&'w str>,
    /// Memo: some constructor on the chain from this class escapes.
    escapes: HashMap<&'w str, bool>,
}

impl<'c: 'w, 'w> Resolver<'c, 'w> {
    /// Whether a base constructor run by `class`'s constructor may let `self`
    /// escape.
    fn base_init_escapes(&mut self, class: &'w str) -> bool {
        let bases: Vec<&'c str> = self
            .index
            .get(class)
            .into_iter()
            .flatten()
            .filter_map(|decl| decl.base_class.as_ref().map(|base| base.name()))
            .collect();
        bases.into_iter().any(|base| self.chain_escapes(base))
    }

    /// Iterative post-order like [`Self::declarations`].
    fn chain_escapes(&mut self, class: &'w str) -> bool {
        let mut entered = HashSet::new();
        let mut stack = vec![(class, false)];
        while let Some((name, expanded)) = stack.pop() {
            if self.escapes.contains_key(name) {
                continue;
            }
            // A base absent from the index (an import alias, a mangled
            // instantiation name) is unknown: assume its constructor escapes.
            let Some(candidates) = self.index.get(name).map(Vec::as_slice) else {
                self.escapes.insert(name, true);
                continue;
            };
            if !expanded {
                if !entered.insert(name) {
                    continue;
                }
                stack.push((name, true));
                for decl in candidates {
                    if let Some(base) = &decl.base_class {
                        stack.push((base.name(), false));
                    }
                }
                continue;
            }
            let escapes = self.escaping.contains(name)
                || candidates.iter().any(|decl| {
                    decl.base_class
                        .as_ref()
                        .is_some_and(|base| self.escapes.get(base.name()) == Some(&true))
                });
            self.escapes.insert(name, escapes);
        }
        self.escapes[class]
    }

    /// Iterative post-order over the base chain, so a deep hierarchy cannot
    /// overflow the compiler stack.
    fn declarations(&mut self, class: &'w str, field: &'w str) -> Rc<[Span]> {
        let mut entered = HashSet::new();
        let mut stack = vec![(class, false)];
        while let Some((name, expanded)) = stack.pop() {
            if self.memo.contains_key(&(name, field)) {
                continue;
            }
            let candidates = self.index.get(name).map(Vec::as_slice).unwrap_or(&[]);
            let declared = |decl: &&'c ClassDecl| {
                decl.fields
                    .iter()
                    .find(|f| !f.is_static && f.name == field)
                    .map(|f| f.span)
            };
            if !expanded {
                // A (rejected elsewhere) cyclic base chain ends here.
                if !entered.insert(name) {
                    continue;
                }
                stack.push((name, true));
                for decl in candidates {
                    if declared(decl).is_none()
                        && let Some(base) = &decl.base_class
                    {
                        stack.push((base.name(), false));
                    }
                }
                continue;
            }
            let mut found = Vec::new();
            for decl in candidates {
                if let Some(span) = declared(decl) {
                    found.push(span);
                } else if let Some(base) = &decl.base_class
                    && let Some(inherited) = self.memo.get(&(base.name(), field))
                {
                    found.extend(inherited.iter().copied());
                }
            }
            self.memo.insert((name, field), found.into());
        }
        Rc::clone(&self.memo[&(class, field)])
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
    pub(crate) fn collect(&self, context: WriteContext<'_>) -> HashSet<RawFieldWrite> {
        let mut collector = Collector {
            context,
            lambda_depth: 0,
            self_escaped: false,
            after_base_init: false,
            writes: HashSet::new(),
        };
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
    mut f: impl FnMut(BodyRef<'a>, WriteContext<'a>),
) {
    for item in &program.items {
        match item {
            Item::Function(function) => f(BodyRef::Block(&function.body), WriteContext::Free),
            Item::Class(class) => {
                for method in &class.methods {
                    f(
                        BodyRef::Block(&method.body),
                        WriteContext::Method(&class.name),
                    );
                }
                for constructor in &class.constructors {
                    f(
                        BodyRef::Block(&constructor.body),
                        WriteContext::Constructor(&class.name),
                    );
                }
                for field in &class.fields {
                    if let Some(expr) = &field.initializer {
                        f(BodyRef::Initializer(expr), WriteContext::Free);
                    }
                }
            }
            Item::Interface(interface) => {
                for method in &interface.methods {
                    if let Some(body) = &method.default_body {
                        f(BodyRef::Block(body), WriteContext::Free);
                    }
                }
            }
            Item::Enum(_) => {}
        }
    }
}

struct Collector<'a> {
    context: WriteContext<'a>,
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
            _ => None,
        };
        self.writes.insert(RawFieldWrite {
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
    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
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
        visit::walk_lambda(self, lambda);
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
    fn unresolved_receiver_marks_every_same_named_field() {
        let source = "class A { pub n: i64; } class B { pub n: i64; pub m: i64; } fn main() { let b = new B(1, 2); b.n = 3; }";
        assert!(reassigned(source, "A", "n"));
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
            lambda_depth: 1,
            self_escaped: false,
            after_base_init: false,
            writes: HashSet::new(),
        };
        collector.visit_block(&class.constructors[0].body);
        assert_eq!(
            collector.writes,
            HashSet::from([RawFieldWrite {
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
        for_each_body(&program, |body, context| {
            writes.extend(body.collect(context))
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

    /// Deep chain: every class writes the root's field. Each `(class, field)`
    /// resolves once and reuses its base's result, so the work is linear in
    /// the chain length rather than quadratic.
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
            for_each_body(&program, |body, context| {
                writes.extend(body.collect(context))
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
        for_each_body(&program, |body, context| {
            writes.extend(body.collect(context))
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
            self_class: Some("A".into()),
            field: "x".into(),
            kind: WriteKind::Write,
        }]);
        let (summary, _) = FieldWrites::resolve_counted(classes(&program), &writes);
        // Undeclared on the chain: falls back to the conservative name set.
        assert!(summary.by_name.contains("x"));
    }
}
