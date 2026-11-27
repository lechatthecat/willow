//! In-process module-driver comparisons; no CLI child processes or linking.
use super::*;
use crate::diagnostics::{Diagnostic, DiagnosticEmitter, Severity, source_map::SourceLookup};
use crate::{CompilerOptions, CompilerSession};
use std::path::PathBuf;

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "willow-effect-model-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create effect project: {error}"),
            }
        }
    }

    fn check(&self, sources: &[String], allowed: ErrorCode) -> bool {
        for (i, source) in sources.iter().enumerate() {
            std::fs::write(self.0.join(format!("m{i}.wi")), source).unwrap();
        }
        let path = self.0.join("m0.wi");
        let mut diagnostics = Diagnostics::default();
        let result =
            CompilerSession::new(path.to_str().unwrap(), "", &CompilerOptions::debug(), None)
                .analysis_with_emitter(&mut diagnostics);
        assert!(
            diagnostics.0.iter().all(|d| d.code == allowed),
            "sources={sources:#?}\ndiagnostics={:?}\nresult={:?}",
            diagnostics.0,
            result.as_ref().err()
        );
        let rejected = !diagnostics.0.is_empty();
        assert_eq!(
            result.is_err(),
            rejected,
            "sources={sources:#?}\n{:?}",
            result.as_ref().err()
        );
        rejected
    }

    fn compare(&self, model: &Model, label: &str, locked: bool) -> bool {
        let sources = model.sources(label, locked);
        let rejected = self.check(&sources, ErrorCode::E2604);
        assert_eq!(
            rejected,
            locked && model.reachable_wait().0,
            "{label}\n{model:?}\n{sources:#?}"
        );
        rejected
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let result = std::fs::remove_dir_all(&self.0);
        // Do not mask a differential failure with a second panic, but do
        // require successful cleanup on every normally completed fixture.
        if !std::thread::panicking() {
            result.expect("remove effect project");
        }
    }
}

#[derive(Default)]
struct Diagnostics(Vec<Diagnostic>);

impl DiagnosticEmitter for Diagnostics {
    fn emit(&mut self, diagnostic: &Diagnostic, _: &dyn SourceLookup) -> std::io::Result<()> {
        if diagnostic.severity == Severity::Error {
            self.0.push(diagnostic.clone());
        }
        Ok(())
    }
}

#[test]
fn core_model_effects_source_imported_entry_paths() {
    // Each imported entry is the ONLY path to the waiting body, so another
    // direct path cannot conceal a dropped virtual/interface/indirect edge.
    let mut cases = 0;
    for kind in [
        Kind::Imported,
        Kind::Virtual,
        Kind::Interface,
        Kind::Indirect,
    ] {
        let mut counts = Counts::default();
        for waiting in [false, true] {
            let mut model = Model::empty(2);
            model.modules[1] = 1;
            model.edges[0].push(Edge { kind, to: 1 });
            model.waits[1] = waiting;
            let label = format!("imported entry kind={kind:?} waiting={waiting}");
            let project = Project::new();
            let path = project.0.clone();
            let rejected = project.compare(&model, &label, true);
            assert_eq!(rejected, waiting);
            counts.add(rejected);
            assert!(!project.compare(&model, &label, false));
            drop(project);
            assert!(!path.exists());
            cases += 1;
        }
        counts.assert(2, 1);
    }
    assert_eq!(cases, 8);
}

#[test]
fn core_model_effects_source_import_cycle_is_rejected() {
    let project = Project::new();
    let path = project.0.clone();
    assert!(project.check(
        &[
            "import m1; fn main() {}".into(),
            "import m2; pub fn one() {}".into(),
            "import m1; pub fn two() {}".into(),
        ],
        ErrorCode::E0403
    ));
    drop(project);
    assert!(!path.exists());
}

#[test]
fn core_model_effects_source_imported_kinds() {
    let mut seed = 0x696d_706f_7274_6564;
    let mut counts: [Counts; 5] = std::array::from_fn(|_| Counts::default());
    let mut cases = 0;
    for case in 0..64 {
        let replay = seed;
        let mut model = Model::empty(9);
        for (i, module) in model.modules.iter_mut().enumerate() {
            *module = i / 3;
        }
        for (from, row) in model.edges.iter_mut().enumerate() {
            model.arity[from] = 1 + (next(&mut seed) % 2) as usize;
            model.waits[from] = case % 4 != 0 && next(&mut seed).is_multiple_of(3);
            for to in 0..9 {
                if model.modules[from] <= model.modules[to] && next(&mut seed).is_multiple_of(4) {
                    let mut kind = LOCAL_KINDS[(next(&mut seed) % 4) as usize];
                    if kind == Kind::Direct && model.modules[from] != model.modules[to] {
                        kind = Kind::Imported;
                    }
                    row.push(Edge { kind, to });
                }
            }
        }
        for base in [0, 3, 6] {
            // All modules mix local dispatch and indirect edges, including
            // cycles. Imports form a DAG; they cannot point back to entry.
            for (offset, kind) in [Kind::Virtual, Kind::Interface, Kind::Indirect]
                .into_iter()
                .enumerate()
            {
                model.edges[base + offset].push(Edge {
                    kind,
                    to: base + (offset + 1) % 3,
                });
            }
            model.edges[base].push(Edge {
                kind: Kind::Direct,
                to: base + 2,
            });
            if base < 6 {
                for (offset, kind) in [Kind::Imported, Kind::Virtual, Kind::Interface]
                    .into_iter()
                    .enumerate()
                {
                    model.edges[base + offset].push(Edge {
                        kind,
                        to: base + offset + 3,
                    });
                }
            }
        }
        if case % 4 != 0 {
            model.waits[8] = true;
        }
        model.lambda = (case % 4 == 1).then(|| 1 + (next(&mut seed) % 2) as usize);
        let label = format!("imported kinds={ALL_KINDS:?} case={case} seed={replay:#x}");
        let project = Project::new();
        let path = project.0.clone();
        let rejected = project.compare(&model, &label, true);
        for count in &mut counts {
            count.add(rejected);
        }
        assert!(!project.compare(&model, &label, false));
        drop(project);
        assert!(!path.exists(), "fixture survived case={case}");
        cases += 1;
    }
    assert_eq!(cases, 64);
    for (kind, count) in ALL_KINDS.into_iter().zip(counts) {
        count.assert(64, 16);
        println!("imported {kind:?}: {count:?}");
    }
}
