//! Conservative recoverable-panic effect analysis (willow-s9ej.8).
//!
//! The analysis proves a deliberately small `NO_PANIC` subset. Unknown calls,
//! function values, interface dispatch, operations with language-level guards,
//! and unclassified builtins all remain `MAY_PANIC`. Direct-call cycles start
//! optimistic and are invalidated from intrinsic/unknown roots, which lets a
//! pure recursive SCC be proved safe without ever making an unknown edge safe.

use std::collections::{HashMap, HashSet};

use crate::parser::ast::*;
use crate::semantic::ids::FunctionMap;

use super::symbols::{
    class_member_symbol, class_method_symbol_name, module_item_symbol, module_symbol_prefix,
};
use super::type_helpers::builtin_call_runtime_name;
use super::{Codegen, FuncGen};

#[derive(Debug, Default)]
struct EffectNode {
    direct_panic: bool,
    callees: HashSet<String>,
}

struct Candidate<'a> {
    key: String,
    params: &'a [Param],
    body: &'a Block,
    current_class: Option<&'a str>,
}

/// Naming information for one compilation unit. Imported modules have a
/// canonical linker prefix; the entry unit does not.
#[derive(Clone, Copy)]
pub(super) struct UnitNaming<'a> {
    pub module_prefix: Option<&'a str>,
}

/// Analyze every source callable in `program` and return backend-symbol keyed
/// `may_panic` facts. `known` contains already-analyzed imported modules and
/// aliases. Missing facts are never interpreted as safe.
pub(super) fn analyze_program(
    program: &Program,
    naming: UnitNaming<'_>,
    known: &FunctionMap<bool>,
    known_modules: &HashMap<String, String>,
    lambdas: &[(String, LambdaExpr)],
) -> HashMap<String, bool> {
    let mut free_keys = HashMap::new();
    let mut method_keys = HashMap::new();
    let mut class_bases = HashMap::new();
    let mut candidates = Vec::new();

    for item in &program.items {
        match item {
            Item::Function(function) => {
                let key = free_key(&function.name, naming);
                free_keys.insert(function.name.clone(), key.clone());
                candidates.push(Candidate {
                    key,
                    params: &function.params,
                    body: &function.body,
                    current_class: None,
                });
            }
            Item::Class(class) => {
                class_bases.insert(
                    class.name.clone(),
                    class
                        .base_class
                        .as_ref()
                        .map(|base| base.name().to_string()),
                );
                for method in &class.methods {
                    let key = method_key(&class.name, &method.name, naming, known_modules);
                    method_keys.insert((class.name.clone(), method.name.clone()), key.clone());
                    candidates.push(Candidate {
                        key,
                        params: &method.params,
                        body: &method.body,
                        current_class: Some(&class.name),
                    });
                }
                for constructor in &class.constructors {
                    let key = method_key(&class.name, "init", naming, known_modules);
                    method_keys.insert((class.name.clone(), "init".to_string()), key.clone());
                    candidates.push(Candidate {
                        key,
                        params: &constructor.params,
                        body: &constructor.body,
                        current_class: Some(&class.name),
                    });
                }
            }
            Item::Enum(_) | Item::Interface(_) => {}
        }
    }

    // Lambda calls remain indirect and therefore conservative at their call
    // sites, but recording their own fact keeps the callable inventory total.
    for (name, lambda) in lambdas {
        if let LambdaBody::Block(body) = &lambda.body {
            candidates.push(Candidate {
                key: name.clone(),
                params: &[],
                body,
                current_class: None,
            });
        }
    }

    let candidate_keys: HashSet<String> = candidates.iter().map(|c| c.key.clone()).collect();
    let context = AnalysisContext {
        known,
        known_modules,
        free_keys: &free_keys,
        method_keys: &method_keys,
        class_bases: &class_bases,
        candidate_keys: &candidate_keys,
    };

    let mut nodes = HashMap::new();
    for candidate in candidates {
        let mut locals = candidate
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect::<HashSet<_>>();
        collect_local_names(candidate.body, &mut locals);
        let mut visitor = EffectVisitor {
            context: &context,
            current_class: candidate.current_class,
            locals,
            local_types: candidate
                .params
                .iter()
                .map(|param| (param.name.clone(), param.ty.clone()))
                .collect(),
            node: EffectNode::default(),
        };
        visitor.visit_block(candidate.body);
        nodes
            .entry(candidate.key)
            .and_modify(|node: &mut EffectNode| {
                // Multiple constructors currently share one backend `init`
                // symbol. Union their effects rather than allowing the last
                // declaration to erase an earlier hazard.
                node.direct_panic |= visitor.node.direct_panic;
                node.callees.extend(visitor.node.callees.iter().cloned());
            })
            .or_insert(visitor.node);
    }

    // Seed intrinsic/unknown hazards, then propagate through direct edges.
    // A pure recursive SCC has no seed and therefore remains proven safe.
    let mut may_panic: HashSet<String> = nodes
        .iter()
        .filter(|(_, node)| node.direct_panic)
        .map(|(key, _)| key.clone())
        .collect();
    loop {
        let newly_panicking = nodes
            .iter()
            .filter(|(key, node)| {
                !may_panic.contains(*key)
                    && node.callees.iter().any(|callee| may_panic.contains(callee))
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        if newly_panicking.is_empty() {
            break;
        }
        may_panic.extend(newly_panicking);
    }

    nodes
        .into_keys()
        .map(|key| {
            let effect = may_panic.contains(&key);
            (key, effect)
        })
        .collect()
}

fn optimization_enabled() -> bool {
    std::env::var("WILLOW_PANIC_EFFECTS")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

impl Codegen {
    pub(super) fn analyze_and_register_panic_effects(
        &mut self,
        program: &Program,
        naming: UnitNaming<'_>,
        lambdas: &[(String, LambdaExpr)],
    ) {
        let effects = analyze_program(
            program,
            naming,
            &self.function_may_panic,
            &self.known_modules,
            lambdas,
        );
        let mut ordered = effects.into_iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.0.cmp(&right.0));
        let log = std::env::var_os("WILLOW_PANIC_EFFECTS_LOG").is_some();
        for (name, may_panic) in ordered {
            if log {
                eprintln!(
                    "[panic-effects] {name}: {}",
                    if may_panic { "may-panic" } else { "no-panic" }
                );
            }
            self.function_may_panic.insert(name, may_panic);
        }
    }

    pub(super) fn user_function_may_panic(&self, name: &str) -> bool {
        !optimization_enabled() || self.function_may_panic.get(name).copied().unwrap_or(true)
    }
}

impl FuncGen<'_, '_> {
    pub(super) fn user_function_may_panic(&self, name: &str) -> bool {
        !optimization_enabled() || self.function_may_panic.get(name).copied().unwrap_or(true)
    }

    /// Snapshot panic depth only for a direct call whose callee was not proven
    /// safe. The ordinary post-call helper accepts `None`, so cleanup/root
    /// balancing remains one shared path for optimized and conservative calls.
    pub(super) fn emit_pre_user_call_panic_depth(
        &mut self,
        callee: &str,
    ) -> Option<cranelift_codegen::ir::Value> {
        self.user_function_may_panic(callee)
            .then(|| self.emit_pre_willow_call_panic_depth())
            .flatten()
    }

    /// Dynamic class dispatch may omit its shared check only when every
    /// possible concrete target has an explicit `NO_PANIC` summary.
    pub(super) fn emit_pre_user_dispatch_panic_depth<'a>(
        &mut self,
        callees: impl IntoIterator<Item = &'a str>,
    ) -> Option<cranelift_codegen::ir::Value> {
        callees
            .into_iter()
            .any(|callee| self.user_function_may_panic(callee))
            .then(|| self.emit_pre_willow_call_panic_depth())
            .flatten()
    }
}

fn free_key(name: &str, naming: UnitNaming<'_>) -> String {
    naming
        .module_prefix
        .map(|prefix| module_item_symbol(prefix, name))
        .unwrap_or_else(|| name.to_string())
}

fn method_key(
    class: &str,
    method: &str,
    naming: UnitNaming<'_>,
    known_modules: &HashMap<String, String>,
) -> String {
    if let Some(prefix) = naming.module_prefix {
        return class_member_symbol(
            &module_item_symbol(prefix, &module_symbol_prefix(class)),
            method,
        );
    }
    class_method_symbol_name(known_modules, class, method)
}

struct AnalysisContext<'a> {
    known: &'a FunctionMap<bool>,
    known_modules: &'a HashMap<String, String>,
    free_keys: &'a HashMap<String, String>,
    method_keys: &'a HashMap<(String, String), String>,
    class_bases: &'a HashMap<String, Option<String>>,
    candidate_keys: &'a HashSet<String>,
}

struct EffectVisitor<'a> {
    context: &'a AnalysisContext<'a>,
    current_class: Option<&'a str>,
    locals: HashSet<String>,
    local_types: HashMap<String, Type>,
    node: EffectNode,
}

impl EffectVisitor<'_> {
    fn mark_direct(&mut self) {
        self.node.direct_panic = true;
    }

    fn record_callee(&mut self, key: String) {
        if self.context.candidate_keys.contains(&key) {
            self.node.callees.insert(key);
            return;
        }
        match self.context.known.get(&key).copied() {
            Some(false) => {}
            Some(true) | None => self.mark_direct(),
        }
    }

    fn visit_block(&mut self, block: &Block) {
        for statement in &block.stmts {
            self.visit_stmt(statement);
        }
    }

    fn visit_stmt(&mut self, statement: &Stmt) {
        match statement {
            Stmt::Let(stmt) => {
                self.visit_expr(&stmt.init);
                if let Some(ty) = stmt
                    .ty
                    .clone()
                    .or_else(|| self.infer_class_value_type(&stmt.init))
                {
                    self.local_types.insert(stmt.name.clone(), ty);
                }
            }
            Stmt::Assign(stmt) => self.visit_expr(&stmt.value),
            Stmt::FieldAssign(stmt) => {
                self.visit_expr(&stmt.object);
                self.visit_expr(&stmt.value);
            }
            Stmt::SuperInit(stmt) => {
                for arg in &stmt.args {
                    self.visit_expr(&arg.expr);
                }
                // Resolving the base constructor requires class hierarchy
                // metadata. Keep this uncommon edge conservative.
                self.mark_direct();
            }
            Stmt::StaticFieldAssign(stmt) => self.visit_expr(&stmt.value),
            Stmt::IndexAssign(stmt) => {
                self.visit_expr(&stmt.array);
                self.visit_expr(&stmt.index);
                self.visit_expr(&stmt.value);
                self.mark_direct(); // bounds guard
            }
            Stmt::If(stmt) => {
                self.visit_expr(&stmt.cond);
                self.visit_block(&stmt.then_block);
                if let Some(block) = &stmt.else_block {
                    self.visit_block(block);
                }
            }
            Stmt::While(stmt) => {
                self.visit_expr(&stmt.cond);
                self.visit_block(&stmt.body);
            }
            Stmt::Break(_) | Stmt::Continue(_) => {}
            Stmt::Defer(stmt) => match &stmt.body {
                DeferBody::Expr(expr) => self.visit_expr(expr),
                DeferBody::Block(block) => self.visit_block(block),
            },
            Stmt::Lock(stmt) => {
                self.visit_expr(&stmt.target);
                self.visit_block(&stmt.body);
                // Recursive acquisition and a lost ownership token are
                // recoverable language faults.
                self.mark_direct();
            }
            Stmt::For(stmt) => {
                self.visit_expr(&stmt.iterable);
                self.visit_block(&stmt.body);
            }
            Stmt::Return(stmt) => {
                if let Some(value) = &stmt.value {
                    self.visit_expr(value);
                }
            }
            Stmt::Expr(stmt) => self.visit_expr(&stmt.expr),
        }
    }

    fn visit_expr(&mut self, expression: &Expr) {
        match expression {
            Expr::Integer(..)
            | Expr::Float(..)
            | Expr::Bool(..)
            | Expr::String(..)
            | Expr::Var(..)
            | Expr::StaticField(_) => {}
            Expr::Binary(expr) => {
                self.visit_expr(&expr.lhs);
                self.visit_expr(&expr.rhs);
                if matches!(expr.op, BinOp::Div | BinOp::Rem | BinOp::Pow) {
                    self.mark_direct();
                }
            }
            Expr::Unary(expr) => self.visit_expr(&expr.expr),
            Expr::Call(call) => {
                for arg in &call.args {
                    self.visit_expr(&arg.expr);
                }
                match call.callee.as_str() {
                    "panic" | "format" => self.mark_direct(),
                    "recover" | "pow" | "powf" => {}
                    callee => {
                        if let Some(runtime_name) = builtin_call_runtime_name(callee) {
                            let may_panic = crate::backend::abi::runtime_symbol(runtime_name)
                                .is_none_or(|symbol| {
                                    symbol
                                        .effects()
                                        .contains(crate::backend::abi::RuntimeEffects::MAY_PANIC)
                                });
                            if may_panic {
                                self.mark_direct();
                            }
                        } else if self.locals.contains(callee) {
                            // Function-value call. The runtime target is not
                            // statically known even when its current value is a
                            // source lambda proven safe.
                            self.mark_direct();
                        } else if let Some(key) = self.context.free_keys.get(callee) {
                            self.record_callee(key.clone());
                        } else if self.context.known.get(callee).is_some() {
                            self.record_callee(callee.to_string());
                        } else {
                            // Enum constructors and future compiler builtins
                            // can reach here. Keeping them conservative is the
                            // fail-closed choice.
                            self.mark_direct();
                        }
                    }
                }
            }
            Expr::FieldAccess(object, ..) => {
                self.visit_expr(object);
                self.mark_direct();
            }
            Expr::MethodCall(call) => {
                self.visit_expr(&call.object);
                for arg in &call.args {
                    self.visit_expr(&arg.expr);
                }
                // `self.method()` is virtual just like a call through any other
                // class-typed receiver.  Recording only the current class's
                // implementation would miss a panicking override selected when
                // this inherited method runs on a subclass instance
                // (willow-s9ej.11).
                let declared_class = if matches!(&call.object, Expr::Var(name, _) if name == "self")
                {
                    self.current_class.map(str::to_owned)
                } else {
                    self.infer_class_name(&call.object)
                };
                if let Some(class) = declared_class {
                    let targets = self.dispatch_targets(&class, &call.method);
                    if !targets.is_empty() {
                        for target in targets {
                            self.record_callee(target);
                        }
                        return;
                    }
                }
                // General method syntax includes interface dispatch (whose raw
                // two-word box is checked) and collection helpers with runtime
                // guards, so an unresolved call remains conservative.
                self.mark_direct();
            }
            Expr::StaticCall(call) => {
                for arg in &call.args {
                    self.visit_expr(&arg.expr);
                }
                if call.class == "Self"
                    && let Some(class) = self.current_class
                    && let Some(key) = self
                        .context
                        .method_keys
                        .get(&(class.to_string(), call.method.clone()))
                {
                    self.record_callee(key.clone());
                    return;
                }
                if let Some(prefix) = self.context.known_modules.get(&call.class) {
                    self.record_callee(module_item_symbol(prefix, &call.method));
                    return;
                }
                if let Some(key) = self
                    .context
                    .method_keys
                    .get(&(call.class.clone(), call.method.clone()))
                {
                    self.record_callee(key.clone());
                    return;
                }
                let key =
                    class_method_symbol_name(self.context.known_modules, &call.class, &call.method);
                self.record_callee(key);
            }
            Expr::New(new) => {
                for arg in &new.args {
                    self.visit_expr(&arg.expr);
                }
                if let Some(key) = self
                    .context
                    .method_keys
                    .get(&(new.class_name.clone(), "init".to_string()))
                {
                    self.record_callee(key.clone());
                } else {
                    let key = class_method_symbol_name(
                        self.context.known_modules,
                        &new.class_name,
                        "init",
                    );
                    if self.context.known.get(&key).is_some() {
                        self.record_callee(key);
                    }
                    // No known `init` means an implicit memberwise
                    // constructor, which only allocates and stores fields.
                }
            }
            Expr::ObjectLiteral(object) => {
                for field in &object.fields {
                    self.visit_expr(&field.value);
                }
            }
            Expr::Await(awaited) => {
                self.visit_expr(&awaited.expr);
                // Strict await can turn cancellation into a language panic.
                // TaskResult awaits are intentionally not distinguished here:
                // retaining a check is conservative.
                self.mark_direct();
            }
            Expr::Select(select) => {
                for case in &select.cases {
                    match &case.kind {
                        SelectCaseKind::Recv { channel, .. } => self.visit_expr(channel),
                        SelectCaseKind::Send { channel, value } => {
                            self.visit_expr(channel);
                            self.visit_expr(value);
                        }
                        SelectCaseKind::Timeout { millis } => self.visit_expr(millis),
                        SelectCaseKind::Join { task, .. } => self.visit_expr(task),
                        SelectCaseKind::Default => {}
                    }
                    self.visit_block(&case.body);
                }
                self.mark_direct();
            }
            Expr::Print(value, ..) => {
                self.visit_expr(value);
                // Object/interface display may invoke user `toString` code.
                self.mark_direct();
            }
            Expr::Ternary(expr) => {
                self.visit_expr(&expr.condition);
                self.visit_expr(&expr.then_expr);
                self.visit_expr(&expr.else_expr);
            }
            Expr::Range(expr) => {
                self.visit_expr(&expr.start);
                self.visit_expr(&expr.end);
            }
            Expr::Lambda(_) => {}
            Expr::Match(expr) => {
                self.visit_expr(&expr.scrutinee);
                for arm in &expr.arms {
                    match &arm.body {
                        MatchBody::Expr(body) => self.visit_expr(body),
                        MatchBody::Block(body) => self.visit_block(body),
                    }
                }
            }
            Expr::TryPropagate(inner, _) => self.visit_expr(inner),
            Expr::ArrayLiteral(elements, _) => {
                for element in elements {
                    self.visit_expr(element);
                }
            }
            Expr::Index(array, index, _) => {
                self.visit_expr(array);
                self.visit_expr(index);
                self.mark_direct();
            }
        }
    }

    fn infer_class_value_type(&self, expression: &Expr) -> Option<Type> {
        match expression {
            Expr::New(new) => Some(Type::Named(new.class_name.clone())),
            Expr::ObjectLiteral(object) => Some(Type::Named(object.class.clone())),
            Expr::Var(name, _) => self.local_types.get(name).cloned(),
            _ => None,
        }
    }

    fn infer_class_name(&self, expression: &Expr) -> Option<String> {
        let ty = self.infer_class_value_type(expression)?;
        match ty {
            Type::Named(name) if self.context.class_bases.contains_key(&name) => Some(name),
            _ => None,
        }
    }

    fn dispatch_targets(&self, declared_class: &str, method: &str) -> Vec<String> {
        let mut targets = HashSet::new();
        for concrete in self.context.class_bases.keys() {
            if !self.is_same_or_subclass(concrete, declared_class) {
                continue;
            }
            let mut search = Some(concrete.as_str());
            let mut seen = HashSet::new();
            while let Some(class) = search {
                if !seen.insert(class.to_string()) {
                    break;
                }
                if let Some(target) = self
                    .context
                    .method_keys
                    .get(&(class.to_string(), method.to_string()))
                {
                    targets.insert(target.clone());
                    break;
                }
                search = self
                    .context
                    .class_bases
                    .get(class)
                    .and_then(|base| base.as_deref());
            }
        }
        let mut targets = targets.into_iter().collect::<Vec<_>>();
        targets.sort();
        targets
    }

    fn is_same_or_subclass(&self, class: &str, base: &str) -> bool {
        let mut current = Some(class);
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if name == base {
                return true;
            }
            if !seen.insert(name.to_string()) {
                return false;
            }
            current = self
                .context
                .class_bases
                .get(name)
                .and_then(|parent| parent.as_deref());
        }
        false
    }
}

fn collect_local_names(block: &Block, names: &mut HashSet<String>) {
    for statement in &block.stmts {
        match statement {
            Stmt::Let(stmt) => {
                names.insert(stmt.name.clone());
                collect_expr_local_names(&stmt.init, names);
            }
            Stmt::If(stmt) => {
                collect_expr_local_names(&stmt.cond, names);
                collect_local_names(&stmt.then_block, names);
                if let Some(block) = &stmt.else_block {
                    collect_local_names(block, names);
                }
            }
            Stmt::While(stmt) => {
                collect_expr_local_names(&stmt.cond, names);
                collect_local_names(&stmt.body, names);
            }
            Stmt::For(stmt) => {
                names.insert(stmt.name.clone());
                collect_expr_local_names(&stmt.iterable, names);
                collect_local_names(&stmt.body, names);
            }
            Stmt::Lock(stmt) => {
                names.insert(stmt.binding.clone());
                collect_expr_local_names(&stmt.target, names);
                collect_local_names(&stmt.body, names);
            }
            Stmt::Defer(stmt) => match &stmt.body {
                DeferBody::Expr(expr) => collect_expr_local_names(expr, names),
                DeferBody::Block(block) => collect_local_names(block, names),
            },
            Stmt::Expr(stmt) => collect_expr_local_names(&stmt.expr, names),
            Stmt::Assign(stmt) => collect_expr_local_names(&stmt.value, names),
            Stmt::FieldAssign(stmt) => {
                collect_expr_local_names(&stmt.object, names);
                collect_expr_local_names(&stmt.value, names);
            }
            Stmt::SuperInit(stmt) => {
                for arg in &stmt.args {
                    collect_expr_local_names(&arg.expr, names);
                }
            }
            Stmt::StaticFieldAssign(stmt) => collect_expr_local_names(&stmt.value, names),
            Stmt::IndexAssign(stmt) => {
                collect_expr_local_names(&stmt.array, names);
                collect_expr_local_names(&stmt.index, names);
                collect_expr_local_names(&stmt.value, names);
            }
            Stmt::Return(stmt) => {
                if let Some(value) = &stmt.value {
                    collect_expr_local_names(value, names);
                }
            }
            Stmt::Break(_) | Stmt::Continue(_) => {}
        }
    }
}

fn collect_expr_local_names(expression: &Expr, names: &mut HashSet<String>) {
    if let Expr::Lambda(lambda) = expression {
        for param in &lambda.params {
            names.insert(param.name.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn analyze(source: &str) -> HashMap<String, bool> {
        let tokens = Lexer::new(source).tokenize().expect("lex");
        let (program, diagnostics) = Parser::new(tokens).parse();
        assert!(diagnostics.is_empty(), "parse diagnostics: {diagnostics:?}");
        analyze_program(
            &program,
            UnitNaming {
                module_prefix: None,
            },
            &FunctionMap::default(),
            &HashMap::new(),
            &[],
        )
    }

    #[test]
    fn pure_recursive_scc_is_no_panic() {
        let effects = analyze(
            "fn even(n: i64) -> bool { if n == 0 { return true; } return odd(n - 1); }\n\
             fn odd(n: i64) -> bool { if n == 0 { return false; } return even(n - 1); }",
        );
        assert_eq!(effects.get("even"), Some(&false));
        assert_eq!(effects.get("odd"), Some(&false));
    }

    #[test]
    fn panic_propagates_through_call_chain() {
        let effects = analyze(
            "fn leaf() { panic(\"boom\"); } fn middle() { leaf(); } fn top() { middle(); }",
        );
        assert_eq!(effects.get("leaf"), Some(&true));
        assert_eq!(effects.get("middle"), Some(&true));
        assert_eq!(effects.get("top"), Some(&true));
    }

    #[test]
    fn function_value_call_is_conservative() {
        let effects = analyze("fn run(f: fn(i64) -> i64) -> i64 { return f(1); }");
        assert_eq!(effects.get("run"), Some(&true));
    }

    #[test]
    fn guarded_operations_are_may_panic() {
        let effects = analyze(
            "fn divide(a: i64, b: i64) -> i64 { return a / b; }\n\
             fn index(xs: Array<i64>) -> i64 { return xs[0]; }",
        );
        assert_eq!(effects.get("divide"), Some(&true));
        assert_eq!(effects.get("index"), Some(&true));
    }

    #[test]
    fn self_method_edges_participate_in_fixpoint() {
        let effects = analyze(
            "class Work { pub fn safe(self, n: i64) -> i64 { return n + 1; }\n\
             pub fn unsafe(self) { self.safe(1); panic(\"x\"); }\n\
             pub fn reaches(self) { self.unsafe(); } }",
        );
        assert_eq!(effects.get("Work.safe"), Some(&false));
        assert_eq!(effects.get("Work.unsafe"), Some(&true));
        assert_eq!(effects.get("Work.reaches"), Some(&true));
    }

    // willow-s9ej.11 perspectives 1-8, 13, and 20: a virtual self-call
    // participates in the same whole-hierarchy target union as another
    // class-typed receiver. These analysis-level cases pin safe controls,
    // direct/transitive descendant hazards, nearest inherited targets,
    // sibling unions, and a subclass method with further descendants.
    #[test]
    fn self_dispatch_includes_a_panicking_override() {
        let effects = analyze(
            "open class Base {\n\
                 pub open fn hook(self) -> i64 { return 1; }\n\
                 pub fn run(self) -> i64 { return self.hook(); }\n\
             }\n\
             class Child extends Base {\n\
                 pub override fn hook(self) -> i64 { panic(\"child\"); return 0; }\n\
             }",
        );
        assert_eq!(effects.get("Base.run"), Some(&true));
    }

    #[test]
    fn self_dispatch_remains_no_panic_when_every_override_is_safe() {
        let effects = analyze(
            "open class Base {\n\
                 pub open fn hook(self) -> i64 { return 1; }\n\
                 pub fn run(self) -> i64 { return self.hook(); }\n\
             }\n\
             class Child extends Base {\n\
                 pub override fn hook(self) -> i64 { return 2; }\n\
             }",
        );
        assert_eq!(effects.get("Base.run"), Some(&false));
    }

    #[test]
    fn self_dispatch_includes_a_grandchild_override() {
        let effects = analyze(
            "open class Base {\n\
                 pub open fn hook(self) -> i64 { return 1; }\n\
                 pub fn run(self) -> i64 { return self.hook(); }\n\
             }\n\
             open class Middle extends Base {}\n\
             class Leaf extends Middle {\n\
                 pub override fn hook(self) -> i64 { panic(\"leaf\"); return 0; }\n\
             }",
        );
        assert_eq!(effects.get("Base.run"), Some(&true));
    }

    #[test]
    fn self_dispatch_unions_safe_and_panicking_siblings() {
        let effects = analyze(
            "open class Base {\n\
                 pub open fn hook(self) -> i64 { return 1; }\n\
                 pub fn run(self) -> i64 { return self.hook(); }\n\
             }\n\
             class SafeChild extends Base {\n\
                 pub override fn hook(self) -> i64 { return 2; }\n\
             }\n\
             class UnsafeChild extends Base {\n\
                 pub override fn hook(self) -> i64 { panic(\"unsafe\"); return 0; }\n\
             }",
        );
        assert_eq!(effects.get("Base.run"), Some(&true));
    }

    #[test]
    fn subclass_self_dispatch_includes_its_own_descendants() {
        let effects = analyze(
            "open class Base { pub open fn hook(self) -> i64 { return 1; } }\n\
             open class Middle extends Base {\n\
                 pub fn run(self) -> i64 { return self.hook(); }\n\
             }\n\
             class Leaf extends Middle {\n\
                 pub override fn hook(self) -> i64 { panic(\"leaf\"); return 0; }\n\
             }",
        );
        assert_eq!(effects.get("Middle.run"), Some(&true));
    }
}
