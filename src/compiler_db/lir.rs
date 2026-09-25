//! Body-addressed executable IR. Unit preparation preserves cross-function
//! optimization; only artifact offsets and symbol bindings remain resident.
use super::{ids::BodyIndex, lir_artifact::FlatLir, query::QueryTable};
use crate::{
    diagnostics::{Diagnostic, Span},
    ir::{
        lower::{self, CheckerTables},
        lowered::{self, LirFunction},
    },
    module::{UnitId, artifacts::ArtifactStore},
    parser::{
        ast::*,
        iter::{AstEvent, AstWalk},
    },
    semantic::ids::{FunctionId, TypeId},
};
use anyhow::{Context, Result};
use std::{
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::Arc,
};

/// One unit's body inventory: which semantic body each lowered function was
/// stored under. The bodies themselves live in the artifact store; nothing
/// here retains IR.
#[derive(Default)]
struct LirUnit {
    functions: Vec<(FunctionId, BodyId)>,
    lambdas: Vec<(ExprId, Span, BodyId)>,
    diagnostics: Arc<[Diagnostic]>,
}

pub(crate) struct LirQueries {
    store: Rc<ArtifactStore>,
    units: QueryTable<UnitId, LirUnit>,
    bodies: QueryTable<BodyId, usize>,
}

impl LirQueries {
    pub(crate) fn new(store: Rc<ArtifactStore>) -> Self {
        Self {
            store,
            units: QueryTable::named("lir_unit"),
            bodies: QueryTable::named("lir_body"),
        }
    }

    /// Lower one unit and store every body it produces, returning the
    /// constructs HIR lowering could not express (willow-mb5).
    ///
    /// The unit evaluator runs once and owns the lowering itself: call lowering
    /// and scalar-leaf inlining require all of a unit's functions together, so
    /// this is the lower bound for the analysis, not a driver convenience.
    /// Final CFG conversion and storage are memoized separately for each
    /// semantic body, including lifted lambdas, and emission addresses those
    /// bodies one at a time through [`LirQueries::body`].
    pub(crate) fn lower_unit(
        &self,
        unit: UnitId,
        program: &Program,
        index: &BodyIndex,
        tables: &CheckerTables,
    ) -> Result<Arc<[Diagnostic]>> {
        Ok(Arc::clone(
            &self.evaluate(unit, program, index, tables)?.diagnostics,
        ))
    }

    fn evaluate(
        &self,
        unit: UnitId,
        program: &Program,
        index: &BodyIndex,
        tables: &CheckerTables,
    ) -> Result<Arc<LirUnit>> {
        self.units.query(unit, || {
            let mut names = HashMap::new();
            let mut roots = Vec::new();
            let mut methods = Vec::new();
            for item in &program.items {
                match item {
                    Item::Function(f) => {
                        names.insert(FunctionId::free(&f.name), f.body.id);
                        roots.push((f.body.id, AstEvent::Block(&f.body)));
                    }
                    Item::Class(c) => {
                        let owner = TypeId::from_source_name(&c.name);
                        for ctor in &c.constructors {
                            names.insert(FunctionId::method(owner, "init"), ctor.body.id);
                            methods.push((ctor.body.id, AstEvent::Block(&ctor.body)));
                        }
                        for m in &c.methods {
                            names.insert(FunctionId::method(owner, &m.name), m.body.id);
                            methods.push((m.body.id, AstEvent::Block(&m.body)));
                        }
                        for field in c.fields.iter().filter(|field| field.is_static) {
                            if let Some(expr) = &field.initializer {
                                let static_id = index
                                    .static_id(expr.id())
                                    .context("missing static identity")?;
                                let body = index
                                    .body(unit, super::ids::BodyOwner::StaticInitializer(static_id))
                                    .context("missing static body identity")?;
                                names.insert(
                                    FunctionId::method(
                                        owner,
                                        format!("$static_init.{}", field.name),
                                    ),
                                    body,
                                );
                                roots.push((body, AstEvent::Expr(expr)));
                            }
                        }
                    }
                    _ => {}
                }
            }
            // Source lowering visits free/static functions, then class bodies.
            // Keep contextual lambda identities when injected defaults share
            // source ExprIds. No scan over every body for each lookup.
            roots.extend(methods);
            let mut lambdas: HashMap<ExprId, VecDeque<BodyId>> = HashMap::new();
            for (root, event) in roots {
                let mut parents = vec![root];
                for event in AstWalk::new(event) {
                    match event {
                        AstEvent::Expr(Expr::Lambda(lambda)) => {
                            let body = index
                                .lambda_in(*parents.last().unwrap(), lambda.id)
                                .context("missing contextual lambda body identity")?;
                            parents.push(body);
                        }
                        AstEvent::ExitExpr(Expr::Lambda(lambda)) => {
                            lambdas
                                .entry(lambda.id)
                                .or_default()
                                .push_back(parents.pop().unwrap());
                        }
                        _ => {}
                    }
                }
            }
            let (hir, diagnostics) = lower::lower_program_with(program, tables);
            let source = lowered::lower_source_program(&hir);
            let mut result = LirUnit {
                diagnostics: diagnostics.into(),
                ..LirUnit::default()
            };
            for function in source.functions {
                let name = function.name;
                let body = *names
                    .get(&name)
                    .with_context(|| format!("missing body identity for {name}"))?;
                self.store_body(body, function)?;
                result.functions.push((name, body));
            }
            for lambda in source.lambdas {
                let body = lambdas
                    .get_mut(&lambda.id)
                    .and_then(VecDeque::pop_front)
                    .context("missing lifted lambda body identity")?;
                self.store_body(body, lambda.function)?;
                result.lambdas.push((lambda.id, lambda.span, body));
            }
            Ok(result)
        })
    }

    /// Materialize a unit only for consumers that need a whole-program view,
    /// such as the textual dump. Native emission reads individual bodies.
    pub(crate) fn unit_program(&self, unit: UnitId) -> Result<lowered::LirProgram> {
        let unit = self.units.query(unit, || {
            anyhow::bail!("LIR unit requested before lowering: {unit:?}")
        })?;
        Ok(lowered::LirProgram {
            functions: unit
                .functions
                .iter()
                .map(|&(_, id)| self.body(id))
                .collect::<Result<_>>()?,
            lambdas: unit
                .lambdas
                .iter()
                .map(|&(id, span, body)| {
                    Ok(lowered::LirLambda {
                        id,
                        span,
                        function: self.body(body)?,
                    })
                })
                .collect::<Result<_>>()?,
        })
    }

    fn store_body(&self, body: BodyId, function: lowered::SourceFunction) -> Result<()> {
        self.bodies.query(body, || {
            self.store
                .write(&FlatLir::from_function(lowered::finish_body(function)))
        })?;
        Ok(())
    }

    pub(crate) fn body(&self, body: BodyId) -> Result<LirFunction> {
        let artifact = self.bodies.query(body, || {
            anyhow::bail!("LIR body requested before preparation: {body:?}")
        })?;
        self.store.read::<FlatLir>(*artifact)?.into_function()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{module::artifacts::UnitArtifacts, semantic::TypeChecker};

    fn check(source: &str) -> (Program, BodyIndex, TypeChecker) {
        let (mut program, errors) =
            crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
                .parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut index = BodyIndex::default();
        index.register_program(&program);
        index.register_unit(&mut program, UnitId::ENTRY);
        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        (program, index, checker)
    }

    fn compare(source: &str) -> (usize, usize) {
        let (program, index, checker) = check(source);
        let tables = CheckerTables::from_checker(&checker);
        let (hir, gaps) = lower::lower_program_with(&program, &tables);
        assert!(gaps.is_empty(), "{gaps:?}");
        let expected = lowered::lower_program(&hir);
        let count = expected.functions.len() + expected.lambdas.len();
        let artifacts = UnitArtifacts::new().unwrap();
        let queries = LirQueries::new(Rc::clone(&artifacts.store));
        let diagnostics = queries
            .lower_unit(UnitId::ENTRY, &program, &index, &tables)
            .unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for _ in 0..4 {
            let again = queries
                .lower_unit(UnitId::ENTRY, &program, &index, &tables)
                .unwrap();
            assert!(Arc::ptr_eq(&diagnostics, &again));
            assert_eq!(queries.unit_program(UnitId::ENTRY).unwrap(), expected);
        }
        assert_eq!(queries.units.stats().computations, 1);
        assert_eq!(queries.bodies.stats().computations, count);
        assert_eq!(queries.bodies.stats().hits, count * 4);
        (expected.functions.len(), expected.lambdas.len())
    }

    #[test]
    fn body_queries_preserve_cross_function_optimization_and_nested_closures() {
        assert_eq!(
            compare(
                r#"
            fn twice(n: i64) -> i64 { return n * 2; }
            fn apply(f: closure(i64) -> i64) -> i64 { return f(2); }
            fn main() {
                let extra = 38;
                println(apply(|x: i64| { let nested = |y: i64| twice(y) + extra; return nested(x); }));
            }
        "#
            ),
            (3, 2)
        );
    }

    #[test]
    fn body_queries_preserve_methods_constructor_static_async_and_cleanup() {
        assert_eq!(
            compare(
                r#"
            class Box {
                pub static seed: i64 = 7;
                pub value: i64;
                pub init(self, value: i64) { self.value = value; }
                pub fn get(self) -> i64 { defer { println(1); } return self.value; }
            }
            async fn fetch() -> i64 { await sleep(1); return 42; }
            async fn main() { let b = new Box(await fetch()); println(b.get()); }
        "#
            ),
            (5, 0)
        );
    }

    #[test]
    fn body_query_counts_scale_with_distinct_bodies_not_requests() {
        for count in [16, 64, 256] {
            let source: String = (0..count)
                .map(|i| {
                    format!(
                        "fn body_{i}(n: i64) -> i64 {{ let f = |x: i64| x + n; return f(2); }}\n"
                    )
                })
                .collect();
            assert_eq!(compare(&source), (count, count));
        }
    }
}
