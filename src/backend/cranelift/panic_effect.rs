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
//! standalone external-call adaptation and [`FunctionId`] -> linker-symbol mapping in
//! [`backend_symbol`].

use super::ModuleSymbols;
use std::collections::HashMap;

use crate::compiler_db::effects::EffectQueries;
use crate::module::UnitId;
use crate::parser::ast::*;
use crate::semantic::effects::RuntimeEffects;
use crate::semantic::ids::{FunctionId, FunctionMap};

use super::symbols::{
    class_member_symbol, class_method_symbol_name, module_item_symbol, module_symbol_prefix,
};
use super::{Codegen, FuncGen};

/// The only effect this analysis reads out of the shared lattice.
const PANIC: RuntimeEffects = RuntimeEffects::MAY_PANIC;

struct Candidate {
    key: String,
    id: FunctionId,
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
    known_modules: &ModuleSymbols,
    lambdas: &[(String, LambdaExpr)],
    expr_types: &HashMap<ExprId, Type<crate::semantic::ids::TypeId>>,
) -> HashMap<String, bool> {
    analyze_program_with_queries(
        program,
        naming,
        known,
        known_modules,
        lambdas,
        expr_types,
        None,
    )
    .expect("standalone panic analysis has no fallible query")
}

/// The session's effect queries for one unit. `lambda_bodies` runs parallel
/// to the lifted lambdas: the checked body identity of each, whose resolved
/// call edges the session graph already holds under [`FunctionId::lambda`].
struct Session<'a> {
    queries: &'a EffectQueries,
    unit: UnitId,
    lambda_bodies: Vec<BodyId>,
    lambda_units: HashMap<FunctionId, UnitId>,
}

/// Without a session, the shared type checker collects resolved edges and
/// the standalone adapter maps lambda block identities to lifted symbols.
fn analyze_program_with_queries(
    program: &Program,
    naming: UnitNaming<'_>,
    known: &FunctionMap<bool>,
    known_modules: &ModuleSymbols,
    lambdas: &[(String, LambdaExpr)],
    expr_types: &HashMap<ExprId, Type<crate::semantic::ids::TypeId>>,
    session: Option<Session<'_>>,
) -> anyhow::Result<HashMap<String, bool>> {
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
                });
            }
            Item::Class(class) => {
                let owner = crate::semantic::ids::TypeId::local(class.name.as_str());
                for method in &class.methods {
                    let key = method_key(&class.name, &method.name, naming, known_modules);
                    method_keys.insert((class.name.clone(), method.name.clone()), key.clone());
                    candidates.push(Candidate {
                        key,
                        id: FunctionId::method(owner, method.name.as_str()),
                    });
                }
                for _constructor in &class.constructors {
                    let key = method_key(&class.name, "init", naming, known_modules);
                    method_keys.insert((class.name.clone(), "init".to_string()), key.clone());
                    candidates.push(Candidate {
                        key,
                        id: FunctionId::method(owner, "init"),
                    });
                }
            }
            Item::Enum(_) | Item::Interface(_) => {}
        }
    }

    // Lambda calls remain indirect and therefore conservative at their call
    // sites, but recording their own fact keeps the callable inventory total.
    // The graph id is the checked body identity when the session has one;
    // otherwise the lifted symbol, which cannot collide with a source name.
    let lambda_bodies = session
        .as_ref()
        .map(|session| session.lambda_bodies.as_slice());
    if let Some(bodies) = lambda_bodies {
        anyhow::ensure!(
            bodies.len() == lambdas.len(),
            "lambda identity/declaration count mismatch"
        );
    }
    let mut lambda_ids = Vec::new();
    for (index, (name, lambda)) in lambdas.iter().enumerate() {
        if let LambdaBody::Block(_) = &lambda.body {
            let id = match lambda_bodies {
                Some(bodies) => FunctionId::lambda(bodies[index]),
                None => FunctionId::free(name.as_str()),
            };
            lambda_ids.push((id, lambda));
            candidates.push(Candidate {
                key: name.clone(),
                id,
            });
        }
    }

    let context = AnalysisContext {
        known,
        known_modules,
        free_keys: &free_keys,
        method_keys: &method_keys,
    };

    let facts = match session {
        Some(session) => std::sync::Arc::new(
            candidates
                .iter()
                .map(|candidate| {
                    let unit = session
                        .lambda_units
                        .get(&candidate.id)
                        .copied()
                        .unwrap_or(session.unit);
                    (candidate.id, session.queries.panic(unit, candidate.id))
                })
                .collect(),
        ),
        None => std::sync::Arc::new(crate::compiler_db::effects::analyze(
            program,
            &lambda_ids,
            expr_types,
            |target| external_effects(target, &context),
        )),
    };

    let mut effects: HashMap<String, bool> = HashMap::new();
    for candidate in candidates {
        // Multiple constructors currently share one backend `init` symbol.
        // Union their facts rather than letting a later declaration erase an
        // earlier hazard.
        let may_panic = facts.get(&candidate.id).copied().unwrap_or(true);
        *effects.entry(candidate.key).or_insert(false) |= may_panic;
    }
    Ok(effects)
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
        lambda_declarations: &[crate::parser::ast::BodyId],
        expr_types: &HashMap<ExprId, Type<crate::semantic::ids::TypeId>>,
    ) -> anyhow::Result<()> {
        let effects = if let Some((queries, unit)) = &self.effect_queries {
            // The checker publishes lambda edges under checked body identities,
            // so a session without body queries would read every lambda as an
            // unchecked body. A copied interface default is checked once under
            // its canonical body: its lambdas' edges live under the source
            // lambda identity. Cross-unit copies read the source unit's facts;
            // generic instances without a canonical proof keep their own facts.
            let index = self
                .body_queries
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("effect queries require body queries"))?
                .index();
            let lambda_bodies: Vec<_> = lambda_declarations
                .iter()
                .map(|body| {
                    // A checked copy combines canonical hazards with its own
                    // resolved edges. Prefer it when present, so a differently
                    // bound free call cannot inherit a false no-panic proof.
                    if queries.has_fact(*unit, FunctionId::lambda(*body)) {
                        return *body;
                    }
                    let source = index.source_body(*body);
                    if index
                        .owner(source)
                        .is_some_and(|(unit, _)| queries.has_fact(unit, FunctionId::lambda(source)))
                    {
                        source
                    } else {
                        *body
                    }
                })
                .collect();
            let lambda_units = lambda_bodies
                .iter()
                .filter_map(|body| {
                    index
                        .owner(*body)
                        .map(|(unit, _)| (FunctionId::lambda(*body), unit))
                })
                .collect();
            analyze_program_with_queries(
                program,
                naming,
                &self.function_may_panic,
                &self.known_modules,
                lambdas,
                expr_types,
                Some(Session {
                    queries,
                    unit: *unit,
                    lambda_bodies,
                    lambda_units,
                }),
            )?
        } else {
            analyze_program(
                program,
                naming,
                &self.function_may_panic,
                &self.known_modules,
                lambdas,
                expr_types,
            )
        };
        let mut ordered = effects.into_iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.0.cmp(&right.0));
        *self.dispatch_cache.get_mut() = Default::default();
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
        Ok(())
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
    known_modules: &ModuleSymbols,
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
    known_modules: &'a ModuleSymbols,
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
    if let Some(prefix) = context.known_modules.linker_prefix(&class) {
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
    if let Some(effects) = crate::compiler_db::effects::intrinsic_effects(target) {
        return effects;
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
            &ModuleSymbols::default(),
            &[],
            &HashMap::new(),
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
    fn concatenation_fault_propagates_to_callers() {
        let effects = analyze(
            "fn concat(a: String, b: String) -> String { return a + b; } \
             fn caller() -> String { return concat(\"a\", \"b\"); }",
        );
        assert_eq!(effects.get("concat"), Some(&true));
        assert_eq!(effects.get("caller"), Some(&true));
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
            "class Work { pub fn safe(self, n: i64) -> i64 { return n - 1; }\n\
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
