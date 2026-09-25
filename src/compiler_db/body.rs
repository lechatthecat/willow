//! Evictable body query values. Only artifact references remain resident.
use super::normalize::{LambdaSplit, StdCollectionImports};
use super::{ids::BodyIndex, query::QueryTable};
use crate::parser::ast::{
    ConstructorDecl, FieldDecl, FunctionDecl, Item, LambdaBody, MethodDecl, Program,
};
use crate::{
    module::artifacts::ArtifactStore, parser::ast::BodyId, semantic::type_checker::body::TypedBody,
};
use anyhow::Result;
use std::rc::Rc;

#[derive(serde::Serialize, serde::Deserialize)]
enum NormalizedBody {
    Function(FunctionDecl),
    Method(MethodDecl),
    Constructor(ConstructorDecl),
    Static(FieldDecl),
    /// A lambda's own body, with its nested lambdas detached (willow-afb5.17).
    Lambda(LambdaBody),
}

impl NormalizedBody {
    /// Walk the body with the normalizer. Constructors and static initializers
    /// are outside the normalization boundary, so they use a walk-only pass
    /// that only splits or reattaches lambdas.
    fn walk(&mut self, imports: &StdCollectionImports, walk_only: &StdCollectionImports) {
        use super::normalize::*;
        match self {
            Self::Function(function) => normalize_std_collection_function(function, imports),
            Self::Method(method) => normalize_std_collection_method(method, imports),
            Self::Constructor(constructor) => {
                normalize_std_collection_block(&mut constructor.body, walk_only)
            }
            Self::Static(field) => {
                if let Some(expr) = &mut field.initializer {
                    normalize_std_collection_expr(expr, walk_only)
                }
            }
            Self::Lambda(_) => unreachable!("lambda bodies are split from their root"),
        }
    }
}

pub(crate) struct BodyQueries {
    store: Rc<ArtifactStore>,
    index: Rc<BodyIndex>,
    typed: QueryTable<BodyId, usize>,
    normalized: QueryTable<BodyId, usize>,
    assignment: QueryTable<BodyId, Vec<bool>>,
    borrows: QueryTable<BodyId, usize>,
}

impl BodyQueries {
    pub(crate) fn index(&self) -> &BodyIndex {
        &self.index
    }
    #[cfg(test)]
    pub(crate) fn stats(&self) -> super::query::QueryStats {
        self.typed.stats()
    }

    pub(crate) fn new(store: Rc<ArtifactStore>, index: Rc<BodyIndex>) -> Self {
        Self {
            store,
            index,
            typed: QueryTable::named("typed_body"),
            normalized: QueryTable::named("normalized_body"),
            assignment: QueryTable::named("definite_assignment"),
            borrows: QueryTable::named("async_borrow_report"),
        }
    }

    /// Constructor flow depends on the current body's checked expression types.
    /// It runs inside typed_body, before that result is published, so querying
    /// typed_body here would introduce a dependency cycle.
    pub(crate) fn definite_assignment(
        &self,
        id: BodyId,
        compute: impl FnOnce() -> Vec<bool>,
    ) -> Result<std::sync::Arc<Vec<bool>>> {
        self.assignment.query(id, || Ok(compute()))
    }

    pub(crate) fn async_borrow_report(
        &self,
        id: BodyId,
        compute: impl FnOnce() -> crate::semantic::async_borrows::BorrowReport,
    ) -> Result<crate::semantic::async_borrows::BorrowReport> {
        let mut fresh = None;
        let record = self.borrows.query(id, || {
            let report = compute();
            let record = self.store.write(&report)?;
            fresh = Some(report);
            Ok(record)
        })?;
        match fresh {
            Some(report) => Ok(report),
            None => self.store.read(*record),
        }
    }

    pub(crate) fn check_async_borrows(
        &self,
        program: &Program,
        types: &std::collections::HashMap<crate::parser::ast::ExprId, crate::parser::ast::Type>,
        modes: &std::collections::HashMap<
            crate::parser::ast::ExprId,
            crate::parser::ast::ParamMode,
        >,
    ) -> Result<Vec<crate::diagnostics::Diagnostic>> {
        use crate::semantic::async_borrows::{self, BorrowRoot};
        async_borrows::check_with(program, types, modes, |root, compute| {
            let id = match root {
                BorrowRoot::Block(id) => id,
                BorrowRoot::Initializer(expr) => self
                    .initializer(expr)
                    .ok_or_else(|| anyhow::anyhow!("missing initializer identity: {expr:?}"))?,
            };
            self.async_borrow_report(id, compute)
        })
    }

    pub(crate) fn evaluate(
        &self,
        id: BodyId,
        compute: impl FnOnce() -> Result<TypedBody>,
    ) -> Result<TypedBody> {
        let mut fresh = None;
        let artifact = self.typed.query(id, || {
            let value = compute()?;
            let artifact = self.store.write(&value)?;
            fresh = Some(value);
            Ok(artifact)
        })?;
        match fresh {
            Some(value) => Ok(value),
            None => self.store.read(*artifact),
        }
    }

    /// Whether `id`'s typed body has been evaluated in this session.
    pub(crate) fn is_typed(&self, id: BodyId) -> bool {
        self.typed.is_ready(&id)
    }

    pub(crate) fn read(&self, id: BodyId) -> Result<TypedBody> {
        let record = self.typed_record(id)?;
        self.store.read(*record)
    }

    fn typed_record(&self, id: BodyId) -> Result<std::sync::Arc<usize>> {
        self.typed.query(id, || {
            // An injected non-generic default uses the canonical interface's
            // checked body. Concrete generic instances already have own keys.
            let source = self.index.source_body(id);
            anyhow::ensure!(source != id, "typed body requested before checking: {id:?}");
            Ok(*self.typed_record(source)?)
        })
    }

    /// A root body's normalized syntax. Its lambdas are stored as their own
    /// records keyed by contextual `BodyId`, so each lambda body is normalized
    /// and serialized once; the returned tree has them reattached.
    fn normalized_body(
        &self,
        id: BodyId,
        imports: &StdCollectionImports,
        walk_only: &StdCollectionImports,
        source: impl FnOnce() -> NormalizedBody,
    ) -> Result<NormalizedBody> {
        let mut fresh = None;
        let record = self.normalized.query(id, || {
            self.typed_record(id)?;
            let mut body = source();
            let target = if matches!(
                body,
                NormalizedBody::Function(_) | NormalizedBody::Method(_)
            ) {
                imports
            } else {
                walk_only
            };
            target.install_split(LambdaSplit::detach(Rc::clone(&self.index), id));
            body.walk(imports, walk_only);
            let lambdas = target.take_split().expect("installed split").finish()?;
            let mut resident = std::collections::HashMap::with_capacity(lambdas.len());
            for (lambda, lambda_body) in lambdas {
                let record = NormalizedBody::Lambda(lambda_body);
                self.normalized
                    .query(lambda, || self.store.write(&record))?;
                let NormalizedBody::Lambda(lambda_body) = record else {
                    unreachable!()
                };
                resident.insert(lambda, lambda_body);
            }
            let record = self.store.write(&body)?;
            fresh = Some((body, resident));
            Ok(record)
        })?;
        let (mut body, resident) = match fresh {
            Some(fresh) => fresh,
            None => (self.store.read(*record)?, self.stored_lambdas(id)?),
        };
        if resident.is_empty() {
            return Ok(body);
        }
        walk_only.install_split(LambdaSplit::attach(Rc::clone(&self.index), id, resident));
        body.walk(walk_only, walk_only);
        walk_only.take_split().expect("installed split").finish()?;
        Ok(body)
    }

    /// Every stored lambda record beneath `root`, read once each.
    fn stored_lambdas(
        &self,
        root: BodyId,
    ) -> Result<std::collections::HashMap<BodyId, LambdaBody>> {
        let mut bodies = std::collections::HashMap::new();
        let mut stack = vec![root];
        while let Some(parent) = stack.pop() {
            for (_, lambda) in self.index.child_lambdas(parent) {
                if !self.normalized.is_ready(&lambda) {
                    continue;
                }
                let record = self.normalized.query(lambda, || {
                    anyhow::bail!("normalized lambda evicted: {lambda:?}")
                })?;
                let NormalizedBody::Lambda(body) = self.store.read(*record)? else {
                    anyhow::bail!("lambda body kind mismatch")
                };
                bodies.insert(lambda, body);
                stack.push(lambda);
            }
        }
        Ok(bodies)
    }

    /// Assemble declaration input from memoized body transformations. Imports
    /// are indexed once per unit. This preserves the established normalization
    /// boundary: constructors/static expressions/default templates are unchanged.
    pub(crate) fn normalized_program(&self, program: &Program) -> Result<Program> {
        use super::normalize::*;
        let imports = std_collection_imports(program);
        let walk_only = StdCollectionImports::walk_only();
        let mut items = Vec::with_capacity(program.items.len());
        for item in &program.items {
            items.push(match item {
                Item::Function(function) => {
                    let body =
                        self.normalized_body(function.body.id, &imports, &walk_only, || {
                            NormalizedBody::Function(function.clone())
                        })?;
                    let NormalizedBody::Function(function) = body else {
                        anyhow::bail!("function body kind mismatch")
                    };
                    Item::Function(function)
                }
                Item::Class(class) => {
                    let mut class = class.clone();
                    for field in &mut class.fields {
                        normalize_std_collection_type(&mut field.ty, &imports);
                    }
                    class.methods = class
                        .methods
                        .into_iter()
                        .map(|method| {
                            let body =
                                self.normalized_body(method.body.id, &imports, &walk_only, || {
                                    NormalizedBody::Method(method)
                                })?;
                            let NormalizedBody::Method(method) = body else {
                                anyhow::bail!("method body kind mismatch")
                            };
                            Ok(method)
                        })
                        .collect::<Result<_>>()?;
                    class.constructors = class
                        .constructors
                        .into_iter()
                        .map(|constructor| {
                            let body = self.normalized_body(
                                constructor.body.id,
                                &imports,
                                &walk_only,
                                || NormalizedBody::Constructor(constructor),
                            )?;
                            let NormalizedBody::Constructor(constructor) = body else {
                                anyhow::bail!("constructor body kind mismatch")
                            };
                            Ok(constructor)
                        })
                        .collect::<Result<_>>()?;
                    class.fields = class
                        .fields
                        .into_iter()
                        .map(|field| {
                            let Some(id) = field
                                .initializer
                                .as_ref()
                                .filter(|_| field.is_static)
                                .and_then(|expr| self.initializer(expr.id()))
                            else {
                                return Ok(field);
                            };
                            let body = self.normalized_body(id, &imports, &walk_only, || {
                                NormalizedBody::Static(field)
                            })?;
                            let NormalizedBody::Static(field) = body else {
                                anyhow::bail!("static body kind mismatch")
                            };
                            Ok(field)
                        })
                        .collect::<Result<_>>()?;
                    Item::Class(class)
                }
                Item::Enum(_) | Item::Interface(_) => {
                    let mut item = item.clone();
                    normalize_std_collection_item(&mut item, &imports);
                    item
                }
            });
        }
        Ok(Program {
            module: program.module.clone(),
            imports: program.imports.clone(),
            items,
        })
    }

    pub(crate) fn initializer(&self, expr: crate::parser::ast::ExprId) -> Option<BodyId> {
        let id = self.index.static_id(expr)?;
        self.index
            .body(id.unit, super::ids::BodyOwner::StaticInitializer(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::{UnitId, artifacts::UnitArtifacts};
    use crate::parser::{
        ast::*,
        iter::{AstEvent, AstWalk},
    };
    use crate::semantic::TypeChecker;

    fn prepare(source: &str, desugar: bool) -> (Program, Rc<BodyQueries>, TypeChecker) {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (mut program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}\n{source}");
        let mut index = BodyIndex::default();
        index.register_program(&program);
        if desugar {
            let output = crate::desugar::DesugarPass::run(&mut program, &mut []);
            assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        }
        index.register_unit(&mut program, UnitId::ENTRY);
        let artifacts = UnitArtifacts::new().unwrap();
        let queries = Rc::new(BodyQueries::new(
            Rc::clone(&artifacts.store),
            Rc::new(index),
        ));
        let mut checker = TypeChecker::new();
        checker.set_body_queries(Rc::clone(&queries));
        checker.check_program(&program);
        checker.finish_body_queries().unwrap();
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        (program, queries, checker)
    }

    fn roots(program: &Program) -> Vec<AstEvent<'_>> {
        let mut roots = Vec::new();
        for item in &program.items {
            match item {
                Item::Function(function) => roots.push(AstEvent::Block(&function.body)),
                Item::Class(class) => {
                    roots.extend(
                        class
                            .methods
                            .iter()
                            .map(|method| AstEvent::Block(&method.body)),
                    );
                    roots.extend(
                        class
                            .constructors
                            .iter()
                            .map(|constructor| AstEvent::Block(&constructor.body)),
                    );
                    roots.extend(
                        class
                            .fields
                            .iter()
                            .filter_map(|field| field.initializer.as_ref().map(AstEvent::Expr)),
                    );
                }
                Item::Interface(interface) => roots.extend(
                    interface
                        .methods
                        .iter()
                        .filter_map(|method| method.default_body.as_ref().map(AstEvent::Block)),
                ),
                Item::Enum(_) => {}
            }
        }
        roots
    }

    fn identities(program: &Program) -> (Vec<BodyId>, Vec<ExprId>) {
        let mut blocks = Vec::new();
        let mut expressions = Vec::new();
        for event in roots(program).into_iter().flat_map(AstWalk::new) {
            match event {
                AstEvent::Block(block) => blocks.push(block.id),
                AstEvent::Expr(expr) => expressions.push(expr.id()),
                _ => {}
            }
        }
        (blocks, expressions)
    }

    fn assert_repeated_normalization(program: &Program, queries: &BodyQueries, count: usize) {
        let expected = super::super::normalize::normalize_std_collection_program(program);
        let expected = serde_json::to_value(expected).unwrap();
        for iteration in 0..4 {
            let normalized = queries.normalized_program(program).unwrap();
            assert_eq!(identities(&normalized), identities(program));
            assert_eq!(serde_json::to_value(normalized).unwrap(), expected);
            assert_eq!(queries.normalized.stats().computations, count);
            assert_eq!(queries.normalized.stats().calls, count * (iteration + 1));
            assert_eq!(queries.normalized.stats().hits, count * iteration);
        }
    }

    #[test]
    fn body_analysis_queries_reuse_results_and_preserve_diagnostics() {
        let (program, queries, checker) = prepare(
            "async fn read(x: &i64) -> i64 { return x; } async fn main() { let x = 1; let t = read(&x); }",
            false,
        );
        let expected = crate::semantic::async_borrows::check(
            &program,
            &checker.expr_types,
            &checker.reference_arg_modes,
        );
        assert!(!expected.is_empty());
        for _ in 0..4 {
            let actual = queries
                .check_async_borrows(&program, &checker.expr_types, &checker.reference_arg_modes)
                .unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(&expected).unwrap()
            );
        }
        assert_eq!(queries.borrows.stats().computations, 2);
        assert_eq!(queries.borrows.stats().hits, 6);
        let (program, queries, _) = prepare(
            "class C { value: i64; pub init(self) { self.value = 42; } }",
            false,
        );
        let Item::Class(class) = &program.items[0] else {
            panic!()
        };
        let result = queries
            .definite_assignment(class.constructors[0].body.id, || panic!("recomputed flow"))
            .unwrap();
        assert_eq!(*result, [false]);
        assert_eq!(queries.assignment.stats().computations, 1);
        assert_eq!(queries.assignment.stats().hits, 1);
    }

    /// Spec 9.3: typed_body is acyclic by design. An evaluation that re-enters
    /// its own key is an ICE naming the key chain, never a hang, and the failed
    /// keys stay failed so a repeated request cannot turn them into a success.
    #[test]
    fn self_dependent_body_query_is_an_ice_not_a_hang() {
        let tokens = crate::lexer::Lexer::new("fn f() {} fn g() {}")
            .tokenize()
            .unwrap();
        let (mut program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut index = BodyIndex::default();
        index.register_program(&program);
        index.register_unit(&mut program, UnitId::ENTRY);
        let artifacts = UnitArtifacts::new().unwrap();
        let queries = BodyQueries::new(Rc::clone(&artifacts.store), Rc::new(index));
        let Item::Function(f) = &program.items[0] else {
            panic!()
        };
        let Item::Function(g) = &program.items[1] else {
            panic!()
        };
        let (f, g) = (f.body.id, g.body.id);
        let cyclic = queries.evaluate(f, || {
            queries.evaluate(g, || {
                queries.evaluate(f, || unreachable!("cycle must not recompute"))
            })
        });
        let error = format!("{:#}", cyclic.err().expect("cycle is an error"));
        assert!(error.contains("E0800"), "{error}");
        assert!(
            error.contains(&format!(
                "typed_body({f:?}) -> typed_body({g:?}) -> typed_body({f:?})"
            )),
            "{error}"
        );
        // Both frames are poisoned: no recomputation, no artifact, no hang.
        assert!(queries.evaluate(f, || unreachable!()).is_err());
        assert!(queries.evaluate(g, || unreachable!()).is_err());
        assert!(queries.read(f).is_err());
        // The third frame is rejected on entry, so only two frames ever ran.
        assert_eq!(queries.stats().max_depth, 2);
    }

    #[test]
    fn borrow_report_query_order_does_not_change_emission_order() {
        use crate::semantic::async_borrows::{self, BorrowRoot};
        let (program, queries, checker) = prepare(
            "async fn read(x: &i64) -> i64 { return x; } async fn first() { let x = 1; let t = read(&x); } async fn second() { let x = 2; let t = read(&x); }",
            false,
        );
        let mut reports = Vec::new();
        let expected = async_borrows::check_with(
            &program,
            &checker.expr_types,
            &checker.reference_arg_modes,
            |root, compute| {
                let BorrowRoot::Block(id) = root else {
                    panic!()
                };
                let report = compute();
                reports.push((id, report.clone()));
                Ok(report)
            },
        )
        .unwrap();
        assert_eq!(expected.len(), 2);
        for (id, report) in reports.into_iter().rev() {
            queries.async_borrow_report(id, || report).unwrap();
        }
        let actual = queries
            .check_async_borrows(&program, &checker.expr_types, &checker.reference_arg_modes)
            .unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(queries.borrows.stats().computations, 3);
        assert_eq!(queries.borrows.stats().hits, 3);
    }

    #[test]
    fn body_analysis_query_work_scales_with_distinct_bodies() {
        for count in [16, 64, 256] {
            let source: String = (0..count)
                .map(|i| {
                    format!("class C{i} {{ value: i64; pub init(self) {{ self.value = 42; }} }}\n")
                })
                .collect();
            let (program, queries, _) = prepare(&source, false);
            for item in program.items.iter().rev() {
                let Item::Class(class) = item else { panic!() };
                for _ in 0..4 {
                    assert_eq!(
                        *queries
                            .definite_assignment(class.constructors[0].body.id, || panic!(
                                "recomputed flow"
                            ))
                            .unwrap(),
                        [false]
                    );
                }
            }
            assert_eq!(queries.assignment.stats().computations, count);
            assert_eq!(queries.assignment.stats().hits, 4 * count);
        }
    }

    #[test]
    fn normalized_body_queries_match_program_alias_and_declaration_semantics() {
        let (program, queries, _) = prepare(
            r#"
            import std::collections::Array as Values;
            import std::collections::Map as Dict;
            import std::env as environment;
            enum Payload { List(Values<i64>) }
            interface Catalog {
                fn values(self, input: Values<i64>) -> Values<i64> { return input; }
            }
            class C {
                pub static seed: i64 = 1;
                data: Values<i64>;
                pub init(self, data: Values<i64>) { self.data = data; }
                pub fn copy(self, input: Values<i64>) -> Values<i64> { return input; }
                pub static fn count() -> i64 { return environment::args_len(); }
            }
            fn copy(input: Values<i64>) -> Values<i64> {
                let mut map = Dict::new(); map.insert(1, 2); return input;
            }
        "#,
            false,
        );
        // Function, two methods, constructor, and static initializer. Interface
        // templates retain the existing signature-only normalization boundary.
        assert_repeated_normalization(&program, &queries, 5);
    }

    #[test]
    fn normalized_body_queries_preserve_nested_lambda_await_ids_and_checked_types() {
        let (program, queries, checker) = prepare(
            r#"
            fn apply(f: closure(i64) -> i64, n: i64) -> i64 { return f(n); }
            async fn fetch() -> i64 { return 2; }
            async fn main() {
                let extra = 40;
                println(apply(|x: i64| {
                    let nested = |y: i64| y + extra;
                    return nested(x);
                }, await fetch()));
            }
        "#,
            false,
        );
        // Three roots plus two lambda records (willow-afb5.17).
        assert_repeated_normalization(&program, &queries, 5);
        // The root record keeps only a placeholder where each lambda was, so
        // every lambda body is serialized once, under its own key.
        let Item::Function(main) = &program.items[2] else {
            panic!()
        };
        let record = queries.normalized.ready(&main.body.id).unwrap();
        let NormalizedBody::Function(stored) = queries.store.read(*record).unwrap() else {
            panic!()
        };
        let mut placeholders = 0;
        for event in AstWalk::new(AstEvent::Block(&stored.body)) {
            if let AstEvent::Expr(Expr::Lambda(lambda)) = event {
                assert!(matches!(&lambda.body, LambdaBody::Block(b) if b.stmts.is_empty()));
                placeholders += 1;
            }
        }
        assert_eq!(placeholders, 1);
        let normalized = queries.normalized_program(&program).unwrap();
        let mut rechecked = TypeChecker::new();
        rechecked.check_program(&normalized);
        assert!(rechecked.errors.is_empty(), "{:?}", rechecked.errors);
        assert_eq!(checker.expr_types, rechecked.expr_types);
        assert_eq!(checker.lambda_captures.len(), 2);
        let mut lambdas = 0;
        let mut awaits = 0;
        for event in roots(&normalized).into_iter().flat_map(AstWalk::new) {
            match event {
                AstEvent::Expr(Expr::Lambda(_)) => lambdas += 1,
                AstEvent::Expr(Expr::Await(_)) => awaits += 1,
                _ => {}
            }
        }
        assert_eq!((lambdas, awaits), (2, 1));
    }

    #[test]
    fn normalized_body_query_computations_are_once_per_root_at_increasing_sizes() {
        for count in [16, 64, 256] {
            let source: String = (0..count)
                .map(|i| format!("fn body_{i}() -> i64 {{ return 1 + 2; }}\n"))
                .collect();
            let (program, queries, _) = prepare(&source, false);
            assert_repeated_normalization(&program, &queries, count);
            assert_eq!(queries.typed.stats().computations, count);
        }
    }

    #[test]
    fn normalized_injected_defaults_reuse_canonical_typed_artifact() {
        let (program, queries, _) = prepare(
            r#"
            interface I { fn value(self) -> i64 { return 42; } }
            class A implements I {}
            class B implements I {}
            fn main() { let a = new A(); let b = new B(); println(a.value() + b.value()); }
        "#,
            true,
        );
        let Item::Interface(interface) = &program.items[0] else {
            panic!()
        };
        let source = interface.methods[0].default_body.as_ref().unwrap().id;
        let source_record = queries.typed_record(source).unwrap();
        let mut instances = Vec::new();
        for item in &program.items {
            if let Item::Class(class) = item {
                let method = &class.methods[0];
                assert!(method.is_default_injected);
                assert_ne!(method.body.id, source);
                assert_eq!(queries.index.source_body(method.body.id), source);
                assert_eq!(
                    *queries.typed_record(method.body.id).unwrap(),
                    *source_record
                );
                instances.push(method.body.id);
            }
        }
        assert_eq!(instances.len(), 2);
        assert_ne!(instances[0], instances[1]);
        let typed_computations = queries.typed.stats().computations;
        assert_repeated_normalization(&program, &queries, 3);
        assert_eq!(queries.typed.stats().computations, typed_computations);
    }

    /// willow-afb5.17: lambdas in constructors, static initializers and each
    /// copy of an injected default get their own normalized records, and the
    /// walk-only split leaves bodies outside the normalization boundary as
    /// written.
    #[test]
    fn normalized_lambda_records_cover_every_root_kind() {
        let (program, queries, _) = prepare(
            r#"
            import std::collections::Map as Dict;
            interface I { fn twice(self, n: i64) -> i64 { let d = |x: i64| { let m: Dict<i64, i64> = Dict::new(); return x * 2; }; return d(n); } }
            class A implements I {}
            class B implements I {}
            class C {
                pub static f: fn(i64) -> i64 = |x: i64| x + 1;
                value: i64;
                pub init(self, v: i64) { let g = |x: i64| { let h = |y: i64| y + v; return h(x); }; self.value = g(1); }
            }
            fn main() { let c = new C(1); println(new A().twice(2) + new B().twice(3)); }
        "#,
            true,
        );
        let lambdas = queries.index.lambda_declarations(&program).unwrap().len();
        assert_eq!(lambdas, 5);
        // main + A.twice + B.twice + C.init + C.f + five lambda records.
        assert_repeated_normalization(&program, &queries, 5 + lambdas);
    }

    /// willow-afb5.17: a lambda nested `depth` deep is normalized and stored
    /// once, so records and stored bytes grow linearly with depth instead of
    /// each ancestor re-serializing every nested body.
    #[test]
    fn normalized_lambda_records_scale_linearly_with_nesting_depth() {
        let mut per_lambda = Vec::new();
        for depth in [4usize, 16, 64] {
            let mut body = "return x;".to_string();
            for level in (0..depth).rev() {
                body = format!(
                    "let f{level} = |x{level}: i64| {{ let x = x{level}; {body} }}; return f{level}(x);"
                );
            }
            let source = format!("fn f(x: i64) -> i64 {{ {body} }} fn main() {{ println(f(1)); }}");
            let (program, queries, _) = prepare(&source, false);
            let before = queries.store.written();
            queries.normalized_program(&program).unwrap();
            let bytes = queries.store.written() - before;
            assert_eq!(queries.normalized.stats().computations, 2 + depth);
            let (program, queries, _) = prepare(&source, false);
            assert_repeated_normalization(&program, &queries, 2 + depth);
            per_lambda.push(bytes / depth as u64);
        }
        // Constant bytes per lambda (identifier widths grow logarithmically).
        assert!(per_lambda[2] <= per_lambda[0] * 2, "{per_lambda:?}");
    }
}
