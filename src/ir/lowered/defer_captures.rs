//! Lexical inputs of cleanup regions. HIR has already disambiguated shadowed
//! bindings. Each nested cleanup passes its free names to its immediate parent.
use super::*;
use crate::ir::typed_ast::{HirNodeRef, HirSelectCaseKind};
use indexmap::IndexSet;
use std::collections::HashSet;
use std::rc::Rc;

pub(super) type NestedAccess = Option<Rc<HashMap<HirDeferId, DeferAccess>>>;

#[cfg(test)]
std::thread_local! {
    pub(super) static VISITED_NODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Default)]
pub(super) struct DeferAccess {
    pub names: Vec<Rc<str>>,
    pub recovers: bool,
}

#[derive(Default)]
struct Scope<'a> {
    reads: HashSet<&'a str>,
    callees: HashSet<FunctionId>,
    bindings: HashSet<&'a str>,
    forwarded: HashSet<usize>,
    recovers: bool,
}

impl<'a> Scope<'a> {
    fn pattern(&mut self, pattern: &'a HirPattern) {
        match pattern {
            HirPattern::Binding { name, .. } | HirPattern::ClassDowncast { binding: name, .. } => {
                self.bindings.insert(name);
            }
            HirPattern::EnumVariantTuple { bindings, .. } => {
                self.bindings
                    .extend(bindings.iter().map(|(name, _)| name.as_str()));
            }
            HirPattern::Wildcard
            | HirPattern::LiteralBool(_)
            | HirPattern::LiteralInt(_)
            | HirPattern::EnumVariant { .. } => {}
        }
    }

    fn finish(
        mut self,
        spellings: &mut IndexSet<Rc<str>>,
        parent: Option<&mut Self>,
    ) -> DeferAccess {
        for binding in &self.bindings {
            if let Some(id) = spellings.get_index_of(*binding) {
                self.forwarded.remove(&id);
            }
        }
        let mut read = |name: &str| {
            if !self.bindings.contains(name) {
                let id = spellings
                    .get_index_of(name)
                    .unwrap_or_else(|| spellings.insert_full(Rc::from(name)).0);
                self.forwarded.insert(id);
            }
        };
        for name in self.reads {
            read(name);
        }
        for callee in self.callees {
            read(&callee.name());
        }
        if let Some(parent) = parent {
            parent.recovers |= self.recovers;
            parent.forwarded.extend(&self.forwarded);
        }
        DeferAccess {
            names: self
                .forwarded
                .into_iter()
                .map(|id| Rc::clone(&spellings[id]))
                .collect(),
            recovers: self.recovers,
        }
    }
}

pub(super) fn analyze(
    body: &HirDeferBody,
    nested: &mut HashMap<HirDeferId, DeferAccess>,
) -> DeferAccess {
    enum Work<'a> {
        Visit(HirNodeRef<'a>),
        Finish(HirDeferId),
    }
    let mut pending = match body {
        HirDeferBody::Expr(expr) => vec![Work::Visit(HirNodeRef::Expr(expr))],
        HirDeferBody::Block(stmts) => stmts
            .iter()
            .map(|s| Work::Visit(HirNodeRef::Stmt(s)))
            .collect(),
    };
    // Intern only free names, within this cleanup tree. Forwarding across a
    // lexical boundary hashes an index; the retained summaries share the text.
    let mut spellings = IndexSet::new();
    let mut parents = Vec::new();
    let mut scope = Scope::default();
    while let Some(work) = pending.pop() {
        let node = match work {
            Work::Visit(node) => node,
            Work::Finish(id) => {
                let child =
                    std::mem::replace(&mut scope, parents.pop().expect("nested defer scope"));
                nested.insert(id, child.finish(&mut spellings, Some(&mut scope)));
                continue;
            }
        };
        #[cfg(test)]
        VISITED_NODES.set(VISITED_NODES.get() + 1);
        match &node {
            HirNodeRef::Stmt(stmt) => match stmt {
                HirStmt::Defer { id, .. } => {
                    parents.push(std::mem::take(&mut scope));
                    pending.push(Work::Finish(*id));
                }
                HirStmt::Let { name, .. } | HirStmt::For { name, .. } => {
                    scope.bindings.insert(name);
                }
                HirStmt::Lock {
                    binding,
                    additional,
                    ..
                } => {
                    scope.bindings.insert(binding);
                    scope
                        .bindings
                        .extend(additional.iter().map(|t| t.binding.as_str()));
                }
                HirStmt::Assign { name, .. } => {
                    scope.reads.insert(name);
                }
                HirStmt::SuperInit { .. } => {
                    scope.reads.insert("self");
                }
                HirStmt::If {
                    cond,
                    then_branch,
                    else_branch,
                    ..
                } => {
                    if let Some(branches) =
                        constant_if_branches(cond, then_branch, else_branch.as_deref())
                    {
                        // Recovery metadata is syntactic, even for an arm the CFG omits.
                        scope.recovers = scope.recovers || block_has_recover(branches.skipped);
                        if let Some(taken) = branches.taken {
                            pending.extend(
                                taken.iter().map(|stmt| Work::Visit(HirNodeRef::Stmt(stmt))),
                            );
                        }
                        continue;
                    }
                }
                HirStmt::While { cond, body, .. } => {
                    if !while_body_is_reachable(cond) {
                        scope.recovers = scope.recovers || block_has_recover(body);
                        continue;
                    }
                }
                HirStmt::Return { .. }
                | HirStmt::Break { .. }
                | HirStmt::Continue { .. }
                | HirStmt::Expr(_)
                | HirStmt::FieldAssign { .. }
                | HirStmt::IndexAssign { .. }
                | HirStmt::StaticFieldAssign { .. } => {}
            },
            HirNodeRef::Expr(expr) => match &expr.kind {
                HirExprKind::Ternary { .. } | HirExprKind::Binary { .. } => {
                    if let Some(branches) = ConditionalOperands::from_kind(&expr.kind)
                        && let Some(truth) = branches.literal_condition()
                    {
                        if let Some(skipped) = branches.operand(!truth) {
                            scope.recovers = scope.recovers || expr_has_recover(skipped);
                        }
                        if let Some(taken) = branches.operand(truth) {
                            pending.push(Work::Visit(HirNodeRef::Expr(taken)));
                        }
                        continue;
                    }
                }
                HirExprKind::Var(name) => {
                    scope.reads.insert(name);
                }
                HirExprKind::Call {
                    callee, binding, ..
                } => {
                    if binding.is_local() {
                        scope.callees.insert(*callee);
                    } else if callee.is_free_named("recover") {
                        scope.recovers = true;
                    }
                }
                HirExprKind::Lambda { captures, .. } => {
                    for capture in captures {
                        scope.reads.insert(&capture.source);
                    }
                    continue; // The lifted callable owns the body.
                }
                HirExprKind::Match { arms, .. } => {
                    for arm in arms {
                        scope.pattern(&arm.pattern);
                        for guard in &arm.guard {
                            if let HirGuard::Let { pattern, .. } = guard {
                                scope.pattern(pattern);
                            }
                        }
                    }
                }
                HirExprKind::Select { cases } => {
                    for case in cases {
                        if let HirSelectCaseKind::Recv { binding, .. }
                        | HirSelectCaseKind::Join { binding, .. } = &case.kind
                        {
                            scope.bindings.insert(binding);
                        }
                    }
                }
                ir_variants!(HirExprKind; Int, Float, Bool, Str, FnRef, Unary, Invoke, Print, Array, Index, New, FieldAccess, MethodCall, ObjectLiteral, StaticField, StaticCall, ReferenceArg, Range, Await, TryPropagate) =>
                    {}
            },
        }
        pending.extend(node.children().into_iter().map(Work::Visit));
    }
    scope.finish(&mut spellings, None)
}
