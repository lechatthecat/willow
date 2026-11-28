//! Bounded, flow-insensitive inclusion constraints for checked function values.
//! Bindings retain lexical identity; assignments, branches and loop backedges
//! union rather than kill. Heap fields merge by member name (sound under aliases
//! and inheritance). Unmodelled stores and module boundaries remain explicit.
mod bounds;
#[cfg(test)]
mod construction_tests;
mod signature;
mod solver;
use crate::diagnostics::Span;
use crate::parser::{
    ast::*,
    iter::{AstEvent, AstWalk},
};
use crate::semantic::{
    builtin_types::BuiltinTypeId,
    ids::{FunctionId, TypeId},
    indirect_lock::IndirectInputs,
};
pub(crate) use bounds::attach;
pub(crate) use signature::type_key;
use solver::*;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
struct Env {
    names: HashMap<String, Vec<usize>>,
    scopes: Vec<Vec<String>>,
}
impl Env {
    fn enter(&mut self) {
        self.scopes.push(Vec::new());
    }
    fn exit(&mut self) {
        for name in self.scopes.pop().unwrap() {
            self.names.get_mut(&name).unwrap().pop();
        }
    }
    fn bind(&mut self, name: &str, node: usize) {
        self.names.entry(name.into()).or_default().push(node);
        self.scopes.last_mut().unwrap().push(name.into());
    }
    fn get(&self, name: &str) -> Option<usize> {
        self.names.get(name)?.last().copied()
    }
}
struct Builder<'a> {
    solver: Solver,
    exprs: HashMap<ExprId, usize>,
    origins: HashMap<Span, Option<FunctionId>>,
    calls: HashMap<Span, usize>,
    fields: HashMap<String, usize>,
    memberwise: HashMap<Span, std::sync::Arc<[usize]>>,
    classes: HashSet<String>,
    open_methods: HashSet<FunctionId>,
    storage_escape: bool,
    forwarded: HashMap<Span, usize>,
    types: &'a HashMap<ExprId, Type>,
    resolved: &'a HashMap<ExprId, Option<FunctionId>>,
    super_calls: HashMap<Span, FunctionId>,
    escape: usize,
}
impl Builder<'_> {
    fn expr(&mut self, e: &Expr) -> usize {
        let node = *self
            .exprs
            .entry(e.id())
            .or_insert_with(|| self.solver.node());
        self.solver.nodes[node].callable |= matches!(
            self.types.get(&e.id()),
            Some(Type::Fn(..) | Type::Closure(..))
        );
        self.solver.nodes[node].storage |= self
            .types
            .get(&e.id())
            .is_some_and(|ty| self.storage_type(ty));
        if let Some(ty @ (Type::Array(_) | Type::FixedArray(..))) = self.types.get(&e.id()) {
            let slot = self.field(&format!("$elements:{ty:?}"));
            self.solver.edge(node, slot);
            self.solver.edge(slot, node);
        }
        node
    }
    fn storage_type(&self, ty: &Type) -> bool {
        let mut work = vec![ty];
        while let Some(ty) = work.pop() {
            match ty {
                Type::Named(name)
                    if self.classes.contains(name) || BuiltinTypeId::from_name(name).is_none() =>
                {
                    return true;
                }
                Type::Generic(name, args) => {
                    if self.classes.contains(name) || BuiltinTypeId::from_name(name).is_none() {
                        return true;
                    }
                    work.extend(args);
                }
                Type::Array(_) | Type::FixedArray(..) => return true,
                _ => {}
            }
        }
        false
    }
    fn field(&mut self, name: &str) -> usize {
        *self.fields.entry(name.into()).or_insert_with(|| {
            let node = self.solver.node();
            self.solver.storage_slot(node);
            node
        })
    }
    fn signature(&mut self, id: FunctionId, n: usize) -> Signature {
        if let Some(mut sig) = self.solver.signatures.get(&id).cloned() {
            while sig.params.len() < n {
                sig.params.push(self.solver.node());
                sig.references.push(false);
            }
            self.solver.register_signature(id, sig.clone());
            return sig;
        }
        let sig = Signature {
            params: (0..n).map(|_| self.solver.node()).collect(),
            references: vec![false; n],
            result: self.solver.node(),
        };
        self.solver.register_signature(id, sig.clone());
        sig
    }
    fn root(&mut self, id: FunctionId, params: &[Param], root: AstEvent<'_>, public: bool) {
        let sig = self.signature(id, params.len());
        if public {
            self.solver.edge(sig.result, self.escape);
        }
        let mut env = Env::default();
        env.enter();
        for (p, &node) in params.iter().zip(&sig.params) {
            env.bind(&p.name, node);
            if public {
                self.solver.unknown(node, EXTERNAL);
            }
            if public && self.storage_type(&p.ty) {
                self.storage_escape = true;
            }
        }
        let mut owners = vec![id];
        let mut bindings = HashMap::new();
        let mut walk = AstWalk::new(root);
        while let Some(event) = walk.next() {
            match event {
                AstEvent::EnterScope => env.enter(),
                AstEvent::ExitScope => env.exit(),
                AstEvent::Bind(name) => {
                    let node = self.solver.node();
                    if let Some(source) = bindings.remove(&(name.as_ptr() as usize)) {
                        self.solver.edge(source, node);
                    } else {
                        self.solver.unknown(node, STORAGE);
                    }
                    env.bind(name, node);
                }
                AstEvent::Stmt(Stmt::Let(s)) => {
                    let source = self.expr(&s.init);
                    bindings.insert(s.name.as_ptr() as usize, source);
                }
                AstEvent::Stmt(Stmt::For(s)) => {
                    let source = self.expr(&s.iterable);
                    bindings.insert(s.name.as_ptr() as usize, source);
                }
                AstEvent::Expr(e) => {
                    let node = self.expr(e);
                    if let Some(target) = self.origins.get(&e.span()).copied() {
                        if let Some(target) = target {
                            self.solver.target(node, target, 0);
                        } else {
                            self.solver.unknown(node, UNRESOLVED);
                        }
                    }
                    if let Expr::Var(name, ..) = e {
                        if let Some(source) = env.get(name) {
                            self.solver.edge(source, node);
                        } else if !self.origins.contains_key(&e.span()) {
                            self.solver.unknown(node, UNRESOLVED);
                        }
                    }
                }
                AstEvent::Lambda(l) => {
                    walk.skip_children();
                    let id = self
                        .origins
                        .get(&l.span)
                        .copied()
                        .flatten()
                        .unwrap_or_else(|| {
                            FunctionId::lambda(match &l.body {
                                LambdaBody::Block(b) => b.id,
                                LambdaBody::Expr(_) => BodyId::fresh(),
                            })
                        });
                    let sig = self.signature(id, l.params.len());
                    owners.push(id);
                    env.enter();
                    for (param, &node) in l.params.iter().zip(&sig.params) {
                        env.bind(&param.name, node);
                    }
                    match &l.body {
                        LambdaBody::Block(b) => walk.push(AstEvent::Block(b)),
                        LambdaBody::Expr(e) => walk.push(AstEvent::Expr(e)),
                    }
                }
                AstEvent::ExitExpr(e) => {
                    let out = self.expr(e);
                    let arguments = match e {
                        Expr::Call(c) => Some(&c.args),
                        Expr::MethodCall(c) => Some(&c.args),
                        Expr::StaticCall(c) => Some(&c.args),
                        Expr::New(c) => Some(&c.args),
                        _ => None,
                    };
                    if let Some(args) = arguments {
                        self.reference_writes(args, &env);
                    }
                    match e {
                        Expr::Lambda(l) => {
                            let id = owners.pop().unwrap();
                            if let LambdaBody::Expr(value) = &l.body {
                                let source = self.expr(value);
                                self.solver.edge(source, self.solver.signatures[&id].result);
                            }
                            env.exit();
                        }
                        Expr::Ternary(t) => {
                            for e in [&t.then_expr, &t.else_expr] {
                                let source = self.expr(e);
                                self.solver.edge(source, out);
                            }
                        }
                        Expr::ArrayLiteral(elements, ..) => {
                            for e in elements {
                                let source = self.expr(e);
                                self.solver.edge(source, out);
                            }
                        }
                        Expr::Index(array, ..) => {
                            let source = self.expr(array);
                            self.solver.edge(source, out);
                        }
                        Expr::FieldAccess(_, name, ..) => {
                            let source = self.field(name);
                            self.solver.edge(source, out);
                        }
                        Expr::StaticField(s) => {
                            let source = self.field(&s.field);
                            self.solver.edge(source, out);
                        }
                        Expr::ObjectLiteral(o) => {
                            for field in &o.fields {
                                let source = self.expr(&field.value);
                                let dest = self.field(&field.name);
                                self.solver.edge(source, dest);
                            }
                        }
                        Expr::Call(c) => {
                            if let Some(callee) = env.get(&c.callee) {
                                self.calls.insert(e.span(), callee);
                                for arg in &c.args {
                                    let node = self.expr(&arg.expr);
                                    self.calls.insert(arg.span, node);
                                    self.forwarded.insert(arg.span, callee);
                                }
                                let args = c.args.iter().map(|a| self.expr(&a.expr)).collect();
                                self.solver.call(callee, args, out);
                            } else {
                                self.invoke(e, &c.args, out);
                            }
                        }
                        Expr::MethodCall(c) => {
                            if !self.collection(c, out) {
                                self.invoke(e, &c.args, out);
                            }
                        }
                        Expr::StaticCall(c) => self.invoke(e, &c.args, out),
                        Expr::New(c) => self.invoke(e, &c.args, out),
                        Expr::Var(..)
                        | Expr::Integer(..)
                        | Expr::Char(..)
                        | Expr::Float(..)
                        | Expr::Bool(..)
                        | Expr::String(..) => {}
                        _ => self.solver.unknown(out, STORAGE),
                    }
                }
                AstEvent::ExitStmt(s) => self.statement(s, &env, *owners.last().unwrap()),
                _ => {}
            }
        }
    }
    fn statement(&mut self, statement: &Stmt, env: &Env, owner: FunctionId) {
        match statement {
            Stmt::SuperInit(s) => {
                self.reference_writes(&s.args, env);
                let out = self.solver.node();
                let target = self.super_calls.get(&s.span).copied();
                if !self.invoke_target(target, &s.args, out, s.span) {
                    // An external constructor can populate inherited storage.
                    self.storage_escape = true;
                    self.external_arguments(&s.args, out);
                }
            }
            Stmt::Assign(s) => {
                if let Some(dest) = env.get(&s.name) {
                    let from = self.expr(&s.value);
                    self.solver.edge(from, dest);
                }
            }
            Stmt::FieldAssign(s) => {
                let from = self.expr(&s.value);
                let to = self.field(&s.field);
                self.solver.edge(from, to);
            }
            Stmt::StaticFieldAssign(s) => {
                let from = self.expr(&s.value);
                let to = self.field(&s.field);
                self.solver.edge(from, to);
            }
            Stmt::IndexAssign(s) => {
                let from = self.expr(&s.value);
                let to = self.expr(&s.array);
                self.solver.edge(from, to); /* aliases */
                self.solver.edge(to, from);
            }
            Stmt::Return(s) => {
                if let Some(value) = &s.value {
                    let from = self.expr(value);
                    let to = self.solver.signatures[&owner].result;
                    self.solver.edge(from, to);
                }
            }
            _ => {}
        }
    }

    fn reference_writes(&mut self, args: &[CallArg], env: &Env) {
        for arg in args {
            if !matches!(arg.mode, CallArgMode::Reference { .. }) {
                continue;
            }
            let destination = match &arg.expr {
                Expr::Var(name, ..) => env.get(name),
                Expr::Index(array, ..) => Some(self.expr(array)),
                Expr::FieldAccess(_, name, ..) => Some(self.field(name)),
                Expr::StaticField(s) => Some(self.field(&s.field)),
                _ => None,
            };
            let value = self.expr(&arg.expr);
            self.solver.nodes[value].reference = true;
            if let Some(destination) = destination {
                self.solver.edge(value, destination);
            } else {
                self.solver.unknown(value, STORAGE);
                self.storage_escape = true;
            }
        }
    }

    /// Summaries for standard array storage operations. They store callbacks;
    /// they do not invoke or export those callbacks. Other receiver operations
    /// retain the external-storage fallback in `invoke`.
    fn collection(&mut self, call: &MethodCallExpr, out: usize) -> bool {
        if !matches!(self.types.get(&call.object.id()), Some(Type::Array(_))) {
            return false;
        }
        let stored = match call.method.as_str() {
            "push" | "fill" => Some(0),
            "insert" => Some(1),
            "pop" | "remove" => {
                let receiver = self.expr(&call.object);
                self.solver.edge(receiver, out);
                return true;
            }
            // These operations cannot introduce new element values.
            "len" | "swap" | "sort" | "index_of" | "contains" | "content_equals" | "join"
            | "to_string" => return true,
            _ => return false,
        };
        if let Some(value) = stored.and_then(|index| call.args.get(index)) {
            let source = self.expr(&value.expr);
            let receiver = self.expr(&call.object);
            self.solver.edge(source, receiver);
        }
        true
    }

    fn invoke_target(
        &mut self,
        target: Option<FunctionId>,
        args: &[CallArg],
        out: usize,
        span: Span,
    ) -> bool {
        if let Some(fields) = self.memberwise.get(&span).cloned() {
            for (&to, arg) in fields.iter().zip(args) {
                let source = self.expr(&arg.expr);
                self.solver.edge(source, to);
            }
        } else if let Some(target) = target.filter(|t| self.solver.signatures.contains_key(t)) {
            let nodes: Vec<_> = args.iter().map(|a| self.expr(&a.expr)).collect();
            self.solver.invoke(target, &nodes, out);
        } else {
            return false;
        }
        true
    }

    fn external_arguments(&mut self, args: &[CallArg], out: usize) {
        let nodes: Vec<_> = args.iter().map(|arg| self.expr(&arg.expr)).collect();
        self.solver.external_call(&nodes, out);
        // Builtins may invoke callbacks synchronously at these recorded sites.
        for (arg, node) in args.iter().zip(nodes) {
            self.calls.insert(arg.span, node);
        }
    }

    fn invoke(&mut self, e: &Expr, args: &[CallArg], out: usize) {
        let target = self.resolved.get(&e.id()).copied().flatten();
        if self.invoke_target(target, args, out, e.span()) {
            if matches!(e, Expr::MethodCall(_))
                && target.is_some_and(|t| self.open_methods.contains(&t))
            {
                self.solver.unknown(out, EXTERNAL);
                self.storage_escape = true;
            }
        } else {
            self.solver.unknown(out, EXTERNAL);
            self.storage_escape |= self
                .types
                .get(&e.id())
                .is_some_and(|ty| self.storage_type(ty));
            if let Expr::MethodCall(call) = e {
                self.storage_escape |= self
                    .types
                    .get(&call.object.id())
                    .is_some_and(|ty| self.storage_type(ty));
            }
            self.external_arguments(args, out);
        }
    }
}

pub(crate) fn resolve(
    program: &Program,
    types: &HashMap<ExprId, Type>,
    calls: &HashMap<ExprId, Option<FunctionId>>,
    inputs: &mut IndirectInputs,
) {
    if inputs.calls.is_empty() {
        return;
    }
    let mut b = Builder {
        solver: Solver::default(),
        exprs: HashMap::new(),
        origins: inputs.values.iter().map(|v| (v.span, v.target)).collect(),
        calls: HashMap::new(),
        fields: HashMap::new(),
        memberwise: HashMap::new(),
        classes: HashSet::new(),
        open_methods: HashSet::new(),
        storage_escape: false,
        forwarded: HashMap::new(),
        types,
        resolved: calls,
        super_calls: inputs.super_calls.iter().copied().collect(),
        escape: 0,
    };
    b.escape = b.solver.node();
    for (span, names) in &inputs.memberwise {
        let fields: std::sync::Arc<[usize]> = names.iter().map(|name| b.field(name)).collect();
        b.memberwise.insert(*span, fields);
    }
    let mut roots = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) if f.rust_bridge.is_none() => roots.push((
                FunctionId::free_from_source_name(&f.name),
                f.params.as_slice(),
                AstEvent::Block(&f.body),
                f.public(),
            )),
            Item::Class(c) => {
                b.classes.insert(c.name.clone());
                b.storage_escape |= c.public();
                for m in &c.methods {
                    if c.is_open() || m.is_open() {
                        b.open_methods
                            .insert(FunctionId::method(TypeId::local(&c.name), &m.name));
                    }
                    roots.push((
                        FunctionId::method(TypeId::local(&c.name), &m.name),
                        m.params.as_slice(),
                        AstEvent::Block(&m.body),
                        m.public() || c.is_open() || m.is_open(),
                    ));
                }
                for m in &c.constructors {
                    roots.push((
                        FunctionId::method(TypeId::local(&c.name), "init"),
                        m.params.as_slice(),
                        AstEvent::Block(&m.body),
                        false,
                    ));
                }
            }
            _ => {}
        }
    }
    for &(id, params, _, _) in &roots {
        b.signature(id, params.len());
        for (p, reference) in params
            .iter()
            .zip(&mut b.solver.signatures.get_mut(&id).unwrap().references)
        {
            *reference |= matches!(p.mode, ParamMode::Reference { .. });
        }
    }
    for item in &program.items {
        if let Item::Function(f) = item
            && f.public()
            && b.storage_type(&f.return_type)
        {
            b.storage_escape = true;
        }
    }
    for (id, params, root, public) in roots {
        b.root(id, params, root, public);
    }
    for item in &program.items {
        if let Item::Class(c) = item {
            for field in &c.fields {
                if let Some(init) = &field.initializer {
                    let id = FunctionId::method(
                        TypeId::local(&c.name),
                        format!("$flow-field:{}", field.name),
                    );
                    b.root(id, &[], AstEvent::Expr(init), false);
                    let source = b.expr(init);
                    let target = b.field(&field.name);
                    b.solver.edge(source, target);
                }
            }
        }
    }
    if b.storage_escape {
        b.solver.invalidate_storage();
    }
    b.solver.escape(b.escape);
    b.solver.solve();
    for call in &mut inputs.calls {
        if let Some(&callee) = b.forwarded.get(&call.span) {
            let node = &b.solver.nodes[callee];
            if node.unknown == 0
                && !node.targets.is_empty()
                && node
                    .targets
                    .keys()
                    .all(|t| b.solver.signatures.contains_key(t))
            {
                call.targets = Some(Vec::new());
                call.unknown_reasons.clear();
                continue;
            }
        }
        let node = b.calls.get(&call.span).and_then(|&n| b.solver.nodes.get(n));
        let reasons = node.map_or(UNRESOLVED, |n| {
            n.unknown | if n.targets.is_empty() { UNRESOLVED } else { 0 }
        });
        call.unknown_reasons = solver::reasons(reasons);
        call.targets = if reasons == 0 {
            let mut targets: Vec<_> = node.unwrap().targets.keys().copied().collect();
            targets.sort();
            Some(targets)
        } else {
            None
        };
    }
}
