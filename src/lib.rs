// `Diagnostic` is the compiler's pervasive error type; returning it by value
// keeps fallible parser/semantic signatures readable. Boxing every `Result` to
// shrink the cold `Err` path is churn not worth it here, so allow it crate-wide.
#![allow(clippy::result_large_err)]

pub mod ai;
pub mod backend;
pub mod compiler_db;
use compiler_db::dependencies::ModuleDependencies;
pub(crate) mod compiler_stack;
pub mod desugar;
pub mod diagnostics;
pub mod errors;
pub mod interpolate;
pub mod ir;
pub mod lexer;
pub mod module;
pub mod package;
pub mod parser;
pub mod prelude;
pub mod project;
mod query_stats;
pub mod semantic;
pub mod stdlib_schema;
pub mod toolchain;

use anyhow::{Context, Result};
use module::artifacts::{LiveUnit, UnitArtifacts, UnitKind};
use std::path::{Path, PathBuf};

use willow_abi::workers::{default_worker_count, parse_worker_count};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BuildMode {
    Debug,
    Release,
}

#[derive(Debug, Clone)]
pub struct TargetOptions {
    pub build_mode: BuildMode,
    pub emit_debug_info: bool,
    pub emit_source_map: bool,
    pub strip_symbols: bool,
    pub runtime_lib: Option<PathBuf>,
    pub cargo_target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct CompilerOptions {
    /// Require an existing, current project.lock; never update it.
    pub locked: bool,
    /// Resolve dependencies exclusively from the local package cache.
    pub offline: bool,
    pub target: TargetOptions,
    pub worker_count: Option<usize>,
    /// Retained for API compatibility; compilation always enforces Send/Sync.
    pub enforce_send_sync: bool,
}

/// Compatibility alias for callers that used the pre-library API name.
pub type CodegenOptions = CompilerOptions;

struct CompilerEnvironment {
    workers: Option<usize>,
    runtime_lib: Option<PathBuf>,
    cargo_target_dir: Option<PathBuf>,
}

impl Default for CompilerEnvironment {
    fn default() -> Self {
        Self {
            workers: Some(default_worker_count()),
            runtime_lib: None,
            cargo_target_dir: None,
        }
    }
}

impl CompilerEnvironment {
    fn read() -> Self {
        Self {
            workers: Some(
                parse_worker_count(std::env::var("WILLOW_WORKERS").ok().as_deref())
                    .unwrap_or_else(default_worker_count),
            ),
            runtime_lib: std::env::var_os("WILLOW_RUNTIME_LIB").map(PathBuf::from),
            cargo_target_dir: std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from),
        }
    }
}

impl CompilerOptions {
    pub fn debug() -> Self {
        Self {
            target: TargetOptions {
                build_mode: BuildMode::Debug,
                emit_debug_info: true,
                emit_source_map: true,
                strip_symbols: false,
                runtime_lib: None,
                cargo_target_dir: None,
            },
            worker_count: None,
            enforce_send_sync: true,
            locked: false,
            offline: false,
        }
    }

    pub fn release() -> Self {
        Self {
            target: TargetOptions {
                build_mode: BuildMode::Release,
                emit_debug_info: false,
                emit_source_map: false,
                strip_symbols: false,
                runtime_lib: None,
                cargo_target_dir: None,
            },
            worker_count: None,
            enforce_send_sync: true,
            locked: false,
            offline: false,
        }
    }

    pub fn release_with_debug_info() -> Self {
        Self {
            target: TargetOptions {
                build_mode: BuildMode::Release,
                emit_debug_info: true,
                emit_source_map: true,
                strip_symbols: false,
                runtime_lib: None,
                cargo_target_dir: None,
            },
            worker_count: None,
            enforce_send_sync: true,
            locked: false,
            offline: false,
        }
    }

    fn resolve_environment(self) -> Self {
        self.with_environment(CompilerEnvironment::read())
    }

    fn with_environment(mut self, environment: CompilerEnvironment) -> Self {
        self.worker_count = Some(
            self.worker_count
                .filter(|workers| *workers > 0)
                .or(environment.workers.filter(|workers| *workers > 0))
                .unwrap_or_else(default_worker_count),
        );
        // Type safety is independent of the runtime scheduling policy.
        self.enforce_send_sync = true;
        if self.target.runtime_lib.is_none() {
            self.target.runtime_lib = environment.runtime_lib;
        }
        if self.target.cargo_target_dir.is_none() {
            self.target.cargo_target_dir = environment.cargo_target_dir;
        }
        self
    }
}

#[cfg(test)]
mod compiler_options_tests {
    use super::*;

    #[test]
    fn debug_and_release_profiles_live_in_target_options() {
        let debug = CompilerOptions::debug();
        assert_eq!(debug.target.build_mode, BuildMode::Debug);
        assert!(debug.target.emit_debug_info);

        let release = CompilerOptions::release();
        assert_eq!(release.target.build_mode, BuildMode::Release);
        assert!(!release.target.emit_source_map);
    }

    #[test]
    fn multi_worker_environment_keeps_send_sync_checks() {
        let options = CompilerOptions::debug().with_environment(CompilerEnvironment {
            workers: Some(8),
            ..CompilerEnvironment::default()
        });
        assert_eq!(options.worker_count, Some(8));
        assert!(options.enforce_send_sync);
    }

    #[test]
    fn default_environment_uses_available_parallelism_and_checks() {
        let options = CompilerOptions::debug().with_environment(CompilerEnvironment::default());
        assert_eq!(options.worker_count, Some(default_worker_count()));
        assert!(options.enforce_send_sync);
    }

    #[test]
    fn single_worker_override_keeps_checks_enabled() {
        let options = CompilerOptions::debug().with_environment(CompilerEnvironment {
            workers: Some(1),
            ..CompilerEnvironment::default()
        });
        assert_eq!(options.worker_count, Some(1));
        assert!(options.enforce_send_sync);
    }

    #[test]
    fn worker_count_cannot_disable_type_safety() {
        for workers in [0, 1, 2, 8, 64] {
            let mut options = CompilerOptions::debug();
            options.worker_count = Some(workers);
            options.enforce_send_sync = false;
            let options = options.with_environment(CompilerEnvironment {
                workers: Some(3),
                ..CompilerEnvironment::default()
            });
            assert_eq!(
                options.worker_count,
                Some(if workers == 0 { 3 } else { workers })
            );
            assert!(options.enforce_send_sync);
        }
    }

    #[test]
    fn explicit_options_take_precedence_over_environment() {
        let mut options = CompilerOptions::debug();
        options.worker_count = Some(2);
        options.enforce_send_sync = true;
        options.target.runtime_lib = Some(PathBuf::from("explicit-runtime.a"));
        options.target.cargo_target_dir = Some(PathBuf::from("explicit-target"));
        let options = options.with_environment(CompilerEnvironment {
            workers: Some(8),
            runtime_lib: Some(PathBuf::from("environment-runtime.a")),
            cargo_target_dir: Some(PathBuf::from("environment-target")),
        });
        assert_eq!(options.worker_count, Some(2));
        assert_eq!(
            options.target.runtime_lib,
            Some(PathBuf::from("explicit-runtime.a"))
        );
        assert_eq!(
            options.target.cargo_target_dir,
            Some(PathBuf::from("explicit-target"))
        );
    }

    #[test]
    fn environment_paths_fill_unspecified_target_options() {
        let options = CompilerOptions::debug().with_environment(CompilerEnvironment {
            runtime_lib: Some(PathBuf::from("runtime.a")),
            cargo_target_dir: Some(PathBuf::from("target-dir")),
            ..CompilerEnvironment::default()
        });
        assert_eq!(options.target.runtime_lib, Some(PathBuf::from("runtime.a")));
        assert_eq!(
            options.target.cargo_target_dir,
            Some(PathBuf::from("target-dir"))
        );
    }

    #[test]
    fn worker_count_parser_rejects_invalid_values() {
        assert_eq!(parse_worker_count(Some("4")), Some(4));
        assert_eq!(parse_worker_count(Some(" 2 ")), Some(2));
        assert_eq!(parse_worker_count(Some("8")), Some(8));
        assert_eq!(parse_worker_count(Some("invalid")), None);
        assert_eq!(parse_worker_count(Some("0")), None);
        assert_eq!(parse_worker_count(None), None);
    }
}
fn register_prelude(checker: &mut semantic::TypeChecker) -> Result<()> {
    let tokens = lexer::Lexer::new(prelude::PRELUDE_SOURCE)
        .tokenize()
        .map_err(|error| errors::InternalCompilerError::new("prelude lexing", error))?;
    let (program, errors) = parser::Parser::new(tokens).parse();
    if !errors.is_empty() {
        return Err(errors::InternalCompilerError::new(
            "prelude parsing",
            format!("{} diagnostic(s)", errors.len()),
        )
        .into());
    }
    // Register only declarations; do not type-check the prelude body.
    use parser::ast::Item;
    for item in &program.items {
        match item {
            Item::Enum(e) => checker.register_prelude_enum(e),
            Item::Interface(i) => checker.register_prelude_interface(i),
            Item::Function(f) => {
                // Future: register prelude functions (e.g. panic) here.
                let _ = f;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Front-end artifacts produced by [`run_frontend`] and consumed by
/// [`run_backend`]: checked declaration summaries and lazy, immutable body
/// artifacts. Checkers and executable trees are owned only by their unit pass.
struct Frontend {
    // These programs contain declarations only. Their body slots are immutable
    // references (source spans / syntax IDs) into module_graph.artifacts.
    program: parser::ast::Program,
    module_graph: module::ModuleGraph,
    db: compiler_db::CompilerDb,
}

/// A single compilation request. Owns the shared context (paths, options,
/// source text, source map) and drives the explicit phases: front-end
/// (lex → parse → import resolution → desugar → type/concurrency checks) and
/// back-end (codegen → link → artifacts). Splitting the phases keeps the
/// driver testable and lets future front-ends (LSP, test harness) reuse them.
pub struct CompilerSession<'a> {
    src: &'a str,
    out: &'a str,
    opts: CompilerOptions,
    project_root: Option<PathBuf>,
}

impl<'a> CompilerSession<'a> {
    pub fn new(
        src: &'a str,
        out: &'a str,
        opts: &CompilerOptions,
        project_root: Option<PathBuf>,
    ) -> Self {
        Self {
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

    /// Build an analysis snapshot from the same checked frontend, without codegen.
    pub fn analysis_with_emitter(
        self,
        emitter: &mut dyn diagnostics::DiagnosticEmitter,
    ) -> Result<ai::Snapshot> {
        let _query_stats = query_stats::Session::enter();
        let _node_ids = parser::ast::NodeIdSession::enter();
        let path = std::fs::canonicalize(self.src)?;
        let source = std::fs::read_to_string(&path)?;
        let root = path.parent().context("source has no parent")?;
        let map = diagnostics::SourceMap::new(path.to_str().context("non UTF-8 path")?, &source);
        let mut inputs = compiler_db::inputs::CompilerInputs::native(self.opts, root.to_path_buf())
            .resolve_project(self.project_root.as_deref())?;
        inputs.capture_analysis = true;
        let frontend = run_frontend_with_inputs(&source, root, &map, inputs, emitter)?;
        ai::snapshot(&frontend, &path, &source, self.project_root.as_deref())
    }

    fn execute(self, emitter: &mut dyn diagnostics::DiagnosticEmitter, build: bool) -> Result<()> {
        let _query_stats = query_stats::Session::enter();
        let _node_ids = parser::ast::NodeIdSession::enter();
        let src_path = PathBuf::from(self.src);
        let source = std::fs::read_to_string(&src_path)
            .with_context(|| format!("cannot read {}", src_path.display()))?;

        // Import resolution root: the directory containing the source file.
        let root = src_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let map = diagnostics::SourceMap::new(self.src, &source);

        let inputs = compiler_db::inputs::CompilerInputs::native(self.opts.clone(), root.clone())
            .resolve_project(self.project_root.as_deref())?;
        let frontend = run_frontend_with_inputs(&source, &root, &map, inputs, emitter)?;
        if build {
            run_backend(
                frontend, self.src, self.out, source, &self.opts, &map, emitter,
            )
        } else {
            Ok(())
        }
    }
}

/// Front-end phases: lex, parse, resolve imports, desugar interface inheritance
/// and default methods, then run the type checker and concurrency analyses.
/// Diagnostics are emitted as they are found; the phase aborts (returning `Err`)
/// if any stage produced an error, so a successful return yields a program that
/// is safe to hand to the back-end.
struct PhaseDiagnostics {
    diagnostics: Vec<diagnostics::Diagnostic>,
    error_count: usize,
}

impl PhaseDiagnostics {
    fn new(diagnostics: Vec<diagnostics::Diagnostic>) -> Self {
        let error_count = diagnostic_error_count(&diagnostics);
        Self {
            diagnostics,
            error_count,
        }
    }
}

struct ParsePhase {
    program: parser::ast::Program,
    outcome: PhaseDiagnostics,
}

#[cfg(test)]
struct ImportPhase {
    graph: module::ModuleGraph,
    item_imports: Vec<module::resolver::ItemImport>,
    outcome: PhaseDiagnostics,
}

struct TypecheckPhase {
    checker: LiveUnit<semantic::TypeChecker>,
    #[cfg(test)]
    error_count: usize,
}

fn run_frontend(
    source: &str,
    root: &std::path::Path,
    map: &diagnostics::SourceMap,
    options: &CompilerOptions,
) -> Result<Frontend> {
    run_frontend_with_emitter(source, root, map, options, &mut diagnostics::HumanEmitter)
}

fn run_frontend_with_emitter(
    source: &str,
    root: &std::path::Path,
    map: &diagnostics::SourceMap,
    options: &CompilerOptions,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> Result<Frontend> {
    run_frontend_with_inputs(
        source,
        root,
        map,
        compiler_db::inputs::CompilerInputs::native(options.clone(), root.to_path_buf()),
        emitter,
    )
}

fn run_frontend_with_inputs(
    source: &str,
    root: &std::path::Path,
    map: &diagnostics::SourceMap,
    inputs: compiler_db::inputs::CompilerInputs,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> Result<Frontend> {
    run_frontend_revision(source, root, map, inputs, emitter, None, false)
}

fn run_frontend_revision(
    source: &str,
    root: &std::path::Path,
    map: &diagnostics::SourceMap,
    inputs: compiler_db::inputs::CompilerInputs,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
    previous: Option<&Frontend>,
    incremental: bool,
) -> Result<Frontend> {
    let mut artifacts = module::artifacts::UnitArtifacts::new()?;
    artifacts.revision_enabled = incremental;
    if let Some(previous) = previous {
        let old = previous
            .module_graph
            .artifacts
            .as_ref()
            .expect("revision artifacts");
        artifacts.previous_parsed = Some((std::rc::Rc::clone(&old.store), old.parsed.clone()));
        artifacts
            .body_index_mut()
            .begin_revision(std::rc::Rc::clone(&old.bodies));
    }
    let ParsePhase {
        mut program,
        outcome: parse,
    } = match artifacts.cached_parse(diagnostics::FileId::ENTRY, source)? {
        Some(program) => ParsePhase {
            program,
            outcome: PhaseDiagnostics::new(Vec::new()),
        },
        None => {
            let tokens = match lex_phase(source) {
                Ok(tokens) => tokens,
                Err(errors) => {
                    for diagnostic in errors.iter() {
                        emitter.emit(diagnostic, map)?;
                    }
                    anyhow::bail!("aborting due to {} lexer error(s)", errors.len());
                }
            };
            parse_phase(tokens)
        }
    };
    for diagnostic in &parse.diagnostics {
        emitter.emit(diagnostic, map)?;
    }
    if incremental && parse.diagnostics.is_empty() {
        artifacts.retain_parse(diagnostics::FileId::ENTRY, source, &program)?;
    }
    artifacts.snapshot_source(diagnostics::FileId::ENTRY, source)?;
    artifacts.offload(&mut program)?;
    let resolution = module::resolver::resolve_imports_spooled_entry(
        &program,
        root,
        artifacts,
        std::fs::canonicalize(&map.path).ok(),
        inputs.package_graph.clone(),
        inputs.project_mode,
    );
    let mut graph = resolution.graph;
    let mut diagnostic_modules = DiagnosticModuleIndex::new(&graph);
    emit_frontend_diagnostics(
        &resolution.diagnostics,
        map,
        &graph,
        &diagnostic_modules,
        emitter,
    )?;
    let imports = PhaseDiagnostics::new(resolution.diagnostics);
    if imports.error_count != 0 {
        graph.files.clear();
        diagnostic_modules.positions.clear();
    }
    let desugar_dependencies =
        ModuleDependencies::with_packages(&graph.files, inputs.package_graph.as_deref());
    let desugar = PhaseDiagnostics::new(
        desugar::DesugarPass::run_resolved(
            &mut program,
            &mut graph.files,
            inputs.package_graph.as_ref().map(|_| &desugar_dependencies),
        )
        .diagnostics,
    );
    emit_frontend_diagnostics(
        &desugar.diagnostics,
        map,
        &graph,
        &diagnostic_modules,
        emitter,
    )?;
    let mut artifacts = graph.artifacts.take().expect("spooled import graph");
    if let Some(packages) = &inputs.package_graph {
        let package = packages.get(packages.root).expect("root package");
        let source_path =
            std::fs::canonicalize(&map.path).unwrap_or_else(|_| map.path.clone().into());
        let relative = source_path
            .strip_prefix(package.source_root())
            .or_else(|_| source_path.strip_prefix(&package.root))
            .unwrap_or(&source_path);
        let logical = relative.with_extension("");
        let mut components: Vec<_> = logical.iter().map(|s| s.to_string_lossy()).collect();
        if components.last().is_some_and(|s| s == "mod") {
            components.pop();
        }
        let path = components.join("::");
        artifacts.body_index_mut().set_symbol_module(
            module::UnitId::ENTRY,
            semantic::ids::SymbolModule::new(package.identity.clone(), module::ModulePath(path)),
        );
    }
    artifacts
        .body_index_mut()
        .register_unit(&mut program, module::UnitId::ENTRY);
    for module in &mut graph.files {
        if let Some(origin) = module.symbol_module {
            artifacts
                .body_index_mut()
                .set_symbol_module(module.id, origin);
        }
        artifacts
            .body_index_mut()
            .register_unit(&mut module.program, module.id);
    }
    artifacts.body_index_mut().finish_revision();
    artifacts.previous_parsed = None;
    let db = compiler_db::CompilerDb::with_dependencies(
        inputs,
        &graph.files,
        std::rc::Rc::clone(&artifacts.bodies),
        std::rc::Rc::clone(&artifacts.store),
        desugar_dependencies,
    );
    if let Some(previous) = previous {
        let reusable =
            compiler_db::revision::reusable_units(previous, &graph.files, &artifacts, &db);
        db.typed_bodies
            .reuse_from(&previous.db.typed_bodies, &reusable)?;
    }
    if db.inputs().capture_analysis {
        *db.effects.analysis.borrow_mut() = Some(Default::default());
    }
    let options = &db.inputs().options;
    let mut error_count = parse.error_count + imports.error_count + desugar.error_count;
    for module in &graph.files {
        db.check_unit(module.id, &mut artifacts, |artifacts| {
            let body = artifacts.hydrate(&module.program, module.id.file_id())?;
            let checker = check_module(&body, module, &graph.files, artifacts, &db)?;
            let local_helpers = db.nonpreemptible_helpers(module.id, &body)?;
            let mut concurrency = check_unit_concurrency(
                &body,
                &graph.files,
                db.dependencies(),
                &db.effects,
                Some((&local_helpers, db.inputs().target)),
            );
            concurrency.extend(db.typed_bodies.check_async_borrows(
                &body,
                &checker.expr_types,
                &checker.reference_arg_modes,
            )?);
            let mut checked = compiler_db::CheckedUnit::from(checker.into_inner());
            checked.diagnostics.extend(concurrency);
            Ok(checked)
        })?;
    }
    db.check_unit(module::UnitId::ENTRY, &mut artifacts, |artifacts| {
        let body = artifacts.hydrate(&program, diagnostics::FileId::ENTRY)?;
        let checked = typecheck_phase(&body, &graph.files, artifacts, options, Some(&db))?;
        let local_helpers = db.nonpreemptible_helpers(module::UnitId::ENTRY, &body)?;
        let mut concurrency = check_unit_concurrency(
            &body,
            &graph.files,
            db.dependencies(),
            &db.effects,
            Some((&local_helpers, db.inputs().target)),
        );
        concurrency.extend(db.typed_bodies.check_async_borrows(
            &body,
            &checked.checker.expr_types,
            &checked.checker.reference_arg_modes,
        )?);
        let mut checked = compiler_db::CheckedUnit::from(checked.checker.into_inner());
        checked.diagnostics.extend(concurrency);
        Ok(checked)
    })?;
    graph.artifacts = Some(artifacts);
    for file in graph
        .files
        .iter()
        .map(|m| m.id)
        .chain(std::iter::once(module::UnitId::ENTRY))
    {
        let diagnostics = db.unit_diagnostics(file)?;
        error_count += diagnostic_error_count(&diagnostics);
        emit_frontend_diagnostics(&diagnostics, map, &graph, &diagnostic_modules, emitter)?;
    }
    let entry = validate_entry_point(&program);
    error_count += diagnostic_error_count(&entry);
    emit_frontend_diagnostics(&entry, map, &graph, &diagnostic_modules, emitter)?;
    if error_count > 0 {
        if let Some(error) = graph.package_import_error.take() {
            return Err(anyhow::Error::new(error)
                .context(format!("aborting due to {error_count} error(s)")));
        }
        anyhow::bail!("aborting due to {} error(s)", error_count);
    }
    Ok(Frontend {
        program,
        module_graph: graph,
        db,
    })
}

/// Lexing is the only hard-stop front-end phase: parsing cannot proceed
/// without a token stream.
fn lex_phase(source: &str) -> std::result::Result<Vec<lexer::token::Token>, errors::LexError> {
    lexer::Lexer::new(source).tokenize()
}

/// Parse into a partial AST and retain all parser diagnostics for downstream
/// aggregation.
fn parse_phase(tokens: Vec<lexer::token::Token>) -> ParsePhase {
    let (program, diagnostics) = parser::Parser::new(tokens).parse();
    ParsePhase {
        program,
        outcome: PhaseDiagnostics::new(diagnostics),
    }
}

/// Resolve imports while preserving diagnostics. Failed import resolution
/// yields no modules or item bindings, matching the previous pipeline policy.
#[cfg(test)]
fn import_phase(program: &parser::ast::Program, root: &std::path::Path) -> ImportPhase {
    let resolution = module::resolve_imports(program, root);
    let outcome = PhaseDiagnostics::new(resolution.diagnostics);
    let item_imports = if outcome.error_count == 0 {
        resolution.item_imports
    } else {
        vec![]
    };
    ImportPhase {
        graph: resolution.graph,
        item_imports,
        outcome,
    }
}

/// Compose interface inheritance and inject default methods across the entry
/// program and all imported modules.
#[cfg(test)]
fn desugar_phase(
    program: &mut parser::ast::Program,
    modules: &mut [module::ResolvedModule],
) -> PhaseDiagnostics {
    let output = desugar::DesugarPass::run(program, modules);
    PhaseDiagnostics::new(output.diagnostics)
}

/// Register prelude/module symbols and type-check the entry program.
fn typecheck_phase(
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    artifacts: &UnitArtifacts,
    options: &CompilerOptions,
    queries: Option<&compiler_db::CompilerDb>,
) -> Result<TypecheckPhase> {
    let db = queries;
    let mut checker = semantic::TypeChecker::new();
    if let Some(db) = queries {
        checker = checker.with_sync_stack_preemption(db.inputs().target.sync_stack_preemption);
        checker.set_effect_queries(std::rc::Rc::clone(&db.effects), module::UnitId::ENTRY);
        checker.set_body_queries(std::rc::Rc::clone(&db.typed_bodies));
        checker
            .set_declaration_queries(std::rc::Rc::clone(&db.declarations), module::UnitId::ENTRY);
    }
    if options.enforce_send_sync {
        checker.set_enforce_send_sync(true);
    }
    register_prelude(&mut checker)?;
    if let Some(db) = queries {
        db.declarations.set_prelude(&checker.symbols);
    }
    let fallback;
    let dependencies = match db {
        Some(db) => db.dependencies(),
        None => {
            fallback = ModuleDependencies::with_packages(modules, None);
            &fallback
        }
    };
    let bindings = ModuleImportBindings::new(program, dependencies);
    for (index, m) in modules.iter().enumerate() {
        prepare_module_registration(&mut checker, m, modules, dependencies);
        checker.register_module_with_id(
            m.id,
            m.registration_name(),
            m.identity_path(),
            &m.path.to_string_lossy(),
            &m.program,
        );
        // Reuse the first registration's types for every consumer spelling.
        // Resolve against package/module identity, never the graph's first alias.
        if let Some(spellings) = bindings.imported.get(&index) {
            for spelling in spellings {
                checker.alias_module_spelling(m.id, spelling, m.registration_name(), &m.program);
            }
        }
    }
    for &(local, module, item, span) in &bindings.items {
        checker.register_item_import(local, module, item, span);
    }
    // Preserve build-wide type identities, but expose only this file's names.
    let visible: std::collections::HashSet<&str> =
        bindings.imported.values().flatten().copied().collect();
    for module in modules {
        for spelling in [&module.name, &module.canonical_path] {
            if !visible.contains(spelling.as_str()) {
                checker.hide_module_spelling(spelling);
            }
        }
    }

    // Without a session there are no checked dependencies to import from.
    if let Some(db) = db {
        checker.set_nonpreemptible_module_methods(imported_nonpreemptible_method_owners(
            program,
            modules,
            &db.effects,
            db.dependencies(),
        ));
    }
    checker.check_program(program);
    checker.finish_body_queries()?;
    #[cfg(test)]
    let error_count = diagnostic_error_count(&checker.errors);
    Ok(TypecheckPhase {
        checker: artifacts.track(UnitKind::Checker, checker),
        #[cfg(test)]
        error_count,
    })
}

fn check_module(
    body: &parser::ast::Program,
    module: &module::ResolvedModule,
    modules: &[module::ResolvedModule],
    artifacts: &UnitArtifacts,
    db: &compiler_db::CompilerDb,
) -> Result<LiveUnit<semantic::TypeChecker>> {
    let options = &db.inputs().options;
    let dependencies = db.dependencies();
    let mut checker = semantic::TypeChecker::new()
        .with_sync_stack_preemption(db.inputs().target.sync_stack_preemption);
    checker.set_effect_queries(std::rc::Rc::clone(&db.effects), module.id);
    checker.set_body_queries(std::rc::Rc::clone(&db.typed_bodies));
    checker.set_declaration_queries(std::rc::Rc::clone(&db.declarations), module.id);
    checker.set_enforce_send_sync(options.enforce_send_sync);
    register_prelude(&mut checker)?;
    db.declarations.set_prelude(&checker.symbols);
    let needed = dependencies.unit_closure(module.id, artifacts)?;
    register_module_imports_with_closure(&mut checker, body, modules, dependencies, Some(&needed));
    checker.set_module_path(module.identity_path());
    checker.set_nonpreemptible_module_methods(imported_nonpreemptible_method_owners(
        body,
        modules,
        &db.effects,
        dependencies,
    ));
    checker.check_module_program(body);
    checker.finish_body_queries()?;
    Ok(artifacts.track(UnitKind::Checker, checker))
}

/// Consumer-local source spellings, indexed once by the resolved module index.
/// Aliases retain source order and share a registration for the same ModuleId.
struct ModuleImportBindings<'a> {
    imported: std::collections::HashMap<usize, Vec<&'a str>>,
    items: Vec<(&'a str, &'a str, &'a str, diagnostics::Span)>,
    #[cfg(test)]
    path_lookups: usize,
}

impl<'a> ModuleImportBindings<'a> {
    fn new(program: &'a parser::ast::Program, dependencies: &ModuleDependencies) -> Self {
        let by_path = dependencies.paths_for(program);
        // `(resolved index, consumer spelling)` for each module this program imports.
        // A module can appear twice under two spellings -- `import base as b;` next
        // to `import base::Parcel;` -- and then it is registered under both, since
        // the item lookup resolves against the full consumer path.
        let mut imported: std::collections::HashMap<usize, Vec<&str>> =
            std::collections::HashMap::new();
        let mut spellings_seen = std::collections::HashSet::new();
        let mut items: Vec<(&str, &str, &str, diagnostics::Span)> = Vec::new();
        #[cfg(test)]
        let mut path_lookups = 0;
        for import in &program.imports {
            let path = import.path.as_str();
            // Whole module: `import worker;`, `import a::b as c;`.
            #[cfg(test)]
            {
                path_lookups += 1;
            }
            if let Some(&id) = by_path.get(path) {
                let access = import
                    .alias
                    .as_deref()
                    .unwrap_or_else(|| path.rsplit("::").next().unwrap_or(path));
                if spellings_seen.insert((id, access)) {
                    imported.entry(id).or_default().push(access);
                }
                continue;
            }
            // Single item: `import math::add;`, `import math::add as plus;`. The
            // module also answers to the consumer's full path so the item
            // lookup can find it without losing the package alias.
            let Some((module_path, item)) = path.rsplit_once("::") else {
                continue;
            };
            #[cfg(test)]
            {
                path_lookups += 1;
            }
            let Some(&id) = by_path.get(module_path) else {
                continue;
            };
            if spellings_seen.insert((id, module_path)) {
                imported.entry(id).or_default().push(module_path);
            }
            let local = import.alias.as_deref().unwrap_or(item);
            items.push((local, module_path, item, import.span));
        }

        Self {
            imported,
            items,
            #[cfg(test)]
            path_lookups,
        }
    }
}

/// Bring the modules `program` itself imports into `checker`'s scope.
///
/// Each source unit sees exactly what its own `import` lines name, under the
/// name it gave them, matching the backend when it compiles that body.
///
/// What an imported module's public signature NAMES is another matter: `mid`
/// may export `Crate extends Parcel` with `Parcel` declared in `base`, and a
/// base class that resolves to nothing takes every inherited member with it
/// (willow-sxcp). So the dependency closure is registered too, but by types
/// only -- see `register_module_type_signatures`. Registration follows graph
/// order, which is dependency order, because a module's exported signature is
/// qualified against the spellings its own dependencies already answer to.
#[cfg(test)]
fn register_module_imports(
    checker: &mut semantic::TypeChecker,
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    dependencies: &ModuleDependencies,
) {
    register_module_imports_with_closure(checker, program, modules, dependencies, None)
}

fn prepare_module_registration(
    checker: &mut semantic::TypeChecker,
    module: &module::ResolvedModule,
    modules: &[module::ResolvedModule],
    dependencies: &ModuleDependencies,
) {
    if module.symbol_module.is_none() {
        return;
    }
    let paths = dependencies.paths_for(&module.program);
    let mut names = std::collections::HashMap::new();
    for import in &module.program.imports {
        let path = import.path.as_str();
        let target = paths.get(path).map(|&id| (path, id)).or_else(|| {
            let (parent, _) = path.rsplit_once("::")?;
            paths.get(parent).map(|&id| (parent, id))
        });
        if let Some((path, id)) = target {
            names.insert(
                path.to_string(),
                modules[id].registration_name().to_string(),
            );
        }
    }
    checker.set_registration_import_paths(names);
}

fn register_module_imports_with_closure(
    checker: &mut semantic::TypeChecker,
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    dependencies: &ModuleDependencies,
    needed: Option<&[usize]>,
) {
    let ModuleImportBindings {
        imported, items, ..
    } = ModuleImportBindings::new(program, dependencies);

    let computed;
    let needed = match needed {
        Some(needed) => needed,
        None => {
            computed = dependencies.reachable(imported.keys().copied());
            &computed
        }
    };
    for &id in needed {
        let dep = &modules[id];
        prepare_module_registration(checker, dep, modules, dependencies);
        let canonical = dep.identity_path();
        let dep_path = dep.path.to_string_lossy();
        let Some(spellings) = imported.get(&id) else {
            checker.register_module_type_signatures(canonical, &dep_path, &dep.program);
            continue;
        };
        // Keep source spelling order and bind aliases to the first registration.
        let mut spellings = spellings.iter().copied();
        let first = spellings.next().expect("import has a spelling");
        let registered = if dep.symbol_module.is_some() {
            dep.registration_name()
        } else {
            first
        };
        checker.register_module_with_id(
            dep.id,
            registered,
            dep.identity_path(),
            &dep_path,
            &dep.program,
        );
        for access in std::iter::once(first).chain(spellings) {
            checker.alias_module_spelling(dep.id, access, registered, &dep.program);
        }
    }

    for (local, module_path, item, span) in items {
        checker.register_item_import(local, module_path, item, span);
    }
}

#[cfg(test)]
thread_local! {
    static DEPENDENCY_WORK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

/// Index non-preemptible methods visible through one module's own imports.
///
/// `check_module_program` computes the module's local helper graph itself, but
/// typed receiver calls into another module need the imported method-owner map
/// that the entry checker is also seeded with. The keys use the exact access
/// spelling of this module: an alias namespace for whole-module imports, or the
/// local class name for a direct item import.
fn imported_nonpreemptible_method_owners(
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    effects: &compiler_db::effects::EffectQueries,
    dependencies: &ModuleDependencies,
) -> std::collections::HashMap<
    semantic::ids::FunctionId,
    (String, semantic::concurrency::NonpreemptibleReason),
> {
    let by_path = dependencies.paths_for(program);
    let mut out = std::collections::HashMap::new();
    for import in &program.imports {
        let (dependency, access, direct_item) = if let Some(dependency) =
            by_path.get(&import.path).map(|&index| &modules[index])
        {
            let access = import.alias.as_deref().unwrap_or_else(|| {
                import
                    .path
                    .rsplit("::")
                    .next()
                    .unwrap_or(import.path.as_str())
            });
            (dependency, access, None)
        } else {
            let Some((module_path, item)) = import.path.rsplit_once("::") else {
                continue;
            };
            let Some(dependency) = by_path.get(module_path).map(|&index| &modules[index]) else {
                continue;
            };
            (
                dependency,
                import.alias.as_deref().unwrap_or(item),
                Some(item),
            )
        };

        let helpers = effects.completed_helpers(dependency.id);
        for (key, helper) in helpers.iter().flat_map(|index| index.iter()) {
            if key.owner().is_none() {
                continue;
            }
            let visible_key = if let Some(item) = direct_item {
                let Some(remapped) = key.remap_imported_item(item, access) else {
                    continue;
                };
                remapped
            } else {
                (*key).in_namespace(access)
            };
            out.insert(
                visible_key,
                (dependency.registration_name().to_string(), helper.reason),
            );
            if direct_item.is_none() || dependency.symbol_module.is_some() {
                // Normalized package types retain their declaring module identity.
                out.insert(
                    (*key).in_namespace(dependency.registration_name()),
                    (dependency.registration_name().to_string(), helper.reason),
                );
            }
        }
    }
    out
}

/// Seed concurrency analysis from immutable body-free helper summaries of
/// the already-checked units this program imports.
fn check_unit_concurrency(
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    dependencies: &ModuleDependencies,
    effects: &compiler_db::effects::EffectQueries,
    local_helpers: Option<(
        &compiler_db::HelperSummary,
        compiler_db::inputs::TargetCapabilities,
    )>,
) -> Vec<diagnostics::Diagnostic> {
    let by_path = dependencies.paths_for(program);
    let bindings = ModuleImportBindings::new(program, dependencies);
    let mut analyzer = semantic::ConcurrencyAnalyzer::new();
    if let Some((_, target)) = local_helpers {
        analyzer = analyzer.with_sync_stack_preemption(target.sync_stack_preemption);
    }
    // Retain one borrowed summary for each source binding for the analyzer run.
    let mut held = Vec::new();
    for (id, spellings) in &bindings.imported {
        if let Some(index) = effects.completed_helpers(modules[*id].id) {
            for &access in spellings {
                held.push((access, None, std::sync::Arc::clone(&index)));
            }
        }
    }
    for &(local, path, item, _) in &bindings.items {
        if let Some(&id) = by_path.get(path)
            && let Some(index) = effects.completed_helpers(modules[id].id)
        {
            held.push((local, Some((item, path)), index));
        }
    }
    for (access, item, index) in &held {
        analyzer = match item {
            Some((item, path)) => analyzer.with_item_helper_index(access, item, path, index),
            None => analyzer.with_module_helper_index(access, index),
        };
    }
    match local_helpers {
        Some((helpers, _)) => analyzer.check_program_with_helpers(program, helpers).errors,
        None => analyzer.check_program(program).errors,
    }
}

#[cfg(test)]
mod diagnostic_emission_tests {
    use super::*;
    use diagnostics::{
        Diagnostic, DiagnosticEmitter, ErrorCode, FileId, FixSuggestion, Severity, SourceMap,
        source_map::SourceLookup,
    };

    #[test]
    fn module_index_probes_scale_with_references_across_batches() {
        struct Inspect;
        impl DiagnosticEmitter for Inspect {
            fn emit(
                &mut self,
                diagnostic: &Diagnostic,
                sources: &dyn SourceLookup,
            ) -> std::io::Result<()> {
                let id = diagnostic.fix_suggestions[0].span.file_id;
                assert_eq!(sources.get(id).unwrap().source, "é");
                assert!(sources.get(FileId::ENTRY).is_some());
                Ok(())
            }
        }
        for n in [16, 64, 256, 1024] {
            let mut graph = module::ModuleGraph::default();
            for i in 0..n {
                graph.files.push(module::ResolvedModule {
                    package: crate::package::PackageId(0),
                    symbol_module: None,
                    id: module::ModuleId(i + 1),
                    name: format!("m{i}"),
                    canonical_path: format!("m{i}"),
                    path: format!("m{i}.wi").into(),
                    source: "é".into(),
                    program: parser::ast::Program {
                        type_uses: Vec::new(),
                        module: None,
                        imports: vec![],
                        items: vec![],
                    },
                });
            }
            let index = DiagnosticModuleIndex::new(&graph);
            let entry = SourceMap::new("main.wi", "fn main() {}");
            for module in &graph.files {
                let diagnostic = Diagnostic::new(Severity::Warning, ErrorCode::W2002, "fix")
                    .with_fix(FixSuggestion::new(
                        diagnostics::Span::in_file(module.id.file_id(), 0, 2, 1, 1),
                        "e",
                        "replace",
                    ));
                // Duplicate references within a batch still load only one source.
                emit_frontend_diagnostics(
                    &[diagnostic.clone(), diagnostic],
                    &entry,
                    &graph,
                    &index,
                    &mut Inspect,
                )
                .unwrap();
            }
            assert_eq!(index.positions.len(), n as usize);
            assert_eq!(index.probes.get(), n as usize);
            assert_eq!(index.lookup(FileId::ENTRY), None);
            eprintln!(
                "module-index modules={n} batches={n} references={} probes={n}",
                2 * n
            );
        }
    }

    #[test]
    fn batches_borrow_entry_and_share_sources_referenced_only_by_fixes() {
        struct Inspect<'a> {
            entry: &'a SourceMap,
            imported: Option<usize>,
            emissions: usize,
        }
        impl DiagnosticEmitter for Inspect<'_> {
            fn emit(
                &mut self,
                diagnostic: &Diagnostic,
                sources: &dyn SourceLookup,
            ) -> std::io::Result<()> {
                assert!(std::ptr::eq(
                    sources.get(FileId::ENTRY).unwrap(),
                    self.entry
                ));
                let fix = &diagnostic.fix_suggestions[0];
                let source = sources.get(fix.span.file_id).expect("fix-only source");
                assert_eq!(source.path, "helper.wi");
                assert_eq!(source.source, "é");
                let address = source as *const SourceMap as usize;
                if let Some(previous) = self.imported {
                    assert_eq!(address, previous, "one source map per batch");
                }
                self.imported = Some(address);
                self.emissions += 1;
                Ok(())
            }
        }

        let entry = SourceMap::new("main.wi", "fn main() {}");
        let mut graph = module::ModuleGraph::default();
        graph.files.push(module::ResolvedModule {
            package: crate::package::PackageId(0),
            symbol_module: None,
            id: module::ModuleId(17),
            name: "helper".into(),
            canonical_path: "helper".into(),
            path: "helper.wi".into(),
            source: "é".into(),
            program: parser::ast::Program {
                type_uses: Vec::new(),
                module: None,
                imports: vec![],
                items: vec![],
            },
        });
        for n in [16, 64, 256, 1024] {
            let diagnostic = Diagnostic::new(Severity::Warning, ErrorCode::W2002, "fix").with_fix(
                FixSuggestion::new(
                    diagnostics::Span::in_file(module::ModuleId(17).file_id(), 0, 2, 1, 1),
                    "e",
                    "replace",
                ),
            );
            let mut inspect = Inspect {
                entry: &entry,
                imported: None,
                emissions: 0,
            };
            emit_frontend_diagnostics(
                &vec![diagnostic; n],
                &entry,
                &graph,
                &DiagnosticModuleIndex::new(&graph),
                &mut inspect,
            )
            .unwrap();
            assert_eq!(inspect.emissions, n);
            eprintln!(
                "batch-count n={n} emissions={} imported_maps=1 entry_borrowed=true",
                inspect.emissions
            );
        }
    }
}

/// Request-local positions stay valid while desugaring/checking module bodies.
/// Clearing the graph on import failure also clears this index.
struct DiagnosticModuleIndex {
    positions: std::collections::HashMap<diagnostics::FileId, usize>,
    #[cfg(test)]
    probes: std::cell::Cell<usize>,
}

impl DiagnosticModuleIndex {
    fn new(graph: &module::ModuleGraph) -> Self {
        Self {
            positions: graph
                .files
                .iter()
                .enumerate()
                .map(|(position, module)| (module.id.file_id(), position))
                .collect(),
            #[cfg(test)]
            probes: std::cell::Cell::new(0),
        }
    }

    fn lookup(&self, id: diagnostics::FileId) -> Option<usize> {
        #[cfg(test)]
        self.probes.set(self.probes.get() + 1);
        self.positions.get(&id).copied()
    }
}

/// Render only the sources a diagnostic actually references. Successful builds
/// never materialize a build-wide collection of source strings/source maps.
fn emit_frontend_diagnostics(
    diagnostics: &[diagnostics::Diagnostic],
    entry: &diagnostics::SourceMap,
    graph: &module::ModuleGraph,
    modules: &DiagnosticModuleIndex,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> Result<()> {
    if diagnostics.is_empty() {
        return Ok(());
    }
    // Load each referenced file once per diagnostic batch, including fixes
    // whose source is different from every label. Never clone the entry text.
    let ids: std::collections::HashSet<_> = diagnostics
        .iter()
        .flat_map(|diagnostic| {
            diagnostic
                .labels
                .iter()
                .map(|label| label.span.file_id)
                .chain(
                    diagnostic
                        .fix_suggestions
                        .iter()
                        .map(|fix| fix.span.file_id),
                )
        })
        .collect();
    let mut imports = diagnostics::SourceMaps::default();
    for id in ids {
        if let Some(position) = modules.lookup(id) {
            let module = &graph.files[position];
            let source = if module.source.is_empty() {
                graph
                    .artifacts
                    .as_ref()
                    .expect("spooled sources")
                    .source(module.id.file_id())?
            } else {
                module.source.clone()
            };
            imports.insert(diagnostics::SourceMap::with_file_id(
                module.id.file_id(),
                module.path.to_string_lossy().into_owned(),
                source,
            ));
        }
    }
    struct Sources<'a> {
        entry: &'a diagnostics::SourceMap,
        imports: diagnostics::SourceMaps,
    }
    impl diagnostics::source_map::SourceLookup for Sources<'_> {
        fn get(&self, file_id: diagnostics::FileId) -> Option<&diagnostics::SourceMap> {
            if file_id == self.entry.file_id {
                Some(self.entry)
            } else {
                self.imports.get(file_id)
            }
        }
    }
    let sources = Sources { entry, imports };
    for diagnostic in diagnostics {
        emitter.emit(diagnostic, &sources)?;
    }
    Ok(())
}

/// One unit's imports in the shape the back end wants them, from the module
/// resolver's own classification (willow-vtlr, willow-28h8).
///
/// Only the resolver knows whether `import a::b;` named a module file or an
/// item of `a`, and the difference decides both halves: an item import leaves
/// `a` unnameable in this file, and binds a local name that another file may
/// bind to a different module's function.
fn backend_unit_imports(
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    dependencies: &ModuleDependencies,
) -> backend::cranelift::UnitImports {
    let by_path = dependencies.paths_for(program);
    let classified = module::resolver::classify_unit_imports_with(program, |path| {
        by_path.get(path).map(|&index| &modules[index])
    });
    let mut visible_modules = std::collections::HashSet::new();
    let mut module_spellings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for binding in &classified.modules {
        if !seen.insert((binding.unit, &binding.access)) {
            continue;
        }
        let dependency = &modules[dependencies
            .index_for_unit(binding.unit)
            .expect("resolved module")];
        let graph_name = dependency.registration_name();
        // Both spellings: this file writes `access`, while the back end's
        // module tables are keyed by the name the graph registered, which is
        // the first importer's alias when that was another file.
        visible_modules.insert(binding.access.clone());
        visible_modules.insert(graph_name.to_string());
        // Visibility alone was not enough: the tables are keyed by ONE of the
        // two spellings, so the other has to be bound to it for this unit's
        // phase (willow-kd1v). The types worth binding are the ones the module
        // itself declares, which is why this is built here rather than in the
        // back end — only the driver holds the imported module's program.
        if binding.access == graph_name {
            continue;
        }
        let Some(dependency) = dependencies
            .index_for_unit(binding.unit)
            .map(|index| &modules[index])
        else {
            continue;
        };
        module_spellings.push(backend::cranelift::ModuleSpelling {
            access: binding.access.clone(),
            graph_name: graph_name.to_string(),
            canonical_path: dependency.identity_path().to_string(),
            types: dependency
                .program
                .items
                .iter()
                .filter_map(|item| match item {
                    parser::ast::Item::Class(c) => Some(c.name.clone()),
                    parser::ast::Item::Enum(e) => Some(e.name.clone()),
                    parser::ast::Item::Interface(i) => Some(i.name.clone()),
                    parser::ast::Item::Function(_) => None,
                })
                .collect(),
        });
    }
    backend::cranelift::UnitImports {
        visible_modules,
        item_imports: classified
            .items
            .iter()
            .map(|item| backend::cranelift::ItemBinding {
                local: item.local.clone(),
                module: modules[dependencies
                    .index_for_module(item.package, &item.canonical_module)
                    .expect("resolved item module")]
                .identity_path()
                .to_string(),
                item: item.item.clone(),
            })
            .collect(),
        module_spellings,
    }
}

/// Report, under `WILLOW_LIR_LOG=1`, the functions HIR lowering could not build
/// at all.
///
/// A gap here is invisible from the walker's side: the function simply has no
/// entry in the LIR table, so the compile error can only say "has no lowered
/// IR" without saying WHY. These diagnostics carry the why. They are not
/// errors themselves — the error is raised where the body is wanted, by
/// `compile_function_named` (willow-0g8j.3).
fn log_hir_gaps(gaps: &[diagnostics::Diagnostic]) {
    if gaps.is_empty() || std::env::var("WILLOW_LIR_LOG").is_err() {
        return;
    }
    for gap in gaps {
        eprintln!("[lir] hir gap: {}", gap.message);
    }
}

/// Back-end phases: drive Cranelift codegen over the modules and entry program,
/// emit the object file, resolve the runtime library, link the native
/// executable, and write debug/source-map artifacts.
fn run_backend(
    frontend: Frontend,
    src: &str,
    out: &str,
    source: String,
    opts: &CompilerOptions,
    map: &diagnostics::SourceMap,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> Result<()> {
    use diagnostics::{Diagnostic, ErrorCode, Severity};
    use toolchain::{HostToolchain, Toolchain};

    // The entry file's item imports are not carried here: the back end gets
    // them from `backend_unit_imports` below, the same way every module's are.
    let Frontend {
        program,
        mut module_graph,
        db,
    } = frontend;
    let module_init_plan = ir::module_init::ModuleInitPlan::from_graph(&module_graph);
    let artifacts = module_graph.artifacts.take().expect("spooled frontend");
    let modules = module_graph.files;
    // Debug metadata is read from the same hydrated trees the declare pass
    // uses, so a debug build hydrates each unit no more often than a release
    // build (willow-afb5.13). Entry first, then modules in graph order.
    let emit_debug_metadata = opts.target.emit_debug_info || opts.target.emit_source_map;
    let mut module_debug_metadata = Vec::new();
    let mut codegen =
        backend::Codegen::new(opts, std::rc::Rc::clone(&db.layouts)).map_err(|error| {
            emit_codegen_error(
                errors::CodegenError::new(errors::CodegenStage::Initialize, error),
                map,
                emitter,
            )
        })?;
    codegen.body_queries = Some(std::rc::Rc::clone(&db.typed_bodies));
    codegen.lir_queries = Some(std::rc::Rc::clone(&db.lir));
    codegen.set_module_init_plan(module_init_plan);
    // Shared declaration metadata comes from the checked entry artifact.
    {
        let checked = db.unit_declarations(module::UnitId::ENTRY, &artifacts)?;
        for (name, info) in &checked.symbols.enums {
            codegen.register_enum_info(name.to_string(), info.to_semantic());
        }
        for (name, info) in &checked.symbols.interfaces {
            let identity = semantic::ids::TypeId::from_source_name(&info.name);
            codegen.register_interface_info(name.to_string(), identity, || info.to_semantic())?;
        }
    }
    // Declare every unit before emitting any body: later overrides must be
    // visible to devirtualization in earlier modules. Each declaration artifact
    // is written and dropped immediately, preserving its lambda symbols/IDs.
    let mut declared_modules = Vec::with_capacity(modules.len());
    for module in &modules {
        let body = artifacts.hydrate(&module.program, module.id.file_id())?;
        if emit_debug_metadata {
            let source = artifacts.source(module.id.file_id())?;
            let source_map =
                diagnostics::SourceMap::new(module.path.to_string_lossy().into_owned(), source);
            module_debug_metadata.push(
                diagnostics::DebugSourceMap::from_program(
                    &source_map.path,
                    source_map.total_lines(),
                    &body,
                )
                .to_text(),
            );
        }
        let checker = db.checked_unit(module.id, &artifacts)?;
        for info in checker.symbols.enums.values() {
            codegen.register_enum_info(info.name.clone(), info.to_semantic());
        }
        let expr_types = checker
            .expr_types
            .iter()
            .map(|(id, ty)| (*id, ty.into()))
            .collect();
        let scope = db.unit_scope(module.id, &body, &modules, &checker.symbols)?;
        codegen.effect_queries = Some((std::rc::Rc::clone(&db.effects), module.id));
        let declared = codegen.declare_module_with_types(
            module.registration_name(),
            module.identity_path(),
            &body,
            &module.path.to_string_lossy(),
            &expr_types,
            scope,
        );
        let unit = declared.map_err(|error| {
            report_backend_failure(
                &mut codegen,
                errors::CodegenError::new(errors::CodegenStage::Module(module.name.clone()), error),
                map,
                emitter,
                &artifacts,
            )
        })?;
        let unit = artifacts.track(UnitKind::Declared, unit);
        declared_modules.push(artifacts.write(&*unit)?);
    }
    let mut debug_metadata = None;
    let entry_artifact = {
        let body = artifacts.hydrate(&program, diagnostics::FileId::ENTRY)?;
        if emit_debug_metadata {
            let mut text =
                diagnostics::DebugSourceMap::from_program(&map.path, map.total_lines(), &body)
                    .to_text();
            for module in module_debug_metadata.drain(..) {
                text.push_str("\n---\n");
                text.push_str(&module);
            }
            debug_metadata = Some(text);
        }
        let checked = db.checked_unit(module::UnitId::ENTRY, &artifacts)?;
        let scope = db.unit_scope(module::UnitId::ENTRY, &body, &modules, &checked.symbols)?;
        let expr_types = checked
            .expr_types
            .iter()
            .map(|(id, ty)| (*id, ty.into()))
            .collect();
        codegen.effect_queries = Some((std::rc::Rc::clone(&db.effects), module::UnitId::ENTRY));
        let declared = codegen.declare_program_with_types(&body, src, &expr_types, scope);
        let unit = declared.map_err(|error| {
            report_backend_failure(
                &mut codegen,
                errors::CodegenError::new(errors::CodegenStage::Entry, error),
                map,
                emitter,
                &artifacts,
            )
        })?;
        let unit = artifacts.track(UnitKind::Declared, unit);
        artifacts.write(&*unit)?
    };
    for (module, artifact) in modules.iter().zip(declared_modules) {
        let unit: backend::cranelift::DeclaredModule = artifacts.read(artifact)?;
        let unit = artifacts.track(UnitKind::Declared, unit);
        let _lir = artifacts.live(UnitKind::Lir);
        {
            let checker = db.checked_unit(module.id, &artifacts)?;
            // ANF declarations may hoist a lambda before an await and create
            // fresh temporary IDs. Preserve source resolutions/captures, but
            // lower the exact declared tree with its extended payload types.
            let mut tables = checker.tables();
            tables.expr_types = Some(unit.normalized_expr_types());
            log_hir_gaps(&db.lir.lower_unit(
                module.id,
                unit.normalized_program(),
                db.bodies(),
                &tables,
            )?);
        }
        // Emission is addressed one body at a time: each target names the
        // semantic body whose lowered IR the artifact store holds, so no
        // whole-unit IR is materialized here (willow-afb5.18).
        let plan = codegen.module_body_plan(&unit);
        let compiled = codegen.with_module_bodies(&unit, |backend| {
            for target in &plan {
                backend.compile_body(target)?;
            }
            Ok(())
        });
        compiled.map_err(|error| {
            report_backend_failure(
                &mut codegen,
                errors::CodegenError::new(errors::CodegenStage::Module(module.name.clone()), error),
                map,
                emitter,
                &artifacts,
            )
        })?;
    }
    let entry_unit: backend::cranelift::DeclaredProgram = artifacts.read(entry_artifact)?;
    let entry_unit = artifacts.track(UnitKind::Declared, entry_unit);
    let _entry_lir = artifacts.live(UnitKind::Lir);
    {
        let checked = db.checked_unit(module::UnitId::ENTRY, &artifacts)?;
        let mut tables = checked.tables();
        tables.expr_types = Some(entry_unit.normalized_expr_types());
        log_hir_gaps(&db.lir.lower_unit(
            module::UnitId::ENTRY,
            entry_unit.normalized_program(),
            db.bodies(),
            &tables,
        )?);
    }
    let plan = codegen.program_body_plan(&entry_unit);
    let compiled = codegen.with_program_bodies(&entry_unit, |backend| {
        for target in &plan {
            backend.compile_body(target)?;
        }
        Ok(())
    });
    compiled.map_err(|error| {
        report_backend_failure(
            &mut codegen,
            errors::CodegenError::new(errors::CodegenStage::Entry, error),
            map,
            emitter,
            &artifacts,
        )
    })?;
    drop(entry_unit);

    let warnings = codegen.take_async_frame_size_warnings();
    // Index once and load each warned file once, even with many large frames.
    let module_paths: std::collections::HashMap<_, _> = if warnings.is_empty() {
        Default::default()
    } else {
        modules
            .iter()
            .map(|module| (module.path.to_string_lossy(), module.id))
            .collect()
    };
    let mut warning_maps = std::collections::HashMap::new();
    for warning in &warnings {
        let warning_map = match warning_maps.entry(warning.source_file.as_str()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let warning_source = if warning.source_file == src {
                    source.clone()
                } else {
                    module_paths
                        .get(warning.source_file.as_str())
                        .map(|id| artifacts.source(id.file_id()))
                        .transpose()?
                        .unwrap_or_default()
                };
                entry.insert(diagnostics::SourceMap::with_file_id(
                    warning.span.file_id,
                    &warning.source_file,
                    warning_source,
                ))
            }
        };
        let point_span = diagnostics::Span::in_file(
            warning.span.file_id,
            warning.span.start,
            warning.span.start.saturating_add(1),
            warning.span.line,
            warning.span.col,
        );
        let diagnostic = Diagnostic::new(
            Severity::Warning,
            ErrorCode::W0801,
            format!(
                "async frame for `{}` is large: {} bytes",
                warning.function_name, warning.size_bytes
            ),
        )
        .with_label(diagnostics::Label::primary(
            point_span,
            "large async frame allocated here",
        ))
        .with_help("avoid keeping large arrays or objects live across await points");
        emitter.emit(&diagnostic, warning_map)?;
    }

    if opts.target.emit_debug_info {
        codegen
            .embed_runtime_metadata(debug_metadata.as_deref().unwrap_or(""))
            .map_err(|error| {
                emit_codegen_error(
                    errors::CodegenError::new(errors::CodegenStage::Metadata, error),
                    map,
                    emitter,
                )
            })?;
    }

    let obj_bytes = codegen.finish().map_err(|error| {
        emit_codegen_error(
            errors::CodegenError::new(errors::CodegenStage::Finish, error),
            map,
            emitter,
        )
    })?;

    let toolchain = HostToolchain::new(&opts.target);
    let obj_path = toolchain.write_object(out, &obj_bytes)?;
    // The object is an intermediate file, deleted as soon as it has been
    // linked. `WILLOW_KEEP_OBJECT=1` keeps it, which is how a test can assert
    // on what the backend actually emitted — the imported-symbol list of the
    // object is the only place a runtime call is visible, since the linked
    // binary also contains everything the runtime staticlib defines.
    let keep_object = std::env::var_os("WILLOW_KEEP_OBJECT").is_some_and(|value| value != "0");
    let discard_object = |path: &Path| {
        if !keep_object {
            let _ = std::fs::remove_file(path);
        }
    };

    let runtime_lib = toolchain.resolve_runtime_library().map_err(|err| {
        discard_object(&obj_path);
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0700,
            format!("runtime library unavailable: {err}"),
        )
        .with_help("place the bundled runtime in ../lib relative to willow, build willow_runtime with Cargo, or pass --runtime-lib / WILLOW_RUNTIME_LIB");
        match emitter.emit(&d, map) {
            Ok(()) => anyhow::anyhow!("runtime library unavailable"),
            Err(error) => error.into(),
        }
    })?;

    let link_result = toolchain.link(&obj_path, &runtime_lib, out);
    discard_object(&obj_path);
    let status = link_result?;

    if !status.success() {
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0700,
            "linking failed: the linker exited with a non-zero status",
        )
        .with_help(format!(
            "check that {} exports the required Willow runtime ABI symbols",
            runtime_lib.display()
        ));
        emitter.emit(&d, map)?;
        anyhow::bail!("linking failed");
    }

    toolchain.update_source_map(
        out,
        opts.target
            .emit_source_map
            .then_some(debug_metadata.as_deref().unwrap_or("")),
    )?;

    let mode = if opts.target.build_mode == BuildMode::Release {
        "release"
    } else {
        "debug"
    };
    eprintln!("compiled [{}]: {}", mode, out);
    Ok(())
}

fn emit_codegen_error(
    error: errors::CodegenError,
    map: &diagnostics::SourceMap,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> anyhow::Error {
    if let Err(error) = emitter.emit(&error.diagnostic(), map) {
        return error.into();
    }
    anyhow::Error::new(error)
}

/// Render a codegen failure, preferring the symbol conflicts the backend
/// recorded over the generic internal-error message (willow-uqzx, item 8).
///
/// A symbol conflict is a user error with a source location, so it gets a real
/// diagnostic instead of `internal compiler error`. The backend stops at the
/// first one, because continuing would leave its function table pointing at the
/// wrong function and abort inside Cranelift before this could be printed.
fn report_backend_failure(
    codegen: &mut backend::Codegen,
    fallback: errors::CodegenError,
    map: &diagnostics::SourceMap,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
    artifacts: &UnitArtifacts,
) -> anyhow::Error {
    let conflicts = codegen.take_symbol_conflicts();
    if conflicts.is_empty() {
        return emit_codegen_error(fallback, map, emitter);
    }
    let mut sources = diagnostics::SourceMaps::default();
    for conflict in &conflicts {
        let owner = &conflict.owner;
        if sources.get(owner.span.file_id).is_none() {
            let source = match artifacts.source(owner.span.file_id) {
                Ok(source) => source,
                Err(error) => return error,
            };
            sources.insert(diagnostics::SourceMap::with_file_id(
                owner.span.file_id,
                &owner.source_file,
                source,
            ));
        }
        if let Err(error) = emit_symbol_conflict(conflict, &sources, emitter) {
            return error.into();
        }
    }
    anyhow::anyhow!("aborting due to {} error(s)", conflicts.len())
}

fn emit_symbol_conflict(
    conflict: &backend::SymbolConflict,
    sources: &diagnostics::SourceMaps,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> std::io::Result<()> {
    use diagnostics::{Diagnostic, ErrorCode, Label, Severity};

    let symbol = &conflict.symbol;
    let owner = &conflict.owner;
    let diagnostic = match &conflict.kind {
        backend::SymbolConflictKind::Reserved => Diagnostic::new(
            Severity::Error,
            ErrorCode::E0705,
            format!("{} would define the reserved symbol `{symbol}`", owner.item),
        )
        .with_label(Label::primary(owner.span, "reserved name"))
        .with_help(
            "`willow_*` belongs to the Willow runtime and `__willow_*` to the compiler; \
             defining one replaces the runtime's version for the whole program",
        ),
        backend::SymbolConflictKind::Duplicate { previous } => Diagnostic::new(
            Severity::Error,
            ErrorCode::E0706,
            format!(
                "{} and {} both map to the linker symbol `{symbol}`",
                previous.item, owner.item
            ),
        )
        .with_label(Label::primary(owner.span, "second declaration"))
        .with_help(format!(
            "two declarations cannot share one linker symbol; rename one of them \
             (the first is {} in {})",
            previous.item, previous.source_file
        )),
    };

    emitter.emit(&diagnostic, sources)
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

/// Lower a source file to typed HIR and render it as text (the `--emit-hir`
/// build flag). Runs the normal front-end (lex → parse → import → desugar →
/// type-check) so the HIR reflects the checked, desugared program; lowering
/// covers the constructs implemented so far (willow-mb5) and lists the rest as
/// trailing comments rather than failing.
pub fn emit_hir_text(src: &str) -> Result<String> {
    let _query_stats = query_stats::Session::enter();
    let _node_ids = parser::ast::NodeIdSession::enter();
    let src_path = PathBuf::from(src);
    let source = std::fs::read_to_string(&src_path)
        .with_context(|| format!("cannot read {}", src_path.display()))?;
    let root = src_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let map = diagnostics::SourceMap::new(src, &source);
    let options = CompilerOptions::debug().resolve_environment();
    let frontend = run_frontend(&source, &root, &map, &options)?;

    let body = frontend
        .module_graph
        .artifacts
        .as_ref()
        .expect("spooled frontend")
        .hydrate(&frontend.program, diagnostics::FileId::ENTRY)?;
    let checked = frontend.db.checked_unit(
        module::UnitId::ENTRY,
        frontend.module_graph.artifacts.as_ref().unwrap(),
    )?;
    let tables = checked.tables();
    let (hir, lowering_diagnostics) = ir::lower::lower_program_with(&body, &tables);
    let mut text = ir::dump::format_program(&hir);
    if !lowering_diagnostics.is_empty() {
        text.push_str("\n// constructs not yet lowered to HIR (willow-mb5):\n");
        for diagnostic in &lowering_diagnostics {
            text.push_str(&format!("//   {}\n", diagnostic.message));
        }
    }
    Ok(text)
}

/// Lower a source file to the basic-block LIR and render it as text (the
/// `--emit-lir` build flag). Runs the normal front-end, lowers to typed HIR,
/// then makes control flow explicit as blocks.
pub fn emit_lir_text(src: &str) -> Result<String> {
    let _query_stats = query_stats::Session::enter();
    let _node_ids = parser::ast::NodeIdSession::enter();
    let src_path = PathBuf::from(src);
    let source = std::fs::read_to_string(&src_path)
        .with_context(|| format!("cannot read {}", src_path.display()))?;
    let root = src_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let map = diagnostics::SourceMap::new(src, &source);
    let options = CompilerOptions::debug().resolve_environment();
    let frontend = run_frontend(&source, &root, &map, &options)?;

    let body = frontend
        .module_graph
        .artifacts
        .as_ref()
        .expect("spooled frontend")
        .hydrate(&frontend.program, diagnostics::FileId::ENTRY)?;
    let checked = frontend.db.checked_unit(
        module::UnitId::ENTRY,
        frontend.module_graph.artifacts.as_ref().unwrap(),
    )?;
    let tables = checked.tables();
    // The source dump historically retains import spellings. Its fresh db
    // session is separate from native emission, which uses the declared tree.
    let gaps =
        frontend
            .db
            .lir
            .lower_unit(module::UnitId::ENTRY, &body, frontend.db.bodies(), &tables)?;
    let lir = frontend.db.lir.unit_program(module::UnitId::ENTRY)?;
    let mut text = ir::lowered::format_program(&lir);
    if !gaps.is_empty() {
        text.push_str("\n// constructs not yet lowered to HIR (willow-mb5):\n");
        for diagnostic in gaps.iter() {
            text.push_str(&format!("//   {}\n", diagnostic.message));
        }
    }
    Ok(text)
}

fn diagnostic_error_count(diagnostics: &[diagnostics::Diagnostic]) -> usize {
    diagnostics
        .iter()
        .filter(|diag| diag.severity == diagnostics::Severity::Error)
        .count()
}

#[cfg(test)]
mod emit_hir_tests {
    use super::*;

    // End-to-end: a real source file goes through the full front-end and is
    // rendered as typed HIR, with each expression carrying its resolved type.
    #[test]
    fn emit_hir_renders_typed_program() {
        let path = std::env::temp_dir().join("willow_emit_hir_e2e_test.wi");
        std::fs::write(
            &path,
            "fn add(a: i64, b: i64) -> i64 { return a + b; }\n\
             fn main() { print(add(1, 2)); }\n",
        )
        .expect("write temp source");
        let text = emit_hir_text(path.to_str().unwrap()).expect("emit hir");
        let _ = std::fs::remove_file(&path);

        assert!(text.contains("fn add(a: i64, b: i64) -> i64 {"), "{text}");
        assert!(text.contains("return (a: i64 + b: i64): i64;"), "{text}");
        assert!(
            text.contains("print(add(1: i64, 2: i64): i64): void;"),
            "{text}"
        );
    }
}

#[cfg(test)]
mod frontend_phase_tests {
    use super::*;

    fn parse_source(source: &str) -> parser::ast::Program {
        let tokens = lex_phase(source).expect("test source should lex");
        let parsed = parse_phase(tokens);
        assert_eq!(parsed.outcome.error_count, 0);
        parsed.program
    }

    #[test]
    fn module_dependency_index_work_is_numeric_and_output_sensitive() {
        for size in [8usize, 16, 32, 64] {
            for shape in ["chain", "fanout", "diamond"] {
                let n = if shape == "diamond" {
                    2 * size + 1
                } else {
                    size
                };
                let modules: Vec<_> = (0..n)
                    .map(|id| {
                        let dependencies = if id == 0 {
                            vec![]
                        } else if shape == "chain" {
                            vec![id - 1]
                        } else if shape == "fanout" || id <= 2 {
                            vec![0]
                        } else {
                            let layer = (id - 1) / 2;
                            vec![2 * layer - 1, 2 * layer]
                        };
                        let source = dependencies
                            .iter()
                            .map(|dep| format!("import m{dep};"))
                            .collect::<String>();
                        module::ResolvedModule {
                            package: crate::package::PackageId(0),
                            symbol_module: None,
                            id: module::ModuleId(id as u32),
                            name: format!("m{id}"),
                            canonical_path: format!("m{id}"),
                            path: format!("m{id}.wi").into(),
                            source: String::new(),
                            program: parse_source(&source),
                        }
                    })
                    .collect();
                DEPENDENCY_WORK.with(|work| work.set((0, 0)));
                let index = ModuleDependencies::new(&modules);
                let expected_edges = if shape == "diamond" {
                    4 * size - 2
                } else {
                    n - 1
                };
                assert_eq!(DEPENDENCY_WORK.with(|work| work.get()), (expected_edges, 0));
                for _ in 0..3 {
                    for root in 0..n {
                        let closure = index.closure(std::iter::once(root));
                        assert!(closure[root]);
                        assert!(closure[0]);
                        assert!(
                            closure
                                .iter()
                                .enumerate()
                                .all(|(id, present)| !present || id <= root)
                        );
                    }
                }
                let expected_visits = match shape {
                    "chain" => n * (n - 1) / 2,
                    "fanout" => n - 1,
                    _ => 4 * size * (size - 1) + 2,
                };
                let work = DEPENDENCY_WORK.with(|work| work.get());
                assert_eq!(work, (expected_edges, 3 * expected_visits));
                println!(
                    "shape={shape} modules={n} passes=3 build_path_lookups={} closure_path_lookups=0 edge_visits={}",
                    work.0, work.1
                );
            }
        }
    }

    #[test]
    fn module_dependency_index_resolves_alias_and_item_edges_once() {
        let sources = [
            "pub class Base {}",
            "import m0 as b; import m0::Base as B;",
            "import m1;",
        ];
        let modules: Vec<_> = sources
            .iter()
            .enumerate()
            .map(|(id, source)| module::ResolvedModule {
                package: crate::package::PackageId(0),
                symbol_module: None,
                id: module::ModuleId(id as u32),
                name: format!("m{id}"),
                canonical_path: format!("m{id}"),
                path: format!("m{id}.wi").into(),
                source: String::new(),
                program: parse_source(source),
            })
            .collect();
        let index = ModuleDependencies::new(&modules);
        assert_eq!(index.edges, [vec![], vec![0], vec![1]]);
        assert_eq!(index.closure(std::iter::once(2)), vec![true, true, true]);
        let body = parse_source(
            "import m0 as b; import m0::Base as B; pub fn f(x: b::Base) -> B { return x; }",
        );
        let mut checker = semantic::TypeChecker::new();
        register_module_imports(&mut checker, &body, &modules, &index);
        checker.check_module_program(&body);
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
    }

    #[test]
    fn lex_phase_separates_success_from_diagnostics() {
        assert!(lex_phase("fn main() {}").is_ok());
        assert!(lex_phase("fn main() { @ }").is_err());
    }

    #[test]
    fn parse_phase_retains_partial_ast_and_error_count() {
        let tokens = lex_phase("fn good() {} fn broken( {").unwrap();
        let parsed = parse_phase(tokens);
        assert!(!parsed.program.items.is_empty());
        assert!(parsed.outcome.error_count > 0);
    }

    #[test]
    fn import_phase_clears_bindings_after_resolution_error() {
        let program = parse_source("import definitely_missing; fn main() {}");
        let root = std::env::temp_dir().join(format!(
            "willow_frontend_import_phase_{}",
            std::process::id()
        ));
        let imports = import_phase(&program, &root);
        assert!(imports.outcome.error_count > 0);
        assert!(imports.graph.files.is_empty());
        assert!(imports.item_imports.is_empty());
    }

    #[test]
    fn desugar_phase_reports_its_own_diagnostic_count() {
        let mut program = parse_source("fn main() {}");
        let outcome = desugar_phase(&mut program, &mut []);
        assert_eq!(outcome.error_count, 0);
        assert!(outcome.diagnostics.is_empty());
    }

    #[test]
    fn typecheck_phase_returns_checker_and_error_count() {
        let program = parse_source("fn main() { println(1); }");
        let phase = typecheck_phase(
            &program,
            &[],
            &UnitArtifacts::new().unwrap(),
            &CompilerOptions::debug(),
            None,
        )
        .unwrap();
        assert_eq!(phase.error_count, 0);
        assert!(phase.checker.errors.is_empty());
    }

    #[test]
    fn imported_nonpreemptible_methods_follow_each_units_aliases() {
        let source = "pub class Work { pub fn heavy(self) { while true {} } }";
        let program = parse_source(source);
        let effects = compiler_db::effects::EffectQueries::default();
        let graph = semantic::TypeChecker::resolved_effect_graph(&program);
        effects
            .complete(module::ModuleId(0), || {
                compiler_db::effects::solve_unit(
                    &program,
                    &graph,
                    &std::collections::HashMap::<parser::ast::ExprId, parser::ast::Type>::new(),
                    None,
                    &Default::default(),
                    &Default::default(),
                    |_| semantic::effects::RuntimeEffects::MAY_PANIC,
                )
            })
            .unwrap();
        let modules = [module::ResolvedModule {
            package: crate::package::PackageId(0),
            symbol_module: None,
            id: module::ModuleId(0),
            name: "another_units_alias".into(),
            canonical_path: "worker".into(),
            path: "worker.wi".into(),
            source: source.into(),
            program,
        }];
        for (import, class) in [
            ("import worker;", "worker::Work"),
            ("import worker as jobs;", "jobs::Work"),
            ("import worker::Work as Job;", "Job"),
        ] {
            let body = parse_source(&format!(
                "{import} pub async fn run() {{ let w: {class} = new {class}(); w.heavy(); }}"
            ));
            let mut checker = semantic::TypeChecker::new().with_sync_stack_preemption(false);
            register_prelude(&mut checker).unwrap();
            register_module_imports(
                &mut checker,
                &body,
                &modules,
                &ModuleDependencies::new(&modules),
            );
            checker.set_nonpreemptible_module_methods(imported_nonpreemptible_method_owners(
                &body,
                &modules,
                &effects,
                &ModuleDependencies::new(&modules),
            ));
            checker.check_module_program(&body);
            assert!(
                checker
                    .errors
                    .iter()
                    .any(|diagnostic| diagnostic.code == diagnostics::ErrorCode::E0810),
                "{class}: {:?}",
                checker.errors
            );
        }
    }

    #[test]
    fn concurrency_phase_reports_entry_errors_without_rendering() {
        let program = parse_source("async fn update(x: &mut i64) {} fn main() {}");
        let phase = check_unit_concurrency(
            &program,
            &[],
            &ModuleDependencies::new(&[]),
            &compiler_db::effects::EffectQueries::default(),
            None,
        );
        assert!(diagnostic_error_count(&phase) > 0);
        assert!(!phase.is_empty());
    }
}

fn validate_entry_point(program: &parser::ast::Program) -> Vec<diagnostics::Diagnostic> {
    use diagnostics::{Diagnostic, ErrorCode, Label, Severity};
    use parser::ast::{Item, Type};

    let mains = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Function(f) if f.name == "main" => Some(f),
            _ => None,
        })
        .collect::<Vec<_>>();

    if mains.is_empty() {
        return vec![
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E1303,
                "missing entry point `main`",
            )
            .with_help("define an entry point: `fn main() { ... }`"),
        ];
    }

    let mut errors = Vec::new();
    if let Some(first) = mains.first() {
        for duplicate in mains.iter().skip(1) {
            errors.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1302,
                    "duplicate entry point `main`",
                )
                .with_label(Label::primary(
                    duplicate.span,
                    "duplicate `main` defined here",
                ))
                .with_label(Label::secondary(first.span, "first `main` defined here"))
                .with_help("keep exactly one top-level `fn main`"),
            );
        }
    }

    let std_collections_module_imported = program.imports.iter().any(|import| {
        import.alias.is_none()
            && module::std_registry::is_std_path(&import.path)
            && matches!(
                module::std_registry::resolve_std_import(&import.path, import.span),
                Ok(module::std_registry::StdImport::Module { module }) if module == "collections"
            )
    });

    for main in mains {
        let valid_args = match main.params.as_slice() {
            [] => true,
            [param] => is_main_args_type(&param.ty, std_collections_module_imported),
            _ => false,
        };
        // `main` may return `void` or `Result<void, E>` (willow-exg). A
        // Result-returning main exits 0 on Ok and prints + exits non-zero on Err.
        let valid_return = main.return_type == Type::Void
            || semantic::builtin_types::binary_args(
                &main.return_type,
                semantic::builtin_types::BuiltinTypeId::Result,
            )
            .is_some_and(|(ok, _)| *ok == Type::Void);

        if !valid_args || !valid_return {
            errors.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1301,
                    "invalid entry point signature for `main`",
                )
                .with_label(Label::primary(
                    main.span,
                    "expected `fn main()` or `fn main(args: Array<String>)`",
                ))
                .with_help("use `fn main() { ... }` or `fn main(args: Array<String>) { ... }`"),
            );
        }
    }

    errors
}

fn is_main_args_type(ty: &parser::ast::Type, std_collections_module_imported: bool) -> bool {
    use parser::ast::Type;

    match ty {
        Type::Array(element) => **element == Type::String,
        Type::Generic(name, args) if std_collections_module_imported => {
            name == "collections::Array" && matches!(args.as_slice(), [Type::String])
        }
        _ => false,
    }
}

#[cfg(test)]
mod single_file_import_tests;

#[cfg(test)]
mod package_identity_tests;
