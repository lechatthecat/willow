mod backend;
mod diagnostics;
mod ir;
mod lexer;
mod module;
mod parser;
mod prelude;
mod project;
mod semantic;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BuildMode {
    Debug,
    Release,
}

pub struct CodegenOptions {
    pub build_mode: BuildMode,
    pub emit_debug_info: bool,
    pub emit_source_map: bool,
    pub strip_symbols: bool,
    pub runtime_lib: Option<PathBuf>,
}

impl CodegenOptions {
    pub fn debug() -> Self {
        Self {
            build_mode: BuildMode::Debug,
            emit_debug_info: true,
            emit_source_map: true,
            strip_symbols: false,
            runtime_lib: None,
        }
    }

    pub fn release() -> Self {
        Self {
            build_mode: BuildMode::Release,
            emit_debug_info: false,
            emit_source_map: false,
            strip_symbols: false,
            runtime_lib: None,
        }
    }

    pub fn release_with_debug_info() -> Self {
        Self {
            build_mode: BuildMode::Release,
            emit_debug_info: true,
            emit_source_map: true,
            strip_symbols: false,
            runtime_lib: None,
        }
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("build") => cmd_build(&args[2..]),
        Some("run") => cmd_run(&args[2..]),
        Some("debug") => cmd_debug(&args[2..]),
        Some(src) if src.ends_with(".wi") => {
            // 後方互換: willowc <file> [-o <output>]
            let out = if let Some(pos) = args.iter().position(|a| a == "-o") {
                args.get(pos + 1)
                    .cloned()
                    .unwrap_or_else(|| "a.out".to_string())
            } else {
                PathBuf::from(src)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("a")
                    .to_string()
            };
            let mut opts = CodegenOptions::debug();
            opts.runtime_lib = parse_runtime_lib_arg(&args[2..]);
            compile(src, &out, &opts, None)
        }
        _ => {
            eprintln!("Usage:");
            eprintln!(
                "  willowc build <source.wi> [-o <output>] [--debug|--release] [--debug-info] [--runtime-lib <path>]"
            );
            eprintln!(
                "  willowc run   <source.wi> [--debug|--release] [--debug-info] [--runtime-lib <path>] [-- <args>...]"
            );
            eprintln!("  willowc debug <source.wi>");
            std::process::exit(1);
        }
    }
}

fn cmd_build(args: &[String]) -> Result<()> {
    // Single-file mode: explicit .wi file in args.
    if let Some(src) = args.iter().find(|a| a.ends_with(".wi")) {
        let out = if let Some(pos) = args.iter().position(|a| a == "-o") {
            args.get(pos + 1).cloned().unwrap_or_else(|| stem(src))
        } else {
            stem(src)
        };
        let opts = parse_build_mode(args);
        return compile(src, &out, &opts, None);
    }

    // Project mode: look for project.toml in the specified dir or cwd.
    let search_dir = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|s| PathBuf::from(s))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let (manifest_path, project_root) =
        project::find_project_manifest(&search_dir).ok_or_else(|| {
            anyhow::anyhow!(
                "no source file or project.toml found (searched from {})",
                search_dir.display()
            )
        })?;

    let manifest = project::ProjectManifest::load(&manifest_path)?;
    let entry = manifest.entry_point(&project_root);

    if !entry.exists() {
        anyhow::bail!(
            "entry point not found: {} (declared in {})",
            entry.display(),
            manifest_path.display()
        );
    }

    let out = if let Some(pos) = args.iter().position(|a| a == "-o") {
        args.get(pos + 1)
            .cloned()
            .unwrap_or_else(|| manifest.project.name.clone())
    } else {
        manifest.project.name.clone()
    };

    let opts = parse_build_mode(args);
    eprintln!(
        "building project '{}' v{}",
        manifest.project.name, manifest.project.version
    );
    compile(entry.to_str().unwrap(), &out, &opts, Some(project_root))
}

fn cmd_run(args: &[String]) -> Result<()> {
    let separator = args.iter().position(|a| a == "--");
    let (compiler_args, program_args) = match separator {
        Some(pos) => (&args[..pos], &args[pos + 1..]),
        None => (args, &[][..]),
    };
    let src = compiler_args
        .iter()
        .find(|a| a.ends_with(".wi"))
        .ok_or_else(|| anyhow::anyhow!("no source file specified"))?;
    let out = format!("/tmp/willow_run_{}", stem(src));
    let opts = parse_build_mode(compiler_args);
    compile(src, &out, &opts, None)?;
    let status = Command::new(&out)
        .args(program_args)
        .status()
        .with_context(|| format!("failed to run {}", out))?;
    std::process::exit(status.code().unwrap_or(0));
}

fn cmd_debug(args: &[String]) -> Result<()> {
    let src = args
        .iter()
        .find(|a| a.ends_with(".wi"))
        .ok_or_else(|| anyhow::anyhow!("no source file specified"))?;
    let out = format!("/tmp/willow_debug_{}", stem(src));
    let mut opts = CodegenOptions::debug();
    opts.runtime_lib = parse_runtime_lib_arg(args);
    compile(src, &out, &opts, None)?;
    eprintln!("note: interactive debugger not yet implemented");
    eprintln!("running in debug mode: {}", out);
    let status = Command::new(&out)
        .status()
        .with_context(|| format!("failed to run {}", out))?;
    std::process::exit(status.code().unwrap_or(0));
}

fn parse_build_mode(args: &[String]) -> CodegenOptions {
    let mut opts = if args.iter().any(|a| a == "--release") {
        if args.iter().any(|a| a == "--debug-info") {
            CodegenOptions::release_with_debug_info()
        } else {
            CodegenOptions::release()
        }
    } else {
        CodegenOptions::debug()
    };
    opts.runtime_lib = parse_runtime_lib_arg(args);
    opts
}

fn parse_runtime_lib_arg(args: &[String]) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--runtime-lib" {
            return iter.next().map(PathBuf::from);
        }
        if let Some(path) = arg.strip_prefix("--runtime-lib=") {
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn stem(path: &str) -> String {
    PathBuf::from(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a")
        .to_string()
}

/// Parse the prelude source and register its declarations with the type checker.
fn register_prelude(checker: &mut semantic::TypeChecker) -> Result<()> {
    let tokens = lexer::Lexer::new(prelude::PRELUDE_SOURCE)
        .tokenize()
        .map_err(|_| anyhow::anyhow!("internal error: prelude lexer failed"))?;
    let (program, errors) = parser::Parser::new(tokens).parse();
    if !errors.is_empty() {
        anyhow::bail!("internal error: prelude parse failed");
    }
    // Register only declarations; do not type-check the prelude body.
    use parser::ast::Item;
    for item in &program.items {
        match item {
            Item::Enum(e) => checker.register_prelude_enum(e),
            Item::Function(f) => {
                // Future: register prelude functions (e.g. panic) here.
                let _ = f;
            }
            _ => {}
        }
    }
    Ok(())
}

fn compile(
    src: &str,
    out: &str,
    opts: &CodegenOptions,
    project_root: Option<PathBuf>,
) -> Result<()> {
    use diagnostics::{Diagnostic, ErrorCode, Severity};

    let src_path = PathBuf::from(src);
    let source = std::fs::read_to_string(&src_path)
        .with_context(|| format!("cannot read {}", src_path.display()))?;

    // Import resolution root: the directory containing the source file.
    let _ = project_root; // available for future use (e.g. package search paths)
    let root = src_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let map = diagnostics::SourceMap::new(src, &source);

    // Lex — hard stop: without tokens there is nothing to parse.
    let tokens = lexer::Lexer::new(&source).tokenize().map_err(|errs| {
        diagnostics::emit_all(&errs, &map);
        anyhow::anyhow!("aborting due to {} lexer error(s)", errs.len())
    })?;

    // Parse — collect errors but continue with the partial AST so downstream
    // stages can surface additional independent diagnostics.
    let (program, parse_errors) = parser::Parser::new(tokens).parse();
    diagnostics::emit_all(&parse_errors, &map);

    // Resolve imports — collect errors but do not abort; we can still type-check
    // items that do not depend on the failed imports.
    let (modules, item_imports, import_errors) = match module::resolve_imports(&program, &root) {
        Ok((mods, items)) => (mods, items, vec![]),
        Err(errs) => {
            diagnostics::emit_all(&errs, &map);
            (vec![], vec![], errs)
        }
    };

    // Type check — register prelude first, then imported modules, then the
    // entry file's single-item imports, then the entry program.
    let mut checker = semantic::TypeChecker::new();
    register_prelude(&mut checker)?;
    for m in &modules {
        checker.register_module(&m.name, &m.path.to_string_lossy(), &m.program);
        if item_imports.iter().any(|item| {
            item.canonical_module == m.canonical_path && item.canonical_module != m.name
        }) {
            checker.register_module(&m.canonical_path, &m.path.to_string_lossy(), &m.program);
        }
    }
    for item in &item_imports {
        checker.register_item_import(&item.local, &item.canonical_module, &item.item, item.span);
    }
    checker.check_program(&program);
    diagnostics::emit_all(&checker.errors, &map);

    let concurrency = semantic::ConcurrencyAnalyzer::new().check_program(&program);
    diagnostics::emit_all(&concurrency.errors, &map);

    let entry_errors = validate_entry_point(&program);
    diagnostics::emit_all(&entry_errors, &map);

    // Abort if any errors were found across all stages.
    let error_count = parse_errors.len()
        + import_errors.len()
        + checker.errors.len()
        + concurrency.errors.len()
        + entry_errors.len();
    if error_count > 0 {
        anyhow::bail!("aborting due to {} error(s)", error_count);
    }

    // Codegen — wrap internal errors in a structured diagnostic.
    let mut codegen = backend::Codegen::new(opts).map_err(|e| {
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0800,
            format!("internal compiler error: {e}"),
        );
        diagnostics::emit(&d, &map);
        anyhow::anyhow!("internal compiler error")
    })?;

    // register_builtin_generic_enums is now a no-op: all enums (including
    // prelude ones) come from the checker symbol table below.
    codegen.register_builtin_generic_enums();
    // Register all enum infos (prelude + user-declared) for the backend.
    for (name, info) in &checker.symbols.enums {
        codegen.register_enum_info(name.clone(), info.clone());
    }
    // Register interface metadata for vtable codegen + interface dispatch.
    for (name, info) in &checker.symbols.interfaces {
        codegen.register_interface_info(name.clone(), info.clone());
    }
    // Pass type-checker-inferred lambda return types so unannotated lambdas
    // get correct Cranelift signatures (instead of falling back to I64).
    codegen.register_lambda_return_types(checker.lambda_return_types.clone());
    // Resolved types of async-fn locals, so the backend can frame-back
    // unannotated live-across-await locals (willow-lpn.5c).
    codegen.register_async_local_types(checker.async_local_types.clone());

    for m in &modules {
        codegen
            .compile_module(
                &m.name,
                &m.canonical_path,
                &m.program,
                &m.path.to_string_lossy(),
            )
            .map_err(|e| {
                let d = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0800,
                    format!("internal compiler error in module `{}`: {e}", m.name),
                );
                diagnostics::emit(&d, &map);
                anyhow::anyhow!("internal compiler error")
            })?;
    }
    // Bind the entry file's single-item imports to the module functions they
    // name, after all modules are compiled (so the mangled symbols exist).
    for item in &item_imports {
        codegen.register_item_import(&item.local, &item.canonical_module, &item.item);
    }
    codegen.compile_program(&program, src).map_err(|e| {
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0800,
            format!("internal compiler error: {e}"),
        );
        diagnostics::emit(&d, &map);
        anyhow::anyhow!("internal compiler error")
    })?;

    let debug_metadata = if opts.emit_debug_info || opts.emit_source_map {
        Some(debug_source_map_text(&map, &program, &modules))
    } else {
        None
    };
    if opts.emit_debug_info {
        codegen
            .embed_runtime_metadata(debug_metadata.as_deref().unwrap_or(""))
            .map_err(|e| {
                let d = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0800,
                    format!("internal compiler error: {e}"),
                );
                diagnostics::emit(&d, &map);
                anyhow::anyhow!("internal compiler error")
            })?;
    }

    let obj_bytes = codegen.finish().map_err(|e| {
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0800,
            format!("internal compiler error: {e}"),
        );
        diagnostics::emit(&d, &map);
        anyhow::anyhow!("internal compiler error")
    })?;

    let obj_path = format!("{}.o", out);
    std::fs::write(&obj_path, &obj_bytes)?;

    let runtime_lib = resolve_runtime_lib(opts).map_err(|err| {
        let _ = std::fs::remove_file(&obj_path);
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0700,
            format!("runtime library unavailable: {err}"),
        )
        .with_help("build willow_runtime with Cargo or pass --runtime-lib / WILLOW_RUNTIME_LIB");
        diagnostics::emit(&d, &map);
        anyhow::anyhow!("runtime library unavailable")
    })?;

    // Link — wrap failure in a structured diagnostic.
    let mut link_args = vec![
        obj_path.clone(),
        runtime_lib.display().to_string(),
        "-o".to_string(),
        out.to_string(),
        // Cranelift emits absolute relocations; disable PIE so the linker
        // does not need DT_TEXTREL in a read-only .text section.
        "-no-pie".to_string(),
        "-lm".to_string(),
        "-lpthread".to_string(),
        "-ldl".to_string(),
    ];
    if opts.strip_symbols {
        link_args.push("-s".to_string());
    }
    let status = Command::new("cc")
        .args(&link_args)
        .status()
        .with_context(|| "failed to run linker")?;

    let _ = std::fs::remove_file(&obj_path);

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
        diagnostics::emit(&d, &map);
        anyhow::bail!("linking failed");
    }

    if opts.emit_source_map {
        write_debug_source_maps(out, &map, &program, &modules)?;
    } else {
        let _ = std::fs::remove_file(debug_source_map_path(out));
    }

    let mode = if opts.build_mode == BuildMode::Release {
        "release"
    } else {
        "debug"
    };
    eprintln!("compiled [{}]: {}", mode, out);
    Ok(())
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

fn write_debug_source_maps(
    out: &str,
    entry_map: &diagnostics::SourceMap,
    entry_program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
) -> Result<()> {
    std::fs::write(
        debug_source_map_path(out),
        debug_source_map_text(entry_map, entry_program, modules),
    )?;
    Ok(())
}

fn debug_source_map_path(out: &str) -> String {
    format!("{out}.wsmap")
}

fn resolve_runtime_lib(opts: &CodegenOptions) -> Result<PathBuf> {
    if let Some(path) = &opts.runtime_lib {
        return validate_runtime_lib_path(path);
    }

    if let Some(path) = std::env::var_os("WILLOW_RUNTIME_LIB") {
        return validate_runtime_lib_path(PathBuf::from(path));
    }

    let path = default_runtime_lib_path(opts);
    build_default_runtime_lib(opts)?;
    validate_runtime_lib_path(path)
}

fn validate_runtime_lib_path(path: impl Into<PathBuf>) -> Result<PathBuf> {
    let path = path.into();
    if path.is_file() {
        Ok(path)
    } else {
        anyhow::bail!("{} does not exist or is not a file", path.display())
    }
}

fn default_runtime_lib_path(opts: &CodegenOptions) -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("target"));
    let profile = if opts.build_mode == BuildMode::Release {
        "release"
    } else {
        "debug"
    };
    target_dir.join(profile).join("libwillow_runtime.a")
}

fn build_default_runtime_lib(opts: &CodegenOptions) -> Result<()> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut args = vec![
        "build".to_string(),
        "-p".to_string(),
        "willow_runtime".to_string(),
    ];
    if opts.build_mode == BuildMode::Release {
        args.push("--release".to_string());
    }

    let status = Command::new("cargo")
        .args(&args)
        .current_dir(&manifest_dir)
        .status()
        .with_context(|| "failed to run cargo to build willow_runtime")?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("cargo failed to build willow_runtime")
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

    for main in mains {
        let valid_args = match main.params.as_slice() {
            [] => true,
            [param] => matches!(&param.ty, Type::Array(element) if **element == Type::String),
            _ => false,
        };
        let valid_return = main.return_type == Type::Void;

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
