// `Diagnostic` is the compiler's pervasive error type; returning it by value
// keeps fallible parser/semantic signatures readable. Boxing every `Result` to
// shrink the cold `Err` path is churn not worth it here, so allow it crate-wide.
#![allow(clippy::result_large_err)]

pub mod backend;
pub(crate) mod compiler_stack;
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
use module::artifacts::{LiveUnit, UnitArtifacts, UnitKind};
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
/// [`run_backend`]: checked declaration summaries and lazy, immutable body
/// artifacts. Checkers and executable trees are owned only by their unit pass.
type HelperIndex = std::collections::HashMap<
    String,
    std::collections::HashMap<
        semantic::ids::FunctionId,
        semantic::concurrency::NonpreemptibleHelper,
    >,
>;

struct Frontend {
    // These programs contain declarations only. Their body slots are immutable
    // references (source spans / syntax IDs) into module_graph.artifacts.
    program: parser::ast::Program,
    module_graph: module::ModuleGraph,
    helpers: HelperIndex,
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
        let _node_ids = parser::ast::NodeIdSession::enter();
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

#[cfg(test)]
struct ImportPhase {
    graph: module::ModuleGraph,
    item_imports: Vec<module::resolver::ItemImport>,
    outcome: PhaseDiagnostics,
}

struct TypecheckPhase {
    checker: LiveUnit<semantic::TypeChecker>,
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
    let mut artifacts = module::artifacts::UnitArtifacts::new()?;
    artifacts.snapshot_source(diagnostics::FileId::ENTRY, source)?;
    artifacts.offload(&mut program)?;
    let resolution = module::resolver::resolve_imports_spooled(&program, root, artifacts);
    let mut graph = resolution.graph;
    emit_frontend_diagnostics(&resolution.diagnostics, map, &graph)?;
    let imports = PhaseDiagnostics::new(resolution.diagnostics);
    let item_imports = if imports.error_count == 0 {
        resolution.item_imports
    } else {
        graph.files.clear();
        vec![]
    };
    let desugar = desugar_phase(&mut program, &mut graph.files);
    emit_frontend_diagnostics(&desugar.diagnostics, map, &graph)?;
    let artifacts = graph.artifacts.as_ref().expect("spooled import graph");
    let mut helpers = HelperIndex::new();
    for module in &graph.files {
        let body = artifacts.hydrate(&module.program)?;
        helpers.insert(
            module.canonical_path.clone(),
            semantic::concurrency::compute_nonpreemptible_helpers(&body),
        );
    }
    let mut error_count = parse.error_count + imports.error_count + desugar.error_count;
    for module in &graph.files {
        let body = artifacts.hydrate(&module.program)?;
        let checker = check_module(&body, module, &graph.files, &helpers, artifacts, options)?;
        error_count += diagnostic_error_count(&checker.errors);
        emit_frontend_diagnostics(&checker.errors, map, &graph)?;
        let concurrency = check_unit_concurrency(&body, &graph.files, &helpers, None);
        error_count += diagnostic_error_count(&concurrency);
        emit_frontend_diagnostics(&concurrency, map, &graph)?;
    }
    {
        let body = artifacts.hydrate(&program)?;
        let checked = typecheck_phase(
            &body,
            &graph.files,
            &item_imports,
            &helpers,
            artifacts,
            options,
        )?;
        error_count += checked.error_count;
        emit_frontend_diagnostics(&checked.checker.errors, map, &graph)?;
        let concurrency =
            check_unit_concurrency(&body, &graph.files, &helpers, Some(&item_imports));
        error_count += diagnostic_error_count(&concurrency);
        emit_frontend_diagnostics(&concurrency, map, &graph)?;
    }
    let entry = validate_entry_point(&program);
    error_count += diagnostic_error_count(&entry);
    emit_frontend_diagnostics(&entry, map, &graph)?;
    if error_count > 0 {
        anyhow::bail!("aborting due to {} error(s)", error_count);
    }
    Ok(Frontend {
        program,
        module_graph: graph,
        helpers,
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
    helpers_index: &HelperIndex,
    artifacts: &UnitArtifacts,
    options: &CompilerOptions,
) -> Result<TypecheckPhase> {
    let mut checker = semantic::TypeChecker::new();
    if options.enforce_send_sync {
        checker.set_enforce_send_sync(true);
    }
    register_prelude(&mut checker)?;
    for m in modules {
        checker.register_module_with_id(
            m.id,
            &m.name,
            &m.canonical_path,
            &m.path.to_string_lossy(),
            &m.program,
        );
        // The graph name is the FIRST importer's spelling, which is another
        // file's whenever a module got here before the entry did. The entry's
        // own spelling has to answer too, and to the same registrations: a
        // second registration under it would make a second class out of every
        // one the module declares (willow-uvlp).
        for spelling in entry_module_spellings(program, item_imports, m) {
            checker.alias_module_spelling(m.id, &spelling, &m.name, &m.program);
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
        let helpers = helpers_index
            .get(&m.canonical_path)
            .cloned()
            .unwrap_or_default();
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
    let error_count = diagnostic_error_count(&checker.errors);
    Ok(TypecheckPhase {
        checker: artifacts.track(UnitKind::Checker, checker),
        error_count,
    })
}

fn check_module(
    body: &parser::ast::Program,
    module: &module::ResolvedModule,
    modules: &[module::ResolvedModule],
    helpers: &HelperIndex,
    artifacts: &UnitArtifacts,
    options: &CompilerOptions,
) -> Result<LiveUnit<semantic::TypeChecker>> {
    let mut checker = semantic::TypeChecker::new();
    checker.set_enforce_send_sync(options.enforce_send_sync);
    register_prelude(&mut checker)?;
    register_module_imports(&mut checker, body, modules);
    checker.set_module_path(&module.canonical_path);
    checker.set_nonpreemptible_module_methods(imported_nonpreemptible_method_owners(
        body, modules, helpers,
    ));
    checker.check_module_program(body);
    Ok(artifacts.track(UnitKind::Checker, checker))
}

/// Every spelling the ENTRY file writes for `module`.
///
/// A whole-module import contributes its alias, or the last segment of its
/// path; an item import (`import sales::Amount;`) contributes the module's
/// canonical path, which is what the item lookup resolves against. The graph
/// name is not excluded here -- `alias_module_spelling` ignores it.
fn entry_module_spellings(
    program: &parser::ast::Program,
    item_imports: &[module::resolver::ItemImport],
    module: &module::ResolvedModule,
) -> Vec<String> {
    let mut spellings: Vec<String> = Vec::new();
    let push = |spelling: String, out: &mut Vec<String>| {
        if !out.contains(&spelling) {
            out.push(spelling);
        }
    };
    for import in &program.imports {
        if import.path != module.canonical_path {
            continue;
        }
        let access = import.alias.clone().unwrap_or_else(|| {
            import
                .path
                .rsplit("::")
                .next()
                .unwrap_or(import.path.as_str())
                .to_string()
        });
        push(access, &mut spellings);
    }
    if item_imports
        .iter()
        .any(|item| item.canonical_module == module.canonical_path)
    {
        push(module.canonical_path.clone(), &mut spellings);
    }
    spellings
}

/// Bring the modules `program` itself imports into `checker`'s scope.
///
/// The entry file registers every module in the graph, including ones it
/// reaches only transitively. A module gets no such latitude: it sees exactly
/// what its own `import` lines name, under the name it gave them, because that
/// is what the backend will resolve when it compiles this body.
///
/// What an imported module's public signature NAMES is another matter: `mid`
/// may export `Crate extends Parcel` with `Parcel` declared in `base`, and a
/// base class that resolves to nothing takes every inherited member with it
/// (willow-sxcp). So the dependency closure is registered too, but by types
/// only -- see `register_module_type_signatures`. Registration follows graph
/// order, which is dependency order, because a module's exported signature is
/// qualified against the spellings its own dependencies already answer to.
fn register_module_imports(
    checker: &mut semantic::TypeChecker,
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
) {
    // `(canonical path, access spelling)` for each module this program imports.
    // A module can appear twice under two spellings -- `import base as b;` next
    // to `import base::Parcel;` -- and then it is registered under both, since
    // the item lookup resolves against the canonical one.
    let mut imported: Vec<(&str, &str)> = Vec::new();
    let mut item_imports: Vec<(&str, &str, &str, diagnostics::Span)> = Vec::new();
    for import in &program.imports {
        let path = import.path.as_str();
        // Whole module: `import worker;`, `import a::b as c;`.
        if let Some(dep) = modules.iter().find(|d| d.canonical_path == path) {
            let access = import
                .alias
                .as_deref()
                .unwrap_or_else(|| path.rsplit("::").next().unwrap_or(path));
            push_unique(&mut imported, (dep.canonical_path.as_str(), access));
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
        push_unique(&mut imported, (dep.canonical_path.as_str(), module_path));
        let local = import.alias.as_deref().unwrap_or(item);
        item_imports.push((local, module_path, item, import.span));
    }

    let needed = dependency_closure(imported.iter().map(|(path, _)| *path), modules);
    for dep in modules {
        let canonical = dep.canonical_path.as_str();
        if !needed.contains(canonical) {
            continue;
        }
        let dep_path = dep.path.to_string_lossy();
        let mut spellings = imported
            .iter()
            .filter(|(path, _)| *path == canonical)
            .peekable();
        if spellings.peek().is_none() {
            checker.register_module_type_signatures(canonical, &dep_path, &dep.program);
            continue;
        }
        // The first spelling registers the module; the rest are bound to
        // those same registrations, so one class this unit can write two names
        // for stays one type (willow-uvlp).
        let mut spellings = spellings.map(|(_, access)| *access);
        let Some(registered) = spellings.next() else {
            continue;
        };
        checker.register_module_with_id(
            dep.id,
            registered,
            &dep.canonical_path,
            &dep_path,
            &dep.program,
        );
        for access in spellings {
            checker.alias_module_spelling(dep.id, access, registered, &dep.program);
        }
    }

    for (local, module_path, item, span) in item_imports {
        checker.register_item_import(local, module_path, item, span);
    }
}

fn push_unique<'a>(out: &mut Vec<(&'a str, &'a str)>, entry: (&'a str, &'a str)) {
    if !out.contains(&entry) {
        out.push(entry);
    }
}

/// The canonical paths of `roots` plus everything they import, transitively.
fn dependency_closure<'a>(
    roots: impl Iterator<Item = &'a str>,
    modules: &'a [module::ResolvedModule],
) -> std::collections::HashSet<&'a str> {
    let mut closure: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut pending: Vec<&str> = roots.collect();
    while let Some(path) = pending.pop() {
        if !closure.insert(path) {
            continue;
        }
        let Some(dep) = modules.iter().find(|d| d.canonical_path == path) else {
            continue;
        };
        for import in &dep.program.imports {
            let sub = import.path.as_str();
            if let Some(found) = modules.iter().find(|d| d.canonical_path == sub) {
                pending.push(found.canonical_path.as_str());
            } else if let Some((module_path, _)) = sub.rsplit_once("::")
                && let Some(found) = modules.iter().find(|d| d.canonical_path == module_path)
            {
                pending.push(found.canonical_path.as_str());
            }
        }
    }
    closure
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
    helpers: &HelperIndex,
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

        for (key, helper) in helpers
            .get(&dependency.canonical_path)
            .into_iter()
            .flatten()
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
                key.clone().in_namespace(access)
            };
            out.insert(visible_key, (dependency.name.clone(), helper.reason));
        }
    }
    out
}

/// Seed concurrency analysis from immutable body-free helper summaries.
fn check_unit_concurrency(
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    helpers: &HelperIndex,
    entry_items: Option<&[module::resolver::ItemImport]>,
) -> Vec<diagnostics::Diagnostic> {
    let mut analyzer = semantic::ConcurrencyAnalyzer::new();
    if let Some(items) = entry_items {
        for module in modules {
            if let Some(index) = helpers.get(&module.canonical_path) {
                analyzer = analyzer.with_module_helper_index(&module.name, index);
            }
        }
        for item in items {
            if let Some(index) = helpers.get(&item.canonical_module) {
                analyzer = analyzer.with_item_helper_index(
                    &item.local,
                    &item.item,
                    &item.canonical_module,
                    index,
                );
            }
        }
    } else {
        for import in &program.imports {
            if let Some(index) = helpers.get(&import.path) {
                let access = import
                    .alias
                    .as_deref()
                    .unwrap_or_else(|| import.path.rsplit("::").next().unwrap_or(&import.path));
                analyzer = analyzer.with_module_helper_index(access, index);
            }
        }
    }
    analyzer.check_program(program).errors
}

/// Render only the sources a diagnostic actually references. Successful builds
/// never materialize a build-wide collection of source strings/source maps.
fn emit_frontend_diagnostics(
    diagnostics: &[diagnostics::Diagnostic],
    entry: &diagnostics::SourceMap,
    graph: &module::ModuleGraph,
) -> Result<()> {
    for diagnostic in diagnostics {
        let mut maps = diagnostics::SourceMaps::new(entry.clone());
        let ids: std::collections::HashSet<_> = diagnostic
            .labels
            .iter()
            .map(|label| label.span.file_id)
            .collect();
        for module in &graph.files {
            if ids.contains(&module.id.file_id()) {
                let source = if module.source.is_empty() {
                    graph
                        .artifacts
                        .as_ref()
                        .expect("spooled sources")
                        .source(module.id.file_id())?
                } else {
                    module.source.clone()
                };
                maps.insert(diagnostics::SourceMap::with_file_id(
                    module.id.file_id(),
                    module.path.to_string_lossy().into_owned(),
                    source,
                ));
            }
        }
        diagnostics::emit_multi(diagnostic, &maps);
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
) -> backend::cranelift::UnitImports {
    let classified = module::resolver::classify_unit_imports(program, modules);
    let mut visible_modules = std::collections::HashSet::new();
    let mut module_spellings = Vec::new();
    for binding in &classified.modules {
        // Both spellings: this file writes `access`, while the back end's
        // module tables are keyed by the name the graph registered, which is
        // the first importer's alias when that was another file.
        visible_modules.insert(binding.access.clone());
        visible_modules.insert(binding.graph_name.clone());
        // Visibility alone was not enough: the tables are keyed by ONE of the
        // two spellings, so the other has to be bound to it for this unit's
        // phase (willow-kd1v). The types worth binding are the ones the module
        // itself declares, which is why this is built here rather than in the
        // back end — only the driver holds the imported module's program.
        if binding.access == binding.graph_name {
            continue;
        }
        let Some(dependency) = modules
            .iter()
            .find(|m| m.canonical_path == binding.canonical_path)
        else {
            continue;
        };
        module_spellings.push(backend::cranelift::ModuleSpelling {
            access: binding.access.clone(),
            graph_name: binding.graph_name.clone(),
            canonical_path: binding.canonical_path.clone(),
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
                module: item.canonical_module.clone(),
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
) -> Result<()> {
    use diagnostics::{Diagnostic, ErrorCode, Severity};
    use toolchain::{HostToolchain, Toolchain};

    // The entry file's item imports are not carried here: the back end gets
    // them from `backend_unit_imports` below, the same way every module's are.
    let Frontend {
        program,
        mut module_graph,
        helpers,
    } = frontend;
    let module_init_plan = ir::module_init::ModuleInitPlan::from_graph(&module_graph);
    let mut artifacts = module_graph.artifacts.take().expect("spooled frontend");
    let modules = module_graph.files;
    let debug_metadata = if opts.target.emit_debug_info || opts.target.emit_source_map {
        let entry = artifacts.hydrate(&program)?;
        let mut text =
            diagnostics::DebugSourceMap::from_program(&map.path, map.total_lines(), &entry)
                .to_text();
        drop(entry);
        for module in &modules {
            let body = artifacts.hydrate(&module.program)?;
            let source = artifacts.source(module.id.file_id())?;
            let source_map =
                diagnostics::SourceMap::new(module.path.to_string_lossy().into_owned(), source);
            text.push_str("\n---\n");
            text.push_str(
                &diagnostics::DebugSourceMap::from_program(
                    &source_map.path,
                    source_map.total_lines(),
                    &body,
                )
                .to_text(),
            );
        }
        Some(text)
    } else {
        None
    };
    let mut codegen = backend::Codegen::new(opts).map_err(|error| {
        emit_codegen_error(
            errors::CodegenError::new(errors::CodegenStage::Initialize, error),
            map,
        )
    })?;
    codegen.set_module_init_plan(module_init_plan);
    let entry_items = module::resolver::classify_unit_imports(&program, &modules).items;
    // Immutable global type declarations contain no executable syntax.
    {
        let body = artifacts.hydrate(&program)?;
        let checked = typecheck_phase(&body, &modules, &entry_items, &helpers, &artifacts, opts)?;
        for (name, info) in &checked.checker.symbols.enums {
            codegen.register_enum_info(name.to_string(), info.to_semantic());
        }
        for (name, info) in &checked.checker.symbols.interfaces {
            codegen.register_interface_info(name.to_string(), info.to_semantic());
        }
    }
    let unit_enum_aliases = |checker: &semantic::TypeChecker| -> Vec<(
        String,
        semantic::symbols::EnumInfo<semantic::ids::TypeId>,
    )> {
        checker
            .symbols
            .enums
            .iter()
            .filter(|(name, info)| {
                name.to_string() != info.name
                    && !checker.symbols.classes.contains_key(*name)
                    && !checker.symbols.interfaces.contains_key(*name)
            })
            .map(|(name, info)| (name.to_string(), info.to_semantic()))
            .collect()
    };
    // Declare every unit before emitting any body: later overrides must be
    // visible to devirtualization in earlier modules. Each declaration artifact
    // is written and dropped immediately, preserving its lambda symbols/IDs.
    let mut declared_modules = Vec::with_capacity(modules.len());
    for module in &modules {
        let body = artifacts.hydrate(&module.program)?;
        let checker = check_module(&body, module, &modules, &helpers, &artifacts, opts)?;
        for info in checker.symbols.enums.values() {
            codegen.register_enum_info(info.name.clone(), info.to_semantic());
        }
        codegen.register_module_checker_tables(&checker);
        codegen.set_unit_imports(backend_unit_imports(&body, &modules));
        let displaced = codegen.install_enum_aliases(&unit_enum_aliases(&checker));
        let declared = codegen.declare_module(
            &module.name,
            &module.canonical_path,
            &body,
            &module.path.to_string_lossy(),
        );
        codegen.restore_enum_aliases(displaced);
        let unit = declared.map_err(|error| {
            report_backend_failure(
                &mut codegen,
                errors::CodegenError::new(errors::CodegenStage::Module(module.name.clone()), error),
                map,
                &artifacts,
            )
        })?;
        let unit = artifacts.track(UnitKind::Declared, unit);
        declared_modules.push(artifacts.write(&*unit)?);
    }
    let entry_artifact = {
        let body = artifacts.hydrate(&program)?;
        let checked = typecheck_phase(&body, &modules, &entry_items, &helpers, &artifacts, opts)?;
        codegen.set_unit_imports(backend_unit_imports(&body, &modules));
        let displaced = codegen.install_enum_aliases(&unit_enum_aliases(&checked.checker));
        codegen.register_expr_types(
            checked
                .checker
                .expr_types
                .iter()
                .map(|(id, ty)| (*id, ty.into()))
                .collect(),
        );
        let declared = codegen.declare_program(&body, src);
        codegen.register_expr_types(Default::default());
        codegen.restore_enum_aliases(displaced);
        let unit = declared.map_err(|error| {
            report_backend_failure(
                &mut codegen,
                errors::CodegenError::new(errors::CodegenStage::Entry, error),
                map,
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
        let aliases = {
            let body = artifacts.hydrate(&module.program)?;
            let checker = check_module(&body, module, &modules, &helpers, &artifacts, opts)?;
            let aliases = unit_enum_aliases(&checker);
            drop(body);
            // ANF declarations may hoist a lambda before an await and create
            // fresh temporary IDs. Preserve source resolutions/captures, but
            // lower the exact declared tree with its extended payload types.
            let mut tables = ir::lower::CheckerTables::from_checker(&checker);
            tables.expr_types = Some(unit.normalized_expr_types());
            let (hir, gaps) = ir::lower::lower_program_with(unit.normalized_program(), &tables);
            log_hir_gaps(&gaps);
            codegen.register_module_lir(&unit, ir::lowered::lower_program(&hir));
            aliases
        };
        let displaced = codegen.install_enum_aliases(&aliases);
        let compiled = codegen.compile_module_bodies(&unit);
        codegen.release_unit_transients();
        codegen.restore_enum_aliases(displaced);
        compiled.map_err(|error| {
            report_backend_failure(
                &mut codegen,
                errors::CodegenError::new(errors::CodegenStage::Module(module.name.clone()), error),
                map,
                &artifacts,
            )
        })?;
    }
    let entry_unit: backend::cranelift::DeclaredProgram = artifacts.read(entry_artifact)?;
    let entry_unit = artifacts.track(UnitKind::Declared, entry_unit);
    let _entry_lir = artifacts.live(UnitKind::Lir);
    let entry_aliases = {
        let body = artifacts.hydrate(&program)?;
        let checked = typecheck_phase(&body, &modules, &entry_items, &helpers, &artifacts, opts)?;
        let aliases = unit_enum_aliases(&checked.checker);
        drop(body);
        let mut tables = ir::lower::CheckerTables::from_checker(&checked.checker);
        tables.expr_types = Some(entry_unit.normalized_expr_types());
        let (hir, gaps) = ir::lower::lower_program_with(entry_unit.normalized_program(), &tables);
        log_hir_gaps(&gaps);
        codegen.register_lir_functions(ir::lowered::lower_program(&hir));
        aliases
    };
    let displaced = codegen.install_enum_aliases(&entry_aliases);
    let compiled = codegen.compile_program_bodies(&entry_unit);
    codegen.release_unit_transients();
    codegen.restore_enum_aliases(displaced);
    compiled.map_err(|error| {
        report_backend_failure(
            &mut codegen,
            errors::CodegenError::new(errors::CodegenStage::Entry, error),
            map,
            &artifacts,
        )
    })?;
    drop(entry_unit);

    for warning in codegen.take_async_frame_size_warnings() {
        let warning_source = if warning.source_file == src {
            source.clone()
        } else {
            modules
                .iter()
                .find(|module| module.path.to_string_lossy() == warning.source_file)
                .map(|module| artifacts.source(module.id.file_id()))
                .transpose()?
                .unwrap_or_default()
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
    artifacts: &UnitArtifacts,
) -> anyhow::Error {
    let conflicts = codegen.take_symbol_conflicts();
    if conflicts.is_empty() {
        return emit_codegen_error(fallback, map);
    }
    for conflict in &conflicts {
        emit_symbol_conflict(conflict, artifacts);
    }
    anyhow::anyhow!("aborting due to {} error(s)", conflicts.len())
}

fn emit_symbol_conflict(conflict: &backend::SymbolConflict, artifacts: &UnitArtifacts) {
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

    let source = artifacts.source(owner.span.file_id).unwrap_or_default();
    diagnostics::emit(
        &diagnostic,
        &diagnostics::SourceMap::new(&owner.source_file, source),
    );
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
        .hydrate(&frontend.program)?;
    let items =
        module::resolver::classify_unit_imports(&frontend.program, &frontend.module_graph.files)
            .items;
    let checked = typecheck_phase(
        &body,
        &frontend.module_graph.files,
        &items,
        &frontend.helpers,
        frontend.module_graph.artifacts.as_ref().unwrap(),
        &options,
    )?;
    let tables = ir::lower::CheckerTables::from_checker(&checked.checker);
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
        .hydrate(&frontend.program)?;
    let items =
        module::resolver::classify_unit_imports(&frontend.program, &frontend.module_graph.files)
            .items;
    let checked = typecheck_phase(
        &body,
        &frontend.module_graph.files,
        &items,
        &frontend.helpers,
        frontend.module_graph.artifacts.as_ref().unwrap(),
        &options,
    )?;
    let tables = ir::lower::CheckerTables::from_checker(&checked.checker);
    let (hir, lowering_diagnostics) = ir::lower::lower_program_with(&body, &tables);
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
        let phase = typecheck_phase(
            &program,
            &[],
            &[],
            &HelperIndex::new(),
            &UnitArtifacts::new().unwrap(),
            &CompilerOptions::debug(),
        )
        .unwrap();
        assert_eq!(phase.error_count, 0);
        assert!(phase.checker.errors.is_empty());
    }

    #[test]
    fn concurrency_phase_reports_entry_errors_without_rendering() {
        let program = parse_source("async fn update(x: &mut i64) {} fn main() {}");
        let phase = check_unit_concurrency(&program, &[], &HelperIndex::new(), Some(&[]));
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
