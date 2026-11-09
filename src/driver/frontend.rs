use super::*;

pub(crate) fn register_prelude(checker: &mut semantic::TypeChecker) -> Result<()> {
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
pub(crate) struct Frontend {
    // These programs contain declarations only. Their body slots are immutable
    // references (source spans / syntax IDs) into module_graph.artifacts.
    pub(crate) program: parser::ast::Program,
    pub(crate) module_graph: module::ModuleGraph,
    pub(crate) db: compiler_db::CompilerDb,
}
/// Front-end phases: lex, parse, resolve imports, desugar interface inheritance
/// and default methods, then run the type checker and concurrency analyses.
/// Diagnostics are emitted as they are found; the phase aborts (returning `Err`)
/// if any stage produced an error, so a successful return yields a program that
/// is safe to hand to the back-end.
pub(crate) struct PhaseDiagnostics {
    pub(crate) diagnostics: Vec<diagnostics::Diagnostic>,
    pub(crate) error_count: usize,
}

impl PhaseDiagnostics {
    pub(super) fn new(diagnostics: Vec<diagnostics::Diagnostic>) -> Self {
        let error_count = diagnostic_error_count(&diagnostics);
        Self {
            diagnostics,
            error_count,
        }
    }
}

pub(super) struct ParsePhase {
    program: parser::ast::Program,
    outcome: PhaseDiagnostics,
}

#[cfg(test)]
pub(crate) struct ImportPhase {
    pub(crate) graph: module::ModuleGraph,
    pub(crate) item_imports: Vec<module::resolver::ItemImport>,
    pub(crate) outcome: PhaseDiagnostics,
}

pub(crate) struct TypecheckPhase {
    pub(crate) checker: LiveUnit<semantic::TypeChecker>,
    #[cfg(test)]
    pub(crate) error_count: usize,
}

/// Roots for project source discovery. A resolved package graph holds the
/// canonical package root and `src/`; the user-spelled project root may differ
/// (macOS `/var` vs `/private/var`), and mixing spellings makes every source
/// look escaped. Without a graph, both fall back to the caller's spellings.
pub(crate) fn discovery_roots(
    inputs: &compiler_db::inputs::CompilerInputs,
    project: &std::path::Path,
    root: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    match inputs.package_graph.as_ref().and_then(|g| g.get(g.root)) {
        Some(package) => (package.root.clone(), package.source_root()),
        None => (project.to_path_buf(), root.to_path_buf()),
    }
}

pub(crate) fn run_frontend(
    source: &str,
    root: &std::path::Path,
    map: &diagnostics::SourceMap,
    options: &CompilerOptions,
) -> Result<Frontend> {
    run_frontend_with_emitter(source, root, map, options, &mut diagnostics::HumanEmitter)
}

pub(super) fn run_frontend_with_emitter(
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

pub(crate) fn run_frontend_with_inputs(
    source: &str,
    root: &std::path::Path,
    map: &diagnostics::SourceMap,
    inputs: compiler_db::inputs::CompilerInputs,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> Result<Frontend> {
    compiler_db::persistent::run(source, root, map, inputs, emitter)
}

pub(crate) fn run_frontend_revision(
    source: &str,
    root: &std::path::Path,
    map: &diagnostics::SourceMap,
    inputs: compiler_db::inputs::CompilerInputs,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
    previous: Option<&Frontend>,
    syntax_queries: Option<
        std::rc::Rc<std::cell::RefCell<compiler_db::incremental::SyntaxQueries>>,
    >,
) -> Result<Frontend> {
    run_frontend_revision_cached(
        source,
        root,
        map,
        inputs,
        emitter,
        (previous, None),
        syntax_queries,
    )
}

pub(crate) fn run_frontend_revision_cached(
    source: &str,
    root: &std::path::Path,
    map: &diagnostics::SourceMap,
    inputs: compiler_db::inputs::CompilerInputs,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
    (previous, disk): (
        Option<&Frontend>,
        Option<&compiler_db::persistent::DiskRevision>,
    ),
    syntax_queries: Option<
        std::rc::Rc<std::cell::RefCell<compiler_db::incremental::SyntaxQueries>>,
    >,
) -> Result<Frontend> {
    let reuse = RevisionReuse { previous, disk };
    let mut artifacts = revision_artifacts(syntax_queries, reuse)?;
    let ParsePhase {
        mut program,
        outcome: parse,
    } = parse_entry(source, map, &mut artifacts, emitter)?;
    artifacts.snapshot_source(diagnostics::FileId::ENTRY, source)?;
    artifacts.offload(&mut program)?;
    let resolution = module::resolver::resolve_imports_spooled_entry(
        &program,
        root,
        artifacts,
        std::fs::canonicalize(&map.path).ok(),
        inputs.package_graph.clone(),
        inputs.project_mode,
        &inputs.check_modules,
    );
    let mut graph = resolution.graph;
    let diagnostic_modules = DiagnosticModuleIndex::new(&graph);
    emit_frontend_diagnostics(
        &resolution.diagnostics,
        map,
        &graph,
        &diagnostic_modules,
        emitter,
    )?;
    let imports = PhaseDiagnostics::new(resolution.diagnostics);
    // Partial syntax is useful to editors and import discovery, but omitted
    // declarations/statements are not a sound input to semantic diagnostics.
    // Finish collecting syntax errors across imports before stopping here.
    if parse.error_count > 0 || graph.has_syntax_errors {
        anyhow::bail!(
            "aborting due to {} error(s)",
            parse.error_count + imports.error_count
        );
    }
    // Keep resolved and recovered declarations even when another import fails.
    // Clearing the graph makes healthy imports disappear from the type checker
    // and manufactures cross-file cascades. All import errors remain fatal at
    // the frontend boundary below, before code generation.
    let desugar_dependencies =
        ModuleDependencies::with_packages(&graph.files, inputs.package_graph.as_deref());
    // Standalone modules also have canonical namespaces. Resolve their source
    // imports before indexing inherited interfaces and default methods.
    let desugar = PhaseDiagnostics::new(
        desugar::DesugarPass::run_resolved(
            &mut program,
            &mut graph.files,
            Some(&desugar_dependencies),
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
    let expanded = expand_templates(
        program,
        &mut graph,
        &inputs,
        reuse,
        map,
        &diagnostic_modules,
        emitter,
    )?;
    let (frontend, artifacts) = prepare_database(
        expanded,
        graph,
        inputs,
        desugar_dependencies,
        reuse,
        root,
        map,
    )?;
    check_frontend(
        frontend,
        artifacts,
        parse.error_count + imports.error_count + desugar.error_count,
        map,
        &diagnostic_modules,
        emitter,
    )
}

#[derive(Clone, Copy)]
struct RevisionReuse<'a> {
    previous: Option<&'a Frontend>,
    disk: Option<&'a compiler_db::persistent::DiskRevision>,
}

struct ExpandedTemplates {
    program: parser::ast::Program,
    generic_sources: std::collections::HashMap<module::UnitId, ai::generics::Source>,
    runtime_names: std::collections::HashMap<String, String>,
    prechecks: std::rc::Rc<semantic::type_checker::precheck::Cache>,
    source_imports_checked: bool,
}
fn revision_artifacts(
    syntax_queries: Option<
        std::rc::Rc<std::cell::RefCell<compiler_db::incremental::SyntaxQueries>>,
    >,
    reuse: RevisionReuse<'_>,
) -> Result<UnitArtifacts> {
    let RevisionReuse { previous, disk } = reuse;
    let mut artifacts = module::artifacts::UnitArtifacts::new()?;
    artifacts.revision_enabled = syntax_queries.is_some();
    artifacts.syntax_queries = syntax_queries;
    if let Some(previous) = previous {
        let old = previous
            .module_graph
            .artifacts
            .as_ref()
            .expect("revision artifacts");
        artifacts.previous_modules = previous
            .module_graph
            .files
            .iter()
            .map(|m| (m.id, m.path.clone()))
            .collect();
        artifacts.previous_parsed = Some((std::rc::Rc::clone(&old.store), old.parsed.clone()));
        artifacts.previous_expanded = Some((std::rc::Rc::clone(&old.store), old.expanded.clone()));
        artifacts
            .body_index_mut()
            .begin_revision(std::rc::Rc::clone(&old.bodies));
    }
    if let Some(disk) = disk {
        artifacts.previous_modules = disk
            .modules
            .iter()
            .map(|(id, path, _)| (*id, path.clone()))
            .collect();
        artifacts.previous_parsed = Some((std::rc::Rc::clone(&disk.store), disk.parsed.clone()));
        artifacts.previous_expanded =
            Some((std::rc::Rc::clone(&disk.store), disk.expanded.clone()));
        artifacts
            .body_index_mut()
            .begin_revision(std::rc::Rc::clone(&disk.index));
    }
    Ok(artifacts)
}
fn parse_entry(
    source: &str,
    map: &diagnostics::SourceMap,
    artifacts: &mut UnitArtifacts,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> Result<ParsePhase> {
    let ParsePhase {
        mut program,
        outcome: parse,
    } = if let Some(queries) = &artifacts.syntax_queries {
        let (program, diagnostics, lexer_failed) = queries.borrow_mut().parse(
            std::path::Path::new(&map.path),
            diagnostics::FileId::ENTRY,
            source,
        )?;
        if lexer_failed {
            for diagnostic in &diagnostics {
                emitter.emit(diagnostic, map)?;
            }
            anyhow::bail!("aborting due to {} lexer error(s)", diagnostics.len());
        }
        ParsePhase {
            program,
            outcome: PhaseDiagnostics::new(diagnostics),
        }
    } else {
        match artifacts.cached_parse(diagnostics::FileId::ENTRY, source)? {
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
        }
    };
    for diagnostic in &parse.diagnostics {
        emitter.emit(diagnostic, map)?;
    }
    if artifacts.revision_enabled && parse.diagnostics.is_empty() {
        artifacts.retain_parse(
            diagnostics::FileId::ENTRY,
            std::path::Path::new(&map.path),
            source,
            &mut program,
        )?;
    }
    Ok(ParsePhase {
        program,
        outcome: parse,
    })
}
fn expand_templates(
    mut program: parser::ast::Program,
    graph: &mut module::ModuleGraph,
    inputs: &compiler_db::inputs::CompilerInputs,
    reuse: RevisionReuse<'_>,
    map: &diagnostics::SourceMap,
    diagnostic_modules: &DiagnosticModuleIndex,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> Result<ExpandedTemplates> {
    let RevisionReuse { previous, disk } = reuse;
    let mut generic_sources = std::collections::HashMap::new();
    let mut runtime_names = std::collections::HashMap::new();
    let has_generics = |p: &parser::ast::Program| {
        p.items.iter().any(|i| match i {
            parser::ast::Item::Function(f) => !f.type_params.is_empty(),
            parser::ast::Item::Class(c) => !c.type_params.is_empty(),
            _ => false,
        })
    };
    // A copy of another unit's generic interface default is checked there,
    // per interface instantiation, by this phase.
    let has_foreign_generic_default = |p: &parser::ast::Program| {
        p.items.iter().any(|i| match i {
            parser::ast::Item::Class(c) => c.methods.iter().any(|m| {
                m.is_interface_default
                    && !m.is_default_injected
                    && m.body.span.file_id != c.span.file_id
            }),
            _ => false,
        })
    };
    // Every copy of a nongeneric default shares its provider's body, so a
    // unit holding one is expanded with the provider, never released early.
    let has_foreign_default = |p: &parser::ast::Program| {
        p.items.iter().any(|i| match i {
            parser::ast::Item::Class(c) => c
                .methods
                .iter()
                .any(|m| m.is_interface_default && m.body.span.file_id != c.span.file_id),
            _ => false,
        })
    };
    let has_templates = |p: &parser::ast::Program| {
        has_generics(p)
            || p.items.iter().any(|i| match i {
                parser::ast::Item::Interface(i) => {
                    !i.type_params.is_empty() && i.methods.iter().any(|m| m.default_body.is_some())
                }
                _ => false,
            })
    };
    let source_imports_checked = std::iter::once(&program)
        .chain(graph.files.iter().map(|m| &m.program))
        .any(|p| has_generics(p) || has_foreign_generic_default(p));
    let prechecks = std::rc::Rc::new(semantic::type_checker::precheck::Cache::revision(
        previous
            .map(|old| old.db.prechecks.as_ref())
            .or_else(|| disk.map(|old| &old.prechecks)),
        &graph
            .artifacts
            .as_ref()
            .expect("source artifacts")
            .correspondence,
    ));
    if source_imports_checked {
        let mut storage = graph.artifacts.take().expect("spooled imports");
        let registry = std::rc::Rc::new(std::cell::RefCell::new(
            semantic::type_checker::generics::TemplateRegistry::default(),
        ));
        registry.borrow_mut().capture_source_references =
            inputs.capture_analysis || inputs.cache_analysis;
        for module in &graph.files {
            std::rc::Rc::make_mut(&mut registry.borrow_mut().module_paths)
                .insert(module.id.file_id(), module.identity_path().to_string());
        }
        // Retain only generic templates. Ordinary executable trees stay in
        // their body artifacts and are hydrated one source unit at a time.
        for (shell, file) in graph
            .files
            .iter()
            .map(|m| (&m.program, m.id.file_id()))
            .chain(std::iter::once((&program, diagnostics::FileId::ENTRY)))
        {
            if !has_templates(shell) {
                continue;
            }
            let source = storage.hydrate(shell, file)?;
            for item in &source.items {
                match item {
                    parser::ast::Item::Interface(i)
                        if !i.type_params.is_empty()
                            && i.methods.iter().any(|m| m.default_body.is_some()) =>
                    {
                        registry.borrow_mut().intern_interface(i);
                    }
                    parser::ast::Item::Function(f) if !f.type_params.is_empty() => {
                        registry.borrow_mut().intern_function(f);
                    }
                    parser::ast::Item::Class(c) if !c.type_params.is_empty() => {
                        registry.borrow_mut().intern_class(c);
                    }
                    _ => {}
                }
            }
        }
        let dependencies =
            ModuleDependencies::with_packages(&graph.files, inputs.package_graph.as_deref());
        // Template checking can report errors before the final checking phase.
        // Give every source checker the same build-wide write summary so a
        // structural marker failure cannot hide a cross-module reassignment.
        // Body artifacts already hold the write facts; no hydration is needed.
        let field_writes = std::rc::Rc::new(
            storage.field_writes(
                graph
                    .files
                    .iter()
                    .map(|module| (module.identity_path(), &module.program))
                    .chain(std::iter::once(("", &program))),
                &dependencies,
            ),
        );
        // Only units declaring templates receive instance checks after their
        // own pass. Every other checker is final once its unit is checked: it
        // expands and is released at once, and template owners are checked
        // last, so live checkers are bounded by the owners, not the graph.
        // A unit receiving foreign default copies waits too: the copies share
        // bodies the provider checks only after every unit's own pass.
        let mut fresh_ids = semantic::type_checker::generics::FreshIds::default();
        let mut pending = std::collections::VecDeque::new();
        let mut checkers = Vec::new();
        let mut owners = std::collections::HashMap::new();
        // Per unit in source order (modules, then the entry): the index of a
        // retained checker, or the diagnostics of a released one.
        let mut reports: Vec<std::result::Result<usize, Vec<diagnostics::Diagnostic>>> = Vec::new();
        reports.resize_with(graph.files.len() + 1, || Ok(usize::MAX));
        let mut released_programs = Vec::new();
        let mut released_entry = None;
        for owners_pass in [false, true] {
            for (index, module) in graph.files.iter().enumerate() {
                if has_templates(&module.program) != owners_pass {
                    continue;
                }
                let mut checker = semantic::TypeChecker::new();
                checker.template_precheck = true;
                checker.prechecks = Some(std::rc::Rc::clone(&prechecks));
                checker.generics.registry = std::rc::Rc::clone(&registry);
                checker.capture_generic_call_targets();
                checker.set_field_writes(std::rc::Rc::clone(&field_writes));
                checker.set_enforce_send_sync(inputs.options.enforce_send_sync);
                register_prelude(&mut checker)?;
                register_module_imports_with_closure(
                    &mut checker,
                    &module.program,
                    &graph.files,
                    &dependencies,
                    None,
                );
                checker.set_module_path(module.identity_path());
                checker.set_class_module_path(module.registration_name());
                let source = storage.hydrate(&module.program, module.id.file_id())?;
                checker.check_module_program(&source);
                let mut checker = storage.track(UnitKind::Checker, checker);
                pending.append(&mut checker.generics.pending);
                if owners_pass
                    || diagnostic_error_count(&checker.errors) > 0
                    || !checker.generics.default_copies.is_empty()
                    || has_foreign_default(&module.program)
                {
                    owners.insert(module.id.file_id(), checkers.len());
                    reports[index] = Ok(checkers.len());
                    checkers.push(checker);
                    continue;
                }
                let expanded = expand_prechecked_unit(
                    &mut checker,
                    source,
                    module.id,
                    &mut storage,
                    &mut fresh_ids,
                    inputs.capture_analysis.then_some(&mut generic_sources),
                )?;
                reports[index] = Err(std::mem::take(&mut checker.errors));
                released_programs.push((index, expanded));
            }
            if has_templates(&program) != owners_pass {
                continue;
            }
            let body = storage.hydrate(&program, diagnostics::FileId::ENTRY)?;
            let mut checker = typecheck_phase_with_generics(
                &body,
                &graph.files,
                &storage,
                &inputs.options,
                None,
                Some(TemplatePrecheck {
                    prechecks: std::rc::Rc::clone(&prechecks),
                    dependencies: &dependencies,
                    registry: std::rc::Rc::clone(&registry),
                    field_writes: std::rc::Rc::clone(&field_writes),
                }),
            )?
            .checker;
            pending.append(&mut checker.generics.pending);
            let entry = graph.files.len();
            if owners_pass
                || diagnostic_error_count(&checker.errors) > 0
                || !checker.generics.default_copies.is_empty()
                || has_foreign_default(&program)
            {
                owners.insert(diagnostics::FileId::ENTRY, checkers.len());
                reports[entry] = Ok(checkers.len());
                checkers.push(checker);
                continue;
            }
            let expanded = expand_prechecked_unit(
                &mut checker,
                body,
                module::UnitId::ENTRY,
                &mut storage,
                &mut fresh_ids,
                inputs.capture_analysis.then_some(&mut generic_sources),
            )?;
            reports[entry] = Err(std::mem::take(&mut checker.errors));
            released_entry = Some(expanded);
        }
        let mut seen = std::collections::HashSet::new();
        let mut instance_count = 0;
        while let Some(item) = pending.pop_front() {
            let (file, name, kind) = match &item.item {
                parser::ast::Item::Function(f) => (f.span.file_id, f.name.clone(), 0u8),
                parser::ast::Item::Class(c) => (
                    c.span.file_id,
                    if item.method_only {
                        QualifiedName::join(&c.name, &c.methods[0].name)
                    } else {
                        c.name.clone()
                    },
                    if item.method_only { 3u8 } else { 1u8 },
                ),
                parser::ast::Item::Interface(i) => (i.span.file_id, i.name.clone(), 2u8),
                parser::ast::Item::Enum(_) => unreachable!(),
            };
            if !seen.insert((file, name, kind)) {
                continue;
            }
            instance_count += usize::from(kind != 3);
            anyhow::ensure!(
                instance_count <= 4096,
                "generic instantiation limit exceeded (possible polymorphic recursion)"
            );
            let checker = &mut checkers[owners[&file]];
            checker.check_generic_instance(item);
            pending.append(&mut checker.generics.pending);
        }
        graph.artifacts = Some(storage);
        let mut errors = 0;
        let mut seen_diagnostics = diagnostics::diagnostic::SourceDiagnosticSet::default();
        for report in &reports {
            let unit = match report {
                Ok(index) => &checkers[*index].errors,
                Err(released) => released,
            };
            let diagnostics: Vec<_> = unit
                .iter()
                .filter_map(|diagnostic| {
                    seen_diagnostics.insert(diagnostic, &diagnostic_modules.package_names)
                })
                .collect();
            emit_frontend_diagnostics(&diagnostics, map, graph, diagnostic_modules, emitter)?;
            errors += diagnostic_error_count(&diagnostics);
        }
        if errors > 0 && !inputs.verification {
            return Err(frontend_abort(graph, errors));
        }
        let mut storage = graph.artifacts.take().expect("generic source artifacts");
        for (index, released) in released_programs {
            graph.files[index].program = released;
        }
        for (index, module) in graph.files.iter_mut().enumerate() {
            let Ok(owner) = &reports[index] else {
                continue;
            };
            let owner = *owner;
            let source = storage.hydrate(&module.program, module.id.file_id())?;
            module.program = expand_prechecked_unit(
                &mut checkers[owner],
                source,
                module.id,
                &mut storage,
                &mut fresh_ids,
                inputs.capture_analysis.then_some(&mut generic_sources),
            )?;
        }
        if let Some(released) = released_entry {
            program = released;
        } else {
            let Ok(entry) = &reports[graph.files.len()] else {
                unreachable!("a released entry keeps its expanded program")
            };
            let entry = *entry;
            let body = storage.hydrate(&program, diagnostics::FileId::ENTRY)?;
            program = expand_prechecked_unit(
                &mut checkers[entry],
                body,
                module::UnitId::ENTRY,
                &mut storage,
                &mut fresh_ids,
                inputs.capture_analysis.then_some(&mut generic_sources),
            )?;
        }
        storage.inherit_generated_owners(&fresh_ids, &registry.borrow().origins);
        runtime_names = std::mem::take(&mut registry.borrow_mut().runtime_names);
        graph.artifacts = Some(storage);
    }
    Ok(ExpandedTemplates {
        program,
        generic_sources,
        runtime_names,
        prechecks,
        source_imports_checked,
    })
}
fn prepare_database(
    expanded: ExpandedTemplates,
    mut graph: module::ModuleGraph,
    inputs: compiler_db::inputs::CompilerInputs,
    desugar_dependencies: ModuleDependencies,
    reuse: RevisionReuse<'_>,
    root: &Path,
    map: &diagnostics::SourceMap,
) -> Result<(Frontend, UnitArtifacts)> {
    let RevisionReuse { previous, disk } = reuse;
    let ExpandedTemplates {
        mut program,
        generic_sources,
        runtime_names,
        prechecks,
        source_imports_checked,
    } = expanded;
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
        let path = QualifiedName::from_segments(components);
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
    artifacts.previous_expanded = None;
    let syntax_queries = artifacts.syntax_queries.take();
    let mut db = compiler_db::CompilerDb::with_dependencies(
        inputs,
        &graph.files,
        std::rc::Rc::clone(&artifacts.bodies),
        std::rc::Rc::clone(&artifacts.store),
        desugar_dependencies,
    );
    db.generic_sources = generic_sources;
    db.runtime_names = runtime_names;
    db.prechecks = prechecks;
    db.source_imports_checked = source_imports_checked;
    db.field_writes = std::rc::Rc::new(
        artifacts.field_writes(
            graph
                .files
                .iter()
                .map(|module| (module.identity_path(), &module.program))
                .chain(std::iter::once(("", &program))),
            db.dependencies(),
        ),
    );
    let mut bridge_identity = None;
    for (namespace, program) in std::iter::once((String::new(), &program)).chain(
        graph
            .files
            .iter()
            .map(|module| (module.identity_path().to_string(), &module.program)),
    ) {
        for item in &program.items {
            if let parser::ast::Item::Function(f) = item
                && let Some(symbol) = &f.rust_bridge
            {
                let identity = bridge_identity.get_or_insert_with(|| {
                    let bridge_root = project::find_project_manifest(root)
                        .map(|(_, root)| root)
                        .unwrap_or_else(|| root.to_path_buf());
                    std::fs::canonicalize(&bridge_root)
                        .unwrap_or(bridge_root)
                        .to_string_lossy()
                        .into_owned()
                });
                let mut symbol = symbol.clone();
                if !namespace.is_empty() {
                    symbol.willow_function =
                        QualifiedName::join(&namespace, &symbol.willow_function);
                }
                db.rust_bridge_symbols
                    .push(((f.span.file_id.0, f.name.clone()), symbol.bind(identity)));
            }
        }
    }
    if let Some(queries) = &syntax_queries {
        db.references.set_tracking(queries);
        let mut paths: std::collections::HashMap<_, _> = graph
            .files
            .iter()
            .map(|module| (module.id, module.path.clone()))
            .collect();
        paths.insert(module::UnitId::ENTRY, std::path::PathBuf::from(&map.path));
        db.declarations
            .set_tracking(std::rc::Rc::clone(queries), paths);
        db.lir.set_tracking(
            std::rc::Rc::clone(queries),
            std::rc::Rc::clone(&db.typed_bodies),
        );
        db.layouts.set_tracking(std::rc::Rc::clone(queries));
        db.effects.set_tracking(
            std::rc::Rc::clone(queries),
            std::rc::Rc::clone(&artifacts.bodies),
            std::rc::Rc::clone(&db.typed_bodies),
        );
        let owners = compiler_db::revision::tracked_body_owners(
            std::path::Path::new(&map.path),
            &graph.files,
            &artifacts,
        );
        db.typed_bodies
            .configure_tracking(std::rc::Rc::clone(queries), owners);
    }
    if let Some(previous) = previous {
        db.effects.reuse_from(&previous.db.effects);
        db.effects
            .retain_units(&compiler_db::revision::compatible_units(
                previous
                    .module_graph
                    .files
                    .iter()
                    .map(|m| (m.id, &m.path, m.symbol_module)),
                &graph.files,
            ));
        db.typed_bodies
            .reuse_checks(&previous.db.typed_bodies, &artifacts.correspondence);
        let candidates =
            compiler_db::revision::candidate_bodies(previous, &graph.files, &artifacts, &db);
        db.typed_bodies.reuse_body_candidates(
            &previous.db.typed_bodies,
            &candidates,
            &artifacts.correspondence,
        )?;
    }
    if let Some(disk) = disk {
        db.typed_bodies
            .reuse_checks(&disk.bodies, &artifacts.correspondence);
        let compatible = compiler_db::revision::compatible_units(
            disk.modules
                .iter()
                .map(|(id, path, symbol)| (*id, path, *symbol)),
            &graph.files,
        );
        db.effects.reuse_solutions(&disk.effects);
        db.effects.retain_units(&compatible);
        let mut candidates =
            compiler_db::revision::candidate_body_ids(&disk.parsed, &graph.files, &artifacts, &db);
        candidates.retain(|id| {
            artifacts
                .bodies
                .owner(*id)
                .is_some_and(|(unit, _)| compatible.contains(&unit))
        });
        if let Some(queries) = &syntax_queries {
            let definitions = compiler_db::stable_path::inventory(
                std::path::Path::new(&map.path),
                &graph.files,
                &artifacts,
                &queries.borrow(),
            )?;
            candidates.retain(|id| {
                definitions
                    .get(id)
                    .is_some_and(|key| disk.definitions.get(key) == Some(id))
            });
        }
        db.typed_bodies.reuse_body_candidates(
            &disk.bodies,
            &candidates,
            &artifacts.correspondence,
        )?;
    }
    db.effects.relocate_solutions(&artifacts.correspondence);
    if db.inputs().capture_analysis || db.inputs().cache_analysis {
        *db.effects.analysis.borrow_mut() = Some(Default::default());
    }
    Ok((
        Frontend {
            program,
            module_graph: graph,
            db,
        },
        artifacts,
    ))
}
fn check_frontend(
    frontend: Frontend,
    mut artifacts: UnitArtifacts,
    mut error_count: usize,
    map: &diagnostics::SourceMap,
    diagnostic_modules: &DiagnosticModuleIndex,
    emitter: &mut dyn diagnostics::DiagnosticEmitter,
) -> Result<Frontend> {
    let Frontend {
        program,
        module_graph: mut graph,
        db,
    } = frontend;
    let options = &db.inputs().options;
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
                Some(&db.typed_bodies),
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
            Some(&db.typed_bodies),
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
        emit_frontend_diagnostics(&diagnostics, map, &graph, diagnostic_modules, emitter)?;
    }
    if error_count == 0 {
        let order: Vec<_> = graph
            .files
            .iter()
            .map(|m| m.id)
            .chain(std::iter::once(module::UnitId::ENTRY))
            .collect();
        let mut diagnostics = db.effects.static_initializers.check_preemption(
            &order,
            &db.effects,
            db.inputs().target.sync_stack_preemption,
        );
        diagnostics.extend(db.effects.static_initializers.solve(&order, &db.effects));
        let lock_facts = db.effects.lock_orders.borrow();
        diagnostics.extend(semantic::type_checker::lock_order::analyze_all(
            order
                .iter()
                .filter_map(|unit| {
                    lock_facts
                        .get(unit)
                        .map(|(facts, _)| (*unit, facts.clone()))
                })
                .collect(),
            |unit, target| {
                lock_facts
                    .get(&unit)
                    .and_then(|(_, imports)| db.effects.external_target(unit, &target, imports))
            },
        ));
        error_count += diagnostic_error_count(&diagnostics);
        emit_frontend_diagnostics(&diagnostics, map, &graph, diagnostic_modules, emitter)?;
    }
    let entry = validate_entry_point(&program);
    error_count += diagnostic_error_count(&entry);
    emit_frontend_diagnostics(&entry, map, &graph, diagnostic_modules, emitter)?;
    if error_count > 0 && !db.inputs().verification {
        return Err(frontend_abort(&mut graph, error_count));
    }
    db.typed_bodies.finish_checks();
    db.prechecks.finish();
    Ok(Frontend {
        program,
        module_graph: graph,
        db,
    })
}

/// Lexing is the only hard-stop front-end phase: parsing cannot proceed
/// without a token stream.
pub(super) fn lex_phase(
    source: &str,
) -> std::result::Result<Vec<lexer::token::Token>, errors::LexError> {
    lexer::Lexer::new(source).tokenize()
}

/// Parse into a partial AST and retain all parser diagnostics for downstream
/// aggregation.
pub(super) fn parse_phase(tokens: Vec<lexer::token::Token>) -> ParsePhase {
    let (program, diagnostics) = parser::Parser::new(tokens).parse();
    ParsePhase {
        program,
        outcome: PhaseDiagnostics::new(diagnostics),
    }
}

/// Resolve imports while preserving diagnostics. Failed import resolution
/// yields no modules or item bindings, matching the previous pipeline policy.
#[cfg(test)]
pub(crate) fn import_phase(program: &parser::ast::Program, root: &std::path::Path) -> ImportPhase {
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
pub(crate) fn desugar_phase(
    program: &mut parser::ast::Program,
    modules: &mut [module::ResolvedModule],
) -> PhaseDiagnostics {
    let output = desugar::DesugarPass::run(program, modules);
    PhaseDiagnostics::new(output.diagnostics)
}

/// Expand one template-prechecked unit, recording its checker's
/// generic metadata first when analysis is captured.
pub(super) fn expand_prechecked_unit(
    checker: &mut semantic::TypeChecker,
    mut source: LiveUnit<parser::ast::Program>,
    unit: module::UnitId,
    storage: &mut UnitArtifacts,
    fresh_ids: &mut semantic::type_checker::generics::FreshIds,
    generic_sources: Option<&mut std::collections::HashMap<module::UnitId, ai::generics::Source>>,
) -> Result<parser::ast::Program> {
    if let Some(sources) = generic_sources {
        let mut metadata = ai::generics::Source::capture(&source);
        metadata.class_templates = checker.generics.class_template_spans();
        metadata.logical_types = checker.generics.logical_types.raw().clone();
        metadata.nominal_aliases = std::mem::take(&mut checker.generics.nominal_aliases);
        metadata.symbols = std::mem::take(&mut checker.generics.source_symbols);
        metadata.index_symbols();
        sources.insert(unit, metadata);
    }
    checker.expand_generics(&mut source, fresh_ids);
    storage.retain_expanded(unit.file_id(), &mut source)?;
    // Offloading accounts the tree itself; the hydration lease ends first.
    let mut program = source.into_inner();
    storage.offload(&mut program)?;
    Ok(program)
}

/// A failed frontend reports the package import that caused it, so package
/// tooling keeps its stable error kind.
pub(super) fn frontend_abort(graph: &mut module::ModuleGraph, errors: usize) -> anyhow::Error {
    let context = format!("aborting due to {errors} error(s)");
    match graph.package_import_error.take() {
        Some(error) => anyhow::Error::new(error).context(context),
        None => anyhow::anyhow!(context),
    }
}

/// Register prelude/module symbols and type-check the entry program.
pub(crate) fn typecheck_phase(
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    artifacts: &UnitArtifacts,
    options: &CompilerOptions,
    queries: Option<&compiler_db::CompilerDb>,
) -> Result<TypecheckPhase> {
    typecheck_phase_with_generics(program, modules, artifacts, options, queries, None)
}

/// State the template pre-check shares across the units it checks.
pub(super) struct TemplatePrecheck<'a> {
    prechecks: std::rc::Rc<semantic::type_checker::precheck::Cache>,
    /// The pre-check has no session yet, but still resolves package aliases
    /// through the build's dependency index.
    dependencies: &'a ModuleDependencies,
    registry: std::rc::Rc<std::cell::RefCell<semantic::type_checker::generics::TemplateRegistry>>,
    field_writes: std::rc::Rc<semantic::field_writes::FieldWrites>,
}

pub(super) fn typecheck_phase_with_generics(
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    artifacts: &UnitArtifacts,
    options: &CompilerOptions,
    queries: Option<&compiler_db::CompilerDb>,
    precheck: Option<TemplatePrecheck<'_>>,
) -> Result<TypecheckPhase> {
    let db = queries;
    let mut checker = semantic::TypeChecker::new();
    checker.template_precheck = precheck.is_some();
    let template_dependencies = precheck.map(|precheck| {
        checker.set_field_writes(precheck.field_writes);
        checker.prechecks = Some(precheck.prechecks);
        checker.generics.registry = precheck.registry;
        checker.capture_generic_call_targets();
        precheck.dependencies
    });
    if let Some(db) = queries {
        checker.source_imports_checked = db.source_imports_checked;
        checker.set_field_writes(std::rc::Rc::clone(&db.field_writes));
        checker = checker.with_sync_stack_preemption(db.inputs().target.sync_stack_preemption);
        checker.set_effect_queries(std::rc::Rc::clone(&db.effects), module::UnitId::ENTRY);
        checker.set_body_queries(std::rc::Rc::clone(&db.typed_bodies));
        checker.declaration_cache = Some(std::rc::Rc::clone(&db.prechecks));
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
    let dependencies = match (db, template_dependencies) {
        (Some(db), _) => db.dependencies(),
        (None, Some(dependencies)) => dependencies,
        (None, None) => {
            fallback = ModuleDependencies::with_packages(modules, None);
            &fallback
        }
    };
    let bindings = ModuleImportBindings::new(program, dependencies);
    let mut aliases = Vec::new();
    for (index, m) in modules.iter().enumerate() {
        prepare_module_registration(&mut checker, m, dependencies);
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
                aliases.push((m.id, *spelling, m.registration_name(), &m.program));
            }
        }
    }
    checker.alias_module_spellings(&aliases);
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

pub(super) fn check_module(
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
    checker.source_imports_checked = db.source_imports_checked;
    checker.set_field_writes(std::rc::Rc::clone(&db.field_writes));
    checker.set_effect_queries(std::rc::Rc::clone(&db.effects), module.id);
    checker.set_body_queries(std::rc::Rc::clone(&db.typed_bodies));
    checker.declaration_cache = Some(std::rc::Rc::clone(&db.prechecks));
    checker.set_declaration_queries(std::rc::Rc::clone(&db.declarations), module.id);
    checker.set_enforce_send_sync(options.enforce_send_sync);
    register_prelude(&mut checker)?;
    db.declarations.set_prelude(&checker.symbols);
    let needed = dependencies.unit_closure(module.id, artifacts)?;
    register_module_imports_with_closure(&mut checker, body, modules, dependencies, Some(&needed));
    checker.set_module_path(module.identity_path());
    checker.set_class_module_path(module.registration_name());
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
pub(crate) struct ModuleImportBindings<'a> {
    pub(crate) imported: std::collections::HashMap<usize, Vec<&'a str>>,
    pub(crate) items: Vec<(&'a str, &'a str, &'a str, diagnostics::Span)>,
    #[cfg(test)]
    pub(crate) path_lookups: usize,
}

impl<'a> ModuleImportBindings<'a> {
    pub(crate) fn new(
        program: &'a parser::ast::Program,
        dependencies: &ModuleDependencies,
    ) -> Self {
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
                    .unwrap_or_else(|| QualifiedName::new(path).last_segment());
                if spellings_seen.insert((id, access)) {
                    imported.entry(id).or_default().push(access);
                }
                continue;
            }
            // Single item: `import math::add;`, `import math::add as plus;`. The
            // module also answers to the consumer's full path so the item
            // lookup can find it without losing the package alias.
            let Some((module_path, item)) = QualifiedName::new(path).split_last() else {
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
///. So the dependency closure is registered too, but by types
/// only -- see `register_module_type_signatures`. Registration follows graph
/// order, which is dependency order, because a module's exported signature is
/// qualified against the spellings its own dependencies already answer to.
#[cfg(test)]
pub(super) fn register_module_imports(
    checker: &mut semantic::TypeChecker,
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    dependencies: &ModuleDependencies,
) {
    register_module_imports_with_closure(checker, program, modules, dependencies, None)
}

pub(super) fn prepare_module_registration(
    checker: &mut semantic::TypeChecker,
    module: &module::ResolvedModule,
    dependencies: &ModuleDependencies,
) {
    if module.canonical_path == "std::collections" {
        for item in &module.program.items {
            if let parser::ast::Item::Class(class) = item
                && let Some(storage) = &class.abi_storage
            {
                checker.array_abi_files.insert(class.span.file_id);
                checker.array_classes.insert(
                    storage.clone(),
                    QualifiedName::join(module.identity_path(), &class.name),
                );
            }
        }
        checker.register_module_type_signatures(
            module.identity_path(),
            &module.path.to_string_lossy(),
            &module.program,
        );
    }
    // Translate source paths into the unspellable canonical namespace.
    if module.symbol_namespace.is_none() {
        return;
    }
    checker.set_registration_import_paths(dependencies.registration_paths(module.id));
}

pub(super) fn register_module_imports_with_closure(
    checker: &mut semantic::TypeChecker,
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    dependencies: &ModuleDependencies,
    needed: Option<&[usize]>,
) {
    if let Some(index) = dependencies.bundled_collections {
        prepare_module_registration(checker, &modules[index], dependencies);
    }
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
    let mut aliases = Vec::new();
    for &id in needed {
        let dep = &modules[id];
        prepare_module_registration(checker, dep, dependencies);
        let canonical = dep.identity_path();
        let dep_path = dep.path.to_string_lossy();
        let Some(spellings) = imported.get(&id) else {
            checker.register_module_type_signatures(canonical, &dep_path, &dep.program);
            checker.register_signature_only_spelling(dep.module_path());
            continue;
        };
        // Keep source spelling order, but register declarations canonically.
        let mut spellings = spellings.iter().copied();
        let first = spellings.next().expect("import has a spelling");
        let registered = dep.registration_name();
        checker.register_module_with_id(
            dep.id,
            registered,
            dep.identity_path(),
            &dep_path,
            &dep.program,
        );
        for access in std::iter::once(first).chain(spellings) {
            aliases.push((dep.id, access, registered, &dep.program));
        }
    }

    checker.alias_module_spellings(&aliases);
    for (local, module_path, item, span) in items {
        checker.register_item_import(local, module_path, item, span);
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static DEPENDENCY_WORK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

/// Index non-preemptible methods visible through one module's own imports.
///
/// `check_module_program` computes the module's local helper graph itself, but
/// typed receiver calls into another module need the imported method-owner map
/// that the entry checker is also seeded with. The keys use the exact access
/// spelling of this module: an alias namespace for whole-module imports, or the
/// local class name for a direct item import.
pub(super) fn imported_nonpreemptible_method_owners(
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
            let access = import
                .alias
                .as_deref()
                .unwrap_or_else(|| QualifiedName::new(&import.path).last_segment());
            (dependency, access, None)
        } else {
            let Some((module_path, item)) = QualifiedName::new(&import.path).split_last() else {
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
pub(super) fn check_unit_concurrency(
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    dependencies: &ModuleDependencies,
    effects: &compiler_db::effects::EffectQueries,
    local_helpers: Option<(
        &compiler_db::HelperSummary,
        compiler_db::inputs::TargetCapabilities,
    )>,
    body_queries: Option<&compiler_db::body::BodyQueries>,
) -> Vec<diagnostics::Diagnostic> {
    let by_path = dependencies.paths_for(program);
    let bindings = ModuleImportBindings::new(program, dependencies);
    let mut analyzer = semantic::ConcurrencyAnalyzer::new();
    if let Some((_, target)) = local_helpers {
        analyzer = analyzer.with_sync_stack_preemption(target.sync_stack_preemption);
    }
    // Class references borrow the variable slot; interfaces/enums remain gated.
    for (id, spellings) in &bindings.imported {
        for item in &modules[*id].program.items {
            if let parser::ast::Item::Class(class) = item {
                for access in spellings {
                    analyzer =
                        analyzer.with_reference_class(QualifiedName::join(access, &class.name));
                }
            }
        }
    }
    let mut reference_classes = std::collections::HashMap::new();
    for &(local, path, item, _) in &bindings.items {
        if let Some(&id) = by_path.get(path) {
            let classes = reference_classes.entry(id).or_insert_with(|| {
                modules[id]
                    .program
                    .items
                    .iter()
                    .filter_map(|decl| match decl {
                        parser::ast::Item::Class(class) => Some(class.name.as_str()),
                        _ => None,
                    })
                    .collect::<std::collections::HashSet<_>>()
            });
            if classes.contains(item) {
                analyzer = analyzer.with_reference_class(local.to_owned());
            }
        }
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
        Some((helpers, _)) => {
            analyzer
                .check_program_cached(program, helpers, body_queries)
                .errors
        }
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
                    symbol_namespace: None,
                    id: module::ModuleId(i + 1),
                    name: format!("m{i}"),
                    canonical_path: format!("m{i}"),
                    path: format!("m{i}.wi").into(),
                    source: "é".into(),
                    program: parser::ast::Program {
                        instantiation_types: Vec::new(),
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
            symbol_namespace: None,
            id: module::ModuleId(17),
            name: "helper".into(),
            canonical_path: "helper".into(),
            path: "helper.wi".into(),
            source: "é".into(),
            program: parser::ast::Program {
                instantiation_types: Vec::new(),
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
pub(super) struct DiagnosticModuleIndex {
    package_names: std::collections::HashMap<String, String>,
    positions: std::collections::HashMap<diagnostics::FileId, usize>,
    #[cfg(test)]
    probes: std::cell::Cell<usize>,
}

impl DiagnosticModuleIndex {
    pub(super) fn package_names(
        graph: &module::ModuleGraph,
    ) -> std::collections::HashMap<String, String> {
        graph
            .package_graph
            .as_ref()
            .map(|packages| {
                packages
                    .packages
                    .iter()
                    .map(|package| {
                        (
                            semantic::ids::package_namespace(&package.identity),
                            if package.id == packages.root {
                                String::new()
                            } else {
                                QualifiedName::prefix(&package.identity.name)
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn new(graph: &module::ModuleGraph) -> Self {
        Self {
            package_names: Self::package_names(graph),
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
pub(super) fn emit_frontend_diagnostics(
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
        emitter.emit(
            &diagnostic.with_source_names(&modules.package_names),
            &sources,
        )?;
    }
    Ok(())
}
pub(super) fn diagnostic_error_count(diagnostics: &[diagnostics::Diagnostic]) -> usize {
    diagnostics
        .iter()
        .filter(|diag| diag.severity == diagnostics::Severity::Error)
        .count()
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
    fn module_registration_ids_ignore_first_consumer_spelling() {
        use semantic::ids::{FunctionId, TypeId};
        for first_access in ["proto", "base", "again"] {
            let modules = vec![module::ResolvedModule {
                id: module::ModuleId(1),
                package: package::PackageId(0),
                symbol_module: None,
                symbol_namespace: Some(std::sync::Arc::from("$local::base")),
                name: first_access.into(),
                canonical_path: "base".into(),
                path: "base.wi".into(),
                source: String::new(),
                program: parse_source(
                    "pub class Parcel { pub fn read(self) -> i64 { return 42; } } pub fn id<T>(x: T) -> T { return x; } pub class Cell<T> { pub value: T; }",
                ),
            }];
            let entry = parse_source(
                "import base as proto; import base as again; import base::Parcel; import base::id as item; import base::Cell as Imported;",
            );
            let dependencies = ModuleDependencies::with_packages(&modules, None);
            let mut checker = semantic::TypeChecker::new();
            register_module_imports(&mut checker, &entry, &modules, &dependencies);
            for spelling in ["proto::Parcel", "again::Parcel", "Parcel"] {
                let info = checker.symbols.lookup_class(spelling).unwrap();
                assert_eq!(info.name, "$local::base::Parcel");
                assert_eq!(
                    FunctionId::method(TypeId::from_source_name(&info.name), "read"),
                    FunctionId::method(TypeId::from_source_name("$local::base::Parcel"), "read")
                );
            }
            assert!(std::ptr::eq(
                checker.symbols.lookup_module("proto").unwrap(),
                checker.symbols.lookup_module("again").unwrap()
            ));
            assert_eq!(checker.generics.function_paths["proto::id"], "$local::base");
            assert_eq!(checker.generics.function_paths["again::id"], "$local::base");
            assert_eq!(checker.generics.function_paths["item"], "$local::base");
            assert_eq!(checker.generics.class_paths["proto::Cell"], "$local::base");
            assert_eq!(checker.generics.class_paths["again::Cell"], "$local::base");
            assert_eq!(checker.generics.class_paths["Imported"], "$local::base");
        }
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
                            symbol_namespace: None,
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
                for module in &modules {
                    let routes = index.registration_paths(module.id);
                    for _ in 0..16 {
                        assert!(std::rc::Rc::ptr_eq(
                            &routes,
                            &index.registration_paths(module.id)
                        ));
                    }
                }
                // Shared route reads do not repeat import-path resolution.
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
                    "shape={shape} modules={n} passes=3 build_path_lookups={} shared_route_reads={} route_path_lookups=0 closure_path_lookups=0 edge_visits={}",
                    work.0,
                    16 * n,
                    work.1
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
                symbol_namespace: None,
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
        assert!(lex_phase("fn main() { $ }").is_err());
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
        assert!(imports.graph.files.iter().all(|file| file.is_bundled()));
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
            symbol_namespace: None,
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
        let program = parse_source("async fn update(x: &mut fn(i64) -> i64) {} fn main() {}");
        let phase = check_unit_concurrency(
            &program,
            &[],
            &ModuleDependencies::new(&[]),
            &compiler_db::effects::EffectQueries::default(),
            None,
            None,
        );
        assert!(diagnostic_error_count(&phase) > 0);
        assert!(!phase.is_empty());
    }
}

pub(super) fn validate_entry_point(program: &parser::ast::Program) -> Vec<diagnostics::Diagnostic> {
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
        // `main` may return `void` or `Result<void, E>`. A
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

pub(super) fn is_main_args_type(
    ty: &parser::ast::Type,
    std_collections_module_imported: bool,
) -> bool {
    use parser::ast::Type;

    match ty {
        Type::Array(element) => **element == Type::String,
        Type::Generic(name, args) if std_collections_module_imported => {
            name == "collections::Array" && matches!(args.as_slice(), [Type::String])
        }
        _ => false,
    }
}
