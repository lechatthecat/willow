// `Diagnostic` is the compiler's pervasive error type; returning it by value
// keeps fallible parser/semantic signatures readable. Boxing every `Result` to
// shrink the cold `Err` path is churn not worth it here, so allow it crate-wide.
#![allow(clippy::result_large_err)]

pub mod backend;
pub mod desugar;
pub mod diagnostics;
pub mod errors;
pub mod interpolate;
pub mod ir;
pub mod lexer;
pub mod module;
pub mod parser;
pub mod prelude;
pub mod project;
pub mod semantic;
pub mod stdlib_schema;
pub mod toolchain;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const DEFAULT_WORKERS: usize = 5;

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
    pub target: TargetOptions,
    pub worker_count: Option<usize>,
    pub enforce_send_sync: bool,
}

/// Compatibility alias for callers that used the pre-library API name.
pub type CodegenOptions = CompilerOptions;

struct CompilerEnvironment {
    data_race_check: bool,
    workers: Option<usize>,
    runtime_lib: Option<PathBuf>,
    cargo_target_dir: Option<PathBuf>,
}

impl Default for CompilerEnvironment {
    fn default() -> Self {
        Self {
            data_race_check: false,
            workers: Some(DEFAULT_WORKERS),
            runtime_lib: None,
            cargo_target_dir: None,
        }
    }
}

impl CompilerEnvironment {
    fn read() -> Self {
        Self {
            data_race_check: truthy_env(std::env::var("WILLOW_DATA_RACE_CHECK").ok().as_deref()),
            workers: Some(
                parse_worker_count(std::env::var("WILLOW_WORKERS").ok().as_deref())
                    .unwrap_or(DEFAULT_WORKERS),
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
            enforce_send_sync: false,
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
            enforce_send_sync: false,
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
            enforce_send_sync: false,
        }
    }

    fn resolve_environment(self) -> Self {
        self.with_environment(CompilerEnvironment::read())
    }

    fn with_environment(mut self, environment: CompilerEnvironment) -> Self {
        self.worker_count = Some(
            self.worker_count
                .or(environment.workers)
                .unwrap_or(DEFAULT_WORKERS)
                .max(DEFAULT_WORKERS),
        );
        self.enforce_send_sync = self.enforce_send_sync
            || environment.data_race_check
            || self.worker_count.is_some_and(|workers| workers > 1);
        if self.target.runtime_lib.is_none() {
            self.target.runtime_lib = environment.runtime_lib;
        }
        if self.target.cargo_target_dir.is_none() {
            self.target.cargo_target_dir = environment.cargo_target_dir;
        }
        self
    }
}

fn truthy_env(value: Option<&str>) -> bool {
    value.is_some_and(|value| value != "0" && !value.is_empty())
}

fn parse_worker_count(value: Option<&str>) -> Option<usize> {
    value
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|workers| *workers > 0)
        .map(|workers| workers.max(DEFAULT_WORKERS))
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
    fn multi_worker_environment_enables_send_sync_checks() {
        let options = CompilerOptions::debug().with_environment(CompilerEnvironment {
            workers: Some(8),
            ..CompilerEnvironment::default()
        });
        assert_eq!(options.worker_count, Some(8));
        assert!(options.enforce_send_sync);
    }

    #[test]
    fn default_environment_uses_five_workers_and_enables_checks() {
        let options = CompilerOptions::debug().with_environment(CompilerEnvironment::default());
        assert_eq!(options.worker_count, Some(5));
        assert!(options.enforce_send_sync);
    }

    #[test]
    fn low_worker_override_is_clamped_and_keeps_checks_enabled() {
        let options = CompilerOptions::debug().with_environment(CompilerEnvironment {
            data_race_check: true,
            workers: Some(1),
            ..CompilerEnvironment::default()
        });
        assert_eq!(options.worker_count, Some(5));
        assert!(options.enforce_send_sync);
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
            ..CompilerEnvironment::default()
        });
        assert_eq!(options.worker_count, Some(5));
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
        assert_eq!(parse_worker_count(Some("4")), Some(5));
        assert_eq!(parse_worker_count(Some(" 2 ")), Some(5));
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
/// [`run_backend`]: a fully desugared + type-checked program plus its resolved
/// modules and the type checker (whose symbol tables feed codegen).
struct Frontend {
    program: parser::ast::Program,
    module_graph: module::ModuleGraph,
    item_imports: Vec<module::resolver::ItemImport>,
    checker: semantic::TypeChecker,
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
        let src_path = PathBuf::from(self.src);
        let source = std::fs::read_to_string(&src_path)
            .with_context(|| format!("cannot read {}", src_path.display()))?;

        // Import resolution root: the directory containing the source file.
        let _ = self.project_root; // available for future use (e.g. package search paths)
        let root = src_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let map = diagnostics::SourceMap::new(self.src, &source);

        let frontend = run_frontend(&source, &root, &map, &self.opts)?;
        run_backend(frontend, self.src, self.out, source, &self.opts, &map)
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

struct ImportPhase {
    graph: module::ModuleGraph,
    item_imports: Vec<module::resolver::ItemImport>,
    outcome: PhaseDiagnostics,
    resolve_error: Option<errors::ResolveError>,
}

struct TypecheckPhase {
    checker: semantic::TypeChecker,
    module_diagnostics: Vec<ModulePhaseDiagnostics>,
    error_count: usize,
}

struct ModulePhaseDiagnostics {
    diagnostics: Vec<diagnostics::Diagnostic>,
}

struct ConcurrencyPhase {
    entry_diagnostics: Vec<diagnostics::Diagnostic>,
    module_diagnostics: Vec<ModulePhaseDiagnostics>,
    error_count: usize,
}

fn run_frontend(
    source: &str,
    root: &std::path::Path,
    map: &diagnostics::SourceMap,
    options: &CompilerOptions,
) -> Result<Frontend> {
    let tokens = lex_phase(source).map_err(|errs| {
        diagnostics::emit_all(&errs, map);
        anyhow::anyhow!("aborting due to {} lexer error(s)", errs.len())
    })?;

    let ParsePhase {
        mut program,
        outcome: parse,
    } = parse_phase(tokens);
    diagnostics::emit_all(&parse.diagnostics, map);

    let ImportPhase {
        mut graph,
        item_imports,
        outcome: imports,
        resolve_error,
    } = import_phase(&program, root);
    let source_maps = source_maps(map, &graph);
    diagnostics::emit_all_multi(&imports.diagnostics, &source_maps);
    if resolve_error.is_some() {
        // Keep the maps long enough to render import diagnostics, but preserve
        // the previous policy of not feeding partially resolved modules into
        // desugaring/type checking after an import error.
        graph.files.clear();
    }

    let desugar = desugar_phase(&mut program, &mut graph.files);
    diagnostics::emit_all_multi(&desugar.diagnostics, &source_maps);

    let TypecheckPhase {
        checker,
        module_diagnostics: typecheck_module_diagnostics,
        error_count: typecheck_error_count,
    } = typecheck_phase(&program, &graph.files, &item_imports, options)?;
    diagnostics::emit_all_multi(&checker.errors, &source_maps);
    for module_diagnostics in &typecheck_module_diagnostics {
        diagnostics::emit_all_multi(&module_diagnostics.diagnostics, &source_maps);
    }

    let concurrency = concurrency_phase(&program, &graph.files, &item_imports);
    diagnostics::emit_all_multi(&concurrency.entry_diagnostics, &source_maps);
    for module_diagnostics in &concurrency.module_diagnostics {
        diagnostics::emit_all_multi(&module_diagnostics.diagnostics, &source_maps);
    }

    let entry = PhaseDiagnostics::new(validate_entry_point(&program));
    diagnostics::emit_all_multi(&entry.diagnostics, &source_maps);

    let error_count = parse.error_count
        + imports.error_count
        + desugar.error_count
        + typecheck_error_count
        + concurrency.error_count
        + entry.error_count;
    if error_count > 0 {
        anyhow::bail!("aborting due to {} error(s)", error_count);
    }

    Ok(Frontend {
        program,
        module_graph: graph,
        item_imports,
        checker,
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
fn import_phase(program: &parser::ast::Program, root: &std::path::Path) -> ImportPhase {
    let resolution = module::resolve_imports(program, root);
    let resolve_error = errors::ResolveError::from_diagnostics(&resolution.diagnostics);
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
        resolve_error,
    }
}

/// Compose interface inheritance and inject default methods across the entry
/// program and all imported modules.
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
    item_imports: &[module::resolver::ItemImport],
    options: &CompilerOptions,
) -> Result<TypecheckPhase> {
    let mut checker = semantic::TypeChecker::new();
    if options.enforce_send_sync {
        checker.set_enforce_send_sync(true);
    }
    register_prelude(&mut checker)?;
    for m in modules {
        checker.register_module_with_id(m.id, &m.name, &m.path.to_string_lossy(), &m.program);
        if item_imports.iter().any(|item| {
            item.canonical_module == m.canonical_path && item.canonical_module != m.name
        }) {
            checker.register_module_with_id(
                m.id,
                &m.canonical_path,
                &m.path.to_string_lossy(),
                &m.program,
            );
        }
    }
    for item in item_imports {
        checker.register_item_import(&item.local, &item.canonical_module, &item.item, item.span);
    }
    // Seed non-preemptible methods of imported classes so a cross-module
    // typed-receiver call (`w.heavy()` where `w: m::Work`) in a task context is
    // flagged E0810 (willow-0a6k.2). Keyed by the receiver class name the
    // checker resolves: `module::Class::method` for a whole-module import,
    // `Local::method` for a direct class import. The reason travels with the
    // module name so the diagnostic can distinguish a loop from recursion.
    let mut module_method_owners: std::collections::HashMap<
        semantic::ids::FunctionId,
        (String, semantic::concurrency::NonpreemptibleReason),
    > = std::collections::HashMap::new();
    for m in modules {
        let helpers = semantic::concurrency::compute_nonpreemptible_helpers(&m.program);
        let methods: Vec<(
            &semantic::ids::FunctionId,
            semantic::concurrency::NonpreemptibleReason,
        )> = helpers
            .iter()
            .filter(|(id, _)| id.owner().is_some())
            .map(|(id, helper)| (id, helper.reason))
            .collect();
        for (key, reason) in &methods {
            // Whole-module access: `name::Class::method`.
            module_method_owners.insert(
                (*key).clone().in_namespace(m.name.as_str()),
                (m.name.clone(), *reason),
            );
        }
        // Direct class imports re-key `Class::method` under the local name.
        for item in item_imports {
            if item.canonical_module == m.canonical_path {
                for (key, reason) in &methods {
                    if let Some(imported) = key.remap_imported_item(&item.item, &item.local) {
                        module_method_owners.insert(imported, (m.name.clone(), *reason));
                    }
                }
            }
        }
    }
    checker.set_nonpreemptible_module_methods(module_method_owners);
    checker.check_program(program);
    let mut error_count = diagnostic_error_count(&checker.errors);

    let module_diagnostics = typecheck_modules(modules, options)?;
    for module in &module_diagnostics {
        error_count += diagnostic_error_count(&module.diagnostics);
    }
    Ok(TypecheckPhase {
        checker,
        module_diagnostics,
        error_count,
    })
}

/// Type-check every imported module's BODIES, one checker per module.
///
/// The entry checker above registers module SIGNATURES so the entry file can
/// call into them; nothing there ever walks a module's statements. Without
/// this pass the whole type checker is entry-file-only: a module could annotate
/// a `let` with the wrong type, fall off the end of a non-void function, or
/// call a function that does not exist, and still compile and run
/// (willow-3eo1).
///
/// A module is checked in ITS OWN scope, not the entry file's: the prelude,
/// plus the modules it imports under the names IT uses for them. Sharing the
/// entry checker would let a module see names it never imported. This mirrors
/// what `concurrency_phase` already does with a fresh `ConcurrencyAnalyzer` per
/// module, and the spans render because `source_maps` registers every module
/// file by `file_id`.
fn typecheck_modules(
    modules: &[module::ResolvedModule],
    options: &CompilerOptions,
) -> Result<Vec<ModulePhaseDiagnostics>> {
    let mut out = Vec::new();
    for m in modules {
        let mut checker = semantic::TypeChecker::new();
        if options.enforce_send_sync {
            checker.set_enforce_send_sync(true);
        }
        register_prelude(&mut checker)?;
        register_module_imports(&mut checker, &m.program, modules);
        checker.set_nonpreemptible_module_methods(imported_nonpreemptible_method_owners(
            &m.program, modules,
        ));
        checker.check_module_program(&m.program);
        if !checker.errors.is_empty() {
            out.push(ModulePhaseDiagnostics {
                diagnostics: checker.errors,
            });
        }
    }
    Ok(out)
}

/// Bring the modules `program` itself imports into `checker`'s scope.
///
/// The entry file registers every module in the graph, including ones it
/// reaches only transitively. A module gets no such latitude: it sees exactly
/// what its own `import` lines name, under the name it gave them, because that
/// is what the backend will resolve when it compiles this body.
fn register_module_imports(
    checker: &mut semantic::TypeChecker,
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
) {
    for import in &program.imports {
        let path = import.path.as_str();
        // Whole module: `import worker;`, `import a::b as c;`.
        if let Some(dep) = modules.iter().find(|d| d.canonical_path == path) {
            let access = import
                .alias
                .as_deref()
                .unwrap_or_else(|| path.rsplit("::").next().unwrap_or(path));
            let dep_path = dep.path.to_string_lossy();
            checker.register_module_with_id(dep.id, access, &dep_path, &dep.program);
            continue;
        }
        // Single item: `import math::add;`, `import math::add as plus;`. The
        // module itself is registered under its canonical path so the item
        // lookup below can find it, matching how the entry file resolves the
        // same shape.
        let Some((module_path, item)) = path.rsplit_once("::") else {
            continue;
        };
        let Some(dep) = modules.iter().find(|d| d.canonical_path == module_path) else {
            continue;
        };
        let dep_path = dep.path.to_string_lossy();
        checker.register_module_with_id(dep.id, module_path, &dep_path, &dep.program);
        let local = import.alias.as_deref().unwrap_or(item);
        checker.register_item_import(local, module_path, item, import.span);
    }
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
) -> std::collections::HashMap<
    semantic::ids::FunctionId,
    (String, semantic::concurrency::NonpreemptibleReason),
> {
    let mut out = std::collections::HashMap::new();
    for import in &program.imports {
        let (dependency, access, direct_item) = if let Some(dependency) = modules
            .iter()
            .find(|module| module.canonical_path == import.path)
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
            let Some(dependency) = modules
                .iter()
                .find(|module| module.canonical_path == module_path)
            else {
                continue;
            };
            (
                dependency,
                import.alias.as_deref().unwrap_or(item),
                Some(item),
            )
        };

        for (key, helper) in
            semantic::concurrency::compute_nonpreemptible_helpers(&dependency.program)
        {
            if key.owner().is_none() {
                continue;
            }
            let visible_key = if let Some(item) = direct_item {
                let Some(remapped) = key.remap_imported_item(item, access) else {
                    continue;
                };
                remapped
            } else {
                key.in_namespace(access)
            };
            out.insert(visible_key, (dependency.name.clone(), helper.reason));
        }
    }
    out
}

/// Run task-aware concurrency checks for the entry program and imported module
/// bodies, retaining each module's source context for later rendering.
fn concurrency_phase(
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    item_imports: &[module::resolver::ItemImport],
) -> ConcurrencyPhase {
    let mut entry_concurrency = semantic::ConcurrencyAnalyzer::new();
    for m in modules {
        entry_concurrency = entry_concurrency.with_module_helpers(&m.name, &m.program);
    }
    // Single-item imports (`import worker::heavy;`) bind a module item under a
    // bare local name; seed it so `heavy()` from an entry async fn is flagged.
    for item in item_imports {
        if let Some(m) = modules
            .iter()
            .find(|m| m.canonical_path == item.canonical_module)
        {
            entry_concurrency = entry_concurrency.with_item_helper(
                &item.local,
                &item.item,
                &item.canonical_module,
                &m.program,
            );
        }
    }
    let entry = entry_concurrency.check_program(program);
    let mut error_count = diagnostic_error_count(&entry.errors);
    let mut module_diagnostics = Vec::new();
    for m in modules {
        let mut module_analyzer = semantic::ConcurrencyAnalyzer::new();
        for import in &m.program.imports {
            if let Some(dep) = modules.iter().find(|d| d.canonical_path == import.path) {
                let access = import.alias.as_deref().unwrap_or_else(|| {
                    import
                        .path
                        .rsplit("::")
                        .next()
                        .unwrap_or(import.path.as_str())
                });
                module_analyzer = module_analyzer.with_module_helpers(access, &dep.program);
            }
        }
        let module = module_analyzer.check_program(&m.program);
        if !module.errors.is_empty() {
            error_count += diagnostic_error_count(&module.errors);
            module_diagnostics.push(ModulePhaseDiagnostics {
                diagnostics: module.errors,
            });
        }
    }
    ConcurrencyPhase {
        entry_diagnostics: entry.errors,
        module_diagnostics,
        error_count,
    }
}

fn source_maps(
    entry: &diagnostics::SourceMap,
    graph: &module::ModuleGraph,
) -> diagnostics::SourceMaps {
    let mut maps = diagnostics::SourceMaps::new(entry.clone());
    for file in &graph.files {
        maps.insert(diagnostics::SourceMap::with_file_id(
            file.id.file_id(),
            file.path.to_string_lossy().into_owned(),
            file.source.clone(),
        ));
    }
    maps
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
) -> Result<()> {
    use diagnostics::{Diagnostic, ErrorCode, Severity};
    use toolchain::{HostToolchain, Toolchain};

    let Frontend {
        program,
        module_graph,
        item_imports,
        checker,
    } = frontend;
    let modules = module_graph.files;

    // Codegen — wrap internal errors in a structured diagnostic.
    let mut codegen = backend::Codegen::new(opts).map_err(|error| {
        emit_codegen_error(
            errors::CodegenError::new(errors::CodegenStage::Initialize, error),
            map,
        )
    })?;

    // register_builtin_generic_enums is now a no-op: all enums (including
    // prelude ones) come from the checker symbol table below.
    codegen.register_builtin_generic_enums();
    // Register all enum infos (prelude + user-declared) for the backend.
    for (name, info) in &checker.symbols.enums {
        codegen.register_enum_info(name.to_string(), info.clone());
    }
    // Register interface metadata for vtable codegen + interface dispatch.
    for (name, info) in &checker.symbols.interfaces {
        codegen.register_interface_info(name.to_string(), info.clone());
    }
    // Pass type-checker-inferred lambda return types so unannotated lambdas
    // get correct Cranelift signatures (instead of falling back to I64).
    codegen.register_lambda_return_types(checker.lambda_return_types.clone());
    // Full contextual lambda types carry parameter types inferred from expected
    // `fn(...) -> ...` positions.
    codegen.register_lambda_fn_types(checker.lambda_fn_types.clone());
    // Resolved types of async-fn locals, so the backend can frame-back
    // unannotated live-across-await locals (willow-lpn.5c).
    codegen.register_async_local_types(checker.async_local_types.clone());
    // Unqualified enum-variant constructions resolved by the type checker
    // (willow-60o.1), so the backend lowers them as variant allocations.
    codegen.register_enum_variant_resolutions(checker.enum_variant_resolutions.clone());
    codegen.register_pattern_resolutions(checker.pattern_resolutions.clone());
    // The checker's authoritative per-expression types (willow-mb5): the
    // backend's type queries consult these FIRST, so the legacy structural
    // re-derivation only covers compiler-synthesized expressions.
    codegen.register_expr_types(checker.expr_types.clone());
    // Lowered IR of the entry program (willow-0g8j): functions in the
    // supported subset are compiled by walking blocks instead of the AST.
    {
        let tables = ir::lower::CheckerTables::from_checker(&checker);
        let (hir, _hir_gaps) = ir::lower::lower_program_with(&program, &tables);
        codegen.register_lir_functions(ir::lowered::lower_program(&hir));
    }

    // Declaration and body lowering are two separate sweeps over the units
    // (willow-4zt8). EVERY unit -- each module and the entry program -- is
    // declared before any body is lowered, so a module body is lowered against
    // the whole program's class hierarchy. Compiling each module completely in
    // turn let a module devirtualize a call to one of its own `open` methods
    // against the classes declared so far, silently skipping an override the
    // entry file had not contributed yet.
    let mut declared_modules = Vec::with_capacity(modules.len());
    for m in &modules {
        match codegen.declare_module(
            &m.name,
            &m.canonical_path,
            &m.program,
            &m.path.to_string_lossy(),
        ) {
            Ok(unit) => declared_modules.push((m.name.clone(), unit)),
            Err(error) => {
                return Err(report_backend_failure(
                    &mut codegen,
                    errors::CodegenError::new(errors::CodegenStage::Module(m.name.clone()), error),
                    map,
                    src,
                    &source,
                ));
            }
        }
    }
    // Bind the entry file's single-item imports to the module functions they
    // name, after all modules are declared (so the mangled symbols exist).
    for item in &item_imports {
        codegen.register_item_import(&item.local, &item.canonical_module, &item.item);
    }
    let entry_unit = match codegen.declare_program(&program, src) {
        Ok(unit) => unit,
        Err(error) => {
            return Err(report_backend_failure(
                &mut codegen,
                errors::CodegenError::new(errors::CodegenStage::Entry, error),
                map,
                src,
                &source,
            ));
        }
    };
    for (name, unit) in &declared_modules {
        if let Err(error) = codegen.compile_module_bodies(unit) {
            return Err(report_backend_failure(
                &mut codegen,
                errors::CodegenError::new(errors::CodegenStage::Module(name.clone()), error),
                map,
                src,
                &source,
            ));
        }
    }
    if let Err(error) = codegen.compile_program_bodies(&entry_unit) {
        return Err(report_backend_failure(
            &mut codegen,
            errors::CodegenError::new(errors::CodegenStage::Entry, error),
            map,
            src,
            &source,
        ));
    }

    for warning in codegen.take_async_frame_size_warnings() {
        let warning_source = if warning.source_file == src {
            source.clone()
        } else {
            std::fs::read_to_string(&warning.source_file).unwrap_or_default()
        };
        let warning_map = diagnostics::SourceMap::new(&warning.source_file, &warning_source);
        let point_span = diagnostics::Span::new(
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
        diagnostics::emit(&diagnostic, &warning_map);
    }

    let debug_metadata = if opts.target.emit_debug_info || opts.target.emit_source_map {
        Some(debug_source_map_text(map, &program, &modules))
    } else {
        None
    };
    if opts.target.emit_debug_info {
        codegen
            .embed_runtime_metadata(debug_metadata.as_deref().unwrap_or(""))
            .map_err(|error| {
                emit_codegen_error(
                    errors::CodegenError::new(errors::CodegenStage::Metadata, error),
                    map,
                )
            })?;
    }

    let obj_bytes = codegen.finish().map_err(|error| {
        emit_codegen_error(
            errors::CodegenError::new(errors::CodegenStage::Finish, error),
            map,
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
        .with_help("build willow_runtime with Cargo or pass --runtime-lib / WILLOW_RUNTIME_LIB");
        diagnostics::emit(&d, map);
        anyhow::anyhow!("runtime library unavailable")
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
        diagnostics::emit(&d, map);
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

fn emit_codegen_error(error: errors::CodegenError, map: &diagnostics::SourceMap) -> anyhow::Error {
    diagnostics::emit(&error.diagnostic(), map);
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
    entry_path: &str,
    entry_source: &str,
) -> anyhow::Error {
    let conflicts = codegen.take_symbol_conflicts();
    if conflicts.is_empty() {
        return emit_codegen_error(fallback, map);
    }
    for conflict in &conflicts {
        emit_symbol_conflict(conflict, map, entry_path, entry_source);
    }
    anyhow::anyhow!("aborting due to {} error(s)", conflicts.len())
}

fn emit_symbol_conflict(
    conflict: &backend::SymbolConflict,
    map: &diagnostics::SourceMap,
    entry_path: &str,
    entry_source: &str,
) {
    use diagnostics::{Diagnostic, ErrorCode, Label, Severity};

    let source_map_for = |path: &str| -> diagnostics::SourceMap {
        if path == entry_path {
            diagnostics::SourceMap::new(path, entry_source)
        } else {
            let text = std::fs::read_to_string(path).unwrap_or_default();
            diagnostics::SourceMap::new(path, &text)
        }
    };

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

    diagnostics::emit(&diagnostic, &source_map_for(&owner.source_file));
    let _ = map;
}

pub fn compile(
    src: &str,
    out: &str,
    opts: &CompilerOptions,
    project_root: Option<PathBuf>,
) -> Result<()> {
    CompilerSession::new(src, out, opts, project_root).run()
}

/// Lower a source file to typed HIR and render it as text (the `--emit-hir`
/// build flag). Runs the normal front-end (lex → parse → import → desugar →
/// type-check) so the HIR reflects the checked, desugared program; lowering
/// covers the constructs implemented so far (willow-mb5) and lists the rest as
/// trailing comments rather than failing.
pub fn emit_hir_text(src: &str) -> Result<String> {
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

    let tables = ir::lower::CheckerTables::from_checker(&frontend.checker);
    let (hir, lowering_diagnostics) = ir::lower::lower_program_with(&frontend.program, &tables);
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

    let tables = ir::lower::CheckerTables::from_checker(&frontend.checker);
    let (hir, lowering_diagnostics) = ir::lower::lower_program_with(&frontend.program, &tables);
    let lir = ir::lowered::lower_program(&hir);
    let mut text = ir::lowered::format_program(&lir);
    if !lowering_diagnostics.is_empty() {
        text.push_str("\n// constructs not yet lowered to HIR (willow-mb5):\n");
        for diagnostic in &lowering_diagnostics {
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
        let phase = typecheck_phase(&program, &[], &[], &CompilerOptions::debug()).unwrap();
        assert_eq!(phase.error_count, 0);
        assert!(phase.checker.errors.is_empty());
    }

    #[test]
    fn concurrency_phase_reports_entry_errors_without_rendering() {
        let program =
            parse_source("fn heavy() { while true {} } async fn run() { heavy(); } fn main() {}");
        let phase = concurrency_phase(&program, &[], &[]);
        assert!(phase.error_count > 0);
        assert!(!phase.entry_diagnostics.is_empty());
        assert!(phase.module_diagnostics.is_empty());
    }
}

fn debug_source_map_text(
    entry_map: &diagnostics::SourceMap,
    entry_program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
) -> String {
    let mut text = diagnostics::DebugSourceMap::from_program(
        &entry_map.path,
        entry_map.total_lines(),
        entry_program,
    )
    .to_text();

    for module in modules {
        let module_map =
            diagnostics::SourceMap::new(module.path.to_string_lossy().to_string(), &module.source);
        text.push_str("\n---\n");
        text.push_str(
            &diagnostics::DebugSourceMap::from_program(
                &module_map.path,
                module_map.total_lines(),
                &module.program,
            )
            .to_text(),
        );
    }

    text
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
