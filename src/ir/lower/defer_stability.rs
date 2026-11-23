//! Find writes between a named defer registration and the exit of its scope.
use super::*;
use crate::ir::typed_ast::{
    ChildrenMut, Siblings, visit_expr_children_mut, visit_stmt_children_mut,
};
use crate::parser::ast::ParamMode;
use crate::semantic::ids::{FunctionId, TypeId};
use crate::semantic::scopes::Scopes;
use std::sync::Arc;

#[cfg(test)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct ScanCounts {
    pub work_items: usize,
    pub integer_operands: usize,
    pub resolved_sites: usize,
    pub reachability_checks: usize,
    pub watched_slots: usize,
    pub peak_pending: usize,
    pub borrowed_parameter_index: bool,
    pub alias_lookups: usize,
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static SCAN_COUNTS: std::cell::Cell<ScanCounts> = Default::default();
}

#[cfg(test)]
fn observe(f: impl FnOnce(&mut ScanCounts)) {
    let mut counts = SCAN_COUNTS.get();
    f(&mut counts);
    SCAN_COUNTS.set(counts);
}

#[derive(Default)]
pub(super) struct Callees {
    /// Share HIR spellings. Unrelated assignments need no entry.
    writes: HashMap<Arc<str>, usize>,
    sites: usize,
    /// The surrounding HIR is still built for diagnostics and recovery metadata.
    /// Its registrations cannot execute on this path.
    pub(super) skipped: bool,
}

impl Callees {
    pub(super) fn register(&mut self, body: &HirDeferBody) {
        if !self.skipped
            && let HirDeferBody::Expr(HirExpr {
                kind:
                    HirExprKind::Call {
                        callee,
                        binding: HirCallBinding::Local { may_change: true },
                        ..
                    },
                ..
            }) = body
        {
            self.writes.entry(callee.name()).or_default();
            self.sites += 1;
        }
    }

    pub(super) fn resolve(
        &mut self,
        body: &mut [HirStmt],
        parameters: Option<&Scopes<Binding>>,
        symbols: Option<&symbols::SymbolTable>,
    ) {
        if self.sites == 0 {
            return;
        }
        // Body scopes have been popped. Reuse the parameter frame only when
        // one of the watched slots is itself borrowed. Lambda parameters are values.
        let aliases = parameters.filter(|scope| {
            self.writes
                .keys()
                .any(|name| borrowed_callable(scope, name))
        });
        #[cfg(test)]
        SCAN_COUNTS.set(ScanCounts {
            watched_slots: self.writes.len(),
            borrowed_parameter_index: aliases.is_some(),
            ..Default::default()
        });
        let mut scan = Scan {
            callees: self,
            symbols,
            aliases,
            clock: 0,
            alias_write: 0,
            work: Vec::new(),
        };
        scan.work.push(Work::Enter(body));
        while let Some(work) = scan.work.pop() {
            #[cfg(test)]
            {
                observe(|n| {
                    n.work_items += 1;
                    n.peak_pending = n.peak_pending.max(scan.work.len() + 1);
                });
            }
            match work {
                Work::Enter(stmts) => {
                    let end = stmts
                        .iter()
                        .position(|s| {
                            #[cfg(test)]
                            observe(|n| {
                                n.reachability_checks += 1;
                            });
                            s.transfers_control()
                                || matches!(s, HirStmt::Expr(e) if e.ty == Type::Never)
                        })
                        .map_or(stmts.len(), |i| i + 1);
                    scan.work.push(Work::Scope(&mut stmts[..end], scan.clock));
                }
                Work::Scope(stmts, exit) => {
                    if let Some((stmt, before)) = stmts.split_last_mut() {
                        scan.work.push(Work::Scope(before, exit));
                        scan.stmt(stmt, exit);
                    }
                }
                Work::Siblings(children) => scan.siblings(children),
                Work::Arguments(args, signature) => scan.argument(args, signature),
                Work::Expr(kind) => scan.expression_kind(kind),
                Work::Register(callee, may_change, exit) => {
                    #[cfg(test)]
                    observe(|n| n.resolved_sites += 1);
                    let name = callee.name();
                    *may_change = scan.callees.writes[name.as_ref()] > exit
                        || (scan.alias_write > exit && scan.is_alias(&name));
                    // A later registered call runs before earlier ones during unwind.
                    scan.alias_effect();
                    scan.callees.sites -= 1;
                    if scan.callees.sites == 0 {
                        break;
                    }
                }
            }
        }
    }
}

pub(super) fn branch_reachable(condition: &HirExpr, selected: bool) -> bool {
    !matches!(condition.kind, HirExprKind::Bool(value) if value != selected)
}

pub(super) fn rhs_reachable(op: &BinOp, lhs: &HirExpr) -> bool {
    match op {
        BinOp::And => branch_reachable(lhs, true),
        BinOp::Or => branch_reachable(lhs, false),
        _ => true,
    }
}

fn borrowed_callable(parameters: &Scopes<Binding>, name: &str) -> bool {
    parameters.current(name).is_some_and(|p| {
        p.storage == BindingStorage::MayChange && matches!(p.ty, Type::Fn(..) | Type::Closure(..))
    })
}

// Resolve parameter modes at most once per call, on the first watched
// reference argument. Sibling cursors retain references into the HIR and declarations.
enum Signature<'a, 's> {
    Function(FunctionId),
    Constructor(TypeId),
    Method(Option<TypeId>, &'a str),
    Resolved(Option<&'s [symbols::ParamInfo]>),
}

impl<'s> Signature<'_, 's> {
    fn params(
        &mut self,
        symbols: Option<&'s symbols::SymbolTable>,
    ) -> Option<&'s [symbols::ParamInfo]> {
        let params = match self {
            Self::Resolved(params) => return *params,
            Self::Function(id) => {
                symbols.and_then(|s| Some(s.functions.get(id)?.param_infos.as_slice()))
            }
            Self::Constructor(id) => symbols.and_then(|s| {
                Some(
                    s.classes
                        .get(id)?
                        .constructor
                        .as_ref()?
                        .param_infos
                        .as_slice(),
                )
            }),
            Self::Method(owner, name) => symbols.and_then(|s| method_params(s, (*owner)?, name)),
        };
        *self = Self::Resolved(params);
        params
    }
}

enum Work<'a, 's> {
    Siblings(Siblings<'a>),
    Arguments(&'a mut [HirExpr], Signature<'a, 's>),
    Enter(&'a mut [HirStmt]),
    Scope(&'a mut [HirStmt], usize),
    Expr(&'a mut HirExprKind),
    Register(FunctionId, &'a mut bool, usize),
}

impl<'a> Work<'a, '_> {
    fn expr(expr: &'a mut HirExpr) -> Self {
        Self::Expr(&mut expr.kind)
    }
}

struct Scan<'a, 's> {
    callees: &'s mut Callees,
    symbols: Option<&'s symbols::SymbolTable>,
    aliases: Option<&'s Scopes<Binding>>,
    // Traverse backward. Each scope records the clock at its exit. Effects of
    // sibling scopes count for their parent, never for a registration inside
    // a different sibling. Source positions need not be unique or ordered.
    clock: usize,
    alias_write: usize,
    work: Vec<Work<'a, 's>>,
}

impl<'a, 's> Scan<'a, 's> {
    fn alias_effect(&mut self) {
        if self.aliases.is_some() {
            self.clock += 1;
            self.alias_write = self.clock;
        }
    }

    fn is_alias(&self, name: &str) -> bool {
        self.aliases.is_some_and(|parameters| {
            #[cfg(test)]
            observe(|n| {
                n.alias_lookups += 1;
            });
            borrowed_callable(parameters, name)
        })
    }

    fn write(&mut self, name: &str) {
        if let Some(last) = self.callees.writes.get_mut(name) {
            self.clock += 1;
            *last = self.clock;
        }
        if self.is_alias(name) {
            self.alias_effect();
        }
    }

    fn stmt(&mut self, stmt: &'a mut HirStmt, exit: usize) {
        match stmt {
            HirStmt::Defer {
                body: HirDeferBody::Expr(expr),
                ..
            } => match &mut expr.kind {
                HirExprKind::Call {
                    callee,
                    binding: HirCallBinding::Local { may_change },
                    args,
                } => {
                    if *may_change {
                        self.work.push(Work::Register(*callee, may_change, exit));
                    } else {
                        self.alias_effect();
                    }
                    self.siblings(Siblings::Exprs(args));
                }
                kind => self.work.push(Work::Expr(kind)),
            },
            HirStmt::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                if branch_reachable(cond, false)
                    && let Some(body) = else_branch
                {
                    self.work.push(Work::Enter(body));
                }
                if branch_reachable(cond, true) {
                    self.work.push(Work::Enter(then_branch));
                }
                self.work.push(Work::expr(cond));
            }
            HirStmt::While { cond, .. } if !branch_reachable(cond, true) => {
                // A literal condition has no effects and the body never executes.
            }
            stmt => {
                match &*stmt {
                    HirStmt::Assign { name, .. } => self.write(name),
                    HirStmt::Lock { .. } | HirStmt::SuperInit { .. } => self.alias_effect(),
                    HirStmt::FieldAssign { value, .. }
                    | HirStmt::IndexAssign { value, .. }
                    | HirStmt::StaticFieldAssign { value, .. }
                        if matches!(value.ty, Type::Fn(..) | Type::Closure(..)) =>
                    {
                        self.alias_effect()
                    }
                    _ => {}
                }
                visit_stmt_children_mut(stmt, self);
            }
        }
    }

    fn expression_kind(&mut self, kind: &'a mut HirExprKind) {
        use HirExprKind as E;
        #[cfg(test)]
        if matches!(kind, E::Int(_)) {
            observe(|n| n.integer_operands += 1);
        }
        match kind {
            E::Lambda { .. } => {} // The lifted callable has its own analysis.
            E::ReferenceArg { place } => {
                if let E::Var(name) = &place.kind {
                    self.write(name);
                }
                self.work.push(Work::expr(place));
            }
            E::Call {
                callee,
                binding,
                args,
            } => {
                self.alias_effect();
                let signature = if binding.is_local() {
                    Signature::Resolved(None)
                } else {
                    Signature::Function(*callee)
                };
                self.arguments(args, signature);
            }
            E::New { class, args } => {
                self.alias_effect();
                self.arguments(args, Signature::Constructor(*class));
            }
            E::MethodCall {
                object,
                method,
                super_class,
                args,
                ..
            } => {
                self.alias_effect();
                let owner = match (&object.ty, super_class.as_ref()) {
                    (_, Some(id)) | (Type::Named(id) | Type::Generic(id, _), None) => Some(*id),
                    _ => None,
                };
                self.arguments(args, Signature::Method(owner, method));
                self.work.push(Work::expr(object));
            }
            E::StaticCall {
                class,
                method,
                args,
            } => {
                self.alias_effect();
                self.arguments(args, Signature::Method(Some(*class), method));
            }
            E::Binary { op, lhs, rhs } => {
                if rhs_reachable(op, lhs) {
                    self.work.push(Work::expr(rhs));
                }
                self.work.push(Work::expr(lhs));
            }
            E::Ternary {
                condition,
                then_expr,
                else_expr,
            } => {
                if branch_reachable(condition, true) {
                    self.work.push(Work::expr(then_expr));
                }
                if branch_reachable(condition, false) {
                    self.work.push(Work::expr(else_expr));
                }
                self.work.push(Work::expr(condition));
            }
            kind => {
                match &*kind {
                    E::Await { .. } | E::Select { .. } | E::Invoke { .. } => self.alias_effect(),
                    E::Print { value, .. }
                        if !matches!(value.ty, Type::Bool | Type::F64 | Type::String)
                            && value.ty.integer_layout().is_none() =>
                    {
                        self.alias_effect()
                    }
                    _ => {}
                }
                visit_expr_children_mut(kind, self);
            }
        }
    }

    fn arguments(&mut self, args: &'a mut [HirExpr], signature: Signature<'a, 's>) {
        if !args.is_empty() {
            self.work.push(Work::Arguments(args, signature));
        }
    }

    fn argument(&mut self, args: &'a mut [HirExpr], mut signature: Signature<'a, 's>) {
        if let Some((arg, rest)) = args.split_last_mut() {
            let child = match &mut arg.kind {
                HirExprKind::ReferenceArg { place } => {
                    if let HirExprKind::Var(name) = &place.kind
                        && let Some(written) = self.callees.writes.get_mut(name.as_str())
                        && !signature
                            .params(self.symbols)
                            .and_then(|p| p.get(rest.len()))
                            .is_some_and(|p| {
                                matches!(p.mode, ParamMode::Reference { mutable: false, .. })
                            })
                    {
                        self.clock += 1;
                        *written = self.clock;
                    }
                    &mut place.kind
                }
                kind => kind,
            };
            self.arguments(rest, signature);
            self.expression_kind(child);
        }
    }

    fn sibling<T>(
        &mut self,
        items: &'a mut [T],
        group: impl FnOnce(&'a mut [T]) -> Siblings<'a>,
        visit: impl FnOnce(&mut Self, &'a mut T),
    ) {
        if let Some((item, rest)) = items.split_last_mut() {
            if !rest.is_empty() {
                self.work.push(Work::Siblings(group(rest)));
            }
            visit(self, item);
        }
    }
}

fn method_params<'s>(
    symbols: &'s symbols::SymbolTable,
    class: crate::semantic::ids::TypeId,
    method: &str,
) -> Option<&'s [symbols::ParamInfo]> {
    if let Some(interface) = symbols.interfaces.get(&class) {
        return Some(&interface.methods.get(method)?.param_infos);
    }
    let owner = symbols.resolved_method_class(&class.name(), method)?;
    Some(
        &symbols
            .lookup_class(&owner)?
            .methods
            .get(method)?
            .param_infos,
    )
}

impl<'a> ChildrenMut<'a> for Scan<'a, '_> {
    fn expr(&mut self, child: &'a mut HirExpr) {
        self.work.push(Work::expr(child));
    }
    fn siblings(&mut self, children: Siblings<'a>) {
        match children {
            Siblings::Exprs(items) => self.sibling(items, Siblings::Exprs, Self::expr),
            Siblings::Fields(items) => self.sibling(items, Siblings::Fields, |s, (_, e)| s.expr(e)),
            Siblings::Guards(items) => {
                self.sibling(items, Siblings::Guards, |s, g| s.expr(g.expr_mut()))
            }
            Siblings::Locks(items) => {
                self.sibling(items, Siblings::Locks, |s, l| s.expr(&mut l.target))
            }
            Siblings::Arms(items) => self.sibling(items, Siblings::Arms, Self::arm),
            Siblings::Cases(items) => self.sibling(items, Siblings::Cases, Self::case),
        }
    }
    fn block(&mut self, children: &'a mut [HirStmt]) {
        self.work.push(Work::Enter(children));
    }
}
