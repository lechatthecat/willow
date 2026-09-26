//! Revision reuse for the existing CompilerDb typed-body queries. Declarations
//! are rebuilt; unchanged parsed syntax preserves its identities.
use super::*;
use crate::{CompilerSession, Frontend, ai, diagnostics, module};
use std::collections::{HashMap, HashSet, VecDeque};

/// One accepted frontend, replaced only after analysis succeeds. Dropping this
/// value releases its query tables and temporary artifact pack.
#[derive(Default)]
pub struct AnalysisRevision {
    frontend: Option<Frontend>,
    tracked: tracked::TrackedQueryTable,
    syntax: std::rc::Rc<std::cell::RefCell<incremental::SyntaxQueries>>,
    pub typechecks: usize,
    pub reused_bodies: usize,
    pub retained_artifact_bytes: u64,
    pub input_visits: usize,
    pub invalidation_visits: usize,
    pub fine_allowed_module_refused: usize,
    pub signature_edges: usize,
    pub distinct_signatures: usize,
}

impl AnalysisRevision {
    pub fn analyze(
        &mut self,
        session: CompilerSession<'_>,
        emitter: &mut dyn diagnostics::DiagnosticEmitter,
    ) -> Result<ai::Snapshot> {
        self.analyze_bounded(session, emitter, 128 * 1024 * 1024)
    }

    fn analyze_bounded(
        &mut self,
        session: CompilerSession<'_>,
        emitter: &mut dyn diagnostics::DiagnosticEmitter,
        artifact_limit: u64,
    ) -> Result<ai::Snapshot> {
        use anyhow::Context;
        let _query_stats = crate::query_stats::Session::enter();
        let _node_ids = crate::parser::ast::NodeIdSession::enter();
        let captured::FileInput { path, source } =
            captured::FileInput::capture(std::path::Path::new(session.src))?;
        let root = path.parent().context("source has no parent")?;
        let map = diagnostics::SourceMap::new(path.to_str().context("non UTF-8 path")?, &source);
        let mut inputs = inputs::CompilerInputs::native(session.opts, root.to_path_buf())
            .resolve_project(session.project_root.as_deref())?;
        inputs.capture_analysis = true;
        let mut accepted =
            captured::configuration(&inputs, &path, session.project_root.as_deref())?;
        let previous = self
            .tracked
            .matches_inputs(&accepted)
            .then_some(self.frontend.as_ref())
            .flatten();
        let syntax = std::rc::Rc::new(std::cell::RefCell::new(self.syntax.borrow().candidate()?));
        let frontend = crate::run_frontend_revision(
            &source,
            root,
            &map,
            inputs,
            emitter,
            previous,
            Some(std::rc::Rc::clone(&syntax)),
        )?;
        let retained = frontend
            .module_graph
            .artifacts
            .as_ref()
            .expect("revision artifacts")
            .store
            .written();
        anyhow::ensure!(
            retained <= artifact_limit,
            "revision artifacts exceed {artifact_limit} bytes"
        );
        let snapshot = ai::snapshot(&frontend, &path, &source, session.project_root.as_deref())?;
        ai::check_size(&snapshot)?;
        accepted.push((
            tracked::InputNode::Source(path.clone()),
            captured::bytes(source.as_bytes().to_vec()),
        ));
        let artifacts = frontend
            .module_graph
            .artifacts
            .as_ref()
            .expect("revision artifacts");
        for module in &frontend.module_graph.files {
            // Use the exact source captured by resolution, never reread a file
            // that could have changed while the frontend was evaluating.
            accepted.push((
                tracked::InputNode::Source(module.path.clone()),
                captured::bytes(artifacts.source(module.id.file_id())?.into_bytes()),
            ));
        }
        syntax.borrow_mut().finish()?;
        self.tracked.replace_inputs(accepted)?;
        self.syntax = syntax;
        self.retained_artifact_bytes = retained;
        (self.input_visits, self.invalidation_visits) = frontend.db.revision_work.get();
        self.typechecks = frontend.db.typed_bodies.typechecks();
        self.reused_bodies = frontend.db.typed_bodies.reused();
        self.fine_allowed_module_refused = frontend.db.typed_bodies.fine_allowed_module_refused();
        self.signature_edges = frontend.db.typed_bodies.signature_edge_count();
        self.distinct_signatures = frontend.db.typed_bodies.distinct_signature_count();
        self.frontend = Some(frontend);
        Ok(snapshot)
    }
}

/// Module-granularity invalidation. All reverse edges are visited at most once;
/// any source change invalidates its consumers, including signature, type,
/// default-body and effect changes. A topology change starts a cold generation
/// because FileId is a resolver-local identity.
pub(crate) fn reusable_units(
    previous: &Frontend,
    modules: &[module::ResolvedModule],
    artifacts: &UnitArtifacts,
    db: &CompilerDb,
) -> HashSet<UnitId> {
    let old_modules = &previous.module_graph.files;
    if old_modules.len() != modules.len()
        || old_modules
            .iter()
            .zip(modules)
            .any(|(a, b)| a.id != b.id || a.path != b.path || a.symbol_module != b.symbol_module)
    {
        return HashSet::new();
    }
    let old = previous
        .module_graph
        .artifacts
        .as_ref()
        .expect("revision artifacts");
    let mut invalid = HashSet::new();
    let mut units = HashSet::new();
    for unit in modules
        .iter()
        .map(|m| m.id)
        .chain(std::iter::once(UnitId::ENTRY))
    {
        units.insert(unit);
        let file = unit.file_id();
        match (artifacts.parsed.get(&file), old.parsed.get(&file)) {
            (Some((fingerprint, _)), Some((old, _))) if fingerprint == old => {}
            _ => {
                invalid.insert(unit);
            }
        }
    }
    let mut reverse: HashMap<UnitId, Vec<UnitId>> = HashMap::new();
    for (consumer, edges) in db.dependencies().edges.iter().enumerate() {
        for &dependency in edges {
            reverse
                .entry(modules[dependency].id)
                .or_default()
                .push(modules[consumer].id);
        }
    }
    // Entry registration observes all loaded module declarations.
    for module in modules {
        reverse.entry(module.id).or_default().push(UnitId::ENTRY);
    }
    let mut visits = 0;
    let mut pending: VecDeque<_> = invalid.iter().copied().collect();
    while let Some(unit) = pending.pop_front() {
        for &consumer in reverse.get(&unit).into_iter().flatten() {
            visits += 1;
            if invalid.insert(consumer) {
                pending.push_back(consumer);
            }
        }
    }
    db.revision_work.set((units.len(), visits));
    units.retain(|unit| !invalid.contains(unit));
    units
}

/// Candidate discovery is separate from semantic validation. A retained body
/// still has to validate every declaration read before its artifact is reused.
pub(crate) fn candidate_bodies(
    previous: &Frontend,
    modules: &[module::ResolvedModule],
    artifacts: &UnitArtifacts,
    db: &CompilerDb,
    module_gate: &HashSet<UnitId>,
) -> HashSet<crate::parser::ast::BodyId> {
    let old = &previous.module_graph.files;
    if old.len() != modules.len()
        || old
            .iter()
            .zip(modules)
            .any(|(a, b)| a.id != b.id || a.path != b.path || a.symbol_module != b.symbol_module)
    {
        return HashSet::new();
    }
    let index = db.bodies();
    let mut result: HashSet<_> = index
        .entries()
        .filter_map(|(id, unit)| {
            (module_gate.contains(&unit)
                || artifacts
                    .correspondence
                    .unchanged
                    .contains(&index.source_body(id)))
            .then_some(id)
        })
        .collect();
    for &expr in &artifacts.correspondence.unchanged_initializers {
        if let Some(id) = index.initializer_body(expr) {
            result.insert(id);
        }
    }
    // Each lambda belongs to one parent; visit the inventory once rather than
    // rewalking its ancestry for every nested body.
    let mut pending: Vec<_> = result.iter().copied().collect();
    while let Some(parent) = pending.pop() {
        for (_, child) in index.child_lambdas(parent) {
            if result.insert(child) {
                pending.push(child);
            }
        }
    }
    result
}

pub(crate) fn tracked_body_owners(
    entry: &std::path::Path,
    modules: &[module::ResolvedModule],
    artifacts: &UnitArtifacts,
) -> HashMap<crate::parser::ast::BodyId, (std::path::PathBuf, String)> {
    let paths: HashMap<_, _> = modules
        .iter()
        .map(|module| (module.id.file_id(), module.path.clone()))
        .chain(std::iter::once((
            diagnostics::FileId::ENTRY,
            entry.to_owned(),
        )))
        .collect();
    let index = &artifacts.bodies;
    let mut result = HashMap::new();
    for (body, _) in index.entries() {
        if let Some((file, owner)) = artifacts.syntax_owners.get(&index.source_body(body))
            && let Some(path) = paths.get(file)
        {
            result.insert(body, (path.clone(), owner.clone()));
        }
    }
    for (expr, (file, owner)) in &artifacts.initializer_owners {
        if let Some(body) = index.initializer_body(*expr)
            && let Some(path) = paths.get(file)
        {
            result.insert(body, (path.clone(), owner.clone()));
        }
    }
    let mut pending: Vec<_> = result.keys().copied().collect();
    while let Some(parent) = pending.pop() {
        let source = result[&parent].clone();
        for (_, child) in index.child_lambdas(parent) {
            if let std::collections::hash_map::Entry::Vacant(entry) = result.entry(child) {
                entry.insert(source.clone());
                pending.push(child);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    #[derive(Default)]
    struct Diagnostics(Vec<serde_json::Value>);
    impl diagnostics::DiagnosticEmitter for Diagnostics {
        fn emit(
            &mut self,
            diagnostic: &diagnostics::Diagnostic,
            _: &dyn diagnostics::source_map::SourceLookup,
        ) -> std::io::Result<()> {
            self.0.push(serde_json::to_value(diagnostic).unwrap());
            Ok(())
        }
    }
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "willow-revision-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn write(&self, file: &str, source: &str) {
            std::fs::write(self.0.join(file), source).unwrap();
        }
        fn compare(&self, warm: &mut AnalysisRevision) -> bool {
            let path = self.0.join("main.wi");
            let options = crate::CompilerOptions::debug();
            let session = || CompilerSession::new(path.to_str().unwrap(), "", &options, None);
            let mut a = Diagnostics::default();
            let mut b = Diagnostics::default();
            let incremental = warm.analyze(session(), &mut a);
            let cold = session().analysis_with_emitter(&mut b);
            assert_eq!(a.0, b.0);
            match (incremental, cold) {
                (Ok(a), Ok(b)) => {
                    let a = serde_json::to_value(a).unwrap();
                    let b = serde_json::to_value(b).unwrap();
                    for key in a
                        .as_object()
                        .unwrap()
                        .keys()
                        .filter(|key| key.as_str() != "revision")
                    {
                        if a[key] != b[key] {
                            fn difference(
                                a: &serde_json::Value,
                                b: &serde_json::Value,
                                path: String,
                            ) -> String {
                                match (a, b) {
                                    (
                                        serde_json::Value::Object(a),
                                        serde_json::Value::Object(b),
                                    ) => {
                                        for (key, value) in a {
                                            if b.get(key) != Some(value) {
                                                return difference(
                                                    value,
                                                    &b[key],
                                                    format!("{path}/{key}"),
                                                );
                                            }
                                        }
                                    }
                                    (serde_json::Value::Array(a), serde_json::Value::Array(b))
                                        if a.len() == b.len() =>
                                    {
                                        for (index, (a, b)) in a.iter().zip(b).enumerate() {
                                            if a != b {
                                                return difference(a, b, format!("{path}/{index}"));
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                                format!("{path}: incremental={a}, cold={b}")
                            }
                            panic!("{}", difference(&a[key], &b[key], key.clone()));
                        }
                    }
                    assert_eq!(a["revision"], b["revision"], "snapshot revision");
                    true
                }
                (Err(a), Err(b)) => {
                    assert_eq!(a.to_string(), b.to_string());
                    false
                }
                (a, b) => panic!("warm/cold mismatch: {a:?} / {b:?}"),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn review_symbol_ids_ignore_offsets_and_keep_shadowed_locals_distinct() {
        let f = Fixture::new();
        let source = "fn value(p: i64) -> i64 { let x = p; if true { let x = 2; println(x); } return x; } fn main() { println(value(1)); }";
        let path = f.0.join("main.wi");
        let options = crate::CompilerOptions::debug();
        let mut revision = AnalysisRevision::default();
        let mut ids = Vec::new();
        for prefix in ["", "\n// moved\n", "\n\n\n"] {
            f.write("main.wi", &format!("{prefix}{source}"));
            let snapshot = revision
                .analyze(
                    CompilerSession::new(path.to_str().unwrap(), "", &options, None),
                    &mut Diagnostics::default(),
                )
                .unwrap();
            let current: Vec<_> = snapshot
                .semantic
                .symbols
                .iter()
                .map(|s| s.id.clone())
                .collect();
            if ids.is_empty() {
                ids = current;
            } else {
                assert_eq!(ids, current);
            }
            let locals: Vec<_> = snapshot
                .semantic
                .symbols
                .iter()
                .filter(|s| s.name == "x")
                .collect();
            assert_eq!(locals.len(), 2);
            assert_ne!(locals[0].id, locals[1].id);
            assert!(f.compare(&mut revision));
        }
    }

    #[test]
    fn review_effect_recomputation_is_independent_of_unrelated_body_count() {
        for count in [8, 32, 128] {
            let f = Fixture::new();
            let unrelated: String = (0..count)
                .map(|i| format!("fn unrelated_{i}() -> i64 {{ return {i}; }}\n"))
                .collect();
            let source = format!(
                "{unrelated} fn get() -> i64 {{ return 1; }} fn caller() -> i64 {{ return get(); }} fn main() {{ println(caller()); }}"
            );
            f.write("main.wi", &source);
            let mut revision = AnalysisRevision::default();
            assert!(f.compare(&mut revision));
            f.write(
                "main.wi",
                &source.replace("return 1; } fn caller", "return 2; } fn caller"),
            );
            assert!(f.compare(&mut revision));
            let work = revision.frontend.as_ref().unwrap().db.effects.work();
            assert_eq!(work, (1, 0), "effect-preserving body edit, count={count}");
            println!(
                "review_effects functions={count} scanned={} equations={}",
                work.0, work.1
            );
            f.write(
                "main.wi",
                &source.replace("return 1; } fn caller", "println(1); return 1; } fn caller"),
            );
            assert!(f.compare(&mut revision));
            let work = revision.frontend.as_ref().unwrap().db.effects.work();
            assert_eq!(work.0, 1);
            assert!(
                work.1 > 0 && work.1 <= 6,
                "unrelated effects must be reused: {work:?}"
            );
            f.write("main.wi", &source);
            assert!(f.compare(&mut revision));
            assert_eq!(revision.frontend.as_ref().unwrap().db.effects.work().0, 1);
        }
    }

    #[test]
    fn revision_native_object_matches_cold_after_body_edit() {
        fn object(
            revision: &AnalysisRevision,
            path: &std::path::Path,
        ) -> std::collections::BTreeMap<String, Vec<u8>> {
            use object::{Object, ObjectSection};
            let frontend = revision.frontend.as_ref().unwrap();
            assert!(frontend.module_graph.files.is_empty());
            let artifacts = frontend.module_graph.artifacts.as_ref().unwrap();
            let program = artifacts
                .hydrate(&frontend.program, diagnostics::FileId::ENTRY)
                .unwrap();
            let db = &frontend.db;
            let checked = db.checked_unit(UnitId::ENTRY, artifacts).unwrap();
            let options = crate::CompilerOptions::debug();
            let mut codegen =
                crate::backend::Codegen::new(&options, std::rc::Rc::clone(&db.layouts)).unwrap();
            codegen.body_queries = Some(std::rc::Rc::clone(&db.typed_bodies));
            codegen.lir_queries = Some(std::rc::Rc::clone(&db.lir));
            codegen.effect_queries = Some((std::rc::Rc::clone(&db.effects), UnitId::ENTRY));
            for (name, info) in &checked.symbols.interfaces {
                let identity = crate::semantic::ids::TypeId::from_source_name(&info.name);
                codegen
                    .register_interface_info(name.to_string(), identity, || info.to_semantic())
                    .unwrap();
            }
            let scope = db
                .unit_scope(UnitId::ENTRY, &program, &[], &checked.symbols)
                .unwrap();
            let types = checked
                .expr_types
                .iter()
                .map(|(id, ty)| (*id, ty.into()))
                .collect();
            let unit = codegen
                .declare_program_with_types(&program, path.to_str().unwrap(), &types, scope)
                .unwrap();
            let mut tables = checked.tables();
            tables.expr_types = Some(unit.normalized_expr_types());
            db.lir
                .lower_unit(
                    UnitId::ENTRY,
                    unit.normalized_program(),
                    db.bodies(),
                    &tables,
                )
                .unwrap();
            let before = revision.syntax.borrow().stats();
            let plan = codegen.program_body_plan(&unit);
            codegen
                .with_program_bodies(&unit, |backend| {
                    for target in &plan {
                        backend.compile_body(target)?;
                    }
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                revision.syntax.borrow().stats(),
                before,
                "emission adds no dependency edges"
            );
            let bytes = codegen.finish().unwrap();
            let object = object::File::parse(bytes.as_slice()).unwrap();
            object
                // Mach-O and some COFF function symbols have size zero. Compare
                // complete executable sections so those targets cannot silently
                // omit functions. Padding is intentionally checked too.
                .sections()
                .filter(|section| {
                    section.kind() == object::SectionKind::Text && section.size() != 0
                })
                .enumerate()
                .map(|(ordinal, section)| {
                    (
                        format!("{ordinal}:{}", section.name().unwrap()),
                        section.data().unwrap().to_vec(),
                    )
                })
                .collect()
        }
        let f = Fixture::new();
        let mut warm = AnalysisRevision::default();
        for (iteration, source) in [
            "fn get() -> i64 { return 1; } fn untouched() -> i64 { return 7; } fn main() { println(get()); }",
            "fn get() -> i64 { return 2; } fn untouched() -> i64 { return 7; } fn main() { println(get()); }",
            "fn get() -> i64 { return 2; } fn untouched() -> i64 { return 7; } fn main() { println(get()); }",
            "interface I { fn get(self) -> i64; } class Counter implements I { pub value: i64; pub fn get(self) -> i64 { return self.value + 1; } } fn main() { println(new Counter(3).get()); }",
            "interface I { fn get(self) -> i64; } class Counter implements I { pub value: i64; pub fn get(self) -> i64 { return self.value + 2; } } fn main() { println(new Counter(3).get()); }",
        ].iter().enumerate() {
            f.write("main.wi", source);
            assert!(f.compare(&mut warm));
            let warm_object = object(&warm, &f.0.join("main.wi"));
            let mut cold = AnalysisRevision::default();
            assert!(f.compare(&mut cold));
            let cold_object = object(&cold, &f.0.join("main.wi"));
            assert!(!warm_object.is_empty());
            assert_eq!(warm_object.keys().collect::<Vec<_>>(), cold_object.keys().collect::<Vec<_>>());
            for (name, code) in warm_object {
                assert_eq!(code, cold_object[&name], "machine code for {name}, edit {iteration}");
            }
        }
    }

    #[test]
    fn revision_signature_edit_loop_measures_actual_consumers() {
        for count in [8, 32, 128] {
            let f = Fixture::new();
            let unrelated = (0..count)
                .map(|i| format!("fn other_{i}() -> i64 {{ return {i}; }}\n"))
                .collect::<String>();
            let mut warm = AnalysisRevision::default();
            for (step, target) in [
                "fn target() -> i64 { return 1; }",
                "fn target() -> i64 { return 2; }",
                "fn target() -> String { return \"changed\"; }",
                "fn target() -> String { return \"again\"; }",
            ]
            .into_iter()
            .enumerate()
            {
                f.write(
                    "main.wi",
                    &format!("{target} fn caller() {{ println(target()); }} fn main() {{ caller(); }} {unrelated}"),
                );
                assert!(f.compare(&mut warm));
                let expected = match step {
                    0 => count + 3,
                    2 => 2,
                    _ => 1,
                };
                assert_eq!(warm.typechecks, expected);
                assert_eq!(warm.reused_bodies, count + 3 - expected);
                if step != 0 {
                    // All functions share the edited unit: this measures the
                    // typed-body family's explicit coarse-gate relaxation.
                    assert_eq!(warm.fine_allowed_module_refused, warm.reused_bodies);
                    println!(
                        "ai_edit_loop unrelated={count} step={step} typechecks={} reused={} signature_edges={} distinct_signatures={} edge_visits={} coarse_refused={}",
                        warm.typechecks,
                        warm.reused_bodies,
                        warm.signature_edges,
                        warm.distinct_signatures,
                        warm.syntax.borrow().stats().dependency_edges_visited,
                        warm.fine_allowed_module_refused,
                    );
                }
            }
        }
    }

    #[test]
    fn revision_body_edit_rechecks_only_changed_function() {
        let f = Fixture::new();
        f.write(
            "main.wi",
            "import value; fn main() { println(value::get()); }",
        );
        f.write(
            "value.wi",
            "pub fn get() -> i64 { return 1; } pub fn unrelated() -> i64 { return 7; }",
        );
        let mut warm = AnalysisRevision::default();
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 3);
        f.write(
            "value.wi",
            "pub fn get() -> i64 { return 2; } pub fn unrelated() -> i64 { return 7; }",
        );
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 1);
        assert_eq!(warm.reused_bodies, 2);
    }

    #[test]
    fn revision_whitespace_and_offset_edits_keep_semantic_bodies() {
        let f = Fixture::new();
        f.write(
            "main.wi",
            "import value; fn main() { println(value::get()); }",
        );
        f.write("value.wi", "pub fn get() -> i64 { let f = |x: i64| x + 1; return f(1); } pub fn untouched() -> i64 { return 7; }");
        let mut warm = AnalysisRevision::default();
        assert!(f.compare(&mut warm));
        let count = warm.typechecks;
        f.write("value.wi", "\n// moved\npub fn get() -> i64 {\n let f = |x: i64| x + 1;\n return f(1);\n }\n pub fn untouched() -> i64 { return 7; }\n");
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 0);
        assert_eq!(warm.reused_bodies, count);
        f.write("value.wi", "\n pub fn get( ) -> i64 { let   f = | x : i64 | x + 1; return   f( 1 ); }\n pub fn untouched( ) -> i64 { return 7; }\n");
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 0);
        assert_eq!(warm.reused_bodies, count);
        f.write(
            "value.wi",
            "pub fn get() -> i64 { return 2000; } pub fn untouched() -> i64 { return 7; }",
        );
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 1);
        assert_eq!(warm.reused_bodies, 2);
    }

    #[test]
    fn revision_reuses_typed_bodies_and_invalidates_dependents() {
        let f = Fixture::new();
        f.write("main.wi", "import caller; import independent; fn main() { println(caller::call()); independent::run(); }");
        f.write(
            "caller.wi",
            "import value; pub fn call() -> i64 { return value::get(); }",
        );
        f.write("value.wi", "pub fn get() -> i64 { return 1; }");
        f.write("independent.wi", "pub fn run() { let f = |x: i64| { let g = |y: i64| y + x; return g(1); }; println(f(1)); }");
        let mut warm = AnalysisRevision::default();
        assert!(f.compare(&mut warm));
        let initial = warm.typechecks;
        assert!(initial >= 6, "{initial}");
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 0);
        assert_eq!(warm.reused_bodies, initial);
        f.write("value.wi", "pub fn get() -> i64 { return 20; }");
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 1);
        assert_eq!(warm.reused_bodies, initial - 1);
        f.write("value.wi", "pub fn get() -> String { return \"changed\"; }");
        assert!(!f.compare(&mut warm));
        f.write("value.wi", "pub fn get() -> i64 { return 1; }");
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 1);
    }
    #[test]
    fn revision_signature_and_type_changes_preserve_unrelated_units() {
        let f = Fixture::new();
        f.write(
            "main.wi",
            "import caller; import independent; fn main() { caller::call(); independent::run(); }",
        );
        f.write(
            "caller.wi",
            "import value; pub fn call() { println(value::get()); }",
        );
        f.write("independent.wi", "pub fn run() {}");
        let mut warm = AnalysisRevision::default();
        for source in [
            "pub fn get() -> i64 { return 1; }",
            "pub fn get() -> String { return \"value\"; }",
        ] {
            f.write("value.wi", source);
            assert!(f.compare(&mut warm));
        }
        assert_eq!((warm.typechecks, warm.reused_bodies), (2, 2));
        f.write(
            "main.wi",
            "import caller; import independent; fn main() { independent::run(); }",
        );
        f.write("value.wi", "pub class Value { pub field: i64; }");
        f.write(
            "caller.wi",
            "import value; pub fn call(v: value::Value) { println(v.field); }",
        );
        assert!(f.compare(&mut warm));
        f.write("value.wi", "pub class Value { pub field: String; }");
        assert!(f.compare(&mut warm));
        assert_eq!(warm.reused_bodies, 2);
        assert_eq!(warm.typechecks, 1);
        f.write(
            "caller.wi",
            "import value; pub fn call(v: value::Value) { let x: i64 = v.field; }",
        );
        assert!(!f.compare(&mut warm));
    }

    #[test]
    fn revision_preserves_defaults_constructors_statics_and_nested_lambdas() {
        let f = Fixture::new();
        f.write("main.wi", r#"
            interface I { fn value(self) -> i64 { let f = |x: i64| x + 1; return f(1); } }
            class A implements I {}
            class B implements I {}
            class C {
                pub static value: i64 = 1;
                n: i64;
                pub init(self, n: i64) { let f = |x: i64| x + n; self.n = f(1); }
                pub fn get(self) -> i64 { return self.n; }
            }
            fn main() { let c = new C(1); println(new A().value() + new B().value() + c.get() + C::value); }
        "#);
        let mut warm = AnalysisRevision::default();
        assert!(f.compare(&mut warm));
        let old = std::rc::Rc::downgrade(
            &warm
                .frontend
                .as_ref()
                .unwrap()
                .module_graph
                .artifacts
                .as_ref()
                .unwrap()
                .store,
        );
        for _ in 0..3 {
            assert!(f.compare(&mut warm));
            assert_eq!(warm.typechecks, 0);
        }
        assert!(old.upgrade().is_none(), "obsolete pack retained");
    }

    #[test]
    fn revision_counts_scale_with_invalidated_modules() {
        for n in [16, 64, 256] {
            let f = Fixture::new();
            let mut entry = String::new();
            for i in 0..n {
                entry.push_str(&format!("import m{i};\n"));
                f.write(
                    &format!("m{i}.wi"),
                    &format!("pub fn value() -> i64 {{ return {i}; }}"),
                );
            }
            entry.push_str("fn main() { println(m0::value()); }");
            f.write("main.wi", &entry);
            let mut warm = AnalysisRevision::default();
            assert!(f.compare(&mut warm));
            assert_eq!(warm.typechecks, n + 1);
            assert!(f.compare(&mut warm));
            assert_eq!((warm.typechecks, warm.reused_bodies), (0, n + 1));
            f.write("m0.wi", "pub fn value() -> i64 { return 1000; }");
            assert!(f.compare(&mut warm));
            assert_eq!((warm.typechecks, warm.reused_bodies), (1, n));
            assert_eq!(warm.input_visits, n + 1);
            assert_eq!(warm.invalidation_visits, 1);
            println!(
                "revision modules={n} rechecked={} reused={} retained_bytes={}",
                warm.typechecks, warm.reused_bodies, warm.retained_artifact_bytes
            );
        }
    }
    #[test]
    fn revision_imported_defaults_follow_changed_source_and_release_artifacts() {
        let f = Fixture::new();
        f.write("main.wi", "import a; import b; import independent; fn main() { println(new a::A().value() + new b::B().value()); }");
        f.write(
            "a.wi",
            "import proto::Value; pub class A implements Value {}",
        );
        f.write(
            "b.wi",
            "import proto::Value; pub class B implements Value {}",
        );
        f.write("independent.wi", "pub fn untouched() {}");
        f.write("proto.wi", "pub interface Value { fn value(self) -> i64 { let f = |x: i64| { let g = |y: i64| y + 1; return g(x); }; return f(1); } }");
        let mut warm = AnalysisRevision::default();
        assert!(f.compare(&mut warm));
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 0);
        f.write(
            "proto.wi",
            "pub interface Value { fn value(self) -> i64 { return 20; } }",
        );
        assert!(f.compare(&mut warm));
        assert_eq!(warm.reused_bodies, 2);
    }

    #[test]
    fn revision_invalidation_visits_chain_and_fanout_edges_once() {
        for chain in [false, true] {
            for n in [8, 16, 32] {
                let f = Fixture::new();
                let mut entry = "import independent;\n".to_owned();
                f.write("independent.wi", "pub fn untouched() {}");
                for i in 0..n {
                    entry.push_str(&format!("import m{i};\n"));
                    let source = if i == 0 {
                        "pub fn value() -> i64 { return 1; }".into()
                    } else {
                        let dep = if chain { i - 1 } else { 0 };
                        format!(
                            "import m{dep}; pub fn value() -> i64 {{ return m{dep}::value(); }}"
                        )
                    };
                    f.write(&format!("m{i}.wi"), &source);
                }
                entry.push_str("fn main() {}");
                f.write("main.wi", &entry);
                let mut warm = AnalysisRevision::default();
                assert!(f.compare(&mut warm));
                f.write("m0.wi", "pub fn value() -> i64 { return 2; }");
                assert!(f.compare(&mut warm));
                assert_eq!((warm.typechecks, warm.reused_bodies), (1, n + 1));
                assert_eq!(warm.input_visits, n + 2);
                assert_eq!(warm.invalidation_visits, 2 * n - 1);
                println!(
                    "revision shape={} modules={n} invalidation_edges={} rechecked={} reused={}",
                    if chain { "chain" } else { "fanout" },
                    warm.invalidation_visits,
                    warm.typechecks,
                    warm.reused_bodies
                );
            }
        }
    }

    #[test]
    fn revision_rejects_failed_inputs_and_restarts_on_configuration_and_topology() {
        let f = Fixture::new();
        f.write("main.wi", "fn main() {}");
        let mut warm = AnalysisRevision::default();
        assert!(f.compare(&mut warm));
        let accepted_revision = warm.tracked.revision();
        for invalid in ["fn main() { missing(); }", "fn main( {", "fn main() { ` }"] {
            f.write("main.wi", invalid);
            assert!(!f.compare(&mut warm));
            assert_eq!(warm.tracked.revision(), accepted_revision);
        }
        f.write("main.wi", "fn main() {}");
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 0);
        assert_eq!(warm.tracked.revision(), accepted_revision);
        let path = f.0.join("main.wi");
        let options = crate::CompilerOptions::release();
        warm.analyze(
            CompilerSession::new(path.to_str().unwrap(), "", &options, None),
            &mut Diagnostics::default(),
        )
        .unwrap();
        assert_eq!((warm.typechecks, warm.reused_bodies), (1, 0));
        assert_eq!(warm.tracked.revision().0, accepted_revision.0 + 1);
        f.write("dep.wi", "pub fn f() {}");
        f.write("main.wi", "import dep; fn main() {}");
        assert!(f.compare(&mut warm));
        assert_eq!((warm.typechecks, warm.reused_bodies), (2, 0));
        f.write("main.wi", "fn main() {}");
        assert!(f.compare(&mut warm));
        assert_eq!((warm.typechecks, warm.reused_bodies), (1, 0));
    }
    #[test]
    fn revision_configuration_manifest_and_entry_boundaries_match_cold() {
        let f = Fixture::new();
        f.write("main.wi", "fn main() {}");
        f.write("project.toml", "[project]\nname='app'\nversion='1.0.0'\n");
        let path = f.0.join("main.wi");
        let options = crate::CompilerOptions::debug();
        let mut warm = AnalysisRevision::default();
        let mut compare = |project: Option<std::path::PathBuf>| {
            let session =
                || CompilerSession::new(path.to_str().unwrap(), "", &options, project.clone());
            let mut a = Diagnostics::default();
            let mut b = Diagnostics::default();
            let incremental = warm.analyze(session(), &mut a).unwrap();
            let cold = session().analysis_with_emitter(&mut b).unwrap();
            assert_eq!(a.0, b.0);
            assert_eq!(
                serde_json::to_value(incremental).unwrap(),
                serde_json::to_value(cold).unwrap()
            );
            warm.typechecks
        };
        assert_eq!(compare(Some(f.0.clone())), 1);
        assert_eq!(compare(Some(f.0.clone())), 0);
        f.write(
            "project.toml",
            "[project]\nname='app'\nversion='1.0.0'\n# edited manifest\n",
        );
        assert_eq!(compare(Some(f.0.clone())), 1);
        assert_eq!(compare(Some(f.0.clone())), 0);
        assert_eq!(compare(None), 1);
        assert_eq!(compare(None), 0);
        // Moving to another entry never reuses its resolver-local identities.
        let other = Fixture::new();
        other.write("main.wi", "fn main() {}");
        assert!(other.compare(&mut warm));
        assert_eq!(warm.typechecks, 1);
    }
    #[test]
    fn revision_artifact_limit_rejects_candidate_without_losing_previous() {
        let f = Fixture::new();
        f.write("main.wi", "fn main() {}");
        let mut warm = AnalysisRevision::default();
        assert!(f.compare(&mut warm));
        let old = std::rc::Rc::downgrade(
            &warm
                .frontend
                .as_ref()
                .unwrap()
                .module_graph
                .artifacts
                .as_ref()
                .unwrap()
                .store,
        );
        let before = warm.retained_artifact_bytes;
        f.write("main.wi", "fn main() { println(1); }");
        let path = f.0.join("main.wi");
        let options = crate::CompilerOptions::debug();
        let error = warm
            .analyze_bounded(
                CompilerSession::new(path.to_str().unwrap(), "", &options, None),
                &mut Diagnostics::default(),
                1,
            )
            .unwrap_err();
        assert!(error.to_string().contains("revision artifacts exceed"));
        assert!(old.upgrade().is_some());
        assert_eq!(warm.retained_artifact_bytes, before);
        f.write("main.wi", "fn main() {}");
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 0);
        assert!(old.upgrade().is_none());
    }

    #[test]
    fn revision_reuse_scales_with_body_count_and_lambda_depth() {
        for n in [16, 64, 256] {
            let f = Fixture::new();
            f.write(
                "main.wi",
                "import library; import value; fn main() { println(value::get()); }",
            );
            f.write(
                "library.wi",
                &(0..n)
                    .map(|i| format!("pub fn body_{i}() -> i64 {{ return {i}; }}\n"))
                    .collect::<String>(),
            );
            f.write("value.wi", "pub fn get() -> i64 { return 1; }");
            let mut warm = AnalysisRevision::default();
            assert!(f.compare(&mut warm));
            f.write("value.wi", "pub fn get() -> i64 { return 2; }");
            assert!(f.compare(&mut warm));
            assert_eq!((warm.typechecks, warm.reused_bodies), (1, n + 1));
            println!(
                "revision bodies={n} rechecked=1 reused={} retained_bytes={}",
                warm.reused_bodies, warm.retained_artifact_bytes
            );
        }
        for depth in [4, 16, 64] {
            let f = Fixture::new();
            let mut body = "return x;".to_owned();
            for i in (0..depth).rev() {
                body = format!("let f{i} = |x: i64| {{ {body} }}; return f{i}(x);");
            }
            f.write(
                "main.wi",
                &format!("fn f(x: i64) -> i64 {{ {body} }} fn main() {{ println(f(1)); }}"),
            );
            let mut warm = AnalysisRevision::default();
            assert!(f.compare(&mut warm));
            assert!(f.compare(&mut warm));
            assert_eq!((warm.typechecks, warm.reused_bodies), (0, depth + 2));
            println!(
                "revision lambda_depth={depth} rechecked=0 reused={} retained_bytes={}",
                depth + 2,
                warm.retained_artifact_bytes
            );
        }
    }

    #[test]
    fn revision_effect_changes_recheck_lock_effects() {
        let f = Fixture::new();
        f.write("main.wi", "import value; import independent; async fn main() { let mutex = Mutex::new(0); let ch: Channel<i64> = Channel::new(); lock mutex as n { let v = value::get(ch); } }");
        f.write("independent.wi", "pub fn untouched() {}");
        f.write(
            "value.wi",
            "pub fn get(ch: Channel<i64>) -> i64 { return 1; }",
        );
        let mut warm = AnalysisRevision::default();
        assert!(f.compare(&mut warm));
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 0);
        f.write(
            "value.wi",
            "pub fn get(ch: Channel<i64>) -> i64 { return ch.recv(); }",
        );
        assert!(f.compare(&mut warm));
        assert_eq!((warm.typechecks, warm.reused_bodies), (1, 2));
        f.write(
            "value.wi",
            "pub fn get(ch: Channel<i64>) -> i64 { return 2; }",
        );
        assert!(f.compare(&mut warm));
        assert_eq!(warm.reused_bodies, 2);
    }
}
