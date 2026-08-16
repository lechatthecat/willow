//! Conservative recoverable-panic effect analysis (willow-s9ej.8).
//!
//! The analysis proves a deliberately small `NO_PANIC` subset. Unknown calls,
//! function values, interface dispatch, operations with language-level guards,
//! and unclassified builtins all remain `MAY_PANIC`. Direct-call cycles start
//! optimistic and are invalidated from intrinsic/unknown roots, which lets a
//! pure recursive SCC be proved safe without ever making an unknown edge safe.
//!
//! The call edges come from the shared graph in [`crate::semantic::call_graph`]
//! (willow-uqzx.1.2) and the propagation from the shared fixpoint in
//! [`crate::semantic::effects`] (willow-uqzx.1.3); this module owns only the
//! hazard classification and the [`FunctionId`] -> linker-symbol mapping in
//! [`backend_symbol`].

use std::collections::{HashMap, HashSet};

use crate::parser::ast::*;
use crate::parser::visit::{AstVisitor, walk_expr, walk_stmt};
use crate::semantic::call_graph::{CallGraph, ClassHierarchy};
use crate::semantic::effects::{EffectProblem, RuntimeEffects};
use crate::semantic::ids::{FunctionId, FunctionMap};

use super::symbols::{
    class_member_symbol, class_method_symbol_name, module_item_symbol, module_symbol_prefix,
};
use super::type_helpers::builtin_call_runtime_name;
use super::{Codegen, FuncGen};

/// The only effect this analysis reads out of the shared lattice.
const PANIC: RuntimeEffects = RuntimeEffects::MAY_PANIC;

struct Candidate<'a> {
    key: String,
    id: FunctionId,
    body: &'a Block,
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
    let mut candidates = Vec::new();

    for item in &program.items {
        match item {
            Item::Function(function) => {
                let key = free_key(&function.name, naming);
                free_keys.insert(function.name.clone(), key.clone());
                candidates.push(Candidate {
                    key,
                    id: FunctionId::free(function.name.as_str()),
                    body: &function.body,
                });
            }
            Item::Class(class) => {
                let owner = crate::semantic::ids::TypeId::local(class.name.as_str());
                for method in &class.methods {
                    let key = method_key(&class.name, &method.name, naming, known_modules);
                    method_keys.insert((class.name.clone(), method.name.clone()), key.clone());
                    candidates.push(Candidate {
                        key,
                        id: FunctionId::method(owner.clone(), method.name.as_str()),
                        body: &method.body,
                    });
                }
                for constructor in &class.constructors {
                    let key = method_key(&class.name, "init", naming, known_modules);
                    method_keys.insert((class.name.clone(), "init".to_string()), key.clone());
                    candidates.push(Candidate {
                        key,
                        id: FunctionId::method(owner.clone(), "init"),
                        body: &constructor.body,
                    });
                }
            }
            Item::Enum(_) | Item::Interface(_) => {}
        }
    }

    // Lambda calls remain indirect and therefore conservative at their call
    // sites, but recording their own fact keeps the callable inventory total.
    // The lifted symbol doubles as the lambda's graph id: it cannot collide
    // with a source function name.
    let mut lambda_ids = Vec::new();
    for (name, lambda) in lambdas {
        if let LambdaBody::Block(body) = &lambda.body {
            let id = FunctionId::free(name.as_str());
            lambda_ids.push((id.clone(), lambda));
            candidates.push(Candidate {
                key: name.clone(),
                id,
                body,
            });
        }
    }

    let hierarchy = ClassHierarchy::from_program(program);
    let graph = CallGraph::build(program, &hierarchy, &lambda_ids);

    let context = AnalysisContext {
        known,
        known_modules,
        free_keys: &free_keys,
        method_keys: &method_keys,
    };

    // Every "cannot see it" answer is `MAY_PANIC`: an unresolved call site, an
    // edge that escapes the problem, and a declared body with no graph node all
    // stay conservative. A pure recursive cycle has no seed at all and is
    // therefore still proved safe.
    let mut problem: EffectProblem<()> = EffectProblem::new()
        .external_callee(PANIC)
        .unknown_callee(PANIC)
        .missing_body(PANIC);

    let own_bodies: HashSet<&FunctionId> =
        candidates.iter().map(|candidate| &candidate.id).collect();
    for candidate in &candidates {
        let mut hazards = HazardVisitor { panics: false };
        hazards.visit_block(candidate.body);
        problem = problem.body(candidate.id.clone());
        if hazards.panics {
            problem = problem.seed(candidate.id.clone(), PANIC, None);
        }
    }

    // Resolve every edge target this unit does not own into a leaf fact:
    // a language intrinsic, a runtime ABI row, or an already-analyzed imported
    // symbol. Seeding them as leaves keeps the classification in one place and
    // lets the shared fixpoint own the propagation.
    for (_, sites) in graph.iter() {
        for target in &sites.targets {
            if own_bodies.contains(target) {
                continue;
            }
            problem = problem.seed(target.clone(), external_effects(target, &context), None);
        }
    }

    let facts = problem.solve(&graph);

    let mut effects: HashMap<String, bool> = HashMap::new();
    for candidate in candidates {
        // Multiple constructors currently share one backend `init` symbol.
        // Union their facts rather than letting a later declaration erase an
        // earlier hazard.
        let may_panic = facts.intersects(&candidate.id, PANIC);
        *effects.entry(candidate.key).or_insert(false) |= may_panic;
    }
    effects
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
}

/// The `::`-qualified source spelling a [`FunctionId`] came from.
fn source_name(id: &FunctionId) -> String {
    match id.namespace() {
        Some(namespace) => format!("{namespace}::{}", id.name()),
        None => id.name().to_string(),
    }
}

/// The one place a shared-graph [`FunctionId`] becomes a linker symbol.
///
/// Free functions declared by this unit use the unit's own naming (a module
/// prefix for an imported unit, the bare name for the entry unit). Anything
/// else keeps its source spelling, which is the key imported facts were
/// registered under. Methods go through the class-symbol resolution the rest of
/// the backend uses, so an aliased import (`import math as m`) resolves to the
/// module's canonical prefix rather than the local alias.
fn backend_symbol(id: &FunctionId, context: &AnalysisContext<'_>) -> String {
    let Some(owner) = id.owner() else {
        let source = source_name(id);
        return context.free_keys.get(&source).cloned().unwrap_or(source);
    };
    let class = match id.namespace() {
        Some(namespace) => format!("{namespace}::{owner}"),
        None => owner.to_string(),
    };
    if let Some(key) = context
        .method_keys
        .get(&(class.clone(), id.name().to_string()))
    {
        return key.clone();
    }
    if let Some(prefix) = context.known_modules.get(&class) {
        return module_item_symbol(prefix, id.name());
    }
    class_method_symbol_name(context.known_modules, &class, id.name())
}

/// The effects of a call target this unit does not own a body for.
///
/// Three kinds land here: language intrinsics, runtime ABI rows, and symbols
/// from already-analyzed imported modules. The runtime rows are the reason the
/// shared lattice is [`RuntimeEffects`] — the fact comes straight off the ABI
/// table rather than a list maintained per analysis.
fn external_effects(target: &FunctionId, context: &AnalysisContext<'_>) -> RuntimeEffects {
    // Language intrinsics and runtime builtins have no user body to reach, so
    // they are classified by source name before any symbol mapping.
    if target.owner().is_none() && target.namespace().is_none() {
        match target.name() {
            "panic" | "format" => return PANIC,
            "recover" | "pow" | "powf" => return RuntimeEffects::NONE,
            name => {
                if let Some(runtime_name) = builtin_call_runtime_name(name) {
                    return crate::backend::abi::runtime_symbol(runtime_name)
                        .map(|symbol| symbol.effects().intersection(PANIC))
                        // An unclassified builtin is not proof of safety.
                        .unwrap_or(PANIC);
                }
            }
        }
    }

    let key = backend_symbol(target, context);
    match context.known.get(&key).copied() {
        Some(false) => RuntimeEffects::NONE,
        Some(true) => PANIC,
        // `new C(...)` where no `init` is known is the implicit memberwise
        // constructor, which only allocates and stores fields.
        None if target.owner().is_some() && target.name() == "init" => RuntimeEffects::NONE,
        None => PANIC,
    }
}

/// Direct hazards a body performs itself, independent of what it calls.
///
/// Read off the shared structural walk (willow-uqzx.1.1). Every arm is
/// POST-order: the children are already accounted for by `walk_*` before this
/// node is classified.
struct HazardVisitor {
    panics: bool,
}

impl HazardVisitor {
    fn mark_direct(&mut self) {
        self.panics = true;
    }
}

impl AstVisitor for HazardVisitor {
    /// A lambda body is registered as its own candidate, and every call through
    /// a function value stays conservative at the call site, so descending here
    /// would only double-count.
    fn visit_lambda(&mut self, _lambda: &LambdaExpr) {}

    fn visit_stmt(&mut self, statement: &Stmt) {
        walk_stmt(self, statement);
        match statement {
            // Bounds guard.
            Stmt::IndexAssign(_) => self.mark_direct(),
            // Recursive acquisition and a lost ownership token are recoverable
            // language faults.
            Stmt::Lock(_) => self.mark_direct(),
            // `super.init` is an unresolved edge in the shared graph, which
            // already makes the body conservative.
            Stmt::Let(_)
            | Stmt::SuperInit(_)
            | Stmt::Assign(_)
            | Stmt::FieldAssign(_)
            | Stmt::StaticFieldAssign(_)
            | Stmt::If(_)
            | Stmt::While(_)
            | Stmt::Break(_)
            | Stmt::Continue(_)
            | Stmt::Defer(_)
            | Stmt::For(_)
            | Stmt::Return(_)
            | Stmt::Expr(_) => {}
        }
    }

    fn visit_expr(&mut self, expression: &Expr) {
        walk_expr(self, expression);
        match expression {
            Expr::Binary(expr) => {
                if matches!(expr.op, BinOp::Div | BinOp::Rem | BinOp::Pow) {
                    self.mark_direct();
                }
            }
            Expr::FieldAccess(..) => self.mark_direct(),
            // Strict await can turn cancellation into a language panic.
            // TaskResult awaits are intentionally not distinguished here:
            // retaining a check is conservative.
            Expr::Await(_) => self.mark_direct(),
            Expr::Select(_) => self.mark_direct(),
            // Object/interface display may invoke user `toString` code.
            Expr::Print(..) => self.mark_direct(),
            // Bounds guard.
            Expr::Index(..) => self.mark_direct(),
            // Every call form is an edge in the shared graph, classified by
            // `classify_edge` rather than here.
            Expr::Call(_)
            | Expr::MethodCall(_)
            | Expr::StaticCall(_)
            | Expr::New(_)
            | Expr::Integer(..)
            | Expr::Float(..)
            | Expr::Bool(..)
            | Expr::String(..)
            | Expr::Var(..)
            | Expr::StaticField(_)
            | Expr::Unary(_)
            | Expr::ObjectLiteral(_)
            | Expr::Ternary(_)
            | Expr::Range(_)
            | Expr::Lambda(_)
            | Expr::Match(_)
            | Expr::TryPropagate(..)
            | Expr::ArrayLiteral(..) => {}
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
