use super::*;

/// Ephemeral callable state. A root body starts empty; nested lambda checking
/// keeps this same lexical/inference state until independent lambda inputs exist.
/// Immutable declarations and unit orchestration are deliberately absent.
pub(super) struct BodyState {
    /// Nesting depth of enclosing loops; `break`/`continue` outside a loop is
    /// E0904. Reset to 0 inside a lambda body (a loop outside the lambda is
    /// not breakable from within it) (willow-kzka).
    pub(crate) loop_depth: u32,
    /// Nesting depth of enclosing `lock` statements. V1 rejects a `lock` inside
    /// another lock's critical section (E2605), and the depth also keeps the
    /// await scan from reporting the same `await` once per enclosing lock
    /// (willow-38w.1.1). Reset inside a lambda body, which only gets
    /// CONSTRUCTED in the section and holds nothing when it runs (willow-3kty).
    pub(crate) lock_depth: u32,
    /// Lexical statement-block depth within the current function-like body.
    /// The outer function/method body is depth 1. Reset for lambdas so recovery
    /// capability cannot cross a function boundary (willow-s9ej.3).
    pub(crate) lexical_block_depth: u32,
    /// Types carried by the current callable's async locals and suspension
    /// temporaries. Each occurrence has its own slot, independent of source
    /// spans; callable checking drains its slots after checking Send.
    pub(super) async_local_types: Vec<Type>,
    /// The type parameters of the generic declaration whose written types are
    /// being normalized or validated (`T` inside `enum Wrap<T>` or `interface Conv<T>`).
    /// A bare `T` there names a parameter, not a missing type, so
    /// [`Self::validate_type`] accepts it; every other position sees an empty
    /// list and reports the name as unknown (willow-rlq9).
    pub(super) declared_type_params: Vec<String>,
    /// Unannotated map constructors and uses awaiting their first insertion.
    pub(super) inferred_maps: HashMap<Span, (Vec<ExprId>, Option<usize>)>,
    /// Every use recorded in `inferred_maps`, for the lambda boundary: a use
    /// typed before its map is resolved holds a placeholder (willow-afb5.17).
    pub(super) pending_map_uses: HashSet<ExprId>,
    /// Assignment statements already reported as writes to a capture
    /// (willow-0g8j.2.12). The lambda body is checked again afterwards, where
    /// the target still resolves to the enclosing function's immutable local;
    /// without this the same statement would also be reported as an ordinary
    /// mutability error, which points the reader at the wrong fix.
    pub(super) capture_writes: HashSet<Span>,
    /// The type a `match` in value position flows into, set for the duration of
    /// that one `match` by [`TypeChecker::check_expr_expecting`] and taken by
    /// [`TypeChecker::check_match_expr`] (willow-0g8j.3).
    pub(super) match_expected: Option<Type>,
    pub(super) current_return_type: Type,
    /// Stack of lambda return types being inferred. When non-empty, `return` stmts
    /// record their type here instead of checking against `current_return_type`.
    pub(super) lambda_return_stack: Vec<Option<Type>>,
    pub(super) current_class: Option<String>,
    /// Whether the body being checked is an `async fn` (or async method), i.e.
    /// whether it has a task frame to suspend into. Cleared inside a lambda
    /// body: a lambda has no `async` form and the backend lifts it into a plain
    /// private function, so `lock` and `await` written there are E2603/E0801
    /// however the enclosing function was declared (willow-3kty).
    pub(super) current_async_context: bool,
    /// Set while checking a `static fn` body — `self` is unavailable there
    /// (willow-qsqf §9.2 → E0831).
    pub(super) in_static_method: bool,
    /// Set while checking a `static` property initializer — `self` is unavailable
    /// there (willow-qsqf §10.3 → E0837).
    pub(super) in_static_initializer: bool,
    /// Set while checking an `init(...)` constructor body — `return <value>` is
    /// rejected (willow-scq2 §8 → E0841).
    pub(super) in_constructor: bool,
    /// The function-like body currently being checked. A lambda is its own
    /// callable: body queries provide its contextual identity; standalone
    /// block lambdas use their block identity. Neither charges the parent
    /// with the lambda's calls or waits.
    pub(super) current_effect_callable: Option<FunctionId>,
    /// The body query being evaluated, so a lambda expression can resolve to
    /// its contextual body identity (`BodyOwner::Lambda { parent, expr }`).
    pub(super) current_body: Option<BodyId>,
}

impl Default for BodyState {
    fn default() -> Self {
        Self {
            loop_depth: 0,
            lock_depth: 0,
            lexical_block_depth: 0,
            async_local_types: Vec::new(),
            declared_type_params: Vec::new(),
            inferred_maps: HashMap::new(),
            pending_map_uses: HashSet::new(),
            capture_writes: HashSet::new(),
            match_expected: None,
            current_return_type: Type::Void,
            lambda_return_stack: Vec::new(),
            current_class: None,
            current_async_context: false,
            in_static_method: false,
            in_static_initializer: false,
            in_constructor: false,
            current_effect_callable: None,
            current_body: None,
        }
    }
}

/// Where one lambda's own outputs join its parent's (willow-afb5.17): the
/// parent's own diagnostic and task-site counts when the lambda was checked.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct LambdaSplice {
    body: BodyId,
    errors: usize,
    task_sites: usize,
}

/// The lambdas checked inside the body being evaluated. Each one's outputs are
/// stored under its own `typed_body` key and referenced from here, so a nested
/// lambda's tables live in exactly one artifact.
#[derive(Default)]
pub(super) struct LambdaChildren {
    splices: Vec<LambdaSplice>,
    /// Fresh child results, in splice order, so the first absorption of the
    /// root does not read back what it has just written.
    resident: Vec<TypedBody>,
    /// Diagnostics reported by the children and their descendants.
    nested_errors: usize,
}

/// The tables one body writes. Swapped out around a lambda so the lambda's
/// outputs start empty and can be stored under the lambda's own key.
#[derive(Default)]
pub(super) struct BodyOutputs {
    errors: Vec<Diagnostic>,
    expr_types: HashMap<ExprId, Type>,
    reference_arg_modes: HashMap<ExprId, ParamMode>,
    enum_variant_resolutions: HashMap<ExprId, String>,
    pattern_resolutions: HashMap<PatternId, Pattern>,
    normalized_types: HashMap<Type, Type>,
    static_call_classes: HashMap<ExprId, String>,
    lambda_captures: HashMap<ExprId, Vec<LambdaCapture>>,
    lock_edges: HashMap<FunctionId, HashSet<FunctionId>>,
    resolved_calls: HashMap<FunctionId, crate::semantic::call_graph::CallSites>,
    analysis_calls: HashMap<ExprId, Option<FunctionId>>,
    analysis_symbols: crate::semantic::analysis_symbols::Facts,
    task_method_calls: Vec<crate::compiler_db::effects::TaskMethodCall>,
    lock_direct: HashMap<FunctionId, LockEffectCause>,
    lock_direct_sites: Vec<LockEffectCause>,
    lock_sites: Vec<LockEffectCallsite>,
    lambdas: LambdaChildren,
}

/// Body-local query output. Declaration metadata is shared by the evaluator,
/// never copied into each body artifact. A lambda's outputs are its own
/// artifact; the parent keeps only the splice points (willow-afb5.17).
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct TypedBody {
    id: BodyId,
    diagnostics: Vec<Diagnostic>,
    expr_types: HashMap<ExprId, Type>,
    reference_arg_modes: HashMap<ExprId, ParamMode>,
    enum_variant_resolutions: HashMap<ExprId, String>,
    pattern_resolutions: HashMap<PatternId, Pattern>,
    #[serde(with = "crate::compiler_db::map_entries")]
    normalized_types: HashMap<Type, Type>,
    static_call_classes: HashMap<ExprId, String>,
    lambda_captures: HashMap<ExprId, Vec<LambdaCapture>>,
    #[serde(with = "crate::compiler_db::map_entries")]
    lock_edges: HashMap<FunctionId, HashSet<FunctionId>>,
    #[serde(with = "crate::compiler_db::map_entries")]
    resolved_calls: HashMap<FunctionId, crate::semantic::call_graph::CallSites>,
    analysis_calls: HashMap<ExprId, Option<FunctionId>>,
    analysis_symbols: crate::semantic::analysis_symbols::Facts,
    task_method_calls: Vec<crate::compiler_db::effects::TaskMethodCall>,
    #[serde(with = "crate::compiler_db::map_entries")]
    lock_direct: HashMap<FunctionId, LockEffectCause>,
    lock_direct_sites: Vec<LockEffectCause>,
    lock_sites: Vec<LockEffectCallsite>,
    collection_names: HashSet<String>,
    missing_collections: HashSet<String>,
    lambdas: Vec<LambdaSplice>,
    nested_errors: usize,
    #[serde(skip)]
    resident: Vec<TypedBody>,
}

impl TypedBody {
    fn new(
        id: BodyId,
        outputs: BodyOutputs,
        collection_names: HashSet<String>,
        missing_collections: HashSet<String>,
    ) -> Self {
        Self {
            id,
            diagnostics: outputs.errors,
            expr_types: outputs.expr_types,
            reference_arg_modes: outputs.reference_arg_modes,
            enum_variant_resolutions: outputs.enum_variant_resolutions,
            pattern_resolutions: outputs.pattern_resolutions,
            normalized_types: outputs.normalized_types,
            static_call_classes: outputs.static_call_classes,
            lambda_captures: outputs.lambda_captures,
            lock_edges: outputs.lock_edges,
            resolved_calls: outputs.resolved_calls,
            analysis_calls: outputs.analysis_calls,
            analysis_symbols: outputs.analysis_symbols,
            task_method_calls: outputs.task_method_calls,
            lock_direct: outputs.lock_direct,
            lock_direct_sites: outputs.lock_direct_sites,
            lock_sites: outputs.lock_sites,
            collection_names,
            missing_collections,
            lambdas: outputs.lambdas.splices,
            nested_errors: outputs.lambdas.nested_errors,
            resident: outputs.lambdas.resident,
        }
    }

    /// Merge this body's tables and then its lambdas' in preorder, the order
    /// in which inline checking last wrote each key.
    pub(crate) fn merge_tables(
        self,
        unit: &mut crate::compiler_db::CheckedUnit,
        queries: &crate::compiler_db::body::BodyQueries,
    ) -> anyhow::Result<()> {
        unit.expr_types.extend(self.expr_types);
        unit.reference_arg_modes.extend(self.reference_arg_modes);
        unit.enum_variant_resolutions
            .extend(self.enum_variant_resolutions);
        unit.pattern_resolutions.extend(self.pattern_resolutions);
        unit.static_call_classes.extend(self.static_call_classes);
        unit.lambda_captures.extend(self.lambda_captures);
        for splice in self.lambdas {
            queries.read(splice.body)?.merge_tables(unit, queries)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn lambda_bodies(&self) -> impl Iterator<Item = BodyId> + '_ {
        self.lambdas.iter().map(|splice| splice.body)
    }
}

struct BodyChecker {
    id: BodyId,
    state: TypeChecker,
}

impl BodyChecker {
    fn new(id: BodyId, declarations: &TypeChecker) -> Self {
        let mut state = declarations.fork_body();
        state.body_queries = declarations.body_queries.clone();
        state.local.current_body = Some(id);
        Self { id, state }
    }

    fn finish(self) -> TypedBody {
        let mut state = self.state;
        let mut outputs = BodyOutputs::default();
        state.swap_body_outputs(&mut outputs);
        TypedBody::new(
            self.id,
            outputs,
            state.fully_qualified_collection_types,
            state.missing_collection_imports_reported,
        )
    }
}

impl TypeChecker {
    /// Start a body evaluator over this checker's frozen declarations. Sharing
    /// the resolution context and declaration symbols is O(1); only the
    /// per-unit diagnostic deduplication sets are copied, and those are
    /// bounded by the handful of std collection names.
    pub(super) fn fork_body(&self) -> TypeChecker {
        let mut body = TypeChecker::empty(
            std::rc::Rc::clone(&self.resolution),
            self.symbols.fork_body_scope(),
        );
        body.capture_call_sites = self.capture_call_sites;
        body.fully_qualified_collection_types = self.fully_qualified_collection_types.clone();
        body.missing_collection_imports_reported = self.missing_collection_imports_reported.clone();
        body
    }

    pub(crate) fn set_body_queries(
        &mut self,
        queries: std::rc::Rc<crate::compiler_db::body::BodyQueries>,
    ) {
        self.body_queries = Some(queries);
    }

    pub(crate) fn finish_body_queries(&mut self) -> anyhow::Result<()> {
        match self.body_query_error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Evaluate one body against shared declarations, then aggregate its owned
    /// outputs in source order. Repeated requests replay the immutable artifact.
    pub(super) fn query_body(&mut self, id: BodyId, check: impl FnOnce(&mut TypeChecker)) {
        let Some(queries) = self.body_queries.clone() else {
            check(self);
            return;
        };
        if self.body_query_error.is_some() {
            return;
        }
        if let Err(error) = queries.validate_candidate(id, &self.symbols) {
            self.body_query_error = Some(error);
            return;
        }
        let result = queries.evaluate(id, || {
            let mut body = BodyChecker::new(id, self);
            check(&mut body.state);
            body.state.finish_body_queries()?;
            Ok(body.finish())
        });
        let body = match result {
            Ok(body) => body,
            Err(error) => {
                self.body_query_error = Some(error);
                return;
            }
        };
        queries.snapshot_dependencies(id, &self.symbols);
        if let Err(error) = queries.publish_tracked(id, &self.symbols) {
            self.body_query_error = Some(error);
            return;
        }
        self.checked_bodies.push(id);
        if let Err(error) = self.absorb_body(body, &queries) {
            self.body_query_error = Some(error);
        }
    }

    /// Merge one evaluated body into this checker: its own tables first, then
    /// each lambda in preorder, with diagnostics and task-method sites
    /// interleaved exactly as inline checking produced them.
    fn absorb_body(
        &mut self,
        body: TypedBody,
        queries: &crate::compiler_db::body::BodyQueries,
    ) -> anyhow::Result<()> {
        for (caller, sites) in body.resolved_calls {
            let current = self.resolved_calls.entry(caller).or_default();
            current.targets.extend(sites.targets);
            current.has_unknown |= sites.has_unknown;
        }
        self.expr_types.extend(body.expr_types);
        self.analysis_calls.extend(body.analysis_calls);
        self.analysis_symbols
            .extend(queries.resolved_references(body.id, &body.analysis_symbols)?);
        self.reference_arg_modes.extend(body.reference_arg_modes);
        self.enum_variant_resolutions
            .extend(body.enum_variant_resolutions);
        self.pattern_resolutions.extend(body.pattern_resolutions);
        self.normalized_types.extend(body.normalized_types);
        self.static_call_classes.extend(body.static_call_classes);
        self.lambda_captures.extend(body.lambda_captures);
        for (caller, targets) in body.lock_edges {
            self.effect_inputs
                .edges
                .entry(caller)
                .or_default()
                .extend(targets);
        }
        for (caller, cause) in body.lock_direct {
            self.effect_inputs.direct.entry(caller).or_insert(cause);
        }
        self.effect_inputs
            .direct_sites
            .extend(body.lock_direct_sites);
        self.effect_inputs.sites.extend(body.lock_sites);
        self.fully_qualified_collection_types
            .extend(body.collection_names);
        self.missing_collection_imports_reported
            .extend(body.missing_collections);

        let mut ordered = OrderedOutputs {
            diagnostics: body.diagnostics.into_iter(),
            sites: body.task_method_calls.into_iter().peekable(),
            errors: 0,
            taken: 0,
        };
        let mut resident = body.resident.into_iter();
        for splice in body.lambdas {
            ordered.absorb_until(self, splice.errors, splice.task_sites);
            let child = match resident.next() {
                Some(child) => child,
                None => queries.read(splice.body)?,
            };
            self.absorb_body(child, queries)?;
        }
        ordered.absorb_until(self, usize::MAX, usize::MAX);
        Ok(())
    }

    fn swap_body_outputs(&mut self, outputs: &mut BodyOutputs) {
        use std::mem::swap;
        swap(&mut self.errors, &mut outputs.errors);
        swap(&mut self.expr_types, &mut outputs.expr_types);
        swap(
            &mut self.reference_arg_modes,
            &mut outputs.reference_arg_modes,
        );
        swap(
            &mut self.enum_variant_resolutions,
            &mut outputs.enum_variant_resolutions,
        );
        swap(
            &mut self.pattern_resolutions,
            &mut outputs.pattern_resolutions,
        );
        swap(&mut self.normalized_types, &mut outputs.normalized_types);
        swap(
            &mut self.static_call_classes,
            &mut outputs.static_call_classes,
        );
        swap(&mut self.lambda_captures, &mut outputs.lambda_captures);
        swap(&mut self.effect_inputs.edges, &mut outputs.lock_edges);
        swap(&mut self.resolved_calls, &mut outputs.resolved_calls);
        swap(&mut self.analysis_calls, &mut outputs.analysis_calls);
        swap(&mut self.analysis_symbols, &mut outputs.analysis_symbols);
        swap(&mut self.task_method_calls, &mut outputs.task_method_calls);
        swap(&mut self.effect_inputs.direct, &mut outputs.lock_direct);
        swap(
            &mut self.effect_inputs.direct_sites,
            &mut outputs.lock_direct_sites,
        );
        swap(&mut self.effect_inputs.sites, &mut outputs.lock_sites);
        swap(&mut self.lambdas, &mut outputs.lambdas);
    }

    /// Diagnostics this body has reported so far, its lambdas' included. A
    /// lambda's diagnostics live in its own artifact rather than in `errors`.
    pub(super) fn error_count(&self) -> usize {
        self.errors.len() + self.lambdas.nested_errors
    }

    /// Check a lambda body as its own `typed_body` query (willow-afb5.17).
    ///
    /// The lambda is typed where it is written, against the enclosing body's
    /// visible locals and inference state, but everything it outputs goes to
    /// fresh tables that are stored under the lambda's `BodyId`. The parent
    /// records only where the lambda's diagnostics fall among its own.
    ///
    /// Returns `None` when the lambda has no independent identity (no body
    /// queries, or no parent body) or was already evaluated, in which case the
    /// caller checks it inline exactly as before.
    pub(super) fn check_lambda_body<T, F: FnOnce(&mut TypeChecker) -> T>(
        &mut self,
        l: &LambdaExpr,
        body: Option<BodyId>,
        check: F,
    ) -> Result<T, F> {
        let (Some(id), Some(queries)) = (body, self.body_queries.clone()) else {
            return Err(check);
        };
        if queries.is_typed(id) {
            #[cfg(test)]
            {
                self.inline_lambda_rechecks += 1;
            }
            return Err(check);
        }
        let mut saved = BodyOutputs::default();
        self.swap_body_outputs(&mut saved);
        let mut check = Some(check);
        let mut value = None;
        let mut lambda_ty = None;
        let result = queries.evaluate(id, || {
            value = Some((check.take().expect("typed_body computes once"))(self));
            let mut outputs = BodyOutputs::default();
            self.swap_body_outputs(&mut outputs);
            // The lambda's own type is an entry of the ENCLOSING body: that
            // body writes it again after this returns, possibly with a
            // different type.
            lambda_ty = outputs.expr_types.remove(&l.id);
            // A use of a map still awaiting its first insertion carries a
            // placeholder type. Whichever body resolves the map writes the
            // final type for every use, so the placeholder must not reach the
            // stored record, which checked_unit merges after its parent's.
            let pending = &self.local.pending_map_uses;
            if !pending.is_empty() {
                outputs.expr_types.retain(|id, _| !pending.contains(id));
            }
            Ok(TypedBody::new(id, outputs, HashSet::new(), HashSet::new()))
        });
        // Whatever the query did, the parent's tables come back.
        self.swap_body_outputs(&mut saved);
        let (child, value) = match (result, value) {
            (Ok(child), Some(value)) => (child, value),
            (result, value) => {
                if let Err(error) = result {
                    self.body_query_error.get_or_insert(error);
                }
                return match (value, check) {
                    (Some(value), _) => Ok(value),
                    (None, Some(check)) => Err(check),
                    (None, None) => unreachable!("a taken check produced a value"),
                };
            }
        };
        if let Some(ty) = lambda_ty {
            self.expr_types.insert(l.id, ty);
        }
        self.lambdas.nested_errors += child.diagnostics.len() + child.nested_errors;
        self.lambdas.splices.push(LambdaSplice {
            body: id,
            errors: self.errors.len(),
            task_sites: self.task_method_calls.len(),
        });
        self.lambdas.resident.push(child);
        Ok(value)
    }
}

/// A body's own diagnostics and task-method sites, consumed in step with the
/// splice points of its lambdas.
struct OrderedOutputs {
    diagnostics: std::vec::IntoIter<Diagnostic>,
    sites: std::iter::Peekable<std::vec::IntoIter<crate::compiler_db::effects::TaskMethodCall>>,
    errors: usize,
    taken: usize,
}

impl OrderedOutputs {
    /// A site recorded when the body had reported `n` diagnostics precedes the
    /// body's diagnostic `n`, and is re-based on the merged list.
    fn absorb_until(&mut self, checker: &mut TypeChecker, errors: usize, sites: usize) {
        loop {
            if self.taken < sites
                && self
                    .sites
                    .peek()
                    .is_some_and(|site| site.diagnostic_index <= self.errors)
            {
                let mut site = self.sites.next().unwrap();
                site.diagnostic_index = checker.errors.len();
                checker.task_method_calls.push(site);
                self.taken += 1;
            } else if self.errors < errors
                && let Some(diagnostic) = self.diagnostics.next()
            {
                checker.errors.push(diagnostic);
                self.errors += 1;
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler_db::{body::BodyQueries, ids::BodyIndex};
    use crate::module::{UnitId, artifacts::UnitArtifacts};
    use std::{fmt::Debug, hash::Hash, rc::Rc};

    fn parse(source: &str) -> Program {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}\n{source}");
        program
    }

    fn queries(program: &mut Program) -> Rc<BodyQueries> {
        let artifacts = UnitArtifacts::new().unwrap();
        let mut index = BodyIndex::default();
        index.register_program(program);
        index.register_unit(program, UnitId::ENTRY);
        Rc::new(BodyQueries::new(
            Rc::clone(&artifacts.store),
            Rc::new(index),
        ))
    }

    fn assert_map_eq<K: Eq + Hash + Debug, V: serde::Serialize>(
        actual: &HashMap<K, V>,
        expected: &HashMap<K, V>,
        name: &str,
    ) {
        assert_eq!(actual.len(), expected.len(), "{name} length");
        for (key, value) in expected {
            let found = actual
                .get(key)
                .unwrap_or_else(|| panic!("{name} missing {key:?}"));
            assert_eq!(
                serde_json::to_value(found).unwrap(),
                serde_json::to_value(value).unwrap(),
                "{name} at {key:?}"
            );
        }
    }

    fn resolved_pattern_value(pattern: &Pattern) -> serde_json::Value {
        fn remove_synthetic_ids(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(fields) => {
                    fields.remove("id");
                    for value in fields.values_mut() {
                        remove_synthetic_ids(value);
                    }
                }
                serde_json::Value::Array(values) => {
                    for value in values {
                        remove_synthetic_ids(value);
                    }
                }
                _ => {}
            }
        }
        // Resolving a binding spelling to an enum variant allocates fresh
        // pattern IDs in each checker; source table keys must still match.
        let mut value = serde_json::to_value(pattern).unwrap();
        remove_synthetic_ids(&mut value);
        value
    }

    fn compare(source: &str) -> (TypeChecker, Rc<BodyQueries>, Program) {
        let mut program = parse(source);
        let queries = queries(&mut program);
        let mut standalone = TypeChecker::new();
        standalone.check_program(&program);
        let mut cached = TypeChecker::new();
        cached.set_body_queries(Rc::clone(&queries));
        cached.check_program(&program);
        cached.finish_body_queries().unwrap();
        assert_eq!(
            serde_json::to_value(&cached.errors).unwrap(),
            serde_json::to_value(&standalone.errors).unwrap(),
            "ordered diagnostics"
        );
        assert_map_eq(&cached.expr_types, &standalone.expr_types, "expr_types");
        assert_map_eq(
            &cached.reference_arg_modes,
            &standalone.reference_arg_modes,
            "reference_arg_modes",
        );
        assert_map_eq(
            &cached.enum_variant_resolutions,
            &standalone.enum_variant_resolutions,
            "enum_variant_resolutions",
        );
        assert_eq!(
            cached.pattern_resolutions.len(),
            standalone.pattern_resolutions.len()
        );
        for (id, pattern) in &standalone.pattern_resolutions {
            assert_eq!(
                resolved_pattern_value(&cached.pattern_resolutions[id]),
                resolved_pattern_value(pattern)
            );
        }
        assert_map_eq(
            &cached.normalized_types,
            &standalone.normalized_types,
            "normalized_types",
        );
        assert_map_eq(
            &cached.static_call_classes,
            &standalone.static_call_classes,
            "static_call_classes",
        );
        assert_map_eq(
            &cached.lambda_captures,
            &standalone.lambda_captures,
            "lambda_captures",
        );
        // Session lambdas have contextual identities, while standalone block
        // lambdas use syntax identities. Named callables' edges must agree.
        let named_edges = |checker: &TypeChecker| {
            let mut edges = checker.effect_inputs.edges.clone();
            edges.retain(|caller, _| !caller.name().starts_with("<lambda "));
            edges
        };
        assert_eq!(named_edges(&cached), named_edges(&standalone));
        assert_map_eq(
            &cached.effect_inputs.direct,
            &standalone.effect_inputs.direct,
            "lock_direct_effects",
        );
        assert_eq!(
            serde_json::to_value(&cached.effect_inputs.sites).unwrap(),
            serde_json::to_value(&standalone.effect_inputs.sites).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&cached.effect_inputs.direct_sites).unwrap(),
            serde_json::to_value(&standalone.effect_inputs.direct_sites).unwrap()
        );
        // checked_unit rebuilds the tables from the stored records, parents
        // before their lambdas; that path must agree as well.
        let mut unit = crate::compiler_db::CheckedUnit::from(TypeChecker::new());
        for &root in &cached.checked_bodies {
            queries
                .read(root)
                .unwrap()
                .merge_tables(&mut unit, &queries)
                .unwrap();
        }
        assert_map_eq(
            &unit.expr_types,
            &standalone.expr_types,
            "stored expr_types",
        );
        assert_map_eq(
            &unit.lambda_captures,
            &standalone.lambda_captures,
            "stored lambda_captures",
        );
        assert_map_eq(
            &unit.reference_arg_modes,
            &standalone.reference_arg_modes,
            "stored reference_arg_modes",
        );
        (cached, queries, program)
    }

    #[test]
    fn typed_body_matches_standalone_forward_calls_defaults_and_ordered_statics() {
        let (checker, _, _) = compare(
            r#"
            fn first() -> i64 { return later(); }
            interface I { fn defaultValue(self) -> i64 { return later(); } }
            class C {
                pub static first: i64 = 1;
                pub static second: i64 = C::first + 1;
                value: i64;
                pub init(self, value: i64) { self.value = value; }
                pub fn get(self) -> i64 { return self.value + later(); }
                pub static fn create() -> C { return new C(C::second); }
            }
            fn later() -> i64 { return 40; }
            fn main() { let c = C::create(); println(first() + c.get()); }
        "#,
        );
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        let (invalid, _, _) = compare(
            "class C { pub static first: i64 = C::second; pub static second: i64 = 1; } fn main() {}",
        );
        assert!(
            invalid
                .errors
                .iter()
                .any(|error| error.code == ErrorCode::E0838)
        );
    }

    #[test]
    fn typed_body_matches_standalone_inference_captures_enum_and_reference_tables() {
        let (checker, _, _) = compare(
            r#"
            import std::collections::Map as Dict;
            enum Choice { Yes(i64), No }
            fn infer() -> i64 {
                let mut values = Dict::new();
                values.insert(1, 41);
                let offset = 1;
                let add = |x: i64| x + offset;
                let choice: Choice = Yes(add(1));
                return match choice { Yes(value) => value, No => 0 };
            }
            async fn read(value: &i64) -> i64 { return value; }
            async fn main() { let n = infer(); println(await read(&n)); }
        "#,
        );
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        assert!(!checker.reference_arg_modes.is_empty());
        assert!(!checker.enum_variant_resolutions.is_empty());
        assert!(!checker.pattern_resolutions.is_empty());
        assert!(!checker.static_call_classes.is_empty());
        assert!(
            checker
                .lambda_captures
                .values()
                .any(|captures| !captures.is_empty())
        );
        assert!(checker.expr_types.values().any(|ty| matches!(ty, Type::Generic(name, args) if name == "Map" && args == &[Type::I64, Type::I64])));
    }

    #[test]
    fn typed_body_aggregates_transitive_recursive_lock_effects() {
        let (checker, _, _) = compare(
            r#"
            fn first(ch: Channel<i64>, n: i64) -> i64 { return second(ch, n); }
            fn second(ch: Channel<i64>, n: i64) -> i64 {
                if n <= 0 { return ch.recv(); }
                return first(ch, n - 1);
            }
            async fn main() {
                let mutex = Mutex::new(0);
                let ch: Channel<i64> = Channel::new();
                lock mutex as value { let result = first(ch, 1); }
            }
        "#,
        );
        assert_eq!(
            checker
                .errors
                .iter()
                .filter(|error| error.code == ErrorCode::E2604)
                .count(),
            1
        );
    }

    #[test]
    fn typed_body_preserves_missing_collection_import_deduplication() {
        let (checker, _, _) = compare(
            r#"
            fn first() { let values: Array<i64> = [1]; let mut map: Map<i64, i64> = Map::new(); map.insert(1, 2); }
            fn second() { let values: Array<i64> = [2]; let mut map: Map<i64, i64> = Map::new(); map.insert(3, 4); }
        "#,
        );
        assert_eq!(
            checker
                .errors
                .iter()
                .filter(|error| error.code == ErrorCode::E2001)
                .count(),
            1
        );
        assert_eq!(
            checker
                .errors
                .iter()
                .filter(|error| error.code == ErrorCode::E2002)
                .count(),
            1
        );
    }

    #[test]
    fn typed_body_computations_scale_with_roots_and_cache_replays_keep_local_tables() {
        for count in [16, 64, 256] {
            let source: String = (0..count)
                .map(|i| format!("fn body_{i}() -> i64 {{ return 1 + 2; }}\n"))
                .collect();
            let (checker, queries, program) = compare(&source);
            assert!(checker.errors.is_empty(), "{:?}", checker.errors);
            // One evaluation per root, then compare()'s stored-record rebuild
            // reads each root once more.
            assert_eq!(queries.stats().computations, count);
            assert_eq!(queries.stats().calls, 2 * count);
            assert_eq!(queries.stats().hits, count);
            let body = checker.fork_body();
            assert!(Rc::ptr_eq(&checker.resolution, &body.resolution));
            assert!(std::ptr::eq(&*checker.symbols, &*body.symbols));
            assert!(body.expr_types.is_empty());
            assert!(body.normalized_types.is_empty());
            assert!(body.lambda_captures.is_empty());
            assert!(body.errors.is_empty());
            let mut expressions = 0;
            for item in &program.items {
                let Item::Function(function) = item else {
                    panic!()
                };
                for _ in 0..3 {
                    let typed = queries
                        .evaluate(function.body.id, || panic!("body was recomputed"))
                        .unwrap();
                    assert_eq!(typed.id, function.body.id);
                    assert!(typed.diagnostics.is_empty());
                    assert_eq!(typed.expr_types.len(), 3);
                    for (id, ty) in &typed.expr_types {
                        assert_eq!(checker.expr_types.get(id), Some(ty));
                    }
                    expressions += typed.expr_types.len();
                }
            }
            assert_eq!(checker.expr_types.len(), 3 * count);
            assert_eq!(expressions, 9 * count);
            assert_eq!(queries.stats().computations, count);
            assert_eq!(queries.stats().calls, 5 * count);
            assert_eq!(queries.stats().hits, 4 * count);
        }
    }

    /// willow-afb5.17: every lambda body is its own typed record, checked once
    /// under its contextual `BodyId`, and the spliced tables still match an
    /// inline check exactly, including diagnostic order around lambdas.
    fn assert_lambdas_typed_once(source: &str) -> TypeChecker {
        let (checker, queries, _) = compare(source);
        assert_eq!(checker.inline_lambda_rechecks, 0);
        let bodies = queries.index().len();
        // Every body is evaluated once; compare()'s stored-record rebuild then
        // reads each one once more, as a hit.
        assert_eq!(queries.stats().computations, bodies);
        assert_eq!(queries.stats().calls, 2 * bodies);
        assert_eq!(queries.stats().hits, bodies);
        let mut lambdas = 0;
        for id in queries.index().ids() {
            let typed = queries.read(id).unwrap();
            for lambda in typed.lambda_bodies() {
                assert!(queries.is_typed(lambda));
                lambdas += 1;
            }
        }
        // Each lambda is spliced into exactly one parent record.
        assert_eq!(lambdas, checker.lambda_captures.len());
        assert_eq!(queries.stats().computations, bodies);
        checker
    }

    #[test]
    fn nested_lambda_bodies_are_typed_once_in_every_root_kind() {
        let checker = assert_lambdas_typed_once(
            r#"
            import std::collections::Map as Dict;
            class C {
                value: i64;
                pub init(self, v: i64) { let f = |x: i64| x + v; self.value = f(1); }
                pub fn get(self) -> i64 {
                    let base = self.value;
                    let f = |x: i64| { let h = |z: i64| z + base; return h(x); };
                    return f(1);
                }
            }
            fn infer() -> i64 {
                let mut values = Dict::new();
                values.insert(1, 41);
                let read = |k: i64| { let inner = |j: i64| j + k; return inner(k); };
                return read(1);
            }
            fn main() { let c = new C(1); println(c.get() + infer()); }
        "#,
        );
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
        assert_eq!(checker.lambda_captures.len(), 5);
    }

    #[test]
    fn lambda_body_diagnostics_keep_source_order_between_parent_errors() {
        let checker = assert_lambdas_typed_once(
            r#"
            fn bad() {
                let before: String = 1;
                let f = |x: i64| {
                    let s: String = x;
                    let g = |y: i64| { let t: bool = y; return y; };
                    let mid: bool = 2;
                    return g(x);
                };
                let after: String = 3;
            }
            fn main() { bad(); }
        "#,
        );
        assert_eq!(checker.errors.len(), 5, "{:?}", checker.errors);
    }

    #[test]
    fn lock_effects_inside_lambda_bodies_match_inline_checking() {
        assert_lambdas_typed_once(
            r#"
            fn recv(ch: Channel<i64>) -> i64 { return ch.recv(); }
            async fn main() {
                let mutex = Mutex::new(0);
                let ch: Channel<i64> = Channel::new();
                let f = |n: i64| { lock mutex as value { let r = recv(ch); } return n; };
                println(f(1));
            }
        "#,
        );
    }

    #[test]
    fn lambda_body_records_scale_linearly_with_nesting_depth() {
        for depth in [4usize, 16, 64] {
            let mut body = "return x;".to_string();
            for level in (0..depth).rev() {
                body = format!(
                    "let f{level} = |x{level}: i64| {{ let x = x{level}; {body} }}; return f{level}(x);"
                );
            }
            let source = format!("fn f(x: i64) -> i64 {{ {body} }} fn main() {{ println(f(1)); }}");
            let checker = assert_lambdas_typed_once(&source);
            assert!(checker.errors.is_empty(), "{:?}", checker.errors);
            assert_eq!(checker.lambda_captures.len(), depth);
        }
    }

    /// willow-afb5.17 acceptance: every single-file example that writes a
    /// lambda types identically with per-lambda records and inline checking.
    #[test]
    fn lambda_examples_match_inline_typing() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example");
        let mut checked = 0;
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "wi"))
            .collect();
        entries.sort();
        for path in entries {
            let source = std::fs::read_to_string(&path).unwrap();
            if !["= |", "(|", ", |"]
                .iter()
                .any(|form| source.contains(form))
            {
                continue;
            }
            let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
            let (program, errors) = crate::parser::Parser::new(tokens).parse();
            // Module imports need the driver; those examples run in integration.
            if !errors.is_empty()
                || !program
                    .imports
                    .iter()
                    .all(|import| import.path.starts_with("std"))
            {
                continue;
            }
            let (checker, _, _) = compare(&source);
            assert_eq!(checker.inline_lambda_rechecks, 0, "{}", path.display());
            checked += 1;
        }
        assert!(checked >= 10, "only {checked} lambda examples checked");
    }

    /// A map whose element types are inferred from a use inside a lambda:
    /// the placeholder type seen by the lambda must not survive in the stored
    /// lambda record that checked_unit merges after the resolving parent.
    #[test]
    fn inferred_map_uses_inside_lambdas_keep_resolved_types_in_stored_records() {
        let (checker, _, _) = compare(
            r#"
            import std::collections::Map as Dict;
            fn main() {
                let mut values = Dict::new();
                let size = || values.len();
                let peek = |k: i64| { let inner = || values.len() + k; return inner(); };
                values.insert(1, 41);
                println(size() + peek(1));
            }
        "#,
        );
        assert_eq!(checker.inline_lambda_rechecks, 0);
    }
}
