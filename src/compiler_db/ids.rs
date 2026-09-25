use std::collections::HashMap;

pub use crate::parser::ast::BodyId;
use crate::{
    module::UnitId,
    parser::{
        ast::*,
        iter::{AstEvent, AstWalk},
    },
    semantic::ids::{FunctionId, TypeId},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StaticId {
    pub unit: UnitId,
    pub owner: TypeId,
    pub index: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(source: &str) -> Program {
        let (program, errors) =
            crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
                .parse();
        assert!(errors.is_empty(), "{errors:?}");
        program
    }
    #[test]
    fn owners_lambdas_and_static_initializers_are_distinct_and_reused() {
        let program = parse(
            "fn f() { let a = |x: i64| { let b = |y: i64| y + x; return b(x); }; } class C { pub static n: i64 = 1; pub init(self) {} pub fn g(self) {} } interface I { fn g(self) {} }",
        );
        let mut index = BodyIndex::default();
        index.register_program(&program);
        assert_eq!(index.len(), 7);
        let calls = index.visits;
        for _ in 0..8 {
            index.register_program(&program);
        }
        assert_eq!(index.visits, calls);
        assert_eq!(index.lambdas.len(), 2);
        for (&expr, &id) in &index.lambdas {
            let (unit, owner) = index.owner(id).unwrap();
            assert_eq!(index.body(unit, owner), Some(id));
            assert_eq!(index.lambda(expr), Some(id));
            assert!(matches!(owner, BodyOwner::Lambda { .. }));
        }
        let (&expr, &static_id) = index.static_ids.iter().next().unwrap();
        assert_eq!(index.static_id(expr), Some(static_id));
        assert!(
            index
                .body(UnitId::ENTRY, BodyOwner::StaticInitializer(static_id))
                .is_some()
        );
    }

    #[test]
    fn instantiated_defaults_and_nested_lambdas_keep_contextual_identity() {
        let mut program = parse(
            "interface I { fn get(self) -> i64 { let outer = |x: i64| { let inner = |y: i64| y + x; return inner(x); }; return outer(1); } } class A { pub fn get(self) -> i64 { return 0; } } class B { pub fn get(self) -> i64 { return 0; } }",
        );
        let mut index = BodyIndex::default();
        index.register_program(&program);
        let Item::Interface(interface) = &program.items[0] else {
            panic!()
        };
        let source = interface.methods[0].default_body.as_ref().unwrap().clone();
        let expressions: Vec<_> = AstWalk::new(AstEvent::Block(&source))
            .filter_map(|event| match event {
                AstEvent::Expr(Expr::Lambda(lambda)) => Some(lambda.id),
                _ => None,
            })
            .collect();
        assert_eq!(expressions.len(), 2);
        let source_outer = index.lambda_in(source.id, expressions[0]).unwrap();
        let source_inner = index.lambda_in(source_outer, expressions[1]).unwrap();
        // Desugaring copies syntax and ExprIds, not semantic owner identity.
        for item in &mut program.items[1..] {
            let Item::Class(class) = item else { panic!() };
            class.methods[0].body = source.clone();
        }
        // Use a separate concrete unit, as for an imported default template.
        let unit = crate::module::ModuleId(7);
        let unbound = program.clone();
        index.register_unit(&mut program, unit);
        let ids: Vec<_> = program.items[1..]
            .iter()
            .map(|item| {
                let Item::Class(class) = item else { panic!() };
                let body = class.methods[0].body.id;
                let owner =
                    BodyOwner::Function(FunctionId::method(TypeId::local(&class.name), "get"));
                assert_eq!(index.owner(body), Some((unit, owner)));
                assert_eq!(index.body(unit, owner), Some(body));
                assert_eq!(index.source_body(body), source.id);
                let outer = index.lambda_in(body, expressions[0]).unwrap();
                let inner = index.lambda_in(outer, expressions[1]).unwrap();
                assert_eq!(index.source_body(outer), source_outer);
                assert_eq!(index.source_body(inner), source_inner);
                assert_eq!(
                    index.owner(outer),
                    Some((
                        unit,
                        BodyOwner::Lambda {
                            parent: body,
                            expr: expressions[0]
                        }
                    ))
                );
                assert_eq!(
                    index.owner(inner),
                    Some((
                        unit,
                        BodyOwner::Lambda {
                            parent: outer,
                            expr: expressions[1]
                        }
                    ))
                );
                [body, outer, inner]
            })
            .collect();
        for (left, right) in ids[0].iter().zip(&ids[1]) {
            assert_ne!(left, right);
        }
        assert_ne!(ids[0][0], source.id);
        assert_ne!(ids[0][1], source_outer);
        assert_ne!(ids[0][2], source_inner);
        let count = index.len();
        let visits = index.visits;
        let bound = serde_json::to_vec(&program).unwrap();
        for _ in 0..4 {
            index.register_unit(&mut program, unit);
            assert_eq!(serde_json::to_vec(&program).unwrap(), bound);
            let mut fresh_shell = unbound.clone();
            index.register_unit(&mut fresh_shell, unit);
            assert_eq!(serde_json::to_vec(&fresh_shell).unwrap(), bound);
        }
        assert_eq!(index.len(), count);
        assert_eq!(index.visits, visits);
    }

    #[test]
    fn indexing_work_is_linear_in_distinct_syntax_not_query_count() {
        let mut previous = None;
        for size in [16, 64, 256, 1024] {
            let source: String = (0..size)
                .map(|i| format!("fn body_{i}() {{ let f = |x: i64| x + 1; }}\n"))
                .collect();
            let program = parse(&source);
            let mut index = BodyIndex::default();
            index.register_program(&program);
            assert_eq!(index.len(), size * 2);
            assert_eq!(index.visits % size, 0);
            let per_body = index.visits / size;
            if let Some(previous) = previous {
                assert_eq!(per_body, previous);
            }
            previous = Some(per_body);
            let visits = index.visits;
            for _ in 0..4 {
                index.register_program(&program);
            }
            assert_eq!(index.visits, visits);
            eprintln!(
                "body-index size={size} identities={} visits={visits}",
                index.len()
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyOwner {
    Function(FunctionId),
    Constructor { owner: TypeId, ordinal: u32 },
    InterfaceDefault(FunctionId),
    Lambda { parent: BodyId, expr: ExprId },
    StaticInitializer(StaticId),
}

#[derive(Debug, Default)]
pub struct BodyIndex {
    owners: HashMap<BodyId, (UnitId, BodyOwner)>,
    bodies: HashMap<(UnitId, BodyOwner), BodyId>,
    lambdas: HashMap<ExprId, BodyId>,
    static_ids: HashMap<ExprId, StaticId>,
    origins: HashMap<BodyId, BodyId>,
    children: HashMap<BodyId, Vec<BodyId>>,
    #[cfg(test)]
    visits: usize,
}

impl BodyIndex {
    pub fn owner(&self, body: BodyId) -> Option<(UnitId, BodyOwner)> {
        self.owners.get(&body).copied()
    }
    pub fn body(&self, unit: UnitId, owner: BodyOwner) -> Option<BodyId> {
        self.bodies.get(&(unit, owner)).copied()
    }
    pub fn lambda(&self, expr: ExprId) -> Option<BodyId> {
        self.lambdas.get(&expr).copied()
    }
    pub fn lambda_in(&self, parent: BodyId, expr: ExprId) -> Option<BodyId> {
        let (unit, _) = self.owner(parent)?;
        self.body(unit, BodyOwner::Lambda { parent, expr })
    }
    /// Instantiated defaults have distinct semantic identities while sharing
    /// the immutable source payload, including source diagnostic coordinates.
    pub(crate) fn child_lambdas(
        &self,
        body: BodyId,
    ) -> impl Iterator<Item = (ExprId, BodyId)> + '_ {
        self.children
            .get(&body)
            .into_iter()
            .flatten()
            .filter_map(|&id| match self.owner(id) {
                Some((_, BodyOwner::Lambda { expr, .. })) => Some((expr, id)),
                _ => None,
            })
    }

    /// Postorder inventory matching source declaration traversal. Unlike an
    /// ExprId map, it distinguishes copies of an injected interface default.
    pub(crate) fn lambda_declarations(&self, program: &Program) -> anyhow::Result<Vec<BodyId>> {
        use anyhow::Context;
        let mut roots = Vec::new();
        for item in &program.items {
            match item {
                Item::Function(f) => roots.push((f.body.id, AstEvent::Block(&f.body))),
                Item::Class(c) => {
                    roots.extend(
                        c.methods
                            .iter()
                            .map(|m| (m.body.id, AstEvent::Block(&m.body))),
                    );
                    roots.extend(
                        c.constructors
                            .iter()
                            .map(|c| (c.body.id, AstEvent::Block(&c.body))),
                    );
                    for field in &c.fields {
                        if let Some(expr) = &field.initializer {
                            let id = self
                                .static_id(expr.id())
                                .context("missing initializer identity")?;
                            let body = self
                                .body(id.unit, BodyOwner::StaticInitializer(id))
                                .context("missing initializer body")?;
                            roots.push((body, AstEvent::Expr(expr)));
                        }
                    }
                }
                _ => {}
            }
        }
        let mut result = Vec::new();
        for (root, event) in roots {
            let mut parents = vec![root];
            for event in AstWalk::new(event) {
                match event {
                    AstEvent::Expr(Expr::Lambda(lambda)) => {
                        let id = self
                            .lambda_in(*parents.last().unwrap(), lambda.id)
                            .context("missing contextual lambda identity")?;
                        parents.push(id);
                    }
                    AstEvent::ExitExpr(Expr::Lambda(_)) => result.push(parents.pop().unwrap()),
                    _ => {}
                }
            }
        }
        Ok(result)
    }

    pub fn source_body(&self, body: BodyId) -> BodyId {
        self.origins.get(&body).copied().unwrap_or(body)
    }
    pub fn static_id(&self, expr: ExprId) -> Option<StaticId> {
        self.static_ids.get(&expr).copied()
    }
    pub fn len(&self) -> usize {
        self.owners.len()
    }
    #[cfg(test)]
    pub(crate) fn ids(&self) -> impl Iterator<Item = BodyId> + '_ {
        self.owners.keys().copied()
    }
    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    fn register(&mut self, id: BodyId, unit: UnitId, owner: BodyOwner) -> bool {
        if self.owners.contains_key(&id) {
            return false;
        }
        self.owners.insert(id, (unit, owner));
        self.bodies.insert((unit, owner), id);
        if let BodyOwner::Lambda { parent, .. } = owner {
            self.children.entry(parent).or_default().push(id);
        }
        true
    }

    fn nested(&mut self, unit: UnitId, parent: BodyId, root: AstEvent<'_>) {
        let mut parents = vec![parent];
        for event in AstWalk::new(root) {
            #[cfg(test)]
            {
                self.visits += 1;
            }
            match event {
                AstEvent::Expr(Expr::Lambda(lambda)) => {
                    let id = *self.lambdas.entry(lambda.id).or_insert_with(BodyId::fresh);
                    self.register(
                        id,
                        unit,
                        BodyOwner::Lambda {
                            parent: *parents.last().unwrap(),
                            expr: lambda.id,
                        },
                    );
                    parents.push(id);
                }
                AstEvent::ExitExpr(Expr::Lambda(_)) => {
                    parents.pop();
                }
                _ => {}
            }
        }
    }

    fn block(&mut self, block: &Block, owner: BodyOwner) {
        let unit = crate::module::ModuleId(block.span.file_id.0);
        if self.register(block.id, unit, owner) {
            self.nested(unit, block.id, AstEvent::Block(block));
        }
    }

    fn instantiate_block(&mut self, block: &mut Block, unit: UnitId, owner: BodyOwner) {
        if self.owner(block.id) == Some((unit, owner)) {
            return;
        }
        if let Some(id) = self.body(unit, owner) {
            block.id = id;
            return;
        }
        let source = self.source_body(block.id);
        let id = BodyId::fresh();
        self.register(id, unit, owner);
        self.origins.insert(id, source);
        block.id = id;
        // Copy only the ownership skeleton, not executable syntax. Each
        // nested lambda receives the concrete parent's semantic identity.
        let mut pending = vec![(source, id)];
        while let Some((source, instance)) = pending.pop() {
            let children = self.children.get(&source).cloned().unwrap_or_default();
            for child in children {
                let Some((_, BodyOwner::Lambda { expr, .. })) = self.owner(child) else {
                    continue;
                };
                let id = BodyId::fresh();
                self.register(
                    id,
                    unit,
                    BodyOwner::Lambda {
                        parent: instance,
                        expr,
                    },
                );
                self.origins.insert(id, self.source_body(child));
                pending.push((child, id));
            }
        }
    }

    /// Rebind desugared declaration shells to their concrete owner. A copied
    /// default's source BodyId alone cannot key type-dependent query results.
    pub(crate) fn register_unit(&mut self, program: &mut Program, unit: UnitId) {
        for item in &mut program.items {
            match item {
                Item::Function(f) => self.instantiate_block(
                    &mut f.body,
                    unit,
                    BodyOwner::Function(FunctionId::free(&f.name)),
                ),
                Item::Class(c) => {
                    let owner = TypeId::local(&c.name);
                    for m in &mut c.methods {
                        self.instantiate_block(
                            &mut m.body,
                            unit,
                            BodyOwner::Function(FunctionId::method(owner, &m.name)),
                        );
                    }
                    for (ordinal, constructor) in c.constructors.iter_mut().enumerate() {
                        self.instantiate_block(
                            &mut constructor.body,
                            unit,
                            BodyOwner::Constructor {
                                owner,
                                ordinal: u32::try_from(ordinal)
                                    .expect("constructor index overflow"),
                            },
                        );
                    }
                }
                Item::Interface(interface) => {
                    for method in &mut interface.methods {
                        if let Some(body) = &mut method.default_body {
                            self.instantiate_block(
                                body,
                                unit,
                                BodyOwner::InterfaceDefault(FunctionId::method(
                                    TypeId::local(&interface.name),
                                    &method.name,
                                )),
                            );
                        }
                    }
                }
                Item::Enum(_) => {}
            }
        }
    }

    /// Called while source bodies are resident, before they are stripped. Each
    /// immutable source body is indexed once, including shared interface defaults.
    pub(crate) fn register_program(&mut self, program: &Program) {
        for item in &program.items {
            match item {
                Item::Function(f) => {
                    self.block(&f.body, BodyOwner::Function(FunctionId::free(&f.name)))
                }
                Item::Class(c) => {
                    let owner = TypeId::local(&c.name);
                    for m in &c.methods {
                        self.block(
                            &m.body,
                            BodyOwner::Function(FunctionId::method(owner, &m.name)),
                        );
                    }
                    for (i, c) in c.constructors.iter().enumerate() {
                        self.block(
                            &c.body,
                            BodyOwner::Constructor {
                                owner,
                                ordinal: u32::try_from(i).expect("constructor index overflow"),
                            },
                        );
                    }
                    for (i, field) in c.fields.iter().enumerate() {
                        if let Some(expr) = &field.initializer {
                            let unit = crate::module::ModuleId(expr.span().file_id.0);
                            let id = StaticId {
                                unit,
                                owner,
                                index: u32::try_from(i).expect("field index overflow"),
                            };
                            if self.static_ids.insert(expr.id(), id).is_none() {
                                let body = BodyId::fresh();
                                self.register(body, unit, BodyOwner::StaticInitializer(id));
                                self.nested(unit, body, AstEvent::Expr(expr));
                            }
                        }
                    }
                }
                Item::Interface(i) => {
                    for m in &i.methods {
                        if let Some(body) = &m.default_body {
                            self.block(
                                body,
                                BodyOwner::InterfaceDefault(FunctionId::method(
                                    TypeId::local(&i.name),
                                    &m.name,
                                )),
                            );
                        }
                    }
                }
                Item::Enum(_) => {}
            }
        }
    }
}
