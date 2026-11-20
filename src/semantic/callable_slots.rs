//! Callable kinds of declaration slots.
//!
//! Users write one function type, `fn(T) -> R`. The checker keeps two
//! internal kinds of it: the shareable one (Send + Sync, see
//! [`Type::Fn`]) and the task-local one ([`Type::Closure`]). A function-typed
//! *declaration slot* — a non-static class field, a constructor parameter, an
//! enum variant payload — gets its kind from the values the whole build
//! stores into it:
//!
//! * a slot is shareable unless some store may put a task-local value into
//!   it. Named functions, `await`ed async results, async parameters and
//!   lambdas whose every capture is an immutable scalar, `String` or
//!   shareable function value are shareable; other values (calls returning a
//!   function, task-local parameters, lambdas capturing mutable or unknown
//!   state, array elements, ...) are task-local. Values copied from another
//!   slot (`c.f`, a constructor parameter) inherit that slot's kind;
//! * a class that implements `Send`/`Sync`, an interface extending one of
//!   them (across files and imports), or that inherits from such a class
//!   promises to be shareable: all its instance fields and constructor
//!   parameters are the shareable kind at every depth.
//!
//! The summary is syntactic, so it is a prediction: the checker still checks
//! every store against the slot's kind. A misprediction can only cause a
//! false E1011/E2402, never an unsound acceptance.
//!
//! Cost: one walk per body records its stores (O(body), O(1) per variable
//! reference for the capture test); resolution builds one graph node per slot
//! plus one per distinct by-name target/source, one edge per store, and one
//! worklist pass — linear in declarations + stores.

use std::collections::{HashMap, HashSet};

use crate::diagnostics::{FileId, Span};
use crate::parser::ast::*;
use crate::parser::iter::{AstEvent, AstWalk};
use crate::qualified_name::QualifiedName;
use crate::semantic::field_writes::{BodyRef, DeclNames};
use crate::semantic::scopes::Scopes;

/// Where a body sits, which decides what `self`, parameters and a
/// constructor's stores mean.
#[derive(Clone, Copy, Default)]
pub(crate) struct StoreContext<'a> {
    /// The class of a method, constructor or field initializer.
    pub(crate) class: Option<&'a str>,
    pub(crate) constructor: bool,
    /// Async signatures carry shareable function parameters.
    pub(crate) is_async: bool,
    /// An instance-field initializer stores into this field.
    pub(crate) field: Option<Span>,
}

/// The slot (or slots) a store writes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum StoreTarget {
    /// `recv.field = v` / object literal; `None`: unresolved receiver.
    Field {
        class: Option<String>,
        field: String,
    },
    /// An instance-field initializer.
    Declared(Span),
    /// `new class(…)` argument `index` of `arity`.
    New {
        class: String,
        arity: usize,
        index: usize,
    },
    /// `super.init(…)` argument in a constructor of `class`.
    BaseInit {
        class: String,
        arity: usize,
        index: usize,
    },
    /// `E::V(…)` (`enum_name` given) or `V(…)` payload `index`.
    Variant {
        enum_name: Option<String>,
        name: String,
        index: usize,
    },
}

/// A stored value that is not known to be shareable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum StoredValue {
    TaskLocal,
    /// A free name: shareable iff it resolves to a top-level function.
    Function(String),
    /// A field read: the field's slot kind.
    Field {
        class: Option<String>,
        field: String,
    },
    /// The current constructor's parameter declared at this span.
    Param(Span),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RawStore {
    pub(crate) file: FileId,
    pub(crate) target: StoreTarget,
    pub(crate) value: StoredValue,
}

/// At most this many distinct values are tracked per binding; more collapse
/// to task-local, so alias chains of conditionals stay linear.
const VALUE_CAP: usize = 4;

#[derive(Clone)]
struct Binding {
    values: Vec<StoredValue>,
    /// Capturing it keeps a lambda shareable.
    capture_ok: bool,
    class: Option<String>,
    /// Lambda nesting depth of the binding.
    depth: usize,
}

impl Binding {
    fn opaque(depth: usize) -> Self {
        Self {
            values: vec![StoredValue::TaskLocal],
            capture_ok: false,
            class: None,
            depth,
        }
    }
}

fn is_scalar(ty: &Type) -> bool {
    matches!(
        ty,
        Type::I8
            | Type::I16
            | Type::I32
            | Type::U16
            | Type::U32
            | Type::U64
            | Type::Char
            | Type::U8
            | Type::I64
            | Type::F64
            | Type::Bool
            | Type::String
    )
}

fn is_callable(ty: &Type) -> bool {
    matches!(ty, Type::Fn(..) | Type::Closure(..))
}

fn is_variant_name(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

struct PendingLet<'a> {
    init: &'a Expr,
    values: Vec<StoredValue>,
    literal_ok: bool,
    class: Option<String>,
}

struct Collector<'a> {
    file: FileId,
    context: StoreContext<'a>,
    type_params: HashSet<&'a str>,
    bindings: Scopes<Binding>,
    /// One entry per open walker scope: whether it is a lambda's scope.
    scope_kinds: Vec<bool>,
    /// Open lambdas: id and the minimum depth of a captured binding that
    /// makes it task-local (`usize::MAX`: none yet).
    lambdas: Vec<(ExprId, usize)>,
    /// Closed lambdas: whether each is task-local.
    lambda_task_local: HashMap<ExprId, bool>,
    lets: Vec<PendingLet<'a>>,
    stores: HashSet<RawStore>,
}

impl<'a> BodyRef<'a> {
    /// The stores of one body slot.
    pub(crate) fn collect_stores(
        &self,
        context: StoreContext<'a>,
        params: &'a [Param],
        type_params: &'a [String],
    ) -> HashSet<RawStore> {
        let file = match self {
            BodyRef::Block(block) => block.span.file_id,
            BodyRef::Initializer(expr) => expr.span().file_id,
        };
        let mut collector = Collector {
            file,
            context,
            type_params: type_params.iter().map(String::as_str).collect(),
            bindings: Scopes::with_frame([]),
            scope_kinds: Vec::new(),
            lambdas: Vec::new(),
            lambda_task_local: HashMap::new(),
            lets: Vec::new(),
            stores: HashSet::new(),
        };
        for param in params {
            let callable = is_callable(&param.ty);
            let (values, capture_ok) = if !callable {
                (Vec::new(), is_scalar(&param.ty))
            } else if context.is_async {
                (Vec::new(), true)
            } else if context.constructor {
                (vec![StoredValue::Param(param.span)], false)
            } else {
                (vec![StoredValue::TaskLocal], false)
            };
            let class = collector.nominal(&param.ty);
            collector.bindings.insert(
                param.name.clone(),
                Binding {
                    values,
                    capture_ok,
                    class,
                    depth: 0,
                },
            );
        }
        match self {
            BodyRef::Block(block) => collector.walk(AstEvent::Block(block)),
            BodyRef::Initializer(expr) => {
                collector.walk(AstEvent::Expr(expr));
                if let Some(field) = context.field {
                    collector.record(StoreTarget::Declared(field), expr);
                }
            }
        }
        collector.stores
    }
}

impl<'a> Collector<'a> {
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
            Expr::Var(name, ..) if name == "self" => self.context.class.map(str::to_owned),
            Expr::New(call) if !self.type_params.contains(call.class_name.as_str()) => {
                Some(call.class_name.clone())
            }
            Expr::Var(name, ..) => self.bindings.get(name).and_then(|b| b.class.clone()),
            _ => None,
        }
    }

    /// Values `expr` may evaluate to that are not known to be shareable.
    /// Non-function expressions yield nothing.
    fn values(&self, expr: &Expr) -> Vec<StoredValue> {
        let mut out = Vec::new();
        let mut work = vec![expr];
        while let Some(expr) = work.pop() {
            match expr {
                Expr::Lambda(lambda) => {
                    if self.lambda_task_local.get(&lambda.id) != Some(&false) {
                        out.push(StoredValue::TaskLocal);
                    }
                }
                Expr::Var(name, ..) => match self.bindings.get(name) {
                    Some(binding) => out.extend(binding.values.iter().cloned()),
                    None if name == "self" || name == "super" => out.push(StoredValue::TaskLocal),
                    None => out.push(StoredValue::Function(name.clone())),
                },
                Expr::FieldAccess(object, field, ..) => out.push(StoredValue::Field {
                    class: self.receiver(object),
                    field: field.clone(),
                }),
                // Async signatures return the shareable kind.
                Expr::Await(inner)
                    if matches!(
                        inner.expr,
                        Expr::Call(_) | Expr::MethodCall(_) | Expr::StaticCall(_)
                    ) => {}
                Expr::Ternary(ternary) => {
                    work.push(&ternary.then_expr);
                    work.push(&ternary.else_expr);
                }
                Expr::Integer(..)
                | Expr::Char(..)
                | Expr::Float(..)
                | Expr::Bool(..)
                | Expr::String(..)
                | Expr::Binary(_)
                | Expr::Unary(_)
                | Expr::New(_)
                | Expr::ObjectLiteral(_)
                | Expr::ArrayLiteral(..)
                | Expr::Range(_)
                | Expr::Print(..) => {}
                _ => out.push(StoredValue::TaskLocal),
            }
            if out.len() > VALUE_CAP {
                return vec![StoredValue::TaskLocal];
            }
        }
        out.sort_unstable_by_key(|v| matches!(v, StoredValue::TaskLocal));
        out.dedup();
        out
    }

    fn record(&mut self, target: StoreTarget, value: &Expr) {
        for value in self.values(value) {
            self.stores.insert(RawStore {
                file: self.file,
                target: target.clone(),
                value,
            });
        }
    }

    fn record_args(&mut self, args: &[CallArg], target: impl Fn(usize) -> StoreTarget) {
        for (index, arg) in args.iter().enumerate() {
            self.record(target(index), &arg.expr);
        }
    }

    /// A reference to `name` from the innermost open lambda.
    fn reference(&mut self, name: &str) {
        let depth = self.lambdas.len();
        let Some((_, bad)) = self.lambdas.last_mut() else {
            return;
        };
        match self.bindings.get(name) {
            Some(binding) if binding.depth < depth && !binding.capture_ok => {
                *bad = (*bad).min(binding.depth)
            }
            Some(_) => {}
            None if name == "self" || name == "super" => *bad = 0,
            None => {}
        }
    }

    fn walk(&mut self, root: AstEvent<'a>) {
        let mut walk = AstWalk::new(root);
        while let Some(event) = walk.next() {
            match event {
                AstEvent::EnterScope => {
                    self.bindings.push();
                    self.scope_kinds.push(false);
                }
                AstEvent::ExitScope => {
                    self.bindings.pop();
                    if self.scope_kinds.pop() == Some(true) {
                        let (id, bad) = self.lambdas.pop().expect("open lambda");
                        let depth = self.lambdas.len() + 1;
                        self.lambda_task_local.insert(id, bad < depth);
                        if let Some((_, parent)) = self.lambdas.last_mut() {
                            *parent = (*parent).min(bad);
                        }
                    }
                }
                AstEvent::Bind(name) => {
                    let binding = Binding::opaque(self.lambdas.len());
                    self.bindings.insert(name.to_owned(), binding);
                }
                AstEvent::Expr(Expr::Var(name, ..)) => self.reference(name),
                AstEvent::Stmt(Stmt::Assign(assign)) => self.reference(&assign.name),
                AstEvent::Stmt(Stmt::Let(binding)) => {
                    // Bindings visible to the initializer are the same before
                    // and after it is walked; its lambdas close before its
                    // exit event, where the values are taken.
                    let class = match &binding.ty {
                        Some(ty) => self.nominal(ty),
                        None => self.receiver(&binding.init),
                    };
                    let literal_ok = match &binding.ty {
                        Some(ty) => is_scalar(ty),
                        None => matches!(
                            binding.init,
                            Expr::Integer(..)
                                | Expr::Char(..)
                                | Expr::Float(..)
                                | Expr::Bool(..)
                                | Expr::String(..)
                        ),
                    };
                    self.lets.push(PendingLet {
                        init: &binding.init,
                        values: vec![StoredValue::TaskLocal],
                        literal_ok,
                        class,
                    });
                }
                AstEvent::ExitExpr(expr) => {
                    if let Some(pending) = self.lets.last()
                        && std::ptr::eq(pending.init, expr)
                    {
                        let values = self.values(expr);
                        let pending = self.lets.last_mut().expect("let entered");
                        pending.values = values;
                        // A shareable lambda or an alias of a capturable
                        // immutable binding is itself capturable.
                        pending.literal_ok |= match expr {
                            Expr::Lambda(lambda) => {
                                self.lambda_task_local.get(&lambda.id) == Some(&false)
                            }
                            Expr::Var(name, ..) => {
                                self.bindings.get(name).is_some_and(|b| b.capture_ok)
                            }
                            _ => false,
                        };
                    }
                    self.exit_expr(expr);
                }
                AstEvent::ExitStmt(Stmt::Let(binding)) => {
                    let pending = self.lets.pop().expect("let entered");
                    let depth = self.lambdas.len();
                    let entry = if binding.mutable {
                        Binding {
                            class: pending.class,
                            ..Binding::opaque(depth)
                        }
                    } else {
                        Binding {
                            values: pending.values,
                            capture_ok: pending.literal_ok,
                            class: pending.class,
                            depth,
                        }
                    };
                    // Replaces the opaque binding the walker just made.
                    self.bindings.insert(binding.name.clone(), entry);
                }
                AstEvent::ExitStmt(Stmt::FieldAssign(assign)) => {
                    let class = self.receiver(&assign.object);
                    let target = StoreTarget::Field {
                        class,
                        field: assign.field.clone(),
                    };
                    self.record(target, &assign.value);
                }
                AstEvent::ExitStmt(Stmt::SuperInit(init)) => {
                    if let Some(class) = self.context.class {
                        let arity = init.args.len();
                        self.record_args(&init.args, |index| StoreTarget::BaseInit {
                            class: class.to_owned(),
                            arity,
                            index,
                        });
                    }
                }
                AstEvent::Lambda(lambda) => {
                    // Parameters bind with their annotations; the scope's
                    // exit closes the lambda.
                    walk.skip_children();
                    self.lambdas.push((lambda.id, usize::MAX));
                    self.bindings.push();
                    self.scope_kinds.push(true);
                    let depth = self.lambdas.len();
                    for param in &lambda.params {
                        let ty = param.ty.as_ref();
                        let binding = Binding {
                            capture_ok: ty.is_some_and(is_scalar),
                            class: ty.and_then(|ty| self.nominal(ty)),
                            ..Binding::opaque(depth)
                        };
                        self.bindings.insert(param.name.clone(), binding);
                    }
                    walk.push(AstEvent::ExitScope);
                    walk.push(match &lambda.body {
                        LambdaBody::Expr(expr) => AstEvent::Expr(expr),
                        LambdaBody::Block(block) => AstEvent::Block(block),
                    });
                }
                _ => {}
            }
        }
    }

    fn exit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::New(new) if !self.type_params.contains(new.class_name.as_str()) => {
                let arity = new.args.len();
                let class = new.class_name.clone();
                self.record_args(&new.args, |index| StoreTarget::New {
                    class: class.clone(),
                    arity,
                    index,
                });
            }
            Expr::ObjectLiteral(literal) if !self.type_params.contains(literal.class.as_str()) => {
                for field in &literal.fields {
                    let target = StoreTarget::Field {
                        class: Some(literal.class.clone()),
                        field: field.name.clone(),
                    };
                    self.record(target, &field.value);
                }
            }
            Expr::Call(call) => {
                let name = QualifiedName::new(&call.callee).last_segment();
                if is_variant_name(name) {
                    let name = name.to_owned();
                    self.record_args(&call.args, |index| StoreTarget::Variant {
                        enum_name: None,
                        name: name.clone(),
                        index,
                    });
                }
            }
            Expr::StaticCall(call) if is_variant_name(&call.method) => {
                let (enum_name, name) = (call.class.clone(), call.method.clone());
                self.record_args(&call.args, |index| StoreTarget::Variant {
                    enum_name: Some(enum_name.clone()),
                    name: name.clone(),
                    index,
                });
            }
            _ => {}
        }
    }
}

/// Visit every body slot of `program` with its store context.
pub(crate) fn for_each_body<'a>(
    program: &'a Program,
    mut f: impl FnMut(BodyRef<'a>, StoreContext<'a>, &'a [Param], &'a [String]),
) {
    for item in &program.items {
        match item {
            Item::Function(function) => f(
                BodyRef::Block(&function.body),
                StoreContext {
                    is_async: function.is_async,
                    ..StoreContext::default()
                },
                &function.params,
                &function.type_params,
            ),
            Item::Class(class) => {
                let class_context = StoreContext {
                    class: Some(&class.name),
                    ..StoreContext::default()
                };
                for method in &class.methods {
                    let context = StoreContext {
                        is_async: method.is_async,
                        ..class_context
                    };
                    f(
                        BodyRef::Block(&method.body),
                        context,
                        &method.params,
                        &class.type_params,
                    );
                }
                for constructor in &class.constructors {
                    let context = StoreContext {
                        constructor: true,
                        ..class_context
                    };
                    f(
                        BodyRef::Block(&constructor.body),
                        context,
                        &constructor.params,
                        &class.type_params,
                    );
                }
                for field in &class.fields {
                    if let Some(expr) = &field.initializer {
                        let context = StoreContext {
                            field: (!field.is_static).then_some(field.span),
                            ..class_context
                        };
                        f(BodyRef::Initializer(expr), context, &[], &class.type_params);
                    }
                }
            }
            Item::Interface(interface) => {
                for method in &interface.methods {
                    if let Some(body) = &method.default_body {
                        f(
                            BodyRef::Block(body),
                            StoreContext::default(),
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

/// A function-typed declaration slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SlotKey {
    /// A field or constructor parameter, by its declaration span.
    Decl(Span),
    /// Payload `index` of the variant declared at the span.
    Payload(Span, usize),
}

/// Deterministic work counts of one resolution, for complexity tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotStats {
    pub(crate) stores: usize,
    pub(crate) nodes: usize,
    pub(crate) edges: usize,
    pub(crate) visits: usize,
}

/// Kind decisions of one build.
#[derive(Debug, Default)]
struct Decisions {
    task_local: HashSet<SlotKey>,
    promised: HashSet<Span>,
}

struct Graph {
    edges: Vec<Vec<usize>>,
    pinned: Vec<bool>,
    tainted: Vec<bool>,
    stats: SlotStats,
}

impl Graph {
    fn node(&mut self) -> usize {
        self.edges.push(Vec::new());
        self.pinned.push(false);
        self.tainted.push(false);
        self.stats.nodes += 1;
        self.edges.len() - 1
    }

    fn edge(&mut self, from: usize, to: usize) {
        self.edges[from].push(to);
        self.stats.edges += 1;
    }
}

struct Index<'a> {
    classes: Vec<(&'a ClassDecl, FileId)>,
    class_names: DeclNames<'a>,
    interfaces: Vec<&'a InterfaceDecl>,
    interface_names: DeclNames<'a>,
    function_names: DeclNames<'a>,
    enum_names: DeclNames<'a>,
    enums: Vec<&'a EnumDecl>,
    bases: Vec<Option<usize>>,
    fields: Vec<HashMap<&'a str, Span>>,
    slots: HashMap<SlotKey, usize>,
    /// Field slots by name, for unresolved receivers.
    field_slots: HashMap<&'a str, Vec<usize>>,
    /// Variant payload slots by (variant name, index), for unqualified
    /// constructions.
    payload_slots: HashMap<(&'a str, usize), Vec<usize>>,
    by_name_targets: HashMap<String, usize>,
    by_name_sources: HashMap<String, usize>,
    by_variant: HashMap<(String, usize), usize>,
    field_memo: HashMap<(usize, String), Option<Span>>,
    implicit_memo: HashMap<usize, Vec<Span>>,
    promised_spans: HashSet<Span>,
    graph: Graph,
}

fn items<'a, T>(
    programs: &[(&'a str, &'a Program)],
    select: impl Fn(&'a Item) -> Option<&'a T>,
) -> Vec<(&'a str, &'a T)> {
    programs
        .iter()
        .flat_map(|(namespace, program)| {
            program
                .items
                .iter()
                .filter_map(&select)
                .map(move |decl| (*namespace, decl))
        })
        .collect()
}

fn names<'a>(
    programs: &[(&'a str, &'a Program)],
    dependencies: Option<&'a crate::compiler_db::dependencies::ModuleDependencies>,
    decls: &[(&'a str, FileId, &'a str)],
) -> DeclNames<'a> {
    let mut names = DeclNames::new(decls.iter().map(|(_, file, name)| (*file, *name)));
    names.dependencies = dependencies;
    for (id, (namespace, _, name)) in decls.iter().enumerate() {
        names.qualify(namespace, name, id);
    }
    for (_, program) in programs {
        names.add_imports(program);
    }
    names
}

impl<'a> Index<'a> {
    fn new(
        programs: &[(&'a str, &'a Program)],
        dependencies: Option<&'a crate::compiler_db::dependencies::ModuleDependencies>,
    ) -> Self {
        let classes = items(programs, |item| match item {
            Item::Class(class) => Some(class),
            _ => None,
        });
        let interfaces = items(programs, |item| match item {
            Item::Interface(interface) => Some(interface),
            _ => None,
        });
        let enums = items(programs, |item| match item {
            Item::Enum(decl) => Some(decl),
            _ => None,
        });
        let functions = items(programs, |item| match item {
            Item::Function(function) => Some(function),
            _ => None,
        });
        let class_decls: Vec<_> = classes
            .iter()
            .map(|(ns, c)| (*ns, c.span.file_id, c.name.as_str()))
            .collect();
        let interface_decls: Vec<_> = interfaces
            .iter()
            .map(|(ns, i)| (*ns, i.span.file_id, i.name.as_str()))
            .collect();
        let enum_decls: Vec<_> = enums
            .iter()
            .map(|(ns, e)| (*ns, e.span.file_id, e.name.as_str()))
            .collect();
        let function_decls: Vec<_> = functions
            .iter()
            .map(|(ns, f)| (*ns, f.span.file_id, f.name.as_str()))
            .collect();
        let class_names = names(programs, dependencies, &class_decls);
        let bases = classes
            .iter()
            .map(|(_, c)| {
                c.base_class
                    .as_ref()
                    .and_then(|base| class_names.lookup(c.span.file_id, base.name()))
            })
            .collect();
        let fields = classes
            .iter()
            .map(|(_, c)| {
                c.fields
                    .iter()
                    .filter(|f| !f.is_static)
                    .map(|f| (f.name.as_str(), f.span))
                    .collect()
            })
            .collect();
        let mut index = Self {
            classes: classes.iter().map(|(_, c)| (*c, c.span.file_id)).collect(),
            class_names,
            interfaces: interfaces.iter().map(|(_, i)| *i).collect(),
            interface_names: names(programs, dependencies, &interface_decls),
            function_names: names(programs, dependencies, &function_decls),
            enum_names: names(programs, dependencies, &enum_decls),
            enums: enums.iter().map(|(_, e)| *e).collect(),
            bases,
            fields,
            slots: HashMap::new(),
            field_slots: HashMap::new(),
            payload_slots: HashMap::new(),
            by_name_targets: HashMap::new(),
            by_name_sources: HashMap::new(),
            by_variant: HashMap::new(),
            field_memo: HashMap::new(),
            implicit_memo: HashMap::new(),
            promised_spans: HashSet::new(),
            graph: Graph {
                edges: Vec::new(),
                pinned: Vec::new(),
                tainted: Vec::new(),
                stats: SlotStats::default(),
            },
        };
        let promised = index.promised();
        for (id, &class_promised) in promised.iter().enumerate() {
            let (class, _) = index.classes[id];
            for field in class.fields.iter().filter(|f| !f.is_static) {
                if is_callable(&field.ty) {
                    let node = index.slot(SlotKey::Decl(field.span), class_promised);
                    index
                        .field_slots
                        .entry(field.name.as_str())
                        .or_default()
                        .push(node);
                }
            }
            for constructor in &class.constructors {
                for param in &constructor.params {
                    if is_callable(&param.ty) {
                        index.slot(SlotKey::Decl(param.span), class_promised);
                    }
                }
            }
        }
        for id in 0..index.enums.len() {
            let decl = index.enums[id];
            for variant in &decl.variants {
                for (position, ty) in variant.payload.iter().enumerate() {
                    if is_callable(ty) {
                        let node = index.slot(SlotKey::Payload(variant.span, position), false);
                        index
                            .payload_slots
                            .entry((variant.name.as_str(), position))
                            .or_default()
                            .push(node);
                    }
                }
            }
        }
        index.promised_spans = promised
            .iter()
            .enumerate()
            .filter(|(_, promised)| **promised)
            .map(|(id, _)| index.classes[id].0.span)
            .collect();
        index
    }

    fn slot(&mut self, key: SlotKey, pinned: bool) -> usize {
        let node = self.graph.node();
        self.graph.pinned[node] = pinned;
        self.slots.insert(key, node);
        node
    }

    /// Whether each class promises Send/Sync: through `implements` (an
    /// interface chain reaching `Send`/`Sync`) or a promised base. Memoized
    /// iterative walks; a cycle promises nothing.
    fn promised(&self) -> Vec<bool> {
        let mut interface_memo: Vec<Option<bool>> = vec![None; self.interfaces.len()];
        let mut on_path = vec![false; self.interfaces.len()];
        let interface_promises =
            |memo: &mut Vec<Option<bool>>, on_path: &mut Vec<bool>, root: usize| -> bool {
                let mut stack = vec![(root, 0usize)];
                on_path[root] = true;
                while let Some((id, next)) = stack.pop() {
                    let interface = self.interfaces[id];
                    let file = interface.span.file_id;
                    if memo[id].is_some() {
                        on_path[id] = false;
                        continue;
                    }
                    if let Some(parent) = interface.extends.get(next) {
                        stack.push((id, next + 1));
                        if parent == "Send" || parent == "Sync" {
                            memo[id] = Some(true);
                            continue;
                        }
                        if let Some(parent) = self.interface_names.lookup(file, parent)
                            && memo[parent].is_none()
                            && !on_path[parent]
                        {
                            on_path[parent] = true;
                            stack.push((parent, 0));
                        }
                        continue;
                    }
                    let found = interface.extends.iter().any(|parent| {
                        self.interface_names
                            .lookup(file, parent)
                            .is_some_and(|parent| memo[parent] == Some(true))
                    });
                    memo[id] = Some(found);
                    on_path[id] = false;
                }
                memo[root] == Some(true)
            };
        let own: Vec<bool> = self
            .classes
            .iter()
            .map(|(class, file)| {
                class.implements.iter().any(|ty| {
                    let (Type::Named(name) | Type::Generic(name, _)) = ty else {
                        return false;
                    };
                    if name == "Send" || name == "Sync" {
                        return true;
                    }
                    self.interface_names
                        .lookup(*file, name)
                        .is_some_and(|id| match interface_memo[id] {
                            Some(found) => found,
                            None => interface_promises(&mut interface_memo, &mut on_path, id),
                        })
                })
            })
            .collect();
        // Base chains, path-compressed: each class is resolved once.
        let mut memo: Vec<Option<bool>> = vec![None; self.classes.len()];
        for start in 0..self.classes.len() {
            let mut path = Vec::new();
            let mut seen = HashSet::new();
            let mut id = start;
            let result = loop {
                if let Some(result) = memo[id] {
                    break result;
                }
                if !seen.insert(id) {
                    break false;
                }
                path.push(id);
                if own[id] {
                    break true;
                }
                match self.bases[id] {
                    Some(base) => id = base,
                    None => break false,
                }
            };
            for id in path {
                memo[id] = Some(result);
            }
        }
        memo.into_iter().map(|p| p == Some(true)).collect()
    }

    /// The declaration of instance field `field` visible in class `id`.
    fn field(&mut self, id: usize, field: &str) -> Option<Span> {
        let key = (id, field.to_owned());
        if let Some(found) = self.field_memo.get(&key) {
            return *found;
        }
        let mut path = Vec::new();
        let mut current = Some(id);
        let mut found = None;
        while let Some(class) = current {
            if let Some(hit) = self.field_memo.get(&(class, field.to_owned())) {
                found = *hit;
                break;
            }
            if path.contains(&class) {
                break;
            }
            path.push(class);
            if let Some(span) = self.fields[class].get(field) {
                found = Some(*span);
                break;
            }
            current = self.bases[class];
        }
        for class in path {
            self.field_memo.insert((class, field.to_owned()), found);
        }
        found
    }

    /// The slot for argument `index` of a `arity`-argument construction of
    /// class `id`: the first constructor on the chain with that arity, or the
    /// implicit constructor's fields (base first) when no class declares one.
    fn constructor_slot(&mut self, id: usize, arity: usize, index: usize) -> Option<usize> {
        let mut current = Some(id);
        let mut seen = HashSet::new();
        while let Some(class) = current {
            if !seen.insert(class) {
                return None;
            }
            let (decl, _) = self.classes[class];
            if !decl.constructors.is_empty() {
                let constructor = decl.constructors.iter().find(|c| c.params.len() == arity)?;
                return self
                    .slots
                    .get(&SlotKey::Decl(constructor.params.get(index)?.span))
                    .copied();
            }
            current = self.bases[class];
        }
        if !self.implicit_memo.contains_key(&id) {
            let mut chain = Vec::new();
            let mut seen = HashSet::new();
            let mut current = Some(id);
            while let Some(class) = current {
                if !seen.insert(class) {
                    break;
                }
                chain.push(class);
                current = self.bases[class];
            }
            let fields = chain
                .iter()
                .rev()
                .flat_map(|class| {
                    self.classes[*class]
                        .0
                        .fields
                        .iter()
                        .filter(|f| !f.is_static)
                        .map(|f| f.span)
                })
                .collect();
            self.implicit_memo.insert(id, fields);
        }
        let span = *self.implicit_memo[&id].get(index)?;
        self.slots.get(&SlotKey::Decl(span)).copied()
    }

    fn by_name_target(&mut self, field: &str) -> usize {
        if let Some(node) = self.by_name_targets.get(field) {
            return *node;
        }
        let node = self.graph.node();
        for slot in self.field_slots.get(field).cloned().unwrap_or_default() {
            self.graph.edge(node, slot);
        }
        self.by_name_targets.insert(field.to_owned(), node);
        node
    }

    fn by_name_source(&mut self, field: &str) -> usize {
        if let Some(node) = self.by_name_sources.get(field) {
            return *node;
        }
        let node = self.graph.node();
        for slot in self.field_slots.get(field).cloned().unwrap_or_default() {
            self.graph.edge(slot, node);
        }
        self.by_name_sources.insert(field.to_owned(), node);
        node
    }

    fn class(&self, file: FileId, name: &str) -> Option<usize> {
        self.class_names.lookup(file, name)
    }

    fn field_node(&mut self, file: FileId, class: &Option<String>, field: &str) -> FieldNode {
        let Some(class) = class else {
            return FieldNode::ByName;
        };
        match self.class(file, class) {
            None => FieldNode::ByName,
            Some(id) => match self.field(id, field) {
                Some(span) => self
                    .slots
                    .get(&SlotKey::Decl(span))
                    .map_or(FieldNode::None, |n| FieldNode::Slot(*n)),
                None => FieldNode::None,
            },
        }
    }

    fn target(&mut self, file: FileId, target: &StoreTarget) -> Option<usize> {
        match target {
            StoreTarget::Field { class, field } => match self.field_node(file, class, field) {
                FieldNode::Slot(node) => Some(node),
                FieldNode::ByName => Some(self.by_name_target(field)),
                FieldNode::None => None,
            },
            StoreTarget::Declared(span) => self.slots.get(&SlotKey::Decl(*span)).copied(),
            StoreTarget::New {
                class,
                arity,
                index,
            } => {
                let id = self.class(file, class)?;
                self.constructor_slot(id, *arity, *index)
            }
            StoreTarget::BaseInit {
                class,
                arity,
                index,
            } => {
                let base = self.bases[self.class(file, class)?]?;
                self.constructor_slot(base, *arity, *index)
            }
            StoreTarget::Variant {
                enum_name: Some(enum_name),
                name,
                index,
            } => {
                let decl = self.enums[self.enum_names.lookup(file, enum_name)?];
                let variant = decl.variants.iter().find(|v| &v.name == name)?;
                self.slots
                    .get(&SlotKey::Payload(variant.span, *index))
                    .copied()
            }
            StoreTarget::Variant {
                enum_name: None,
                name,
                index,
            } => {
                let key = (name.clone(), *index);
                if let Some(node) = self.by_variant.get(&key) {
                    return Some(*node);
                }
                let node = self.graph.node();
                let slots = self
                    .payload_slots
                    .get(&(name.as_str(), *index))
                    .cloned()
                    .unwrap_or_default();
                for slot in slots {
                    self.graph.edge(node, slot);
                }
                self.by_variant.insert(key, node);
                Some(node)
            }
        }
    }

    /// `Err(())`: the value may be task-local outright; `Ok(Some(node))`: it
    /// has that slot's kind; `Ok(None)`: shareable.
    fn source(&mut self, file: FileId, value: &StoredValue) -> Result<Option<usize>, ()> {
        match value {
            StoredValue::TaskLocal => Err(()),
            StoredValue::Function(name) => match self.function_names.lookup(file, name) {
                Some(_) => Ok(None),
                None => Err(()),
            },
            StoredValue::Field { class, field } => match self.field_node(file, class, field) {
                FieldNode::Slot(node) => Ok(Some(node)),
                FieldNode::ByName => Ok(Some(self.by_name_source(field))),
                FieldNode::None => Ok(None),
            },
            StoredValue::Param(span) => Ok(self.slots.get(&SlotKey::Decl(*span)).copied()),
        }
    }

    fn resolve<'s>(
        mut self,
        stores: impl IntoIterator<Item = &'s RawStore>,
    ) -> (Decisions, SlotStats) {
        let mut work = Vec::new();
        for store in stores {
            self.graph.stats.stores += 1;
            let Some(target) = self.target(store.file, &store.target) else {
                continue;
            };
            match self.source(store.file, &store.value) {
                Err(()) => work.push(target),
                Ok(Some(source)) => self.graph.edge(source, target),
                Ok(None) => {}
            }
        }
        while let Some(node) = work.pop() {
            self.graph.stats.visits += 1;
            if self.graph.pinned[node] || self.graph.tainted[node] {
                continue;
            }
            self.graph.tainted[node] = true;
            work.extend(self.graph.edges[node].iter().copied());
        }
        let task_local = self
            .slots
            .iter()
            .filter(|(_, node)| self.graph.tainted[**node])
            .map(|(key, _)| *key)
            .collect();
        (
            Decisions {
                task_local,
                promised: std::mem::take(&mut self.promised_spans),
            },
            self.graph.stats,
        )
    }
}

enum FieldNode {
    Slot(usize),
    ByName,
    None,
}

fn set_top_kind(ty: &Type, shareable: bool) -> Type {
    let ty = ty.with_parsed_callable_kinds();
    match &ty {
        Type::Fn(params, ret) | Type::Closure(params, ret) if shareable => {
            Type::Fn(params.clone(), ret.clone())
        }
        Type::Fn(params, ret) | Type::Closure(params, ret) => {
            Type::Closure(params.clone(), ret.clone())
        }
        _ => ty,
    }
}

impl Decisions {
    fn apply(&self, program: &mut Program) {
        let shareable = |key| !self.task_local.contains(&key);
        for item in &mut program.items {
            match item {
                Item::Class(class) => {
                    let promised = self.promised.contains(&class.span);
                    let kind = |ty: &Type, span: Span| {
                        if promised {
                            ty.with_callable_kind(true)
                        } else {
                            set_top_kind(ty, shareable(SlotKey::Decl(span)))
                        }
                    };
                    for field in class.fields.iter_mut().filter(|f| !f.is_static) {
                        field.ty = kind(&field.ty, field.span);
                    }
                    for constructor in &mut class.constructors {
                        for param in &mut constructor.params {
                            param.ty = kind(&param.ty, param.span);
                        }
                    }
                }
                Item::Enum(decl) => {
                    for variant in &mut decl.variants {
                        let span = variant.span;
                        for (index, ty) in variant.payload.iter_mut().enumerate() {
                            *ty = set_top_kind(ty, shareable(SlotKey::Payload(span, index)));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Decide and apply the callable kinds of every declaration slot of a build.
/// `programs` are (namespace, program) pairs; `stores` are the recorded
/// stores of all their bodies.
pub(crate) fn apply_build<'s>(
    programs: &mut [(&str, &mut Program)],
    dependencies: Option<&crate::compiler_db::dependencies::ModuleDependencies>,
    stores: impl IntoIterator<Item = &'s RawStore>,
) -> SlotStats {
    let (decisions, stats) = {
        let shared: Vec<(&str, &Program)> = programs.iter().map(|(ns, p)| (*ns, &**p)).collect();
        Index::new(&shared, dependencies).resolve(stores)
    };
    for (_, program) in programs.iter_mut() {
        decisions.apply(program);
    }
    stats
}

/// Every store of `program`'s resident bodies.
pub(crate) fn program_stores(program: &Program) -> HashSet<RawStore> {
    let mut stores = HashSet::new();
    for_each_body(program, |body, context, params, type_params| {
        stores.extend(body.collect_stores(context, params, type_params));
    });
    stores
}

/// Single-file decisions, made when a file is parsed. A build re-decides
/// with [`apply_build`] once every file is known.
pub(crate) fn apply_program(program: &mut Program) -> SlotStats {
    let stores = program_stores(program);
    apply_build(&mut [("", program)], None, &stores)
}

#[cfg(test)]
#[path = "callable_slots_tests.rs"]
mod tests;
