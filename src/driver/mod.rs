use crate::qualified_name::QualifiedName;
use crate::{
    ai, backend, compiler_db, desugar, diagnostics, errors, ir, lexer, module, package, parser,
    prelude, project, query_stats, rust_bridge_ir, semantic, toolchain,
};
use anyhow::{Context, Result};
use compiler_db::dependencies::ModuleDependencies;
use module::artifacts::{LiveUnit, UnitArtifacts, UnitKind};
use std::path::{Path, PathBuf};
mod options;
pub use options::{BuildMode, CodegenOptions, CompilerOptions, TargetOptions};
mod codegen;
mod frontend;
pub(crate) use codegen::backend_unit_imports;
use codegen::run_backend;
pub use codegen::{emit_hir_text, emit_lir_text};
pub(crate) use frontend::*;

/// A single compilation request. Owns the shared context (paths, options,
/// source text, source map) and drives the explicit phases: front-end
/// (lex → parse → import resolution → desugar → type/concurrency checks) and
/// back-end (codegen → link → artifacts). Splitting the phases keeps the
/// driver testable and lets future front-ends (LSP, test harness) reuse them.
pub struct CompilerSession<'a> {
    pub(crate) symbols: semantic::ids::SymbolInterner,
    pub(crate) src: &'a str,
    pub(crate) out: &'a str,
    pub(crate) opts: CompilerOptions,
    pub(crate) project_root: Option<PathBuf>,
}

impl<'a> CompilerSession<'a> {
    pub fn new(
        src: &'a str,
        out: &'a str,
        opts: &CompilerOptions,
        project_root: Option<PathBuf>,
    ) -> Self {
        Self {
            symbols: semantic::ids::SymbolInterner::new(),
            src,
            out,
            opts: opts.clone().resolve_environment(),
            project_root,
        }
    }

    pub fn run(self) -> Result<()> {
        self.run_with_emitter(&mut diagnostics::HumanEmitter)
    }

    /// Compile once, routing frontend and backend diagnostics to the caller.
    /// Tool progress remains on stderr; IO failures are returned to the caller.
    pub fn run_with_emitter(self, emitter: &mut dyn diagnostics::DiagnosticEmitter) -> Result<()> {
        self.execute(emitter, true)
    }

    /// Run the identical project-aware frontend without native artifacts.
    pub fn check_with_emitter(
        self,
        emitter: &mut dyn diagnostics::DiagnosticEmitter,
    ) -> Result<()> {
        self.execute(emitter, false)
    }

    /// Build a read-only analysis snapshot from the same checked frontend,
    /// without codegen or publishing project locks/edit metadata.
    pub fn analysis_with_emitter(
        self,
        emitter: &mut dyn diagnostics::DiagnosticEmitter,
    ) -> Result<ai::Snapshot> {
        self.analysis_with_policy(emitter, false, false, false, false)
    }

    /// Edit preflight covers all project sources and must not rewrite project inputs.
    pub fn analysis_for_edit_with_emitter(
        self,
        emitter: &mut dyn diagnostics::DiagnosticEmitter,
    ) -> Result<ai::Snapshot> {
        self.analysis_with_policy(emitter, false, true, false, false)
    }
    /// Analyze all project sources without importing additional roots into the entry scope
    /// or publishing lock/edit metadata.
    pub fn overview_with_emitter(
        self,
        emitter: &mut dyn diagnostics::DiagnosticEmitter,
    ) -> Result<ai::Snapshot> {
        self.analysis_with_policy(emitter, false, true, true, false)
    }

    /// Capture semantic evidence even when the final checker reports errors.
    /// This never authorizes code generation or edit application.
    pub(crate) fn analysis_for_verification(
        self,
        emitter: &mut dyn diagnostics::DiagnosticEmitter,
    ) -> Result<ai::Snapshot> {
        self.analysis_with_policy(emitter, false, true, false, true)
    }

    fn analysis_with_policy(
        self,
        emitter: &mut dyn diagnostics::DiagnosticEmitter,
        publish_lock: bool,
        discover: bool,
        overview_aliases: bool,
        verification: bool,
    ) -> Result<ai::Snapshot> {
        let _symbols = self.symbols.enter();
        let _query_stats = query_stats::Session::enter();
        let _node_ids = parser::ast::NodeIdSession::enter();
        let path = std::fs::canonicalize(self.src)?;
        let source = compiler_db::captured::FileInput::read(path.clone())?.source;
        let root = path.parent().context("source has no parent")?;
        let mut edit_context = if publish_lock {
            None
        } else {
            Some(ai::edit::AnalysisInputs::capture(
                &path,
                self.project_root.as_deref(),
                &self.opts,
            )?)
        };
        let map = diagnostics::SourceMap::new(path.to_str().context("non UTF-8 path")?, &source);
        let mut inputs = compiler_db::inputs::CompilerInputs::native(self.opts, root.to_path_buf())
            .resolve_project_analysis(self.project_root.as_deref(), publish_lock)?;
        if let Some(context) = &edit_context {
            context.verify_files()?;
        }
        if discover && let Some(project) = &self.project_root {
            let (project, source_root) = discovery_roots(&inputs, project, root);
            inputs.check_modules = if let Some(modules) = compiler_db::captured::overlay_modules() {
                modules
            } else if overview_aliases {
                package::discover_sources_for_overview(&project, &source_root)?
            } else {
                package::discover_sources(&project, &source_root)?
            };
        }
        if let Some(context) = &mut edit_context {
            context.check_modules.clone_from(&inputs.check_modules);
        }
        inputs.capture_analysis = true;
        inputs.verification = verification;
        let frontend = if let Some(context) = &mut edit_context
            && !verification
        {
            let mut recording = ai::verify::RecordingDiagnostics {
                downstream: emitter,
                captured: ai::verify::Diagnostics::default(),
            };
            let frontend = run_frontend_with_inputs(&source, root, &map, inputs, &mut recording)?;
            context.diagnostics = recording.captured.facts;
            frontend
        } else {
            run_frontend_with_inputs(&source, root, &map, inputs, emitter)?
        };
        let mut snapshot = ai::snapshot(&frontend, &path, &source, self.project_root.as_deref())?;
        if let Some(context) = &edit_context {
            context.verify_files()?;
        }
        snapshot.edit_context = edit_context;
        Ok(snapshot)
    }

    /// Reuse the checked frontend IR for Cargo-only adapter validation. This
    /// performs no Rust build, code generation, snapshot construction, or link.
    pub fn rust_bridge_symbols_with_emitter(
        self,
        emitter: &mut dyn diagnostics::DiagnosticEmitter,
    ) -> Result<Vec<rust_bridge_ir::RustBridgeSymbol>> {
        let _symbols = self.symbols.enter();
        let _query_stats = query_stats::Session::enter();
        let _node_ids = parser::ast::NodeIdSession::enter();
        let (frontend, _, _) = self.checked_frontend(emitter, true)?;
        Ok(frontend
            .db
            .rust_bridge_symbols
            .into_iter()
            .map(|(_, symbol)| symbol)
            .collect())
    }

    fn checked_frontend(
        &self,
        emitter: &mut dyn diagnostics::DiagnosticEmitter,
        discover: bool,
    ) -> Result<(Frontend, String, diagnostics::SourceMap)> {
        // Match analysis/verification identity before deriving roots and query
        // keys, so relative CLI checks populate the same persistent cache.
        let src_path =
            std::fs::canonicalize(self.src).with_context(|| format!("cannot read {}", self.src))?;
        let source = std::fs::read_to_string(&src_path)
            .with_context(|| format!("cannot read {}", src_path.display()))?;

        // Import resolution root: the directory containing the source file.
        let root = src_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let mut map =
            diagnostics::SourceMap::new(src_path.to_str().context("non UTF-8 path")?, &source);
        if map.path != self.src {
            map.display = Some(self.src.to_string());
        }

        let mut inputs =
            compiler_db::inputs::CompilerInputs::native(self.opts.clone(), root.clone())
                .resolve_project(self.project_root.as_deref())?;
        if discover && let Some(project) = &self.project_root {
            let (project, source_root) = discovery_roots(&inputs, project, &root);
            inputs.check_modules = package::discover_sources(&project, &source_root)?;
        }
        let frontend = run_frontend_with_inputs(&source, &root, &map, inputs, emitter)?;
        Ok((frontend, source, map))
    }

    fn execute(self, emitter: &mut dyn diagnostics::DiagnosticEmitter, build: bool) -> Result<()> {
        let _symbols = self.symbols.enter();
        let _query_stats = query_stats::Session::enter();
        let _node_ids = parser::ast::NodeIdSession::enter();
        let (frontend, source, map) = self.checked_frontend(emitter, !build)?;
        if let Some(plan) = frontend
            .db
            .package_graph()
            .and_then(|graph| graph.native.as_ref())
        {
            for notice in &plan.notices {
                eprintln!("{notice}");
            }
        }
        let bound = frontend.db.rust_bridge_symbols.clone();
        let rust_bridge = if let Some(project_root) = &self.project_root {
            let manifest = project::ProjectManifest::load(&project_root.join("project.toml"))?;
            if manifest.rust_dependencies.is_empty() {
                anyhow::ensure!(
                    bound.is_empty(),
                    "rust_bridge_missing: extern rust requires Rust dependencies and [rust] bridge"
                );
                None
            } else {
                let mut options =
                    toolchain::rust_bridge_build::BridgeOptions::new(self.opts.target.build_mode)?;
                options.locked = self.opts.locked;
                options.offline = self.opts.offline;
                options.symbols = bound.iter().map(|(_, symbol)| symbol.clone()).collect();
                if build && !bound.is_empty() {
                    drop(toolchain::rust_bridge_build::build_bridge(
                        &manifest,
                        project_root,
                        &options,
                        true,
                    )?);
                }
                toolchain::rust_bridge_build::build_bridge(
                    &manifest,
                    project_root,
                    &options,
                    !build,
                )?
            }
        } else {
            anyhow::ensure!(
                bound.is_empty(),
                "rust_bridge_missing: extern rust requires a project"
            );
            None
        };
        if build {
            run_backend(
                frontend,
                (self.src, source),
                self.out,
                &self.opts,
                &map,
                emitter,
                rust_bridge.as_ref().map(|bridge| (bridge, &bound)),
            )
        } else {
            Ok(())
        }
    }
}
pub fn compile(
    src: &str,
    out: &str,
    opts: &CompilerOptions,
    project_root: Option<PathBuf>,
) -> Result<()> {
    CompilerSession::new(src, out, opts, project_root).run()
}

/// Check an executable source file with a request-local diagnostic destination.
///
/// Runs the normal frontend, including imports, type/concurrency checks and
/// entry-point validation, without code generation, linking or runtime builds.
/// Source IO and emitter failures are returned directly; language diagnostics
/// are sent to `emitter` before returning an error for a failed check.
pub fn check_file(
    src: &str,
    options: &CompilerOptions,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> Result<()> {
    CompilerSession::new(src, "", options, None).check_with_emitter(emitter)
}
