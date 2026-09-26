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
    configuration: Option<String>,
    pub typechecks: usize,
    pub reused_bodies: usize,
    pub retained_artifact_bytes: u64,
    pub input_visits: usize,
    pub invalidation_visits: usize,
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
        let path = std::fs::canonicalize(session.src)?;
        let source = std::fs::read_to_string(&path)?;
        let root = path.parent().context("source has no parent")?;
        let map = diagnostics::SourceMap::new(path.to_str().context("non UTF-8 path")?, &source);
        let mut inputs = inputs::CompilerInputs::native(session.opts, root.to_path_buf())
            .resolve_project(session.project_root.as_deref())?;
        inputs.capture_analysis = true;
        // Resolution work counters are observations, not semantic inputs.
        let configuration = format!(
            "{path:?}:{:?}:{:?}:{}:{:?}:{:?}",
            inputs.options,
            inputs.project_root,
            inputs.project_mode,
            inputs.target,
            inputs.package_graph.as_ref().map(|g| (g.root, &g.packages))
        );
        let previous = (self.configuration.as_ref() == Some(&configuration))
            .then_some(self.frontend.as_ref())
            .flatten();
        let frontend =
            crate::run_frontend_revision(&source, root, &map, inputs, emitter, previous, true)?;
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
        self.retained_artifact_bytes = retained;
        (self.input_visits, self.invalidation_visits) = frontend.db.revision_work.get();
        self.typechecks = frontend.db.typed_bodies.typechecks();
        self.reused_bodies = frontend.db.typed_bodies.reused();
        self.frontend = Some(frontend);
        self.configuration = Some(configuration);
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
                    for key in a.as_object().unwrap().keys() {
                        assert_eq!(a[key], b[key], "snapshot field {key}");
                    }
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
        assert_eq!(warm.typechecks, 3);
        assert_eq!(warm.reused_bodies, initial - 3);
        f.write("value.wi", "pub fn get() -> String { return \"changed\"; }");
        assert!(!f.compare(&mut warm));
        f.write("value.wi", "pub fn get() -> i64 { return 1; }");
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 3);
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
        assert_eq!((warm.typechecks, warm.reused_bodies), (3, 1));
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
        assert_eq!(warm.reused_bodies, 1);
        assert_eq!(warm.typechecks, 2);
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
            assert_eq!((warm.typechecks, warm.reused_bodies), (2, n - 1));
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
        assert_eq!(warm.reused_bodies, 1);
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
                assert_eq!((warm.typechecks, warm.reused_bodies), (n + 1, 1));
                assert_eq!(warm.input_visits, n + 2);
                assert_eq!(warm.invalidation_visits, 2 * n - 1);
                println!(
                    "revision shape={} modules={n} invalidation_edges={} rechecked={} reused=1",
                    if chain { "chain" } else { "fanout" },
                    warm.invalidation_visits,
                    warm.typechecks
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
        for invalid in ["fn main() { missing(); }", "fn main( {", "fn main() { ` }"] {
            f.write("main.wi", invalid);
            assert!(!f.compare(&mut warm));
        }
        f.write("main.wi", "fn main() {}");
        assert!(f.compare(&mut warm));
        assert_eq!(warm.typechecks, 0);
        let path = f.0.join("main.wi");
        let options = crate::CompilerOptions::release();
        warm.analyze(
            CompilerSession::new(path.to_str().unwrap(), "", &options, None),
            &mut Diagnostics::default(),
        )
        .unwrap();
        assert_eq!((warm.typechecks, warm.reused_bodies), (1, 0));
        f.write("dep.wi", "pub fn f() {}");
        f.write("main.wi", "import dep; fn main() {}");
        assert!(f.compare(&mut warm));
        assert_eq!((warm.typechecks, warm.reused_bodies), (2, 0));
        f.write("main.wi", "fn main() {}");
        assert!(f.compare(&mut warm));
        assert_eq!((warm.typechecks, warm.reused_bodies), (1, 0));
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
            assert_eq!((warm.typechecks, warm.reused_bodies), (2, n));
            println!(
                "revision bodies={n} rechecked=2 reused={n} retained_bytes={}",
                warm.retained_artifact_bytes
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
        assert_eq!((warm.typechecks, warm.reused_bodies), (2, 1));
        f.write(
            "value.wi",
            "pub fn get(ch: Channel<i64>) -> i64 { return 2; }",
        );
        assert!(f.compare(&mut warm));
        assert_eq!(warm.reused_bodies, 1);
    }
}
